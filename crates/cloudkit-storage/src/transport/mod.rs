//! CloudTransport trait 家族（Phase 1 Batch R 自 cloudkit-core 迁入 L2）。
//!
//! 这是 telegram 时代的后端接缝（历史形态）：core 永不链接具体网络
//! 客户端，只依赖 [`CloudTransport`] trait；生产实现（grammers/telegram）
//! 在 L1 驱动 crate，内存实现 [`mock::MockTransport`] 是共享测试基建。
//! 行为契约由上游 `cloudkit-core` 的 `tests/transport.rs`（经 re-export
//! 引用）冻结；新语义（trait 拆分/能力位/错误归一）由本 crate 的
//! `tests/transport_traits.rs` 钉死。
//!
//! # trait 拆分（interfaces §1：能力探测代替 trait 分叉）
//!
//! - 核心面 [`CloudTransport`]：connect / upload / open / open_range /
//!   delete_remote + [`CloudTransport::capabilities`]（必选，镜像
//!   StorageDriver 的诚实声明要求）+ [`CloudTransport::as_inbound`] /
//!   [`CloudTransport::as_chat`] 探测（provided 默认 `None`，
//!   storage-only 后端免实现，**消费方探测绝不 panic**）；
//! - 可选能力 trait：[`InboundCap`]（入站事件流，bot 收文件）、
//!   [`ChatCap`]（对话式交互，bot 回复；默认实现返回
//!   `StorageError::Unsupported`）。
//!
//! # 错误归一（D2 / R2：TransportError → StorageError）
//!
//! 迁移前的 `TransportError` 按下表机械映射（语义零变化，载荷尽量保留）：
//!
//! | TransportError（旧）        | StorageError（新）                                | 备注 |
//! |-----------------------------|---------------------------------------------------|------|
//! | `NotConnected`              | [`StorageError::Invalid`]                          | 非法状态类（操作先于 connect）；无载荷变体，诊断靠调用点日志 |
//! | `FloodWait { seconds }`     | [`StorageError::RateLimited`] `{ retry_after: Some(seconds) }` | 逐秒保留（interfaces §3 既有先例） |
//! | `Disconnected(String)`      | [`StorageError::Unavailable`]                      | 连接丢失 = 暂时性不可用，消息保留 |
//! | `NotFound(i32)`             | [`StorageError::NotFound`]                         | id 载荷不再携带（分类学无载荷），诊断上下文由调用方日志承担 |
//! | `Remote(String)`            | [`StorageError::Unavailable`]                      | 「其他后端失败」归置暂时性不可用，消息保留 |
//! | `Io(std::io::Error)`        | [`StorageError::Io`]                               | 经 [`From`] 转换，`to_string()` 保留消息 |
//! | （无鉴权变体）              | [`StorageError::Unauthorized`]                     | 预留：telegram bot 无自动自救流；Phase 2 baidu 110/111/-6 三档启用 |
//!
//! 注意 [`crate::vocab::ByteStream`]（StorageDriver 家族，Send）与本模块
//! [`ByteStream`]（历史接缝，Send + Sync）是两个不同的流类型——后者延续
//! 迁移前的 bounds 以保持 WebDAV 消费面零变化。

pub mod mock;

use crate::capability::Capabilities;
// The family speaks the D2 taxonomy; re-exported so consumers can import
// the whole contract surface from one path (mirrors how the pre-split
// core module owned its error type).
pub use crate::error::StorageError;
use crate::vpath::RelPath;

/// Byte stream of a downloaded file, chunked into `Bytes` frames.
pub type ByteStream = std::pin::Pin<
    Box<dyn futures_core::Stream<Item = Result<bytes::Bytes, StorageError>> + Send + Sync>,
>;

/// Stream of inbound remote events (files sent to us, bot commands).
pub type IncomingStream = std::pin::Pin<
    Box<dyn futures_core::Stream<Item = Result<IncomingEvent, StorageError>> + Send + Sync>,
>;

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

/// Events yielded by [`InboundCap::incoming`].
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

