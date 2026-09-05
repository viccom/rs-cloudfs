//! RED-phase tests for the sync-lite pure engine of `cydrive_core::sync`
//! (docs/plans/2026-09-04-sync-lite.md, «客户端», core kernel unit).
//!
//! Contract under test:
//!
//! - **Logical row payload**: one `files` row serialized as JSON together
//!   with its `chunks` sequence `[{index, msg_id, size}, ...]`. The payload
//!   is this module's private format (both directions go through the same
//!   functions). Payload carries every *logical* data field — but **not**
//!   the local-only ones: `id` (local rowid, remapped per instance),
//!   `is_cached` (the peer has no local cache copy; apply forces `false`)
//!   and `created_at` / `updated_at` (DB-maintained row bookkeeping that
//!   [`MetaDatabase::upsert_file`] cannot restore — see
//!   `serialize_row_omits_local_only_fields` for why carrying them would
//!   break convergence). `mtime` — the user-visible file timestamp — is
//!   carried as-is.
//! - `row_hash`: SHA-256 hex of the payload (golden-vector pinned).
//! - `namespace_key`: `hex(SHA-256("{token}:{chat_id}"))` (golden-vector
//!   pinned, precomputed with an independent `python hashlib` run).
//! - `push_diff`: no mirror → push as new; hash differs → push as changed;
//!   mirror-only path → push as tombstone (`deleted: true`, empty payload).
//! - `apply_pulled_rows`: per-row version idempotency gate, tombstone
//!   delete (row + mirror + local cache copy; an in-flight pending
//!   upload keeps its source file — "later action wins" LWW, decisions
//!   2026-09-05), ghost-pending skip (no local cache copy), hash-equal
//!   updates the mirror only, remote-wins overwrite replaces the row +
//!   chunks and clears a stale cached copy **keyed on disk presence**
//!   (hydrate's hit probe is disk-based; the row flag can drift);
//!   undecodable payloads and invalid row keys are counted
//!   `skipped_invalid` and skipped without wedging the cursor;
//!   `max_pulled` advances monotonically.

use cydrive_core::cache::CacheManager;
use cydrive_core::database::{ChunkRecord, FileRecord, FileUpsert, MetaDatabase};
use cydrive_core::rel_path::RelPath;
use cydrive_core::sync::{
    apply_pulled_rows, deserialize_row, namespace_key, push_diff, row_hash, serialize_row,
    ApplyOutcome, PayloadChunk, RowPayload, SyncPulledRow, SyncRowUpdate,
};

// ------------------------------------------------------------- helpers ---

fn fresh_env(tag: &str) -> (tempfile::TempDir, MetaDatabase, CacheManager) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join(format!("{tag}.db"))).expect("open db");
    let cache = CacheManager::new(dir.path().join(format!("{tag}-cache")), 64 << 20);
    (dir, db, cache)
}

/// A fully populated `files` row for `rel_path` (caller mutates fields as
/// needed). Timestamps are fixed constants so hashes are reproducible.
fn record(rel_path: &str, size: i64, is_uploaded: bool) -> FileRecord {
    let name = rel_path
        .rsplit_once('/')
        .map_or(rel_path, |(_, n)| n)
        .to_string();
    let parent_dir = match rel_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => rel_path[..i].to_string(),
    };
    FileRecord {
        id: 1,
        rel_path: rel_path.to_string(),
        name,
        parent_dir,
        size,
        mtime: 1_759_971_600.5,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(900),
        is_uploaded,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: Some("application/octet-stream".to_string()),
        created_at: Some(1_759_971_500.25),
        updated_at: Some(1_759_971_600.75),
    }
}

fn chunk(index: i64, msg_id: i64, size: i64) -> ChunkRecord {
    ChunkRecord {
        chunk_index: index,
        telegram_msg_id: Some(msg_id),
        size,
        // Chunk digests are deliberately NOT part of the payload contract
        // (only index/msg_id/size travel) — set here to prove they are
        // dropped on the wire.
        sha256: Some("deadbeef".to_string()),
    }
}

