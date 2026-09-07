//! RED-phase tests for `cloudkit_core::database`.
//!
//! Contract under test: the SQLite schema and SQL semantics of the Python
//! `cydrive/database.py` (frozen by `docs/rust-rewrite-design.md`,
//! «兼容契约»), plus the two mandated fixes — `RETURNING id` for stable
//! upsert ids and explicit chunk deletion inside the delete transaction.
//! The `adopts_python_generated_database` test replays a real Python
//! `iterdump` fixture and must read it back unchanged.

use std::thread;
use std::time::Duration;

use cloudkit_core::database::{FileRecord, FileUpsert, MetaDatabase, Stats};
use rusqlite::Connection;

// ------------------------------------------------------------- helpers ---

fn fresh_db(tag: &str) -> (tempfile::TempDir, MetaDatabase) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join(format!("{tag}.db"))).expect("open");
    (dir, db)
}

/// Flat upsert payload with `name`/`parent_dir` derived from `rel`.
fn entry(rel: &str, size: i64) -> FileUpsert {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    let parent_dir = match rel.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => rel[..i].to_string(),
    };
    FileUpsert {
        rel_path: rel.to_string(),
        name,
        parent_dir,
        size,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: None,
        is_uploaded: false,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    }
}

fn dir_entry(rel: &str) -> FileUpsert {
    let mut e = entry(rel, 0);
    e.is_dir = true;
    e
}

fn names(records: &[FileRecord]) -> Vec<&str> {
    records.iter().map(|r| r.name.as_str()).collect()
}

// ------------------------------------------------------- open / schema ---

#[test]
fn open_creates_schema_and_enables_wal() {
    let (dir, db) = fresh_db("schema");
    let db_path = dir.path().join("schema.db");
    drop(db);

    // Re-opening an existing database (IF NOT EXISTS) must succeed too.
    let db_again = MetaDatabase::open(&db_path);
    assert!(db_again.is_ok());

    let conn = Connection::open(&db_path).expect("external connection");
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal_mode");
    assert_eq!(mode.to_lowercase(), "wal");

    for object in [
        "files",
        "chunks",
        "stats",
        "idx_files_parent",
        "idx_files_msg_id",
        "idx_files_uploaded",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                [object],
                |row| row.get(0),
            )
            .expect("sqlite_master");
        assert_eq!(n, 1, "expected object {object} to exist");
    }
}

// --------------------------------------------------------------- upsert ---

#[test]
fn upsert_returns_stable_row_id_on_conflict() {
    let (_dir, db) = fresh_db("upsert-id");

    let id1 = db.upsert_file(&entry("/docs/a.txt", 100)).expect("insert");
    assert_eq!(id1, 1);

    // Same rel_path: the conflict update must return the SAME row id
    // (RETURNING id — fixes the Python lastrowid bug).
    let id2 = db.upsert_file(&entry("/docs/a.txt", 200)).expect("update");
    assert_eq!(id2, id1);

    let rec = db.get_file("/docs/a.txt").expect("query").expect("row");
    assert_eq!(rec.id, id1);
    assert_eq!(rec.size, 200);
}

#[test]
fn conflict_update_coalesces_optional_columns() {
    let (_dir, db) = fresh_db("coalesce");

    let mut first = entry("/f.bin", 1);
    first.sha256 = Some("aa".repeat(32));
    first.telegram_msg_id = Some(555);
    first.mime_type = Some("application/octet-stream".to_string());
    db.upsert_file(&first).expect("insert with values");

    // None on conflict keeps the stored value (Python coalesce).
    db.upsert_file(&entry("/f.bin", 2))
        .expect("update with None");
    let rec = db.get_file("/f.bin").expect("query").expect("row");
    assert_eq!(rec.size, 2, "non-optional columns are always overwritten");
    assert_eq!(rec.sha256.as_deref(), Some("aa".repeat(32).as_str()));
    assert_eq!(rec.telegram_msg_id, Some(555));
    assert_eq!(rec.mime_type.as_deref(), Some("application/octet-stream"));

    // Some on conflict overwrites the stored value.
    let mut third = entry("/f.bin", 3);
    third.sha256 = Some("bb".repeat(32));
    third.telegram_msg_id = Some(666);
    third.mime_type = Some("text/plain".to_string());
    db.upsert_file(&third).expect("update with Some");
    let rec = db.get_file("/f.bin").expect("query").expect("row");
    assert_eq!(rec.sha256.as_deref(), Some("bb".repeat(32).as_str()));
    assert_eq!(rec.telegram_msg_id, Some(666));
    assert_eq!(rec.mime_type.as_deref(), Some("text/plain"));
}

