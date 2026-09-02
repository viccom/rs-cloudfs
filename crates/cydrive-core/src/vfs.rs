//! Virtual-filesystem facade assembling the metadata DB, the LRU cache,
//! the upload queue and the transport into the two operations every
//! consumer drives: `put` (upload path) and `hydrate` (download path).
//!
//! The frozen behavior contract is pinned by the tests under
//! `tests/vfs.rs`.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::stream::StreamExt;

use crate::cache::CacheManager;
use crate::crypto::{self, CryptoError};
use crate::database::{DbError, FileRecord, FileUpsert, MetaDatabase};
use crate::rel_path::RelPath;
use crate::transport::{CloudTransport, InboundFile, RemoteHandle, TransportError, UploadJob};
use crate::upload_queue::{
    spawn_queue, QueueError, QueueStats, RetryPolicy, UploadQueueConfig, UploadQueueHandle,
    DEFAULT_CHUNK_SIZE_MB,
};

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
    /// Upper bound on the remote-dependent span of a hydration
    /// (transport open through the final cache copy); default 180s,
    /// mirroring the Python WebDAV thread's `future.result(timeout=180)`
    /// download deadline. Cache hits are local reads and never bounded.
    pub hydrate_timeout: std::time::Duration,
}

impl Default for VfsConfig {
    /// Mirrors [`UploadQueueConfig::default`] field by field.
    fn default() -> Self {
        Self {
            chunk_size_bytes: DEFAULT_CHUNK_SIZE_MB * 1024 * 1024,
            workers: 2,
            queue_capacity: 256,
            retry: RetryPolicy::default(),
            encryption_password: None,
            hydrate_timeout: std::time::Duration::from_secs(180),
        }
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
    /// Hydration exceeded `hydrate_timeout` (Python parity: the WebDAV
    /// thread's 180s `future.result` cap; the remote-dependent span
    /// never hangs a GET forever).
    #[error("hydration timed out after {0:?}")]
    Timeout(std::time::Duration),
}

/// Sibling staging path of `target`: the full file name plus a `.tmp`
/// suffix (`foo.txt` -> `foo.txt.tmp`). Appending beats `with_extension`,
/// which would replace an existing extension and can collide across files
/// sharing a stem.
fn tmp_sibling(target: &Path) -> PathBuf {
    let file_name = target.file_name().unwrap_or_else(|| OsStr::new("cydrive"));
    let mut staged = file_name.to_os_string();
    staged.push(".tmp");
    target.with_file_name(staged)
}

/// Writes `bytes` to `target` through a `.tmp` sibling plus an atomic
/// rename (creating parent directories): the final path only ever holds a
/// complete file, and no staging file survives the call.
fn write_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = tmp_sibling(target);
    std::fs::write(&staged, bytes)?;
    std::fs::rename(&staged, target)
}

/// Rebuilds the upsert of `row` with `is_cached` flipped and every other
/// field carried over verbatim (used by hydration and by eviction).
fn cached_upsert(row: &FileRecord, is_cached: bool) -> FileUpsert {
    FileUpsert {
        rel_path: row.rel_path.clone(),
        name: row.name.clone(),
        parent_dir: row.parent_dir.clone(),
        size: row.size,
        mtime: row.mtime,
        sha256: row.sha256.clone(),
        is_dir: row.is_dir,
        telegram_msg_id: row.telegram_msg_id,
        is_uploaded: row.is_uploaded,
        is_cached,
        is_encrypted: row.is_encrypted,
        chunk_count: row.chunk_count,
        mime_type: row.mime_type.clone(),
    }
}

/// Narrows an i64 metadata message id to the transport's i32; an id that
/// does not fit is corrupt metadata, surfaced as a transport error.
fn narrow_msg_id(id: i64) -> Result<i32, VfsError> {
    i32::try_from(id).map_err(|_| {
        VfsError::Transport(TransportError::Remote(format!(
            "message id {id} does not fit an i32"
        )))
    })
}

/// Maps queue-handle errors onto the facade's error surface.
fn map_queue_error(error: QueueError) -> VfsError {
    match error {
        QueueError::Closed => VfsError::QueueClosed,
        QueueError::Db(error) => VfsError::Db(error),
        QueueError::Io(error) => VfsError::Io(error),
    }
}

