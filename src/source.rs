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
//! | [`FileSource`] | File source – reads one JSON object per line |
//! | [`IterSource`] | In-memory source – wraps a `Vec<Iec104Message>` |
//!
//! # Extending
//!
//! Implement [`MessageSource`] on your type and return a
//! `BoxStream<'static, anyhow::Result<Iec104Message>>`.  A Unix-socket source
//! would, for example, accept connections, read newline-delimited JSON, and
//! yield parsed [`Iec104Message`] values through the stream.

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

        let client = async_nats::connect(&config.nats_url)
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

// ─── FileSource ───────────────────────────────────────────────────────────────

/// Reads a newline-delimited JSON file and yields one [`Iec104Message`] per
/// non-blank, non-comment line.
///
/// Lines starting with `#` (after trimming whitespace) are treated as comments
/// and skipped.  Parse errors are surfaced as `Err` items so the caller can
/// decide whether to abort or continue.
///
/// Primarily intended for integration testing and manual replay scenarios, but
/// can also be used in production to replay a saved message log.
pub struct FileSource {
    path: std::path::PathBuf,
}

impl FileSource {
    /// Create a source that will read from `path` when
    /// [`into_messages`](MessageSource::into_messages) is called.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl MessageSource for FileSource {
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<Iec104Message>> {
        let path = self.path;

        // Read the file asynchronously, then convert it into a stream of
        // results.  We load the whole file upfront; for test/replay files this
        // is fine (they are not expected to be large).
        let fut = async move {
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| anyhow::anyhow!("Cannot open {:?}: {e}", path))?;

            let items: Vec<anyhow::Result<Iec104Message>> = content
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#')
                })
                .map(|l| {
                    serde_json::from_str(l.trim())
                        .map_err(|e| anyhow::anyhow!("JSON parse error on line {:?}: {e}", l.trim()))
                })
                .collect();

            Ok::<_, anyhow::Error>(items)
        };

        // Flatten: one future → one batch of items → stream of items.
        Box::pin(
            futures::stream::once(fut).flat_map(|result| {
                let items = match result {
                    Err(e) => vec![Err(e)],
                    Ok(items) => items,
                };
                futures::stream::iter(items)
            }),
        )
    }
}

// ─── IterSource ───────────────────────────────────────────────────────────────

/// An in-memory source backed by a pre-built `Vec<Iec104Message>`.
///
/// Intended for unit and integration tests where you want to drive the bridge
/// logic with known data without touching the filesystem or a message broker.
///
/// ```rust
/// # use iec104bridge::source::IterSource;
/// # use iec104bridge::message::{Iec104Message, DataValue, QualityField, CotField};
/// let msgs = vec![/* … */];
/// let source = IterSource::new(msgs);
/// ```
pub struct IterSource {
    messages: Vec<Iec104Message>,
}

impl IterSource {
    /// Create a source that will yield exactly the messages in `messages`,
    /// in order, then end.
    pub fn new(messages: Vec<Iec104Message>) -> Self {
        Self { messages }
    }
}

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
    use crate::message::{CotField, DataType, DataValue, QualityField};

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

    /// Write `content` to a unique temp file and return its path.
    /// The caller is responsible for deleting it (ignored in tests for simplicity).
    async fn write_temp(content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "iec104bridge_test_{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        tokio::fs::write(&path, content).await.unwrap();
        path
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

    // ── FileSource ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn file_source_parses_valid_lines() {
        let content = r#"{"ioa": 10, "value": 1.0}
{"ioa": 20, "value": 2.0}
{"ioa": 30, "value": true}
"#;
        let path = write_temp(content).await;
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.is_ok()));
        assert_eq!(results[0].as_ref().unwrap().ioa, 10);
        assert_eq!(results[1].as_ref().unwrap().ioa, 20);
        assert_eq!(results[2].as_ref().unwrap().ioa, 30);
    }

    #[tokio::test]
    async fn file_source_skips_blank_lines() {
        let content = "\n\n{\"ioa\": 1, \"value\": 0.0}\n\n";
        let path = write_temp(content).await;
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().ioa, 1);
    }

    #[tokio::test]
    async fn file_source_skips_comment_lines() {
        let content = "# this is a header comment\n{\"ioa\": 5, \"value\": 9.9}\n# another comment\n";
        let path = write_temp(content).await;
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().ioa, 5);
    }

    #[tokio::test]
    async fn file_source_yields_error_for_invalid_json() {
        let content = "not json at all\n{\"ioa\": 7, \"value\": 0.0}\n";
        let path = write_temp(content).await;
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(results.len(), 2);
        assert!(results[0].is_err());
        assert!(results[1].is_ok());
        assert_eq!(results[1].as_ref().unwrap().ioa, 7);
    }

    #[tokio::test]
    async fn file_source_error_for_missing_file() {
        let path = std::path::PathBuf::from("/nonexistent/path/to/file.jsonl");
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;

        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[tokio::test]
    async fn file_source_empty_file_yields_nothing() {
        let path = write_temp("").await;
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;
        let _ = tokio::fs::remove_file(&path).await;

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn file_source_only_comments_yields_nothing() {
        let content = "# comment 1\n  # comment 2\n";
        let path = write_temp(content).await;
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;
        let _ = tokio::fs::remove_file(&path).await;

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn file_source_full_message_schema() {
        let content = r#"{"ioa": 100, "value": 42.5, "type": "float", "ca": 3, "quality": "good", "cot": "periodic"}
"#;
        let path = write_temp(content).await;
        let source: Box<dyn MessageSource> = Box::new(FileSource::new(&path));
        let results: Vec<_> = source.into_messages().collect().await;
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(results.len(), 1);
        let msg = results[0].as_ref().unwrap();
        assert_eq!(msg.ioa, 100);
        assert_eq!(msg.ca, Some(3));
        assert_eq!(msg.cot, CotField::Periodic);
        assert_eq!(msg.data_type, Some(DataType::Float));
    }
}