#[test]
fn timestamps_are_db_maintained() {
    let (_dir, db) = fresh_db("timestamps");

    db.upsert_file(&entry("/t.txt", 1)).expect("insert");
    let rec = db.get_file("/t.txt").expect("query").expect("row");
    let created = rec.created_at.expect("created_at set on first insert");
    let updated = rec.updated_at.expect("updated_at set on insert");
    assert!(created > 0.0);
    assert!(updated >= created);
}

// --------------------------------------------------------------- lookup ---

#[test]
fn get_file_and_msg_id_lookup_hit_and_miss() {
    let (_dir, db) = fresh_db("lookup");

    let mut e = entry("/x/y.md", 5);
    e.telegram_msg_id = Some(42);
    db.upsert_file(&e).expect("insert");

    let hit = db.get_file("/x/y.md").expect("query").expect("hit");
    assert_eq!(hit.name, "y.md");
    assert_eq!(hit.parent_dir, "/x");

    assert!(db.get_file("/missing").expect("query").is_none());

    let by_msg = db.get_file_by_msg_id(42).expect("query").expect("hit");
    assert_eq!(by_msg.rel_path, "/x/y.md");
    assert!(db.get_file_by_msg_id(999).expect("query").is_none());
}

// -------------------------------------------------------------- listing ---

#[test]
fn list_dir_orders_dirs_first_then_name_asc() {
    let (_dir, db) = fresh_db("listdir");

    db.upsert_file(&dir_entry("/a")).expect("insert");
    db.upsert_file(&entry("/a/z.txt", 1)).expect("insert");
    db.upsert_file(&entry("/a/m.txt", 1)).expect("insert");
    db.upsert_file(&dir_entry("/a/sub")).expect("insert");
    db.upsert_file(&entry("/m.txt", 1)).expect("insert");

    let children = db.list_dir("/a").expect("list");
    assert_eq!(names(&children), vec!["sub", "m.txt", "z.txt"]);
    assert!(children[0].is_dir, "directories sort first");
    assert!(!children[1].is_dir);

    assert!(db.list_dir("/nope").expect("list").is_empty());
}

#[test]
fn list_all_files_orders_by_updated_at_desc() {
    let (_dir, db) = fresh_db("listall");

    db.upsert_file(&entry("/one.txt", 1)).expect("insert");
    thread::sleep(Duration::from_millis(10));
    db.upsert_file(&entry("/two.txt", 1)).expect("insert");
    thread::sleep(Duration::from_millis(10));
    db.upsert_file(&entry("/three.txt", 1)).expect("insert");

    let all = db.list_all_files().expect("list");
    assert_eq!(names(&all), vec!["three.txt", "two.txt", "one.txt"]);
}

#[test]
fn search_matches_name_and_rel_path_substrings() {
    let (_dir, db) = fresh_db("search");

    db.upsert_file(&entry("/Documents/report.pdf", 1))
        .expect("insert");
    db.upsert_file(&entry("/notes/readme.txt", 1))
        .expect("insert");

    // Hit via the name column.
    let by_name = db.search_files("report").expect("search");
    assert_eq!(names(&by_name), vec!["report.pdf"]);

    // Hit via the rel_path column ('Documents' is not in the name).
    let by_path = db.search_files("Doc").expect("search");
    assert_eq!(names(&by_path), vec!["report.pdf"]);

    assert!(db.search_files("no-such-thing").expect("search").is_empty());
}

