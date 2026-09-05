//! RED-phase tests for the sync-lite orchestration (`sync_once`) and the
//! batch's core acceptance: **two independent instances converge through
//! one shared LWW server** (docs/plans/2026-09-04-sync-lite.md, «客户端»).
//!
//! The [`InMemorySyncServer`] mirrors the server semantics: push assigns
//! `++counter` per row and upserts, pull returns rows with
//! `version > since`. Two throwaway databases then exercise the full
//! pull → apply → diff → push cycle:
//!
//! - A seeds data (directory, multi-chunk file, single-chunk file,
//!   pending row without local bytes) and syncs;
//! - empty B syncs: it receives A's rows, but the ghost pending row is
//!   skipped (B holds no bytes for it);
//! - B changes a row and adds a new one → A receives both (remote wins);
//! - A deletes a row → the tombstone removes it on B;
//! - the pending row later completes on A → B receives it as a live row;
//! - both sides end with identical `(rel_path, row_hash)` sets, and an
//!   idle sync pushes nothing.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use cydrive_core::cache::CacheManager;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::sync::{
    namespace_key, row_hash, serialize_row, sync_once, SyncClient, SyncError, SyncOutcome,
    SyncPullResult, SyncPulledRow, SyncRowUpdate,
};

// ------------------------------------------ in-memory LWW sync server ---

#[derive(Debug, Clone)]
struct StoredRow {
    version: i64,
    deleted: bool,
    payload: String,
}

#[derive(Default)]
struct ServerInner {
    counter: i64,
    rows: HashMap<String, StoredRow>,
}

/// Family-scale stand-in for the `cydrive-sync` server: per-row monotonic
/// versions, last push wins, tombstones overwrite.
struct InMemorySyncServer {
    inner: Mutex<ServerInner>,
}

impl InMemorySyncServer {
    fn new() -> Self {
        Self {
            inner: Mutex::new(ServerInner::default()),
        }
    }

    fn stored_row_count(&self) -> usize {
        self.inner.lock().expect("server mutex").rows.len()
    }
}

#[async_trait]
impl SyncClient for InMemorySyncServer {
    async fn push(
        &self,
        _key: &str,
        _secret: Option<&str>,
        rows: &[SyncRowUpdate],
    ) -> Result<i64, SyncError> {
        let mut inner = self.inner.lock().expect("server mutex");
        for row in rows {
            inner.counter += 1;
            let version = inner.counter;
            inner.rows.insert(
                row.rel_path.clone(),
                StoredRow {
                    version,
                    deleted: row.deleted,
                    payload: row.payload.clone(),
                },
            );
        }
        Ok(inner.counter)
    }

    async fn pull(&self, _key: &str, since: i64) -> Result<SyncPullResult, SyncError> {
        let inner = self.inner.lock().expect("server mutex");
        let mut rows: Vec<SyncPulledRow> = inner
            .rows
            .iter()
            .filter(|(_, stored)| stored.version > since)
            .map(|(rel_path, stored)| SyncPulledRow {
                rel_path: rel_path.clone(),
                version: stored.version,
                deleted: stored.deleted,
                payload: stored.payload.clone(),
            })
            .collect();
        rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path).then(a.version.cmp(&b.version)));
        Ok(SyncPullResult {
            rows,
            max_version: inner.counter,
        })
    }
}

// ------------------------------------------------------------- helpers ---

struct Instance {
    _dir: tempfile::TempDir,
    db: MetaDatabase,
    cache: CacheManager,
}

fn instance(tag: &str) -> Instance {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join(format!("{tag}.db"))).expect("open db");
    let cache = CacheManager::new(dir.path().join(format!("{tag}-cache")), 64 << 20);
    Instance {
        _dir: dir,
        db,
        cache,
    }
}

