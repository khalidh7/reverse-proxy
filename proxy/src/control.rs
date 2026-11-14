use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};

use crate::{
    load_balancer::{LoadBalancer, Snapshot, Update, UpdateError},
    metrics::Metrics,
};

#[derive(Clone)]
struct Management {
    metrics: Metrics,
    balancer: Arc<LoadBalancer>,
}

pub fn router(metrics: Metrics, balancer: Arc<LoadBalancer>) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(|State(state): State<Management>| async move { Json(state.metrics.snapshot()) }),
        )
        .route(
            "/routing",
            get(|State(state): State<Management>| async move { Json(state.balancer.snapshot()) })
                .put(update),
        )
        .route(
            "/routing/clear-counts",
            post(|State(state): State<Management>| async move {
                Json(state.balancer.clear_routed_counts())
            }),
        )
        .layer(DefaultBodyLimit::max(4096))
        .with_state(Management { metrics, balancer })
}

async fn update(
    State(state): State<Management>,
    Json(update): Json<Update>,
) -> Result<Json<Snapshot>, (StatusCode, &'static str)> {
    state
        .balancer
        .update(update)
        .map(Json)
        .map_err(|error| match error {
            UpdateError::Stale => (
                StatusCode::CONFLICT,
                "routing settings changed; refresh and try again",
            ),
            UpdateError::InvalidUpstreams => {
                (StatusCode::BAD_REQUEST, "unknown or duplicate upstream ID")
            }
        })
}
