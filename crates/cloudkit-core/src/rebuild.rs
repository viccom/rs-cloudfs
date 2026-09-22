//! Authoritative backend bootstrap of the metadata DB (Phase 2 / K11,
//! foundation D4 `rebuild_from_backend`): `cydrive rebuild` walks the
//! backend's authoritative index over the [`StorageDriver`] face
//! (`CloudTransport` has no list/stat) and upserts one `files` row per
//! entry.
//!
//! Row shape (the authoritative-index contract):
//!
//! - files: `is_uploaded = 1` (the bytes exist in the backend — nothing
//!   is pending), `chunk_count = 1` (whole-file view; the driver's
//!   chunking is invisible above L2), `telegram_msg_id` = the entry's
//!   fs_id-shaped handle parsed as i64 (path-shaped local handles
//!   degrade to the K6 `0` placeholder — the column cannot hold a
//!   path), `size`/`mtime` from the [`Entry`]; `sha256`/`mime_type`
//!   stay `NULL` so [`MetaDatabase::upsert_file`]'s coalesce keeps any
//!   stored value on a re-rebuild;
//! - chunks (per file row): one single-container row — index 0, the
//!   row's `msg_id`, the whole size, no sha. This is the exact shape
//!   [`persist_success`](crate::upload_queue) writes for a one-element
//!   `chunk_msg_ids` receipt, so a rebuilt file is row/chunks-
//!   equivalent to a sync-copied one (K11 bookkeeping parity).
//!   Directory rows write no chunks rows;
//! - directories: `create_dir` parity (`size = 0`, `chunk_count = 0`,
//!   `msg_id = NULL`, `is_uploaded = 1`, `is_cached = 1`).
//!
//! Walk shape (Phase 8 / D8, the read-through demotion): the K11
//! boxed-recursive walk is now an **explicit work queue** with a
//! persisted checkpoint —
//!
//! 1. resume (D8①): the remaining directory queue, the scan's start
//!    time and the cumulative entry count live in the `rebuild_state`
//!    KV table ([`MetaDatabase::rebuild_state_get`]), rewritten after
//!    EVERY completed directory. An interrupted pass (budget, crash,
//!    `Err`) leaves that checkpoint standing, so a rerun resumes from
//!    the cursor instead of re-scanning completed directories — a
//!    completed directory is listed exactly once per scan, not per
//!    process. A drained queue clears the checkpoint;
//! 2. bounds (D8②): [`RebuildLimits`] caps a pass at `max_entries`
//!    (checked at directory granularity, so a stop never leaves a
//!    half-listed directory and a rerun never re-lists a completed one)
//!    and an optional wall-clock budget (also enforced between list
//!    pages, where a mid-directory stop re-queues that directory);
//! 3. sweep (D8③): only a COMPLETING pass prunes —
//!    [`MetaDatabase::sweep_unseen`] deletes uploaded rows whose
//!    `updated_at` predates the scan's persisted `scan_started_at`
//!    (the K11 "no pruning" scope ended here). Three protections: rows
//!    materialized by any pass of the scan carry fresh `updated_at`
//!    (the anchor persists across resumed passes, so early-pass rows
//!    survive); in-flight rows (`is_uploaded = 0`) are excluded; and
//!    an interrupted pass never reaches the sweep at all. Terminology
//!    (review L5): this module's *sweep-ghost* is an `is_uploaded = 1`
//!    row whose backend object is gone — the sweep's only lawful
//!    target; a *pending* row (`is_uploaded = 0`) is NEVER one —
//!    in-flight (a live upload whose write-back revives it) and
//!    ghost-pending (its cache copy already vanished, K4 semantics)
//!    alike stay untouched.
//!
//! Plaintext-only semantics (K11): an instance with
//! `enable_encryption = true` is refused before any listing — the
//! backend only sees ciphertext containers under plaintext names, so a
//! rebuilt row would mislabel encrypted payloads as plaintext. Encrypted
//! multi-instance cold starts go through `cydrive sync` (the payload
//! carries `is_encrypted`/scheme — the full row semantics). See
//! [`ensure_plaintext_instance`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use cloudkit_storage::{EntryKind, Page, PageCursor, RelPath, StorageDriver, StorageError};

use crate::config::CyDriveConfig;
use crate::database::{DbError, MetaDatabase};

/// Counters of one [`rebuild_from_backend`] pass, for CLI display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildOutcome {
    /// File rows upserted (this pass).
    pub files: usize,
    /// Directory rows upserted (this pass).
    pub dirs: usize,
    /// Rows the completion sweep pruned (D8③). Always `0` on an
    /// interrupted pass — only a pass that drained its queue sweeps.
    pub pruned: usize,
    /// Why the pass stopped before draining its queue; `None` = it
    /// completed (queue drained, sweep ran, checkpoint cleared). An
    /// interrupted pass keeps its persisted checkpoint — a rerun
    /// resumes instead of re-scanning (D8①).
    pub interrupted: Option<RebuildInterrupted>,
}