fn upsert_from(rec: &FileRecord, is_cached: bool) -> FileUpsert {
    FileUpsert {
        rel_path: rec.rel_path.clone(),
        name: rec.name.clone(),
        parent_dir: rec.parent_dir.clone(),
        size: rec.size,
        mtime: rec.mtime,
        sha256: rec.sha256.clone(),
        is_dir: rec.is_dir,
        telegram_msg_id: rec.telegram_msg_id,
        is_uploaded: rec.is_uploaded,
        is_cached,
        is_encrypted: rec.is_encrypted,
        chunk_count: rec.chunk_count,
        mime_type: rec.mime_type.clone(),
    }
}

fn insert(db: &MetaDatabase, entry: &FileUpsert, chunks: &[ChunkRecord]) -> i64 {
    let id = db.upsert_file(entry).expect("upsert_file");
    for c in chunks {
        db.upsert_chunk(
            id,
            c.chunk_index,
            c.telegram_msg_id.expect("chunk msg id"),
            c.size,
            c.sha256.as_deref(),
        )
        .expect("upsert_chunk");
    }
    id
}

fn pulled(rel_path: &str, version: i64, deleted: bool, payload: &str) -> SyncPulledRow {
    SyncPulledRow {
        rel_path: rel_path.to_string(),
        version,
        deleted,
        payload: payload.to_string(),
    }
}

fn write_cache_copy(cache: &CacheManager, rel: &str, bytes: &[u8]) -> std::path::PathBuf {
    let rel = RelPath::new(rel).expect("valid rel path");
    let path = cache.local_path(&rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create cache parent dir");
    }
    std::fs::write(&path, bytes).expect("write cache copy");
    path
}

fn serialize(rec: &FileRecord, chunks: &[ChunkRecord]) -> String {
    serialize_row(rec, chunks).expect("serialize_row")
}

// -------------------------------------------------------- serialization ---

#[test]
fn serialize_row_carries_all_data_fields_and_roundtrips() {
    let mut rec = record("/docs/big.bin", 1_048_576, true);
    rec.chunk_count = 3;
    let chunks = vec![
        chunk(0, 101, 524_288),
        chunk(1, 102, 524_288),
        chunk(2, 103, 10),
    ];
    let payload = serialize(&rec, &chunks);

    let parsed = deserialize_row(&payload).expect("payload must roundtrip");
    assert_eq!(
        parsed,
        RowPayload {
            rel_path: "/docs/big.bin".to_string(),
            name: "big.bin".to_string(),
            parent_dir: "/docs".to_string(),
            size: 1_048_576,
            mtime: rec.mtime,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(900),
            is_uploaded: true,
            is_encrypted: false,
            chunk_count: 3,
            mime_type: Some("application/octet-stream".to_string()),
            chunks: vec![
                PayloadChunk {
                    index: 0,
                    msg_id: Some(101),
                    size: 524_288,
                },
                PayloadChunk {
                    index: 1,
                    msg_id: Some(102),
                    size: 524_288,
                },
                PayloadChunk {
                    index: 2,
                    msg_id: Some(103),
                    size: 10,
                },
            ],
        },
        "every data field (and only the {{index,msg_id,size}} chunk triple) survives the roundtrip"
    );
}

#[test]
fn serialize_row_omits_local_only_fields() {
    // `is_cached` is a local runtime flag (the peer has no cache copy —
    // apply forces `false`). `id` is a local rowid remapped per instance.
    // `created_at` / `updated_at` are DB-maintained bookkeeping that
    // `upsert_file` cannot restore: if they traveled in the payload, every
    // apply would rewrite them with local values, the recomputed local
    // hash would never equal the mirror hash again, and both instances
    // would re-push the same logical row forever (push ping-pong) — the
    // convergence and idle-push-0 guarantees would be impossible. The
    // user-visible file timestamp `mtime` IS carried as-is.
    let mut rec = record("/a.txt", 5, true);
    rec.is_cached = true;
    rec.created_at = Some(1.0);
    rec.updated_at = Some(2.0);
    let payload = serialize(&rec, &[chunk(0, 7, 5)]);
    for key in [
        "\"is_cached\"",
        "\"created_at\"",
        "\"updated_at\"",
        "\"id\"",
    ] {
        assert!(
            !payload.contains(key),
            "payload must not carry local-only field {key}: {payload}"
        );
    }
}