/// The virtual filesystem: metadata DB + LRU cache + upload queue +
/// transport, fronting `put` / `hydrate`. Interior state is private.
pub struct Vfs {
    db: Arc<MetaDatabase>,
    cache: Arc<CacheManager>,
    transport: Arc<dyn CloudTransport>,
    cfg: VfsConfig,
    queue: UploadQueueHandle,
}

impl Vfs {
    /// Assembles the VFS and spawns its upload queue; the
    /// `UploadQueueConfig` is derived from `cfg` (`chunk_size_bytes`
    /// carried over unchanged).
    pub fn new(
        db: Arc<MetaDatabase>,
        cache: CacheManager,
        transport: Arc<dyn CloudTransport>,
        cfg: VfsConfig,
    ) -> Self {
        let queue = spawn_queue(
            Arc::clone(&db),
            Arc::clone(&transport),
            UploadQueueConfig {
                workers: cfg.workers,
                queue_capacity: cfg.queue_capacity,
                retry: cfg.retry.clone(),
                chunk_size_bytes: cfg.chunk_size_bytes,
                encryption_password: cfg.encryption_password.clone(),
            },
        );
        Self {
            db,
            cache: Arc::new(cache),
            transport,
            cfg,
            queue,
        }
    }

    /// Upload path (fire-and-forget, Python parity). Stages the bytes to
    /// `cache.local_path(rel)` through a `.tmp` file plus an atomic
    /// rename — a half-written cache copy is never visible (fixing the
    /// Python direct-write defect) — upserts the row as
    /// `is_uploaded = false, is_cached = true` and enqueues the upload
    /// job. Returning means accepted, not uploaded.
    pub async fn put(&self, rel: &RelPath, bytes: &[u8], mtime: f64) -> Result<(), VfsError> {
        // Stage the bytes before touching metadata: the queue reads the
        // local copy (and only deletes it after a successful upload).
        let local = self.cache.local_path(rel);
        write_atomic(&local, bytes)?;
        self.commit_put(rel, local, bytes.len() as u64, mtime).await
    }

    /// Upload path for externally staged bytes (the WebDAV PUT writer):
    /// the caller already wrote the payload to `staged_tmp`; this renames
    /// it onto the final cache path (same-directory atomic rename, parent
    /// directories created), then runs the exact [`Vfs::put`] lifecycle —
    /// pending row, `is_cached = true`, enqueued upload. The payload is
    /// never read back into memory.
    pub async fn put_staged(
        &self,
        rel: &RelPath,
        staged_tmp: &Path,
        mtime: f64,
    ) -> Result<(), VfsError> {
        let local = self.cache.local_path(rel);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let size = std::fs::metadata(staged_tmp)?.len();
        std::fs::rename(staged_tmp, &local)?;
        self.commit_put(rel, local, size, mtime).await
    }

    /// Shared `put` tail once the full payload sits at `local`: upsert the
    /// pending row sized off the staged bytes, then enqueue the upload.
    ///
    /// Last await point contract (see [`Vfs::put`]): after the enqueue
    /// returns this method must not yield again — callers rely on the row
    /// still being pending the instant the future resolves.
    async fn commit_put(
        &self,
        rel: &RelPath,
        local: PathBuf,
        size: u64,
        mtime: f64,
    ) -> Result<(), VfsError> {
        // 0-byte rows carry a zero chunk plan (the queue never touches
        // the transport for them); otherwise ceil(len / chunk_size) >= 1.
        let chunk_count = if size == 0 {
            0
        } else {
            size.div_ceil(self.cfg.chunk_size_bytes.max(1)) as u32
        };
        let parent_dir = match rel.parent() {
            Some(parent) => parent.as_str().to_string(),
            None => "/".to_string(),
        };
        self.db.upsert_file(&FileUpsert {
            rel_path: rel.as_str().to_string(),
            name: rel.name().to_string(),
            parent_dir,
            size: size as i64,
            mtime,
            sha256: None,
            is_dir: false,
            // None keeps a previously recorded msg id (upsert coalesce)
            // until the re-upload replaces it.
            telegram_msg_id: None,
            is_uploaded: false,
            is_cached: true,
            // The global switch decides the flag up front (Python
            // `is_encrypted` in telegram_client.py:164-174); the queue
            // then encrypts rows whose flag is set while it holds a
            // password (the AND semantics). `chunk_count` above is still
            // planned on the plaintext — the worker overwrites it with
            // the real ciphertext chunk count on success.
            is_encrypted: self.cfg.encryption_password.is_some(),
            chunk_count: chunk_count as i64,
            mime_type: None,
        })?;

        self.queue
            .enqueue(UploadJob {
                rel_path: rel.clone(),
                local_path: local,
                size,
                chunk_count,
                chunk_size: self.cfg.chunk_size_bytes,
            })
            .await
            .map_err(map_queue_error)?;
        Ok(())
    }

