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

use cloudkit_storage::{Entry, EntryKind, Page, PageCursor, RelPath, StorageDriver, StorageError};

use crate::config::CyDriveConfig;
use crate::database::{DbError, FileUpsert, MetaDatabase};

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
                upsert_entry(db, entry, outcome)?;
                if entry.kind == EntryKind::Dir {
                    // Depth-first after the directory row itself, so a
                    // parent always exists before its children. The
                    // recursion goes through this boxed future (E0733:
                    // a bare recursive async fn has an unbounded size).
                    rebuild_dir(driver, db, &entry.path, outcome).await?;
                }
            }
            match listing.next {
                Some(next) => cursor = next,
                None => return Ok(()),
            }
        }
    })
}

/// Upserts one backend entry as a `files` row (see the module doc for
/// the row-shape contract).
fn upsert_entry(
    db: &MetaDatabase,
    entry: &Entry,
    outcome: &mut RebuildOutcome,
) -> Result<(), RebuildError> {
    // vocab (volume-relative, "docs/readme.md") → vpath row key
    // ("/docs/readme.md"): the files table's rel_path convention every
    // other writer uses.
    let rel_path = format!("/{}", entry.path.as_str());
    let name = entry.path.file_name().unwrap_or_default().to_string();
    let parent_dir = match entry.path.parent() {
        Some(parent) => format!("/{}", parent.as_str()),
        None => "/".to_string(),
    };
    match entry.kind {
        EntryKind::File => {
            // msg_id = the handle: fs_id-shaped handles (baidu, mock)
            // parse directly; path-shaped local handles cannot occupy
            // the i64 column and degrade to the K6 0 placeholder.
            let msg_id = entry.id.handle.as_str().parse::<i64>().unwrap_or(0);
            db.upsert_file(&FileUpsert {
                rel_path,
                name,
                parent_dir,
                size: entry.size as i64,
                mtime: entry.mtime,
                sha256: None,
                is_dir: false,
                telegram_msg_id: Some(msg_id),
                is_uploaded: true,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 1,
                mime_type: None,
            })?;
            outcome.files += 1;
        }
        EntryKind::Dir => {
            // create_dir parity: zero-sized, zero chunks, NULL msg_id,
            // born uploaded + cached.
            db.upsert_file(&FileUpsert {
                rel_path,
                name,
                parent_dir,
                size: 0,
                mtime: entry.mtime,
                sha256: None,
                is_dir: true,
                telegram_msg_id: None,
                is_uploaded: true,
                is_cached: true,
                is_encrypted: false,
                chunk_count: 0,
                mime_type: None,
            })?;
            outcome.dirs += 1;
        }
    }
    Ok(())
}
