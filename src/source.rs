// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense
//
//! Abstractions over the incoming message stream.
//!
//! [`MessageSource`] is the central trait: any type that implements it can be
//! used as the input to the bridge's message loop. This makes it easy to swap
//! transports (NATS today, Unix socket tomorrow) and to inject test data
//! without spinning up external services.

use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::message::Iec104Message;
use crate::validation::validate_message;

const UNIX_SOCKET_BACKLOG: usize = 64;

/// A parsed message together with an optional back-channel for acknowledging it
/// to the transport layer.
pub struct IncomingMessage {
    /// The decoded IEC-104 message.
    pub message: Iec104Message,
    /// Transport-level ack callback, set only when the source requires it.
    ack_fn: Option<Box<dyn FnOnce() -> BoxFuture<'static, anyhow::Result<()>> + Send>>,
}

impl IncomingMessage {
    /// Create a message that requires no acknowledgement (e.g. test sources).
    pub fn new(message: Iec104Message) -> Self {
        Self {
            message,
            ack_fn: None,
        }
    }

    /// Create a message that will invoke `ack` when [`.ack()`](Self::ack) is awaited.
    pub fn with_ack<F, Fut>(message: Iec104Message, ack: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        Self {
            message,
            ack_fn: Some(Box::new(move || Box::pin(ack()))),
        }
    }

    /// Acknowledge this message to the transport layer.
    pub async fn ack(self) -> anyhow::Result<()> {
        if let Some(f) = self.ack_fn {
            f().await
        } else {
            Ok(())
        }
    }
}

/// A source of [`Iec104Message`] items.
pub trait MessageSource: Send {
    /// Consume this source and return a stream of parsed messages.
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<IncomingMessage>>;
}

/// JetStream pull-consumer source.
pub struct NatsSource {
    messages: BoxStream<'static, anyhow::Result<async_nats::jetstream::Message>>,
}

impl NatsSource {
    /// Connect to NATS and open the JetStream pull consumer described by `config`.
    pub async fn from_config(config: &Config) -> anyhow::Result<Self> {
        info!(url = %config.nats_url, "Connecting to NATS");

        let urls = nats_urls(&config.nats_url);
        let client = connect_to_nats(config, &urls).await?;

        info!("Connected to NATS");

        let jetstream = async_nats::jetstream::new(client);
        let stream = open_jetstream_stream(&jetstream, &config.nats_stream).await?;

        info!(stream = %config.nats_stream, "Opened JetStream stream");

        let consumer_config = build_consumer_config(config);
        let consumer =
            get_or_create_consumer(&stream, &config.nats_consumer, consumer_config).await?;

        info!(consumer = %config.nats_consumer, "Subscribed to JetStream consumer");

        let raw_messages = consumer_messages(&consumer).await?;
        let messages = Box::pin(raw_messages.map(|r| r.map_err(anyhow::Error::from)));

        Ok(Self { messages })
    }
}

fn nats_urls(nats_url: &str) -> Vec<&str> {
    nats_url.split(',').map(str::trim).collect()
}

async fn connect_to_nats(config: &Config, urls: &[&str]) -> anyhow::Result<async_nats::Client> {
    if let Some(ref creds_path) = config.nats_credentials_path {
        return connect_to_nats_with_credentials(creds_path, urls, &config.nats_url).await;
    }

    async_nats::connect(urls)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to NATS at {}: {e}", config.nats_url))
}

