//! Virtual-filesystem facade assembling the metadata DB, the LRU cache,
//! the upload queue and the transport into the two operations every
//! consumer drives: `put` (upload path) and `hydrate` (download path).
//!
//! RED-phase stub: every body is `todo!()`; the frozen behavior contract
//! is encoded by the tests under `tests/vfs.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::cache::CacheManager;
use crate::crypto::CryptoError;
use crate::database::{DbError, MetaDatabase};
use crate::rel_path::RelPath;
use crate::transport::{CloudTransport, TransportError};
use crate::upload_queue::{QueueStats, RetryPolicy};

/// Knobs of the VFS: queue shape, upload chunk split and the optional
/// client-side encryption password.
#[derive(Debug, Clone)]
pub struct VfsConfig {
    /// Upload chunk split size; default
    /// `DEFAULT_CHUNK_SIZE_MB * 1024 * 1024`.
    pub chunk_size_bytes: u64,
    /// Upload queue worker count; default 2.
    pub workers: usize,
    /// Upload queue channel bound; default 256.
    pub queue_capacity: usize,
    /// Backoff/degradation policy handed to the queue; default
    /// `RetryPolicy::default()`.
    pub retry: RetryPolicy,
    /// Password enabling client-side encryption; default `None`.
    pub encryption_password: Option<String>,
}

impl Default for VfsConfig {
    fn default() -> Self {
        todo!()
    }
}

/// Errors surfaced by the VFS facade.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    /// No `files` row exists at the virtual path.
    #[error("no such file: {0}")]
    NotFound(String),
    /// The row at the virtual path is a directory.
    #[error("path is a directory: {0}")]
    IsDirectory(String),
    /// The row is encrypted but no password is configured.
    #[error("encrypted file but no encryption password configured")]
    MissingPassword,
    /// The upload queue no longer accepts jobs.
    #[error("queue closed")]
    QueueClosed,
    /// Metadata persistence failed.
    #[error("metadata db error: {0}")]
    Db(#[from] DbError),
    /// The remote backend failed.
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    /// Decryption failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    /// Local file I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The virtual filesystem: metadata DB + LRU cache + upload queue +
/// transport, fronting `put` / `hydrate`. Interior state is private
/// (empty placeholder while stubbed).
pub struct Vfs {}

impl Vfs {
    /// Assembles the VFS and spawns its upload queue; the
    /// `UploadQueueConfig` is derived from `cfg` (`chunk_size_bytes`
    /// carried over unchanged).
    #[allow(unused_variables)]
    pub fn new(
        db: Arc<MetaDatabase>,
        cache: CacheManager,
        transport: Arc<dyn CloudTransport>,
        cfg: VfsConfig,
    ) -> Self {
        todo!()
    }

    /// Upload path (fire-and-forget, Python parity). Stages the bytes to
    /// `cache.local_path(rel)` through a `.tmp` file plus an atomic
    /// rename — a half-written cache copy is never visible (fixing the
    /// Python direct-write defect) — upserts the row as
    /// `is_uploaded = false, is_cached = true` and enqueues the upload
    /// job. Returning means accepted, not uploaded.
    #[allow(unused_variables)]
    pub async fn put(&self, rel: &RelPath, bytes: &[u8], mtime: f64) -> Result<(), VfsError> {
        todo!()
    }

    /// Download path: returns the local cache path of `rel`, hydrating
    /// from the remote first when the cached copy is missing (LRU
    /// eviction included; evicted rows keep every field except
    /// `is_cached`). Encrypted rows decrypt before the cache copy is
    /// written (the cached file is plaintext, Python behavior).
    #[allow(unused_variables)]
    pub async fn hydrate(&self, rel: &RelPath) -> Result<PathBuf, VfsError> {
        todo!()
    }

    /// Snapshot of the upload queue counters.
    pub fn queue_stats(&self) -> QueueStats {
        todo!()
    }

    /// Shuts the upload queue down and waits for it to drain.
    pub async fn shutdown(&self) {
        todo!()
    }
}
