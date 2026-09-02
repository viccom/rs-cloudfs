//! Transport seam between the domain core and a remote storage backend.
//!
//! The core never links a concrete network client; it depends on the
//! [`CloudTransport`] trait only. The production implementation wraps a
//! Telegram MTProto client and lives outside this crate; the in-memory
//! [`mock::MockTransport`] is the shared test infrastructure.
//!
//! The frozen behavior contract is encoded by the tests under
//! `tests/transport.rs`.

pub mod mock;

use crate::rel_path::RelPath;

/// Byte stream of a downloaded file, chunked into `Bytes` frames.
pub type ByteStream = std::pin::Pin<
    Box<dyn futures_core::Stream<Item = Result<bytes::Bytes, TransportError>> + Send + Sync>,
>;

/// Stream of inbound remote events (files sent to us, bot commands).
pub type IncomingStream = std::pin::Pin<
    Box<dyn futures_core::Stream<Item = Result<IncomingEvent, TransportError>> + Send + Sync>,
>;

/// Errors surfaced by any [`CloudTransport`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// An operation was attempted before a successful `connect()`.
    #[error("not connected; call connect() first")]
    NotConnected,
    /// The remote asked us to slow down; retry after `seconds`.
    #[error("flood wait: retry after {seconds}s")]
    FloodWait { seconds: u32 },
    /// The transport lost its connection.
    #[error("connection dropped: {0}")]
    Disconnected(String),
    /// A remote message id is unknown to the backend.
    #[error("remote message {0} not found")]
    NotFound(i32),
    /// Any other backend-reported failure.
    #[error("remote error: {0}")]
    Remote(String),
    /// Local I/O failure while serving the transport.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// One file upload request: the file on disk plus its chunk plan.
///
/// `rel_path` feeds the remote caption (compat contract 3); the chunk plan
/// must match what re-splitting `local_path` at `chunk_size` produces.
#[derive(Debug, Clone)]
pub struct UploadJob {
    /// Virtual path of the file; carried into each chunk caption.
    pub rel_path: RelPath,
    /// Real local file to upload.
    pub local_path: std::path::PathBuf,
    /// Total file size in bytes.
    pub size: u64,
    /// Planned chunk count (>= 1).
    pub chunk_count: u32,
    /// Size in bytes of every chunk except the last.
    pub chunk_size: u64,
}

/// Result of a successful upload.
#[derive(Debug)]
pub struct UploadReceipt {
    /// msg_id of chunk 0; this is what the metadata DB stores (contract 5).
    pub first_msg_id: i32,
    /// msg_id of every chunk, in order; `len == chunk_count`.
    pub chunk_msg_ids: Vec<i32>,
    /// Total bytes actually uploaded.
    pub uploaded_bytes: u64,
}

/// Reference to a file stored on the remote backend.
#[derive(Debug)]
pub struct RemoteHandle {
    /// msg_id of chunk 0.
    pub first_msg_id: i32,
    /// msg_id of every chunk, in order (single chunk: `vec![msg_id]`).
    pub chunk_msg_ids: Vec<i32>,
    /// Total file size in bytes.
    pub total_size: u64,
}

/// A file received from the remote (indexed metadata only until hydrated).
#[derive(Debug)]
pub struct InboundFile {
    /// Original remote filename.
    pub filename: String,
    /// Where the bytes live on the remote.
    pub handle: RemoteHandle,
}

/// Events yielded by [`CloudTransport::incoming`].
#[derive(Debug)]
pub enum IncomingEvent {
    /// A file was sent to us.
    File(InboundFile),
    /// A bot command was received.
    Command {
        /// Raw command text.
        text: String,
    },
}

/// The backend seam: everything the core needs from "the cloud".
///
/// Uploads are fire-and-forget at the call site, downloads stream through
/// [`ByteStream`]; FloodWait handling and chunking details are the
/// implementation's concern.
#[async_trait::async_trait]
pub trait CloudTransport: Send + Sync {
    /// Establishes the backend session. All other operations (except
    /// [`CloudTransport::incoming`]) fail with [`TransportError::NotConnected`]
    /// until this succeeds.
    async fn connect(&self) -> Result<(), TransportError>;
    /// Uploads `job` chunk-by-chunk, returning the receipt on success.
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, TransportError>;
    /// Streams the full bytes of `file`.
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, TransportError>;
    /// Streams the slice `[off, min(off + len, EOF))` of `file`.
    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, TransportError>;
    /// Deletes one remote message.
    async fn delete_remote(&self, msg_id: i32) -> Result<(), TransportError>;
    /// Stream of inbound events; not gated by `connect()`.
    fn incoming(&self) -> IncomingStream;
    /// Bot reply surface; storage-only transports keep the default (unsupported).
    async fn send_text(&self, _text: &str) -> Result<(), TransportError> {
        Err(TransportError::Remote(
            "send_text not supported by this transport".to_string(),
        ))
    }
    /// Sends a document message to the configured chat.
    async fn send_document(&self, _name: &str, _bytes: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Remote(
            "send_document not supported by this transport".to_string(),
        ))
    }
}