fn dir_upsert(rel_path: &str, mtime: f64) -> FileUpsert {
    let name = rel_path
        .rsplit_once('/')
        .map_or(rel_path, |(_, n)| n)
        .to_string();
    FileUpsert {
        rel_path: rel_path.to_string(),
        name,
        parent_dir: "/".to_string(),
        size: 0,
        mtime,
        sha256: None,
        is_dir: true,
        telegram_msg_id: None,
        is_uploaded: true,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
    }
}

fn file_upsert(
    rel_path: &str,
    size: i64,
    mtime: f64,
    msg_id: Option<i64>,
    chunk_count: i64,
    is_uploaded: bool,
) -> FileUpsert {
    let name = rel_path
        .rsplit_once('/')
        .map_or(rel_path, |(_, n)| n)
        .to_string();
    let parent_dir = match rel_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => rel_path[..i].to_string(),
    };
    FileUpsert {
        rel_path: rel_path.to_string(),
        name,
        parent_dir,
        size,
        mtime,
        sha256: None,
        is_dir: false,
        telegram_msg_id: msg_id,
        is_uploaded,
        is_cached: false,
        is_encrypted: false,
        chunk_count,
        mime_type: Some("application/octet-stream".to_string()),
    }
}

/// `(rel_path, row_hash)` of every local live row, sorted — the
/// convergence comparison set.
fn live_row_hashes(db: &MetaDatabase) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = db
        .list_all_files()
        .expect("list_all_files")
        .iter()
        .map(|row| {
            let chunks = db.get_chunks_by_file_id(row.id).expect("chunks");
            let payload = serialize_row(row, &chunks).expect("serialize");
            (row.rel_path.clone(), row_hash(&payload))
        })
        .collect();
    out.sort();
    out
}

/// `(rel_path, row_hash)` of every mirror row — the mirror comparison set.
fn mirror_hashes(db: &MetaDatabase) -> Vec<(String, String)> {
    db.sync_mirror_all()
        .expect("sync_mirror_all")
        .into_iter()
        .map(|(rel_path, hash, _)| (rel_path, hash))
        .collect()
}

// --------------------------------------------------------------- tests ---

#[tokio::test]
async fn sync_once_on_fresh_instance_and_empty_server_is_all_zero() {
    let inst = instance("empty");
    let server = InMemorySyncServer::new();
    let outcome = sync_once(
        &inst.db,
        &inst.cache,
        &server,
        &namespace_key("bot", "42"),
        Some("family-secret"),
    )
    .await
    .expect("sync_once");
    assert_eq!(
        outcome,
        SyncOutcome {
            pulled: 0,
            applied: 0,
            skipped_ghost: 0,
            skipped_idempotent: 0,
            skipped_invalid: 0,
            tombstoned: 0,
            pushed: 0,
            pushed_tombstones: 0,
        }
    );
    assert_eq!(server.stored_row_count(), 0);
}

