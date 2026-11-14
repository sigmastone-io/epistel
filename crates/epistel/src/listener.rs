// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! Accepting connections: TCP, then TLS, then the protocol.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::server::TlsStream;
use tracing::{debug, info, warn};

use crate::tls::Identity;

/// How long a client gets to finish the TLS handshake.
const HANDSHAKE_PATIENCE: Duration = Duration::from_secs(15);
/// How many connections one listener serves at a time. Further ones wait in the accept queue.
const MOST_CONNECTIONS: usize = 2048;

/// Accepts connections on `address` until `shutdown` completes, and runs `serve` on each once it
/// is inside TLS. There is no other way in: a client that does not speak TLS gets nothing.
pub async fn run<S, F>(
    protocol: &'static str,
    negotiated: Option<&'static [u8]>,
    address: SocketAddr,
    identity: Arc<Identity>,
    shutdown: impl Future<Output = ()>,
    serve: S,
) -> std::io::Result<()>
where
    S: Fn(TlsStream<TcpStream>) -> F + Send + Sync + 'static,
    F: Future<Output = std::io::Result<()>> + Send + 'static,
{
    let listener = TcpListener::bind(address).await?;
    info!(protocol, %address, "listening (implicit TLS)");
    let serve = Arc::new(serve);
    let slots = Arc::new(Semaphore::new(MOST_CONNECTIONS));
    tokio::pin!(shutdown);
    loop {
        let slot = tokio::select! {
            () = &mut shutdown => return Ok(()),
            slot = slots.clone().acquire_owned() => slot.expect("the semaphore is never closed"),
        };
        let accepted = tokio::select! {
            () = &mut shutdown => return Ok(()),
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(connection) => connection,
            Err(error) => {
                // Out of descriptors or the like. Give it a moment instead of spinning.
                warn!(protocol, %error, "could not accept a connection");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let acceptor = identity.acceptor(negotiated);
        let serve = serve.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let stream = match tokio::time::timeout(HANDSHAKE_PATIENCE, acceptor.accept(stream)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return debug!(protocol, %peer, %error, "TLS handshake failed"),
                Err(_) => return debug!(protocol, %peer, "TLS handshake took too long"),
            };
            debug!(protocol, %peer, "connected");
            if let Err(error) = serve(stream).await {
                debug!(protocol, %peer, %error, "connection ended with an error");
            }
        });
    }
}