// --------------------------------------------------------------- chunks ---

#[test]
fn chunks_upsert_sorts_and_overwrites_on_conflict() {
    let (_dir, db) = fresh_db("chunks");

    let file_id = db
        .upsert_file(&entry("/big/movie.mkv", 5_976_883_200))
        .expect("insert");

    // Insert out of order; reads come back ordered by chunk_index ASC.
    db.upsert_chunk(file_id, 2, 203, 1024, None).expect("chunk");
    db.upsert_chunk(file_id, 0, 201, 1_992_294_400, Some(&"c0".repeat(16)))
        .expect("chunk");
    db.upsert_chunk(file_id, 1, 202, 1_992_294_400, None)
        .expect("chunk");

    let chunks = db.get_chunks_by_file_id(file_id).expect("read");
    let indexes: Vec<i64> = chunks.iter().map(|c| c.chunk_index).collect();
    let msgs: Vec<Option<i64>> = chunks.iter().map(|c| c.telegram_msg_id).collect();
    assert_eq!(indexes, vec![0, 1, 2]);
    assert_eq!(msgs, vec![Some(201), Some(202), Some(203)]);

    // Same (file_id, chunk_index): msg_id/size overwritten, no duplicate row.
    db.upsert_chunk(file_id, 1, 999, 7, None)
        .expect("overwrite");
    let chunks = db.get_chunks_by_file_id(file_id).expect("read");
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[1].telegram_msg_id, Some(999));
    assert_eq!(chunks[1].size, 7);

    // Chunk sha256 coalesces like the file columns: None keeps the old value.
    db.upsert_chunk(file_id, 0, 201, 1, None).expect("coalesce");
    let chunks = db.get_chunks_by_file_id(file_id).expect("read");
    assert_eq!(chunks[0].sha256.as_deref(), Some("c0".repeat(16).as_str()));

    assert!(db
        .get_chunks_by_file_id(file_id + 12345)
        .expect("read")
        .is_empty());
}

// --------------------------------------------------------------- delete ---

#[test]
fn delete_file_cascades_chunks_in_transaction() {
    let (_dir, db) = fresh_db("delete");

    let file_id = db.upsert_file(&entry("/gone.bin", 10)).expect("insert");
    db.upsert_chunk(file_id, 0, 77, 10, None).expect("chunk");
    db.upsert_chunk(file_id, 1, 78, 10, None).expect("chunk");

    db.delete_file("/gone.bin").expect("delete");
    assert!(db.get_file("/gone.bin").expect("query").is_none());
    assert!(db.get_chunks_by_file_id(file_id).expect("read").is_empty());

    // Deleting a missing path is a no-op, not an error (Python parity).
    db.delete_file("/never-there").expect("no-op delete");
}

// ---------------------------------------------------------------- stats ---

#[test]
fn stats_count_files_dirs_and_upload_state() {
    let (_dir, db) = fresh_db("stats");

    // Empty database: SUM(size) over zero rows is NULL -> 0, not an error.
    let empty = db.get_stats().expect("stats");
    assert_eq!(
        empty,
        Stats {
            total_files: 0,
            total_bytes: 0,
            total_dirs: 0,
            uploaded_files: 0,
            pending_uploads: 0,
        }
    );

    db.upsert_file(&dir_entry("/d1")).expect("insert");
    db.upsert_file(&dir_entry("/d2")).expect("insert");

    let mut f1 = entry("/f1.bin", 100);
    f1.is_uploaded = true;
    f1.telegram_msg_id = Some(10);
    db.upsert_file(&f1).expect("insert");
    db.upsert_file(&entry("/f2.bin", 50)).expect("insert");
    let mut f3 = entry("/f3.bin", 25);
    f3.is_uploaded = true;
    db.upsert_file(&f3).expect("insert");

    let stats = db.get_stats().expect("stats");
    assert_eq!(
        stats,
        Stats {
            total_files: 3,
            total_bytes: 175,
            total_dirs: 2,
            uploaded_files: 2,
            pending_uploads: 1,
        }
    );
}