async fn connect_to_nats_with_credentials(
    creds_path: &str,
    urls: &[&str],
    nats_url: &str,
) -> anyhow::Result<async_nats::Client> {
    info!(path = %creds_path, "Authenticating with NATS credentials file");
    async_nats::ConnectOptions::with_credentials_file(creds_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load NATS credentials from '{creds_path}': {e}"))?
        .connect(urls)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to NATS at {nats_url}: {e}"))
}

async fn open_jetstream_stream(
    jetstream: &async_nats::jetstream::Context,
    stream_name: &str,
) -> anyhow::Result<async_nats::jetstream::stream::Stream> {
    jetstream
        .get_stream(stream_name)
        .await
        .map_err(|e| anyhow::anyhow!("JetStream stream '{stream_name}' not found: {e}"))
}

fn build_consumer_config(config: &Config) -> async_nats::jetstream::consumer::pull::Config {
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

    consumer_config
}

async fn get_or_create_consumer(
    stream: &async_nats::jetstream::stream::Stream,
    consumer_name: &str,
    consumer_config: async_nats::jetstream::consumer::pull::Config,
) -> anyhow::Result<async_nats::jetstream::consumer::PullConsumer> {
    stream
        .get_or_create_consumer(consumer_name, consumer_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get/create consumer '{consumer_name}': {e}"))
}

async fn consumer_messages(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
) -> anyhow::Result<async_nats::jetstream::consumer::pull::Stream> {
    consumer
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start message stream: {e}"))
}

impl MessageSource for NatsSource {
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<IncomingMessage>> {
        Box::pin(self.messages.filter_map(|result| async move {
            match result {
                Err(e) => Some(Err(e)),
                Ok(msg) => {
                    let subject = msg.subject.as_str().to_owned();
                    let payload = msg.payload.clone();

                    debug!(subject = %subject, bytes = payload.len(), "Received NATS message");

                    match serde_json::from_slice::<Iec104Message>(&payload) {
                        Ok(iec_msg) => match validate_message(&iec_msg) {
                            Ok(()) => {
                                let incoming =
                                    IncomingMessage::with_ack(iec_msg, move || async move {
                                        msg.ack().await.map_err(|e| anyhow::anyhow!("{e}"))
                                    });
                                Some(Ok(incoming))
                            }
                            Err(e) => {
                                warn!(
                                    subject = %subject,
                                    error = %e,
                                    bytes = payload.len(),
                                    "Rejected invalid NATS message"
                                );
                                if let Err(e) = msg.ack().await {
                                    error!(error = %e, "Failed to ack invalid NATS message");
                                }
                                None
                            }
                        },
                        Err(e) => {
                            warn!(
                                subject = %subject,
                                error = %e,
                                bytes = payload.len(),
                                "Failed to parse JSON; skipping"
                            );
                            if let Err(e) = msg.ack().await {
                                error!(error = %e, "Failed to ack unparseable NATS message");
                            }
                            None
                        }
                    }
                }
            }
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerCred {
    uid: u32,
    gid: u32,
    pid: u32,
}

enum AuthorizationFailure {
    Uid { expected_uid: u32 },
    Gid { expected_gid: u32 },
}

type PeerCredLookup = Arc<dyn Fn(&UnixStream) -> anyhow::Result<PeerCred> + Send + Sync>;

/// Local Unix domain socket source with per-message replies.
pub struct UnixSocketSource {
    listener: Option<UnixListener>,
    socket_path: PathBuf,
    allowed_uid: Option<u32>,
    allowed_gid: Option<u32>,
    max_line_bytes: usize,
    peer_cred_lookup: PeerCredLookup,
}

impl UnixSocketSource {
    pub async fn from_config(config: &Config) -> anyhow::Result<Self> {
        Self::bind(
            &config.unix_socket_path,
            config.unix_socket_allowed_uid,
            config.unix_socket_allowed_gid,
            config.unix_socket_max_line_bytes,
        )
        .await
    }

    async fn bind(
        socket_path: &str,
        allowed_uid: Option<u32>,
        allowed_gid: Option<u32>,
        max_line_bytes: usize,
    ) -> anyhow::Result<Self> {
        let path = PathBuf::from(socket_path);
        let created_parent_dir = prepare_socket_path(&path)?;

        let listener = UnixListener::bind(&path)
            .map_err(|e| anyhow::anyhow!("Failed to bind Unix socket '{}': {e}", path.display()))?;

        set_socket_permissions(&path, created_parent_dir)?;

        info!(path = %path.display(), "Unix socket input listener started");

        Ok(Self {
            listener: Some(listener),
            socket_path: path,
            allowed_uid,
            allowed_gid,
            max_line_bytes,
            peer_cred_lookup: Arc::new(default_peer_cred_lookup),
        })
    }

    #[cfg(test)]
    async fn with_peer_cred_lookup(
        socket_path: &Path,
        allowed_uid: Option<u32>,
        allowed_gid: Option<u32>,
        max_line_bytes: usize,
        peer_cred_lookup: PeerCredLookup,
    ) -> anyhow::Result<Self> {
        let listener = UnixListener::bind(socket_path).map_err(anyhow::Error::from)?;
        Ok(Self {
            listener: Some(listener),
            socket_path: socket_path.to_path_buf(),
            allowed_uid,
            allowed_gid,
            max_line_bytes,
            peer_cred_lookup,
        })
    }
}

impl MessageSource for UnixSocketSource {
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<IncomingMessage>> {
        let mut this = self;
        let listener = this
            .listener
            .take()
            .expect("UnixSocketSource listener already consumed");
        let runtime = UnixSocketRuntime::from_source(&this);

        let (tx, rx) = mpsc::channel(UNIX_SOCKET_BACKLOG);

        tokio::spawn(async move {
            run_unix_socket_accept_loop(listener, tx, runtime).await;
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

struct UnixSocketRuntime {
    socket_path: PathBuf,
    allowed_uid: Option<u32>,
    allowed_gid: Option<u32>,
    max_line_bytes: usize,
    peer_cred_lookup: PeerCredLookup,
}

impl UnixSocketRuntime {
    fn from_source(source: &UnixSocketSource) -> Self {
        Self {
            socket_path: source.socket_path.clone(),
            allowed_uid: source.allowed_uid,
            allowed_gid: source.allowed_gid,
            max_line_bytes: source.max_line_bytes,
            peer_cred_lookup: Arc::clone(&source.peer_cred_lookup),
        }
    }
}

async fn run_unix_socket_accept_loop(
    listener: UnixListener,
    tx: mpsc::Sender<anyhow::Result<IncomingMessage>>,
    runtime: UnixSocketRuntime,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => spawn_unix_client_task(stream, tx.clone(), &runtime),
            Err(e) => {
                if send_accept_error(&tx, e).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn spawn_unix_client_task(
    stream: UnixStream,
    tx: mpsc::Sender<anyhow::Result<IncomingMessage>>,
    runtime: &UnixSocketRuntime,
) {
    let peer_cred_lookup = Arc::clone(&runtime.peer_cred_lookup);
    let socket_path = runtime.socket_path.clone();
    let allowed_uid = runtime.allowed_uid;
    let allowed_gid = runtime.allowed_gid;
    let max_line_bytes = runtime.max_line_bytes;

    tokio::spawn(async move {
        if let Err(e) = handle_unix_client(
            stream,
            tx,
            allowed_uid,
            allowed_gid,
            max_line_bytes,
            peer_cred_lookup,
        )
        .await
        {
            warn!(path = %socket_path.display(), error = %e, "Unix socket client ended with error");
        }
    });
}

async fn send_accept_error(
    tx: &mpsc::Sender<anyhow::Result<IncomingMessage>>,
    error: std::io::Error,
) -> Result<(), mpsc::error::SendError<anyhow::Result<IncomingMessage>>> {
    tx.send(Err(anyhow::anyhow!("Unix socket accept failed: {error}")))
        .await
}

async fn handle_unix_client(
    stream: UnixStream,
    tx: mpsc::Sender<anyhow::Result<IncomingMessage>>,
    allowed_uid: Option<u32>,
    allowed_gid: Option<u32>,
    max_line_bytes: usize,
    peer_cred_lookup: PeerCredLookup,
) -> anyhow::Result<()> {
    let peer = peer_cred_lookup(&stream)?;
    let stream = authorize_unix_client(stream, peer, allowed_uid, allowed_gid).await?;
    log_unix_client_connected(peer);
    let (reader, writer) = split_unix_client_stream(stream);
    forward_unix_client_lines(reader, writer, tx, max_line_bytes, peer).await
}

fn log_unix_client_connected(peer: PeerCred) {
    info!(
        peer_uid = peer.uid,
        peer_gid = peer.gid,
        peer_pid = peer.pid,
        "Accepted Unix socket client"
    );
}

fn split_unix_client_stream(
    stream: UnixStream,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let (read_half, write_half) = stream.into_split();
    (
        BufReader::new(read_half),
        Arc::new(tokio::sync::Mutex::new(write_half)),
    )
}

async fn forward_unix_client_lines(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    tx: mpsc::Sender<anyhow::Result<IncomingMessage>>,
    max_line_bytes: usize,
    peer: PeerCred,
) -> anyhow::Result<()> {
    loop {
        let Some((line, bytes)) = read_unix_client_line(&mut reader).await? else {
            log_unix_client_disconnected(peer);
            return Ok(());
        };

        if let Some(incoming) =
            process_unix_socket_line(&line, bytes, max_line_bytes, peer, Arc::clone(&writer))
                .await?
        {
            let should_continue = forward_unix_message(&tx, incoming).await;
            if !should_continue {
                return Ok(());
            }
        }
    }
}

async fn read_unix_client_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> anyhow::Result<Option<(String, usize)>> {
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).await?;
    if bytes == 0 {
        return Ok(None);
    }

    Ok(Some((line, bytes)))
}

fn log_unix_client_disconnected(peer: PeerCred) {
    debug!(
        peer_uid = peer.uid,
        peer_gid = peer.gid,
        peer_pid = peer.pid,
        "Unix socket client disconnected"
    );
}

async fn forward_unix_message(
    tx: &mpsc::Sender<anyhow::Result<IncomingMessage>>,
    incoming: IncomingMessage,
) -> bool {
    tx.send(Ok(incoming)).await.is_ok()
}

async fn authorize_unix_client(
    stream: UnixStream,
    peer: PeerCred,
    allowed_uid: Option<u32>,
    allowed_gid: Option<u32>,
) -> anyhow::Result<UnixStream> {
    if let Some(failure) = check_peer_authorization(peer, allowed_uid, allowed_gid) {
        log_authorization_failure(peer, failure);
        reply_unauthorized(stream).await?;
        anyhow::bail!("unauthorized unix socket peer");
    }

    Ok(stream)
}

async fn reply_unauthorized(mut stream: UnixStream) -> anyhow::Result<()> {
    stream.write_all(b"error unauthorized\n").await?;
    Ok(())
}

fn check_peer_authorization(
    peer: PeerCred,
    allowed_uid: Option<u32>,
    allowed_gid: Option<u32>,
) -> Option<AuthorizationFailure> {
    authorization_failure_for_uid(peer, allowed_uid)
        .or_else(|| authorization_failure_for_gid(peer, allowed_gid))
}

fn authorization_failure_for_uid(
    peer: PeerCred,
    allowed_uid: Option<u32>,
) -> Option<AuthorizationFailure> {
    if let Some(expected_uid) = allowed_uid
        && peer.uid != expected_uid
    {
        return Some(AuthorizationFailure::Uid { expected_uid });
    }

    None
}

fn authorization_failure_for_gid(
    peer: PeerCred,
    allowed_gid: Option<u32>,
) -> Option<AuthorizationFailure> {
    if let Some(expected_gid) = allowed_gid
        && peer.gid != expected_gid
    {
        return Some(AuthorizationFailure::Gid { expected_gid });
    }

    None
}

fn log_authorization_failure(peer: PeerCred, failure: AuthorizationFailure) {
    match failure {
        AuthorizationFailure::Uid { expected_uid } => warn!(
            peer_uid = peer.uid,
            expected_uid,
            peer_pid = peer.pid,
            "Rejected Unix socket client due to UID mismatch"
        ),
        AuthorizationFailure::Gid { expected_gid } => warn!(
            peer_gid = peer.gid,
            expected_gid,
            peer_pid = peer.pid,
            "Rejected Unix socket client due to GID mismatch"
        ),
    }
}

async fn process_unix_socket_line(
    line: &str,
    bytes: usize,
    max_line_bytes: usize,
    peer: PeerCred,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> anyhow::Result<Option<IncomingMessage>> {
    if !enforce_line_size_limit(bytes, max_line_bytes, peer, &writer).await? {
        return Ok(None);
    }

    let Some(trimmed) = trim_unix_socket_line(line, &writer).await? else {
        return Ok(None);
    };
    let Some(message) = parse_unix_socket_message(trimmed, peer, bytes, &writer).await? else {
        return Ok(None);
    };
    if !ensure_valid_unix_socket_message(&message, peer, &writer).await? {
        return Ok(None);
    }

    Ok(Some(message_with_ack(message, writer)))
}

async fn enforce_line_size_limit(
    bytes: usize,
    max_line_bytes: usize,
    peer: PeerCred,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> anyhow::Result<bool> {
    if payload_exceeds_limit(bytes, max_line_bytes) {
        log_line_too_large(peer, bytes, max_line_bytes);
        write_socket_reply(writer, b"error validation\n").await?;
        return Ok(false);
    }

    Ok(true)
}

async fn trim_unix_socket_line<'a>(
    line: &'a str,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> anyhow::Result<Option<&'a str>> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        write_socket_reply(writer, b"error parse\n").await?;
        return Ok(None);
    }

    Ok(Some(trimmed))
}

async fn ensure_valid_unix_socket_message(
    message: &Iec104Message,
    peer: PeerCred,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> anyhow::Result<bool> {
    if validate_unix_socket_message(message, peer, writer).await? {
        return Ok(true);
    }

    Ok(false)
}

fn payload_exceeds_limit(bytes: usize, max_line_bytes: usize) -> bool {
    bytes > max_line_bytes
}

fn log_line_too_large(peer: PeerCred, bytes: usize, max_line_bytes: usize) {
    warn!(
        peer_uid = peer.uid,
        peer_gid = peer.gid,
        peer_pid = peer.pid,
        bytes,
        max_line_bytes,
        "Unix socket payload exceeded maximum line length"
    );
}

async fn parse_unix_socket_message(
    trimmed: &str,
    peer: PeerCred,
    bytes: usize,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> anyhow::Result<Option<Iec104Message>> {
    match serde_json::from_str::<Iec104Message>(trimmed) {
        Ok(message) => Ok(Some(message)),
        Err(e) => {
            warn!(
                peer_uid = peer.uid,
                peer_gid = peer.gid,
                peer_pid = peer.pid,
                bytes,
                error = %e,
                "Failed to parse Unix socket JSON message"
            );
            write_socket_reply(writer, b"error parse\n").await?;
            Ok(None)
        }
    }
}

async fn validate_unix_socket_message(
    message: &Iec104Message,
    peer: PeerCred,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> anyhow::Result<bool> {
    if let Err(e) = validate_message(message) {
        warn!(
            peer_uid = peer.uid,
            peer_gid = peer.gid,
            peer_pid = peer.pid,
            ioa = message.ioa,
            error = %e,
            "Rejected invalid Unix socket message"
        );
        write_socket_reply(writer, b"error validation\n").await?;
        return Ok(false);
    }

    Ok(true)
}

async fn write_socket_reply(
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    reply: &[u8],
) -> anyhow::Result<()> {
    writer.lock().await.write_all(reply).await?;
    Ok(())
}

fn message_with_ack(
    message: Iec104Message,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> IncomingMessage {
    IncomingMessage::with_ack(message, move || async move {
        writer.lock().await.write_all(b"ok\n").await?;
        Ok(())
    })
}

fn prepare_socket_path(path: &Path) -> anyhow::Result<bool> {
    let parent = socket_parent(path)?;
    let created_parent_dir = ensure_socket_parent_dir(parent)?;
    remove_existing_socket_path(path)?;

    Ok(created_parent_dir)
}

fn socket_parent(path: &Path) -> anyhow::Result<&Path> {
    path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Unix socket path '{}' must have a parent directory",
            path.display()
        )
    })
}

fn ensure_socket_parent_dir(parent: &Path) -> anyhow::Result<bool> {
    let created_parent_dir = if parent.exists() {
        false
    } else {
        std::fs::create_dir_all(parent).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create Unix socket directory '{}': {e}",
                parent.display()
            )
        })?;
        true
    };

    let metadata = std::fs::symlink_metadata(parent).map_err(|e| {
        anyhow::anyhow!(
            "Failed to inspect Unix socket directory '{}': {e}",
            parent.display()
        )
    })?;
    if !metadata.is_dir() {
        anyhow::bail!(
            "Unix socket parent '{}' is not a directory",
            parent.display()
        );
    }

    Ok(created_parent_dir)
}

fn remove_existing_socket_path(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(existing) => remove_existing_socket_entry(path, existing),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "Failed to inspect Unix socket path '{}': {e}",
            path.display()
        )),
    }
}

fn remove_existing_socket_entry(path: &Path, existing: std::fs::Metadata) -> anyhow::Result<()> {
    if existing.file_type().is_symlink() {
        anyhow::bail!(
            "Refusing to replace symlink at Unix socket path '{}'",
            path.display()
        );
    }

    if existing.file_type().is_socket() || existing.is_file() {
        std::fs::remove_file(path).map_err(|e| {
            anyhow::anyhow!(
                "Failed to remove stale Unix socket '{}': {e}",
                path.display()
            )
        })?;
        return Ok(());
    }

    anyhow::bail!(
        "Unix socket path '{}' already exists and is not removable",
        path.display()
    )
}

fn set_socket_permissions(path: &Path, created_parent_dir: bool) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let parent = path.parent().expect("validated parent exists");
        if created_parent_dir {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o750)).map_err(|e| {
                anyhow::anyhow!("Failed to set permissions on '{}': {e}", parent.display())
            })?;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o660)).map_err(|e| {
            anyhow::anyhow!("Failed to set permissions on '{}': {e}", path.display())
        })?;
    }

    Ok(())
}

fn default_peer_cred_lookup(stream: &UnixStream) -> anyhow::Result<PeerCred> {
    let fd = stream.as_raw_fd();
    let mut raw: libc::ucred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    // SAFETY: `raw` points to a valid `ucred` buffer and `len` is initialized to its size.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut raw as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("SO_PEERCRED failed");
    }

    Ok(PeerCred {
        uid: raw.uid,
        gid: raw.gid,
        pid: raw.pid as u32,
    })
}

#[cfg(test)]
pub struct IterSource {
    messages: Vec<Iec104Message>,
}

#[cfg(test)]
impl IterSource {
    pub fn new(messages: Vec<Iec104Message>) -> Self {
        Self { messages }
    }
}

#[cfg(test)]
impl MessageSource for IterSource {
    fn into_messages(self: Box<Self>) -> BoxStream<'static, anyhow::Result<IncomingMessage>> {
        Box::pin(futures::stream::iter(
            self.messages
                .into_iter()
                .map(|msg| Ok(IncomingMessage::new(msg))),
        ))
    }
}

