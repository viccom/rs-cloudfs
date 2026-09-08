//! RED-phase tests for `cloudkit_core::rebuild` (Phase 2 / K11,
//! docs/plans/2026-09-08-phase2-execution.md §6): the authoritative
//! backend bootstrap of the metadata DB.
//!
//! Contract under test:
//!
//! - `rebuild_from_backend(driver, db, root)`: recursive `list` from
//!   `root` → one `files` row per backend entry (upsert keyed on the
//!   vpath-shaped `rel_path`), with the authoritative-index row shape:
//!   `is_uploaded = 1` (the bytes exist in the backend — nothing is
//!   pending), `chunk_count = 1` (whole-file view: the driver's
//!   chunking is invisible above L2), `telegram_msg_id = fs_id`-shaped
//!   handle parsed as i64 (path-shaped local handles degrade to the K6
//!   `0` placeholder), `mtime = Entry.mtime`. Directory rows follow the
//!   `create_dir` parity (size 0, `chunk_count = 0`, `msg_id = NULL`,
//!   `is_cached = 1`). `sha256`/`mime_type` stay `NULL` (coalescing
//!   keeps any stored value on re-rebuild).
//! - Plaintext-only semantics (K11): an instance with
//!   `enable_encryption = true` is refused up front with an actionable
//!   error pointing at `cydrive sync` — the backend only sees
//!   ciphertext containers under plaintext names, so a rebuilt row
//!   would mislabel encrypted payloads as plaintext. The gate is a
//!   pure config check (`ensure_plaintext_instance`), the CLI layer
//!   calls it before anything else.
//! - Empty backend root: `Ok` with zero rows (a fresh app dir is a
//!   legitimate state, not an error).

use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rebuild::{ensure_plaintext_instance, rebuild_from_backend, RebuildOutcome};
use cloudkit_storage::{MockStorageDriver, RelPath, StorageDriver, VolumeId, WriteHint};

// ------------------------------------------------------------- helpers ---

/// A mock driver over a scratch volume, plus the handle of its writer
/// seeding helper's last Entry (assertions key on the handle digits).
fn seeded_driver() -> MockStorageDriver {
    MockStorageDriver::new(VolumeId::parse("baidu:123456789").expect("volume id"))
}

/// Seeds one backend file of `data` at the volume-relative `path`
/// (writer + write + close; parents auto-created by the mock).
async fn seed_file(driver: &MockStorageDriver, path: &str, data: &[u8]) {
    let rel = RelPath::new(path).expect("seed path");
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel, &hint).await.expect("seed writer");
    stager.write(data).await.expect("seed write");
    stager.close().await.expect("seed close");
}

/// The entry id (numeric mock handle) the backend assigned to `path`.
async fn handle_of(driver: &MockStorageDriver, path: &str) -> i64 {
    driver
        .stat(&RelPath::new(path).expect("stat path"))
        .await
        .expect("seeded stat")
        .id
        .handle
        .as_str()
        .parse()
        .expect("mock handles are numeric")
}

// -------------------------------------------------------------- tests ---

#[tokio::test]
async fn rebuild_walks_the_backend_tree_into_db_rows() {
    let driver = seeded_driver();
    // A tree: two top-level files, one directory with a nested file
    // (the writer auto-creates the parent dir in the mock backend).
    seed_file(&driver, "hello.txt", b"hello").await;
    seed_file(&driver, "big.bin", &[7u8; 100]).await;
    seed_file(&driver, "docs/readme.md", b"# doc").await;
    let hello_id = handle_of(&driver, "hello.txt").await;
    let big_id = handle_of(&driver, "big.bin").await;
    let readme_id = handle_of(&driver, "docs/readme.md").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let outcome = rebuild_from_backend(&driver, &db, &RelPath::root())
        .await
        .expect("rebuild walks the tree");
    assert_eq!(
        outcome,
        RebuildOutcome { files: 3, dirs: 1 },
        "three files and the docs/ directory"
    );

    // File rows: authoritative-index shape.
    let hello = db.get_file("/hello.txt").expect("read hello").expect("row");
    assert_eq!(hello.size, 5);
    assert!(hello.is_uploaded, "backend bytes exist → is_uploaded=1");
    assert_eq!(hello.chunk_count, 1, "whole-file view above L2");
    assert_eq!(hello.telegram_msg_id, Some(hello_id), "msg_id = handle");
    assert!(!hello.is_encrypted);
    assert!(hello.mtime > 0.0, "mtime from the Entry");
    assert_eq!(hello.name, "hello.txt");
    assert_eq!(hello.parent_dir, "/");

    let big = db.get_file("/big.bin").expect("read big").expect("row");
    assert_eq!(big.size, 100);
    assert_eq!(big.telegram_msg_id, Some(big_id));

    // Nested file carries the directory parent_dir.
    let readme = db
        .get_file("/docs/readme.md")
        .expect("read readme")
        .expect("row");
    assert_eq!(readme.size, 5, "b\"# doc\" is five bytes");
    assert_eq!(readme.telegram_msg_id, Some(readme_id));
    assert_eq!(readme.parent_dir, "/docs");

    // Directory row: create_dir parity (0-size, 0 chunks, NULL msg_id).
    let docs = db.get_file("/docs").expect("read docs").expect("row");
    assert!(docs.is_dir);
    assert_eq!(docs.size, 0);
    assert_eq!(docs.chunk_count, 0);
    assert_eq!(docs.telegram_msg_id, None);
    assert!(docs.is_uploaded);
}