/// Why a rebuild pass stopped before draining its queue (D8②). The
/// [`Display`](std::fmt::Display) names the budget and carries the
/// rerun-to-continue semantics; each entry point wraps it with its own
/// command spelling (`cydrive rebuild` / `REBUILD`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildInterrupted {
    /// [`RebuildLimits::max_entries`] was reached with directories
    /// still pending.
    EntriesBudget,
    /// [`RebuildLimits::time_budget`] elapsed.
    TimeBudget,
}

impl std::fmt::Display for RebuildInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntriesBudget => f.write_str("the entries budget ran out; rerun to continue"),
            Self::TimeBudget => f.write_str("the time budget ran out; rerun to continue"),
        }
    }
}

/// The completion sweep's volume floor (M3 / Phase 8 review): the sweep
/// is only ever attempted when the uploaded population is at most this
/// size, or the prune batch stays at or under half of it. Below the
/// floor a full prune is still loud enough to be recoverable; above it a
/// majority prune is treated as a silent listing failure (see the gate
/// in [`rebuild_from_backend_with`]).
const SWEEP_FLOOR_MIN_UPLOADED: usize = 100;

/// Per-pass bounds of one rebuild walk (D8②). Defaults: 200 000 entries
/// per pass, no wall-clock budget (the CLI injects the same 15 minutes
/// the live background rebuild has always had; the live path itself
/// keeps its R4 supervision as the wall-clock authority and feeds only
/// the entry cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildLimits {
    /// Entries materialized beyond which the pass stops gracefully,
    /// keeping its persisted queue for the rerun. Checked at directory
    /// granularity — a pass never stops mid-directory for this cap (one
    /// directory may overshoot it).
    pub max_entries: usize,
    /// Wall-clock budget for the whole pass; `None` = the caller
    /// supervises time itself. Checked between directories AND between
    /// list pages (a mid-directory stop re-queues that directory —
    /// idempotent upserts make the re-list exact).
    pub time_budget: Option<Duration>,
}

impl Default for RebuildLimits {
    fn default() -> Self {
        Self {
            max_entries: 200_000,
            time_budget: None,
        }
    }
}

/// Failures of the rebuild flow.
#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    /// A metadata DB write failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The backend listing failed at a directory (propagated with the
    /// path for diagnosis; nothing is swallowed — a partial bootstrap
    /// that looks complete would be worse than a loud failure).
    #[error("backend listing failed at {path}: {source}")]
    List {
        /// Volume-relative directory being listed.
        path: String,
        /// The driver's failure.
        #[source]
        source: StorageError,
    },
    /// The instance carries client-side encryption: rebuild is refused
    /// (K11 plaintext-only). The message points at `cydrive sync`.
    #[error(
        "rebuild refuses encrypted instances: the backend only sees ciphertext containers \
         under plaintext names, so a rebuilt row would mislabel encrypted payloads as \
         plaintext; use `cydrive sync` instead — the sync payload carries the encrypted \
         row semantics (is_encrypted/scheme) to the other instance (sync_url / \
         CYDRIVE_SYNC_URL)"
    )]
    EncryptedInstance,
    /// A rebuild checkpoint could not be serialized (unobserved in
    /// practice — `RelPath`/counts are JSON-trivial; surfaced rather
    /// than unwrapped).
    #[error("rebuild checkpoint serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
}

/// The K11 plaintext-only gate: `Ok` unless `enable_encryption` is on.
/// Pure — the CLI calls it before building any driver, and the tests
/// pin the actionable sync guidance in the [`RebuildError::EncryptedInstance`]
/// message.
pub fn ensure_plaintext_instance(cfg: &CyDriveConfig) -> Result<(), RebuildError> {
    if cfg.enable_encryption {
        Err(RebuildError::EncryptedInstance)
    } else {
        Ok(())
    }
}

/// Walks the backend tree from `root` and upserts one row per entry
/// into `db`, under the default [`RebuildLimits`] (200 000 entries per
/// pass, no wall-clock budget).
///
/// `root` is volume-relative (the [`StorageDriver`] vocabulary — the
/// CLI passes the volume root); row `rel_path`s are vpath-shaped
/// (`"/a.txt"`) to match every other writer of the `files` table. The
/// outcome counts rows written this pass, not rows visited — a re-rebuild
/// of an unchanged tree re-upserts and counts the same rows (idempotent
/// by the rel_path conflict key).
pub async fn rebuild_from_backend(
    driver: &dyn StorageDriver,
    db: &MetaDatabase,
    root: &RelPath,
) -> Result<RebuildOutcome, RebuildError> {
    rebuild_from_backend_with(driver, db, root, RebuildLimits::default()).await
}

