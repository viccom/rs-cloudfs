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
//! RED phase stub: every function body is `todo!()`; the frozen behavior
//! contract is encoded by the tests under `tests/upload_queue.rs`.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::CacheManager;
use crate::database::{DbError, MetaDatabase};
use crate::transport::{CloudTransport, TransportError, UploadJob};

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
        todo!()
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
#[allow(unused_variables)] // RED stub: body is todo!()
pub fn decide_retry(
    policy: &RetryPolicy,
    error: &TransportError,
    consecutive_failures: u32,
) -> RetryDecision {
    todo!()
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
        todo!()
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

/// Handle to a spawned queue; interior state is private.
pub struct UploadQueueHandle {}

impl UploadQueueHandle {
    /// Pushes `job` into the bounded channel; `Err(QueueError::Closed)`
    /// once the queue has been shut down.
    #[allow(unused_variables)] // RED stub: body is todo!()
    pub async fn enqueue(&self, job: UploadJob) -> Result<(), QueueError> {
        todo!()
    }

    /// Snapshot of the queue counters.
    pub fn stats(&self) -> QueueStats {
        todo!()
    }

    /// Re-enqueues every pending upload whose local copy still exists.
    ///
    /// Scans `list_all_files()` for non-directory `is_uploaded = false`
    /// rows; a row whose cache file is missing is skipped (the Python
    /// version lost such files on power loss — only uploadable files are
    /// enqueued). Each surviving row becomes an `UploadJob` sized from the
    /// real file metadata. Returns the number of jobs enqueued.
    #[allow(unused_variables)] // RED stub: body is todo!()
    pub async fn requeue_pending(&self, cache: &CacheManager) -> Result<usize, QueueError> {
        todo!()
    }

    /// Shuts the queue down, idempotently: closes the channel, waits for
    /// the workers to drain it (in-flight retries included) and joins
    /// them. Every job enqueued before shutdown reaches a terminal state.
    pub async fn shutdown(&self) {
        todo!()
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
#[allow(unused_variables)] // RED stub: body is todo!()
pub fn spawn_queue(
    db: Arc<MetaDatabase>,
    transport: Arc<dyn CloudTransport>,
    cfg: UploadQueueConfig,
) -> UploadQueueHandle {
    todo!()
}