#[tokio::test]
async fn rebuild_upserts_over_stale_rows_and_reports_counts() {
    let driver = seeded_driver();
    seed_file(&driver, "a.txt", b"aaa").await;
    let a_id = handle_of(&driver, "a.txt").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    // A stale pending row (crash-staged upload that never drained) and
    // a ghost row for a file the backend no longer has — the rebuild
    // refreshes the first and leaves the second untouched (K11 scope:
    // bootstrap rows from the backend, no pruning).
    db.upsert_file(&cloudkit_core::database::FileUpsert {
        rel_path: "/a.txt".to_string(),
        name: "a.txt".to_string(),
        parent_dir: "/".to_string(),
        size: 3,
        mtime: 1.0,
        sha256: Some("stale-hash".to_string()),
        is_dir: false,
        telegram_msg_id: Some(999),
        is_uploaded: false,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed stale row");
    db.upsert_file(&cloudkit_core::database::FileUpsert {
        rel_path: "/ghost.txt".to_string(),
        name: "ghost.txt".to_string(),
        parent_dir: "/".to_string(),
        size: 1,
        mtime: 1.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(555),
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed ghost row");

    let outcome = rebuild_from_backend(&driver, &db, &RelPath::root())
        .await
        .expect("rebuild");
    assert_eq!(outcome.files, 1, "one backend file");

    // The stale row is refreshed to the backend truth: uploaded, the
    // backend's handle — while the stored sha256 survives (coalesce).
    let a = db.get_file("/a.txt").expect("read a").expect("row");
    assert!(a.is_uploaded, "pending row refreshed to uploaded");
    assert_eq!(a.telegram_msg_id, Some(a_id), "msg_id refreshed");
    assert_eq!(
        a.sha256.as_deref(),
        Some("stale-hash"),
        "coalesce keeps sha"
    );

    // Ghost row untouched (no pruning in K11).
    let ghost = db.get_file("/ghost.txt").expect("read ghost").expect("row");
    assert_eq!(ghost.telegram_msg_id, Some(555), "ghost row survives");
}

#[tokio::test]
async fn rebuild_of_an_empty_backend_is_ok_with_zero_rows() {
    let driver = seeded_driver();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = MetaDatabase::open(&dir.path().join("meta.db")).expect("open db");
    let outcome = rebuild_from_backend(&driver, &db, &RelPath::root())
        .await
        .expect("empty backend is a legitimate state");
    assert_eq!(outcome, RebuildOutcome { files: 0, dirs: 0 });
}

#[test]
fn encrypted_instances_are_refused_with_sync_guidance() {
    // A mock config with encryption on: the pure gate refuses with an
    // actionable message pointing at cydrive sync (K11 plaintext-only).
    let encrypted = CyDriveConfig {
        enable_encryption: true,
        encryption_password: Some("pw".to_string()),
        ..CyDriveConfig::default()
    };
    let err =
        ensure_plaintext_instance(&encrypted).expect_err("encrypted instance must be refused");
    let message = err.to_string();
    assert!(
        message.contains("sync"),
        "the refusal must point at cydrive sync, got: {message}"
    );
    assert!(
        message.contains("encrypt"),
        "the refusal must name encryption as the reason, got: {message}"
    );

    // Plaintext instances (the default) pass the gate.
    ensure_plaintext_instance(&CyDriveConfig::default())
        .expect("plaintext instance passes the gate");
}
