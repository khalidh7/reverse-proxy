mod control;
mod health;
mod load_balancer;
mod metrics;
mod proxy;
mod server;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use load_balancer::{Algorithm, LoadBalancer, UpstreamConfig};
use serde::Deserialize;
use tokio::{net::TcpListener, signal, sync::watch, time::timeout};
use tracing_subscriber::EnvFilter;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    management_listen: SocketAddr,
    #[serde(default)]
    upstream: Option<String>,
    #[serde(default)]
    upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    algorithm: Algorithm,
    #[serde(default = "health_interval")]
    health_interval_ms: u64,
    #[serde(default = "health_timeout")]
    health_timeout_ms: u64,
    #[serde(default)]
    allow_non_loopback_management: bool,
    max_connections: usize,
    upstream_timeout_ms: u64,
    shutdown_timeout_ms: u64,
}

fn health_interval() -> u64 {
    1000
}
fn health_timeout() -> u64 {
    500
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let path = std::env::args_os()
        .nth(1)
        .unwrap_or_else(|| "proxy/config/development.toml".into());
    let config: Config =
        toml::from_str(&std::fs::read_to_string(&path).with_context(|| format!("read {path:?}"))?)?;
    ensure!(
        config.management_listen.ip().is_loopback() || config.allow_non_loopback_management,
        "management listener must be loopback"
    );
    ensure!(
        (1..=65_536).contains(&config.max_connections),
        "max_connections must be 1–65536"
    );
    ensure!(
        config.upstream_timeout_ms > 0 && config.shutdown_timeout_ms > 0,
        "timeouts must be positive"
    );

    ensure!(
        config.health_interval_ms > 0 && config.health_timeout_ms > 0,
        "health timings must be positive"
    );
    let upstreams = match config.upstream {
        Some(origin) => {
            ensure!(
                config.upstreams.is_empty(),
                "configure upstream or upstreams"
            );
            vec![UpstreamConfig {
                id: "app-1".into(),
                origin,
            }]
        }
        None => config.upstreams,
    };
    let balancer = Arc::new(LoadBalancer::new(upstreams, config.algorithm)?);

    let data = TcpListener::bind(config.listen)
        .await
        .context("bind proxy data listener")?;
    let management = TcpListener::bind(config.management_listen)
        .await
        .context("bind proxy management listener")?;
    let proxy = proxy::Proxy::new(
        balancer.clone(),
        Duration::from_millis(config.upstream_timeout_ms),
    );
    let management_router = control::router(proxy.metrics.clone(), balancer.clone());
    let (shutdown, mut management_shutdown) = watch::channel(false);
    let data_shutdown = shutdown.subscribe();
    let health_shutdown = shutdown.subscribe();
    let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;

    tracing::info!(listen = %data.local_addr()?, management = %management.local_addr()?, "proxy listening");
    let mut serving = std::pin::pin!(async {
        tokio::try_join!(
            server::serve(data, proxy, config.max_connections, data_shutdown),
            health::monitor(
                balancer,
                Duration::from_millis(config.health_interval_ms),
                Duration::from_millis(config.health_timeout_ms),
                health_shutdown
            ),
            async {
                axum::serve(management, management_router)
                    .with_graceful_shutdown(async move {
                        let _ = management_shutdown.changed().await;
                    })
                    .await
            }
        )
    });

    tokio::select! {
        result = &mut serving => { result?; return Ok(()); }
        result = signal::ctrl_c() => { result?; }
        _ = terminate.recv() => {}
    }
    let _ = shutdown.send(true);
    tracing::info!("draining proxy connections");
    match timeout(Duration::from_millis(config.shutdown_timeout_ms), serving).await {
        Ok(result) => {
            result?;
        }
        Err(_) => tracing::warn!("proxy shutdown deadline reached"),
    }
    Ok(())
}
