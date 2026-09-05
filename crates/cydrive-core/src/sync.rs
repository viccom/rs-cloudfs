//! Sync-lite client engine core (2026-09-04 plan, «客户端»).
//!
//! Pure kernel of the family-scale metadata sync: logical-row
//! serialization, row hashing, the namespace key, the [`SyncClient`]
//! seam, the pull-apply state machine and the push diff. The orchestration
//! ([`sync_once`]) and the HTTP client live one layer up; this module must
//! stay free of `cydrive-sync` (wire types map onto the types here at the
//! CLI layer).
//!
//! A *logical row* is one `files` row plus its `chunks` sequence,
//! serialized as JSON (the payload). The payload is this module's private
//! format — both sides of the wire go through [`serialize_row`] /
//! [`deserialize_row`]. It carries every logical data field, but **not**
//! the local-only ones:
//!
//! - `id` — a local rowid, remapped per instance (the whole point of the
//!   chunks-inside-payload design is to bypass `file_id` remapping);
//! - `is_cached` — a local runtime flag; the peer has no cache copy, and
//!   apply always stores `false`;
//! - `created_at` / `updated_at` — DB-maintained row bookkeeping that
//!   [`MetaDatabase::upsert_file`] regenerates on every write and cannot
//!   restore from a value. If they traveled in the payload, every applied
//!   row would hash differently from its mirror forever, and both
//!   instances would re-push identical logical rows endlessly — the
//!   convergence and idle-push-0 guarantees would be unattainable.
//!
//! The user-visible file timestamp `mtime` is carried as-is (it survives
//! an apply through [`FileUpsert::mtime`], so hashes stay stable).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::CacheManager;
use crate::database::{ChunkRecord, DbError, FileRecord, FileUpsert, MetaDatabase};
use crate::rel_path::RelPath;

/// Errors of the sync engine: DB failures, payload decode failures,
/// malformed virtual paths from the server, and the transport wrapper
/// variant the CLI-layer HTTP client maps its own errors into.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// Local metadata database failure.
    #[error("sync database error: {0}")]
    Db(#[from] DbError),
    /// The payload is not a valid logical row.
    #[error("sync payload decode error: {0}")]
    Payload(#[from] serde_json::Error),
    /// A pulled row key is not a valid virtual path.
    #[error("invalid virtual path from sync: {0}")]
    InvalidPath(String),
    /// Transport-level failure of the underlying [`SyncClient`].
    #[error("sync client transport error: {0}")]
    Client(String),
}

/// One chunk of the payload's `chunks` sequence — exactly the three
/// fields needed to rebuild the row (`{index, msg_id, size}`). Chunk
/// digests are deliberately not synced: they are recomputed locally when
/// chunks are written and are not part of the logical identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadChunk {
    /// Zero-based chunk ordinal within the file.
    pub index: i64,
    /// Telegram message id holding this chunk (`null` only for rows that
    /// no Rust write path produces; apply skips such chunks because a
    /// chunk without a message id is not retrievable).
    pub msg_id: Option<i64>,
    /// Chunk size in bytes.
    pub size: i64,
}

/// The serialized logical row (module-private format, see the module
/// docs for the field-set rationale). `rel_path` is carried for hash
/// fidelity; on apply the pulled row's key is authoritative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowPayload {
    /// Canonical virtual path (unique key of the logical row).
    pub rel_path: String,
    /// Final path segment.
    pub name: String,
    /// Virtual path of the parent directory (`/` for top level).
    pub parent_dir: String,
    /// Size in bytes; `0` for directories.
    pub size: i64,
    /// Modification time as fractional seconds since the Unix epoch.
    pub mtime: f64,
    /// SHA-256 hex digest when computed (files at or below 100 MB).
    pub sha256: Option<String>,
    /// Directories are plain `files` rows and sync like any other row.
    pub is_dir: bool,
    /// Telegram message id of chunk 0 once uploaded.
    pub telegram_msg_id: Option<i64>,
    /// Whether all chunks have been pushed to Telegram.
    pub is_uploaded: bool,
    /// Whether the payload is client-side encrypted.
    pub is_encrypted: bool,
    /// Number of `.partNNN` chunks.
    pub chunk_count: i64,
    /// MIME type, when known.
    pub mime_type: Option<String>,
    /// The file's chunk sequence, ordered by `index` (empty for
    /// directories and not-yet-uploaded rows).
    pub chunks: Vec<PayloadChunk>,
}

