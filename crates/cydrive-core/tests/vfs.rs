//! RED-phase tests for `cydrive_core::vfs`. All bodies are expected to
//! panic with "not yet implemented" until the GREEN phase lands.
//!
//! Contract under test (design doc «关键数据流» upload/download paths +
//! compat contracts 6/7): `put` stages through a `.tmp` file + atomic
//! rename (no half-written cache copy is ever visible, fixing the Python
//! direct-write defect), flips the row to pending+cached and hands off to
//! the fire-and-forget queue (0-byte puts skip the transport); `hydrate`
//! prefers the local cache, otherwise downloads — merging chunks,
//! decrypting encrypted rows into a plaintext cache copy — after LRU
//! eviction that clears evicted rows' `is_cached` flag while preserving
//! every other field. The remote-dependent span of a hydration is
//! bounded by `hydrate_timeout` (Python parity: the WebDAV thread's 180s
//! `future.result` cap); cache hits bypass the bound.
//!
//! Determinism note: `#[tokio::test]` runs a current-thread runtime, so
//! between `put(...).await` returning and the next await the upload
//! worker cannot be polled — the "immediately after put" assertions are
//! scheduling-deterministic, not timing-based.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cydrive_core::cache::CacheManager;
use cydrive_core::crypto::encrypt;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::rel_path::RelPath;
use cydrive_core::transport::mock::MockTransport;
use cydrive_core::transport::{CloudTransport, UploadJob, UploadReceipt};
use cydrive_core::upload_queue::RetryPolicy;
use cydrive_core::vfs::{Vfs, VfsConfig, VfsError};

