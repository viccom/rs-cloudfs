//! Production [`CloudTransport`] over Telegram MTProto, wrapping the
//! grammers-client 0.10 API.
//!
//! This module is network glue only: every byte-level decision (caption
//! text, part naming, flood-wait parsing, range planning, range serving)
//! is delegated to the pure helpers in [`crate::caption`], [`crate::flood`],
//! [`crate::plan`], [`crate::range`] and [`crate::stream`].
//!
//! grammers 0.10 wiring notes (the API changed shape since the design doc):
//!
//! * There is no `Client::connect(Config { .. })`. The session storage is
//!   created first, then a [`SenderPool`] is built from `Arc<session> +
//!   api_id`, its **runner is spawned on a tokio task** to drive all network
//!   I/O, and the [`Client`] is created from the pool's fat handle.
//!   `api_hash` is only consumed by `Client::bot_sign_in`.
//! * Session persistence is file-backed ([`SqliteSession`] at
//!   `config.session_path`, write-through): grammers-session is redirected
//!   by the workspace `[patch.crates-io]` to an in-tree vendored copy whose
//!   sqlite-storage backend is rusqlite(bundled) — the same SQLite
//!   cloudkit-core links — because upstream's libsql backend statically
//!   bundles a second SQLite whose C symbols collide on MSVC with
//!   rusqlite(bundled) (`LNK2005`). A stored authorization key means
//!   `bot_sign_in` is skipped on restarts.
//! * `DownloadIter` is not an `Iterator`/`Stream`: chunks are pulled with
//!   `async fn next()`, so requested spans are buffered (bounded by the
//!   request length, plus at most one chunk) before [`serve_range`] wraps
//!   them into the [`ByteStream`].

use std::sync::Arc;

use async_trait::async_trait;
use grammers_client::client::UpdatesConfiguration;
use grammers_client::media::Media;
use grammers_client::message::InputMessage;
use grammers_client::sender::ConnectionParams;
use grammers_client::sender::SenderPool;
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError};
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use grammers_session::updates::UpdatesLike;
use grammers_session::Session;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

