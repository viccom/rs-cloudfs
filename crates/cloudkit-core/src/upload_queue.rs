//! Bounded upload queue driving [`CloudTransport::upload`] for pending
//! files (`is_uploaded = 0` rows of the metadata DB).
//!
//! Contract sources: the design doc «上传队列（core）» and the Python
//! behavior baseline (contracts 6/7): 0-byte uploads skip the transport
//! entirely, FloodWait sleeps the exact server-provided seconds and
//! retries the whole upload, every other failure backs off exponentially
//! and degrades after `max_attempts` consecutive tries, and the local
//! cache copy is deleted only after a successful upload (fixing the
//! Python unconditional-delete bug). Successful uploads additionally
//! store the plaintext sha256 for files at or below [`SHA256_MAX_BYTES`]
//! (Python baseline `telegram_client.py:160`), computed up front —
//! before the first upload attempt, while the local copy is guaranteed
//! to still exist (a post-success hash raced with a same-path rewrite's
//! success-delete in the field: os error 2, digest lost).
//!
//! Layout: the handle fronts one [`QueueInner`] (sender slot, worker
//! join handles, atomic counters, config); worker tasks share the single
//! `mpsc::Receiver` behind an async mutex and claim one job at a time,
//! so a single-worker queue is strictly FIFO. The frozen behavior
//! contract is encoded by the tests under `tests/upload_queue.rs`.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::cache::CacheManager;
use crate::database::{DbError, FileRecord, FileUpsert, MetaDatabase};
use crate::rel_path::RelPath;
use crate::transport::{CloudTransport, StorageError, UploadJob, UploadReceipt};

/// File size above which uploads split into `.partNNN` chunks; 1900 MB
/// keeps every message under Telegram's 2 GB per-message cap (contract 5).
pub const DEFAULT_CHUNK_SIZE_MB: u64 = 1900;

/// Uploads at or below this size get a sha256 digest stored in their row;
/// larger ones store None. Frozen by the Python baseline
/// (`telegram_client.py:160`: `file_size <= 100 * 1024 * 1024`), which in
/// turn feeds the sha-based WebDAV ETag (contract 6).
pub const SHA256_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Pure size gate for the sha256 side-computation (boundary-exact:
/// `SHA256_MAX_BYTES` itself hashes, one byte over skips; 0 hashes — the
/// Python baseline computes the empty digest).
#[inline]
pub fn should_hash(size: u64) -> bool {
    size <= SHA256_MAX_BYTES
}

/// Backoff and degradation knobs for non-FloodWait upload failures.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Backoff after the first failure; doubles on every consecutive one.
    pub initial_backoff: Duration,
    /// Upper bound of the exponential backoff.
    pub max_backoff: Duration,
    /// Degrade the job once consecutive non-FloodWait failures reach this.
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    /// 1s initial backoff, 5min cap, degrade after 5 consecutive failures.
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(5 * 60),
            max_attempts: 5,
        }
    }
}

/// Verdict for a failed upload attempt.
pub enum RetryDecision {
    /// Sleep the duration, then retry the whole upload.
    RetryAfter(Duration),
    /// Stop retrying: keep the local file, the row stays `is_uploaded = 0`.
    Degrade,
}

/// The single judge of retry semantics for a failed upload attempt.
///
/// - `RateLimited { retry_after: Some(wait) }` (the old FloodWait) →
///   [`RetryDecision::RetryAfter`] of exactly that long: never clamped by
///   `max_backoff`, never degraded no matter how large
///   `consecutive_failures` is.
/// - Any other error, with `n = consecutive_failures` (counting the
///   current failure, starting at 1): `n >= max_attempts` degrades,
///   otherwise retry after `min(initial * 2^(n-1), max)`.
pub fn decide_retry(
    policy: &RetryPolicy,
    error: &StorageError,
    consecutive_failures: u32,
) -> RetryDecision {
    // The server's authoritative wait is honored exactly, before any
    // attempt accounting (never clamped, never degrading). A RateLimited
    // without a retry_after has no authoritative word to honor and falls
    // through to the regular backoff ladder.
    if let StorageError::RateLimited {
        retry_after: Some(wait),
    } = error
    {
        return RetryDecision::RetryAfter(*wait);
    }
    if consecutive_failures >= policy.max_attempts {
        return RetryDecision::Degrade;
    }
    // initial * 2^(n-1) via saturating doublings; the early break and the
    // final min() clamp make the cap kick in long before any overflow.
    let mut delay = policy.initial_backoff;
    for _ in 1..consecutive_failures.min(64) {
        if delay >= policy.max_backoff {
            break;
        }
        delay = delay.saturating_mul(2);
    }
    RetryDecision::RetryAfter(delay.min(policy.max_backoff))
}