/// [`rebuild_from_backend`] with explicit per-pass bounds — the D8②
/// seam (the CLI feeds its `RebuildTuning`; tests shrink the caps).
pub async fn rebuild_from_backend_with(
    driver: &dyn StorageDriver,
    db: &MetaDatabase,
    root: &RelPath,
    limits: RebuildLimits,
) -> Result<RebuildOutcome, RebuildError> {
    let mut outcome = RebuildOutcome::default();
    let deadline = limits.time_budget.map(|budget| Instant::now() + budget);

    // D8① resume: a persisted anchor means an earlier pass of THIS scan
    // left a checkpoint — continue it (the start time is reused, never
    // reset: it is the completion sweep's protection anchor). No
    // checkpoint = a fresh scan from `root`, anchored and persisted
    // BEFORE the first upsert, so every pass of the scan is sweep-immune
    // by construction.
    let (queue, scan_started_at, mut entries_done) = match load_scan_state(db)? {
        Some(ScanState {
            queue,
            scan_started_at,
            entries_done,
        }) => (queue, scan_started_at, entries_done),
        None => {
            let scan_started_at = unix_now();
            let queue = vec![root.clone()];
            persist_scan_state(db, &queue, scan_started_at, 0)?;
            (queue, scan_started_at, 0)
        }
    };
    let mut queue = VecDeque::from(queue);
    let mut entries_this_pass = 0usize;

    // The D8① work queue: an explicit deque replaced the K11 boxed
    // recursion. Every interrupt returns WITHOUT persisting — the
    // on-disk checkpoint is always the last completed directory's state
    // (the popped-but-unlisted directory is still on it), so a rerun
    // resumes exactly there. Pop-then-check: an empty queue always
    // reaches completion, even when the last directory spent the budget
    // exactly.
    while let Some(dir) = queue.pop_front() {
        // D8② bounds, at directory granularity: between directories, so
        // a stop never leaves a half-listed directory behind — and a
        // completed directory is never re-listed (the resume test pins
        // this by counting `list` calls).
        if entries_this_pass >= limits.max_entries {
            outcome.interrupted = Some(RebuildInterrupted::EntriesBudget);
            return Ok(outcome);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            outcome.interrupted = Some(RebuildInterrupted::TimeBudget);
            return Ok(outcome);
        }
        match walk_one_dir(driver, db, &dir, deadline, &mut outcome).await? {
            DirWalk::Completed { children, entries } => {
                // Depth-first after the directory row itself, so a
                // parent always exists before its children (the
                // recursion-era ordering, queue-flattened).
                queue.extend(children);
                entries_done += entries;
                entries_this_pass += entries;
                // D8① per-directory checkpoint: the remaining queue and
                // the cumulative count land in `rebuild_state` after
                // EVERY completed directory.
                persist_scan_state(db, queue.make_contiguous(), scan_started_at, entries_done)?;
            }
            // Mid-directory deadline stop: the directory goes back to
            // the queue front — in memory for clarity, and it never left
            // the persisted queue (the last checkpoint predates the
            // pop). It will be re-listed on the rerun; upserts are
            // idempotent and `entries_done` counts completed
            // directories only.
            DirWalk::DeadlineStop => {
                queue.push_front(dir);
                outcome.interrupted = Some(RebuildInterrupted::TimeBudget);
                return Ok(outcome);
            }
        }
    }

    // Completion (queue drained — only now, D8③): prune the rows the
    // whole scan never re-touched, then clear the checkpoint. An
    // interrupted pass returned above and reaches neither.
    //
    // M3 volume floor: when the prune batch would take more than half of
    // a sizable index (>`SWEEP_FLOOR_MIN_UPLOADED` uploaded rows), the
    // likelier explanation is a silently-empty listing-class failure (a
    // lying driver — the K74 silent-misplacement precedent) than a mass
    // remote deletion: abandon the ENTIRE sweep (zero rows deleted,
    // chunks included) and say so in the log. The pass still completes
    // and clears its checkpoint — the surviving rows are simply
    // re-judged by the next completing scan.
    let (uploaded, candidates) = db.sweep_census(scan_started_at)?;
    if uploaded > SWEEP_FLOOR_MIN_UPLOADED && candidates * 2 > uploaded {
        tracing::warn!(
            uploaded,
            candidates,
            "completion sweep abandoned: the prune batch would take more than half of \
             a sizable index — a silent backend listing failure is suspected, \
             nothing was deleted"
        );
    } else {
        outcome.pruned = db.sweep_unseen(scan_started_at)?;
    }
    db.rebuild_state_clear()?;
    Ok(outcome)
}