use crate::config::TransportConfig;
use crate::flood::parse_flood_wait;
use crate::plan::plan_chunk_sends;
use crate::range::{range_plan, MAX_CHUNK_SIZE};
use crate::stream::serve_range;
use cloudkit_storage::transport::{
    ByteStream, ChatCap, CloudTransport, InboundCap, InboundFile, IncomingEvent, IncomingStream,
    RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_storage::Capabilities;

/// Maps a grammers [`InvocationError`] onto the converged taxonomy
/// [`StorageError`] (R2: backend errors never cross the layer boundary
/// raw; the variant mapping table lives at the L2 module docs).
///
/// grammers 0.10 splits the numeric suffix out of RPC error names
/// (`"FLOOD_WAIT_31"` arrives as `name = "FLOOD_WAIT"`, `value = Some(31)`),
/// so the wire-format name is re-joined before being fed to the pure
/// [`parse_flood_wait`] parser (the single source of truth for the name
/// format). `FLOOD_WAIT*` becomes
/// `StorageError::RateLimited { retry_after }`, I/O errors pass through
/// as `Io` (message preserved), a dropped pool runner maps to
/// `Unavailable`, and everything else is `Unavailable` carrying the
/// re-joined error name (diagnosable, retry-reasonable).
fn map_invocation_error(err: InvocationError) -> StorageError {
    match err {
        InvocationError::Rpc(rpc) => {
            let joined = match rpc.value {
                Some(value) => format!("{}_{}", rpc.name, value),
                None => rpc.name.clone(),
            };
            parse_flood_wait(&joined)
                .map(|seconds| StorageError::RateLimited {
                    retry_after: Some(std::time::Duration::from_secs(u64::from(seconds))),
                })
                .unwrap_or_else(|| StorageError::Unavailable(joined))
        }
        InvocationError::Io(io) => StorageError::Io(io.to_string()),
        InvocationError::Dropped => StorageError::Unavailable("sender pool runner is gone".into()),
        other => StorageError::Unavailable(other.to_string()),
    }
}

/// [`CloudTransport`] implementation over a Telegram bot account.
///
/// A `GrammersTransport` is always constructed connected: the associated
/// [`GrammersTransport::connect`] performs session load, bot sign-in and
/// target-chat resolution, so the trait's [`CloudTransport::connect`] is
/// idempotent by construction (a `GrammersTransport` cannot exist in an
/// unconnected state; grammers also re-establishes connections on demand).
pub struct GrammersTransport {
    client: Client,
    chat: grammers_session::types::PeerRef,
    config: TransportConfig,
    /// The raw update receiver kept from the `SenderPool` (the
    /// NOTE(inbound) anchor in [`GrammersTransport::connect`]).
    /// `incoming()` consumes it exactly once to feed
    /// `Client::stream_updates`; a plain mutex works because the
    /// `Option::take` never holds the guard across an await.
    updates_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<UpdatesLike>>>,
}

impl GrammersTransport {
    /// Creates a signed-in transport: spawns the sender-pool runner driving
    /// all network I/O, opens the file-backed session at
    /// `config.session_path` (created on first use, write-through — see the
    /// module docs for the vendored rusqlite backend), signs in with the
    /// bot token only if the stored session is not yet authorized, and
    /// resolves the target chat from `config.chat_id` (a Bot API dialog
    /// id).
    ///
    /// Chat resolution is cache-first: the session's cached access hash is
    /// used when present (populated automatically as the bot interacts with
    /// chats), otherwise an ambient-authority reference is built, which
    /// Telegram accepts for peers the bot may address (e.g. chats the bot is
    /// a member of). The first send/delete against the chat is what
    /// ultimately validates it.
    pub async fn connect(config: TransportConfig) -> Result<Self, StorageError> {
        let session: Arc<SqliteSession> = Arc::new(
            SqliteSession::open(&config.session_path)
                .await
                .map_err(|e| StorageError::Unavailable(format!("session open: {e}")))?,
        );
        // `SenderPool::new` would use `ConnectionParams::default()`, whose
        // descriptive fields are machine-specific (probed 2026-09-03 on the
        // dev machine): device_model = "{os_type} {bitness}" ("Windows
        // 64-bit"), system_version = the OS version string, app_version =
        // grammers-mtsender's own package version ("0.10.0"),
        // system_lang_code/lang_code = system/user locale with an "en"
        // fallback. We pin static values instead so the client fingerprint
        // is stable across machines; `proxy_url` is forwarded verbatim
        // (grammers passes the `socks5://host:port` URI to tokio-socks
        // without parsing it beyond the scheme) and `use_ipv6` matches the
        // default `false`.
        let pool = SenderPool::with_configuration(
            Arc::clone(&session),
            config.api_id,
            ConnectionParams {
                device_model: "cydrive".to_string(),
                system_version: std::env::consts::OS.to_string(),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                system_lang_code: "en".to_string(),
                lang_code: "en".to_string(),
                proxy_url: config.proxy_url.clone(),
                use_ipv6: false,
                __non_exhaustive: (),
            },
        );
        let client = Client::new(pool.handle);
        tokio::spawn(pool.runner.run());
        // NOTE(inbound): the raw update receiver is kept (not dropped)
        // so `incoming()` can hand it to `Client::stream_updates`; it is
        // a single-consumer channel, hence the Option-behind-a-mutex.
        let updates_rx = pool.updates;

        if !client.is_authorized().await.map_err(map_invocation_error)? {
            client
                .bot_sign_in(&config.bot_token, &config.api_hash)
                .await
                .map_err(map_invocation_error)?;
        }

        let peer_id = PeerId::from_bot_api_dialog_id(config.chat_id).ok_or_else(|| {
            StorageError::Unavailable(format!(
                "chat_id {} is not a valid dialog id",
                config.chat_id
            ))
        })?;
        let chat = session
            .peer_ref(peer_id)
            .await
            .map_err(|e| StorageError::Unavailable(format!("peer lookup: {e}")))?
            .unwrap_or_else(|| peer_id.to_ambient_ref());

        Ok(Self {
            client,
            chat,
            config,
            updates_rx: std::sync::Mutex::new(Some(updates_rx)),
        })
    }

    /// The configuration this transport was built from (session path, chat
    /// id, credentials) — needed by callers that re-connect or report stats.
    pub fn config(&self) -> &TransportConfig {
        &self.config
    }

    /// Fetches the media of one remote part message by id.
    ///
    /// [`StorageError::NotFound`] is returned both when the message id is
    /// unknown to the chat and when the message carries no downloadable
    /// media; the transport cannot do anything useful in either case.
    async fn part_media(&self, msg_id: i32) -> Result<Media, StorageError> {
        let messages = self
            .client
            .get_messages_by_id(self.chat, &[msg_id])
            .await
            .map_err(map_invocation_error)?;
        let message = messages
            .into_iter()
            .next()
            .flatten()
            .ok_or(StorageError::NotFound)?;
        message.media().ok_or(StorageError::NotFound)
    }

    /// Downloads `[offset, offset + len)` of one part's document, reusing
    /// [`range_plan`] to translate the offset into `skip_chunks` (whole
    /// chunks skipped via the `DownloadIter` builder) plus an in-chunk skip.
    ///
    /// Returns the buffered chunk frames (unchanged: the in-chunk skip is
    /// applied later by [`serve_range`]) together with the in-chunk skip the
    /// caller must pass on to [`serve_range`]. Pulling stops as soon as
    /// `skip + len` bytes are buffered (at most one chunk of over-read); a
    /// document shorter than requested simply yields fewer frames, which
    /// [`serve_range`] tolerates as a short read. `len = u64::MAX` drains
    /// the whole document.
    async fn download_span(
        &self,
        media: &Media,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<Vec<u8>>, u64), StorageError> {
        let plan = range_plan(offset, len, MAX_CHUNK_SIZE)
            .map_err(|e| StorageError::Unavailable(e.to_string()))?;
        let mut download = self
            .client
            .iter_download(media)
            .chunk_size(MAX_CHUNK_SIZE)
            .skip_chunks(plan.skip_chunks);
        let mut need = plan.skip_bytes_in_first_chunk + plan.bytes_to_yield;
        let mut frames = Vec::new();
        while need > 0 {
            match download.next().await.map_err(map_invocation_error)? {
                Some(chunk) => {
                    need = need.saturating_sub(chunk.len() as u64);
                    frames.push(chunk);
                }
                None => break, // short document; serve_range handles it
            }
        }
        Ok((frames, plan.skip_bytes_in_first_chunk))
    }
}

#[async_trait]
impl CloudTransport for GrammersTransport {
    /// No-op: construction (see [`GrammersTransport::connect`]) performs the
    /// whole login flow, so an existing transport is always connected and
    /// re-invoking this is a no-op.
    async fn connect(&self) -> Result<(), StorageError> {
        Ok(())
    }

    /// Uploads `job` exactly as the Python baseline's `upload_file` does:
    /// [`plan_chunk_sends`] decides the per-part document name and caption,
    /// each part is streamed off disk from its byte offset (`seek` + `take`,
    /// so peak memory stays at the upload buffer size even for 1900 MB
    /// parts), uploaded via `upload_stream` and sent to the chat as a
    /// document whose caption is the planned caption text.
    ///
    /// A 0-byte job plans zero sends (the caller is expected to skip those
    /// uploads entirely, mirroring the Python placeholder-file behavior) and
    /// produces an empty receipt with `first_msg_id == 0`.
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        // TODO(M2 encryption): whether bytes/captions are encrypted is
        // decided upstream (encrypt-then-chunk); until that unit lands,
        // sends are planned and captioned as plaintext.
        let sends = plan_chunk_sends(job, false);
        let mut chunk_msg_ids = Vec::with_capacity(sends.len());
        let mut uploaded_bytes = 0;
        for (index, send) in sends.iter().enumerate() {
            let start = job.chunk_size * index as u64;
            let part_len = usize::try_from(send.byte_len).map_err(|_| {
                StorageError::Unavailable(format!("part too large: {} bytes", send.byte_len))
            })?;
            let mut file = tokio::fs::File::open(&job.local_path).await?;
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let mut part = file.take(part_len as u64);
            let uploaded = self
                .client
                .upload_stream(&mut part, part_len, send.document_name.clone())
                .await?;
            let message = self
                .client
                .send_message(
                    self.chat,
                    InputMessage::new()
                        .text(send.caption.clone())
                        .document(uploaded),
                )
                .await
                .map_err(map_invocation_error)?;
            chunk_msg_ids.push(message.id());
            uploaded_bytes += send.byte_len;
        }
        Ok(UploadReceipt {
            first_msg_id: chunk_msg_ids.first().copied().unwrap_or_default(),
            chunk_msg_ids,
            uploaded_bytes,
        })
    }

    /// Streams the full file: every part document is downloaded completely
    /// and concatenated, then wrapped by [`serve_range`] with the whole-file
    /// byte budget.
    ///
    /// Known limitation (wiring-unit scope): the served window is buffered
    /// in memory, so a full `open` holds the entire file in RAM. The M3
    /// cache layer is the hydrate-to-disk path for whole files (as in the
    /// Python baseline); `open_range` is the bounded-memory entry point.
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        for &msg_id in &file.chunk_msg_ids {
            let media = self.part_media(msg_id).await?;
            let (part_frames, _) = self.download_span(&media, 0, u64::MAX).await?;
            frames.extend(part_frames);
        }
        Ok(serve_range(frames.into_iter(), 0, file.total_size))
    }

    /// Streams `[off, min(off + len, EOF))` of the file across parts.
    ///
    /// `RemoteHandle` carries no chunk plan, so part byte boundaries are
    /// derived from the remote documents themselves (each part is a separate
    /// Telegram document whose exact size the media reports) — no assumption
    /// about the configured `chunk_size_mb` is needed. Only the intersecting
    /// parts are downloaded, and within them only the requested span (via
    /// [`range_plan`] + `Self::download_span`); the first intersecting part's
    /// in-chunk skip is threaded through to [`serve_range`], whose take
    /// budget trims any over-read at the window's end.
    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        // Clamp to EOF per the trait contract.
        let want = len.min(file.total_size.saturating_sub(off));
        if want == 0 {
            return Ok(serve_range(std::iter::empty(), 0, 0));
        }
        let window_start = off;
        let window_end = off + want;
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut skip_in_first = 0;
        let mut part_start = 0u64;
        for &msg_id in &file.chunk_msg_ids {
            if part_start >= window_end {
                break; // window entirely before this part
            }
            let media = self.part_media(msg_id).await?;
            let part_len = u64::try_from(media.size().unwrap_or_default()).map_err(|_| {
                StorageError::Unavailable(format!("part {msg_id} has no known size"))
            })?;
            let part_end = part_start + part_len;
            if window_start < part_end {
                // Intersects [window_start, window_end): for the first
                // intersecting part the window may start mid-part; for every
                // later one it starts at the part boundary.
                let local_off = window_start.saturating_sub(part_start);
                let local_len = part_end.min(window_end) - (part_start + local_off);
                let (part_frames, skip) = self.download_span(&media, local_off, local_len).await?;
                if frames.is_empty() {
                    skip_in_first = skip;
                }
                frames.extend(part_frames);
                if part_end >= window_end {
                    break; // window fully covered
                }
            }
            part_start = part_end;
        }
        Ok(serve_range(frames.into_iter(), skip_in_first, want))
    }

    /// Deletes one remote message. Telegram reports the count of deleted
    /// messages; zero (message already gone) surfaces as
    /// [`StorageError::NotFound`].
    async fn delete_remote(&self, msg_id: i32) -> Result<(), StorageError> {
        let deleted = self
            .client
            .delete_messages(self.chat, &[msg_id])
            .await
            .map_err(map_invocation_error)?;
        if deleted == 0 {
            return Err(StorageError::NotFound);
        }
        Ok(())
    }

    /// Capability declaration (R4). **Transition-period basis (Phase 1)**:
    /// the declared bits rest on this driver's unit tests plus the
    /// production-verified rs-CyDrive runs — the conformance-suite
    /// prerequisite (Phase 2 driver handbook) is not in place yet, which
    /// is exactly why the undeclared bits stay off (宁缺勿滥):
    ///
    /// - `range_read`: `open_range` is unit-tested (range planning/serving)
    ///   and production-verified via the WebDAV read path;
    /// - `multipart`: the transport performs Telegram-native chunked part
    ///   uploads itself (`plan_chunk_sends`, 1900 MB parts — contract 5),
    ///   unit-tested and production-verified;
    /// - `inbound` / `chat`: the bot inbound/reply surfaces; wiring is
    ///   compile-verified offline and shape-identical to the Python
    ///   baseline's production behavior (see NOTE(real-machine) items);
    /// - NOT declared: `resume` (no mid-upload resume against Telegram),
    ///   `server_side_move` (no rename primitive wired), `rapid_upload`
    ///   (no content-addressed upload), `authoritative_index` (Telegram
    ///   is a shadow index by design, foundation D4), `change_feed` (no
    ///   push API consumed).
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            multipart: true,
            inbound: true,
            chat: true,
            ..Capabilities::none()
        }
    }

    fn as_inbound(&self) -> Option<&dyn InboundCap> {
        Some(self)
    }

    fn as_chat(&self) -> Option<&dyn ChatCap> {
        Some(self)
    }
}

