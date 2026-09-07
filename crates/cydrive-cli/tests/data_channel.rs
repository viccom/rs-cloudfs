//! RED-phase tests for the Tier-1 CLI data channel (plan
//! `docs/plans/2026-09-03-tier1-utilities.md`, contracts C7 + C11,
//! Task 4): the `push` / `pull` library bodies and the `cache clear`
//! runner.
//!
//! Contract under test:
//!
//! * `push_file(&Vfs, &Path, &RelPath)` — creates the missing ancestor
//!   directory rows (deepest-first, `Exists` swallowed), streams the
//!   source into the upload queue and returns the source size;
//! * `pull_file(&Vfs, &RelPath, &Path)` — hydrates the drive file (a
//!   cold cache re-downloads from the remote) and copies into `out`;
//!   an existing-directory `out` receives the rel path's basename;
//! * `cache_clear_cmd(&CyDriveConfig)` — opens the db + cache manager
//!   off the config alone (no transport, so tests call it directly),
//!   deletes the cached copies of uploaded files and clears their
//!   `is_cached` flags (plan revision A1: pending-upload staging
//!   copies are preserved);
//! * `vfs_config` — maps the new tuning keys (`upload_workers` /
//!   `queue_capacity` / `hydrate_timeout_secs`) onto the corresponding
//!   `VfsConfig` fields instead of the hard-coded 2 / 256 / 180s.
//!
//! Assembly copies the core `tests/vfs_ops.rs` pattern: real temp
//! SQLite db + mirrored cache tree + pre-connected `MockTransport`,
//! one worker, fast retry. The transport-injection seam already covers
//! the full stack wiring (`tests/run_e2e.rs`); `connect_stack` itself
//! needs a real Telegram connect and is deliberately never called
//! here — these tests exercise the library functions against a
//! hand-assembled Vfs.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::CloudTransport;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cydrive_cli::{cache_clear_cmd, pull_file, push_file, vfs_config};

// ------------------------------------------------------------- helpers ---

