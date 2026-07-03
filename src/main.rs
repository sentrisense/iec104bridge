// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense
//
//! IEC-104 bridge – subscribes to a message source and forwards JSON messages
//! to connected IEC-104 clients as spontaneous data.
//!
//! # Quick start
//!
//! ```bash
//! export NATS_URL="nats://localhost:4222"
//! export NATS_STREAM="sensors"
//! export NATS_CONSUMER="iec104bridge"
//! export NATS_SUBJECT_FILTER="plant.a.measurements.>"  # optional
//! export IEC104_PORT=2404                               # optional
//! export IEC104_CA=1                                    # optional
//! cargo run
//! ```
//!
//! # JSON message format
//!
//! Minimal:
//! ```json
//! { "ioa": 100, "value": 42.5 }
//! ```
//!
//! Full schema:
//! ```json
//! {
//!   "ioa":     100,
//!   "value":   42.5,
//!   "type":    "float",
//!   "ca":      1,
//!   "quality": "good",
//!   "cot":     "spontaneous",
//!   "timestamp": "2026-05-29T12:34:56.789Z"
//! }
//! ```

mod asdu;
mod bridge;
mod config;
mod message;
mod source;
mod tls;
mod validation;

#[cfg(test)]
mod e2e_tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use lib60870::server::ServerBuilder;
use lib60870::types::QOI_STATION;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use config::{Config, InputTransport};
use message::Iec104Message;
use source::{MessageSource, NatsSource, UnixSocketSource};
use tls::TlsConfig;

// ─── Shared metrics ───────────────────────────────────────────────────────────

type DataStore = Arc<Mutex<HashMap<(u16, u32), Iec104Message>>>;
type SharedServer = Arc<Mutex<lib60870::Server>>;
type ServerSlot = Arc<Mutex<Option<SharedServer>>>;

#[derive(Debug, Default)]
pub struct Metrics {
    pub messages_dispatched: AtomicU64,
    pub gi_responses: AtomicU64,
}

/// Why [`run_message_loop`] returned, so the supervisor knows whether to stop
/// or to re-establish the input source.
#[derive(Debug, PartialEq, Eq)]
enum LoopOutcome {
    /// Ctrl-C received — shut the bridge down.
    Shutdown,
    /// The input stream ended (source dropped, subscription closed, accept loop
    /// gone). The IEC-104 server and its cache stay up; the source is rebuilt.
    SourceEnded,
}

/// Backoff bounds for re-establishing the input source after it ends.
const SOURCE_MIN_BACKOFF: Duration = Duration::from_millis(200);
const SOURCE_MAX_BACKOFF: Duration = Duration::from_secs(5);
/// A source that stayed up at least this long is treated as a fresh incident:
/// the backoff resets so a later, unrelated drop reconnects promptly.
const SOURCE_STABLE_RUN: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let config = Config::from_env()?;
    log_startup(&config);

    let metrics: Arc<Metrics> = Arc::new(Metrics::default());
    let data_store: DataStore = Arc::new(Mutex::new(HashMap::new()));
    let server = build_iec_server(&config, Arc::clone(&data_store), Arc::clone(&metrics))?;
    spawn_tls_listener_if_enabled(&config).await?;
    spawn_metrics_http_server(
        config.metrics_port,
        Arc::clone(&metrics),
        Arc::clone(&data_store),
    );
    // The input source is supervised: if it ever ends (source dropped, NATS
    // subscription closed, accept loop gone), the IEC-104 server and its cache
    // stay up — so clients keep getting GI replies — and the source is
    // re-established with backoff. Only Ctrl-C stops the bridge.
    supervise_message_source(&config, server, data_store, metrics).await?;

    info!("Bridge stopped");
    Ok(())
}

