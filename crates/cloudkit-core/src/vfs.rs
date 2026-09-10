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
use tokio::sync::Notify;

use crate::cache::CacheManager;
use crate::crypto::{self, CryptoError};
use crate::database::{DbError, FileRecord, FileUpsert, MetaDatabase};
use crate::rel_path::RelPath;
use crate::transport::{CloudTransport, InboundFile, RemoteHandle, StorageError, UploadJob};
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
    /// Container scheme for newly encrypted rows (Batch E / E-4):
    /// `Gcm` (default — v1 whole-file staging, byte-identical to the
    /// pre-E-4 behavior) or `AeadV2` (streaming upload, zero `.enc.tmp`).
    /// Recorded on the `files` row at `put` time; the read path dispatches
    /// on the row's scheme, never on this field.
    pub encryption_scheme: crate::config::EncryptionScheme,
    /// Upper bound on the remote-dependent span of a hydration
    /// (transport open through the final cache copy); default 1800s.
    /// The Python baseline mirrored the WebDAV thread's
    /// `future.result(timeout=180)` cap, but real-machine downstream
    /// bandwidth measured ~0.45 MB/s through a local proxy
    /// (decisions.md 2026-09-03, Tier-1 真机端到端 发现①), so 180s timed
    /// out every file above ~80 MB — the Rust default is 1800s, and an
    /// explicit `hydrate_timeout_secs` still wins. Cache hits are local
    /// reads and never bounded.
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
            encryption_scheme: crate::config::EncryptionScheme::default(),
            hydrate_timeout: std::time::Duration::from_secs(
                // Keep aligned with `config::default_hydrate_timeout_secs`
                // (BUG②): this is the conversion chain's fallback when no
                // `CyDriveConfig` maps over.
                1800,
            ),
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
    /// The remote backend failed (D2: converged taxonomy, L3+ only ever
    /// sees [`StorageError`]).
    #[error("transport error: {0}")]
    Transport(#[from] StorageError),
    /// Decryption failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    /// The row's `encryption_scheme` names a scheme this build does not
    /// know — the read path cannot dispatch. A newer build encrypted
    /// this file; the message names the stored value and both schemes
    /// this build understands so the operator can act.
    #[error(
        "unsupported encryption scheme {scheme:?} on {path}: this build \
         knows \"gcm\" and \"aead_v2\" (upgrade the instance that stored it)"
    )]
    UnsupportedEncryptionScheme { scheme: String, path: String },
    /// Local file I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Hydration exceeded `hydrate_timeout` (the remote-dependent span
    /// never hangs a GET forever; the Python baseline capped the WebDAV
    /// thread's `future.result` — see `VfsConfig::hydrate_timeout` for
    /// the default's history).
    #[error("hydration timed out after {0:?}")]
    Timeout(std::time::Duration),
    /// The virtual path already holds a row (file or directory).
    #[error("path already exists: {0}")]
    Exists(String),
    /// The parent path is missing or not a directory.
    #[error("parent path is missing or not a directory: {0}")]
    ParentMissing(String),
    /// Deleting was refused: the row is a pending upload whose local
    /// cache copy still exists — for such a row that copy is the only
    /// copy of the bytes (nothing is on the remote yet), so delete
    /// surfaces refuse until the upload finishes. A ghost pending row
    /// (its copy already vanished, the bytes neither local nor remote)
    /// never raises this and stays deletable — otherwise it could never
    /// be cleaned up.
    #[error("upload still pending: {0}")]
    UploadPending(String),
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

/// Current wall-clock time as fractional Unix seconds — the timestamp
/// source for rows the VFS writes outside the DB layer (mirrors the
/// `database::now` helper; the WebDAV adapter keeps its own copy).
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
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

