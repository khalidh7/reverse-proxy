mod supervisor;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use client::Workload;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Uri, body::Bytes};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};
use serde_json::{Value, json};
use supervisor::{ApiError, SERVICES, Supervisor};
use tokio::{net::TcpListener, signal, time::timeout};

type HttpClient = Client<HttpConnector, Empty<Bytes>>;

#[derive(Clone)]
struct App {
    supervisor: Arc<Supervisor>,
    assets: PathBuf,
    http: HttpClient,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let root = std::env::current_dir()?;
    let binary = std::env::current_exe()?;
    let binary_dir = binary
        .parent()
        .context("locate sibling binaries")?
        .to_path_buf();
    let app = App {
        supervisor: Arc::new(Supervisor::new(binary_dir, root.clone())),
        assets: root.join("controlplane/frontend/dist"),
        http: Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .build_http(),
    };
    let listener = TcpListener::bind("127.0.0.1:9090").await?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("http://{host}");
    let router = Router::new()
        .route("/", get(|| async { axum::response::Html(include_str!("../../frontend/index.html")) }))
        .route("/assets/main.js", get(|State(app): State<App>| async move {
            asset(app, "main.js", "text/javascript").await
        }))
        .route("/assets/main.css", get(|State(app): State<App>| async move {
            asset(app, "main.css", "text/css").await
        }))
        .route("/api/status", get(status))
        .route("/api/services/{id}/start", post(start_service))
        .route("/api/services/{id}/stop", post(stop_service))
        .route("/api/workload/start", post(start_workload))
        .route("/api/workload/stop", post(stop_workload))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let valid = request.headers().get(header::HOST).is_some_and(|value| value == host.as_str())
                && request.headers().get(header::ORIGIN).is_none_or(|value| value == origin.as_str());
            async move {
                if !valid {
                    return (StatusCode::FORBIDDEN, "unexpected host or origin").into_response();
                }
                next.run(request).await
            }
        }))
        .with_state(app);

    let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    tracing::info!(url = %format_args!("http://{}", listener.local_addr()?), "control plane listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        })
        .await?;
    Ok(())
}

async fn asset(app: App, name: &'static str, content_type: &'static str) -> Response {
    match tokio::fs::read(app.assets.join(name)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type)], bytes).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            "build the dashboard with npm run build",
        )
            .into_response(),
    }
}

async fn status(State(app): State<App>) -> Json<Value> {
    let (metrics, app1, app2, app3) = tokio::join!(
        probe(&app.http, SERVICES[0].management_port, true),
        probe(&app.http, SERVICES[1].management_port, false),
        probe(&app.http, SERVICES[2].management_port, false),
        probe(&app.http, SERVICES[3].management_port, false),
    );
    let (workload, processes) = app.supervisor.snapshot().await;
    let online = [
        metrics.is_some(),
        app1.is_some(),
        app2.is_some(),
        app3.is_some(),
    ];
    let services: Vec<_> = SERVICES
        .iter()
        .enumerate()
        .map(|(index, service)| {
            json!({
                "id": service.id,
                "port": service.port,
                "online": online[index],
                "process": processes[index],
            })
        })
        .collect();
    Json(json!({ "services": services, "workload": workload, "proxy_metrics": metrics }))
}

async fn probe(client: &HttpClient, port: u16, metrics: bool) -> Option<Value> {
    let path = if metrics { "/metrics" } else { "/health" };
    let url = format!("http://127.0.0.1:{port}{path}");
    timeout(Duration::from_millis(750), async {
        let mut response = client.get(url.parse::<Uri>().ok()?).await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bytes = Limited::new(response.body_mut(), 4096)
            .collect()
            .await
            .ok()?
            .to_bytes();
        if metrics {
            serde_json::from_slice(&bytes).ok()
        } else {
            Some(Value::Bool(true))
        }
    })
    .await
    .ok()
    .flatten()
}

async fn start_service(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.supervisor.start_service(&id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn stop_service(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.supervisor.stop_service(&id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn start_workload(
    State(app): State<App>,
    Json(workload): Json<Workload>,
) -> Result<StatusCode, ApiError> {
    app.supervisor.start_client(workload).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn stop_workload(State(app): State<App>) -> Result<StatusCode, ApiError> {
    app.supervisor.stop_client().await?;
    Ok(StatusCode::ACCEPTED)
}