impl RowPayload {
    /// Builds the payload view of a `files` row and its chunks. Chunks
    /// are taken in the caller's order — [`MetaDatabase::get_chunks_by_file_id`]
    /// already orders by `chunk_index ASC`.
    fn from_parts(file: &FileRecord, chunks: &[ChunkRecord]) -> Self {
        Self {
            rel_path: file.rel_path.clone(),
            name: file.name.clone(),
            parent_dir: file.parent_dir.clone(),
            size: file.size,
            mtime: file.mtime,
            sha256: file.sha256.clone(),
            is_dir: file.is_dir,
            telegram_msg_id: file.telegram_msg_id,
            is_uploaded: file.is_uploaded,
            is_encrypted: file.is_encrypted,
            chunk_count: file.chunk_count,
            mime_type: file.mime_type.clone(),
            chunks: chunks
                .iter()
                .map(|c| PayloadChunk {
                    index: c.chunk_index,
                    msg_id: c.telegram_msg_id,
                    size: c.size,
                })
                .collect(),
        }
    }
}

/// Serializes one logical row (`files` + its `chunks`) into the canonical
/// payload JSON string. Deterministic: struct field order is fixed.
pub fn serialize_row(file: &FileRecord, chunks: &[ChunkRecord]) -> Result<String, SyncError> {
    Ok(serde_json::to_string(&RowPayload::from_parts(
        file, chunks,
    ))?)
}

/// Parses a payload produced by [`serialize_row`].
pub fn deserialize_row(payload: &str) -> Result<RowPayload, SyncError> {
    Ok(serde_json::from_str(payload)?)
}

/// Content hash of a payload: lowercase hex SHA-256.
pub fn row_hash(payload: &str) -> String {
    let digest = Sha256::digest(payload.as_bytes());
    hex_lower(&digest)
}

/// Namespace key of a drive: `hex(SHA-256("{bot_token}:{chat_id}"))`.
/// The server never stores the bare token; same bot + chat = same drive,
/// different member bots = different drives (both topologies, one
/// mechanism).
pub fn namespace_key(bot_token: &str, chat_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bot_token.as_bytes());
    hasher.update(b":");
    hasher.update(chat_id.as_bytes());
    hex_lower(&hasher.finalize())
}

/// Lowercase hex of a digest (mirror of the `chunker` helper).
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// One row the client wants pushed to the server. A tombstone is
/// `deleted: true` with an empty payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRowUpdate {
    /// Canonical virtual path (the row key).
    pub rel_path: String,
    /// `true` = deletion (tombstone).
    pub deleted: bool,
    /// Canonical payload JSON (empty for tombstones).
    pub payload: String,
}

/// One row received from the server, at its LWW version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPulledRow {
    /// Canonical virtual path (the row key).
    pub rel_path: String,
    /// Server-assigned monotonic version of this row state.
    pub version: i64,
    /// `true` = deletion (tombstone).
    pub deleted: bool,
    /// Canonical payload JSON (empty for tombstones).
    pub payload: String,
}

/// A pull response: the rows with `version > since` plus the highest
/// version the server has assigned.
#[derive(Debug, Clone, Default)]
pub struct SyncPullResult {
    /// Rows newer than the request's `since`.
    pub rows: Vec<SyncPulledRow>,
    /// The server's current maximum version.
    pub max_version: i64,
}

/// Counters of one full [`sync_once`] pass, for CLI display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Rows in the pull response.
    pub pulled: usize,
    /// Live rows that moved local state (mirror-only updates included).
    pub applied: usize,
    /// Live pending rows skipped: no local cache copy (ghost).
    pub skipped_ghost: usize,
    /// Rows skipped by the version idempotency gate.
    pub skipped_idempotent: usize,
    /// Rows skipped for an undecodable payload or an invalid row key
    /// (counted and logged, never fatal — see [`apply_pulled_rows`]).
    pub skipped_invalid: usize,
    /// Tombstones applied (local row and mirror deleted).
    pub tombstoned: usize,
    /// Live rows pushed to the server.
    pub pushed: usize,
    /// Tombstones pushed to the server.
    pub pushed_tombstones: usize,
}

/// The server seam of the sync engine. The CLI layer provides the HTTP
/// implementation over the `cydrive-sync` wire protocol; core must not
/// depend on that crate, so both sides speak the types above and the CLI
/// maps them.
#[async_trait]
pub trait SyncClient: Send + Sync {
    /// Pushes `rows` into the namespace `key` (registering the namespace
    /// if needed; `secret` is the optional family-level gate). Returns
    /// the server's maximum version after the batch — every pushed row
    /// carries a version at or below it.
    async fn push(
        &self,
        key: &str,
        secret: Option<&str>,
        rows: &[SyncRowUpdate],
    ) -> Result<i64, SyncError>;

