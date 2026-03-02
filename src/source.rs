// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2024 Sentrisense
//
//! Abstractions over the incoming message stream.
//!
//! [`MessageSource`] is the central trait: any type that implements it can be
//! used as the input to the bridge's message loop.  This makes it easy to
//! swap transports (NATS today, Unix socket tomorrow) and to inject test data
//! without spinning up external services.
//!
//! # Provided implementations
//!
//! | Type | Description |
//! |---|---|
//! | [`NatsSource`] | Production source – JetStream pull consumer |
//! | [`IterSource`] | In-memory source – wraps a `Vec<Iec104Message>` |
//!
//! # Extending
//!
//! Implement [`MessageSource`] on your type and return a
//! `BoxStream<'static, anyhow::Result<Iec104Message>>`.  A Unix-socket or
//! TCP-stream source would, for example, accept connections, read
//! newline-delimited JSON, and yield parsed [`Iec104Message`] values through
//! the stream.

use futures::stream::BoxStream;
use futures::StreamExt as _;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::message::Iec104Message;

// ─── trait ────────────────────────────────────────────────────────────────────

/// A source of [`Iec104Message`] items.
///
/// Implementors produce a self-contained, `'static` stream.  The stream yields:
/// * `Ok(msg)` for every successfully received and parsed message.
/// * `Err(e)` for transport-level errors (e.g. a broken socket, a closed NATS
///   connection).  In that case the bridge logs the error and continues; the
///   stream may terminate naturally afterwards.
///
/// Parse-level errors (malformed JSON) are handled *inside* the source
/// implementation: they are logged, the offending message is discarded
/// (and, if applicable, acknowledged to prevent infinite redelivery), and the
/// stream continues with the next message — so callers never see them as
/// stream items.
pub trait MessageSource: Send {
    /// Consume this source and return a stream of parsed messages.
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<Iec104Message>>;
}

// ─── NatsSource ───────────────────────────────────────────────────────────────

/// JetStream pull-consumer source.
///
/// All NATS connection and consumer setup is encapsulated here.  On parse
/// errors the offending delivery is *acknowledged* (to prevent redelivery) and
/// silently dropped from the stream.
pub struct NatsSource {
    /// The raw NATS delivery stream, with transport errors mapped to
    /// `anyhow::Error`.  Storing as `BoxStream` avoids naming the concrete
    /// `Messages` type from async_nats, which is not publicly re-exported at a
    /// stable path.
    messages: BoxStream<'static, anyhow::Result<async_nats::jetstream::Message>>,
}

impl NatsSource {
    /// Connect to NATS and open the JetStream pull consumer described by
    /// `config`.  Returns an error if the connection fails, the stream does
    /// not exist, or the consumer cannot be created.
    pub async fn from_config(config: &Config) -> anyhow::Result<Self> {
        info!(url = %config.nats_url, "Connecting to NATS");

        // async_nats::connect expects a single URL or a *slice* of URL strings.
        // A comma-separated string (common in docker-compose env vars) must be
        // split before being passed; passing the raw string causes a parse error.
        let urls: Vec<&str> = config.nats_url.split(',').map(str::trim).collect();
        let client = async_nats::connect(urls.as_slice())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to NATS at {}: {e}", config.nats_url))?;

        info!("Connected to NATS");

        let jetstream = async_nats::jetstream::new(client);

        let stream = jetstream
            .get_stream(&config.nats_stream)
            .await
            .map_err(|e| anyhow::anyhow!("JetStream stream '{}' not found: {e}", config.nats_stream))?;

        info!(stream = %config.nats_stream, "Opened JetStream stream");

        let mut consumer_config = async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(config.nats_consumer.clone()),
            deliver_policy: async_nats::jetstream::consumer::DeliverPolicy::New,
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            ..Default::default()
        };

        if let Some(ref filter) = config.nats_subject_filter {
            consumer_config.filter_subject = filter.clone();
            info!(filter = %filter, "Applying subject filter");
        }