/// Sibling staging path of the plaintext-under-decryption: the full file
/// name plus a `.dec.tmp` suffix. The v2 streaming hydration decrypts
/// into this sibling (it cannot decrypt in place — the staged file holds
/// the ciphertext being read) and promotes it with an atomic rename.
fn dec_tmp_sibling(target: &Path) -> PathBuf {
    let file_name = target.file_name().unwrap_or_else(|| OsStr::new("cydrive"));
    let mut staged = file_name.to_os_string();
    staged.push(".dec.tmp");
    target.with_file_name(staged)
}

/// v2 streaming hydration (E-3): decrypt the staged ciphertext file
/// chunk-by-chunk into the `.dec.tmp` sibling, then promote it onto the
/// final cache path atomically and drop the consumed ciphertext. Neither
/// the ciphertext nor the plaintext is ever buffered whole — the v2
/// decryptor walks one crypto chunk at a time (the frozen v1 path keeps
/// its whole-file buffering; that is the format's documented cost).
///
/// Failure of any step removes the `.dec.tmp` sibling and leaves the
/// staged ciphertext for the caller's existing cleanup path.
fn hydrate_v2(password: &str, staged: &Path, local: &Path) -> Result<(), VfsError> {
    use cloudkit_crypto::CryptoScheme as _;
    let dec = dec_tmp_sibling(local);
    let scheme = cloudkit_crypto::AeadV2::new();
    let result = (|| -> Result<(), CryptoError> {
        let mut src = std::fs::File::open(staged)?;
        let mut dst = std::io::BufWriter::new(std::fs::File::create(&dec)?);
        scheme.decrypt_stream(password, &mut src, &mut dst)?;
        dst.flush()?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            // The closure's drop already flushed and closed the output
            // file (Windows rename safety); promote the plaintext onto
            // the final path and drop the consumed ciphertext.
            std::fs::rename(&dec, local)?;
            std::fs::remove_file(staged)?;
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&dec);
            Err(error.into())
        }
    }
}

/// Maps queue-handle errors onto the facade's error surface.
fn map_queue_error(error: QueueError) -> VfsError {
    match error {
        QueueError::Closed => VfsError::QueueClosed,
        QueueError::Db(error) => VfsError::Db(error),
        QueueError::Io(error) => VfsError::Io(error),
    }
}

/// How [`Vfs::remote_handle_for`] treats a row whose id set is unusable
/// — the read and delete sites share the assembly (chunks first / row
/// id fallback) but differ in failure policy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandlePolicy {
    /// Read path (`hydrate`, `open_read`): a chunk row without a remote
    /// id is an `Unavailable` error and an id-less row is `NotFound` —
    /// the bytes cannot be located. `total_size` follows the E-5
    /// encrypted-container budget (see [`Vfs::remote_handle_for`]).
    Read,
    /// Delete path (`delete_remote_gated`): id-less chunk rows are
    /// skipped; a row with no ids at all yields the empty placeholder
    /// handle (`first_msg_id = 0`) — the K2 path carries the locator
    /// for path-addressed backends (local), id-keyed backends answer
    /// the tolerated NotFound. `total_size` is the row size (deletion
    /// never transfers bytes).
    Delete,
}

/// Outcome of [`Vfs::open_read`] — the K33 triple gate either admits a
/// row to range streaming or explicitly routes it back to the existing
/// full-hydrate path. An enum (not an error variant / internal
/// fallback) so every caller sees both arms at compile time and must
/// decide how to serve the fallback.
pub enum StreamSource {
    /// Range streaming admitted (K33: plaintext row, transport declares
    /// `range_read`, non-zero size).
    Stream {
        /// Locates the remote object (chunks-first assembly, K2 path
        /// carried).
        handle: RemoteHandle,
        /// Authoritative total length (K35: the plaintext row's size —
        /// rebuild derived it from the backend listing — fit for
        /// Content-Length / Range math).
        total_size: u64,
        /// The VFS's transport, for bounded-window `open_range` calls —
        /// the caller never re-reaches into the VFS per window.
        transport: Arc<dyn CloudTransport>,
    },
    /// Serve the row through the existing full-hydrate path (R-5):
    /// encrypted row (whole-file AEAD can never be range-sliced),
    /// transport without `range_read`, or a 0-byte row (hydrate
    /// materializes the empty copy).
    Hydrate,
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
                encryption_scheme: cfg.encryption_scheme,
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

