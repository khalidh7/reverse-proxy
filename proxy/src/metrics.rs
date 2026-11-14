use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering::Relaxed},
};

use hyper::StatusCode;
use serde::Serialize;

#[derive(Clone, Default)]
pub struct Metrics(Arc<Counters>);

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    upstream_responses: AtomicU64,
    gateway_errors: AtomicU64,
    server_errors: AtomicU64,
}

#[derive(Serialize)]
pub struct Snapshot {
    requests: u64,
    upstream_responses: u64,
    gateway_errors: u64,
    server_errors: u64,
}

impl Metrics {
    pub fn request(&self) {
        self.0.requests.fetch_add(1, Relaxed);
    }

    pub fn upstream_response(&self, status: StatusCode) {
        self.0.upstream_responses.fetch_add(1, Relaxed);
        if status.is_server_error() {
            self.0.server_errors.fetch_add(1, Relaxed);
        }
    }

    pub fn gateway_error(&self) {
        self.0.gateway_errors.fetch_add(1, Relaxed);
        self.0.server_errors.fetch_add(1, Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            requests: self.0.requests.load(Relaxed),
            upstream_responses: self.0.upstream_responses.load(Relaxed),
            gateway_errors: self.0.gateway_errors.load(Relaxed),
            server_errors: self.0.server_errors.load(Relaxed),
        }
    }
}
