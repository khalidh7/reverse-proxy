use std::{io, time::Duration};

use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

use crate::proxy::Proxy;

pub async fn serve(
    listener: TcpListener,
    proxy: Proxy,
    max_connections: usize,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            Some(result) = connections.join_next() => {
                if let Err(error) = result {
                    tracing::error!(%error, "connection task failed");
                }
            }
            accepted = listener.accept(), if connections.len() < max_connections => {
                let (socket, peer) = accepted?;
                let proxy = proxy.clone();
                let mut shutdown = shutdown.clone();
                connections.spawn(async move {
                    let connection = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(10))
                        .serve_connection(TokioIo::new(socket), service_fn(move |request| {
                            let proxy = proxy.clone();
                            async move { Ok::<_, std::convert::Infallible>(proxy.forward(request).await) }
                        }));
                    tokio::pin!(connection);
                    let result = tokio::select! {
                        result = &mut connection => result,
                        _ = shutdown.changed() => {
                            connection.as_mut().graceful_shutdown();
                            connection.await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(%peer, %error, "downstream connection ended with an error");
                    }
                });
            }
        }
    }
    drop(listener);
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "connection task failed during shutdown");
        }
    }
    Ok(())
}