// ------------------------------------------------------------- adoption ---

/// The decisive compatibility test: a real Python `iterdump` fixture must be
/// adopted unchanged, every field round-tripped, and stay writable.
// Timestamp literal copied verbatim from the Python fixture (beyond f64
// round-trip precision on purpose).
#[allow(clippy::excessive_precision)]
#[test]
fn adopts_python_generated_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cydrive_meta.db");

    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/compat/fixtures/python_meta.sql"
    ))
    .expect("fixture");
    let conn = Connection::open(&db_path).expect("external connection");
    // The dump orders tables alphabetically (chunks before files), so the
    // replay connection must not enforce foreign keys — matching Python's
    // sqlite3, which keeps FK off by default. (The bundled SQLite that
    // rusqlite ships compiles SQLITE_DEFAULT_FOREIGN_KEYS=1, hence the
    // explicit pragma.)
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .expect("disable fk for replay");
    conn.execute_batch(&sql).expect("replay python dump");
    drop(conn);

    let db = MetaDatabase::open(&db_path).expect("adopt python database");

    // /Documents/report.pdf — every column matches the fixture row.
    let report = db
        .get_file("/Documents/report.pdf")
        .expect("query")
        .expect("report.pdf row");
    assert_eq!(report.id, 2);
    assert_eq!(report.rel_path, "/Documents/report.pdf");
    assert_eq!(report.name, "report.pdf");
    assert_eq!(report.parent_dir, "/Documents");
    assert_eq!(report.size, 1024);
    assert!((report.mtime - 1_700_000_000.5).abs() < 1e-3);
    assert_eq!(report.sha256.as_deref(), Some("ab".repeat(32).as_str()));
    assert!(!report.is_dir);
    assert_eq!(report.telegram_msg_id, Some(111));
    assert!(report.is_uploaded);
    assert!(report.is_cached);
    assert!(!report.is_encrypted);
    assert_eq!(report.chunk_count, 1);
    assert_eq!(report.mime_type.as_deref(), Some("application/pdf"));
    let created = report.created_at.expect("created_at");
    assert!((created - 1.78826717518969512e9).abs() < 1.0);

    // msg-id lookup lands on the chunked movie file.
    let movie = db
        .get_file_by_msg_id(200)
        .expect("query")
        .expect("movie row");
    assert_eq!(movie.rel_path, "/big/movie.mkv");
    assert_eq!(movie.id, 5);
    assert_eq!(movie.size, 5_976_883_200);
    assert_eq!(movie.chunk_count, 3);

    // Its chunks come back as msg 201/202/203 ordered by index.
    let chunks = db.get_chunks_by_file_id(movie.id).expect("read");
    let msgs: Vec<Option<i64>> = chunks.iter().map(|c| c.telegram_msg_id).collect();
    assert_eq!(msgs, vec![Some(201), Some(202), Some(203)]);
    assert_eq!(chunks[0].size, 1_992_294_400);
    assert_eq!(chunks[2].size, 1024);

    // Root listing: directories first (Documents < big in byte order).
    let root = db.list_dir("/").expect("list");
    assert_eq!(names(&root), vec!["Documents", "big", "notes.txt"]);
    assert!(root[0].is_dir && root[1].is_dir && !root[2].is_dir);

    // Aggregate stats over the fixture data.
    let stats = db.get_stats().expect("stats");
    assert_eq!(
        stats,
        Stats {
            total_files: 3,
            total_bytes: 1024 + 42 + 5_976_883_200,
            total_dirs: 2,
            uploaded_files: 2,
            pending_uploads: 1,
        }
    );

    // The adopted database stays writable: upsert then delete round-trips.
    let id = db
        .upsert_file(&entry("/adopted-new.txt", 7))
        .expect("upsert on adopted db");
    assert_eq!(id, 6, "AUTOINCREMENT continues after the dump's max id");
    let rec = db
        .get_file("/adopted-new.txt")
        .expect("query")
        .expect("row");
    assert_eq!(rec.size, 7);
    db.delete_file("/adopted-new.txt").expect("delete");
    assert!(db.get_file("/adopted-new.txt").expect("query").is_none());
}