    /// Returns every row of the namespace with `version > since`, plus
    /// the server's current maximum version. `secret` is the optional
    /// family-level gate — servers configured with a shared secret
    /// reject a pull that does not carry it, so it must ride along on
    /// the pull exactly like on the push.
    async fn pull(
        &self,
        key: &str,
        secret: Option<&str>,
        since: i64,
    ) -> Result<SyncPullResult, SyncError>;
}

/// Counters of a pull-apply pass (see [`apply_pulled_rows`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Rows in the pull response.
    pub pulled: usize,
    /// Live rows that moved local state (mirror-only updates included).
    pub applied: usize,
    /// Live pending rows skipped: no local cache copy (ghost).
    pub skipped_ghost: usize,
    /// Rows skipped by the version idempotency gate.
    pub skipped_idempotent: usize,
    /// Rows skipped for an undecodable payload or an invalid row key
    /// (counted and logged, never fatal — see [`apply_pulled_rows`]).
    pub skipped_invalid: usize,
    /// Tombstones applied (local row and mirror deleted).
    pub tombstoned: usize,
}

/// Computes the push list: every local row whose payload hash differs
/// from its mirror (or has no mirror) is pushed as a live row; every
/// mirrored path with no local row is pushed as a tombstone. Output is
/// sorted by `rel_path` for determinism.
///
/// `local_rows` are `(rel_path, payload)` pairs — the caller serializes
/// the full `files` table (directories included, they are plain rows);
/// `mirror` is the `sync_mirror_all()` triple shape. Both sides are
/// joined through hash indexes (`sync_mirror` rows are unique by
/// `rel_path`, so a path-keyed map is equivalent to the old per-row
/// scan) instead of the original nested `any` loops — with 10k+ row
/// libraries those O(local × mirror) scans dominated every sync pass
/// with pure CPU (review follow-up BUG⑤); the semantics are unchanged.
pub fn push_diff(
    local_rows: &[(String, String)],
    mirror: &[(String, String, i64)],
) -> Vec<SyncRowUpdate> {
    // Mirror hash by path; the server version never enters the diff.
    let mirror_hash_by_path: std::collections::HashMap<&str, &str> = mirror
        .iter()
        .map(|(path, hash, _version)| (path.as_str(), hash.as_str()))
        .collect();
    let mut updates = Vec::new();
    for (rel_path, payload) in local_rows {
        let hash = row_hash(payload);
        let unchanged = mirror_hash_by_path
            .get(rel_path.as_str())
            .is_some_and(|m_hash| *m_hash == hash);
        if !unchanged {
            updates.push(SyncRowUpdate {
                rel_path: rel_path.clone(),
                deleted: false,
                payload: payload.clone(),
            });
        }
    }
    // Tombstones: mirrored paths with no local row (local paths into a
    // set for O(1) membership). Deterministic output order is preserved
    // by the whole-vec `rel_path` sort below, exactly as before.
    let local_paths: std::collections::HashSet<&str> =
        local_rows.iter().map(|(path, _)| path.as_str()).collect();
    for (m_path, _, _) in mirror {
        if !local_paths.contains(m_path.as_str()) {
            updates.push(SyncRowUpdate {
                rel_path: m_path.clone(),
                deleted: true,
                payload: String::new(),
            });
        }
    }
    updates.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    updates
}

