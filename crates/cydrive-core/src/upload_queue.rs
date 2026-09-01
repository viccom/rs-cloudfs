//! Bounded upload queue driving [`CloudTransport::upload`] for pending
//! files (`is_uploaded = 0` rows of the metadata DB).
//!
//! Contract sources: the design doc «上传队列（core）» and the Python
//! behavior baseline (contracts 6/7): 0-byte uploads skip the transport
//! entirely, FloodWait sleeps the exact server-provided seconds and
//! retries the whole upload, every other failure backs off exponentially
//! and degrades after `max_attempts` consecutive tries, and the local
//! cache copy is deleted only after a successful upload (fixing the
//! Python unconditional-delete bug).
//!
//! Layout: the handle fronts one [`QueueInner`] (sender slot, worker
//! join handles, atomic counters, config); worker tasks share the single
//! `mpsc::Receiver` behind an async mutex and claim one job at a time,
//! so a single-worker queue is strictly FIFO. The frozen behavior
//! contract is encoded by the tests under `tests/upload_queue.rs`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::cache::CacheManager;
use crate::database::{DbError, FileRecord, FileUpsert, MetaDatabase};
use crate::rel_path::RelPath;
use crate::transport::{CloudTransport, TransportError, UploadJob, UploadReceipt};

/// File size above which uploads split into `.partNNN` chunks; 1900 MB
/// keeps every message under Telegram's 2 GB per-message cap (contract 5).
pub const DEFAULT_CHUNK_SIZE_MB: u64 = 1900;

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
/// - `FloodWait { seconds }` → [`RetryDecision::RetryAfter`] of exactly
///   those seconds: never clamped by `max_backoff`, never degraded no
///   matter how large `consecutive_failures` is.
/// - Any other error, with `n = consecutive_failures` (counting the
///   current failure, starting at 1): `n >= max_attempts` degrades,
///   otherwise retry after `min(initial * 2^(n-1), max)`.
pub fn decide_retry(
    policy: &RetryPolicy,
    error: &TransportError,
    consecutive_failures: u32,
) -> RetryDecision {
    // FloodWait is the server's authoritative word: honor it exactly,
    // before any attempt accounting (never clamped, never degrading).
    if let TransportError::FloodWait { seconds } = error {
        return RetryDecision::RetryAfter(Duration::from_secs(u64::from(*seconds)));
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
}

impl Default for UploadQueueConfig {
    /// 2 workers, capacity 256, default retry, 1900 MB chunks.
    fn default() -> Self {
        Self {
            workers: 2,
            queue_capacity: 256,
            retry: RetryPolicy::default(),
            chunk_size_bytes: DEFAULT_CHUNK_SIZE_MB * 1024 * 1024,
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
            Some(job) => {
                process_job(db.as_ref(), transport.as_ref(), &cfg.retry, &stats, job).await
            }
            None => return, // channel closed and drained: shut down
        }
    }
}

/// Runs one job to a terminal state, updating the stats counters and
/// never touching a row's remote state on failure paths.
async fn process_job(
    db: &MetaDatabase,
    transport: &dyn CloudTransport,
    retry_policy: &RetryPolicy,
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

    // Contract 6: 0-byte uploads never touch the remote.
    if job.size == 0 {
        match persist_zero_byte(db, &row) {
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

    let mut consecutive_failures = 0u32;
    loop {
        match transport.upload(&job).await {
            Ok(receipt) => {
                match persist_success(db, &row, &job, &receipt) {
                    Ok(()) => {
                        delete_local_copy(&job.local_path);
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
                return;
            }
            Err(TransportError::FloodWait { seconds }) => {
                tracing::info!(
                    rel_path = %job.rel_path,
                    seconds,
                    "flood wait; retrying the whole upload"
                );
                tokio::time::sleep(Duration::from_secs(u64::from(seconds))).await;
                bump(&stats.retries);
                continue;
            }
            Err(error) => {
                consecutive_failures += 1;
                match decide_retry(retry_policy, &error, consecutive_failures) {
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
                        return;
                    }
                }
            }
        }
    }
}

/// Builds the upsert flipping `row` to the uploaded state, keeping every
/// row field the queue does not own (name/mtime/sha256/encryption/mime).
fn uploaded_upsert(
    row: &FileRecord,
    size: i64,
    telegram_msg_id: Option<i64>,
    chunk_count: i64,
) -> FileUpsert {
    FileUpsert {
        rel_path: row.rel_path.clone(),
        name: row.name.clone(),
        parent_dir: row.parent_dir.clone(),
        size,
        mtime: row.mtime,
        sha256: row.sha256.clone(),
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
fn persist_zero_byte(db: &MetaDatabase, row: &FileRecord) -> Result<(), QueueError> {
    db.upsert_file(&uploaded_upsert(row, row.size, None, row.chunk_count))?;
    Ok(())
}

/// Success persistence for a real receipt: refresh size/chunk_count from
/// the receipt, then write one `chunks` row per uploaded message — every
/// full chunk at `chunk_size`, the last one carrying the remainder.
fn persist_success(
    db: &MetaDatabase,
    row: &FileRecord,
    job: &UploadJob,
    receipt: &UploadReceipt,
) -> Result<(), QueueError> {
    let chunk_count = receipt.chunk_msg_ids.len() as i64;
    let file_id = db.upsert_file(&uploaded_upsert(
        row,
        receipt.uploaded_bytes as i64,
        Some(i64::from(receipt.first_msg_id)),
        chunk_count,
    ))?;
    for (index, &msg_id) in receipt.chunk_msg_ids.iter().enumerate() {
        let index = index as i64;
        let size = if index + 1 < chunk_count {
            job.chunk_size as i64
        } else {
            // Last chunk (covers the single-chunk case: n-1 == 0).
            receipt.uploaded_bytes as i64 - (job.chunk_size as i64) * (chunk_count - 1)
        };
        db.upsert_chunk(file_id, index, i64::from(msg_id), size, None)?;
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
/// 3. Otherwise loop over `transport.upload(job)`:
///    - `Ok(receipt)` → persist `is_uploaded = true`, `telegram_msg_id =
///      Some(first)`, `chunk_count` and the per-chunk rows from the
///      receipt (last chunk = `uploaded_bytes - (n-1) * chunk_size`),
///      `size = uploaded_bytes`; delete the local file; count succeeded.
///    - `Err(FloodWait { s })` → sleep `s` (0 sleeps nothing), count
///      retries, retry the whole upload (never counts toward
///      degradation).
///    - any other `Err` → [`decide_retry`]: sleep-and-retry (count
///      retries) or degrade (count degraded, keep the local file, row
///      stays `is_uploaded = 0`).
/// 4. Persistence errors on the success path → no re-upload (a retry
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