#[cfg(test)]
mod tests {
    use lib60870::types::{CauseOfTransmission, TypeId};
    use std::time::Duration;

    use futures::StreamExt as _;
    use tempfile::tempdir;
    use time::OffsetDateTime;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;
    use crate::bridge::dispatch;
    use crate::bridge::test_support::{CapturingSink, SentCall};
    use crate::message::{CotField, DataType, DataValue, QualityField};

    fn make_msg(ioa: u32, value: f64) -> Iec104Message {
        Iec104Message {
            ioa,
            value: DataValue::Number(value),
            data_type: Some(DataType::Float),
            ca: None,
            quality: QualityField::Good,
            cot: CotField::Spontaneous,
            timestamp: None,
        }
    }

    fn timestamp_ms(timestamp: &str) -> u64 {
        OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
            .unwrap()
            .unix_timestamp_nanos() as u64
            / 1_000_000
    }

    #[tokio::test]
    async fn iter_source_yields_all_messages_in_order() {
        let msgs = vec![make_msg(1, 1.0), make_msg(2, 2.0), make_msg(3, 3.0)];
        let source: Box<dyn MessageSource> = Box::new(IterSource::new(msgs.clone()));
        let result: Vec<_> = source.into_messages().collect().await;

        assert_eq!(result.len(), 3);
        for (i, item) in result.iter().enumerate() {
            let incoming = item.as_ref().unwrap();
            assert_eq!(incoming.message.ioa, msgs[i].ioa);
        }
    }

