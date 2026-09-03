//! RED-phase tests for the Tier-1 VFS operations contract (plan
//! `docs/plans/2026-09-03-tier1-utilities.md`, contract table C1–C5 +
//! C10, Task 1): `MetaDatabase::clear_cached_flags` (exercised through
//! `Vfs::cache_clear`), the `VfsError::Exists` / `ParentMissing`
//! variants, `Vfs::create_dir`, `Vfs::remove_file`, `Vfs::cache_clear`
//! and `Vfs::ingest_file`.
//!
//! Semantics mirror the WebDAV adapter's mutation paths
//! (`crates/cydrive-webdav/src/lib.rs`) but surface the core error enum:
//! `create_dir` upserts a directory *row* only (no filesystem directory —
//! those are created lazily by put/hydrate), `remove_file` deletes the
//! row and the local cache copy while deliberately keeping the remote
//! Telegram messages (Python parity), `cache_clear` empties the cache
//! tree and clears `is_cached` on file rows only, and `ingest_file`
//! streams a local source file through a staged cache sibling into the
//! upload pipeline without ever reading it whole into memory.
//!
//! Assembly copies the established `tests/vfs.rs` pattern: real temp
//! SQLite db + mirrored cache tree + pre-connected `MockTransport`, one
//! worker, fast retry. The queue is drained with `vfs.shutdown().await`
//! (the existing deterministic wait). Determinism note: `#[tokio::test]`
//! runs a current-thread runtime, and `put` completes in a single poll
//! (sync I/O + an immediately-ready channel send), so between back-to-
//! back puts the worker is never polled — the "flags are still set"
//! assertions before `cache_clear` are scheduling-deterministic.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cydrive_core::cache::CacheManager;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::rel_path::RelPath;
use cydrive_core::transport::mock::MockTransport;
use cydrive_core::transport::CloudTransport;
use cydrive_core::upload_queue::RetryPolicy;
use cydrive_core::vfs::{Vfs, VfsConfig, VfsError};

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

/// Real temp environment: SQLite db + mirrored cache tree + pre-connected
/// mock transport. `cache_root` is remembered separately because
/// `Vfs::new` consumes the `CacheManager`; assertion sides re-open the
/// root for pure path math. The first tuple item keeps the temp dir
/// alive.
async fn test_env(
    cache_limit: u64,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    PathBuf,
    Arc<MockTransport>,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(cache_root.clone(), cache_limit);
    let mock = Arc::new(MockTransport::new());
    // The Vfs/queue never calls connect(); pre-connect the shared mock so
    // its upload/download gates are open for the workers and hydrate.
    mock.connect().await.expect("pre-connect mock transport");
    (dir, db, cache, cache_root, mock)
}

/// Assembles the VFS over the shared environment pieces.
fn build_vfs(
    db: &Arc<MetaDatabase>,
    cache: CacheManager,
    mock: &Arc<MockTransport>,
    chunk_size_bytes: u64,
) -> Vfs {
    let transport: Arc<dyn CloudTransport> = mock.clone();
    Vfs::new(db.clone(), cache, transport, test_cfg(chunk_size_bytes))
}

/// Inserts a bare `files` row at `rel` shaped like the create_dir
/// directory row when `is_dir` (born uploaded + cached), a plain row
/// otherwise — enough for the existence/parent checks under test.
fn seed_row(db: &MetaDatabase, rel: &str, is_dir: bool) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent_dir = rel_path
        .parent()
        .map(|parent| parent.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir,
        size: 0,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir,
        telegram_msg_id: None,
        is_uploaded: is_dir,
        is_cached: is_dir,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
    })
    .expect("seed files row");
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

// ---------------------------------------------------------- create_dir ---

/// 1. Happy path: a root-level create upserts a directory row with the
///    contract's exact shape (is_dir, size 0, uploaded + cached flags
///    set, zero chunks, no msg id / crypto / mime metadata, parent `/`)
///    and the row shows up as a child of `/` in `list_dir`. No
///    filesystem directory is created (fs dirs are lazy).
#[tokio::test]
async fn create_dir_happy_path_creates_dir_row() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let vfs = build_vfs(&db, cache, &mock, 64);
    let rel = RelPath::new("/docs").expect("valid rel path");

    vfs.create_dir(&rel).expect("create_dir accepted");

    let row = db.get_file("/docs").expect("db read").expect("row exists");
    assert!(row.is_dir, "a directory row was created");
    assert_eq!(row.size, 0, "directory rows are zero-sized");
    assert!(row.is_uploaded, "directory rows are born uploaded");
    assert!(row.is_cached, "directory rows are born cached");
    assert_eq!(row.chunk_count, 0, "directories have no chunks");
    assert_eq!(row.telegram_msg_id, None, "directories carry no msg id");
    assert!(!row.is_encrypted, "directories are never encrypted");
    assert_eq!(row.mime_type, None, "directories carry no mime type");
    assert_eq!(row.parent_dir, "/", "parent derived from the rel path");

    let listing = db.list_dir("/").expect("db read");
    assert!(
        listing.iter().any(|entry| entry.rel_path == "/docs"),
        "list_dir(/) contains the new directory row: {listing:?}"
    );

    vfs.shutdown().await;
}