/// Applies a pull response to the local database, row by row:
///
/// 1. `version <= mirror.server_version` (no mirror reads as 0) → skip
///    (idempotency gate — re-pulled own rows and stale duplicates die
///    here);
/// 2. an invalid row key (`RelPath` rejects it) or — for live rows — an
///    undecodable payload → count `skipped_invalid`, `warn!`, `continue`.
///    A poisoned row must never abort the whole pass: the old `?`
///    returned before the cursor write below and permanently wedged the
///    namespace (no later update could ever arrive past it);
/// 3. tombstone → delete the local `files` row (chunks go with it) and
///    the mirror row (missing local state is fine); the local cache copy
///    is removed too — keyed on DISK presence, the same basis as the
///    hydrate hit probe (which never consults the row), so a surviving
///    copy of a deleted file would be served again after a same-path
///    recreate. One carve-out: an in-flight upload (local row still
///    pending AND its copy on disk) keeps its source file — the upload
///    worker's success write-back revives the row («later action wins»
///    LWW, decisions 2026-09-05);
/// 4. ghost-pending: live row with `is_uploaded == false` and no local
///    cache copy → skip the whole row (no `files` row, no mirror row — a
///    pending row without bytes is a ghost on this machine);
/// 5. local hash == payload hash → update the mirror only (no `files`
///    write; saves write amplification);
/// 6. otherwise remote wins: replace the row (is_cached forced `false`)
///    and its chunks; the stale cache copy is removed when one exists on
///    disk and the old row is not an in-flight pending upload (the ghost
///    gate guarantees a pulled pending row that gets this far has its
///    copy on disk — that copy is the upload's source and must survive,
///    same «later action wins» carve-out as tombstones; any other copy
///    under changed content is stale bytes a later hydrate hit would
///    serve — a correctness hazard); removal is best-effort; write the
///    mirror.
///
/// After all rows: `max_pulled = max(old, max_version)` — monotonic, so a
/// rebuilt server database can never move the cursor backwards.
pub fn apply_pulled_rows(
    db: &MetaDatabase,
    cache: &CacheManager,
    rows: &[SyncPulledRow],
    max_version: i64,
) -> Result<ApplyOutcome, SyncError> {
    let mut outcome = ApplyOutcome {
        pulled: rows.len(),
        ..ApplyOutcome::default()
    };
    for row in rows {
        let mirror_version = db
            .sync_mirror_get(&row.rel_path)?
            .map(|(_, version)| version)
            .unwrap_or(0);
        if row.version <= mirror_version {
            outcome.skipped_idempotent += 1;
            continue;
        }
        // The key check covers live rows and tombstones alike (the
        // tombstone's cache cleanup below needs a valid path too); a bad
        // key is a poisoned row — count, log, skip past it.
        let rel = match RelPath::new(&row.rel_path) {
            Ok(rel) => rel,
            Err(_) => {
                let err = SyncError::InvalidPath(row.rel_path.clone());
                tracing::warn!(
                    rel_path = %row.rel_path,
                    error = %err,
                    "sync pull row key is not a valid virtual path — skipping"
                );
                outcome.skipped_invalid += 1;
                continue;
            }
        };
        if row.deleted {
            // In-flight protection («later action wins», decisions
            // 2026-09-05): read the local row BEFORE deleting it — a
            // pending row whose cache copy still sits on disk is an
            // upload in progress, and its source file must survive so
            // the upload worker's success write-back can revive the row.
            let local = db.get_file(&row.rel_path)?;
            let inflight_upload = local.as_ref().is_some_and(|rec| !rec.is_uploaded)
                && cache.local_path(&rel).exists();
            // delete_file removes the chunks in the same transaction and
            // is a no-op for a missing path; same for the mirror delete.
            db.delete_file(&row.rel_path)?;
            db.sync_mirror_delete(&row.rel_path)?;
            // Disk-based cleanup (hydrate's hit probe never consults the
            // row): every non-in-flight tombstone drops the local copy
            // too. Best-effort — directory rows and already-missing
            // files are natural no-ops.
            if !inflight_upload {
                let _ = std::fs::remove_file(cache.local_path(&rel));
            }
            outcome.tombstoned += 1;
            continue;
        }

        let payload = match deserialize_row(&row.payload) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!(
                    rel_path = %row.rel_path,
                    error = %err,
                    "sync pull row payload is undecodable — skipping"
                );
                outcome.skipped_invalid += 1;
                continue;
            }
        };
        if !payload.is_uploaded && !cache.local_path(&rel).exists() {
            outcome.skipped_ghost += 1;
            continue;
        }

        let local = db.get_file(&row.rel_path)?;
        let payload_hash = row_hash(&row.payload);
        let local_hash = match &local {
            Some(rec) => {
                let chunks = db.get_chunks_by_file_id(rec.id)?;
                Some(row_hash(&serialize_row(rec, &chunks)?))
            }
            None => None,
        };
        if local_hash.as_deref() == Some(payload_hash.as_str()) {
            db.sync_mirror_set(&row.rel_path, &payload_hash, row.version)?;
            outcome.applied += 1;
            continue;
        }

        // Remote wins. Cache cleanup keys on DISK presence — the same
        // basis as the hydrate hit probe — with the in-flight carve-out:
        // a local pending upload (the ghost gate guarantees its copy is
        // on disk) keeps its source file so the worker's success
        // write-back can revive the row («later action wins»). Any other
        // on-disk copy under changed content is stale bytes and must not
        // survive; removal is best-effort.
        let inflight_upload = local.as_ref().is_some_and(|rec| !rec.is_uploaded);
        if !inflight_upload && cache.local_path(&rel).exists() {
            let _ = std::fs::remove_file(cache.local_path(&rel));
        }
        replace_row(db, &row.rel_path, &payload)?;
        db.sync_mirror_set(&row.rel_path, &payload_hash, row.version)?;
        outcome.applied += 1;
    }

    let previous_max = db.sync_state_get()?;
    if max_version > previous_max {
        db.sync_state_set(max_version)?;
    }
    Ok(outcome)
}