/// VfsConfig for integration tests: one worker (deterministic order),
/// capacity 16, fast retry (1ms/2ms, degrade after 3), no encryption.
fn test_cfg(chunk_size_bytes: u64) -> VfsConfig {
    VfsConfig {
        chunk_size_bytes,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// Real temp environment: SQLite db + cache tree + pre-connected mock
/// transport + the assembled Vfs (queue workers spawn inside).
/// `cache_root` is remembered separately for assertion-side path math.
async fn test_env(
    cache_limit: u64,
    chunk_size_bytes: u64,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    PathBuf,
    Arc<MockTransport>,
    Vfs,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(cache_root.clone(), cache_limit);
    let mock = Arc::new(MockTransport::new());
    // The queue workers and hydrate never call connect(); pre-connect
    // the shared mock so its upload/download gates are open.
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(
        Arc::clone(&db),
        cache,
        transport,
        test_cfg(chunk_size_bytes),
    );
    (dir, db, cache_root, mock, vfs)
}

/// Bounded wait for a mid-test drain (the queue closes on shutdown, so
/// tests that keep using the Vfs poll instead): the worker writes the
/// terminal state in order (row persisted -> local copy deleted ->
/// success counter bumped), so `succeeded >= expected` plus the copy
/// being gone guarantees the upload really finished.
async fn wait_until_uploaded(vfs: &Vfs, rel: &RelPath, cache_root: &Path, expected: u64) {
    let paths = CacheManager::new(cache_root.to_path_buf(), u64::MAX);
    for _ in 0..2500 {
        if vfs.queue_stats().succeeded >= expected && !paths.local_path(rel).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the queue never drained to {expected} succeeded upload(s) for {rel}");
}

/// Recursively collects every regular file under `dir` (a local mirror
/// of the private cache walker).
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Drops the cached copy of `rel`, if any — the explicit "make this
/// cold" step so the following pull must hydrate from the remote.
fn evict_copy(cache_root: &Path, rel: &RelPath) {
    let paths = CacheManager::new(cache_root.to_path_buf(), u64::MAX);
    let _ = fs::remove_file(paths.local_path(rel));
}

// ---------------------------------------------------------------- push ---

/// 1. Happy path: pushing a small local file returns the source size,
///    and once the queue drains the mock remote holds the file's
///    document (single chunk -> the plain basename) and the db row is
///    marked uploaded at the source size.
#[tokio::test]
async fn push_file_enqueues_and_uploads_to_mock() {
    let (dir, db, _cache_root, mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let source = dir.path().join("source.txt");
    fs::write(&source, b"push channel payload").expect("write source file");
    let dest = RelPath::new("/report.txt").expect("valid dest");

    let pushed = push_file(&vfs, &source, &dest)
        .await
        .expect("push accepted");
    assert_eq!(
        pushed,
        b"push channel payload".len() as u64,
        "the source size is returned"
    );

    vfs.shutdown().await; // deterministic drain to the terminal state

    let names = mock.message_names();
    assert!(
        names.iter().any(|name| name == "report.txt"),
        "the mock remote holds the pushed file's document: {names:?}"
    );
    let row = db
        .get_file("/report.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "the push drained to an uploaded row");
    assert_eq!(row.size, b"push channel payload".len() as i64);
}

/// 2. Pushing into a path whose ancestors have no rows creates them
///    deepest-first: `/a/b/c.txt` materialises directory rows for both
///    `/a` (listed under `/`) and `/a/b` (listed under `/a`).
#[tokio::test]
async fn push_file_creates_missing_ancestor_rows() {
    let (dir, db, _cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let source = dir.path().join("c.txt");
    fs::write(&source, b"nested").expect("write source file");
    let dest = RelPath::new("/a/b/c.txt").expect("valid dest");

    push_file(&vfs, &source, &dest)
        .await
        .expect("push accepted");

    let root = db.list_dir("/").expect("list /");
    let a = root
        .iter()
        .find(|entry| entry.rel_path == "/a")
        .expect("ancestor row /a exists");
    assert!(a.is_dir, "/a is a directory row: {a:?}");
    let under_a = db.list_dir("/a").expect("list /a");
    let b = under_a
        .iter()
        .find(|entry| entry.rel_path == "/a/b")
        .expect("ancestor row /a/b exists");
    assert!(b.is_dir, "/a/b is a directory row: {b:?}");

    vfs.shutdown().await;
}

/// 3. Pushing a source that does not exist fails and leaves no trace:
///    no files row is created for the destination.
#[tokio::test]
async fn push_file_missing_source_errors() {
    let (dir, db, _cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let missing = dir.path().join("does-not-exist.bin");
    let dest = RelPath::new("/ghost.bin").expect("valid dest");

    let result = push_file(&vfs, &missing, &dest).await;
    assert!(
        result.is_err(),
        "a missing source must error, got: {result:?}"
    );

    assert!(
        db.get_file("/ghost.bin").expect("db read").is_none(),
        "no row for the failed push"
    );

    vfs.shutdown().await;
}

// ---------------------------------------------------------------- pull ---

/// 4. Round-trip: push a distinctive payload, drain the upload, evict
///    the local copy (cold cache) and pull — the target file's bytes
///    equal the original payload because the pull hydrated the content
///    back from the mock remote.
#[tokio::test]
async fn pull_file_roundtrips_bytes() {
    let (dir, _db, cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let payload: Vec<u8> = (0..256u32).map(|i| (i % 251) as u8).collect();
    let source = dir.path().join("cold.bin");
    fs::write(&source, &payload).expect("write source file");
    let rel = RelPath::new("/cold.bin").expect("valid rel");

    push_file(&vfs, &source, &rel).await.expect("push accepted");
    wait_until_uploaded(&vfs, &rel, &cache_root, 1).await;
    evict_copy(&cache_root, &rel); // the drain already removed it; belt and braces

    let out = dir.path().join("pulled.bin");
    let pulled_to = pull_file(&vfs, &rel, &out).await.expect("pull accepted");
    assert_eq!(
        pulled_to, out,
        "a non-directory out is the target file itself"
    );
    assert_eq!(
        fs::read(&out).expect("read pulled file"),
        payload,
        "bytes round-trip through the remote"
    );

    vfs.shutdown().await;
}

/// 5. Pulling with `out` an existing directory lands the file under
///    that directory using the rel path's basename, and returns the
///    joined path.
#[tokio::test]
async fn pull_file_into_existing_directory_uses_file_name() {
    let (dir, _db, cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let payload = b"into a directory".to_vec();
    let source = dir.path().join("blob.bin");
    fs::write(&source, &payload).expect("write source file");
    let rel = RelPath::new("/blob.bin").expect("valid rel");

    push_file(&vfs, &source, &rel).await.expect("push accepted");
    wait_until_uploaded(&vfs, &rel, &cache_root, 1).await;
    evict_copy(&cache_root, &rel);

    let out_dir = dir.path().join("outdir");
    fs::create_dir(&out_dir).expect("create output directory");

    let pulled_to = pull_file(&vfs, &rel, &out_dir)
        .await
        .expect("pull accepted");
    let expected = out_dir.join("blob.bin");
    assert_eq!(
        pulled_to, expected,
        "the basename lands inside the existing directory"
    );
    assert_eq!(
        fs::read(&expected).expect("read pulled file"),
        payload,
        "the pulled copy carries the source bytes"
    );

    vfs.shutdown().await;
}

/// 6. Pulling a rel path with no row errors and writes no output file.
#[tokio::test]
async fn pull_file_missing_rel_errors() {
    let (dir, _db, _cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let rel = RelPath::new("/never-existed.bin").expect("valid rel");
    let out = dir.path().join("out.bin");

    let result = pull_file(&vfs, &rel, &out).await;
    assert!(
        result.is_err(),
        "a missing drive path must error, got: {result:?}"
    );
    assert!(!out.exists(), "no output file may appear for a failed pull");

    vfs.shutdown().await;
}

// ---------------------------------------------------------- cache clear ---

/// 7. The `cache clear` runner against a real db + cache tree: two
///    files are pushed, drained and hydrated back (both own a cache
///    copy with `is_cached = 1` — the state clear must actually undo);
///    `cache_clear_cmd` then empties the cache tree and clears both
///    flags while the rows stay uploaded. It opens the db and cache
///    manager from the config alone, so the test calls it directly
///    with a temp-rooted config — no transport involved.
#[tokio::test]
async fn cache_clear_frees_files_and_flags() {
    let (dir, db, cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);

    let one = RelPath::new("/one.txt").expect("valid rel");
    let two = RelPath::new("/two.txt").expect("valid rel");
    let source_one = dir.path().join("one.txt");
    let source_two = dir.path().join("two.txt");
    fs::write(&source_one, b"one").expect("write source one");
    fs::write(&source_two, b"two").expect("write source two");

    push_file(&vfs, &source_one, &one).await.expect("push one");
    wait_until_uploaded(&vfs, &one, &cache_root, 1).await;
    vfs.hydrate(&one).await.expect("hydrate one back");
    push_file(&vfs, &source_two, &two).await.expect("push two");
    wait_until_uploaded(&vfs, &two, &cache_root, 2).await;
    vfs.hydrate(&two).await.expect("hydrate two back");

    // Preconditions: both cached copies present, both flags set.
    assert!(paths.local_path(&one).exists(), "file one owns a copy");
    assert!(paths.local_path(&two).exists(), "file two owns a copy");
    assert!(
        db.get_file("/one.txt")
            .expect("db read")
            .expect("row one")
            .is_cached
    );
    assert!(
        db.get_file("/two.txt")
            .expect("db read")
            .expect("row two")
            .is_cached
    );

    vfs.shutdown().await;

    let cfg = CyDriveConfig {
        db_path: dir.path().join("meta.db").to_string_lossy().into_owned(),
        cache_path: cache_root.to_string_lossy().into_owned(),
        ..CyDriveConfig::default()
    };
    cache_clear_cmd(&cfg).expect("cache clear runs");

    let mut remaining = Vec::new();
    collect_files(&cache_root, &mut remaining);
    assert!(
        remaining.is_empty(),
        "the cache tree holds no files anymore: {remaining:?}"
    );
    for rel in ["/one.txt", "/two.txt"] {
        let row = db.get_file(rel).expect("db read").expect("row kept");
        assert!(!row.is_cached, "{rel} had its flag cleared");
        assert!(row.is_uploaded, "{rel} stays uploaded");
    }
}

// ------------------------------------------------------- tuning mapping ---

/// 8. `vfs_config` maps the tier-1 tuning keys (contract C7): the
///    config's `upload_workers` / `queue_capacity` /
///    `hydrate_timeout_secs` reach the corresponding `VfsConfig`
///    fields instead of the hard-coded 2 / 256 / 180s defaults.
#[test]
fn vfs_config_maps_new_tuning_keys() {
    let cfg = CyDriveConfig {
        upload_workers: 4,
        queue_capacity: 512,
        hydrate_timeout_secs: 300,
        ..CyDriveConfig::default()
    };

    let vc = vfs_config(&cfg);

    assert_eq!(vc.workers, 4, "upload_workers reaches VfsConfig.workers");
    assert_eq!(
        vc.queue_capacity, 512,
        "queue_capacity reaches VfsConfig.queue_capacity"
    );
    assert_eq!(
        vc.hydrate_timeout,
        Duration::from_secs(300),
        "hydrate_timeout_secs reaches VfsConfig.hydrate_timeout"
    );
}

// ------------------------------------- tier-1 review additions (F4/L) ---

/// 9. (review F4) Pushing a directory as the source is rejected up
///    front: the error names the directory nature of the source, and
///    neither the destination file row nor the missing-ancestor
///    directory rows linger in the db afterwards. The dest name avoids
///    the word "directory" so the message assertion can only pass on
///    the real gate message, not on the echoed path.
#[tokio::test]
async fn push_file_rejects_directory_source() {
    let (dir, db, _cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let source_dir = dir.path().join("subdir");
    fs::create_dir(&source_dir).expect("create source directory");
    let dest = RelPath::new("/a/b/payload.bin").expect("valid dest");

    let result = push_file(&vfs, &source_dir, &dest).await;
    let err = result.expect_err("a directory source must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("directory"),
        "the error must name the directory nature of the source: {err:#}"
    );

    assert!(
        db.get_file("/a/b/payload.bin").expect("db read").is_none(),
        "no row for the rejected push"
    );
    let root = db.list_dir("/").expect("list /");
    assert!(
        !root.iter().any(|entry| entry.rel_path == "/a"),
        "no ancestor directory row /a may linger: {root:?}"
    );

    vfs.shutdown().await;
}

/// 10. (review F4) Pulling onto an existing file overwrites it
///     wholesale: a stale, longer file at the exact output path is
///     replaced by the payload bytes (no merge, no leftovers).
#[tokio::test]
async fn pull_file_overwrites_existing_file() {
    let (dir, _db, cache_root, _mock, vfs) = test_env(1 << 20, 64 * 1024).await;
    let payload = b"fresh bytes".to_vec();
    let source = dir.path().join("fresh.bin");
    fs::write(&source, &payload).expect("write source file");
    let rel = RelPath::new("/fresh.bin").expect("valid rel");

    push_file(&vfs, &source, &rel).await.expect("push accepted");
    wait_until_uploaded(&vfs, &rel, &cache_root, 1).await;
    evict_copy(&cache_root, &rel); // force the pull through hydration

    let out = dir.path().join("overwritten.bin");
    fs::write(
        &out,
        b"stale contents that are longer than the fresh payload and must vanish",
    )
    .expect("pre-create a stale output file");

    let pulled_to = pull_file(&vfs, &rel, &out).await.expect("pull accepted");
    assert_eq!(pulled_to, out, "the exact file path is the target");
    assert_eq!(
        fs::read(&out).expect("read pulled file"),
        payload,
        "the stale file is overwritten with the source bytes"
    );

    vfs.shutdown().await;
}