#[test]
fn serialize_row_chunk_objects_carry_exactly_three_keys() {
    let rec = record("/a.txt", 5, true);
    let payload = serialize(&rec, &[chunk(0, 7, 5)]);
    let value: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
    let chunks = value["chunks"].as_array().expect("chunks array");
    assert_eq!(chunks.len(), 1);
    let mut keys: Vec<&str> = chunks[0]
        .as_object()
        .expect("chunk object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["index", "msg_id", "size"]);
}

#[test]
fn serialize_row_is_deterministic() {
    let rec = record("/det.txt", 9, true);
    let chunks = vec![chunk(0, 1, 9)];
    assert_eq!(serialize(&rec, &chunks), serialize(&rec, &chunks));
}

#[test]
fn serialize_row_directory_row_has_empty_chunks() {
    let mut rec = record("/docs", 0, true);
    rec.is_dir = true;
    rec.chunk_count = 0;
    rec.sha256 = None;
    rec.telegram_msg_id = None;
    rec.mime_type = None;
    let payload = serialize(&rec, &[]);
    assert!(
        payload.contains("\"chunks\":[]"),
        "directory rows serialize with an empty chunk list: {payload}"
    );
    let parsed = deserialize_row(&payload).expect("decode");
    assert!(parsed.chunks.is_empty());
    assert!(parsed.is_dir);
}

// ----------------------------------------------------------- row_hash ---

#[test]
fn row_hash_is_lowercase_hex_sha256_golden_vector() {
    // Precomputed independently: python -c 'import hashlib;
    // print(hashlib.sha256(b"cydrive sync golden vector").hexdigest())'
    assert_eq!(
        row_hash("cydrive sync golden vector"),
        "321b50f29c04dc5872fbadc82a6653d9c14450eb9736432b81d1314e18941814"
    );
    // SHA-256 of the empty string pins the empty-input edge.
    assert_eq!(
        row_hash(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(row_hash("").len(), 64);
}

#[test]
fn row_hash_is_stable_across_equal_payloads() {
    let rec = record("/same.txt", 3, true);
    let payload = serialize(&rec, &[]);
    assert_eq!(row_hash(&payload), row_hash(&payload));
}

// ------------------------------------------------------ namespace_key ---

#[test]
fn namespace_key_golden_vectors() {
    // Precomputed independently: python -c 'import hashlib;
    // print(hashlib.sha256(b"bot:42").hexdigest())' and the same for the
    // full "{token}:{chat_id}" concatenation below (the token itself
    // contains a colon — the concatenation is still "{token}:{chat_id}").
    assert_eq!(
        namespace_key("bot", "42"),
        "0c0aa9463ef5ee35cc31f32f8d7f59fbd715a934c5afb946ae354f7e4092ead9"
    );
    assert_eq!(
        namespace_key(
            "123456789:AAHfiqkKZ8W2fRzBn8Gh5jX7yLmNpQrStUvWxYz",
            "-1001234567890"
        ),
        "1de41cf8423e647d699188a5d2133fc5a235cc6cd24153e6c7b075a804e78052"
    );
}

#[test]
fn namespace_key_differs_for_different_inputs() {
    assert_ne!(namespace_key("bot-a", "1"), namespace_key("bot-b", "1"));
    assert_ne!(namespace_key("bot", "1"), namespace_key("bot", "2"));
}

// ----------------------------------------------------------- push_diff ---

#[test]
fn push_diff_sends_rows_without_mirror_as_new() {
    let local = vec![
        ("/a.txt".to_string(), "payload-a".to_string()),
        ("/b/nested.txt".to_string(), "payload-b".to_string()),
    ];
    let diff = push_diff(&local, &[]);
    assert_eq!(
        diff,
        vec![
            SyncRowUpdate {
                rel_path: "/a.txt".to_string(),
                deleted: false,
                payload: "payload-a".to_string(),
            },
            SyncRowUpdate {
                rel_path: "/b/nested.txt".to_string(),
                deleted: false,
                payload: "payload-b".to_string(),
            },
        ],
        "rows without a mirror entry must be pushed (new), sorted by rel_path"
    );
}

#[test]
fn push_diff_sends_only_rows_whose_hash_changed() {
    let unchanged_payload = "payload-a".to_string();
    let changed_payload = "payload-b-v2".to_string();
    let local = vec![
        ("/a.txt".to_string(), unchanged_payload.clone()),
        ("/b.txt".to_string(), changed_payload.clone()),
        ("/c.txt".to_string(), "payload-c".to_string()),
    ];
    let mirror = vec![("/a.txt".to_string(), row_hash(&unchanged_payload), 7)];
    let diff = push_diff(&local, &mirror);
    assert_eq!(
        diff,
        vec![
            SyncRowUpdate {
                rel_path: "/b.txt".to_string(),
                deleted: false,
                payload: changed_payload,
            },
            SyncRowUpdate {
                rel_path: "/c.txt".to_string(),
                deleted: false,
                payload: "payload-c".to_string(),
            },
        ],
        "hash-equal rows are skipped; changed and new rows are pushed"
    );
}

#[test]
fn push_diff_emits_tombstones_for_mirror_only_paths() {
    let local = vec![("/keep.txt".to_string(), "payload-keep".to_string())];
    let mirror = vec![
        ("/keep.txt".to_string(), row_hash("payload-keep"), 3),
        ("/gone.txt".to_string(), row_hash("payload-gone"), 4),
    ];
    let diff = push_diff(&local, &mirror);
    assert_eq!(
        diff,
        vec![SyncRowUpdate {
            rel_path: "/gone.txt".to_string(),
            deleted: true,
            payload: String::new(),
        }],
        "a mirrored path with no local row must be pushed as a tombstone with an empty payload"
    );
}

// ----------------------------------------------------- apply_pulled_rows ---

#[test]
fn apply_skips_rows_at_or_below_mirror_version() {
    let (_dir, db, cache) = fresh_env("gate");
    let rec = record("/gate.txt", 10, true);
    let id = insert(&db, &upsert_from(&rec, false), &[chunk(0, 5, 10)]);
    db.sync_mirror_set("/gate.txt", "stale-hash", 5)
        .expect("mirror set");

    // Row version == mirror server_version -> skipped by the idempotency
    // gate (a fresh row with no mirror reads as version 0, so a version-0
    // pull is skipped too).
    let rows = vec![
        pulled("/gate.txt", 5, false, &serialize(&rec, &[])),
        pulled("/never-mirrored.txt", 0, false, "anything"),
    ];
    let outcome = apply_pulled_rows(&db, &cache, &rows, 5).expect("apply");
    assert_eq!(
        outcome,
        ApplyOutcome {
            pulled: 2,
            applied: 0,
            skipped_ghost: 0,
            skipped_idempotent: 2,
            skipped_invalid: 0,
            tombstoned: 0,
        }
    );

    // Neither the files row nor the mirror moved.
    assert_eq!(db.get_file("/gate.txt").expect("get").expect("row").id, id);
    assert_eq!(
        db.sync_mirror_get("/gate.txt").expect("mirror"),
        Some(("stale-hash".to_string(), 5))
    );
    assert!(
        db.get_file("/never-mirrored.txt").expect("get").is_none(),
        "a gated row must not create local state"
    );
}

#[test]
fn apply_tombstone_deletes_files_chunks_and_mirror() {
    let (dir, db, cache) = fresh_env("tomb");
    let rec = record("/tomb.txt", 10, true);
    let chunks = vec![chunk(0, 5, 6), chunk(1, 6, 4)];
    insert(&db, &upsert_from(&rec, false), &chunks);
    db.sync_mirror_set("/tomb.txt", "hash", 1)
        .expect("mirror set");

    let rows = vec![pulled("/tomb.txt", 2, true, "")];
    let outcome = apply_pulled_rows(&db, &cache, &rows, 2).expect("apply");
    assert_eq!(
        outcome,
        ApplyOutcome {
            pulled: 1,
            applied: 0,
            skipped_ghost: 0,
            skipped_idempotent: 0,
            skipped_invalid: 0,
            tombstoned: 1,
        }
    );

    assert!(db.get_file("/tomb.txt").expect("get").is_none());
    assert_eq!(db.sync_mirror_get("/tomb.txt").expect("mirror"), None);
    let remaining: i64 = {
        let conn = rusqlite::Connection::open(dir.path().join("tomb.db")).expect("conn");
        conn.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
            .expect("count chunks")
    };
    assert_eq!(remaining, 0, "chunk rows must be deleted with the file row");
}

#[test]
fn tombstone_apply_removes_local_cache_copy() {
    // High① fix pin (decisions.md 2026-09-05): an uploaded row whose
    // bytes sit in the local cache. The tombstone must remove the row,
    // the mirror AND the cache copy — hydrate's hit probe is disk-based
    // (it never consults the row), so a surviving copy of a deleted file
    // would be served again after a same-path recreate.
    let (_dir, db, cache) = fresh_env("tomb-cache");
    let rec = record("/gone.bin", 10, true);
    insert(&db, &upsert_from(&rec, true), &[chunk(0, 5, 10)]);
    db.sync_mirror_set("/gone.bin", "hash", 1)
        .expect("mirror set");
    let cache_path = write_cache_copy(&cache, "/gone.bin", b"stale bytes");

    let rows = vec![pulled("/gone.bin", 2, true, "")];
    let outcome = apply_pulled_rows(&db, &cache, &rows, 2).expect("apply");
    assert_eq!(outcome.tombstoned, 1);
    assert!(db.get_file("/gone.bin").expect("get").is_none());
    assert_eq!(db.sync_mirror_get("/gone.bin").expect("mirror"), None);
    assert!(
        !cache_path.exists(),
        "the local cache copy must not survive a tombstone — hydrate's disk-based hit probe would serve the deleted bytes"
    );
}

#[test]
fn tombstone_preserves_inflight_pending_upload_source() {
    // In-flight upload protection (decisions 2026-09-05, "later action
    // wins" LWW): a local PENDING row whose cache copy still sits on
    // disk is an upload in progress. The tombstone removes the db row
    // and mirror but must keep the source file — the upload worker's
    // success write-back revives the row.
    let (_dir, db, cache) = fresh_env("tomb-inflight");
    let pending = record("/inflight.bin", 10, false);
    insert(&db, &upsert_from(&pending, false), &[chunk(0, 5, 10)]);
    db.sync_mirror_set("/inflight.bin", "hash", 1)
        .expect("mirror set");
    let src = write_cache_copy(&cache, "/inflight.bin", b"uploading bytes");

    let rows = vec![pulled("/inflight.bin", 2, true, "")];
    let outcome = apply_pulled_rows(&db, &cache, &rows, 2).expect("apply");
    assert_eq!(outcome.tombstoned, 1);
    assert!(db.get_file("/inflight.bin").expect("get").is_none());
    assert_eq!(db.sync_mirror_get("/inflight.bin").expect("mirror"), None);
    assert!(
        src.exists(),
        "an in-flight upload's source file must survive the tombstone — the worker's write-back revives the row"
    );
}

#[test]
fn apply_tombstone_for_missing_local_row_is_ok() {
    let (_dir, db, cache) = fresh_env("tomb-missing");
    let rows = vec![pulled("/never-existed.txt", 1, true, "")];
    let outcome = apply_pulled_rows(&db, &cache, &rows, 1).expect("apply");
    assert_eq!(outcome.tombstoned, 1);
    assert_eq!(outcome.applied, 0);
}

#[test]
fn apply_skips_ghost_pending_row_without_local_copy() {
    let (_dir, db, cache) = fresh_env("ghost");
    // A pending row (is_uploaded == false) whose bytes this instance does
    // not hold: the local cache path does not exist, so the row is a ghost
    // — no files row, no mirror row, nothing written.
    let pending = record("/pending.bin", 10, false);
    let payload = serialize(&pending, &[]);
    let rows = vec![pulled("/pending.bin", 1, false, &payload)];

    let outcome = apply_pulled_rows(&db, &cache, &rows, 1).expect("apply");
    assert_eq!(
        outcome,
        ApplyOutcome {
            pulled: 1,
            applied: 0,
            skipped_ghost: 1,
            skipped_idempotent: 0,
            skipped_invalid: 0,
            tombstoned: 0,
        }
    );
    assert!(db.get_file("/pending.bin").expect("get").is_none());
    assert_eq!(db.sync_mirror_get("/pending.bin").expect("mirror"), None);
}

#[test]
fn apply_pending_row_with_local_copy_is_applied() {
    let (_dir, db, cache) = fresh_env("ghost-copy");
    let pending = record("/pending.bin", 10, false);
    write_cache_copy(&cache, "/pending.bin", b"pending bytes");
    let payload = serialize(&pending, &[]);
    let rows = vec![pulled("/pending.bin", 3, false, &payload)];

    let outcome = apply_pulled_rows(&db, &cache, &rows, 3).expect("apply");
    assert_eq!(outcome.applied, 1);
    assert_eq!(outcome.skipped_ghost, 0);

    let row = db.get_file("/pending.bin").expect("get").expect("row");
    assert_eq!(row.size, 10);
    assert!(!row.is_uploaded, "pending state travels in the payload");
    assert!(!row.is_cached, "apply always stores is_cached = false");
    assert_eq!(
        db.sync_mirror_get("/pending.bin").expect("mirror"),
        Some((row_hash(&payload), 3))
    );
}

#[test]
fn apply_hash_equal_row_updates_mirror_only() {
    let (_dir, db, cache) = fresh_env("hash-eq");
    let rec = record("/same.txt", 42, true);
    let chunks = vec![chunk(0, 5, 42)];
    let id = insert(&db, &upsert_from(&rec, false), &chunks);
    let before = db.get_file("/same.txt").expect("get").expect("row");
    db.sync_mirror_set("/same.txt", "old-hash", 1)
        .expect("mirror set");

    let payload = serialize(&rec, &chunks);
    let rows = vec![pulled("/same.txt", 9, false, &payload)];
    let outcome = apply_pulled_rows(&db, &cache, &rows, 9).expect("apply");
    assert_eq!(
        outcome.applied, 1,
        "the mirror advanced — row counts as applied"
    );

    // The files row must not be rewritten: id and both DB timestamps are
    // unchanged (a write would bump updated_at and, on a delete+insert
    // implementation, change the id).
    let after = db.get_file("/same.txt").expect("get").expect("row");
    assert_eq!(after.id, id);
    assert_eq!(
        after.created_at, before.created_at,
        "no files write: created_at untouched"
    );
    assert_eq!(
        after.updated_at, before.updated_at,
        "no files write: updated_at untouched"
    );

    // Mirror advanced to (payload hash, row version).
    assert_eq!(
        db.sync_mirror_get("/same.txt").expect("mirror"),
        Some((row_hash(&payload), 9))
    );
}

#[test]
fn apply_remote_wins_overwrites_row_and_chunks() {
    let (_dir, db, cache) = fresh_env("overwrite");
    let local = record("/big.bin", 100, true);
    let local_chunks = vec![chunk(0, 11, 60), chunk(1, 12, 40)];
    insert(&db, &upsert_from(&local, true), &local_chunks);
    db.sync_mirror_set("/big.bin", "old-hash", 1)
        .expect("mirror set");

    let mut remote = record("/big.bin", 200, true);
    remote.mtime = local.mtime + 500.0;
    let remote_chunks = vec![chunk(0, 77, 200)];
    let payload = serialize(&remote, &remote_chunks);

    let rows = vec![pulled("/big.bin", 2, false, &payload)];
    let outcome = apply_pulled_rows(&db, &cache, &rows, 2).expect("apply");
    assert_eq!(outcome.applied, 1);
    assert_eq!(outcome.skipped_ghost, 0);

    let row = db.get_file("/big.bin").expect("get").expect("row");
    assert_eq!(row.size, 200, "remote wins on hash change");
    assert_eq!(row.mtime, remote.mtime, "payload mtime is applied as-is");
    assert!(!row.is_cached, "overwrite always stores is_cached = false");

    let after = db.get_chunks_by_file_id(row.id).expect("chunks");
    assert_eq!(
        after.len(),
        1,
        "the old chunk list is replaced by the payload chunks"
    );
    assert_eq!(after[0].chunk_index, 0);
    assert_eq!(after[0].telegram_msg_id, Some(77));
    assert_eq!(after[0].size, 200);
    assert_eq!(
        db.sync_mirror_get("/big.bin").expect("mirror"),
        Some((row_hash(&payload), 2))
    );
}

#[test]
fn apply_overwrite_clears_stale_cached_copy() {
    let (_dir, db, cache) = fresh_env("stale");
    let local = record("/stale.bin", 10, true);
    insert(&db, &upsert_from(&local, true), &[chunk(0, 11, 10)]);
    let stale_path = write_cache_copy(&cache, "/stale.bin", b"old bytes");

    let mut remote = record("/stale.bin", 20, true);
    remote.mtime = local.mtime + 1.0;
    let payload = serialize(&remote, &[chunk(0, 12, 20)]);
    let rows = vec![pulled("/stale.bin", 1, false, &payload)];

    apply_pulled_rows(&db, &cache, &rows, 1).expect("apply");
    assert!(
        !stale_path.exists(),
        "a stale cache copy of changed content is a correctness hazard — it must be removed"
    );
    let row = db.get_file("/stale.bin").expect("get").expect("row");
    assert!(
        !row.is_cached,
        "the is_cached flag is cleared with the copy"
    );
}

#[test]
fn apply_overwrite_removes_stale_cache_file_on_disk() {
    // CONTRACT CORRECTION (decisions.md 2026-09-05 High②, owner-approved
    // fix batch): cache cleanup keys on DISK presence — the same basis as
    // hydrate's hit probe — not on the row's is_cached flag. The flag and
    // the disk drift (e.g. an upload-succeeded-then-cache-delete-failed
    // residue leaves the flag cleared with bytes on disk); a flag-gated
    // cleanup would keep serving those stale bytes. Replaces the old
    // apply_overwrite_keeps_unflagged_cache_file contract test, which
    // pinned the flag-gated semantics the review found wrong.
    let (_dir, db, cache) = fresh_env("stale-disk");
    let local = record("/drift.bin", 10, true);
    // Row flag says not cached (drifted state), but a file sits on disk.
    insert(&db, &upsert_from(&local, false), &[chunk(0, 11, 10)]);
    let disk_path = write_cache_copy(&cache, "/drift.bin", b"drifted bytes");

    let mut remote = record("/drift.bin", 20, true);
    remote.mtime = local.mtime + 1.0;
    let payload = serialize(&remote, &[chunk(0, 12, 20)]);
    let rows = vec![pulled("/drift.bin", 1, false, &payload)];

    apply_pulled_rows(&db, &cache, &rows, 1).expect("apply");
    assert!(
        !disk_path.exists(),
        "cleanup keys on disk presence, not the row flag — hydrate's hit probe is disk-based and would serve the stale bytes"
    );
}

#[test]
fn apply_overwrite_preserves_inflight_pending_upload_source() {
    // In-flight upload protection on the overwrite path (decisions
    // 2026-09-05, "later action wins" LWW): a local PENDING row (the
    // ghost gate guarantees its cache copy is on disk when a pulled
    // pending row gets this far) hit by a remote overwrite keeps its
    // source file — the upload worker's success write-back revives the
    // row.
    let (_dir, db, cache) = fresh_env("ow-inflight");
    let local = record("/inflight.bin", 10, false);
    insert(&db, &upsert_from(&local, false), &[chunk(0, 11, 10)]);
    let src = write_cache_copy(&cache, "/inflight.bin", b"uploading bytes");

    let mut remote = record("/inflight.bin", 20, true);
    remote.mtime = local.mtime + 1.0;
    let payload = serialize(&remote, &[chunk(0, 12, 20)]);
    let rows = vec![pulled("/inflight.bin", 2, false, &payload)];

    let outcome = apply_pulled_rows(&db, &cache, &rows, 2).expect("apply");
    assert_eq!(outcome.applied, 1);
    let row = db.get_file("/inflight.bin").expect("get").expect("row");
    assert_eq!(row.size, 20, "remote content wins the row itself");
    assert!(
        src.exists(),
        "the in-flight upload's source file must survive the overwrite — the worker's write-back revives the row"
    );
}

#[test]
fn apply_advances_max_pulled_monotonically() {
    let (_dir, db, cache) = fresh_env("cursor");
    db.sync_state_set(10).expect("state set");

    // A stale server response (e.g. a rebuilt server db) must never move
    // the cursor backwards.
    apply_pulled_rows(&db, &cache, &[], 3).expect("apply stale");
    assert_eq!(db.sync_state_get().expect("state"), 10);

    apply_pulled_rows(&db, &cache, &[], 12).expect("apply newer");
    assert_eq!(db.sync_state_get().expect("state"), 12);
}

#[test]
fn invalid_row_is_skipped_and_cursor_advances() {
    // Poisoned-row fix pin (decisions 2026-09-05 Medium): one bad payload
    // row must not abort the whole pass — the old `?` returned before
    // `sync_state_set`, permanently wedging the namespace cursor so no
    // later update could ever arrive. Now the bad row is counted and
    // skipped, the good row behind it applies, and the cursor advances
    // past both.
    let (_dir, db, cache) = fresh_env("poison");
    let good = record("/good.txt", 7, true);
    let good_payload = serialize(&good, &[chunk(0, 9, 7)]);
    let rows = vec![
        pulled("/bad.txt", 1, false, "{ this is not json"),
        pulled("/good.txt", 2, false, &good_payload),
    ];

    let outcome = apply_pulled_rows(&db, &cache, &rows, 2).expect("apply");
    assert_eq!(outcome.skipped_invalid, 1);
    assert_eq!(outcome.applied, 1);
    assert!(
        db.get_file("/good.txt").expect("get").is_some(),
        "the good row after the poisoned one must still apply"
    );
    assert!(
        db.get_file("/bad.txt").expect("get").is_none(),
        "an invalid row must not create local state"
    );
    assert_eq!(
        db.sync_state_get().expect("state"),
        2,
        "the cursor must advance past the poisoned row"
    );

    // A re-pull of the same batch neither fails nor wedges: the good row
    // dies at the idempotency gate, the bad row is skipped again.
    let outcome = apply_pulled_rows(&db, &cache, &rows, 2).expect("apply 2");
    assert_eq!(outcome.skipped_invalid, 1);
    assert_eq!(outcome.skipped_idempotent, 1);
    assert_eq!(db.sync_state_get().expect("state"), 2);
}