/// Errors surfaced by the queue handle.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// The queue was shut down and accepts no more jobs.
    #[error("queue is closed")]
    Closed,
    /// Metadata persistence failed.
    #[error("metadata db error: {0}")]
    Db(#[from] DbError),
    /// Local file I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Knobs of the spawned queue.
#[derive(Debug, Clone)]
pub struct UploadQueueConfig {
    /// Worker tasks sharing the bounded channel.
    pub workers: usize,
    /// Bound of the job channel; `enqueue` waits for capacity.
    pub queue_capacity: usize,
    /// Retry/degradation policy for upload failures.
    pub retry: RetryPolicy,
    /// Chunk split size used for re-enqueued pending rows.
    pub chunk_size_bytes: u64,
    /// Password enabling client-side encryption of uploaded rows
    /// (Python AND semantics: a row is encrypted only when its own
    /// `is_encrypted` flag is set *and* this is `Some`); default `None`
    /// keeps every upload byte-identical to the plaintext path.
    pub encryption_password: Option<String>,
    /// Container scheme for rows this queue uploads encrypted (Batch E /
    /// E-3): `Gcm` (default — the frozen v1 whole-file `.enc.tmp`
    /// staging, byte-identical to the pre-E-3 behavior) or `AeadV2`
    /// (streaming encryption straight into `upload_stream`, zero
    /// ciphertext staging file). Mirrored from `VfsConfig` by
    /// [`crate::vfs::Vfs::new`].
    pub encryption_scheme: crate::config::EncryptionScheme,
}

impl Default for UploadQueueConfig {
    /// 2 workers, capacity 256, default retry, 1900 MB chunks, no
    /// encryption.
    fn default() -> Self {
        Self {
            workers: 2,
            queue_capacity: 256,
            retry: RetryPolicy::default(),
            chunk_size_bytes: DEFAULT_CHUNK_SIZE_MB * 1024 * 1024,
            encryption_password: None,
            encryption_scheme: crate::config::EncryptionScheme::default(),
        }
    }
}

/// Atomic counters snapshot of the queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    /// Jobs accepted into the channel.
    pub enqueued: u64,
    /// Jobs that reached a successful terminal state.
    pub succeeded: u64,
    /// Jobs abandoned after retry exhaustion (or metadata failures).
    pub degraded: u64,
    /// Retry attempts actually performed (backoff or FloodWait sleeps).
    pub retries: u64,
}

impl QueueStats {
    /// Jobs that have not reached a terminal state yet — the queue's
    /// drain predicate (REMOVE's budgeted wait, LIST's `pending=`).
    /// Every skip path must land in exactly one terminal counter, or
    /// this never settles (H2: a stale empty-PUT artifact skipped
    /// without accounting kept one ghost outstanding forever).
    pub fn outstanding(&self) -> u64 {
        self.enqueued.saturating_sub(self.succeeded + self.degraded)
    }
}

/// The four atomic counters shared by the handle and every worker task.
#[derive(Default)]
struct StatsCounters {
    enqueued: AtomicU64,
    succeeded: AtomicU64,
    degraded: AtomicU64,
    retries: AtomicU64,
}

impl StatsCounters {
    /// Relaxed loads suffice: these are independent tallies, and
    /// cross-task visibility is provided by the worker join in
    /// [`UploadQueueHandle::shutdown`].
    fn snapshot(&self) -> QueueStats {
        QueueStats {
            enqueued: self.enqueued.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
            degraded: self.degraded.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
        }
    }
}

/// Increments one counter (Relaxed — see [`StatsCounters::snapshot`]).
fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Shared interior of a spawned queue. Both mutexes are tokio mutexes:
/// the guards are held across awaits by design (bounded-channel
/// backpressure in `enqueue`, worker joining in `shutdown`) and are
/// always acquired in `tx` → `workers` order, so no deadlock cycle.
struct QueueInner {
    /// Sender slot, taken (and dropped) by `shutdown`; every later
    /// `enqueue` then observes `QueueError::Closed`.
    tx: AsyncMutex<Option<mpsc::Sender<UploadJob>>>,
    /// Worker join handles, taken exactly once by `shutdown`: a racing
    /// second call blocks on this mutex until the drain finished, a
    /// sequential one sees `None` and returns immediately.
    workers: AsyncMutex<Option<Vec<JoinHandle<()>>>>,
    /// Counters shared with the worker tasks.
    stats: Arc<StatsCounters>,
    cfg: UploadQueueConfig,
    db: Arc<MetaDatabase>,
}

/// Handle to a spawned queue; interior state is private.
pub struct UploadQueueHandle {
    inner: Arc<QueueInner>,
}