#[tokio::test]
async fn two_instances_converge_through_shared_server() {
    let key = namespace_key(
        "123456789:AAHfiqkKZ8W2fRzBn8Gh5jX7yLmNpQrStUvWxYz",
        "-1001234567890",
    );
    let secret = Some("family-secret");
    let server = InMemorySyncServer::new();
    let a = instance("a");
    let b = instance("b");

    // --- A seeds: directory, multi-chunk file, single-chunk file, and a
    // pending row whose local cache copy does not exist (the ghost case).
    a.db.upsert_file(&dir_upsert("/docs", 100.0))
        .expect("mkdir /docs");
    let big = file_upsert("/docs/big.bin", 1_048_586, 101.0, Some(101), 3, true);
    let big_id = a.db.upsert_file(&big).expect("insert big");
    a.db.upsert_chunk(big_id, 0, 101, 524_288, None)
        .expect("chunk 0");
    a.db.upsert_chunk(big_id, 1, 102, 524_288, None)
        .expect("chunk 1");
    a.db.upsert_chunk(big_id, 2, 103, 10, None)
        .expect("chunk 2");
    let small = file_upsert("/small.txt", 5, 102.0, Some(200), 1, true);
    let small_id = a.db.upsert_file(&small).expect("insert small");
    a.db.upsert_chunk(small_id, 0, 200, 5, None)
        .expect("small chunk");
    let pending = file_upsert("/pending.bin", 10, 103.0, None, 1, false);
    a.db.upsert_file(&pending).expect("insert pending");

    // --- A's first sync: pull is empty, everything is new → 4 pushes.
    let out = sync_once(&a.db, &a.cache, &server, &key, secret)
        .await
        .expect("sync A");
    assert_eq!(
        out,
        SyncOutcome {
            pulled: 0,
            applied: 0,
            skipped_ghost: 0,
            skipped_idempotent: 0,
            skipped_invalid: 0,
            tombstoned: 0,
            pushed: 4,
            pushed_tombstones: 0,
        }
    );
    // Deliberate semantics: max_pulled is advanced by PULL responses only.
    // The push response's max_version may already cover rows other
    // instances pushed concurrently — using it as the cursor would
    // permanently skip those rows. A's cursor stays 0 here.
    assert_eq!(
        a.db.sync_state_get().expect("A max_pulled"),
        0,
        "push must not advance the pull cursor"
    );

    // --- A's immediate re-sync: the next pull returns A's own rows
    // (idempotency-gated) and nothing is pushed.
    let out = sync_once(&a.db, &a.cache, &server, &key, secret)
        .await
        .expect("sync A again");
    assert_eq!(out.pulled, 4, "A re-pulls its own rows");
    assert_eq!(out.skipped_idempotent, 4);
    assert_eq!(out.pushed, 0, "idle re-sync pushes nothing");

    // --- Empty B syncs: receives A's three live rows, skips the ghost
    // pending row (B has no bytes for it), pushes nothing back.
    let out = sync_once(&b.db, &b.cache, &server, &key, secret)
        .await
        .expect("sync B");
    assert_eq!(
        out,
        SyncOutcome {
            pulled: 4,
            applied: 3,
            skipped_ghost: 1,
            skipped_idempotent: 0,
            skipped_invalid: 0,
            tombstoned: 0,
            pushed: 0,
            pushed_tombstones: 0,
        }
    );
    for path in ["/docs", "/docs/big.bin", "/small.txt"] {
        assert!(
            b.db.get_file(path).expect("get").is_some(),
            "B must receive A's live row {path}"
        );
    }
    let big_on_b = b.db.get_file("/docs/big.bin").expect("get").expect("row");
    let big_chunks = b.db.get_chunks_by_file_id(big_on_b.id).expect("chunks");
    assert_eq!(big_chunks.len(), 3, "B rebuilt the chunk list");
    assert!(
        b.db.get_file("/pending.bin").expect("get").is_none(),
        "ghost pending row must not materialize on B"
    );
    assert_eq!(
        b.db.sync_mirror_get("/pending.bin").expect("mirror"),
        None,
        "ghost pending row must not leave a mirror entry on B"
    );
    assert_eq!(b.db.sync_state_get().expect("B max_pulled"), 4);

    // --- B changes /small.txt (db-level) and adds a new row → pushes 2.
    let small_on_b = b.db.get_file("/small.txt").expect("get").expect("row");
    let mut changed = file_upsert(
        "/small.txt",
        small_on_b.size + 100,
        small_on_b.mtime,
        small_on_b.telegram_msg_id,
        small_on_b.chunk_count,
        small_on_b.is_uploaded,
    );
    changed.sha256 = small_on_b.sha256.clone();
    changed.mime_type = small_on_b.mime_type.clone();
    b.db.upsert_file(&changed).expect("B changes small.txt");
    let from_b = file_upsert("/from-b.txt", 7, 104.0, Some(300), 1, true);
    let from_b_id = b.db.upsert_file(&from_b).expect("B adds from-b");
    b.db.upsert_chunk(from_b_id, 0, 300, 7, None)
        .expect("from-b chunk");

    let out = sync_once(&b.db, &b.cache, &server, &key, secret)
        .await
        .expect("sync B after change");
    assert_eq!(out.pushed, 2);
    assert_eq!(out.pushed_tombstones, 0);

    // --- A syncs: receives B's change (remote wins) and the new row.
    let out = sync_once(&a.db, &a.cache, &server, &key, secret)
        .await
        .expect("sync A after B");
    assert_eq!(out.applied, 2, "A applies B's changed row and new row");
    assert_eq!(out.pushed, 0);
    assert_eq!(
        a.db.get_file("/small.txt").expect("get").expect("row").size,
        105,
        "remote wins: B's change landed on A"
    );
    let from_b_on_a = a.db.get_file("/from-b.txt").expect("get").expect("row");
    assert_eq!(
        b.db.get_chunks_by_file_id(from_b_on_a.id)
            .expect("chunks")
            .len(),
        1,
        "A rebuilt from-b's chunk list"
    );

    // --- A deletes /docs/big.bin → tombstone; B applies it.
    a.db.delete_file("/docs/big.bin")
        .expect("A deletes big.bin");
    let out = sync_once(&a.db, &a.cache, &server, &key, secret)
        .await
        .expect("sync A after delete");
    assert_eq!(out.pushed_tombstones, 1);
    assert_eq!(out.pushed, 0);

    let out = sync_once(&b.db, &b.cache, &server, &key, secret)
        .await
        .expect("sync B tombstone");
    assert_eq!(out.tombstoned, 1);
    assert!(
        b.db.get_file("/docs/big.bin").expect("get").is_none(),
        "tombstone removed the row on B"
    );
    assert_eq!(
        b.db.sync_mirror_get("/docs/big.bin").expect("mirror"),
        None,
        "tombstone removed the mirror entry on B"
    );

    // --- A's pending row completes (uploaded) → B now receives it.
    let flipped = file_upsert("/pending.bin", 10, 103.0, Some(555), 1, true);
    let flipped_id = a.db.upsert_file(&flipped).expect("A completes pending");
    a.db.upsert_chunk(flipped_id, 0, 555, 10, None)
        .expect("pending chunk");

    let out = sync_once(&a.db, &a.cache, &server, &key, secret)
        .await
        .expect("sync A after completing pending");
    // A also receives its own tombstone back (cursor was behind it): a
    // no-op delete, counted as tombstoned.
    assert_eq!(out.tombstoned, 1);
    assert_eq!(out.pushed, 1, "the completed row is a change → pushed");

    let out = sync_once(&b.db, &b.cache, &server, &key, secret)
        .await
        .expect("sync B after pending completed");
    assert_eq!(
        out.applied, 1,
        "completed row is live now — no longer a ghost"
    );
    let pending_on_b = b.db.get_file("/pending.bin").expect("get").expect("row");
    assert!(pending_on_b.is_uploaded);
    assert!(!pending_on_b.is_cached);

    // --- Convergence: identical live-row and mirror sets on both sides.
    assert_eq!(
        live_row_hashes(&a.db),
        live_row_hashes(&b.db),
        "both instances must hold the same logical rows"
    );
    assert_eq!(
        mirror_hashes(&a.db),
        mirror_hashes(&b.db),
        "both mirrors must agree after convergence"
    );

    // --- Idempotence: idle syncs push nothing.
    let out = sync_once(&a.db, &a.cache, &server, &key, secret)
        .await
        .expect("final sync A");
    assert_eq!((out.pushed, out.pushed_tombstones), (0, 0));
    let out = sync_once(&b.db, &b.cache, &server, &key, secret)
        .await
        .expect("final sync B");
    assert_eq!((out.pushed, out.pushed_tombstones), (0, 0));
}