/// VfsConfig for integration tests: tiny chunk size (multi-chunk at byte
/// scales), one worker (deterministic order), capacity 16, fast retry
/// (1ms/2ms, degrade after 3), optional encryption password.
fn test_cfg(chunk_size_bytes: u64, encryption_password: Option<&str>) -> VfsConfig {
    VfsConfig {
        chunk_size_bytes,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: encryption_password.map(str::to_string),
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// Real temp environment: SQLite db + mirrored cache tree + pre-connected
/// mock transport. `cache_root` is remembered separately because
/// `Vfs::new` consumes the `CacheManager`; assertion sides re-open the
/// root for pure path math. The first tuple item keeps the temp dir
/// alive.
async fn test_env_with_mock(
    mock: MockTransport,
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
    let mock = Arc::new(mock);
    // The Vfs/queue never calls connect(); pre-connect the shared mock so
    // its upload/download gates are open for the workers and hydrate.
    mock.connect().await.expect("pre-connect mock transport");
    (dir, db, cache, cache_root, mock)
}

/// `test_env_with_mock` with the default always-Ok mock.
async fn test_env(
    cache_limit: u64,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    PathBuf,
    Arc<MockTransport>,
) {
    test_env_with_mock(MockTransport::new(), cache_limit).await
}

/// Pushes `bytes` to the mock remote as `rel` (split at `chunk_size`) by
/// calling `CloudTransport::upload` directly; returns the receipt with
/// the allocated msg ids. The scratch file lives in a private temp dir
/// dropped once the upload has read it.
async fn seed_remote(
    mock: &Arc<MockTransport>,
    rel: &str,
    bytes: &[u8],
    chunk_count: u32,
    chunk_size: u64,
) -> UploadReceipt {
    let dir = tempfile::tempdir().expect("seed scratch dir");
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local_path = dir.path().join(rel_path.name());
    fs::write(&local_path, bytes).expect("write seed scratch file");
    mock.upload(&UploadJob {
        rel_path,
        local_path,
        size: bytes.len() as u64,
        chunk_count,
        chunk_size,
    })
    .await
    .expect("seed upload to the mock remote")
}

/// Inserts an uploaded `files` row for `rel` (`is_uploaded = true`,
/// `is_cached = false`, `telegram_msg_id` = chunk 0 of the receipt, size
/// and chunk_count derived from `bytes`); one chunk row is added per
/// receipt message (full chunks at `chunk_size`, the last one the
/// remainder).
fn seed_uploaded_row(
    db: &MetaDatabase,
    rel: &str,
    bytes: &[u8],
    is_encrypted: bool,
    receipt: &UploadReceipt,
    chunk_size: u64,
) {
    let size = bytes.len() as i64;
    let chunk_count = receipt.chunk_msg_ids.len() as i64;
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent = rel_path.parent().expect("non-root path");
    let file_id = db
        .upsert_file(&FileUpsert {
            rel_path: rel_path.as_str().to_string(),
            name: rel_path.name().to_string(),
            parent_dir: parent.as_str().to_string(),
            size,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(i64::from(receipt.first_msg_id)),
            is_uploaded: true,
            is_cached: false,
            is_encrypted,
            chunk_count,
            mime_type: None,
        })
        .expect("seed files row");
    for (index, &msg_id) in receipt.chunk_msg_ids.iter().enumerate() {
        let index = index as i64;
        let chunk_row_size = if index + 1 < chunk_count {
            chunk_size as i64
        } else {
            size - (chunk_size as i64) * (chunk_count - 1)
        };
        db.upsert_chunk(file_id, index, i64::from(msg_id), chunk_row_size, None)
            .expect("seed chunk row");
    }
}

/// The common hydrate precondition in one shot: remote bytes at the mock
/// remote plus the matching uploaded row (telegram_msg_id = chunk 0,
/// chunk_count and per-chunk rows from the receipt). Returns the receipt.
async fn seed_remote_file(
    db: &MetaDatabase,
    mock: &Arc<MockTransport>,
    rel: &str,
    bytes: &[u8],
    chunk_size: u64,
    is_encrypted: bool,
) -> UploadReceipt {
    let chunk_count = if bytes.is_empty() {
        1
    } else {
        bytes.len().div_ceil(chunk_size as usize) as u32
    };
    let receipt = seed_remote(mock, rel, bytes, chunk_count, chunk_size).await;
    seed_uploaded_row(db, rel, bytes, is_encrypted, &receipt, chunk_size);
    receipt
}

/// Writes `bytes` to the mirrored cache path of `rel` (creating parent
/// dirs); returns the local path.
fn seed_local(cache: &CacheManager, rel: &str, bytes: &[u8]) -> PathBuf {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local = cache.local_path(&rel_path);
    fs::create_dir_all(local.parent().expect("local parent dir")).expect("create cache dirs");
    fs::write(&local, bytes).expect("write cache file");
    local
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

/// 1. Full cycle: put accepts immediately (pending row + fully visible
///    cache copy), shutdown drains the upload (row uploaded, msg id
///    recorded, cache copy deleted), and hydrate round-trips the bytes
///    back from the mock remote.
#[tokio::test]
async fn put_then_upload_completes_full_cycle() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/cyc.txt").expect("valid rel path");

    vfs.put(&rel, b"full cycle", 1_700_000_000.0)
        .await
        .expect("put accepted");

    // Acceptance is immediate: pending row + fully visible cache copy.
    let row = db
        .get_file("/cyc.txt")
        .expect("db read")
        .expect("row exists");
    assert!(!row.is_uploaded, "row pending right after put");
    assert!(row.is_cached);
    assert_eq!(row.size, 10, "size recorded from the put bytes");
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    assert_eq!(
        fs::read(paths.local_path(&rel)).expect("read cache copy"),
        b"full cycle"
    );

    vfs.shutdown().await;

    let row = db
        .get_file("/cyc.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "upload finished during the drain");
    assert!(row.telegram_msg_id.is_some(), "chunk-0 msg id recorded");
    assert!(
        !paths.local_path(&rel).exists(),
        "cache copy deleted after the successful upload"
    );
    assert_eq!(vfs.queue_stats().succeeded, 1);

    let hydrated = vfs.hydrate(&rel).await.expect("hydrate after upload");
    assert_eq!(
        fs::read(hydrated).expect("read hydrated copy"),
        b"full cycle",
        "bytes came back from the mock remote"
    );
}

/// 2. 0-byte put: the transport is never called (compat contract 7), the
///    row is persisted as uploaded without a msg id, and the empty local
///    file is removed.
#[tokio::test]
async fn put_zero_byte_skips_transport() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/empty.txt").expect("valid rel path");

    vfs.put(&rel, b"", 1_700_000_000.0)
        .await
        .expect("put accepted");
    vfs.shutdown().await;

    assert!(
        mock.upload_calls().is_empty(),
        "0-byte put never touches the transport"
    );
    let row = db
        .get_file("/empty.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "0-byte counts as uploaded");
    assert_eq!(row.telegram_msg_id, None);
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    assert!(!paths.local_path(&rel).exists(), "empty local copy deleted");
}

/// 3. put leaves no `.tmp` residue: right after put (before shutdown —
///    see the determinism note in the module docs) the final cache file
///    exists with the full bytes and no `*.tmp` file lingers anywhere in
///    the cache tree.
#[tokio::test]
async fn put_leaves_no_tmp_files() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/nested/tmpcheck.txt").expect("valid rel path");

    vfs.put(&rel, b"atomic", 1_700_000_000.0)
        .await
        .expect("put accepted");

    let local = paths.local_path(&rel);
    assert!(local.exists(), "final cache file exists right after put");
    assert_eq!(fs::read(&local).expect("read cache copy"), b"atomic");
    let mut all_files = Vec::new();
    collect_files(&cache_root, &mut all_files);
    assert!(
        all_files
            .iter()
            .all(|path| path.extension().is_none_or(|ext| ext != "tmp")),
        "no .tmp files left in the cache tree: {all_files:?}"
    );

    vfs.shutdown().await;
}

/// 4. hydrate prefers the local cache: a pre-seeded cache copy wins over
///    different remote bytes (the remote is never consulted).
#[tokio::test]
async fn hydrate_prefers_local_cache() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    seed_remote_file(&db, &mock, "/cached.bin", b"REMOTE", 64, false).await;
    let local = seed_local(&cache, "/cached.bin", b"LOCAL");

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/cached.bin").expect("valid rel path");

    let hydrated = vfs.hydrate(&rel).await.expect("hydrate from local cache");
    assert_eq!(hydrated, local, "the existing cache path is returned");
    assert_eq!(
        fs::read(&hydrated).expect("read cache copy"),
        b"LOCAL",
        "local bytes win over the remote"
    );
}

/// 5. Single-chunk hydrate: no local copy → downloaded to the mirrored
///    cache path, row flagged `is_cached = true`.
#[tokio::test]
async fn hydrate_downloads_single_chunk_to_cache() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    seed_remote_file(&db, &mock, "/single.bin", b"hello", 64, false).await;

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/single.bin").expect("valid rel path");

    let hydrated = vfs.hydrate(&rel).await.expect("hydrate downloads");
    assert_eq!(fs::read(&hydrated).expect("read downloaded copy"), b"hello");
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    assert_eq!(
        paths.local_path(&rel),
        hydrated,
        "download lands at the mirrored cache path"
    );
    let row = db
        .get_file("/single.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_cached, "row flagged cached after hydration");
}

/// 6. Chunked hydrate: 7 bytes at chunk_size 3 (three remote chunks)
///    merge back into "abcdefg".
#[tokio::test]
async fn hydrate_merges_chunks() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    seed_remote_file(&db, &mock, "/multi.bin", b"abcdefg", 3, false).await;

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(3, None));
    let rel = RelPath::new("/multi.bin").expect("valid rel path");

    let hydrated = vfs.hydrate(&rel).await.expect("hydrate merges chunks");
    assert_eq!(
        fs::read(&hydrated).expect("read merged copy"),
        b"abcdefg",
        "chunks concatenated in index order"
    );
    let row = db
        .get_file("/multi.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_cached);
}