impl UploadQueueHandle {
    /// Pushes `job` into the bounded channel; `Err(QueueError::Closed)`
    /// once the queue has been shut down.
    pub async fn enqueue(&self, job: UploadJob) -> Result<(), QueueError> {
        let guard = self.inner.tx.lock().await;
        let Some(sender) = guard.as_ref() else {
            return Err(QueueError::Closed);
        };
        // A send error means every receiver is gone (workers joined),
        // which is indistinguishable from a shut-down queue for callers.
        sender.send(job).await.map_err(|_| QueueError::Closed)?;
        bump(&self.inner.stats.enqueued);
        Ok(())
    }

    /// Snapshot of the queue counters.
    pub fn stats(&self) -> QueueStats {
        self.inner.stats.snapshot()
    }

    /// Re-enqueues every pending upload whose local copy still exists.
    ///
    /// Scans `list_all_files()` for non-directory `is_uploaded = false`
    /// rows; a row whose cache file is missing is skipped (the Python
    /// version lost such files on power loss — only uploadable files are
    /// enqueued). Each surviving row becomes an `UploadJob` sized from the
    /// real file metadata. Returns the number of jobs enqueued.
    pub async fn requeue_pending(&self, cache: &CacheManager) -> Result<usize, QueueError> {
        let rows = self.inner.db.list_all_files()?;
        let mut enqueued = 0usize;
        for row in rows {
            if row.is_dir || row.is_uploaded {
                continue;
            }
            let rel_path = match RelPath::new(&row.rel_path) {
                Ok(rel_path) => rel_path,
                Err(error) => {
                    tracing::warn!(
                        rel_path = %row.rel_path,
                        %error,
                        "pending row has an invalid virtual path; skipped"
                    );
                    continue;
                }
            };
            let local_path = cache.local_path(&rel_path);
            // A missing local copy means nothing uploadable remains.
            let size = match std::fs::metadata(&local_path) {
                Ok(meta) => meta.len(),
                Err(_) => continue,
            };
            self.enqueue(UploadJob {
                chunk_count: row.chunk_count.max(0) as u32,
                chunk_size: self.inner.cfg.chunk_size_bytes,
                rel_path,
                local_path,
                size,
            })
            .await?;
            enqueued += 1;
        }
        Ok(enqueued)
    }

    /// Shuts the queue down, idempotently: closes the channel, waits for
    /// the workers to drain it (in-flight retries included) and joins
    /// them. Every job enqueued before shutdown reaches a terminal state.
    pub async fn shutdown(&self) {
        // First caller closes the channel (take + drop); workers drain
        // the buffered jobs before `recv` yields None, so queued and
        // in-flight jobs (retries included) still finish.
        self.inner.tx.lock().await.take();
        // Join exactly once. Holding this mutex across the awaits makes a
        // concurrent second shutdown wait for the drain as well.
        let mut workers = self.inner.workers.lock().await;
        if let Some(handles) = workers.take() {
            for handle in handles {
                let _ = handle.await;
            }
        }
    }
}

/// Body of one worker task: claims jobs one at a time off the shared
/// receiver and runs each to a terminal state (retries included) before
/// claiming the next. `recv()` under the shared-receiver lock serializes
/// claiming, so a `workers = 1` queue is strictly FIFO.
async fn worker_loop(
    db: Arc<MetaDatabase>,
    transport: Arc<dyn CloudTransport>,
    cfg: UploadQueueConfig,
    rx: Arc<AsyncMutex<mpsc::Receiver<UploadJob>>>,
    stats: Arc<StatsCounters>,
) {
    loop {
        // Hold the receiver lock only while claiming: a parked `recv`
        // hands the lock to the next worker as soon as a job arrives.
        let job = {
            let mut rx = rx.lock().await;
            rx.recv().await
        };
        match job {
            Some(job) => process_job(db.as_ref(), transport.as_ref(), &cfg, &stats, job).await,
            None => return, // channel closed and drained: shut down
        }
    }
}

/// The staged encryption of one job: the effective ciphertext job the
/// transport must see. The plaintext sha256 is not staged here — it is
/// computed once in [`process_job`] before the first attempt (same
/// order as the Python baseline: digest first, then encryption).
struct StagedUpload {
    /// Effective job over the ciphertext temp file (same rel_path and
    /// chunk split size; size/chunk_count re-planned on the ciphertext).
    job: UploadJob,
}

/// Sibling staging path of the plaintext cache copy for the encrypted
/// upload: the full file name plus `.enc.tmp` (`foo.txt` ->
/// `foo.txt.enc.tmp`), in the same directory so the cleanup and the
/// cache-tree walks both see it.
fn enc_tmp_sibling(target: &Path) -> std::path::PathBuf {
    let file_name = target.file_name().unwrap_or_else(|| OsStr::new("cydrive"));
    let mut staged = file_name.to_os_string();
    staged.push(".enc.tmp");
    target.with_file_name(staged)
}

