mod metrics;
mod proxy;
mod server;

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{Json, Router, extract::State, routing::get};
use hyper::Uri;
use serde::Deserialize;
use tokio::{net::TcpListener, signal, sync::watch, time::timeout};
use tracing_subscriber::EnvFilter;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    management_listen: SocketAddr,
    upstream: String,
    max_connections: usize,
    upstream_timeout_ms: u64,
    shutdown_timeout_ms: u64,
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
        config.management_listen.ip().is_loopback(),
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

    let upstream: Uri = config.upstream.parse()?;
    ensure!(
        upstream.scheme_str() == Some("http")
            && upstream
                .authority()
                .is_some_and(|authority| !authority.as_str().contains('@'))
            && upstream.path() == "/"
            && upstream.query().is_none(),
        "upstream must be an HTTP origin, for example http://127.0.0.1:3001"
    );

    let data = TcpListener::bind(config.listen)
        .await
        .context("bind proxy data listener")?;
    let management = TcpListener::bind(config.management_listen)
        .await
        .context("bind proxy management listener")?;
    let proxy = proxy::Proxy::new(upstream, Duration::from_millis(config.upstream_timeout_ms));
    let metrics = proxy.metrics.clone();
    let management_router = Router::new()
        .route(
            "/metrics",
            get(|State(metrics): State<metrics::Metrics>| async move { Json(metrics.snapshot()) }),
        )
        .with_state(metrics);
    let (shutdown, mut management_shutdown) = watch::channel(false);
    let data_shutdown = shutdown.subscribe();
    let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;

    tracing::info!(listen = %data.local_addr()?, management = %management.local_addr()?, "proxy listening");
    let mut serving = std::pin::pin!(async {
        tokio::try_join!(
            server::serve(data, proxy, config.max_connections, data_shutdown),
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