/// Part file name for `base_name` and `chunk_index`:
/// `{base}.part{idx:03}` (min-width padding, so index 1000 keeps growing).
///
/// 分块命名兼容契约（R6 契约 3）：mock 与 telegram 驱动共用同一实现，
/// 自 cloudkit-core `chunker` 随 trait 家族迁入（core 侧 re-export 保持
/// `cloudkit_core::chunker::part_name` 路径）。
pub fn part_name(base_name: &str, chunk_index: usize) -> String {
    format!("{base_name}.part{chunk_index:03}")
}

/// The backend seam: everything the core needs from "the cloud".
///
/// Uploads are fire-and-forget at the call site, downloads stream through
/// [`ByteStream`]; rate-limit handling and chunking details are the
/// implementation's concern.
///
/// 核心面只含存储语义操作；入站/对话能力经 [`InboundCap`]/[`ChatCap`]
/// 探测（见模块文档）。并发：所有方法可并发调用（`&self` + 实现方内部
/// 同步）；生命周期：连接/会话归实现方自理。
#[async_trait::async_trait]
pub trait CloudTransport: Send + Sync {
    /// Establishes the backend session. All other core-face operations
    /// fail with [`StorageError::Invalid`] (not connected = invalid
    /// state) until this succeeds.
    async fn connect(&self) -> Result<(), StorageError>;
    /// Uploads `job` chunk-by-chunk, returning the receipt on success.
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError>;
    /// Streaming-upload face (Batch E / E-3, foundation D7): uploads the
    /// byte stream `data` under `job`'s chunk plan instead of reading
    /// `job.local_path`. `job.size` / `chunk_count` / `chunk_size` carry
    /// the same plan semantics as [`CloudTransport::upload`] and `data`
    /// must yield exactly `job.size` bytes before EOF; `job.local_path`
    /// is provenance only (the caller's plaintext cache copy — the bytes
    /// on the wire are the stream's). Encryption chunking and storage
    /// chunking are orthogonal (D1): the stream is an opaque byte
    /// sequence to the transport.
    ///
    /// Provided default: [`StorageError::Unsupported`] — the
    /// capability-probe evolution rule (interfaces §1: prefer provided
    /// methods over trait forks; consumers must degrade, never panic).
    async fn upload_stream(
        &self,
        job: &UploadJob,
        data: ByteStream,
    ) -> Result<UploadReceipt, StorageError> {
        let _ = (job, data);
        Err(StorageError::Unsupported)
    }
    /// Streams the full bytes of `file`.
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError>;
    /// Streams the slice `[off, min(off + len, EOF))` of `file`.
    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError>;
    /// Deletes one remote message.
    async fn delete_remote(&self, msg_id: i32) -> Result<(), StorageError>;
    /// 能力位声明（R4：必须诚实——与 StorageDriver 同一要求）。
    fn capabilities(&self) -> Capabilities;
    /// 探测入站能力（默认无）。返回的借用与 `self` 同生命周期；
    /// 消费方拿到 `None` 只降级（日志声明），**绝不 panic**。
    fn as_inbound(&self) -> Option<&dyn InboundCap> {
        None
    }
    /// 探测对话能力（默认无）；语义同 [`CloudTransport::as_inbound`]。
    fn as_chat(&self) -> Option<&dyn ChatCap> {
        None
    }
}

/// 入站通道能力（INBOUND 能力位的 trait 面）：bot 收文件/命令的事件流。
///
/// `incoming` 不受 `connect()` 门控（事件流独立于存储会话）；通常只可
/// 接线一次（单消费者通道语义由实现方文档化，重复接线返回单错误流
/// 而非 panic——见 grammers 实现）。
pub trait InboundCap: Send + Sync {
    /// Stream of inbound events; not gated by `connect()`.
    fn incoming(&self) -> IncomingStream;
}

/// 对话通道能力（CHAT 能力位的 trait 面）：bot 对话式交互的回复面。
#[async_trait::async_trait]
pub trait ChatCap: Send + Sync {
    /// Sends a plain text message to the configured chat (bot replies).
    async fn send_text(&self, _text: &str) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }
    /// Sends a document message to the configured chat.
    async fn send_document(&self, _name: &str, _bytes: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }
}