/// Whole-file encryption staging (Python `telegram_client.py:162-177`,
/// the adjudicated v1 semantics: plaintext -> whole-file v1 crypto format
/// -> the ciphertext is what gets chunked and uploaded).
///
/// 1. read the plaintext (already hashed by the caller —
///    [`precompute_sha256`] runs before staging, matching the Python
///    digest-before-encryption order);
/// 2. encrypt it in memory (Python parity — the whole file is buffered;
///    streaming is an explicitly deferred v2 item) and write the
///    ciphertext to a sibling `{name}.enc.tmp`;
/// 3. re-plan the chunk split over the ciphertext bytes.
///
/// I/O failures surface to the caller as ordinary upload failures
/// (retry/degrade; the plaintext is never touched).
fn stage_encrypted(job: &UploadJob, password: &str) -> std::io::Result<StagedUpload> {
    let plaintext = std::fs::read(&job.local_path)?;
    let ciphertext = crate::crypto::encrypt(password, &plaintext);
    let tmp = enc_tmp_sibling(&job.local_path);
    std::fs::write(&tmp, &ciphertext)?;
    let size = ciphertext.len() as u64;
    let chunk_count = size.div_ceil(job.chunk_size.max(1)) as u32;
    Ok(StagedUpload {
        job: UploadJob {
            rel_path: job.rel_path.clone(),
            local_path: tmp,
            size,
            chunk_count,
            chunk_size: job.chunk_size,
        },
    })
}

/// finally-semantics cleanup of the encrypted staging file: dropped on
/// every exit path of [`process_job`] (success, degradation, panic),
/// deleting the ciphertext temp file whether or not the upload succeeded
/// (Python `finally` in `telegram_client.py:290-296`). The plaintext
/// cache copy is *not* this guard's business — that one is still deleted
/// only after a successful upload.
struct EncTempGuard(Option<std::path::PathBuf>);

impl Drop for EncTempGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "failed to delete the encrypted staging file"
                    );
                }
            }
        }
    }
}

/// Exact ciphertext length of a v2 container for `plain` plaintext bytes
/// at the default 1 MiB crypto chunk: header + per chunk (plain_len +
/// tag); the empty file carries one tag-only final chunk (the encoder
/// always seals at least one chunk, so the container is
/// self-describing). Used to plan the storage chunk split BEFORE the
/// stream runs — the transport's chunk plan must be exact.
fn v2_cipher_size(plain: u64) -> u64 {
    const CRYPTO_CHUNK: u64 = cloudkit_crypto::v2::DEFAULT_CHUNK_SIZE as u64;
    const HEADER: u64 = cloudkit_crypto::v2::HEADER_SIZE as u64;
    const TAG: u64 = cloudkit_crypto::v2::TAG_SIZE as u64;
    let n = if plain == 0 {
        1
    } else {
        plain.div_ceil(CRYPTO_CHUNK)
    };
    HEADER + plain + n * TAG
}

/// `std::io::Write` end of the streaming-encryption bridge: every write
/// the v2 encryptor emits (header once, then one chunk+tag at a time)
/// becomes one channel frame. Bounded channel capacity is the
/// backpressure that keeps the whole pipeline at a few crypto chunks of
/// resident memory; `blocking_send` is legal because the encryptor runs
/// inside `spawn_blocking`.
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tx
            .blocking_send(Ok(bytes::Bytes::copy_from_slice(buf)))
            .map(|_| buf.len())
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "upload_stream consumer dropped the ciphertext stream",
                )
            })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One v2 streaming upload attempt (E-3): the plaintext cache copy is