/// What one directory of the work queue produced.
enum DirWalk {
    /// The whole directory was listed (paginated to exhaustion) and
    /// every entry materialized.
    Completed {
        /// Subdirectories discovered, for the work queue.
        children: Vec<RelPath>,
        /// Entries materialized for this directory.
        entries: usize,
    },
    /// The deadline elapsed between pages: stop. The caller re-queues
    /// the directory — half-listed is not completed.
    DeadlineStop,
}

/// Lists one directory (`Page`-chained to exhaustion) and materializes
/// every entry, collecting subdirectories for the queue.
fn walk_one_dir<'a>(
    driver: &'a dyn StorageDriver,
    db: &'a MetaDatabase,
    dir: &'a RelPath,
    deadline: Option<Instant>,
    outcome: &'a mut RebuildOutcome,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<DirWalk, RebuildError>> + Send + 'a>>
{
    Box::pin(async move {
        let mut children = Vec::new();
        let mut entries = 0usize;
        let mut cursor = PageCursor::Start;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(DirWalk::DeadlineStop);
            }
            let listing = driver
                .list(dir, Page { limit: 512, cursor })
                .await
                .map_err(|source| RebuildError::List {
                    path: dir.as_str().to_string(),
                    source,
                })?;
            for entry in &listing.entries {
                // Phase 8 / D4: the Entry→row mapping moved verbatim to
                // `materialize::materialize_entry` (rebuild and
                // read-through share the single mapping); only the
                // outcome counting stays on the rebuild side.
                crate::materialize::materialize_entry(db, entry)?;
                entries += 1;
                if entry.kind == EntryKind::Dir {
                    outcome.dirs += 1;
                    children.push(entry.path.clone());
                } else {
                    outcome.files += 1;
                }
            }
            match listing.next {
                Some(next) => cursor = next,
                None => return Ok(DirWalk::Completed { children, entries }),
            }
        }
    })
}

/// The `rebuild_state` keys (D8①). Part of the checkpoint's contract —
/// the tests read them back by name.
const KEY_PENDING: &str = "pending";
const KEY_SCAN_STARTED_AT: &str = "scan_started_at";
const KEY_ENTRIES_DONE: &str = "entries_done";

/// The persisted checkpoint of an in-progress scan (D8①): the remaining
/// queue, the FIRST pass's start time, the cumulative entry count.
struct ScanState {
    queue: Vec<RelPath>,
    scan_started_at: f64,
    entries_done: usize,
}

/// Loads the persisted checkpoint, `None` = start a fresh scan. A
/// checkpoint we cannot parse (a hand-edited or foreign row) degrades
/// to a fresh scan with a `warn` — provably safe: a fresh scan re-lists
/// from the root, so every row it could have missed is re-upserted (and
/// thereby sweep-immune) by the fresh pass itself; nothing is silently
/// wrong, only work redone.
fn load_scan_state(db: &MetaDatabase) -> Result<Option<ScanState>, RebuildError> {
    let Some(raw_started) = db.rebuild_state_get(KEY_SCAN_STARTED_AT)? else {
        return Ok(None);
    };
    let scan_started_at: f64 = match raw_started.parse() {
        Ok(started) => started,
        Err(error) => {
            tracing::warn!(
                %error,
                "rebuild_state scan_started_at is unreadable; starting a fresh scan"
            );
            return Ok(None);
        }
    };
    let queue = match db.rebuild_state_get(KEY_PENDING)? {
        Some(raw) => match serde_json::from_str::<Vec<RelPath>>(&raw) {
            Ok(queue) => queue,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "rebuild_state pending queue is unreadable; starting a fresh scan"
                );
                return Ok(None);
            }
        },
        // The anchor without a queue is a torn write (crash between the
        // two keys): a fresh scan is the exact recovery — nothing had
        // completed yet.
        None => return Ok(None),
    };
    let entries_done = db
        .rebuild_state_get(KEY_ENTRIES_DONE)?
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);
    Ok(Some(ScanState {
        queue,
        scan_started_at,
        entries_done,
    }))
}

/// Writes the full checkpoint (D8①). The anchor goes first so a crash
/// mid-write leaves "anchor without queue" — the torn-write shape
/// [`load_scan_state`] recovers from with a fresh scan.
fn persist_scan_state(
    db: &MetaDatabase,
    queue: &[RelPath],
    scan_started_at: f64,
    entries_done: usize,
) -> Result<(), RebuildError> {
    db.rebuild_state_set(KEY_SCAN_STARTED_AT, &scan_started_at.to_string())?;
    db.rebuild_state_set(KEY_PENDING, &serde_json::to_string(queue)?)?;
    db.rebuild_state_set(KEY_ENTRIES_DONE, &entries_done.to_string())?;
    Ok(())
}

/// Seconds since the Unix epoch — the same clock
/// [`MetaDatabase::upsert_file`] stamps `updated_at` with, so the
/// persisted anchor and the rows it protects are always comparable.
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