/// Establish the input source and run the message loop, re-establishing the
/// source whenever it ends, until Ctrl-C. The IEC-104 server and data cache
/// outlive individual source instances so cached values survive a source
/// outage. The first source build fails fast (surfacing genuine misconfig);
/// every rebuild afterward is retried with bounded backoff.
async fn supervise_message_source(
    config: &Config,
    server: SharedServer,
    data_store: DataStore,
    metrics: Arc<Metrics>,
) -> anyhow::Result<()> {
    let mut source = build_message_source(config).await?;
    let mut backoff = SOURCE_MIN_BACKOFF;

    loop {
        let started = Instant::now();
        let outcome = run_message_loop(
            source,
            Arc::clone(&server),
            Arc::clone(&data_store),
            Arc::clone(&metrics),
            config.iec104_default_ca,
            config.iec104_gi_only,
        )
        .await;

        if outcome == LoopOutcome::Shutdown {
            break;
        }

        if started.elapsed() >= SOURCE_STABLE_RUN {
            backoff = SOURCE_MIN_BACKOFF;
        }
        warn!(
            backoff_ms = backoff.as_millis(),
            "input source ended; IEC-104 server stays up (cache intact), re-establishing"
        );

        // Retry building the source with backoff; Ctrl-C during the wait stops.
        source = loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    server.lock().unwrap().stop();
                    return Ok(());
                }
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(SOURCE_MAX_BACKOFF);
            match build_message_source(config).await {
                Ok(source) => break source,
                Err(error) => {
                    error!(error = %error, "failed to re-establish input source; retrying");
                }
            }
        };
    }

    server.lock().unwrap().stop();
    Ok(())
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec104bridge=info".into()),
        )
        .init();
}

fn log_startup(config: &Config) {
    if config.iec104_gi_only {
        warn!(
            "IEC104_GI_ONLY enabled: caching updates and serving points only via General Interrogation"
        );
    }

    match config.input_transport {
        InputTransport::Nats => info!(
            source = config.input_transport.as_str(),
            nats_url = %config.nats_url,
            stream = %config.nats_stream,
            consumer = %config.nats_consumer,
            filter = ?config.nats_subject_filter,
            iec104_port = config.iec104_port,
            iec104_ca = config.iec104_default_ca,
            iec104_gi_only = config.iec104_gi_only,
            metrics_port = config.metrics_port,
            tls_enabled = config.tls_enabled,
            tls_port = config.tls_port,
            "Starting IEC-104 bridge"
        ),
        InputTransport::UnixSocket => info!(
            source = config.input_transport.as_str(),
            unix_socket_path = %config.unix_socket_path,
            unix_socket_allowed_uid = ?config.unix_socket_allowed_uid,
            unix_socket_allowed_gid = ?config.unix_socket_allowed_gid,
            unix_socket_max_bytes = config.unix_socket_max_line_bytes,
            iec104_port = config.iec104_port,
            iec104_ca = config.iec104_default_ca,
            iec104_gi_only = config.iec104_gi_only,
            metrics_port = config.metrics_port,
            tls_enabled = config.tls_enabled,
            tls_port = config.tls_port,
            "Starting IEC-104 bridge"
        ),
    }
}

fn build_iec_server(
    config: &Config,
    data_store: DataStore,
    metrics: Arc<Metrics>,
) -> anyhow::Result<SharedServer> {
    let mut iec_server = ServerBuilder::new()
        .local_address(&effective_iec104_bind_addr(config))
        .local_port(config.iec104_port)
        .build()
        .ok_or_else(|| anyhow::anyhow!("Failed to build IEC-104 server"))?;

    install_connection_handlers(&mut iec_server);

    let server_slot: ServerSlot = Arc::new(Mutex::new(None));
    install_interrogation_handler(
        &mut iec_server,
        Arc::clone(&server_slot),
        data_store,
        metrics,
        config.iec104_default_ca,
    );

    iec_server.start();
    info!(port = config.iec104_port, "IEC-104 server started");

    let server = Arc::new(Mutex::new(iec_server));
    *server_slot.lock().unwrap() = Some(Arc::clone(&server));
    Ok(server)
}