/// re-opened, streamed through the v2 chunked-AEAD encryptor on a
/// blocking thread, and the ciphertext frames flow through a bounded
/// channel straight into [`CloudTransport::upload_stream`] — no
/// ciphertext file ever exists on disk, and resident memory stays at
/// channel capacity × crypto chunk regardless of file size. Returns the
/// receipt plus the ciphertext job the transport saw (the persist step's
/// chunk math follows ciphertext boundaries).
///
/// Each call re-derives the key (fresh salt, fresh PBKDF2) — retries are
/// the rare path and a per-attempt salt is the cryptographically
/// conservative direction; a partially-uploaded previous attempt left
/// nothing local to reuse by design.
async fn upload_v2_stream(
    transport: &dyn CloudTransport,
    job: &UploadJob,
    password: &str,
) -> Result<(UploadReceipt, UploadJob), StorageError> {
    let cipher_size = v2_cipher_size(job.size);
    let cipher_chunk_count = if cipher_size == 0 {
        0
    } else {
        cipher_size.div_ceil(job.chunk_size.max(1)) as u32
    };
    let cipher_job = UploadJob {
        rel_path: job.rel_path.clone(),
        // Provenance only: the bytes on the wire are the stream's.
        local_path: job.local_path.clone(),
        size: cipher_size,
        chunk_count: cipher_chunk_count,
        chunk_size: job.chunk_size,
    };

    // Encrypt on a blocking thread; the bounded channel (a few frames)
    // turns the sync writer into the async stream with backpressure.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    let plain_path = job.local_path.clone();
    let password = password.to_string();
    let encrypt = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        use cloudkit_crypto::CryptoScheme as _;
        let mut src = std::fs::File::open(&plain_path)?;
        let scheme = cloudkit_crypto::AeadV2::new();
        let mut dst = ChannelWriter { tx };
        match scheme.encrypt_stream(&password, &mut src, &mut dst) {
            Ok(written) => Ok(written),
            Err(cloudkit_crypto::CryptoError::Io(error)) => Err(error),
            Err(error) => Err(std::io::Error::other(error.to_string())),
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| {
            let frame = item.map_err(|error| StorageError::Io(error.to_string()));
            (frame, rx)
        })
    });
    let stream: crate::transport::ByteStream = Box::pin(stream);

    let receipt = match transport.upload_stream(&cipher_job, stream).await {
        Ok(receipt) => receipt,
        Err(error) => {
            // The consumer is gone; join the encryptor so it exits
            // through the BrokenPipe path instead of leaking the task.
            let _ = encrypt.await;
            return Err(error);
        }
    };
    let written = encrypt
        .await
        .map_err(|error| StorageError::Unavailable(format!("v2 encryption task failed: {error}")))?
        .map_err(|error| StorageError::Io(error.to_string()))?;
    if written != cipher_size {
        return Err(StorageError::Unavailable(format!(
            "v2 ciphertext length {written} disagrees with the planned {cipher_size}; \
             refusing to persist a chunk plan computed on the wrong size"
        )));
    }
    Ok((receipt, cipher_job))
}

