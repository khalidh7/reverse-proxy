use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use client::{Report, Workload};
use http_body_util::{BodyExt, Empty};
use hyper::{Request, Uri, body::Bytes};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};
use tokio::{task::JoinSet, time::timeout};

type HttpClient = Client<HttpConnector, Empty<Bytes>>;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let uri: Uri = args
        .next()
        .as_deref()
        .unwrap_or("http://127.0.0.1:8080/")
        .parse()?;
    let workload = Workload {
        requests: args
            .next()
            .as_deref()
            .unwrap_or("1000")
            .parse()
            .context("parse request count")?,
        concurrency: args
            .next()
            .as_deref()
            .unwrap_or("16")
            .parse()
            .context("parse concurrency")?,
    };
    ensure!(
        args.next().is_none(),
        "usage: client [HTTP_URL] [REQUESTS] [CONCURRENCY]"
    );
    ensure!(
        uri.scheme_str() == Some("http") && uri.authority().is_some(),
        "target must be an absolute HTTP URL"
    );
    workload.validate()?;

    let client = Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .retry_canceled_requests(false)
        .build_http();
    let next = Arc::new(AtomicUsize::new(0));
    let mut workers = JoinSet::new();
    let started = Instant::now();

    for _ in 0..workload.concurrency {
        let (client, uri, next) = (client.clone(), uri.clone(), next.clone());
        workers.spawn(async move {
            let mut successful = 0;
            let mut latencies = Vec::new();
            while next.fetch_add(1, Relaxed) < workload.requests {
                let started = Instant::now();
                if timeout(Duration::from_secs(10), send(&client, &uri))
                    .await
                    .unwrap_or(false)
                {
                    successful += 1;
                }
                latencies.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            (successful, latencies)
        });
    }

    let mut successful = 0;
    let mut latencies = Vec::with_capacity(workload.requests);
    while let Some(result) = workers.join_next().await {
        let (count, samples) = result?;
        successful += count;
        latencies.extend(samples);
    }
    let elapsed = started.elapsed().as_secs_f64();
    latencies.sort_unstable_by(f64::total_cmp);
    let percentile = |percent: usize| latencies[(latencies.len() * percent).div_ceil(100) - 1];
    let report = Report {
        requests: workload.requests,
        concurrency: workload.concurrency,
        successful,
        failed: workload.requests - successful,
        elapsed_ms: elapsed * 1000.0,
        requests_per_second: workload.requests as f64 / elapsed,
        mean_ms: latencies.iter().sum::<f64>() / latencies.len() as f64,
        p50_ms: percentile(50),
        p95_ms: percentile(95),
        p99_ms: percentile(99),
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

async fn send(client: &HttpClient, uri: &Uri) -> bool {
    let request = Request::get(uri.clone())
        .body(Empty::new())
        .expect("valid GET request");
    let Ok(mut response) = client.request(request).await else {
        return false;
    };
    while let Some(frame) = response.body_mut().frame().await {
        if frame.is_err() {
            return false;
        }
    }
    response.status().is_success()
}
