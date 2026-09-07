//! RED-phase tests for the sync-lite tables of `cloudkit_core::database`.
//!
//! Contract under test (docs/plans/2026-09-04-sync-lite.md, «客户端»):
//! `MetaDatabase` grows two **additive** tables (the Python-contract DDL
//! of `files`/`chunks`/`stats` is untouched — schema additions only,
//! created with `IF NOT EXISTS` so an existing database adopted at open
//! gains them automatically):
//!
//! ```sql
//! sync_mirror(rel_path TEXT PRIMARY KEY, row_hash TEXT,
//!             server_version INTEGER NOT NULL DEFAULT 0)
//! sync_state(id INTEGER PRIMARY KEY CHECK(id=0),
//!            max_pulled INTEGER NOT NULL DEFAULT 0)
//! ```
//!
//! plus the accessor methods: `sync_mirror_get` / `sync_mirror_set`
//! (upsert) / `sync_mirror_delete` (missing is Ok) / `sync_mirror_all`
//! (ordered for deterministic diffs) and `sync_state_get` (no row = 0) /
//! `sync_state_set` (upsert into the single `id = 0` row).

use cloudkit_core::database::MetaDatabase;
use rusqlite::Connection;

// ------------------------------------------------------------- helpers ---

fn fresh_db(tag: &str) -> (tempfile::TempDir, MetaDatabase) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join(format!("{tag}.db"))).expect("open");
    (dir, db)
}

/// `sqlite_master.sql` of `object`, with every whitespace run collapsed to
/// a single space (DDL fragments are asserted against this normalised
/// text, so formatting never breaks the contract pins).
fn master_sql(conn: &Connection, object: &str) -> String {
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [object],
            |row| row.get(0),
        )
        .unwrap_or_else(|e| panic!("table {object} must exist: {e}"));
    whitespace_collapsed(&sql)
}

fn whitespace_collapsed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Counts rows in `table` through an external connection.
fn row_count(db_path: &std::path::Path, table: &str) -> i64 {
    let conn = Connection::open(db_path).expect("external connection");
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count")
}

// ------------------------------------------------------ schema / open ---

#[test]
fn sync_tables_created_on_open_and_reopen_is_idempotent() {
    let (dir, db) = fresh_db("schema");
    let db_path = dir.path().join("schema.db");
    drop(db);

    // Re-opening an existing database (IF NOT EXISTS) must succeed and
    // keep the sync tables present.
    let db_again = MetaDatabase::open(&db_path).expect("reopen");
    let state = db_again.sync_state_get().expect("sync_state_get");
    assert_eq!(state, 0, "reopened db still answers sync_state_get");
}

#[test]
fn sync_tables_added_to_pre_existing_database() {
    // A database created before sync-lite (contract tables only) must
    // gain the two sync tables when adopted by MetaDatabase::open —
    // the "已存在的库打开时自动补建" requirement.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("pre-existing.db");
    {
        let conn = Connection::open(&db_path).expect("raw connection");
        conn.execute_batch(
            "CREATE TABLE files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                rel_path TEXT UNIQUE NOT NULL,
                name TEXT NOT NULL,
                parent_dir TEXT NOT NULL,
                size INTEGER DEFAULT 0,
                mtime REAL DEFAULT 0,
                sha256 TEXT,
                is_dir INTEGER DEFAULT 0,
                telegram_msg_id INTEGER,
                is_uploaded INTEGER DEFAULT 0,
                is_cached INTEGER DEFAULT 1,
                is_encrypted INTEGER DEFAULT 0,
                chunk_count INTEGER DEFAULT 1,
                mime_type TEXT,
                created_at REAL,
                updated_at REAL
            );
            CREATE TABLE chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                telegram_msg_id INTEGER,
                size INTEGER NOT NULL,
                sha256 TEXT,
                FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE,
                UNIQUE(file_id, chunk_index)
            );
            CREATE TABLE stats (
                key TEXT PRIMARY KEY,
                value TEXT
            );",
        )
        .expect("pre-sync-lite schema");
    }

    let db = MetaDatabase::open(&db_path).expect("adopt pre-existing db");
    assert_eq!(
        db.sync_state_get().expect("sync_state_get"),
        0,
        "adopted db answers sync queries right away"
    );

    let conn = Connection::open(&db_path).expect("external connection");
    for table in ["sync_mirror", "sync_state"] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("sqlite_master");
        assert_eq!(n, 1, "table {table} must exist after adoption");
    }
}

#[test]
fn sync_table_ddl_shape_is_pinned() {
    let (dir, _db) = fresh_db("ddl");
    let conn = Connection::open(dir.path().join("ddl.db")).expect("external connection");

    let mirror = master_sql(&conn, "sync_mirror");
    assert!(
        mirror.contains("rel_path TEXT PRIMARY KEY"),
        "sync_mirror pk column: {mirror}"
    );
    assert!(mirror.contains("row_hash TEXT"), "sync_mirror: {mirror}");
    assert!(
        mirror.contains("server_version INTEGER NOT NULL DEFAULT 0"),
        "sync_mirror: {mirror}"
    );

    let state = master_sql(&conn, "sync_state");
    assert!(
        state.contains("id INTEGER PRIMARY KEY CHECK(id=0)"),
        "sync_state single-row constraint: {state}"
    );
    assert!(
        state.contains("max_pulled INTEGER NOT NULL DEFAULT 0"),
        "sync_state: {state}"
    );
}