/// Runs one job to a terminal state, updating the stats counters and
/// never touching a row's remote state on failure paths.
async fn process_job(
    db: &MetaDatabase,
    transport: &dyn CloudTransport,
    cfg: &UploadQueueConfig,
    stats: &StatsCounters,
    job: UploadJob,
) {
    let row = match db.get_file(job.rel_path.as_str()) {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(rel_path = %job.rel_path, "upload job without a files row; degrading");
            bump(&stats.degraded);
            return;
        }
        Err(error) => {
            tracing::warn!(rel_path = %job.rel_path, %error, "metadata read failed; degrading");
            bump(&stats.degraded);
            return;
        }
    };

    // The plaintext digest is computed once, here — before the first
    // upload attempt and before any encryption staging, while the local
    // copy is guaranteed to still exist. Hashing after remote success
    // raced with a same-path rewrite: the earlier job's success-delete
    // removed the cache copy out from under this hash (field log: os
    // error 2, digest silently dropped). 0 bytes hashes too — the empty
    // digest is well-defined (Python parity).
    let sha256 = precompute_sha256(&job);

    // Contract 6: 0-byte uploads never touch the remote.
    if job.size == 0 {
        // MiniRedir's small-file chain opens with an empty PUT artifact
        // (empty PUT → LOCK → full PUT). When that artifact's job is
        // processed after a superseding full PUT has already updated the
        // row, `row` no longer describes an empty file: fast-pathing here
        // would mark the row uploaded without any remote artifact AND
        // delete the cache copy the superseding job still has to read
        // (field log 2026-09-09: baidu demo readme.txt — the full job died
        // 5× os error 2, the row phantom-uploaded with no msg id, the
        // remote stayed empty). The row above is read fresh, so `row.size`
        // is the current truth: a 0-byte job over a non-empty row is a
        // stale artifact — skip it entirely; the superseding job owns the
        // outcome.
        if row.size != 0 {
            tracing::debug!(
                rel_path = %job.rel_path,
                row_size = row.size,
                "stale empty-PUT artifact skipped: the row was superseded by a full PUT"
            );
            // Terminal accounting: the job was already counted in
            // `enqueued`, so the skip must land in exactly one terminal
            // counter or the drain predicate (enqueued − terminal) never
            // settles and REMOVE/LIST wait forever (H2's ghost
            // outstanding). Counted as `degraded` — this queue's
            // established "not uploaded" terminal state (same as the
            // unknown-row and metadata-failure paths): no requeue side
            // effects, and the db row belongs to the superseding full
            // PUT's job.
            bump(&stats.degraded);
            return;
        }
        match persist_zero_byte(db, &row, sha256) {
            Ok(()) => {
                delete_local_copy(&job.local_path);
                bump(&stats.succeeded);
            }
            Err(error) => {
                tracing::warn!(
                    rel_path = %job.rel_path,
                    %error,
                    "0-byte persist failed; degrading"
                );
                bump(&stats.degraded);
            }
        }
        return;
    }

    // Encrypted staging: required only when the row is flagged encrypted
    // AND the queue carries a password (Python AND semantics). Staged
    // once and reused across retries of the same job; the guard deletes
    // the temp ciphertext on every exit path. V2 rows (E-3) skip this
    // entirely — their ciphertext exists only as a stream, per attempt.
    let use_v2 = row.is_encrypted
        && cfg.encryption_password.is_some()
        && cfg.encryption_scheme == crate::config::EncryptionScheme::AeadV2;
    let mut staged: Option<StagedUpload> = None;
    let mut enc_tmp = EncTempGuard(None);

    let mut consecutive_failures = 0u32;
    loop {
        // Lazily (re)attempt the staging: an I/O failure here behaves
        // like any other upload failure — backoff/degrade, plaintext
        // untouched, retried on the next loop pass.
        if !use_v2 && staged.is_none() && row.is_encrypted && cfg.encryption_password.is_some() {
            let password = cfg.encryption_password.as_deref().expect("checked above");
            match stage_encrypted(&job, password) {
                Ok(s) => {
                    enc_tmp.0 = Some(s.job.local_path.clone());
                    staged = Some(s);
                }
                Err(error) => {
                    let error = StorageError::Io(error.to_string());
                    consecutive_failures += 1;
                    match decide_retry(&cfg.retry, &error, consecutive_failures) {
                        RetryDecision::RetryAfter(delay) => {
                            tracing::info!(
                                rel_path = %job.rel_path,
                                attempt = consecutive_failures,
                                delay = ?delay,
                                %error,
                                "encryption staging failed; backing off"
                            );
                            tokio::time::sleep(delay).await;
                            bump(&stats.retries);
                            continue;
                        }
                        RetryDecision::Degrade => {
                            tracing::warn!(
                                rel_path = %job.rel_path,
                                attempts = consecutive_failures,
                                %error,
                                "encryption staging retries exhausted; degrading"
                            );
                            bump(&stats.degraded);
                            return;
                        }
                    }
                }
            }
        }
        // What the transport sees: the v2 ciphertext stream (re-encrypted
        // per attempt, zero staging file), the v1 ciphertext job when
        // staged, or the plaintext job otherwise (password None is
        // byte-identical to the pre-encryption behavior). All three
        // resolve to the receipt plus the job the transport actually
        // saw, so the persist step's chunk math follows the bytes on the
        // wire.
        let outcome = if use_v2 {
            let password = cfg
                .encryption_password
                .as_deref()
                .expect("use_v2 implies a password");
            upload_v2_stream(transport, &job, password).await
        } else {
            let target = staged.as_ref().map(|s| &s.job).unwrap_or(&job);
            transport
                .upload(target)
                .await
                .map(|receipt| (receipt, target.clone()))
        };
        match outcome {
            Ok((receipt, target)) => {
                // The digest was computed up front (before the first
                // attempt), so it is already in hand for both the staged
                // (plaintext digest of the encrypted upload) and the
                // plaintext path — no hash runs after the remote success.
                // Row size: encrypted rows (v1 staged or v2 streamed)
                // keep their plaintext size (Python upserts `file_size`,
                // the plaintext length); plaintext rows take the
                // receipt's byte count as before.
                let file_size = if use_v2 || staged.is_some() {
                    row.size
                } else {
                    receipt.uploaded_bytes as i64
                };
                match persist_success(db, &row, &target, &receipt, file_size, sha256) {
                    Ok(()) => {
                        delete_local_copy(&job.local_path);
                        // No manual sync doorbell anymore: the success
                        // persist's files upsert rang it through the
                        // db-layer update hook (the chokepoint) — a pass
                        // from here on pushes the real (uploaded) payload
                        // instead of a pending one.
                        bump(&stats.succeeded);
                    }
                    Err(error) => {
                        // No re-upload: the bytes are remote already, a
                        // retry cannot fix the DB and would duplicate
                        // remote data. The kept local copy survives for
                        // a later `requeue_pending`.
                        tracing::warn!(
                            rel_path = %job.rel_path,
                            %error,
                            "post-upload persist failed; degrading"
                        );
                        bump(&stats.degraded);
                    }
                }
                return; // `enc_tmp` drops here: ciphertext temp deleted.
            }
            Err(StorageError::RateLimited {
                retry_after: Some(wait),
            }) => {
                tracing::info!(
                    rel_path = %job.rel_path,
                    retry_after = ?wait,
                    "flood wait; retrying the whole upload"
                );
                tokio::time::sleep(wait).await;
                bump(&stats.retries);
                continue;
            }
            Err(error) => {
                consecutive_failures += 1;
                match decide_retry(&cfg.retry, &error, consecutive_failures) {
                    RetryDecision::RetryAfter(delay) => {
                        tracing::info!(
                            rel_path = %job.rel_path,
                            attempt = consecutive_failures,
                            delay = ?delay,
                            %error,
                            "upload failed; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        bump(&stats.retries);
                        continue;
                    }
                    RetryDecision::Degrade => {
                        // Row stays is_uploaded = 0; the local file is kept.
                        tracing::warn!(
                            rel_path = %job.rel_path,
                            attempts = consecutive_failures,
                            %error,
                            "upload retries exhausted; degrading"
                        );
                        bump(&stats.degraded);
                        // Best-effort bot notice, always on (degradation is
                        // a rare terminal state worth surfacing — one per
                        // degraded job); a failed notification must never
                        // break the degrade path. Capability probing
                        // (interfaces §1): a transport without CHAT skips
                        // the notice with a log line, never panics.
                        let notice = format!(
                            "⚠️ CyDrive: upload failed after {n} attempts: {rel} — kept on disk, will retry on next start",
                            n = consecutive_failures,
                            rel = job.rel_path,
                        );
                        match transport.as_chat() {
                            Some(chat) => {
                                if let Err(notify_error) = chat.send_text(&notice).await {
                                    tracing::warn!(
                                        %notify_error,
                                        rel_path = %job.rel_path,
                                        "degrade notification failed"
                                    );
                                }
                            }
                            None => tracing::info!(
                                rel_path = %job.rel_path,
                                "transport declares no CHAT capability; degrade notice skipped"
                            ),
                        }
                        return; // `enc_tmp` drops here too.
                    }
                }
            }
        }
    }
}

