// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

//! The server's TLS identity, read from files and read again when they change.
//!
//! Whatever issues the certificate (an ACME client, an operator, a secrets agent) only has to
//! replace the files. New connections use the new certificate; open ones are left alone.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use rustls::ServerConfig;
use rustls::version::{TLS12, TLS13};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use crate::config::Tls;

/// How often the files are looked at.
const CHECK_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{0} holds no certificate")]
    NoCertificate(PathBuf),
    #[error("{0} holds no private key")]
    NoPrivateKey(PathBuf),
    #[error("certificate and key cannot be used: {0}")]
    Unusable(#[from] rustls::Error),
}

type Stamp = (Option<SystemTime>, Option<SystemTime>);

/// The current TLS configuration, shared by every listener.
pub struct Identity {
    paths: Tls,
    current: RwLock<Arc<ServerConfig>>,
}

fn open(path: &Path) -> Result<BufReader<File>, TlsError> {
    File::open(path).map(BufReader::new).map_err(|source| TlsError::Read { path: path.to_owned(), source })
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|metadata| metadata.modified()).ok()
}

/// Builds the configuration: TLS 1.3, and TLS 1.2 with the forward-secret AEAD suites that are
/// all this library offers for it. No client certificates, no older versions.
fn load(paths: &Tls) -> Result<ServerConfig, TlsError> {
    let read = |path: &Path, source| TlsError::Read { path: path.to_owned(), source };
    let certificates = rustls_pemfile::certs(&mut open(&paths.certificate)?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| read(&paths.certificate, source))?;
    if certificates.is_empty() {
        return Err(TlsError::NoCertificate(paths.certificate.clone()));
    }
    let key = rustls_pemfile::private_key(&mut open(&paths.private_key)?)
        .map_err(|source| read(&paths.private_key, source))?
        .ok_or_else(|| TlsError::NoPrivateKey(paths.private_key.clone()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13, &TLS12])?
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    Ok(config)
}

impl Identity {
    /// Reads the certificate and key. Fails when they cannot be used, so that a server never
    /// starts without a working identity.
    pub fn load(paths: &Tls) -> Result<Arc<Self>, TlsError> {
        let config = load(paths)?;
        Ok(Arc::new(Self { paths: paths.clone(), current: RwLock::new(Arc::new(config)) }))
    }

    fn stamp(&self) -> Stamp {
        (modified(&self.paths.certificate), modified(&self.paths.private_key))
    }

    /// An acceptor with the certificate as it is now. `protocol` is the name to agree on during
    /// the handshake, for protocols that have one registered.
    pub fn acceptor(&self, protocol: Option<&[u8]>) -> TlsAcceptor {
        let current = self.current.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let mut config = (*current).clone();
        config.alpn_protocols = protocol.map(|name| name.to_vec()).into_iter().collect();
        TlsAcceptor::from(Arc::new(config))
    }

    /// Watches the files and takes over a new certificate when they change. A pair that cannot
    /// be used is reported and the old one stays in place.
    pub async fn keep_fresh(self: Arc<Self>) {
        let mut seen = self.stamp();
        loop {
            tokio::time::sleep(CHECK_EVERY).await;
            let now = self.stamp();
            if now == seen {
                continue;
            }
            match load(&self.paths) {
                Ok(config) => {
                    *self.current.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(config);
                    seen = now;
                    info!("took over a new TLS certificate");
                }
                // Not remembered as seen: the files may be half written, so look again next time.
                Err(error) => warn!(%error, "the TLS files changed but cannot be used; keeping the old certificate"),
            }
        }
    }
}
