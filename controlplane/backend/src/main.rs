mod supervisor;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use client::Workload;
use http_body_util::{BodyExt, Limited};
use hyper::Uri;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};
use serde_json::{Value, json};
use supervisor::{ApiError, SERVICES, Supervisor};
use tokio::{net::TcpListener, signal, time::timeout};

type HttpClient = Client<HttpConnector, Body>;

#[derive(Clone)]
struct App {
    supervisor: Arc<Supervisor>,
    assets: PathBuf,
    http: HttpClient,
    container: bool,
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
    let container = std::env::var_os("LAB_CONTAINER").is_some();
    let app = App {
        supervisor: Arc::new(Supervisor::new(binary_dir, root.clone(), container)),
        assets: root.join("controlplane/frontend/dist"),
        container,
        http: Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .build_http(),
    };
    let listener = TcpListener::bind(if container {
        "0.0.0.0:9090"
    } else {
        "127.0.0.1:9090"
    })
    .await?;
    let host = "127.0.0.1:9090";
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
        .route("/api/routing", put(update_routing))
        .route("/api/routing/clear-counts", post(clear_routed_counts))
        .route("/api/services/{id}/start", post(start_service))
        .route("/api/services/{id}/stop", post(stop_service))
        .route("/api/workload/start", post(start_workload))
        .route("/api/workload/stop", post(stop_workload))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let valid = request.headers().get(header::HOST).is_some_and(|value| value == host)
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
    let (metrics, routing, app1, app2, app3) = tokio::join!(
        probe(&app, 0, "/metrics", true),
        probe(&app, 0, "/routing", true),
        probe(&app, 1, "/health", false),
        probe(&app, 2, "/health", false),
        probe(&app, 3, "/health", false),
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
    Json(json!({
        "services": services, "workload": workload, "proxy_metrics": metrics,
        "routing": routing, "service_controls": !app.container,
    }))
}

fn management_uri(app: &App, index: usize, path: &str) -> Uri {
    let service = SERVICES[index];
    let host = if app.container {
        service.id
    } else {
        "127.0.0.1"
    };
    format!("http://{host}:{}{path}", service.management_port)
        .parse()
        .expect("fixed management URI")
}

async fn probe(app: &App, index: usize, path: &str, json: bool) -> Option<Value> {
    timeout(Duration::from_millis(750), async {
        let response = app.http.get(management_uri(app, index, path)).await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bytes = Limited::new(response.into_body(), 16_384)
            .collect()
            .await
            .ok()?
            .to_bytes();
        if json {
            serde_json::from_slice(&bytes).ok()
        } else {
            Some(Value::Bool(true))
        }
    })
    .await
    .ok()
    .flatten()
}

async fn update_routing(
    State(app): State<App>,
    Json(update): Json<Value>,
) -> Result<Response, ApiError> {
    let request = hyper::Request::put(management_uri(&app, 0, "/routing"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(update.to_string()))
        .expect("valid routing request");
    send_routing_command(&app, request).await
}

async fn clear_routed_counts(State(app): State<App>) -> Result<Response, ApiError> {
    let request = hyper::Request::post(management_uri(&app, 0, "/routing/clear-counts"))
        .body(Body::empty())
        .expect("valid routing request");
    send_routing_command(&app, request).await
}

async fn send_routing_command(
    app: &App,
    request: hyper::Request<Body>,
) -> Result<Response, ApiError> {
    timeout(Duration::from_secs(2), async {
        let response = app.http.request(request).await.ok()?;
        let (parts, body) = response.into_parts();
        let bytes = Limited::new(body, 16_384).collect().await.ok()?.to_bytes();
        Some(Response::from_parts(parts, Body::from(bytes)))
    })
    .await
    .ok()
    .flatten()
    .ok_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            "proxy management unavailable".into(),
        )
    })
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