/// 2. Parent enforcement: a nested create without a parent row fails
///    with `ParentMissing(parent)`, and so does a create under a parent
///    that exists but is a plain file row.
#[tokio::test]
async fn create_dir_nested_requires_parent_dir_row() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let vfs = build_vfs(&db, cache, &mock, 64);

    // No row at /a at all.
    let orphan = RelPath::new("/a/b").expect("valid rel path");
    match vfs.create_dir(&orphan) {
        Err(VfsError::ParentMissing(parent)) => assert_eq!(parent, "/a"),
        other => panic!("expected ParentMissing(/a), got: {other:?}"),
    }

    // /f exists but is a file, not a directory.
    seed_row(&db, "/f", false);
    let under_file = RelPath::new("/f/sub").expect("valid rel path");
    match vfs.create_dir(&under_file) {
        Err(VfsError::ParentMissing(parent)) => assert_eq!(parent, "/f"),
        other => panic!("expected ParentMissing(/f), got: {other:?}"),
    }

    vfs.shutdown().await;
}

/// 3. Collision rejection: the root itself, an existing file row and an
///    existing directory row all answer `Exists(path)` carrying the
///    virtual path.
#[tokio::test]
async fn create_dir_rejects_root_and_existing() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let vfs = build_vfs(&db, cache, &mock, 64);

    match vfs.create_dir(&RelPath::root()) {
        Err(VfsError::Exists(path)) => assert_eq!(path, "/"),
        other => panic!("expected Exists(/) for the root, got: {other:?}"),
    }

    seed_row(&db, "/dup.txt", false);
    let file_rel = RelPath::new("/dup.txt").expect("valid rel path");
    match vfs.create_dir(&file_rel) {
        Err(VfsError::Exists(path)) => assert_eq!(path, "/dup.txt"),
        other => panic!("expected Exists(/dup.txt), got: {other:?}"),
    }

    let dir_rel = RelPath::new("/docs").expect("valid rel path");
    vfs.create_dir(&dir_rel).expect("first create succeeds");
    match vfs.create_dir(&dir_rel) {
        Err(VfsError::Exists(path)) => assert_eq!(path, "/docs"),
        other => panic!("expected Exists(/docs), got: {other:?}"),
    }

    vfs.shutdown().await;
}

// ---------------------------------------------------------- remove_file ---

/// 4. remove_file on a real file: the row disappears, the local cache
///    copy is deleted, and the remote Telegram messages are deliberately
///    kept (Python parity — no delete_remote call ever fires).
#[tokio::test]
async fn remove_file_deletes_row_and_cache_copy() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock, 64);
    let rel = RelPath::new("/gone.txt").expect("valid rel path");

    vfs.put(&rel, b"bye", 1_700_000_000.0)
        .await
        .expect("put accepted");
    let local = paths.local_path(&rel);
    assert!(local.exists(), "the cache copy exists right after put");

    vfs.remove_file(&rel).await.expect("remove_file accepted");

    assert!(
        db.get_file("/gone.txt").expect("db read").is_none(),
        "the files row is gone"
    );
    assert!(!local.exists(), "the cache copy was deleted");
    assert!(
        mock.deleted().is_empty(),
        "remote Telegram messages are kept (no delete_remote call)"
    );

    vfs.shutdown().await;
}

/// 5. remove_file error surface: a missing row is `NotFound(path)` and a
///    directory row is `IsDirectory(path)` — both carrying the virtual
///    path.
#[tokio::test]
async fn remove_file_not_found_and_is_directory() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let vfs = build_vfs(&db, cache, &mock, 64);

    let missing = RelPath::new("/nope.bin").expect("valid rel path");
    match vfs.remove_file(&missing).await {
        Err(VfsError::NotFound(path)) => assert_eq!(path, "/nope.bin"),
        other => panic!("expected NotFound(/nope.bin), got: {other:?}"),
    }

    seed_row(&db, "/somedir", true);
    let dir_rel = RelPath::new("/somedir").expect("valid rel path");
    match vfs.remove_file(&dir_rel).await {
        Err(VfsError::IsDirectory(path)) => assert_eq!(path, "/somedir"),
        other => panic!("expected IsDirectory(/somedir), got: {other:?}"),
    }

    vfs.shutdown().await;
}

// ----------------------------------------------------------- cache_clear ---