/// 7. Encrypted hydrate: a ciphertext seeded on the remote decrypts into
///    a plaintext cache copy (Python behavior: the cache stores the
///    decrypted bytes).
#[tokio::test]
async fn hydrate_encrypted_decrypts_to_plaintext() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let ciphertext = encrypt("pw", b"secret!");
    seed_remote_file(&db, &mock, "/enc.bin", &ciphertext, 64, true).await;

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, Some("pw")));
    let rel = RelPath::new("/enc.bin").expect("valid rel path");

    let hydrated = vfs.hydrate(&rel).await.expect("decrypting hydrate");
    assert_eq!(
        fs::read(&hydrated).expect("read decrypted copy"),
        b"secret!",
        "cache stores the plaintext"
    );
}

/// 8. Encrypted row without a configured password: MissingPassword (the
///    check happens before any download).
#[tokio::test]
async fn hydrate_encrypted_without_password_is_missing_password() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let ciphertext = encrypt("pw", b"secret!");
    seed_remote_file(&db, &mock, "/enc.bin", &ciphertext, 64, true).await;

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/enc.bin").expect("valid rel path");

    let result = vfs.hydrate(&rel).await;
    assert!(
        matches!(result, Err(VfsError::MissingPassword)),
        "expected MissingPassword, got: {result:?}"
    );
}

/// 9. hydrate rejects a missing row with NotFound and a directory row
///    with IsDirectory (both carrying the virtual path).
#[tokio::test]
async fn hydrate_missing_row_and_dir_row_rejected() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let dir_rel = RelPath::new("/somedir").expect("valid rel path");
    db.upsert_file(&FileUpsert {
        rel_path: dir_rel.as_str().to_string(),
        name: dir_rel.name().to_string(),
        parent_dir: "/".to_string(),
        size: 0,
        mtime: 0.0,
        sha256: None,
        is_dir: true,
        telegram_msg_id: None,
        is_uploaded: false,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
    })
    .expect("seed dir row");

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));

    let missing = RelPath::new("/nope.bin").expect("valid rel path");
    match vfs.hydrate(&missing).await {
        Err(VfsError::NotFound(path)) => assert_eq!(path, "/nope.bin"),
        other => panic!("expected NotFound, got: {other:?}"),
    }
    match vfs.hydrate(&dir_rel).await {
        Err(VfsError::IsDirectory(path)) => assert_eq!(path, "/somedir"),
        other => panic!("expected IsDirectory, got: {other:?}"),
    }
}