fn effective_iec104_bind_addr(config: &Config) -> String {
    if config.tls_enabled {
        info!("TLS mode: binding lib60870 to loopback only (127.0.0.1)");
        "127.0.0.1".to_string()
    } else {
        config.iec104_bind_addr.clone()
    }
}

fn install_connection_handlers(iec_server: &mut lib60870::Server) {
    iec_server.set_connection_request_handler(|ip| {
        info!(remote_ip = %ip, "IEC-104 connection request – accepted");
        true
    });

    iec_server.set_connection_event_handler(|event| {
        info!(event = ?event, "IEC-104 connection event");
    });
}

fn install_interrogation_handler(
    iec_server: &mut lib60870::Server,
    server_slot: ServerSlot,
    data_store: DataStore,
    metrics: Arc<Metrics>,
    default_ca: u16,
) {
    iec_server.set_interrogation_handler(
        move |conn: &lib60870::MasterConnection, asdu: lib60870::Asdu, qoi: u8| {
            info!(qoi, "Received station interrogation");
            conn.send_act_con(&asdu, false);

            if qoi == QOI_STATION {
                replay_cached_values(&server_slot, &data_store, default_ca);
            }

            metrics.gi_responses.fetch_add(1, Ordering::Relaxed);
            conn.send_act_term(&asdu);
            true
        },
    );
}

fn replay_cached_values(server_slot: &ServerSlot, data_store: &DataStore, default_ca: u16) {
    if let Some(server) = server_slot.lock().unwrap().as_ref().cloned() {
        let store = data_store.lock().unwrap();
        let server = server.lock().unwrap();

        for msg in store.values() {
            let ca = msg.ca.unwrap_or(default_ca);
            bridge::dispatch(&bridge::LiveSink(&server), msg, ca);
        }
    }
}

async fn spawn_tls_listener_if_enabled(config: &Config) -> anyhow::Result<()> {
    if !config.tls_enabled {
        return Ok(());
    }

    let tls_cfg = TlsConfig {
        cert_path: config
            .tls_cert_path
            .clone()
            .expect("validated in Config::from_lookup"),
        key_path: config
            .tls_key_path
            .clone()
            .expect("validated in Config::from_lookup"),
        ca_cert_path: config
            .tls_ca_cert_path
            .clone()
            .expect("validated in Config::from_lookup"),
    };

    let acceptor = tls::build_acceptor(&tls_cfg)
        .map_err(|e| anyhow::anyhow!("Failed to build TLS acceptor: {e}"))?;
    let tls_bind = format!("{}:{}", config.iec104_bind_addr, config.tls_port);
    let iec104_local: std::net::SocketAddr = format!("127.0.0.1:{}", config.iec104_port).parse()?;
    let tls_listener = tokio::net::TcpListener::bind(&tls_bind)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind TLS listener on {tls_bind}: {e}"))?;

    info!(addr = %tls_bind, "TLS (IEC 62351-3) listener started");

    tokio::spawn(async move {
        loop {
            match tls_listener.accept().await {
                Ok((tcp, peer)) => {
                    tls::spawn_tls_proxy(tcp, acceptor.clone(), peer, iec104_local);
                }
                Err(e) => error!(error = %e, "TLS listener: accept error"),
            }
        }
    });

    Ok(())
}

fn spawn_metrics_http_server(metrics_port: u16, metrics: Arc<Metrics>, data_store: DataStore) {
    let metrics_clone = Arc::clone(&metrics);
    let data_store_clone = Arc::clone(&data_store);

    tokio::spawn(async move {
        let listener = match bind_metrics_listener(metrics_port).await {
            Ok(listener) => listener,
            Err(e) => {
                error!(error = %e, "Failed to bind metrics port");
                return;
            }
        };

        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };

            let metrics_ref = Arc::clone(&metrics_clone);
            let data_store_ref = Arc::clone(&data_store_clone);
            tokio::spawn(async move {
                serve_metrics_connection(stream, metrics_ref, data_store_ref).await;
            });
        }
    });
}

