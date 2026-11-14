use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
};

use anyhow::{Context, Result, ensure};
use hyper::Uri;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    #[default]
    RoundRobin,
    LeastActive,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub id: String,
    pub origin: String,
}

pub struct Upstream {
    id: String,
    pub origin: Uri,
    pub health_uri: Uri,
    healthy: AtomicBool,
    active: AtomicU64,
    requests: AtomicU64,
}

impl Upstream {
    pub fn set_health(&self, healthy: bool) {
        if self.healthy.swap(healthy, Relaxed) != healthy {
            tracing::info!(upstream = %self.id, healthy, "upstream health changed");
        }
    }
}

pub struct ActiveRequest(pub Arc<Upstream>);

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Relaxed);
    }
}

pub struct LoadBalancer {
    pub upstreams: Vec<Arc<Upstream>>,
    routing: Mutex<Routing>,
}

struct Routing {
    version: u64,
    algorithm: Algorithm,
    enabled: Vec<bool>,
    next: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub version: u64,
    pub algorithm: Algorithm,
    pub enabled_upstreams: Vec<String>,
}

pub enum UpdateError {
    Stale,
    InvalidUpstreams,
}

#[derive(Serialize)]
pub struct Snapshot {
    version: u64,
    algorithm: Algorithm,
    upstreams: Vec<UpstreamStatus>,
}

#[derive(Serialize)]
struct UpstreamStatus {
    id: String,
    origin: String,
    enabled: bool,
    healthy: bool,
    active_requests: u64,
    requests: u64,
}

impl LoadBalancer {
    pub fn new(configs: Vec<UpstreamConfig>, algorithm: Algorithm) -> Result<Self> {
        ensure!(
            (1..=32).contains(&configs.len()),
            "configure 1–32 upstreams"
        );
        let mut ids = HashSet::new();
        let mut origins = HashSet::new();
        let mut upstreams = Vec::new();
        for config in configs {
            ensure!(
                (1..=32).contains(&config.id.len())
                    && config
                        .id
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_')
                    && ids.insert(config.id.clone()),
                "upstream IDs must be unique and contain 1–32 letters, digits, hyphens, or underscores"
            );
            let origin: Uri = config
                .origin
                .parse()
                .with_context(|| format!("parse upstream {}", config.id))?;
            ensure!(
                origin.scheme_str() == Some("http")
                    && origin
                        .authority()
                        .is_some_and(|authority| !authority.as_str().contains('@'))
                    && origin.path() == "/"
                    && origin.query().is_none()
                    && !config.origin.contains('#'),
                "upstream {} must be an HTTP origin",
                config.id
            );
            let authority = origin.authority().unwrap();
            let port = authority.port_u16().unwrap_or(80);
            ensure!(
                port > 0 && origins.insert((authority.host().to_ascii_lowercase(), port)),
                "upstream origins must be unique and have nonzero ports"
            );
            let health_uri = Uri::builder()
                .scheme("http")
                .authority(authority.clone())
                .path_and_query("/health")
                .build()?;
            upstreams.push(Arc::new(Upstream {
                id: config.id,
                origin,
                health_uri,
                healthy: AtomicBool::new(false),
                active: AtomicU64::new(0),
                requests: AtomicU64::new(0),
            }));
        }
        let enabled = vec![true; upstreams.len()];
        Ok(Self {
            upstreams,
            routing: Mutex::new(Routing {
                version: 1,
                algorithm,
                enabled,
                next: 0,
            }),
        })
    }

    pub fn select(&self) -> Option<ActiveRequest> {
        let mut routing = self.routing.lock().expect("routing lock");
        let count = self.upstreams.len();
        let eligible = (0..count)
            .map(|offset| (routing.next + offset) % count)
            .filter(|&index| routing.enabled[index] && self.upstreams[index].healthy.load(Relaxed));
        let index = match routing.algorithm {
            Algorithm::RoundRobin => eligible.into_iter().next(),
            Algorithm::LeastActive => {
                eligible.min_by_key(|&index| self.upstreams[index].active.load(Relaxed))
            }
        }?;
        routing.next = (index + 1) % count;
        let upstream = &self.upstreams[index];
        upstream.active.fetch_add(1, Relaxed);
        upstream.requests.fetch_add(1, Relaxed);
        Some(ActiveRequest(upstream.clone()))
    }

