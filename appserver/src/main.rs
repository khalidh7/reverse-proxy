use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use tokio::{net::TcpListener, time::sleep};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_DELAY_MS: u64 = 10_000;

#[derive(Deserialize)]
struct Payload {
    bytes: usize,
}

#[derive(Deserialize)]
struct Delay {
    ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let id = args.next().unwrap_or_else(|| "app-1".into());
    let listen: SocketAddr = args.next().as_deref().unwrap_or("127.0.0.1:3001").parse()?;
    let management: SocketAddr = args.next().as_deref().unwrap_or("127.0.0.1:9301").parse()?;
    ensure!(
        management.ip().is_loopback(),
        "management listener must be loopback"
    );
    ensure!(
        args.next().is_none(),
        "usage: appserver [ID] [LISTEN] [MANAGEMENT_LISTEN]"
    );
    let app = Router::new()
        .route(
            "/",
            get(|State(id): State<String>| async move { format!("hello from {id}\n") }),
        )
        .route("/health", get(|| async { "ok\n" }))
        .route("/payload", get(payload))
        .route("/delay", get(delay))
        .route("/echo", post(echo))
        .route(
            "/fail",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "simulated failure\n") }),
        )
        .layer(DefaultBodyLimit::max(MAX_BYTES))
        .with_state(id.clone());
    let status = Router::new().route("/health", get(|| async { "ok\n" }));
    let listener = TcpListener::bind(listen)
        .await
        .context("bind app data listener")?;
    let management = TcpListener::bind(management)
        .await
        .context("bind app management listener")?;
    tracing::info!(%id, listen = %listener.local_addr()?, management = %management.local_addr()?, "app server listening");
    tokio::try_join!(async { axum::serve(listener, app).await }, async {
        axum::serve(management, status).await
    },)?;
    Ok(())
}

async fn payload(Query(query): Query<Payload>) -> Response {
    if query.bytes > MAX_BYTES {
        return (StatusCode::BAD_REQUEST, "maximum payload is 1048576 bytes").into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        Body::from(vec![b'x'; query.bytes]),
    )
        .into_response()
}

async fn delay(Query(query): Query<Delay>, State(id): State<String>) -> Response {
    if query.ms > MAX_DELAY_MS {
        return (StatusCode::BAD_REQUEST, "maximum delay is 10000 ms").into_response();
    }
    sleep(Duration::from_millis(query.ms)).await;
    format!("delayed response from {id}\n").into_response()
}

async fn echo(body: Bytes) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/octet-stream")], body)
}