async fn bind_metrics_listener(metrics_port: u16) -> anyhow::Result<TcpListener> {
    let addr = format!("0.0.0.0:{metrics_port}");
    let listener = TcpListener::bind(&addr).await?;
    info!(port = metrics_port, "Metrics endpoint listening");
    Ok(listener)
}

async fn serve_metrics_connection(
    mut stream: tokio::net::TcpStream,
    metrics: Arc<Metrics>,
    data_store: DataStore,
) {
    let cache_size = data_store.lock().unwrap().len() as u64;
    let msgs_dispatched = metrics.messages_dispatched.load(Ordering::Relaxed);
    let gi_responses = metrics.gi_responses.load(Ordering::Relaxed);

    let body = format!(
        "# HELP iec104bridge_cache_size Number of data points currently cached\n\
         # TYPE iec104bridge_cache_size gauge\n\
         iec104bridge_cache_size {cache_size}\n\
         # HELP iec104bridge_messages_dispatched_total Total IEC-104 messages dispatched\n\
         # TYPE iec104bridge_messages_dispatched_total counter\n\
         iec104bridge_messages_dispatched_total {msgs_dispatched}\n\
         # HELP iec104bridge_gi_responses_total Total General Interrogation responses\n\
         # TYPE iec104bridge_gi_responses_total counter\n\
         iec104bridge_gi_responses_total {gi_responses}\n"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn build_message_source(config: &Config) -> anyhow::Result<Box<dyn MessageSource>> {
    match config.input_transport {
        InputTransport::Nats => Ok(Box::new(NatsSource::from_config(config).await?)),
        InputTransport::UnixSocket => Ok(Box::new(UnixSocketSource::from_config(config).await?)),
    }
}

/// Drive the bridge's message loop until the stream ends or Ctrl-C is received.
///
/// Returns [`LoopOutcome`] so the caller ([`supervise_message_source`]) can
/// decide whether to shut down or re-establish the source. This function does
/// not touch the server's lifecycle — the supervisor owns that — so the server
/// and its cache survive across source rebuilds. A per-message receive error is
/// logged and skipped; it does not end the loop.
///
/// Also callable with any [`MessageSource`] in integration tests (same crate).
async fn run_message_loop(
    source: Box<dyn MessageSource>,
    server: Arc<Mutex<lib60870::Server>>,
    data_store: Arc<Mutex<HashMap<(u16, u32), Iec104Message>>>,
    metrics: Arc<Metrics>,
    default_ca: u16,
    gi_only: bool,
) -> LoopOutcome {
    let mut messages = source.into_messages();

    loop {
        tokio::select! {
            biased;

            // Graceful Ctrl-C shutdown.
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal – stopping");
                return LoopOutcome::Shutdown;
            }

            result = messages.next() => {
                match result {
                    None => {
                        // The source ended. The IEC-104 server and cache are left
                        // untouched so clients can still trigger a General
                        // Interrogation for the last-known values while the
                        // supervisor re-establishes the source.
                        warn!("Input stream ended – IEC-104 server still active; source will be re-established");
                        return LoopOutcome::SourceEnded;
                    }
                    Some(Err(e)) => {
                        error!(error = %e, "Error receiving message");
                    }
                    Some(Ok(incoming)) => {
                        if let Err(e) = handle_incoming_message(
                            incoming,
                            &server,
                            &data_store,
                            &metrics,
                            default_ca,
                            gi_only,
                        ).await {
                            error!(error = %e, "Error processing message");
                        }
                    }
                }
            }
        }
    }
}

async fn handle_incoming_message(
    incoming: source::IncomingMessage,
    server: &SharedServer,
    data_store: &DataStore,
    metrics: &Arc<Metrics>,
    default_ca: u16,
    gi_only: bool,
) -> anyhow::Result<()> {
    let ca = incoming.message.ca.unwrap_or(default_ca);

    {
        let mut store = data_store.lock().unwrap();
        store.insert((ca, incoming.message.ioa), incoming.message.clone());
    }

    let dispatched = if gi_only {
        false
    } else {
        let srv = server.lock().unwrap();
        dispatch_message_if_enabled(&bridge::LiveSink(&srv), &incoming.message, ca, gi_only)
    };

    incoming.ack().await.map_err(|e| {
        anyhow::anyhow!("Failed to acknowledge message after dispatch/cache update: {e}")
    })?;

    if dispatched {
        metrics.messages_dispatched.fetch_add(1, Ordering::Relaxed);
    }

    Ok(())
}

fn dispatch_message_if_enabled<S: bridge::DataSink>(
    sink: &S,
    message: &Iec104Message,
    ca: u16,
    gi_only: bool,
) -> bool {
    if gi_only {
        return false;
    }

    bridge::dispatch(sink, message, ca);
    true
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use lib60870::server::ServerBuilder;

    use super::{
        DataStore, Metrics, SharedServer, dispatch_message_if_enabled, handle_incoming_message,
    };
    use crate::bridge::test_support::{CapturingSink, SentCall};
    use crate::message::{CotField, DataType, DataValue, Iec104Message, QualityField};
    use crate::source::IncomingMessage;

    fn sample_message() -> Iec104Message {
        Iec104Message {
            ioa: 100,
            value: DataValue::Number(42.5),
            data_type: Some(DataType::Float),
            ca: None,
            quality: QualityField::Good,
            cot: CotField::Spontaneous,
            timestamp: None,
        }
    }

    fn test_server() -> SharedServer {
        Arc::new(std::sync::Mutex::new(
            ServerBuilder::new()
                .local_address("127.0.0.1")
                .local_port(0)
                .build()
                .expect("server should build"),
        ))
    }

    #[test]
    fn dispatch_message_if_enabled_skips_when_gi_only() {
        let sink = CapturingSink::default();
        let dispatched = dispatch_message_if_enabled(&sink, &sample_message(), 7, true);

        assert!(!dispatched);
        assert!(sink.calls.borrow().is_empty());
    }

    #[test]
    fn dispatch_message_if_enabled_dispatches_when_spontaneous_enabled() {
        let sink = CapturingSink::default();
        let dispatched = dispatch_message_if_enabled(&sink, &sample_message(), 7, false);

        assert!(dispatched);
        assert!(matches!(
            sink.calls.borrow()[0],
            SentCall::MeasuredFloat {
                ca: 7,
                ioa: 100,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn handle_incoming_message_gi_only_caches_and_acks_without_dispatching() {
        let server = test_server();
        let data_store: DataStore = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let metrics = Arc::new(Metrics::default());
        let acked = Arc::new(tokio::sync::Mutex::new(false));
        let acked_flag = Arc::clone(&acked);
        let message = sample_message();

        handle_incoming_message(
            IncomingMessage::with_ack(message.clone(), move || {
                let acked_flag = Arc::clone(&acked_flag);
                async move {
                    *acked_flag.lock().await = true;
                    Ok(())
                }
            }),
            &server,
            &data_store,
            &metrics,
            7,
            true,
        )
        .await
        .unwrap();

        // Scope the std MutexGuard so it is released before the await below
        // (an `await` while holding a std lock is a clippy/deadlock hazard).
        {
            let store = data_store.lock().unwrap();
            assert_eq!(store.len(), 1);
            assert_eq!(store.get(&(7, 100)), Some(&message));
        }

        assert!(*acked.lock().await);
        assert_eq!(metrics.messages_dispatched.load(Ordering::Relaxed), 0);
    }
}