    /// Download path: returns the local cache path of `rel`, hydrating
    /// from the remote first when the cached copy is missing (LRU
    /// eviction included; evicted rows keep every field except
    /// `is_cached`). Encrypted rows decrypt before the cache copy is
    /// written (the cached file is plaintext, Python behavior).
    pub async fn hydrate(&self, rel: &RelPath) -> Result<PathBuf, VfsError> {
        let row = self
            .db
            .get_file(rel.as_str())?
            .ok_or_else(|| VfsError::NotFound(rel.as_str().to_string()))?;
        if row.is_dir {
            return Err(VfsError::IsDirectory(row.rel_path));
        }
        let local = self.cache.local_path(rel);

        // Cached copy wins: the remote is never consulted, and the cache
        // holds plaintext by contract.
        if self.cache.is_cached(rel) {
            self.cache.record_access(rel);
            return Ok(local);
        }

        // Password gate before any download work.
        if row.is_encrypted && self.cfg.encryption_password.is_none() {
            return Err(VfsError::MissingPassword);
        }

        // 0-byte rows have no remote bytes; materialize an empty copy.
        if row.size == 0 {
            write_atomic(&local, &[])?;
            self.db.upsert_file(&cached_upsert(&row, true))?;
            self.cache.record_access(rel);
            return Ok(local);
        }

        // Remote handle: per-chunk rows first (already index-ordered),
        // else the row's chunk-0 msg id covers single-chunk files.
        let chunks = self.db.get_chunks_by_file_id(row.id)?;
        let mut msg_ids = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let id = chunk.telegram_msg_id.ok_or_else(|| {
                VfsError::Transport(TransportError::Remote(format!(
                    "chunk {} of {} has no remote message id",
                    chunk.chunk_index, row.rel_path
                )))
            })?;
            msg_ids.push(narrow_msg_id(id)?);
        }
        if msg_ids.is_empty() {
            msg_ids.push(narrow_msg_id(
                // Pending upload whose local copy vanished: the bytes
                // live neither locally nor remotely.
                row.telegram_msg_id
                    .ok_or_else(|| VfsError::NotFound(row.rel_path.clone()))?,
            )?);
        }
        // Non-empty by construction: chunk rows yielded ids, or the
        // fallback above pushed one (else we already returned).
        let first_msg_id = msg_ids[0];

        // Make room before filling: every evicted row keeps all fields
        // except the cached flag.
        for victim in self.cache.evict_lru(row.size.max(0) as u64)? {
            if let Some(victim_row) = self.db.get_file(victim.as_str())? {
                self.db.upsert_file(&cached_upsert(&victim_row, false))?;
            }
        }