// ------------------------------------------------------------ rename ---

/// rename_path on a file: the row moves to the new rel_path with name /
/// parent_dir rewritten, keeps its id (so chunk rows stay linked) and
/// every other field verbatim; the old path is gone.
#[test]
fn rename_path_moves_file_row_keeping_chunks_linkage() {
    let (_dir, db) = fresh_db("rename_file");
    let file_id = db
        .upsert_file(&FileUpsert {
            telegram_msg_id: Some(4242),
            is_uploaded: true,
            chunk_count: 3,
            ..entry("/old.bin", 7)
        })
        .expect("seed file row");
    for index in 0..3 {
        db.upsert_chunk(file_id, index, 4242 + index, 3, None)
            .expect("seed chunk row");
    }

    db.rename_path("/old.bin", "/new-dir/new.bin")
        .expect("rename file");

    assert!(
        db.get_file("/old.bin").expect("query old").is_none(),
        "old path gone"
    );
    let row = db
        .get_file("/new-dir/new.bin")
        .expect("query new")
        .expect("row moved");
    assert_eq!(row.id, file_id, "row keeps its id");
    assert_eq!(row.name, "new.bin");
    assert_eq!(row.parent_dir, "/new-dir");
    assert_eq!(row.telegram_msg_id, Some(4242), "msg id carried over");
    assert_eq!(row.size, 7);
    let chunks = db.get_chunks_by_file_id(file_id).expect("chunks");
    assert_eq!(chunks.len(), 3, "chunk rows still linked to the same id");
    assert_eq!(chunks[0].telegram_msg_id, Some(4242));
}

/// rename_path on a directory: the dir row and every descendant row have
/// rel_path / parent_dir rewritten under the new prefix (dir first, then
/// children, in one transaction); nothing else changes.
#[test]
fn rename_path_moves_dir_subtree() {
    let (_dir, db) = fresh_db("rename_dir");
    db.upsert_file(&dir_entry("/docs")).expect("seed dir");
    db.upsert_file(&dir_entry("/docs/sub"))
        .expect("seed subdir");
    db.upsert_file(&entry("/docs/a.txt", 1)).expect("seed file");
    db.upsert_file(&entry("/docs/sub/b.txt", 2))
        .expect("seed nested file");
    // A sibling sharing the prefix text must not be touched.
    db.upsert_file(&dir_entry("/docs2"))
        .expect("seed prefix sibling");

    db.rename_path("/docs", "/books").expect("rename dir");

    for old in ["/docs", "/docs/a.txt", "/docs/sub", "/docs/sub/b.txt"] {
        assert!(
            db.get_file(old).expect("query old").is_none(),
            "old path gone: {old}"
        );
    }
    let dir = db.get_file("/books").expect("q").expect("dir moved");
    assert_eq!((dir.name.as_str(), dir.parent_dir.as_str()), ("books", "/"));
    let sub = db.get_file("/books/sub").expect("q").expect("sub moved");
    assert_eq!(sub.parent_dir, "/books");
    let a = db.get_file("/books/a.txt").expect("q").expect("file moved");
    assert_eq!(
        (a.name.as_str(), a.parent_dir.as_str()),
        ("a.txt", "/books")
    );
    let b = db
        .get_file("/books/sub/b.txt")
        .expect("q")
        .expect("nested file moved");
    assert_eq!(b.parent_dir, "/books/sub");
    // The dir row itself is not its own parent.
    assert!(
        db.get_file("/books/books").expect("q").is_none(),
        "no self-nesting artifact"
    );
    assert!(
        db.get_file("/docs2").expect("q").is_some(),
        "prefix sibling untouched"
    );
    assert_eq!(
        names(&db.list_dir("/books").expect("list")),
        vec!["sub", "a.txt"],
        "listing works through the new paths (dirs first, name asc)"
    );
}
