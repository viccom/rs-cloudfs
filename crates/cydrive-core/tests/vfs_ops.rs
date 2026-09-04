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
//! Telegram messages (Python parity), `cache_clear` (plan revision A1)
//! deletes only the cached copies of *uploaded* files — pending uploads
//! keep their local copies, the only copy of the bytes — and clears
//! `is_cached` on uploaded file rows only, and `ingest_file`
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

/// Bounded wait for a mid-test drain. `Vfs::shutdown` cannot be used to
/// drain file A here — it closes the queue, and file B must still be
/// enqueuable afterwards — so the test polls the terminal state the
/// worker writes in order (row persisted -> local copy deleted ->
/// success counter bumped): the counter must have reached `expected`
/// and the copy must be gone, which guarantees the following hydrate
/// really re-downloads instead of hitting the local copy.
async fn wait_for_drained_uploads(vfs: &Vfs, paths: &CacheManager, rel: &RelPath, expected: u64) {
    for _ in 0..2500 {
        if vfs.queue_stats().succeeded >= expected && !paths.local_path(rel).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the queue never drained {expected} upload(s) for {rel}");
}

/// 6 (plan revision A1): cache_clear spares pending uploads. File A is
/// put and drained (uploaded), then re-hydrated so it owns a cache copy
/// again (`is_cached = 1`, `is_uploaded = 1`). File B is only put — it
/// stays pending (`is_uploaded = 0`) with its local copy as the data's
/// only copy — and a directory row sits alongside. cache_clear must then
/// delete only the uploaded file's copy and clear only its flag (return
/// value 1), while B's copy, B's flags and the directory row's flag stay
/// untouched; the subsequent shutdown drains B to a successful upload,
/// proving the preserved copy was the real, uploadable staging file.
#[tokio::test]
async fn cache_clear_preserves_pending_uploads_and_clears_uploaded_only() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock, 64);
    let rel_a = RelPath::new("/uploaded.txt").expect("valid rel path");
    let rel_b = RelPath::new("/sub/pending.txt").expect("valid rel path");
    let dir_rel = RelPath::new("/docs").expect("valid rel path");

    // File A: put + drain (the success path persists the row, deletes the
    // cache copy and bumps the counter, in that order), then hydrate the
    // copy back from the mock remote.
    vfs.put(&rel_a, b"uploaded", 1_700_000_000.0)
        .await
        .expect("put accepted");
    wait_for_drained_uploads(&vfs, &paths, &rel_a, 1).await;
    vfs.hydrate(&rel_a).await.expect("hydrate file A back");
    let local_a = paths.local_path(&rel_a);
    assert!(local_a.exists(), "file A owns a cache copy again");
    let row_a = db
        .get_file("/uploaded.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row_a.is_uploaded, "file A finished uploading");
    assert!(row_a.is_cached, "file A is cached after the hydrate");

    // File B: put only. Nothing is awaited from here until after
    // cache_clear (current-thread runtime, immediately-ready enqueue),
    // so the worker is never polled and B stays pending with its local
    // copy as the only copy of the bytes.
    vfs.put(&rel_b, b"pending", 1_700_000_000.0)
        .await
        .expect("put accepted");
    let local_b = paths.local_path(&rel_b);
    assert!(
        local_b.exists(),
        "file B's pending copy is in the cache tree"
    );
    vfs.create_dir(&dir_rel).expect("create_dir accepted");

    let cleared = vfs.cache_clear().expect("cache_clear accepted");
    assert_eq!(
        cleared, 1,
        "only the uploaded file row had its flag cleared; the pending row's flag survives"
    );

    assert!(
        !local_a.exists(),
        "the uploaded file's cache copy is deleted"
    );
    assert!(
        local_b.exists(),
        "the pending upload's local copy is preserved (it is the only copy)"
    );

    let row_a = db
        .get_file("/uploaded.txt")
        .expect("db read")
        .expect("row kept");
    assert!(!row_a.is_cached, "the uploaded row's flag is cleared");
    assert!(row_a.is_uploaded, "the uploaded row stays uploaded");
    let row_b = db
        .get_file("/sub/pending.txt")
        .expect("db read")
        .expect("row kept");
    assert!(row_b.is_cached, "the pending row's flag is untouched");
    assert!(!row_b.is_uploaded, "the pending row is still pending");
    let dir_row = db.get_file("/docs").expect("db read").expect("row kept");
    assert!(dir_row.is_cached, "the directory row flag is untouched");

    // The preserved copy is the real staging file: the drain uploads B.
    vfs.shutdown().await;
    let row_b = db
        .get_file("/sub/pending.txt")
        .expect("db read")
        .expect("row kept");
    assert!(
        row_b.is_uploaded,
        "the preserved pending copy completed its upload during the shutdown drain"
    );
    assert!(
        !local_b.exists(),
        "file B's cache copy is deleted after its successful upload"
    );
    assert_eq!(vfs.queue_stats().succeeded, 2, "both files uploaded");
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

// ----------------------------------------------- pending-upload guard ---

/// 9 (plan F2, review H2): `remove_file` refuses to delete a pending
/// upload whose local cache copy still exists — for such a row the copy
/// is the **only** copy of the bytes (nothing is on the remote yet), so
/// deleting the row would orphan the upload's data. The refusal is the
/// `VfsError::UploadPending` variant carrying the virtual path, and
/// both the row and the local copy survive it untouched.
///
/// Determinism mirrors test 6's file B: the put is never followed by an
/// await before the guard runs (current-thread runtime, immediately-
/// ready enqueue, and the guard is a synchronous prefix of
/// `remove_file`), so the row is deterministically still pending when
/// the guard reads it.
#[tokio::test]
async fn remove_file_refuses_pending_upload_with_local_copy() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock, 64);
    let rel = RelPath::new("/uploading.txt").expect("valid rel path");

    // put without draining: the row is pending and the cache tree holds
    // the only copy of the bytes.
    vfs.put(&rel, b"only local copy", 1_700_000_000.0)
        .await
        .expect("put accepted");
    let local = paths.local_path(&rel);
    assert!(local.exists(), "the pending copy is in the cache tree");

    match vfs.remove_file(&rel).await {
        Err(VfsError::UploadPending(path)) => assert_eq!(path, "/uploading.txt"),
        other => panic!("expected UploadPending(/uploading.txt), got: {other:?}"),
    }

    let row = db
        .get_file("/uploading.txt")
        .expect("db read")
        .expect("the row survives the refused delete");
    assert!(!row.is_uploaded, "the row is still a pending upload");
    assert!(
        local.exists(),
        "the only copy of the bytes survives the refused delete"
    );

    vfs.shutdown().await;
}

/// 10 (plan F2 ghost-row refinement): a pending row whose local copy
/// has vanished is a ghost — the bytes are neither local nor remote —
/// and MUST stay deletable, otherwise it could never be cleaned up.
#[tokio::test]
async fn remove_file_allows_ghost_pending_row() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock, 64);
    let rel = RelPath::new("/ghost.bin").expect("valid rel path");

    vfs.put(&rel, b"ghost bytes", 1_700_000_000.0)
        .await
        .expect("put accepted");
    let local = paths.local_path(&rel);
    assert!(local.exists(), "the pending copy is in the cache tree");

    // Vanish the copy out from under the pending row (a sync std call —
    // no await, the worker stays unpolled and the row stays pending).
    fs::remove_file(&local).expect("delete the local copy");

    vfs.remove_file(&rel)
        .await
        .expect("a ghost pending row is deletable");
    assert!(
        db.get_file("/ghost.bin").expect("db read").is_none(),
        "the ghost row is gone"
    );

    vfs.shutdown().await;
}
