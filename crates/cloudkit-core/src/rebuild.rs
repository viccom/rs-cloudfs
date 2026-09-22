//! Authoritative backend bootstrap of the metadata DB (Phase 2 / K11,
//! foundation D4 `rebuild_from_backend`): `cydrive rebuild` walks the
//! backend's authoritative index (recursive `list` over the
//! [`StorageDriver`] face — `CloudTransport` has no list/stat) and
//! upserts one `files` row per entry.
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
//! Scope (K11): bootstrap only — **no pruning**. Rows for paths the
//! backend no longer carries are left untouched; deleting them is the
//! delete-wiring unit's semantics (K4), not rebuild's.
//!
//! Plaintext-only semantics (K11): an instance with
//! `enable_encryption = true` is refused before any listing — the
//! backend only sees ciphertext containers under plaintext names, so a
//! rebuilt row would mislabel encrypted payloads as plaintext. Encrypted
//! multi-instance cold starts go through `cydrive sync` (the payload
//! carries `is_encrypted`/scheme — the full row semantics). See
//! [`ensure_plaintext_instance`].

use cloudkit_storage::{EntryKind, Page, PageCursor, RelPath, StorageDriver, StorageError};

use crate::config::CyDriveConfig;
use crate::database::{DbError, MetaDatabase};

/// Counters of one [`rebuild_from_backend`] pass, for CLI display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildOutcome {
    /// File rows upserted.
    pub files: usize,
    /// Directory rows upserted.
    pub dirs: usize,
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

/// Walks the backend tree from `root` (depth-first, `list` paginated to
/// exhaustion per directory) and upserts one row per entry into `db`.
///
/// `root` is volume-relative (the [`StorageDriver`] vocabulary — the
/// CLI passes the volume root); row `rel_path`s are vpath-shaped
/// (`"/a.txt"`) to match every other writer of the `files` table. The
/// outcome counts rows written, not rows visited — a re-rebuild of an
/// unchanged tree re-upserts and counts the same rows (idempotent
/// by the rel_path conflict key).
pub async fn rebuild_from_backend(
    driver: &dyn StorageDriver,
    db: &MetaDatabase,
    root: &RelPath,
) -> Result<RebuildOutcome, RebuildError> {
    let mut outcome = RebuildOutcome::default();
    rebuild_dir(driver, db, root, &mut outcome).await?;
    Ok(outcome)
}

/// Lists one directory (`Page`-chained to exhaustion) and recurses into
/// subdirectories.
fn rebuild_dir<'a>(
    driver: &'a dyn StorageDriver,
    db: &'a MetaDatabase,
    dir: &'a RelPath,
    outcome: &'a mut RebuildOutcome,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), RebuildError>> + Send + 'a>> {
    Box::pin(async move {
        let mut cursor = PageCursor::Start;
        loop {
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
                if entry.kind == EntryKind::Dir {
                    outcome.dirs += 1;
                    // Depth-first after the directory row itself, so a
                    // parent always exists before its children. The
                    // recursion goes through this boxed future (E0733:
                    // a bare recursive async fn has an unbounded size).
                    rebuild_dir(driver, db, &entry.path, outcome).await?;
                } else {
                    outcome.files += 1;
                }
            }
            match listing.next {
                Some(next) => cursor = next,
                None => return Ok(()),
            }
        }
    })
}