    pub fn snapshot(&self) -> Snapshot {
        self.describe(&self.routing.lock().expect("routing lock"))
    }

    pub fn clear_routed_counts(&self) -> Snapshot {
        let routing = self.routing.lock().expect("routing lock");
        for upstream in &self.upstreams {
            upstream.requests.store(0, Relaxed);
        }
        self.describe(&routing)
    }

    pub fn update(&self, update: Update) -> Result<Snapshot, UpdateError> {
        let enabled: HashSet<_> = update
            .enabled_upstreams
            .iter()
            .map(String::as_str)
            .collect();
        if enabled.len() != update.enabled_upstreams.len()
            || !enabled
                .iter()
                .all(|id| self.upstreams.iter().any(|upstream| upstream.id == *id))
        {
            return Err(UpdateError::InvalidUpstreams);
        }
        let mut routing = self.routing.lock().expect("routing lock");
        if routing.version != update.version {
            return Err(UpdateError::Stale);
        }
        routing.algorithm = update.algorithm;
        for (upstream, active) in self.upstreams.iter().zip(&mut routing.enabled) {
            *active = enabled.contains(upstream.id.as_str());
        }
        routing.version += 1;
        Ok(self.describe(&routing))
    }

    fn describe(&self, routing: &Routing) -> Snapshot {
        Snapshot {
            version: routing.version,
            algorithm: routing.algorithm,
            upstreams: self
                .upstreams
                .iter()
                .zip(&routing.enabled)
                .map(|(upstream, &enabled)| UpstreamStatus {
                    id: upstream.id.clone(),
                    origin: upstream.origin.to_string(),
                    enabled,
                    healthy: upstream.healthy.load(Relaxed),
                    active_requests: upstream.active.load(Relaxed),
                    requests: upstream.requests.load(Relaxed),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balancer() -> LoadBalancer {
        let configs = (1..=3)
            .map(|n| UpstreamConfig {
                id: format!("app-{n}"),
                origin: format!("http://127.0.0.1:300{n}"),
            })
            .collect();
        let balancer = LoadBalancer::new(configs, Algorithm::RoundRobin).unwrap();
        for upstream in &balancer.upstreams {
            upstream.set_health(true);
        }
        balancer
    }

    #[test]
    fn round_robin_skips_unhealthy_and_disabled_upstreams() {
        let balancer = balancer();
        for id in ["app-1", "app-2", "app-3", "app-1"] {
            assert_eq!(balancer.select().unwrap().0.id, id);
        }
        balancer.upstreams[1].set_health(false);
        balancer
            .update(Update {
                version: 1,
                algorithm: Algorithm::RoundRobin,
                enabled_upstreams: vec!["app-2".into(), "app-3".into()],
            })
            .ok()
            .unwrap();
        assert_eq!(balancer.select().unwrap().0.id, "app-3");
        balancer.upstreams[2].set_health(false);
        assert!(balancer.select().is_none());
        balancer.upstreams[1].set_health(true);
        assert_eq!(balancer.select().unwrap().0.id, "app-2");
    }

    #[test]
    fn least_active_tracks_leases_and_rejects_stale_changes() {
        let balancer = balancer();
        let enabled = vec!["app-1".into(), "app-2".into()];
        balancer
            .update(Update {
                version: 1,
                algorithm: Algorithm::LeastActive,
                enabled_upstreams: enabled.clone(),
            })
            .ok()
            .unwrap();
        assert!(matches!(
            balancer.update(Update {
                version: 1,
                algorithm: Algorithm::RoundRobin,
                enabled_upstreams: enabled,
            }),
            Err(UpdateError::Stale)
        ));
        let first = balancer.select().unwrap();
        let second = balancer.select().unwrap();
        assert_eq!(first.0.id, "app-1");
        assert_eq!(second.0.id, "app-2");
        drop(second);
        assert_eq!(balancer.select().unwrap().0.id, "app-2");
    }
}