    #[tokio::test]
    async fn iter_source_empty_vec_ends_immediately() {
        let source: Box<dyn MessageSource> = Box::new(IterSource::new(vec![]));
        let result: Vec<_> = source.into_messages().collect().await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn unix_socket_source_accepts_valid_message_and_acks_after_dispatch() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("input.sock");
        let source = UnixSocketSource::with_peer_cred_lookup(
            &socket_path,
            None,
            None,
            1024,
            Arc::new(|_| {
                Ok(PeerCred {
                    uid: 1000,
                    gid: 1000,
                    pid: 1234,
                })
            }),
        )
        .await
        .unwrap();

        let mut messages = Box::new(source).into_messages();
        let client = UnixStream::connect(&socket_path).await.unwrap();
        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);

        write_half
            .write_all(b"{\"ioa\":1,\"value\":42.5,\"type\":\"float\"}\n")
            .await
            .unwrap();

        let incoming = tokio::time::timeout(Duration::from_secs(1), messages.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(incoming.message.ioa, 1);

        incoming.ack().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        assert_eq!(response, "ok\n");
    }

    #[tokio::test]
    async fn unix_socket_source_timestamped_message_dispatches_as_timed_iec_output() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("input.sock");
        let source = UnixSocketSource::with_peer_cred_lookup(
            &socket_path,
            None,
            None,
            1024,
            Arc::new(|_| {
                Ok(PeerCred {
                    uid: 1000,
                    gid: 1000,
                    pid: 1234,
                })
            }),
        )
        .await
        .unwrap();

