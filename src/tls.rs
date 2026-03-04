// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense
//
//! IEC 62351-3 compliant Mutual TLS (mTLS) support.
//!
//! # Overview
//!
//! IEC 62351-3 specifies TLS with mutual authentication for IEC 60870-5-104
//! transport-layer security.  Concretely this means:
//!
//! * The server presents its own certificate to the client.
//! * The server **requires** a certificate from the client.
//! * Both certificates must be signed by the same trusted Root CA.
//!
//! # Architecture
//!
//! `lib60870` manages its own loopback TCP listener.  When TLS is enabled the
//! bridge runs a second listener on the IEC 62351-3 well-known port (19998)
//! that terminates mTLS and proxies the plaintext traffic to `lib60870`.
//!
//! ```text
//!  SCADA Master (TLS client)
//!       │  TCP :19998
//!       ▼
//!  ┌─────────────────────────┐
//!  │  TLS listener (this mod)│  ← mTLS handshake + client-cert verification
//!  └────────────┬────────────┘
//!               │ plaintext  TCP 127.0.0.1:2404
//!               ▼
//!  ┌─────────────────────────┐
//!  │  lib60870 IEC-104 server│
//!  └─────────────────────────┘
//! ```
//!
//! # Required environment variables (when `TLS_ENABLED=true`)
//!
//! | Variable | Description |
//! |---|---|
//! | `TLS_CERT_PATH` | PEM file with the server's certificate chain |
//! | `TLS_KEY_PATH` | PEM file with the server's private key (PKCS-8 or PKCS-1) |
//! | `TLS_CA_CERT_PATH` | PEM file with the Root CA certificate used to verify client certs |
//!
//! # Note on cipher suites
//!
//! `rustls` only exposes safe modern cipher suites by default.  No additional
//! configuration is required to meet IEC 62351-3's cryptographic requirements.

use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Context as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::ServerConfig;
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

// ─── Certificate / key loading ────────────────────────────────────────────────

/// Load all PEM-encoded certificates from `path`.
///
/// The file may contain a full chain (server cert followed by intermediates).
/// Each `-----BEGIN CERTIFICATE-----` block is returned as one entry.
pub fn load_certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path)
        .with_context(|| format!("Cannot open certificate file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .with_context(|| format!("Failed to parse certificate PEM: {}", path.display()))?;

    if certs.is_empty() {
        anyhow::bail!("No certificates found in {}", path.display());
    }

    Ok(certs)
}

/// Load the first PEM-encoded private key from `path`.
///
/// Supports PKCS-8 (`BEGIN PRIVATE KEY`), PKCS-8 encrypted, RSA
/// (`BEGIN RSA PRIVATE KEY`), and EC (`BEGIN EC PRIVATE KEY`) encodings.
pub fn load_private_key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    let file = File::open(path)
        .with_context(|| format!("Cannot open private key file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let key = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("Failed to parse private key PEM: {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("No private key found in {}", path.display()))?;

    Ok(key)
}

// ─── TlsConfig ────────────────────────────────────────────────────────────────

/// Paths to the PEM files required for mTLS.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Server certificate chain (PEM).
    pub cert_path: String,
    /// Server private key (PEM).
    pub key_path: String,
    /// Root CA certificate used to verify **client** certificates (PEM).
    pub ca_cert_path: String,
}

// ─── TlsAcceptor builder ──────────────────────────────────────────────────────

/// Build a [`TlsAcceptor`] from a [`TlsConfig`].
///
/// The resulting acceptor:
/// * Presents the server certificate chain to every connecting client.
/// * Requires a client certificate signed by the CA in `cfg.ca_cert_path`.
/// * Uses rustls's default safe cipher suites (AES-GCM, ChaCha20-Poly1305,
///   TLS 1.2 and TLS 1.3) — no unsafe options are enabled.
///
/// # Errors
/// Returns an error if any PEM file cannot be read, contains no usable
/// material, or if the `rustls` configuration is inconsistent (e.g. the key
/// does not match the certificate).
pub fn build_acceptor(cfg: &TlsConfig) -> anyhow::Result<TlsAcceptor> {
    // ── CA / client-verifier ─────────────────────────────────────────────────
    let ca_certs = load_certs(Path::new(&cfg.ca_cert_path))
        .context("Loading TLS CA certificate")?;

    let mut root_store = rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store
            .add(cert)
            .context("Adding CA certificate to root store")?;
    }

    // WebPkiClientVerifier rejects connections that provide no certificate or
    // whose certificate chain does not validate against `root_store`.
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build client cert verifier: {e}"))?;

    // ── Server certificate + key ──────────────────────────────────────────────
    let server_certs = load_certs(Path::new(&cfg.cert_path))
        .context("Loading TLS server certificate")?;

    let server_key = load_private_key(Path::new(&cfg.key_path))
        .context("Loading TLS server private key")?;

    // ── ServerConfig ──────────────────────────────────────────────────────────
    let server_config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(server_certs, server_key)
        .context("Building rustls ServerConfig")?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

// ─── MaybeTlsStream ───────────────────────────────────────────────────────────

/// A unified stream type that is either a raw TCP connection or a TLS one.
///
/// Both variants implement [`AsyncRead`] and [`AsyncWrite`], so downstream
/// code (e.g. `tokio::io::copy_bidirectional`) can work with either mode
/// without branching.
#[allow(dead_code)] // Plaintext variant reserved for future plaintext-passthrough use
pub enum MaybeTlsStream {
    /// Plain TCP — TLS disabled (plaintext mode, port 2404).
    Plaintext(TcpStream),
    /// TLS-wrapped TCP — mTLS mode (port 19998).
    Tls(tokio_rustls::server::TlsStream<TcpStream>),
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plaintext(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s)       => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Plaintext(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s)       => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plaintext(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s)       => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plaintext(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s)       => Pin::new(s).poll_shutdown(cx),
        }
    }
}