/// 10. Hydration evicts LRU entries to fit: with a 100-byte cache, an
///     80-byte cached A is evicted for a 30-byte B — A's file is gone,
///     its row keeps every field except `is_cached = false`, and B's row
///     is flagged cached.
#[tokio::test]
async fn hydrate_evicts_lru_and_clears_flags() {
    let (_dir, db, cache, _cache_root, mock) = test_env(100).await;
    let receipt_a = seed_remote_file(&db, &mock, "/old.bin", &[7u8; 80], 64, false).await;
    let local_a = seed_local(&cache, "/old.bin", &[7u8; 80]);
    let b_bytes = vec![9u8; 30];
    seed_remote_file(&db, &mock, "/new.bin", &b_bytes, 64, false).await;

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel_b = RelPath::new("/new.bin").expect("valid rel path");

    let hydrated = vfs.hydrate(&rel_b).await.expect("hydrate B after eviction");
    assert_eq!(fs::read(&hydrated).expect("read B copy"), b_bytes);

    assert!(!local_a.exists(), "A's cache file evicted to fit B");
    let a = db
        .get_file("/old.bin")
        .expect("db read")
        .expect("row exists");
    assert!(!a.is_cached, "A's cached flag cleared on eviction");
    assert!(a.is_uploaded, "A's uploaded state preserved");
    assert_eq!(
        a.telegram_msg_id,
        Some(i64::from(receipt_a.first_msg_id)),
        "A's msg id preserved"
    );
    let b = db
        .get_file("/new.bin")
        .expect("db read")
        .expect("row exists");
    assert!(b.is_cached, "B flagged cached after hydration");
}

/// 11. put after shutdown: QueueClosed.
#[tokio::test]
async fn put_after_shutdown_is_queue_closed() {
    let (_dir, db, cache, _cache_root, mock) = test_env(1 << 20).await;
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));

    vfs.shutdown().await;

    let rel = RelPath::new("/late.txt").expect("valid rel path");
    let result = vfs.put(&rel, b"too late", 1_700_000_000.0).await;
    assert!(
        matches!(result, Err(VfsError::QueueClosed)),
        "expected QueueClosed, got: {result:?}"
    );
}

/// 12. put_staged: a caller-staged tmp file is renamed onto the final cache
///     path (no in-memory buffering, WebDAV PUT path), the row goes pending
///     immediately, and the drained upload round-trips the bytes.
#[tokio::test]
async fn put_staged_renames_then_uploads_full_cycle() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/nested/staged.txt").expect("valid rel path");

    let final_local = paths.local_path(&rel);
    let staged = final_local.with_file_name(".staged.txt.tmp");
    fs::create_dir_all(staged.parent().expect("staged parent")).expect("create staged dirs");
    fs::write(&staged, b"staged bytes").expect("write staged file");

    vfs.put_staged(&rel, &staged, 1_700_000_000.0)
        .await
        .expect("put_staged accepted");

    assert!(!staged.exists(), "staging file renamed away");
    assert_eq!(
        fs::read(&final_local).expect("read cache copy"),
        b"staged bytes"
    );
    let row = db
        .get_file("/nested/staged.txt")
        .expect("db read")
        .expect("row exists");
    assert!(!row.is_uploaded, "row pending right after put_staged");
    assert!(row.is_cached);
    assert_eq!(row.size, 12, "size from the staged file");

    vfs.shutdown().await;

    let row = db
        .get_file("/nested/staged.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "upload finished during the drain");
    assert!(row.telegram_msg_id.is_some(), "chunk-0 msg id recorded");
    assert!(
        !final_local.exists(),
        "cache copy deleted after the successful upload"
    );
    let hydrated = vfs.hydrate(&rel).await.expect("hydrate after upload");
    assert_eq!(
        fs::read(hydrated).expect("read hydrated copy"),
        b"staged bytes",
        "bytes came back from the mock remote"
    );
}