        // Stream the remote bytes into the cache through the same
        // tmp+rename pair (close the handle before renaming on Windows).
        // The remote-dependent span — transport open through the final DB
        // upsert — is bounded by `hydrate_timeout`: a stalled remote
        // surfaces as `Timeout` instead of a hung GET (Python parity:
        // the WebDAV thread's `future.result(timeout=180)` cap). Cache
        // hits and the LRU bookkeeping above are local fast paths and
        // stay outside the bound.
        let handle = RemoteHandle {
            first_msg_id,
            chunk_msg_ids: msg_ids,
            total_size: row.size.max(0) as u64,
        };
        let staged = tmp_sibling(&local);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let timeout = self.cfg.hydrate_timeout;
        let download = async {
            let mut stream = self.transport.open(&handle).await?;
            {
                let mut file = std::fs::File::create(&staged)?;
                while let Some(frame) = stream.next().await {
                    file.write_all(&frame?)?;
                }
                file.flush()?;
            }
            std::fs::rename(&staged, &local)?;

            // Python behavior: the cache copy is plaintext, so decrypt
            // after the ciphertext landed locally.
            if row.is_encrypted {
                let password = self
                    .cfg
                    .encryption_password
                    .as_deref()
                    .ok_or(VfsError::MissingPassword)?;
                let plaintext = crypto::decrypt(password, &std::fs::read(&local)?)?;
                write_atomic(&local, &plaintext)?;
            }

            self.db.upsert_file(&cached_upsert(&row, true))?;
            Ok(())
        };
        let failed = match tokio::time::timeout(timeout, download).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_elapsed) => Some(VfsError::Timeout(timeout)),
        };
        if let Some(error) = failed {
            // No staging file survives a failed hydration (timeout,
            // transport or I/O): a half-written `.tmp` copy is never
            // left in the cache tree. Removal of a not-yet-created
            // staging file is a benign no-op.
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        self.cache.record_access(rel);
        Ok(local)
    }

    /// Snapshot of the upload queue counters.
    pub fn queue_stats(&self) -> QueueStats {
        self.queue.stats()
    }

    /// Handle to the metadata DB (Arc clone) — surfaces that need direct
    /// reads next to the VFS operations (bot `/stats` + `/search`, the
    /// WebDAV adapter) share the exact same store the VFS writes through.
    /// Frozen API addition (trait-evolution adjudication, 2026-09-02).
    pub fn db(&self) -> Arc<MetaDatabase> {
        Arc::clone(&self.db)
    }

    /// Indexes one inbound remote file at the root (metadata only; payload
    /// stays remote until hydrated on demand). Returns the rel_path used.
    ///
    /// Python baseline (`telegram_client.py:55-85`): the row always sits at
    /// the root — `parent_dir = "/"` even when the filename itself contains
    /// `/` (a nested virtual path forms, unsanitized) — and a filename that
    /// is missing or cannot form a valid path falls back to
    /// `Telegram_File_{first_msg_id}.bin`. Single-message media is recorded
    /// with `chunk_count = 1`, `is_uploaded = true`, `is_cached = false`;
    /// a same-named file overwrites the row through the rel_path unique
    /// key. `InboundFile` carries no mime field, so `mime_type` is `None`
    /// where the Python baseline stored `msg.file.mime_type`.
    pub async fn index_inbound(&self, file: InboundFile) -> Result<RelPath, VfsError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let msg_id = file.handle.first_msg_id;

        // Python mirrors the filename verbatim into both rel_path and
        // name; an unusable name (empty, backslash, `..` segment, ...)
        // degrades to the per-message fallback name. An empty filename
        // would otherwise degenerate the path to the root itself, so a
        // root result is rejected too.
        let candidate = RelPath::new(&format!("/{}", file.filename))
            .ok()
            .filter(|rel| !rel.is_root());
        let (rel, name) = match candidate {
            Some(rel) => (rel, file.filename),
            None => {
                let fallback = format!("Telegram_File_{msg_id}.bin");
                // A single clean segment by construction (digits plus a
                // fixed extension), so this cannot fail; building it via
                // `RelPath::new` keeps the fallback subject to the same
                // validation instead of blind trust.
                let rel = RelPath::new(&format!("/{fallback}"))
                    .expect("the fallback name is a single clean segment");
                (rel, fallback)
            }
        };

        self.db.upsert_file(&FileUpsert {
            rel_path: rel.as_str().to_string(),
            name,
            parent_dir: "/".to_string(),
            size: file.handle.total_size as i64,
            mtime: now,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(i64::from(msg_id)),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
        })?;
        tracing::info!(
            path = %rel,
            msg_id,
            size = file.handle.total_size,
            "indexed inbound remote file (metadata only)"
        );
        Ok(rel)
    }

    /// Boot-time recovery: re-enqueues every pending upload row whose
    /// local cache copy still exists (rows without one are skipped — the
    /// Python baseline lost such files on power loss). Returns the number
    /// of jobs enqueued. Callers booting a serving surface should run
    /// this before accepting traffic so crash-staged uploads resume
    /// ahead of new client writes.
    pub async fn requeue_pending(&self) -> Result<usize, QueueError> {
        self.queue.requeue_pending(&self.cache).await
    }

    /// Shuts the upload queue down and waits for it to drain.
    pub async fn shutdown(&self) {
        self.queue.shutdown().await
    }
}