impl InboundCap for GrammersTransport {
    /// Wires the M2 inbound unit: consumes the pool's raw update receiver
    /// (kept from [`GrammersTransport::connect`]) through
    /// `Client::stream_updates`, forwarding mapped events over a channel
    /// that backs the returned stream. The receiver is single-consumer,
    /// so only the first call wires the stream; later calls get a
    /// single-error stream (never a panic — a reconnecting caller just
    /// keeps consuming the first stream).
    fn incoming(&self) -> IncomingStream {
        let rx_updates = self
            .updates_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(rx_updates) = rx_updates else {
            return Box::pin(futures_util::stream::iter(vec![Err(
                StorageError::Unavailable(
                    "the incoming stream is already wired (the update receiver was consumed)"
                        .into(),
                ),
            )]));
        };

        let client = self.client.clone();
        // PeerRef is Copy: cheap to move into the forwarding task.
        let chat = self.chat;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut updates = match client
                .stream_updates(rx_updates, UpdatesConfiguration::default())
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    // stream_updates failed before any update arrived:
                    // surface the failure once and end the forwarding
                    // task (the consumer sees an Err item, then EOF).
                    let _ = tx.send(Err(StorageError::Unavailable(format!(
                        "stream_updates failed to start: {error}"
                    ))));
                    return;
                }
            };
            loop {
                match updates.next().await {
                    Ok(update) => {
                        let Some(event) = map_update(&update, &chat) else {
                            continue;
                        };
                        if tx.send(Ok(event)).is_err() {
                            break; // consumer dropped the stream
                        }
                    }
                    Err(error) => {
                        if tx.send(Err(map_invocation_error(error))).is_err() {
                            break; // consumer dropped the stream
                        }
                    }
                }
            }
        });

        // Channel-as-stream: recv() returning None (forwarding task gone)
        // ends the stream like any exhausted iterator.
        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