/// Replaces the local logical row at `rel_path` with the payload's state:
/// delete (files row + chunks; no-op when absent), re-insert with
/// `is_cached = false`, then rebuild the chunk list. The delete+insert
/// pair is required because `MetaDatabase` offers no chunks-only delete,
/// and the fresh insert also means the upsert's `coalesce` clauses have
/// no stale values to keep — a true full replace.
fn replace_row(db: &MetaDatabase, rel_path: &str, payload: &RowPayload) -> Result<(), SyncError> {
    db.delete_file(rel_path)?;
    let file_id = db.upsert_file(&FileUpsert {
        rel_path: rel_path.to_string(),
        name: payload.name.clone(),
        parent_dir: payload.parent_dir.clone(),
        size: payload.size,
        mtime: payload.mtime,
        sha256: payload.sha256.clone(),
        is_dir: payload.is_dir,
        telegram_msg_id: payload.telegram_msg_id,
        is_uploaded: payload.is_uploaded,
        is_cached: false,
        is_encrypted: payload.is_encrypted,
        chunk_count: payload.chunk_count,
        mime_type: payload.mime_type.clone(),
    })?;
    for chunk in &payload.chunks {
        if let Some(msg_id) = chunk.msg_id {
            db.upsert_chunk(file_id, chunk.index, msg_id, chunk.size, None)?;
        }
    }
    Ok(())
}

/// One full sync pass: pull → apply → diff → push.
///
/// - **Pull** with `since = max_pulled` and the same `secret` the push
///   carries (a secret-gated server rejects a secretless pull with 403
///   before anything else happens), apply every row through
///   [`apply_pulled_rows`] (which also advances `max_pulled` by the pull
///   response's `max_version`, monotonically).
/// - **Diff** the local table (directories included — they are plain
///   `files` rows) against `sync_mirror` via [`push_diff`].
/// - **Push** the updates (skipped entirely when the diff is empty), then
///   reconcile the mirror: live rows get `(payload hash, push max_version)`
///   — the batch's atomicity makes the whole-batch maximum safe, any
///   later change necessarily carries a version above it — and tombstone
///   paths get their mirror row *deleted* (a surviving mirror would make
///   the next diff push the tombstone again, forever).
///
/// `max_pulled` is advanced by **pull responses only**. The plan sketch
/// said «push → 更新 max_pulled»; this is a deliberate correction: the
/// push response's `max_version` may already cover rows other instances
/// pushed concurrently, so adopting it as the pull cursor would
/// permanently skip those rows. The cost of the stricter rule is one
/// extra pull of our own just-pushed rows, which die at the
/// idempotency gate — harmless.
pub async fn sync_once(
    db: &MetaDatabase,
    cache: &CacheManager,
    client: &dyn SyncClient,
    key: &str,
    secret: Option<&str>,
) -> Result<SyncOutcome, SyncError> {
    let since = db.sync_state_get()?;
    let pull = client.pull(key, secret, since).await?;
    let apply = apply_pulled_rows(db, cache, &pull.rows, pull.max_version)?;

    let mut local_rows: Vec<(String, String)> = Vec::new();
    for row in db.list_all_files()? {
        let chunks = db.get_chunks_by_file_id(row.id)?;
        local_rows.push((row.rel_path.clone(), serialize_row(&row, &chunks)?));
    }
    let mirror = db.sync_mirror_all()?;
    let updates = push_diff(&local_rows, &mirror);

    let mut outcome = SyncOutcome {
        pulled: apply.pulled,
        applied: apply.applied,
        skipped_ghost: apply.skipped_ghost,
        skipped_idempotent: apply.skipped_idempotent,
        skipped_invalid: apply.skipped_invalid,
        tombstoned: apply.tombstoned,
        pushed: 0,
        pushed_tombstones: 0,
    };
    if updates.is_empty() {
        return Ok(outcome);
    }

    let push_max = client.push(key, secret, &updates).await?;
    for update in &updates {
        if update.deleted {
            db.sync_mirror_delete(&update.rel_path)?;
            outcome.pushed_tombstones += 1;
        } else {
            db.sync_mirror_set(&update.rel_path, &row_hash(&update.payload), push_max)?;
            outcome.pushed += 1;
        }
    }
    Ok(outcome)
}