// ─── TLS accept helper ────────────────────────────────────────────────────────

/// Perform the TLS handshake on an incoming TCP stream.
///
/// * Logs the peer address and, on success, the subject of the client
///   certificate (for audit purposes).
/// * Returns `Err` on handshake failure (expired certificate, unknown CA,
///   missing client cert, etc.) so the caller can drop the connection cleanly
///   without panicking.
pub async fn accept_tls(
    tcp: TcpStream,
    acceptor: &TlsAcceptor,
    peer: SocketAddr,
) -> anyhow::Result<MaybeTlsStream> {
    info!(peer = %peer, "Starting TLS handshake");

    let tls = acceptor.accept(tcp).await.map_err(|e| {
        // Log here so the caller does not need to know the peer address.
        warn!(
            peer  = %peer,
            error = %e,
            "TLS handshake failed – connection dropped"
        );
        anyhow::anyhow!("TLS handshake with {peer} failed: {e}")
    })?;

    // Log the number of peer certificate DER bytes for audit traceability.
    // (Full X.509 parsing would require an additional dep; DER length is
    // sufficient to confirm a cert was presented and accepted.)
    let cert_len = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.len())
        .unwrap_or(0);

    info!(
        peer     = %peer,
        cert_der_bytes = cert_len,
        "TLS handshake completed – client certificate accepted"
    );

    Ok(MaybeTlsStream::Tls(tls))
}

// ─── TLS proxy ────────────────────────────────────────────────────────────────

/// Spawn a task that:
/// 1. Completes the TLS handshake for `tcp`.
/// 2. Opens a loopback TCP connection to `iec104_local_addr` (lib60870 server).
/// 3. Bidirectionally proxies raw bytes between the two sockets.
///
/// The task is fire-and-forget: errors are logged but never surface to the
/// caller.  Call this once per accepted TCP connection from the TLS listener.
pub fn spawn_tls_proxy(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    peer: SocketAddr,
    iec104_local_addr: SocketAddr,
) {
    tokio::spawn(async move {
        // ── TLS handshake ─────────────────────────────────────────────────────
        let mut tls_stream = match accept_tls(tcp, &acceptor, peer).await {
            Ok(s)  => s,
            Err(_) => return, // error already logged by accept_tls
        };

        // ── Connect to lib60870 loopback ──────────────────────────────────────
        let mut local = match TcpStream::connect(iec104_local_addr).await {
            Ok(s)  => s,
            Err(e) => {
                error!(
                    peer  = %peer,
                    local = %iec104_local_addr,
                    error = %e,
                    "Failed to connect to local IEC-104 server"
                );
                return;
            }
        };

        info!(
            peer  = %peer,
            local = %iec104_local_addr,
            "Proxy session established"
        );

        // ── Bidirectional copy ────────────────────────────────────────────────
        match tokio::io::copy_bidirectional(&mut tls_stream, &mut local).await {
            Ok((from_tls, from_local)) => {
                info!(
                    peer       = %peer,
                    from_tls   = from_tls,
                    from_local = from_local,
                    "Proxy session closed"
                );
            }
            Err(e) => {
                // A broken-pipe / reset is normal when a SCADA client
                // disconnects; log at debug level to avoid noise.
                use std::io::ErrorKind::{BrokenPipe, ConnectionReset};
                if matches!(e.kind(), BrokenPipe | ConnectionReset) {
                    tracing::debug!(peer = %peer, error = %e, "Proxy session ended (peer disconnected)");
                } else {
                    warn!(peer = %peer, error = %e, "Proxy session error");
                }
            }
        }
    });
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_pem(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f
    }

    // ── load_certs ────────────────────────────────────────────────────────────

    #[test]
    fn load_certs_rejects_empty_file() {
        let f = write_temp_pem(b"");
        assert!(load_certs(f.path()).is_err());
    }

    #[test]
    fn load_certs_rejects_non_pem() {
        let f = write_temp_pem(b"not a certificate");
        assert!(load_certs(f.path()).is_err());
    }

    #[test]
    fn load_certs_missing_file_is_error() {
        assert!(load_certs(Path::new("/nonexistent/cert.pem")).is_err());
    }

    // ── load_private_key ──────────────────────────────────────────────────────

    #[test]
    fn load_private_key_missing_file_is_error() {
        assert!(load_private_key(Path::new("/nonexistent/key.pem")).is_err());
    }

    #[test]
    fn load_private_key_rejects_empty_file() {
        let f = write_temp_pem(b"");
        assert!(load_private_key(f.path()).is_err());
    }
}