/// 6. cache_clear: two cached file rows and one directory row (born
///    cached per the create_dir contract) collapse to an empty cache
///    tree — the root itself survives — the call reports exactly the two
///    file flags cleared, both file rows flip to `is_cached = 0`, and
///    the directory row's flag is untouched.
#[tokio::test]
async fn cache_clear_empties_tree_and_clears_file_flags_only() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let vfs = build_vfs(&db, cache, &mock, 64);
    let rel_a = RelPath::new("/keep1.txt").expect("valid rel path");
    let rel_b = RelPath::new("/sub/keep2.txt").expect("valid rel path");
    let dir_rel = RelPath::new("/docs").expect("valid rel path");

    vfs.put(&rel_a, b"first", 1_700_000_000.0)
        .await
        .expect("put accepted");
    vfs.put(&rel_b, b"second", 1_700_000_000.0)
        .await
        .expect("put accepted");
    vfs.create_dir(&dir_rel).expect("create_dir accepted");

    let cleared = vfs.cache_clear().expect("cache_clear accepted");
    assert_eq!(
        cleared, 2,
        "exactly the two file rows had their flag cleared"
    );

    let mut remaining = Vec::new();
    collect_files(&cache_root, &mut remaining);
    assert!(
        remaining.is_empty(),
        "no files remain under the cache root: {remaining:?}"
    );
    assert!(
        cache_root.is_dir(),
        "the cache root itself survives the clear"
    );

    let row_a = db
        .get_file("/keep1.txt")
        .expect("db read")
        .expect("row kept");
    assert!(!row_a.is_cached, "file row A flag cleared");
    let row_b = db
        .get_file("/sub/keep2.txt")
        .expect("db read")
        .expect("row kept");
    assert!(!row_b.is_cached, "file row B flag cleared");
    let dir_row = db.get_file("/docs").expect("db read").expect("row kept");
    assert!(dir_row.is_cached, "the directory row flag is untouched");

    vfs.shutdown().await;
}

// ----------------------------------------------------------- ingest_file ---

/// 7. ingest_file full cycle: a 1 MiB source file streams through the
///    staged cache sibling into the queue (never read whole into memory
///    — the copy is `tokio::fs::copy` and the upload reads from disk),
///    the call returns the source size, and after the queue drains the
///    mock remote holds the upload, the row is uploaded at the source
///    size with the passed-through mtime, the cache copy is deleted, and
///    the bytes hydrate back intact.
#[tokio::test]
async fn ingest_file_streams_into_queue_without_reading_into_memory() {
    let (dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    // 64 KiB chunks: the 1 MiB source splits into 16 planned chunks.
    let vfs = build_vfs(&db, cache, &mock, 64 * 1024);
    let rel = RelPath::new("/big/ingested.bin").expect("valid rel path");

    let source = dir.path().join("source.bin");
    let payload: Vec<u8> = (0..(1usize << 20)).map(|i| (i % 251) as u8).collect();
    fs::write(&source, &payload).expect("write 1 MiB source file");

    let ingested = vfs
        .ingest_file(&rel, &source, 1_700_000_000.0)
        .await
        .expect("ingest accepted");
    assert_eq!(
        ingested,
        payload.len() as u64,
        "the source size is returned"
    );

    vfs.shutdown().await;

    assert!(
        !mock.upload_calls().is_empty(),
        "the mock remote received the upload"
    );
    let row = db
        .get_file("/big/ingested.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "the upload finished during the drain");
    assert_eq!(row.size, payload.len() as i64, "row keeps the source size");
    assert_eq!(row.mtime, 1_700_000_000.0, "mtime passed through");
    assert!(
        !paths.local_path(&rel).exists(),
        "cache copy deleted after the successful upload"
    );
    assert_eq!(vfs.queue_stats().succeeded, 1);

    let hydrated = vfs.hydrate(&rel).await.expect("hydrate after upload");
    assert_eq!(
        fs::read(hydrated).expect("read hydrated copy"),
        payload,
        "bytes streamed to the remote and back intact"
    );
}

/// 8. ingest_file with a missing source: the metadata probe fails before
///    anything else happens — the error surfaces as `VfsError::Io`, no
///    files row is created, and no staging file is left anywhere in the
///    cache tree.
#[tokio::test]
async fn ingest_file_missing_source_errors_and_leaves_no_staging() {
    let (dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock, 64);
    let rel = RelPath::new("/ghost.bin").expect("valid rel path");
    let missing = dir.path().join("does-not-exist.bin");

    let result = vfs.ingest_file(&rel, &missing, 1_700_000_000.0).await;
    assert!(
        matches!(result, Err(VfsError::Io(_))),
        "expected Io for the missing source, got: {result:?}"
    );

    assert!(
        db.get_file("/ghost.bin").expect("db read").is_none(),
        "no files row is created for a failed ingest"
    );
    assert!(!paths.local_path(&rel).exists(), "no cache copy appeared");
    let mut remaining = Vec::new();
    collect_files(&cache_root, &mut remaining);
    assert!(
        remaining.is_empty(),
        "no staging file left in the cache tree: {remaining:?}"
    );

    vfs.shutdown().await;
}