/// Computes the row's sha256 from the local plaintext copy BEFORE any
/// upload attempt (Python baseline, `telegram_client.py:160`: the digest
/// covers the plaintext and the size gate uses the plaintext size —
/// `telegram_client.py:162-177` hashes before encrypting, so hashing
/// before staging is the same order). Sizes at or below
/// [`SHA256_MAX_BYTES`] get the plaintext digest, larger ones None. A
/// hash failure only warns and yields None — the digest is bonus
/// metadata and must not fail the upload.
///
/// Timing note: this used to run after remote success, but a same-path
/// rewrite lets the earlier job's success-delete remove the cache copy
/// out from under the hash (field log: os error 2, digest lost). Here
/// the copy is guaranteed to still exist: it is deleted only after a
/// successful persist, and every path that consumes the digest (0-byte
/// persist, plaintext persist, encrypted staging) runs after this point.
fn precompute_sha256(job: &UploadJob) -> Option<String> {
    if !should_hash(job.size) {
        return None;
    }
    match crate::chunker::sha256_file(&job.local_path) {
        Ok(hex) => Some(hex),
        Err(error) => {
            tracing::warn!(
                rel_path = %job.rel_path,
                path = %job.local_path.display(),
                %error,
                "sha256 pre-computation failed; storing None"
            );
            None
        }
    }
}

/// Builds the upsert flipping `row` to the uploaded state, keeping every
/// row field the queue does not own (name/mtime/encryption/mime); sha256
/// is the job's own pre-computed digest (None for oversized files or a
/// failed computation; the DB layer then coalesces to any pre-existing
/// value).
fn uploaded_upsert(
    row: &FileRecord,
    size: i64,
    telegram_msg_id: Option<i64>,
    chunk_count: i64,
    sha256: Option<String>,
) -> FileUpsert {
    FileUpsert {
        rel_path: row.rel_path.clone(),
        name: row.name.clone(),
        parent_dir: row.parent_dir.clone(),
        size,
        mtime: row.mtime,
        sha256,
        is_dir: row.is_dir,
        telegram_msg_id,
        is_uploaded: true,
        is_cached: false,
        is_encrypted: row.is_encrypted,
        chunk_count,
        mime_type: row.mime_type.clone(),
    }
}

/// Success persistence for the 0-byte fast path: no receipt exists, so
/// the row keeps its own size/chunk_count and gains no msg id.
fn persist_zero_byte(
    db: &MetaDatabase,
    row: &FileRecord,
    sha256: Option<String>,
) -> Result<(), QueueError> {
    db.upsert_file(&uploaded_upsert(
        row,
        row.size,
        None,
        row.chunk_count,
        sha256,
    ))?;
    Ok(())
}