// ------------------------------------------------------- sync_mirror ---

#[test]
fn sync_mirror_get_missing_returns_none() {
    let (_dir, db) = fresh_db("mirror-missing");
    assert_eq!(
        db.sync_mirror_get("/nope.bin").expect("sync_mirror_get"),
        None,
        "a path never mirrored must read as None"
    );
}

#[test]
fn sync_mirror_set_then_get_roundtrip() {
    let (_dir, db) = fresh_db("mirror-rt");
    db.sync_mirror_set("/docs/a.txt", "aa11", 5).expect("set");

    let (row_hash, server_version) = db
        .sync_mirror_get("/docs/a.txt")
        .expect("get")
        .expect("row must exist after set");
    assert_eq!(row_hash, "aa11");
    assert_eq!(server_version, 5);
}

#[test]
fn sync_mirror_set_upsert_overwrites() {
    let (_dir, db) = fresh_db("mirror-upsert");
    db.sync_mirror_set("/a.txt", "old", 1).expect("first set");
    db.sync_mirror_set("/a.txt", "new-hash", 9)
        .expect("second set");

    let (row_hash, server_version) = db
        .sync_mirror_get("/a.txt")
        .expect("get")
        .expect("row survives the upsert");
    assert_eq!(row_hash, "new-hash", "upsert must overwrite row_hash");
    assert_eq!(server_version, 9, "upsert must overwrite server_version");
}

#[test]
fn sync_mirror_delete_removes_and_missing_is_ok() {
    let (_dir, db) = fresh_db("mirror-del");
    db.sync_mirror_set("/gone.txt", "hash", 2).expect("set");

    db.sync_mirror_delete("/gone.txt").expect("delete existing");
    assert_eq!(
        db.sync_mirror_get("/gone.txt").expect("get"),
        None,
        "deleted row must be gone"
    );

    // Deleting a path that was never mirrored is Ok, not an error
    // (same leniency as MetaDatabase::delete_file).
    db.sync_mirror_delete("/never-was.txt")
        .expect("delete missing is Ok");
}

#[test]
fn sync_mirror_all_enumerates_every_row_ordered() {
    let (_dir, db) = fresh_db("mirror-all");
    db.sync_mirror_set("/c.txt", "hc", 3).expect("set c");
    db.sync_mirror_set("/a.txt", "ha", 1).expect("set a");
    db.sync_mirror_set("/b/nested.txt", "hb", 2).expect("set b");

    let all = db.sync_mirror_all().expect("sync_mirror_all");
    assert_eq!(
        all,
        vec![
            ("/a.txt".to_string(), "ha".to_string(), 1),
            ("/b/nested.txt".to_string(), "hb".to_string(), 2),
            ("/c.txt".to_string(), "hc".to_string(), 3),
        ],
        "sync_mirror_all must return every row ordered by rel_path"
    );
}

#[test]
fn sync_mirror_rows_survive_reopen() {
    let (dir, db) = fresh_db("mirror-persist");
    let db_path = dir.path().join("mirror-persist.db");
    db.sync_mirror_set("/persist.txt", "hp", 4).expect("set");
    drop(db);

    let db = MetaDatabase::open(&db_path).expect("reopen");
    assert_eq!(
        db.sync_mirror_get("/persist.txt").expect("get"),
        Some(("hp".to_string(), 4)),
        "mirror rows must survive a close/reopen"
    );
}

// ------------------------------------------------------- sync_state ---

#[test]
fn sync_state_defaults_to_zero() {
    let (_dir, db) = fresh_db("state-default");
    assert_eq!(
        db.sync_state_get().expect("sync_state_get"),
        0,
        "a fresh database has no row and must read as 0"
    );
}

#[test]
fn sync_state_set_then_overwrite_single_row() {
    let (dir, db) = fresh_db("state-set");
    db.sync_state_set(42).expect("set 42");
    assert_eq!(db.sync_state_get().expect("get"), 42);

    db.sync_state_set(99).expect("overwrite with 99");
    assert_eq!(
        db.sync_state_get().expect("get"),
        99,
        "sync_state_set must upsert, not append"
    );

    // The CHECK(id=0) single-row invariant: still exactly one row.
    assert_eq!(
        row_count(&dir.path().join("state-set.db"), "sync_state"),
        1,
        "sync_state must hold exactly one row (id = 0)"
    );
}

#[test]
fn sync_state_survives_reopen() {
    let (dir, db) = fresh_db("state-persist");
    let db_path = dir.path().join("state-persist.db");
    db.sync_state_set(1234).expect("set");
    drop(db);

    let db = MetaDatabase::open(&db_path).expect("reopen");
    assert_eq!(
        db.sync_state_get().expect("get"),
        1234,
        "max_pulled must survive a close/reopen"
    );
}
