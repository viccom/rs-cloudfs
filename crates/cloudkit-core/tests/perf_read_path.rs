//! Offline read-path performance benchmark (M6).
//!
//! `#[ignore]`d by default: it seeds 100k rows and asserts a wall-clock
//! budget, so it is opt-in (`cargo test -p cloudkit-core --test
//! perf_read_path -- --ignored --nocapture`) rather than part of the
//! per-commit suite. M6 acceptance wants "reads must be cheap": this
//! benchmark measures the metadata DB side (`MetaDatabase::list_dir`)
//! on a realistic tree (100 dirs x 1000 files). The real PROPFIND path
//! adds WebDAV XML serialization on top in `cloudkit-webdav`; that layer
//! is not measured here — this is the DB-direct lower bound that the
//! XML layer is expected to dominate by a small constant factor.

use std::time::Instant;

use cloudkit_core::database::MetaDatabase;
use rusqlite::Connection;

/// Budget: mean per-call latency of `list_dir` must stay under this.
const MAX_MEAN_MS: f64 = 100.0;

/// Iterations for each timed query set.
const RUNS: usize = 100;

fn seed_100k(dir_tag: &str) -> (tempfile::TempDir, MetaDatabase) {
    let dir = tempfile::tempdir().expect("tempdir");

    // Open once through the real API so the schema (indexes included) is
    // exactly what production creates, then hand the file to a raw
    // connection for the bulk seed.
    let db =
        MetaDatabase::open(&dir.path().join(format!("{dir_tag}.db"))).expect("open for schema");
    drop(db);

    let conn = Connection::open(dir.path().join(format!("{dir_tag}.db"))).expect("raw open");

    // Same upsert statement shape as `MetaDatabase::upsert_file`
    // (INSERT ... ON CONFLICT(rel_path) DO UPDATE), executed in one
    // explicit transaction: 100k autocommit fsyncs would take minutes,
    // while the row data produced is identical for a fresh database.
    conn.execute_batch("BEGIN").expect("begin");
    {
        let mut stmt = conn
            .prepare(
                "INSERT INTO files (
                    rel_path, name, parent_dir, size, mtime, is_dir,
                    is_uploaded, is_cached, is_encrypted, chunk_count,
                    created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(rel_path) DO UPDATE SET
                    updated_at=excluded.updated_at",
            )
            .expect("prepare seed stmt");

        let now = 1_700_000_000.0;
        for d in 0..100u32 {
            let dir_name = format!("dir_{d:04}");
            stmt.execute(rusqlite::params![
                format!("/{dir_name}"),
                dir_name,
                "/",
                0i64,
                now,
                1i64, // is_dir
                1i64,
                0i64,
                0i64,
                1i64,
                now,
                now,
            ])
            .expect("seed dir row");
            for f in 0..1000u32 {
                stmt.execute(rusqlite::params![
                    format!("/{dir_name}/file_{f:04}.bin"),
                    format!("file_{f:04}.bin"),
                    format!("/{dir_name}"),
                    (f % 4096) as i64,
                    now,
                    0i64, // file row
                    1i64,
                    0i64,
                    0i64,
                    1i64,
                    now,
                    now,
                ])
                .expect("seed file row");
            }
        }
    }
    conn.execute_batch("COMMIT").expect("commit");
    drop(conn);

    // Reader side goes through the real API from here on.
    let db =
        MetaDatabase::open(&dir.path().join(format!("{dir_tag}.db"))).expect("reopen after seed");
    (dir, db)
}

#[test]
#[ignore = "perf benchmark: opt-in via --ignored; seeds 100k rows"]
fn ignored_propfind_100k_read_path_under_100ms() {
    let (_dir, db) = seed_100k("perf100k");

    // Warm-up: page the root listing in once so OS file cache effects
    // do not dominate the first timed run.
    let warm = db.list_dir("/").expect("warm-up root");
    assert_eq!(warm.len(), 100, "root should list exactly the 100 dirs");

    // --- read path A: root directory listing (100 dir rows) ---
    let start = Instant::now();
    for _ in 0..RUNS {
        let rows = db.list_dir("/").expect("list root");
        assert_eq!(rows.len(), 100);
        assert!(rows[0].is_dir, "dirs sort first (is_dir DESC)");
    }
    let root_elapsed = start.elapsed();
    let root_mean_ms = root_elapsed.as_secs_f64() * 1000.0 / RUNS as f64;

    // --- read path B: deepest common case, one dir with 1000 files ---
    let start = Instant::now();
    for _ in 0..RUNS {
        let rows = db.list_dir("/dir_0005").expect("list dir_0005");
        assert_eq!(rows.len(), 1000);
        assert!(!rows[0].is_dir, "file rows only");
    }
    let dir_elapsed = start.elapsed();
    let dir_mean_ms = dir_elapsed.as_secs_f64() * 1000.0 / RUNS as f64;

    println!(
        "perf_read_path (100k rows, {RUNS} runs each, {} build):\n  \
         list_dir(\"/\")          100 rows : mean {root_mean_ms:.3} ms  (total {root_elapsed:?})\n  \
         list_dir(\"/dir_0005\") 1000 rows : mean {dir_mean_ms:.3} ms  (total {dir_elapsed:?})",
        if cfg!(debug_assertions) { "debug" } else { "release" },
    );

    assert!(
        root_mean_ms < MAX_MEAN_MS,
        "root listing mean {root_mean_ms:.3} ms exceeded {MAX_MEAN_MS} ms budget"
    );
    assert!(
        dir_mean_ms < MAX_MEAN_MS,
        "dir listing mean {dir_mean_ms:.3} ms exceeded {MAX_MEAN_MS} ms budget"
    );
}
