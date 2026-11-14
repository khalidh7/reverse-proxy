use std::{sync::Arc, time::Duration};

use axum::body::Body;
use http_body_util::{BodyExt, Limited};
use hyper::{Request, header};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use tokio::{
    sync::watch,
    task::JoinSet,
    time::{MissedTickBehavior, interval, timeout},
};

use crate::load_balancer::LoadBalancer;

pub async fn monitor(
    balancer: Arc<LoadBalancer>,
    interval_duration: Duration,
    deadline: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let client = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build_http();
    let mut tick = interval(interval_duration);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(()),
            _ = tick.tick() => {}
        }
        let mut probes = JoinSet::new();
        for upstream in &balancer.upstreams {
            let upstream = upstream.clone();
            let client = client.clone();
            probes.spawn(async move {
                let request = Request::get(upstream.health_uri.clone())
                    .header(header::CONNECTION, "close")
                    .body(Body::empty())
                    .expect("validated health URI");
                let healthy = timeout(deadline, async {
                    let response = client.request(request).await?;
                    Ok::<_, hyper_util::client::legacy::Error>(
                        response.status().is_success()
                            && Limited::new(response.into_body(), 4096)
                                .collect()
                                .await
                                .is_ok(),
                    )
                })
                .await
                .is_ok_and(|result| result.unwrap_or(false));
                upstream.set_health(healthy);
            });
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(()),
            _ = async {
                while let Some(result) = probes.join_next().await {
                    if let Err(error) = result {
                        tracing::error!(%error, "health probe task failed");
                    }
                }
            } => {}
        }
    }
}