/// 13. put_staged 0-byte: same semantics as `put` — the transport is never
///     called, the row ends uploaded without a msg id and the empty local
///     file is removed by the drain.
#[tokio::test]
async fn put_staged_zero_byte_skips_transport() {
    let (_dir, db, cache, cache_root, mock) = test_env(1 << 20).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, test_cfg(64, None));
    let rel = RelPath::new("/empty-staged.txt").expect("valid rel path");

    let final_local = paths.local_path(&rel);
    let staged = final_local.with_file_name(".empty-staged.txt.tmp");
    fs::create_dir_all(staged.parent().expect("staged parent")).expect("create staged dirs");
    fs::write(&staged, b"").expect("write empty staged file");

    vfs.put_staged(&rel, &staged, 1_700_000_000.0)
        .await
        .expect("put_staged accepted");
    vfs.shutdown().await;

    assert!(
        mock.upload_calls().is_empty(),
        "0-byte put_staged never touches the transport"
    );
    let row = db
        .get_file("/empty-staged.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "0-byte counts as uploaded");
    assert_eq!(row.telegram_msg_id, None);
    assert!(
        !final_local.exists(),
        "empty local copy deleted by the drain"
    );
}

/// 14. Stalled remote: an `open_delay` of 500ms against a 50ms
///     `hydrate_timeout` surfaces `Timeout(50ms)` (Python parity: the
///     WebDAV thread's `future.result(timeout=180)` download cap) and
///     leaves no `.tmp` staging file anywhere in the cache tree.
#[tokio::test]
async fn hydrate_times_out_when_transport_stalls() {
    let mock = MockTransport::builder()
        .open_delay(Duration::from_millis(500))
        .build();
    let (_dir, db, cache, cache_root, mock) = test_env_with_mock(mock, 1 << 20).await;
    seed_remote_file(&db, &mock, "/stalled.bin", b"stalled bytes", 64, false).await;

    let mut cfg = test_cfg(64, None);
    cfg.hydrate_timeout = Duration::from_millis(50);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, cfg);
    let rel = RelPath::new("/stalled.bin").expect("valid rel path");

    let result = vfs.hydrate(&rel).await;
    assert!(
        matches!(&result, Err(VfsError::Timeout(d)) if *d == Duration::from_millis(50)),
        "expected Timeout(50ms), got: {result:?}"
    );

    let mut all_files = Vec::new();
    collect_files(&cache_root, &mut all_files);
    assert!(
        all_files
            .iter()
            .all(|path| path.extension().is_none_or(|ext| ext != "tmp")),
        "no .tmp files left in the cache tree: {all_files:?}"
    );
}

/// 15. Cache hits bypass the timeout: a pre-seeded local copy wins over
///     a stalling remote (500ms open delay, 50ms timeout) — the remote
///     is never consulted and the local bytes come back.
#[tokio::test]
async fn hydrate_cache_hit_ignores_timeout() {
    let mock = MockTransport::builder()
        .open_delay(Duration::from_millis(500))
        .build();
    let (_dir, db, cache, _cache_root, mock) = test_env_with_mock(mock, 1 << 20).await;
    seed_remote_file(&db, &mock, "/hit.bin", b"REMOTE", 64, false).await;
    seed_local(&cache, "/hit.bin", b"LOCAL");

    let mut cfg = test_cfg(64, None);
    cfg.hydrate_timeout = Duration::from_millis(50);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, cfg);
    let rel = RelPath::new("/hit.bin").expect("valid rel path");

    let hydrated = vfs
        .hydrate(&rel)
        .await
        .expect("cache hit returns despite the tight timeout");
    assert_eq!(
        fs::read(&hydrated).expect("read cache copy"),
        b"LOCAL",
        "local bytes win; the stalling remote was never consulted"
    );
}

/// 16. A generous timeout never bites a merely slow download: a 50ms
///     open delay under a 5s budget hydrates normally with the right
///     bytes.
#[tokio::test]
async fn hydrate_completes_within_generous_timeout() {
    let mock = MockTransport::builder()
        .open_delay(Duration::from_millis(50))
        .build();
    let (_dir, db, cache, _cache_root, mock) = test_env_with_mock(mock, 1 << 20).await;
    seed_remote_file(&db, &mock, "/slow.bin", b"slow but fine", 64, false).await;

    let mut cfg = test_cfg(64, None);
    cfg.hydrate_timeout = Duration::from_secs(5);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Vfs::new(db.clone(), cache, transport, cfg);
    let rel = RelPath::new("/slow.bin").expect("valid rel path");

    let hydrated = vfs
        .hydrate(&rel)
        .await
        .expect("slow download still within budget");
    assert_eq!(
        fs::read(&hydrated).expect("read hydrated copy"),
        b"slow but fine"
    );
}
