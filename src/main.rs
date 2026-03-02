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
//!   "cot":     "spontaneous"
//! }
//! ```

mod bridge;
mod config;
mod message;
mod source;

#[cfg(test)]
mod e2e_tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use lib60870::server::ServerBuilder;
use lib60870::types::QOI_STATION;
use tracing::{error, info, warn};

use config::Config;
use message::Iec104Message;
use source::{FileSource, MessageSource, NatsSource};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec104bridge=info".into()),
        )
        .init();

    // ── Config ────────────────────────────────────────────────────────────────
    let config = Config::from_env()?;

    info!(
        source      = if config.input_file.is_some() { "file" } else { "nats" },
        nats_url    = %config.nats_url,
        stream      = %config.nats_stream,
        consumer    = %config.nats_consumer,
        filter      = ?config.nats_subject_filter,
        input_file  = ?config.input_file,
        iec104_port = config.iec104_port,
        iec104_ca   = config.iec104_default_ca,
        "Starting IEC-104 bridge"
    );

    // ── IEC-104 data cache ────────────────────────────────────────────────────
    // Stores the most-recent value for each (ca, ioa) pair so that general
    // interrogation commands can replay current values.
    let data_store: Arc<Mutex<HashMap<(u16, u32), Iec104Message>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // ── IEC-104 server ────────────────────────────────────────────────────────
    let mut iec_server = ServerBuilder::new()
        .local_address(&config.iec104_bind_addr)
        .local_port(config.iec104_port)
        .build()
        .ok_or_else(|| anyhow::anyhow!("Failed to build IEC-104 server"))?;

    // ── Connection handlers ───────────────────────────────────────────────────
    iec_server.set_connection_request_handler(|ip| {
        info!(remote_ip = %ip, "IEC-104 connection request – accepted");
        true
    });

    iec_server.set_connection_event_handler(|event| {
        info!(event = ?event, "IEC-104 connection event");
    });

    // ── Interrogation handler ─────────────────────────────────────────────────
    // The handler needs access to the Server instance to enqueue data, but the
    // Server is not yet wrapped in an Arc when the handler is registered.  We
    // work around this with an Arc<Mutex<Option<Arc<Mutex<Server>>>>> that is
    // set to Some(...) immediately after the server is promoted to an Arc.
    let server_slot: Arc<Mutex<Option<Arc<Mutex<lib60870::Server>>>>> =
        Arc::new(Mutex::new(None));

    {
        let slot_clone  = Arc::clone(&server_slot);
        let store_clone = Arc::clone(&data_store);
        let default_ca  = config.iec104_default_ca;

        iec_server.set_interrogation_handler(move |conn: &lib60870::MasterConnection, asdu: lib60870::Asdu, qoi: u8| {
            info!(qoi, "Received station interrogation");

            conn.send_act_con(&asdu, false);

            if let Some(ref srv_arc) = *slot_clone.lock().unwrap() {
                let store  = store_clone.lock().unwrap();
                let server = srv_arc.lock().unwrap();

                for (_, msg) in store.iter() {
                    let ca = msg.ca.unwrap_or(default_ca);
                    if qoi == QOI_STATION {
                        bridge::dispatch(&bridge::LiveSink(&server), msg, ca);
                    }
                    // For group-specific interrogations (qoi 21–36) filter by
                    // IOA group range here.
                }
            }

            conn.send_act_term(&asdu);
            true
        });
    }

    // Start the server and promote it to a shared Arc.
    iec_server.start();
    info!(port = config.iec104_port, "IEC-104 server started");

    let server: Arc<Mutex<lib60870::Server>> = Arc::new(Mutex::new(iec_server));
    *server_slot.lock().unwrap() = Some(Arc::clone(&server));

    // ── Message source ────────────────────────────────────────────────────────
    // Select the input source based on configuration:
    //   INPUT_FILE set  → replay a newline-delimited JSON file (FileSource)
    //   otherwise       → pull from NATS JetStream (NatsSource)
    // Any type implementing MessageSource can be substituted here.
    let source: Box<dyn MessageSource> = if let Some(ref path) = config.input_file {
        info!(path = %path, "Using file source (INPUT_FILE mode)");
        Box::new(FileSource::new(path))
    } else {
        Box::new(NatsSource::from_config(&config).await?)
    };

    run_message_loop(source, server, data_store, config.iec104_default_ca).await;

    info!("Bridge stopped");
    Ok(())
}

/// Drive the bridge's message loop until the stream ends or Ctrl-C is received.
///
/// Extracted from `main` so it can be called with any [`MessageSource`] in
/// integration tests.
pub async fn run_message_loop(
    source: Box<dyn MessageSource>,
    server: Arc<Mutex<lib60870::Server>>,
    data_store: Arc<Mutex<HashMap<(u16, u32), Iec104Message>>>,
    default_ca: u16,
) {
    let mut messages = source.into_messages();

    loop {
        tokio::select! {
            biased;

            // Graceful Ctrl-C shutdown.
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal – stopping");
                server.lock().unwrap().stop();
                break;
            }

            result = messages.next() => {
                match result {
                    None => {
                        // The source is exhausted (e.g. end of file).  The server
                        // stays up so clients can still connect and trigger a
                        // General Interrogation to retrieve the cached values.
                        warn!("Message stream exhausted – IEC-104 server still active, waiting for Ctrl-C");
                        tokio::signal::ctrl_c().await.ok();
                        info!("Received shutdown signal – stopping");
                        server.lock().unwrap().stop();
                        break;
                    }
                    Some(Err(e)) => {
                        error!(error = %e, "Error receiving message");
                    }
                    Some(Ok(iec_msg)) => {
                        let ca = iec_msg.ca.unwrap_or(default_ca);

                        // Update the data cache (lock released before any await).
                        {
                            let mut store = data_store.lock().unwrap();
                            store.insert((ca, iec_msg.ioa), iec_msg.clone());
                        }

                        // Forward to the IEC-104 server.
                        {
                            let srv = server.lock().unwrap();
                            bridge::dispatch(&bridge::LiveSink(&srv), &iec_msg, ca);
                        }
                    }
                }
            }
        }
    }
}