#[async_trait]
impl ChatCap for GrammersTransport {
    /// Sends a plain text message to the configured chat (bot replies).
    ///
    /// NOTE(real-machine): wiring follows `upload`'s `send_message` shape
    /// verbatim; actual delivery needs a live bot account (offline this
    /// is compile-verified only — see the module's grammers notes).
    async fn send_text(&self, text: &str) -> Result<(), StorageError> {
        self.client
            .send_message(self.chat, InputMessage::new().text(text.to_string()))
            .await
            .map(|_| ())
            .map_err(map_invocation_error)
    }

    /// Uploads `bytes` as a document named `name` and sends it to the
    /// configured chat — the `/get` reply path (the document is the
    /// reply; no caption, the remote document name carries the filename).
    ///
    /// NOTE(real-machine): the in-memory cursor mirrors `upload`'s
    /// stream plumbing; actual delivery needs a live bot account
    /// (offline this is compile-verified only).
    async fn send_document(&self, name: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        let uploaded = self
            .client
            .upload_stream(&mut cursor, bytes.len(), name.to_string())
            .await?;
        self.client
            .send_message(self.chat, InputMessage::new().document(uploaded))
            .await
            .map(|_| ())
            .map_err(map_invocation_error)
    }
}

/// Maps one processed grammers update onto the core's inbound event
/// shape, mirroring the Python handler registration
/// (`telegram_client.py:48-53`): only messages from the configured chat
/// are considered (`chats=self.config.chat_id` in the Python filter);
/// within those, media wins over text — downloadable media becomes
/// [`IncomingEvent::File`] (a missing document filename arrives as an
/// empty string so `Vfs::index_inbound` applies the
/// `Telegram_File_{id}.bin` fallback), other messages degrade to a
/// [`IncomingEvent::Command`] only when they carry text.
///
/// NOTE(real-machine): this mapping cannot be exercised offline — which
/// media shapes real chats produce (document vs sticker vs photo,
/// unnamed documents, album grouping) needs a live bot account sending
/// real media. The unit-testable policy (fallback naming, chat
/// filtering semantics) lives in `cloudkit-core`'s inbound tests.
fn map_update(update: &Update, chat: &grammers_session::types::PeerRef) -> Option<IncomingEvent> {
    let Update::NewMessage(message) = update else {
        return None;
    };
    if message.peer_id() != chat.id {
        return None;
    }
    let msg_id = message.id();
    match message.media() {
        Some(Media::Document(document)) => Some(IncomingEvent::File(InboundFile {
            filename: document.name().unwrap_or_default().to_string(),
            handle: RemoteHandle {
                first_msg_id: msg_id,
                chunk_msg_ids: vec![msg_id],
                total_size: document.size().unwrap_or_default() as u64,
            },
        })),
        Some(Media::Sticker(sticker)) => Some(IncomingEvent::File(InboundFile {
            filename: sticker.document.name().unwrap_or_default().to_string(),
            handle: RemoteHandle {
                first_msg_id: msg_id,
                chunk_msg_ids: vec![msg_id],
                total_size: sticker.document.size().unwrap_or_default() as u64,
            },
        })),
        Some(Media::Photo(photo)) => Some(IncomingEvent::File(InboundFile {
            // Photos carry no filename (Python: `msg.file.name` is None);
            // the empty name triggers the fallback naming in the core.
            filename: String::new(),
            handle: RemoteHandle {
                first_msg_id: msg_id,
                chunk_msg_ids: vec![msg_id],
                total_size: photo.size().unwrap_or_default() as u64,
            },
        })),
        // Other media shapes (contacts, polls, geo...) are not files the
        // VFS can hydrate; a message with no downloadable media is only
        // interesting as a potential bot command.
        _ => {
            let text = message.text();
            (!text.is_empty()).then(|| IncomingEvent::Command {
                text: text.to_string(),
            })
        }
    }
}