        let mut messages = Box::new(source).into_messages();
        let client = UnixStream::connect(&socket_path).await.unwrap();
        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);
        let timestamp = "2026-06-01T12:34:56.789Z";

        write_half
            .write_all(
                format!(
                    "{{\"ioa\":1001,\"value\":132.4,\"type\":\"float\",\"timestamp\":\"{timestamp}\"}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let incoming = messages.next().await.unwrap().unwrap();
        let sink = CapturingSink::default();
        dispatch(&sink, &incoming.message, 1);
        incoming.ack().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        assert_eq!(response, "ok\n");
        assert!(matches!(
            &sink.calls.into_inner()[0],
            SentCall::Timed {
                ioa: 1001,
                ca: 1,
                cot: CauseOfTransmission::Spontaneous,
                data_type: TypeId::MeasuredFloatTime,
                timestamp_ms: actual_timestamp_ms,
                ..
            } if *actual_timestamp_ms == timestamp_ms(timestamp)
        ));
    }

    #[tokio::test]
    async fn unix_socket_source_rejects_parse_errors() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("input.sock");
        let source = UnixSocketSource::with_peer_cred_lookup(
            &socket_path,
            None,
            None,
            1024,
            Arc::new(|_| {
                Ok(PeerCred {
                    uid: 1000,
                    gid: 1000,
                    pid: 1234,
                })
            }),
        )
        .await
        .unwrap();

        let mut messages = Box::new(source).into_messages();
        let client = UnixStream::connect(&socket_path).await.unwrap();
        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);

        write_half.write_all(b"not-json\n").await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        assert_eq!(response, "error parse\n");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), messages.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unix_socket_source_rejects_unauthorized_peer() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("input.sock");
        let source = UnixSocketSource::with_peer_cred_lookup(
            &socket_path,
            Some(42),
            None,
            1024,
            Arc::new(|_| {
                Ok(PeerCred {
                    uid: 7,
                    gid: 1000,
                    pid: 1234,
                })
            }),
        )
        .await
        .unwrap();

        let _messages = Box::new(source).into_messages();
        let client = UnixStream::connect(&socket_path).await.unwrap();
        let mut reader = BufReader::new(client);

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        assert_eq!(response, "error unauthorized\n");
    }

    #[test]
    fn prepare_socket_path_removes_stale_socket_file() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("input.sock");
        std::fs::write(&socket_path, b"stale").unwrap();

        let created_parent_dir = prepare_socket_path(&socket_path).unwrap();
        assert!(!socket_path.exists());
        assert!(!created_parent_dir);
    }

    #[test]
    fn ensure_socket_parent_dir_reports_when_it_creates_directory() {
        let dir = tempdir().unwrap();
        let socket_parent = dir.path().join("new-socket-dir");

        let created_parent_dir = ensure_socket_parent_dir(&socket_parent).unwrap();

        assert!(created_parent_dir);
        assert!(socket_parent.is_dir());
    }
}