    /// Rings the sync doorbell manually. Since the wake chokepoint batch
    /// the doorbell lives on the database (its `update_hook` rings for
    /// every `files` row change), so this is no longer the mechanism —
    /// it remains as an explicit escape hatch for callers that change
    /// sync-relevant state without touching the `files` table.
    pub fn wake_sync(&self) {
        self.db.sync_notifier().notify_one();
    }

    /// The shared sync doorbell — the CLI's periodic sync task holds this
    /// and waits on `notified()` alongside its interval tick, turning
    /// local changes into immediate passes. Delegates to the database's
    /// doorbell: that is where the files-table `update_hook` rings.
    pub fn sync_notifier(&self) -> Arc<Notify> {
        self.db.sync_notifier()
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
        // The global switch decides the flag up front (Python
        // `is_encrypted` in telegram_client.py:164-174); the queue
        // then encrypts rows whose flag is set while it holds a
        // password (the AND semantics). `chunk_count` above is still
        // planned on the plaintext — the worker overwrites it with
        // the real ciphertext chunk count on success. A row flagged
        // encrypted also records the configured container scheme here
        // (E-4): the read path dispatches on this per-row value, never
        // on the live config, so turning the config key later never
        // breaks reads of already-stored files. Unencrypted rows keep
        // the plain upsert — the column stays at its `gcm` default
        // and the scheme key stays dormant without a password.
        let upsert = FileUpsert {
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
            is_encrypted: self.cfg.encryption_password.is_some(),
            chunk_count: chunk_count as i64,
            mime_type: None,
        };
        if self.cfg.encryption_password.is_some() {
            self.db
                .upsert_file_scheme(&upsert, self.cfg.encryption_scheme.as_str())?;
        } else {
            self.db.upsert_file(&upsert)?;
        }

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
        // No manual doorbell here anymore: the pending-row upsert above
        // already rang it through the db-layer files hook (the chokepoint).
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
            // is_cached-only flip — targeted column write, never a
            // whole-row upsert of the stale snapshot (P3 race); the
            // local flag is sync-payload-excluded, so the doorbell
            // stays silent for it.
            {
                let _quiet = self.db.suppress_files_hook();
                self.db.set_cached_flag(row.id, true)?;
            }
            self.cache.record_access(rel);
            return Ok(local);
        }

        // Remote handle: the shared row -> handle assembly (chunks
        // first / row id fallback; id widths and the E-5 budget notes
        // live in `remote_handle_for`). Assembled before eviction so a
        // lookup failure never leaves evicted victims behind.
        let handle = self.remote_handle_for(rel, &row, HandlePolicy::Read)?;

