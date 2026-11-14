//! Workload options and the JSON result written by the standalone client.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const MAX_REQUESTS: usize = 100_000;
pub const MAX_CONCURRENCY: usize = 256;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub requests: usize,
    pub concurrency: usize,
}

impl Workload {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_REQUESTS).contains(&self.requests),
            "requests must be 1–{MAX_REQUESTS}"
        );
        ensure!(
            (1..=MAX_CONCURRENCY.min(self.requests)).contains(&self.concurrency),
            "concurrency must be 1–{MAX_CONCURRENCY} and at most the request count"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
pub struct Report {
    pub requests: usize,
    pub concurrency: usize,
    pub successful: usize,
    pub failed: usize,
    pub elapsed_ms: f64,
    pub requests_per_second: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}
