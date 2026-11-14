use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::body::Body;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode, Uri, Version,
    body::{Bytes, Incoming},
    header::{self, HeaderName},
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};
use tokio::time::timeout;

use crate::{
    load_balancer::{ActiveRequest, LoadBalancer},
    metrics::Metrics,
};

#[derive(Clone)]
pub struct Proxy {
    balancer: Arc<LoadBalancer>,
    client: Client<HttpConnector, Body>,
    upstream_timeout: Duration,
    pub metrics: Metrics,
}

impl Proxy {
    pub fn new(balancer: Arc<LoadBalancer>, upstream_timeout: Duration) -> Self {
        Self {
            balancer,
            client: Client::builder(TokioExecutor::new())
                .pool_timer(TokioTimer::new())
                .retry_canceled_requests(false)
                .build_http(),
            upstream_timeout,
            metrics: Metrics::default(),
        }
    }

    pub async fn forward(&self, request: Request<Incoming>) -> Response<Body> {
        self.metrics.request();
        let (mut parts, body) = request.into_parts();
        if parts.method == Method::CONNECT || parts.headers.contains_key(header::UPGRADE) {
            return error(
                StatusCode::NOT_IMPLEMENTED,
                "CONNECT and protocol upgrades are unsupported",
            );
        }
        let Some(path) = parts
            .uri
            .path_and_query()
            .filter(|path| path.path().starts_with('/'))
        else {
            return error(StatusCode::BAD_REQUEST, "expected a request path");
        };
        let Some(active) = self.balancer.select() else {
            self.metrics.gateway_error();
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no healthy enabled upstream",
            );
        };
        let upstream = &active.0.origin;
        let Ok(uri) = Uri::builder()
            .scheme(upstream.scheme().unwrap().clone())
            .authority(upstream.authority().unwrap().clone())
            .path_and_query(path.clone())
            .build()
        else {
            return error(StatusCode::BAD_REQUEST, "invalid request path");
        };

        strip_hop_headers(&mut parts.headers);
        for name in [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-port",
            "x-forwarded-proto",
            "x-real-ip",
        ] {
            parts.headers.remove(name);
        }
        parts.headers.remove(header::HOST); // Hyper sets Host from the configured URI.
        parts.uri = uri;
        parts.version = Version::HTTP_11;
        // Hyper independently frames each hop.
        let request = Request::from_parts(parts, Body::new(DataOnly(body, None)));

        match timeout(self.upstream_timeout, self.client.request(request)).await {
            Ok(Ok(response)) if response.status() != StatusCode::SWITCHING_PROTOCOLS => {
                self.metrics.upstream_response(response.status());
                let (mut parts, body) = response.into_parts();
                strip_hop_headers(&mut parts.headers);
                Response::from_parts(parts, Body::new(DataOnly(body, Some(active))))
            }
            Ok(Ok(_)) => {
                self.metrics.gateway_error();
                error(
                    StatusCode::BAD_GATEWAY,
                    "unexpected upstream protocol upgrade",
                )
            }
            Ok(Err(err)) => {
                self.metrics.gateway_error();
                tracing::debug!(error = %err, upstream = %upstream, "upstream request failed");
                error(StatusCode::BAD_GATEWAY, "upstream request failed")
            }
            Err(_) => {
                self.metrics.gateway_error();
                tracing::debug!(upstream = %upstream, "upstream request timed out");
                error(StatusCode::GATEWAY_TIMEOUT, "upstream request timed out")
            }
        }
    }
}

// Drop trailers while retaining the original body's framing hints.
struct DataOnly(Incoming, Option<ActiveRequest>);

impl HttpBody for DataOnly {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        loop {
            match Pin::new(&mut self.0).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) if !frame.is_data() => continue,
                result => {
                    if matches!(result, Poll::Ready(None | Some(Err(_)))) || self.0.is_end_stream()
                    {
                        self.1.take();
                    }
                    return result;
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.0.size_hint()
    }
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let connection_headers: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .filter_map(|name| HeaderName::from_bytes(name.trim_ascii()).ok())
        .collect();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn error(status: StatusCode, message: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(message))
        .expect("valid static response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_all_connection_options_and_preserves_end_to_end_headers() {
        let mut headers = HeaderMap::new();
        headers.append(header::CONNECTION, " Keep-Alive, X-Hop ".parse().unwrap());
        headers.append(header::CONNECTION, "x-other".parse().unwrap());
        for name in [
            "keep-alive",
            "x-hop",
            "x-other",
            "proxy-connection",
            "trailer",
            "content-type",
        ] {
            headers.insert(name, "value".parse().unwrap());
        }
        strip_hop_headers(&mut headers);
        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key(header::CONTENT_TYPE));
    }
}