        // Make room before filling: every evicted row keeps all fields
        // except the cached flag.
        for victim in self.cache.evict_lru(row.size.max(0) as u64)? {
            if let Some(victim_row) = self.db.get_file(victim.as_str())? {
                // is_cached-only flip — targeted column write, never a
                // whole-row upsert of a possibly-stale snapshot (P3
                // race); the local flag is sync-payload-excluded, so
                // the doorbell stays silent for it.
                let _quiet = self.db.suppress_files_hook();
                self.db.set_cached_flag(victim_row.id, false)?;
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
        // Download budget (E-5): encrypted rows hydrate through a
        // `total_size` of u64::MAX — the full rationale (plaintext vs
        // container length, AEAD self-validation) lives with the budget
        // decision in `remote_handle_for`.
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

            // Decrypt in the staging sibling and only then touch the
            // final cache path: the cache copy is plaintext by contract,
            // and a failed decryption (wrong password, corrupted
            // payload) must leave NO file at `local` — hydrate's hit
            // probe is disk-based, so ciphertext parked there would be
            // served verbatim on every later read (a Python-baseline
            // defect deliberately fixed here; review follow-up BUG①).
            // `write_atomic` stages at the same `.tmp` sibling, so it
            // overwrites the now-consumed ciphertext staging file and
            // renames the plaintext onto the final path in one atomic
            // promotion.
            //
            // Scheme dispatch (E-3) is on the ROW, never on the live
            // config: `gcm` rows keep the frozen whole-file path below
            // (zero change), `aead_v2` rows hydrate through the v2
            // streaming decryptor (constant memory — no whole-ciphertext
            // buffering, the staging file feeds the decryptor chunk by
            // chunk), and an unknown scheme fails with an actionable
            // error instead of guessing.
            if row.is_encrypted {
                let password = self
                    .cfg
                    .encryption_password
                    .as_deref()
                    .ok_or(VfsError::MissingPassword)?;
                match row.encryption_scheme.as_str() {
                    crate::config::SCHEME_GCM => {
                        let plaintext = crypto::decrypt(password, &std::fs::read(&staged)?)?;
                        write_atomic(&local, &plaintext)?;
                    }
                    crate::config::SCHEME_AEAD_V2 => {
                        hydrate_v2(password, &staged, &local)?;
                    }
                    unknown => {
                        return Err(VfsError::UnsupportedEncryptionScheme {
                            scheme: unknown.to_string(),
                            path: row.rel_path.clone(),
                        });
                    }
                }
            } else {
                std::fs::rename(&staged, &local)?;
            }

            // is_cached-only flip — targeted column write, never a
            // whole-row upsert of the pre-download snapshot (P3 race:
            // the row may have been concurrently updated inside the
            // download window); the local flag is sync-payload-excluded,
            // so the doorbell stays silent for it (the chokepoint hook
            // would otherwise ring for every hydration).
            {
                let _quiet = self.db.suppress_files_hook();
                self.db.set_cached_flag(row.id, true)?;
            }
            Ok(())
        };
        let failed = match tokio::time::timeout(timeout, download).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_elapsed) => Some(VfsError::Timeout(timeout)),
        };
        if let Some(error) = failed {
            // No staging file survives a failed hydration (timeout,
            // transport, I/O or decryption): a half-written `.tmp` copy
            // — or, after a failed decrypt, the ciphertext itself — is
            // never left in the cache tree, and `local` was never
            // touched (promotion is the last step). Removal of a
            // not-yet-created staging file is a benign no-op.
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        self.cache.record_access(rel);
        Ok(local)
    }

    /// Assembles the row's [`RemoteHandle`] (SR0 dedup — `hydrate` and
    /// `delete_remote_gated` previously carried the same inline block):
    /// per-chunk rows first (already index-ordered), else the row's own
    /// chunk-0 msg id covers single-chunk files. Ids are i64
    /// end-to-end since Batch B3a (K1) — the DB column and the
    /// transport seam speak the same width, no narrowing. K2: the
    /// caller's rel path rides along so path-addressed backends (local)
    /// can locate the object by it; id-keyed backends ignore it.
    ///
    /// Download budget (E-5, adjudicated plan B — the Read policy): the
    /// row's `size` is the PLAINTEXT length under the Python contract
    /// (R6), but for encrypted rows the remote artifact is the
    /// ciphertext container (v1: salt + nonce + plaintext + tag; v2:
    /// header + plaintext + one tag per crypto chunk) — always LONGER
    /// than the plaintext. A budget-honest transport treats
    /// `total_size` as a hard cap (the telegram `open()` serves through
    /// `serve_range(…, 0, total_size)`), so budgeting an encrypted row
    /// by `row.size` trims the ciphertext and the final AEAD tag check
    /// fails — the real-machine E-5 defect (2621440 B plaintext budget
    /// vs 2621522 B container, 82 bytes short; the v1 path carries the
    /// same latent defect, merely never exercised on a real machine
    /// before E-5). Encrypted containers are self-describing and
    /// AEAD-authenticated: the decryptor itself validates completeness
    /// and integrity, so the budget is not a correctness source —
    /// encrypted rows download unbounded (the telegram `open()` already
    /// pulls every part document in full, so u64::MAX is a pure
    /// pass-through with zero extra I/O). Plaintext rows keep the
    /// row-size budget unchanged. Alternative semantics (row size
    /// stores the ciphertext length — plan A) recorded in decisions.md
    /// as pending-owner-review.
    fn remote_handle_for(
        &self,
        rel: &RelPath,
        row: &FileRecord,
        policy: HandlePolicy,
    ) -> Result<RemoteHandle, VfsError> {
        let chunks = self.db.get_chunks_by_file_id(row.id)?;
        let mut msg_ids = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            match chunk.telegram_msg_id {
                Some(id) => msg_ids.push(id),
                // Read: an id-less chunk row means the bytes cannot be
                // located. Delete (the pre-helper filter_map): skip it.
                None if policy == HandlePolicy::Read => {
                    return Err(VfsError::Transport(StorageError::Unavailable(format!(
                        "chunk {} of {} has no remote message id",
                        chunk.chunk_index, row.rel_path
                    ))));
                }
                None => {}
            }
        }
        if msg_ids.is_empty() {
            match row.telegram_msg_id {
                Some(id) => msg_ids.push(id),
                // Read: a pending upload whose local copy vanished —
                // the bytes live neither locally nor remotely. Delete:
                // keep the empty placeholder (first_msg_id = 0 below).
                None if policy == HandlePolicy::Read => {
                    return Err(VfsError::NotFound(row.rel_path.clone()));
                }
                None => {}
            }
        }
        Ok(RemoteHandle {
            // Read rows are non-empty by construction (chunk ids or the
            // row id above); Delete keeps the pre-helper placeholder 0
            // for id-less rows.
            first_msg_id: msg_ids.first().copied().unwrap_or(0),
            chunk_msg_ids: msg_ids,
            total_size: match policy {
                HandlePolicy::Read if row.is_encrypted => u64::MAX,
                _ => row.size.max(0) as u64,
            },
            path: Some(rel.clone()),
        })
    }

    /// Streaming-read entry (SR0 / K33): resolves `rel` to a
    /// range-streaming source ([`StreamSource::Stream`]) or an explicit
    /// fallback signal ([`StreamSource::Hydrate`]).
    ///
    /// Gate order mirrors `hydrate`: row lookup (`NotFound` /
    /// `IsDirectory`), then the password gate — an encrypted row with
    /// no configured password is `MissingPassword`, an actionable
    /// error, never a fallback — then the K33 triple gate: an encrypted
    /// row (whole-file AEAD can never be range-sliced), a transport
    /// without `range_read`, or a 0-byte row all answer `Hydrate`.
    /// Everything else streams with the row's remote handle, its
    /// authoritative size (K35) and the shared transport for bounded
    /// `open_range` windows.
    ///
    /// Cache-first (WF0 / K42): ahead of the triple gate, a row whose
    /// cached copy exists on disk routes to `Hydrate` — the hydrate arm
    /// then serves the local plaintext with zero remote traffic and
    /// records the LRU access there (this seam stays a pure routing
    /// probe: no hydration side effect, no eviction, no access entry).
    /// Cold rows are untouched: they still stream (or fall back) exactly
    /// as before. The password gate deliberately stays ahead of the
    /// cache probe (hydrate parity, order pinned by tests): an encrypted
    /// row without a configured password must surface the actionable
    /// error, never silently serve a decrypted local copy.
    pub async fn open_read(&self, rel: &RelPath) -> Result<StreamSource, VfsError> {
        let row = self
            .db
            .get_file(rel.as_str())?
            .ok_or_else(|| VfsError::NotFound(rel.as_str().to_string()))?;
        if row.is_dir {
            return Err(VfsError::IsDirectory(row.rel_path));
        }
        // Password gate before the fallback decision (hydrate parity):
        // an encrypted row without a password must surface the
        // actionable error, not a Hydrate signal the caller would
        // blindly retry.
        if row.is_encrypted && self.cfg.encryption_password.is_none() {
            return Err(VfsError::MissingPassword);
        }
        // WF0 cache-first: the cached copy is local plaintext and wins
        // over the remote (byte truth = hydrate's cache-first leg, which
        // re-probes and records the access).
        if self.cache.is_cached(rel) {
            return Ok(StreamSource::Hydrate);
        }
        // K33 triple gate: whole-file-AEAD rows, range-incapable
        // transports and 0-byte rows all serve through the full hydrate
        // path (R-5).
        if row.is_encrypted || !self.transport.capabilities().range_read || row.size == 0 {
            return Ok(StreamSource::Hydrate);
        }
        let handle = self.remote_handle_for(rel, &row, HandlePolicy::Read)?;
        Ok(StreamSource::Stream {
            total_size: handle.total_size,
            handle,
            transport: Arc::clone(&self.transport),
        })
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
            // K1: the handle's id is already the DB's i64 width.
            telegram_msg_id: Some(msg_id),
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
        // No manual doorbell: the inbound row upsert above rang it
        // through the db-layer files hook.
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

    /// Creates a directory row at `rel` (no filesystem directory — put /
    /// hydrate create those lazily), mirroring the WebDAV adapter's
    /// `create_dir`. The root and any existing row collide with
    /// [`VfsError::Exists`]; a missing or non-directory parent row fails
    /// with [`VfsError::ParentMissing`]. Directory rows are zero-sized,
    /// born uploaded + cached, with zero chunks (Python
    /// `create_collection` parity).
    pub fn create_dir(&self, rel: &RelPath) -> Result<(), VfsError> {
        // The root collection always exists.
        if rel.is_root() {
            return Err(VfsError::Exists(rel.as_str().to_string()));
        }
        if self.db.get_file(rel.as_str())?.is_some() {
            return Err(VfsError::Exists(rel.as_str().to_string()));
        }
        // Parent gate + parent_dir in one pass: the root has no row and
        // always passes; any other parent must exist as a directory row.
        let parent_dir = match rel.parent() {
            Some(parent) => {
                if !parent.is_root() {
                    match self.db.get_file(parent.as_str())? {
                        Some(row) if row.is_dir => {}
                        _ => return Err(VfsError::ParentMissing(parent.as_str().to_string())),
                    }
                }
                parent.as_str().to_string()
            }
            None => "/".to_string(),
        };
        self.db.upsert_file(&FileUpsert {
            rel_path: rel.as_str().to_string(),
            name: rel.name().to_string(),
            parent_dir,
            size: 0,
            mtime: unix_now(),
            sha256: None,
            is_dir: true,
            telegram_msg_id: None,
            is_uploaded: true,
            is_cached: true,
            is_encrypted: false,
            chunk_count: 0,
            mime_type: None,
        })?;
        // No manual doorbell: the directory row upsert above rang it
        // through the db-layer files hook.
        Ok(())
    }

    /// Deletes the `rel` row and the local cache copy. When the
    /// transport declares the `remote_delete` capability (K4, Phase 2 —
    /// baidu/local), the remote object is deleted FIRST through the
    /// shared gate ([`Vfs::delete_remote_for_row`]): only after the
    /// remote side has actually been removed do the row and the cached
    /// copy die, so a refused remote delete aborts with the row kept
    /// (an orphaned local delete would hand the authoritative-index
    /// rebuild a resurrected file). Transports that declare the bit off
    /// (telegram/mock) keep the legacy semantics byte-for-byte: the
    /// remote Telegram messages are deliberately NOT deleted (Python
    /// `handle_delete` parity — the WebDAV adapter shares this
    /// semantic), so a cache copy removal failure is logged, never
    /// propagated. A pending upload whose local cache copy still exists
    /// is refused with [`VfsError::UploadPending`] — that copy is the
    /// only copy of the bytes; a ghost pending row (copy already
    /// vanished) deletes normally.
    pub async fn remove_file(&self, rel: &RelPath) -> Result<(), VfsError> {
        let row = self
            .db
            .get_file(rel.as_str())?
            .ok_or_else(|| VfsError::NotFound(rel.as_str().to_string()))?;
        if row.is_dir {
            return Err(VfsError::IsDirectory(row.rel_path));
        }
        // Pending-upload guard (review H2 / plan F2): while the only
        // copy of the bytes sits in the cache tree, deleting the row
        // would orphan the upload. A vanished copy (ghost row) passes.
        if !row.is_uploaded && self.local_copy_exists(rel) {
            return Err(VfsError::UploadPending(rel.as_str().to_string()));
        }
        // K4 ordering: remote first (when the backend supports it),
        // local state only after the remote side is gone.
        self.delete_remote_gated(rel, &row).await?;
        self.db.delete_file(rel.as_str())?;
        // The cached copy goes too; a missing copy is the normal
        // not-cached case, and other removal errors never fail the call.
        if let Err(error) = tokio::fs::remove_file(self.cache.local_path(rel)).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, rel_path = %rel, "failed to remove cache copy");
            }
        }
        // No manual doorbell: the row delete above rang it through the
        // db-layer files hook (deletion = tombstone origin).
        Ok(())
    }

    /// The K4 remote-delete gate as the shared seam for the surfaces'
    /// DIRECTORY-delete paths (the WebDAV DELETE on a collection, the
    /// dashboard's directory fallback): deletes the row's remote object
    /// under the same ordering contract [`Vfs::remove_file`] carries —
    /// the caller may only delete local state after this returns
    /// `Ok(())`. Answers `Ok(())` immediately when the transport
    /// declares no `remote_delete` (legacy semantics — telegram/mock)
    /// or the row does not exist, so an ungated caller is a no-op, never
    /// an error.
    pub async fn delete_remote_for_row(&self, rel: &RelPath) -> Result<(), VfsError> {
        if !self.transport.capabilities().remote_delete {
            return Ok(());
        }
        let Some(row) = self.db.get_file(rel.as_str())? else {
            return Ok(());
        };
        self.delete_remote_gated(rel, &row).await
    }

    /// One K4 gate evaluation: `remote_delete` bit on → delete the
    /// row's remote object, tolerating NotFound (the idempotent end
    /// state — the object is already gone) and retrying any other
    /// failure exactly once (the remote may have deleted it under us,
    /// or the refusal was transient). A refusal that survives the retry
    /// aborts with an actionable error naming the path and the kept
    /// row; the caller must not delete local state.
    ///
    /// Never-uploaded rows (`is_uploaded = false`) have no remote
    /// object — the only remaining shape is the ghost pending row
    /// (bytes neither local nor remote), which skips the gate and stays
    /// deletable.
    async fn delete_remote_gated(&self, rel: &RelPath, row: &FileRecord) -> Result<(), VfsError> {
        if !self.transport.capabilities().remote_delete {
            return Ok(());
        }
        if !row.is_uploaded {
            return Ok(());
        }
        // The handle mirrors hydrate's assembly (chunks first / row id
        // fallback, K2 path always carried); the Delete policy keeps
        // this site's looser id handling — id-less chunk rows skipped,
        // an id-less row yields the path-only placeholder (id-keyed
        // backends answer the NotFound tolerated above).
        let handle = self.remote_handle_for(rel, row, HandlePolicy::Delete)?;
        match self.transport.delete_remote(&handle).await {
            Ok(()) | Err(StorageError::NotFound) => Ok(()),
            Err(first) => match self.transport.delete_remote(&handle).await {
                Ok(()) | Err(StorageError::NotFound) => Ok(()),
                Err(second) => Err(VfsError::Transport(StorageError::Unavailable(format!(
                    "remote delete failed for {}: the row and its cache copy were KEPT — \
                     retry the delete later; first attempt: {first}, retry: {second}",
                    rel.as_str()
                )))),
            },
        }
    }

    /// Whether a local cache copy of `rel` is currently on disk — a
    /// plain synchronous stat over the cache tree. The pending-upload
    /// delete guard's copy check ([`Vfs::remove_file`] and the
    /// dashboard's delete route, which holds no cache handle of its
    /// own, share it).
    pub fn local_copy_exists(&self, rel: &RelPath) -> bool {
        std::fs::metadata(self.cache.local_path(rel)).is_ok()
    }

    /// Empties the cache of **uploaded** files (the root itself survives)
    /// and clears the `is_cached` flag on those rows only, returning the
    /// number of flags cleared. Pending uploads (`is_uploaded = 0`) keep
    /// both their local cache copy — for them it is the only copy of the
    /// bytes — and their flag (plan revision A1). Uploaded payloads stay
    /// in Telegram — this is a local disk operation only; I/O failures
    /// surface as [`VfsError::Io`].
    ///
    /// Thin delegate; the semantics live in
    /// [`clear_cache_preserving_pending`].
    pub fn cache_clear(&self) -> Result<u64, VfsError> {
        clear_cache_preserving_pending(&self.db, &self.cache)
    }

    /// Upload path for an existing local file (CLI `push`): streams the
    /// source into the cache staging sibling (`tokio::fs::copy`, never
    /// reading the file whole into memory), then hands the staged copy to
    /// [`Vfs::put_staged`] — atomic rename onto the cache path, pending
    /// row, enqueued upload. Returns the source size. Ancestor directory
    /// rows are the caller's responsibility (put_staged only creates the
    /// filesystem directories).
    pub async fn ingest_file(
        &self,
        rel: &RelPath,
        source: &Path,
        mtime: f64,
    ) -> Result<u64, VfsError> {
        let size = tokio::fs::metadata(source).await?.len();
        let local = self.cache.local_path(rel);
        let staged = tmp_sibling(&local);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Err(error) = tokio::fs::copy(source, &staged).await {
            let _ = std::fs::remove_file(&staged);
            return Err(error.into());
        }
        if let Err(error) = self.put_staged(rel, &staged, mtime).await {
            // Backstop cleanup for the copy/put failure paths; usually a
            // no-op — put_staged either moved the staged copy with its
            // rename or never got that far.
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        Ok(size)
    }

    /// Shuts the upload queue down and waits for it to drain.
    pub async fn shutdown(&self) {
        self.queue.shutdown().await
    }
}