/// Success persistence for a real receipt: the row's size becomes
/// `file_size` (the plaintext row size for encrypted uploads — the
/// receipt's byte count is the *ciphertext* length there — and the
/// receipt's byte count for plaintext uploads), `chunk_count` counts the
/// receipt's messages, and one `chunks` row is written per uploaded
/// message — every full chunk at the `job`'s chunk size, the last one
/// carrying the remainder. `job` is the job the transport actually saw
/// (the ciphertext job for encrypted uploads), so the per-chunk sizes
/// follow the ciphertext boundaries.
fn persist_success(
    db: &MetaDatabase,
    row: &FileRecord,
    job: &UploadJob,
    receipt: &UploadReceipt,
    file_size: i64,
    sha256: Option<String>,
) -> Result<(), QueueError> {
    let chunk_count = receipt.chunk_msg_ids.len() as i64;
    let file_id = db.upsert_file(&uploaded_upsert(
        row,
        file_size,
        // K1: receipt ids are already the DB's i64 width.
        Some(receipt.first_msg_id),
        chunk_count,
        sha256,
    ))?;
    for (index, &msg_id) in receipt.chunk_msg_ids.iter().enumerate() {
        let index = index as i64;
        let size = if index + 1 < chunk_count {
            job.chunk_size as i64
        } else {
            // Last chunk (covers the single-chunk case: n-1 == 0).
            receipt.uploaded_bytes as i64 - (job.chunk_size as i64) * (chunk_count - 1)
        };
        db.upsert_chunk(file_id, index, msg_id, size, None)?;
    }
    Ok(())
}

/// Removes the local cache copy after a successful persist. A missing
/// file is fine; any other failure only warns — the remote state is
/// already consistent, so an orphaned cache file is a cache-layer issue,
/// not an upload failure.
fn delete_local_copy(local_path: &Path) {
    if let Err(error) = std::fs::remove_file(local_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %local_path.display(),
                %error,
                "failed to delete the local copy after upload"
            );
        }
    }
}

/// Spawns `cfg.workers` worker tasks over one bounded channel and returns
/// the queue handle.
///
/// Per-job semantics (frozen by `tests/upload_queue.rs`):
///
/// 1. `db.get_file(rel)` has no row → count degraded, `tracing::warn`,
///    no remote call, no local delete.
/// 2. `job.size == 0` → skip the transport entirely (contract: 0-byte
///    uploads never go remote), persist as success (`telegram_msg_id =
///    None`, the row's `chunk_count` kept, `is_cached = false`, local
///    file deleted), count succeeded.
/// 3. Rows flagged `is_encrypted` while `cfg.encryption_password` is
///    `Some` are staged first (Python `telegram_client.py:162-177`): the
///    plaintext was already hashed before staging, is encrypted whole-file
///    into a sibling `{name}.enc.tmp`, and the *ciphertext* is what gets
///    uploaded and chunk-planned. Staging I/O failures count as ordinary
///    upload failures (retry/degrade, plaintext kept); the temp file is
///    deleted on every exit path (success and failure alike). A row
///    without either half of the AND condition uploads plaintext,
///    byte-identical to the pre-encryption behavior.
/// 4. Otherwise loop over `transport.upload(job)`:
///    - `Ok(receipt)` → persist `is_uploaded = true`, `telegram_msg_id =
///      Some(first)`, `chunk_count` and the per-chunk rows from the
///      receipt (last chunk = `uploaded_bytes - (n-1) * chunk_size`, over
///      the bytes the transport saw — ciphertext boundaries when
///      encrypted), `size` = the receipt's byte count (plaintext rows)
///      or the row's kept plaintext size (encrypted rows),
///      `sha256` = the digest computed before the first attempt (None
///      for oversized files or a failed hash — warn only, never an
///      upload failure); delete the local file; count succeeded.
///    - `Err(RateLimited { retry_after: Some(s) })` (the old FloodWait) → sleep
///      `s` (0 sleeps nothing), count
///      retries, retry the whole upload (never counts toward
///      degradation).
///    - any other `Err` → [`decide_retry`]: sleep-and-retry (count
///      retries) or degrade (count degraded, keep the local file, row
///      stays `is_uploaded = 0`, and one best-effort bot notice goes out
///      via `send_text` — a failed notice only warns and never breaks
///      the degrade path).
/// 5. Persistence errors on the success path → no re-upload (a retry
///    cannot fix the DB and would duplicate remote data): count
///    degraded, `warn`.
pub fn spawn_queue(
    db: Arc<MetaDatabase>,
    transport: Arc<dyn CloudTransport>,
    cfg: UploadQueueConfig,
) -> UploadQueueHandle {
    if cfg.workers == 0 {
        tracing::warn!("upload queue configured with 0 workers; running one");
    }
    let worker_count = cfg.workers.max(1);
    // A zero bound would panic inside tokio's bounded channel ctor.
    let (tx, rx) = mpsc::channel(cfg.queue_capacity.max(1));
    let rx = Arc::new(AsyncMutex::new(rx));
    let stats = Arc::new(StatsCounters::default());

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        handles.push(tokio::spawn(worker_loop(
            Arc::clone(&db),
            Arc::clone(&transport),
            cfg.clone(),
            Arc::clone(&rx),
            Arc::clone(&stats),
        )));
    }

    let inner = Arc::new(QueueInner {
        tx: AsyncMutex::new(Some(tx)),
        workers: AsyncMutex::new(Some(handles)),
        stats,
        cfg,
        db,
    });
    UploadQueueHandle { inner }
}