        let consumer = stream
            .get_or_create_consumer(&config.nats_consumer, consumer_config)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to get/create consumer '{}': {e}", config.nats_consumer)
            })?;

        info!(consumer = %config.nats_consumer, "Subscribed to JetStream consumer");

        let raw_messages = consumer
            .messages()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start message stream: {e}"))?;

        // Erase the concrete Messages type to BoxStream and map errors upfront.
        let messages = Box::pin(raw_messages.map(|r| r.map_err(anyhow::Error::from)));

        Ok(Self { messages })
    }
}

impl MessageSource for NatsSource {
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<Iec104Message>> {
        Box::pin(self.messages.filter_map(|result| async move {
            match result {
                // Transport error – surface it so the bridge can log / break.
                Err(e) => Some(Err(e)),

                Ok(msg) => {
                    let subject = msg.subject.as_str().to_owned();
                    let payload = msg.payload.clone();

                    debug!(subject = %subject, bytes = payload.len(), "Received NATS message");

                    match serde_json::from_slice::<Iec104Message>(&payload) {
                        Ok(iec_msg) => {
                            if let Err(e) = msg.ack().await {
                                error!(error = %e, "Failed to ack NATS message");
                            }
                            Some(Ok(iec_msg))
                        }
                        Err(e) => {
                            // Log, ack (prevent redelivery), and skip.
                            warn!(
                                subject = %subject,
                                error   = %e,
                                payload = %String::from_utf8_lossy(&payload),
                                "Failed to parse JSON – skipping"
                            );
                            if let Err(e) = msg.ack().await {
                                error!(error = %e, "Failed to ack unparseable message");
                            }
                            None // skip; filter_map will try the next item
                        }
                    }
                }
            }
        }))
    }
}

// ─── IterSource ───────────────────────────────────────────────────────────────

/// An in-memory source backed by a pre-built `Vec<Iec104Message>`.
///
/// Intended for unit and integration tests where you want to drive the bridge
/// logic with known data without touching the filesystem or a message broker.
#[cfg(test)]
pub struct IterSource {
    messages: Vec<Iec104Message>,
}

#[cfg(test)]
impl IterSource {
    /// Create a source that will yield exactly the messages in `messages`,
    /// in order, then end.
    pub fn new(messages: Vec<Iec104Message>) -> Self {
        Self { messages }
    }
}

#[cfg(test)]
impl MessageSource for IterSource {
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<Iec104Message>> {
        Box::pin(futures::stream::iter(
            self.messages.into_iter().map(Ok),
        ))
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;

    use super::*;
    use crate::message::{CotField, DataValue, DataType, QualityField};

    // ── helpers ───────────────────────────────────────────────────────────────

    fn make_msg(ioa: u32, value: f64) -> Iec104Message {
        Iec104Message {
            ioa,
            value: DataValue::Number(value),
            data_type: Some(DataType::Float),
            ca: None,
            quality: QualityField::Good,
            cot: CotField::Spontaneous,
        }
    }

    // ── IterSource ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn iter_source_yields_all_messages_in_order() {
        let msgs = vec![make_msg(1, 1.0), make_msg(2, 2.0), make_msg(3, 3.0)];
        let source: Box<dyn MessageSource> = Box::new(IterSource::new(msgs.clone()));
        let result: Vec<_> = source.into_messages().collect().await;

        assert_eq!(result.len(), 3);
        for (i, item) in result.iter().enumerate() {
            let msg = item.as_ref().unwrap();
            assert_eq!(msg.ioa, msgs[i].ioa);
        }
    }

    #[tokio::test]
    async fn iter_source_empty_vec_ends_immediately() {
        let source: Box<dyn MessageSource> = Box::new(IterSource::new(vec![]));
        let result: Vec<_> = source.into_messages().collect().await;
        assert!(result.is_empty());
    }

}