/// Empties the cache of **uploaded** files (the root itself survives)
/// and clears the `is_cached` flag on those rows only, returning the
/// number of flags cleared. Pending uploads (`is_uploaded = 0`) keep
/// both their local cache copy — for them it is the only copy of the
/// bytes — and their flag (plan revision A1). A pending row whose path
/// fails to parse as a [`RelPath`] only warns — its copy cannot be
/// preserved, and the clear proceeds without it. Uploaded payloads stay
/// in Telegram — this is a local disk operation only; I/O failures
/// surface as [`VfsError::Io`].
///
/// The single source of the pending-preserving cache-clear semantics;
/// both [`Vfs::cache_clear`] and the CLI `cache clear` command go
/// through it.
pub fn clear_cache_preserving_pending(
    db: &MetaDatabase,
    cache: &CacheManager,
) -> Result<u64, VfsError> {
    let pending = db.pending_file_paths()?;
    let keep: Vec<RelPath> = pending
        .iter()
        .filter_map(|path| match RelPath::new(path) {
            Ok(rel) => Some(rel),
            Err(error) => {
                tracing::warn!(
                    %path,
                    %error,
                    "pending path failed to parse; cache clear cannot preserve its copy"
                );
                None
            }
        })
        .collect();
    cache.clear_except(&keep)?;
    Ok(db.clear_cached_flags()?)
}
