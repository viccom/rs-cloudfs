//! RED-phase tests for the `CyDriveFs` DavFileSystem adapter.
//!
//! Contract under test (Python `cydrive/webdav_server.py` baseline +
//! `docs/rust-rewrite-design.md` «WebDAV 层» + compat contract 6):
//! metadata/ETag semantics (sha256 unquoted else `{int(mtime)}-{size}`,
//! 0-len dirs, quota 10 TB), directory listings straight off the DB,
//! hydrated reads with seek, staged writes that flush into the VFS
//! upload path (pending row, no `.tmp` residue, drained upload), and the
//! delete/rename/mkdir semantics the Python provider actually shows
//! (delete never touches the remote; MKCOL mirrors `create_collection`).
//!
//! All tests run offline against the pre-connected `MockTransport`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dav_server::davpath::DavPath;
use dav_server::fs::{DavFileSystem, FsError, OpenOptions};
use futures_util::StreamExt;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{CloudTransport, StorageError, UploadJob, UploadReceipt};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_webdav::CyDriveFs;

/// The virtual 10 TB quota from compat contract 6 (Python
/// `get_available_bytes`).
const TEN_TB: u64 = 10 * 1024 * 1024 * 1024 * 1024;

/// VfsConfig for integration tests: tiny chunks, one worker, fast retry.
fn test_cfg() -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 64,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// The mock's default declared face (mirrors
/// `mock_default_capabilities`): the three legacy bits.
fn legacy_caps() -> cloudkit_core::transport::Capabilities {
    cloudkit_core::transport::Capabilities {
        range_read: true,
        inbound: true,
        chat: true,
        ..cloudkit_core::transport::Capabilities::none()
    }
}

/// The K4 declared face: the legacy bits plus `remote_delete` (a
/// baidu/local-shaped declaration for the gated delete tests).
fn remote_delete_caps() -> cloudkit_core::transport::Capabilities {
    cloudkit_core::transport::Capabilities {
        remote_delete: true,
        ..legacy_caps()
    }
}

/// A directory row born uploaded + cached carrying a backend handle
/// (`telegram_msg_id` = the remote object id; the K4 collection gate
/// consumes exactly this shape).
fn seed_dir_row_with_handle(db: &Arc<MetaDatabase>, rel: &str, msg_id: i64) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir: rel_path
            .parent()
            .map(|parent| parent.as_str().to_string())
            .unwrap_or_else(|| "/".to_string()),
        size: 0,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: true,
        telegram_msg_id: Some(msg_id),
        is_uploaded: true,
        is_cached: true,
        is_encrypted: false,
        chunk_count: 0,
        mime_type: None,
    })
    .expect("seed dir row with handle");
}

/// Real temp environment: SQLite db + mirrored cache tree + pre-connected
/// mock transport + Vfs + the adapter under test. `cache_root` is returned
/// for path assertions (the Vfs owns its own `CacheManager`).
async fn test_env(
    cache_limit: u64,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    PathBuf,
    Arc<MockTransport>,
    Arc<Vfs>,
    CyDriveFs,
) {
    test_env_with_caps(cache_limit, legacy_caps()).await
}

/// The K4 environment variant (B3b 段二b red tests): same assembly with
/// the mock's declared capability face injected — a remote_delete=true
/// declaration stands in for a baidu/local transport.
async fn test_env_with_caps(
    cache_limit: u64,
    caps: cloudkit_core::transport::Capabilities,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    PathBuf,
    Arc<MockTransport>,
    Arc<Vfs>,
    CyDriveFs,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(cache_root.clone(), cache_limit);
    let mock = Arc::new(MockTransport::builder().capabilities(caps).build());
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg()));
    // The adapter's cache handle is path-math only (same root).
    let fs = CyDriveFs::new(
        vfs.clone(),
        db.clone(),
        CacheManager::new(cache_root.clone(), u64::MAX),
    );
    (dir, db, cache_root, mock, vfs, fs)
}

/// Reads-only `OpenOptions` for `open`.
fn read_options() -> OpenOptions {
    OpenOptions {
        read: true,
        ..Default::default()
    }
}

/// PUT-style `OpenOptions` for `open` (dav-server's handle_put shape).
fn write_options(size: Option<u64>) -> OpenOptions {
    OpenOptions {
        write: true,
        create: true,
        truncate: true,
        size,
        ..Default::default()
    }
}

/// Pushes `bytes` to the mock remote as `rel` (split at `chunk_size`).
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

/// Inserts an uploaded `files` row for `rel` plus one chunk row per
/// receipt message (the common hydrate precondition).
fn seed_uploaded_row(
    db: &Arc<MetaDatabase>,
    rel: &str,
    bytes: &[u8],
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
            telegram_msg_id: Some(receipt.first_msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
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
        db.upsert_chunk(file_id, index, msg_id, chunk_row_size, None)
            .expect("seed chunk row");
    }
}

/// Remote bytes plus the matching uploaded row in one shot.
async fn seed_remote_file(
    db: &Arc<MetaDatabase>,
    mock: &Arc<MockTransport>,
    rel: &str,
    bytes: &[u8],
    chunk_size: u64,
) {
    let chunk_count = bytes.len().div_ceil(chunk_size as usize).max(1) as u32;
    let receipt = seed_remote(mock, rel, bytes, chunk_count, chunk_size).await;
    seed_uploaded_row(db, rel, bytes, &receipt, chunk_size);
}

/// Writes a row of any shape straight into the DB.
fn seed_row(db: &Arc<MetaDatabase>, rel: &str, is_dir: bool, size: i64) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent_dir = match rel_path.parent() {
        Some(parent) => parent.as_str().to_string(),
        None => "/".to_string(),
    };
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir,
        size,
        mtime: 1_700_000_123.0,
        sha256: None,
        is_dir,
        telegram_msg_id: None,
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: if is_dir { 0 } else { 1 },
        mime_type: None,
    })
    .expect("seed row");
}

/// Writes `bytes` to the mirrored cache path of `rel`; returns the path.
fn seed_local(cache_root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local = CacheManager::new(cache_root.to_path_buf(), u64::MAX).local_path(&rel_path);
    fs::create_dir_all(local.parent().expect("local parent dir")).expect("create cache dirs");
    fs::write(&local, bytes).expect("write cache file");
    local
}

/// Recursively collects every regular file under `dir`.
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

/// 1. metadata: file/dir rows surface as metadata (len, mtime, type),
///    the root always lists as a directory, missing paths are NotFound.
#[tokio::test]
async fn metadata_file_dir_root_and_missing() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/notes.txt", false, 42);
    seed_row(&db, "/Documents", true, 0);

    let meta = fs
        .metadata(&DavPath::new("/notes.txt").expect("path"))
        .await
        .expect("file metadata");
    assert!(!meta.is_dir());
    assert_eq!(meta.len(), 42);
    let modified = meta.modified().expect("modified");
    assert_eq!(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after epoch")
            .as_secs(),
        1_700_000_123
    );

    let meta = fs
        .metadata(&DavPath::new("/Documents").expect("path"))
        .await
        .expect("dir metadata");
    assert!(meta.is_dir());
    assert_eq!(meta.len(), 0);

    let meta = fs
        .metadata(&DavPath::new("/").expect("path"))
        .await
        .expect("root metadata");
    assert!(meta.is_dir(), "the root always exists as a collection");

    let err = fs
        .metadata(&DavPath::new("/missing.txt").expect("path"))
        .await
        .expect_err("missing row");
    assert_eq!(err, FsError::NotFound);
}

/// 2. ETag contract 6: sha256 unquoted when present, else
///    `{int(mtime)}-{size}`; directories carry no ETag (Python
///    `support_etag` is False for folders).
#[tokio::test]
async fn etag_follows_contract() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;

    let rel_path = RelPath::new("/hashed.txt").expect("valid rel path");
    db.upsert_file(&FileUpsert {
        sha256: Some("abc123".to_string()),
        ..shape_upsert(&rel_path, false, 7, 1_700_000_999.9)
    })
    .expect("seed hashed row");
    let meta = fs
        .metadata(&DavPath::new("/hashed.txt").expect("path"))
        .await
        .expect("hashed metadata");
    assert_eq!(meta.etag().as_deref(), Some("abc123"), "sha unquoted");

    seed_row(&db, "/plain.bin", false, 9);
    let meta = fs
        .metadata(&DavPath::new("/plain.bin").expect("path"))
        .await
        .expect("plain metadata");
    // mtime 1_700_000_123.0 -> int() truncates toward zero.
    assert_eq!(meta.etag().as_deref(), Some("1700000123-9"));

    seed_row(&db, "/folder", true, 0);
    let meta = fs
        .metadata(&DavPath::new("/folder").expect("path"))
        .await
        .expect("folder metadata");
    assert_eq!(meta.etag(), None, "folders carry no ETag");
}

/// Helper: a FileUpsert for `rel` with every default shape filled in.
fn shape_upsert(rel: &RelPath, is_dir: bool, size: i64, mtime: f64) -> FileUpsert {
    FileUpsert {
        rel_path: rel.as_str().to_string(),
        name: rel.name().to_string(),
        parent_dir: rel
            .parent()
            .map_or("/".to_string(), |p| p.as_str().to_string()),
        size,
        mtime,
        sha256: None,
        is_dir,
        telegram_msg_id: None,
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: if is_dir { 0 } else { 1 },
        mime_type: None,
    }
}

/// 3. read_dir: direct children from `list_dir` — directories and files,
///    names as raw bytes.
#[tokio::test]
async fn read_dir_lists_dirs_and_files() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/Documents", true, 0);
    seed_row(&db, "/notes.txt", false, 42);

    let stream = fs
        .read_dir(
            &DavPath::new("/").expect("path"),
            dav_server::fs::ReadDirMeta::Data,
        )
        .await
        .expect("root listing");
    let entries: Vec<_> = stream.map(|entry| entry.expect("entry")).collect().await;
    assert_eq!(entries.len(), 2, "both children listed");
    // list_dir order: dirs first, then name asc.
    assert!(entries[0].is_dir().await.expect("dir type"), "dir first");
    assert_eq!(entries[0].name(), b"Documents".to_vec());
    assert!(
        !entries[1].is_dir().await.expect("file type"),
        "file second"
    );
    assert_eq!(entries[1].name(), b"notes.txt".to_vec());

    let stream = fs
        .read_dir(
            &DavPath::new("/Documents/").expect("path"),
            dav_server::fs::ReadDirMeta::Data,
        )
        .await
        .expect("dir listing with trailing slash");
    let entries: Vec<_> = stream.map(|entry| entry.expect("entry")).collect().await;
    assert!(entries.is_empty(), "no children");
}

/// 4. read_dir errors: on a file row Forbidden, on a missing path
///    NotFound.
#[tokio::test]
async fn read_dir_rejects_file_and_missing() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/plain.txt", false, 1);

    match fs
        .read_dir(
            &DavPath::new("/plain.txt").expect("path"),
            dav_server::fs::ReadDirMeta::Data,
        )
        .await
    {
        Err(e) => assert_eq!(e, FsError::Forbidden),
        Ok(_) => panic!("expected Forbidden from read_dir on a file"),
    }

    match fs
        .read_dir(
            &DavPath::new("/nope").expect("path"),
            dav_server::fs::ReadDirMeta::Data,
        )
        .await
    {
        Err(e) => assert_eq!(e, FsError::NotFound),
        Ok(_) => panic!("expected NotFound from read_dir on a missing path"),
    }
}

/// 5. open read: hydrates from the mock remote through the VFS and reads
///    sequentially, seeks and reads past EOF as an empty payload.
#[tokio::test]
async fn open_read_returns_hydrated_content_with_seek() {
    let (_dir, db, _cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_remote_file(&db, &mock, "/greeting.txt", b"hello world", 64).await;

    let mut file = fs
        .open(
            &DavPath::new("/greeting.txt").expect("path"),
            read_options(),
        )
        .await
        .expect("open for read");
    assert_eq!(
        file.read_bytes(5).await.expect("first read"),
        b"hello".as_slice()
    );
    assert_eq!(
        file.read_bytes(100).await.expect("rest read"),
        b" world".as_slice()
    );
    assert!(
        file.read_bytes(10).await.expect("eof read").is_empty(),
        "reads at EOF return empty"
    );
    let pos = file.seek(std::io::SeekFrom::Start(6)).await.expect("seek");
    assert_eq!(pos, 6);
    assert_eq!(
        file.read_bytes(5).await.expect("read after seek"),
        b"world".as_slice()
    );
}

/// 6. open read errors: missing row NotFound, directory row Forbidden.
#[tokio::test]
async fn open_read_rejects_missing_and_dir() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/folder", true, 0);

    let err = fs
        .open(&DavPath::new("/missing.bin").expect("path"), read_options())
        .await
        .expect_err("open missing");
    assert_eq!(err, FsError::NotFound);

    let err = fs
        .open(&DavPath::new("/folder").expect("path"), read_options())
        .await
        .expect_err("open dir");
    assert_eq!(err, FsError::Forbidden);
}

/// Deterministic content pattern for the streaming tests (period 251
/// keeps byte values distinct across the small fixtures used here).
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// 6a. SR1 streaming reads: with the transport's RANGE_READ bit on (the
///     mock's default face) a plaintext uploaded row opens as a
///     RangeFile — sequential reads reassemble the file through bounded
///     `open_range` windows (never a whole-file `open`), the last window
///     clamped to the row size.
#[tokio::test]
async fn open_read_streams_sequential_reads_through_bounded_windows() {
    let (_dir, db, _cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    let content = pattern(300);
    seed_remote_file(&db, &mock, "/stream.bin", &content, 64).await;
    let fs = fs.with_stream_window(64);

    let mut file = fs
        .open(&DavPath::new("/stream.bin").expect("path"), read_options())
        .await
        .expect("open for read");
    let meta = file.metadata().await.expect("file metadata");
    assert_eq!(meta.len(), 300, "metadata carries the row size (K35)");

    let mut read_back = Vec::new();
    loop {
        let frame = file.read_bytes(7).await.expect("read frame");
        if frame.is_empty() {
            break;
        }
        read_back.extend_from_slice(&frame);
    }
    assert_eq!(read_back, content, "windowed reads reassemble the file");
    assert!(
        mock.open_calls().is_empty(),
        "the streaming path never asks for a whole-file open"
    );
    assert_eq!(
        mock.open_range_calls(),
        vec![(0, 64), (64, 64), (128, 64), (192, 64), (256, 44)],
        "bounded windows in sequence, the last clamped to EOF"
    );
}

/// 6b. SR1 streaming reads: seeks are lazy (never touch the transport),
///     a seek outside the current window drops it and the next read
///     opens a window AT the target position (K34: windows anchor at
///     the read position, not at fixed boundaries), a read spanning the
///     window edge shortens at the edge, and repeated same-position
///     seeks open no new window (dav-server seeks before every range
///     body — the repeat must stay free).
#[tokio::test]
async fn open_read_lazy_seek_crosses_and_reuses_windows() {
    let (_dir, db, _cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    let content = pattern(300);
    seed_remote_file(&db, &mock, "/stream.bin", &content, 64).await;
    let fs = fs.with_stream_window(64);

    let mut file = fs
        .open(&DavPath::new("/stream.bin").expect("path"), read_options())
        .await
        .expect("open for read");
    assert_eq!(
        file.read_bytes(10).await.expect("read at 0"),
        content[..10],
        "first read opens the window [0, 64)"
    );
    // A read spanning the window edge shortens at the edge (short reads
    // are legal DavFile semantics); the next read opens the next window
    // at its own position.
    assert_eq!(
        file.seek(std::io::SeekFrom::Start(60)).await.expect("seek"),
        60
    );
    assert_eq!(
        file.read_bytes(8).await.expect("edge-spanning read"),
        content[60..64],
        "short read at the window edge"
    );
    assert_eq!(
        file.read_bytes(8).await.expect("continuation read"),
        content[64..72],
        "the next window continues the stream"
    );
    // A seek outside the window drops it; the read refetches AT the
    // target position (K34's position-anchored windows).
    assert_eq!(
        file.seek(std::io::SeekFrom::Start(200))
            .await
            .expect("seek"),
        200
    );
    assert_eq!(
        file.read_bytes(10).await.expect("read at 200"),
        content[200..210],
        "the window reopens at the seek target"
    );
    assert_eq!(
        file.seek(std::io::SeekFrom::Start(5)).await.expect("seek"),
        5
    );
    assert_eq!(
        file.read_bytes(10).await.expect("read at 5"),
        content[5..15],
        "seeking back reopens an earlier region"
    );
    assert_eq!(
        mock.open_range_calls(),
        vec![(0, 64), (64, 64), (200, 64), (5, 64)],
        "window sequence: initial, edge continuation, forward seek, back seek"
    );
    assert!(mock.open_calls().is_empty());

    // Repeated same-position and in-window seeks stay free: the position
    // sits inside the current window [5, 69), so no fetch happens.
    let calls_before = mock.open_range_calls().len();
    assert_eq!(
        file.seek(std::io::SeekFrom::Current(10))
            .await
            .expect("current-relative seek"),
        25,
        "Current math off the live position"
    );
    file.seek(std::io::SeekFrom::Start(25))
        .await
        .expect("repeated seek to the same position");
    assert_eq!(
        file.read_bytes(5).await.expect("read after repeats"),
        content[25..30]
    );
    assert_eq!(
        mock.open_range_calls().len(),
        calls_before,
        "same-position seeks and in-window reads open no new window"
    );
}

/// 6c. SR1 streaming reads: `SeekFrom::End` math, EOF and past-EOF reads
///     answer empty without touching the transport, and a seek that
///     would land before byte 0 is an error (dav-server turns seek
///     errors into 416s).
#[tokio::test]
async fn open_read_seek_eof_and_negative_semantics() {
    let (_dir, db, _cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    let content = pattern(300);
    seed_remote_file(&db, &mock, "/stream.bin", &content, 64).await;
    let fs = fs.with_stream_window(64);

    let mut file = fs
        .open(&DavPath::new("/stream.bin").expect("path"), read_options())
        .await
        .expect("open for read");
    assert_eq!(
        file.seek(std::io::SeekFrom::End(0))
            .await
            .expect("seek EOF"),
        300
    );
    assert!(
        file.read_bytes(10).await.expect("read at EOF").is_empty(),
        "reads at EOF return empty"
    );
    assert!(
        mock.open_range_calls().is_empty(),
        "an EOF read opens no window"
    );
    assert_eq!(
        file.seek(std::io::SeekFrom::End(-5))
            .await
            .expect("seek EOF-5"),
        295
    );
    assert_eq!(
        file.read_bytes(100).await.expect("EOF-clamped read"),
        content[295..],
        "the last window clamps to the file end"
    );
    assert_eq!(
        mock.open_range_calls(),
        vec![(295, 5)],
        "EOF-relative reads still use bounded windows"
    );
    // Past EOF is a legal position; reads there stay empty and free.
    assert_eq!(
        file.seek(std::io::SeekFrom::Start(400))
            .await
            .expect("seek past EOF"),
        400
    );
    assert!(file.read_bytes(10).await.expect("read past EOF").is_empty());
    // A negative target (before byte 0) is an error, and the failed
    // seek leaves the position untouched.
    assert_eq!(
        file.seek(std::io::SeekFrom::Current(-1000))
            .await
            .expect_err("negative target"),
        FsError::GeneralFailure
    );
    assert_eq!(
        file.seek(std::io::SeekFrom::Current(4))
            .await
            .expect("current-relative seek after the failure"),
        404,
        "the refused seek did not move the position"
    );
    assert!(
        mock.open_calls().is_empty(),
        "seek math never triggers a whole-file open"
    );
}

/// 6d. SR1 dispatch fallbacks (R-5 legs at the adapter): a 0-byte row
///     and a pending-upload row (bytes only local) serve through the
///     hydrate path — the transport sees neither `open` nor
///     `open_range`.
#[tokio::test]
async fn open_read_fallback_rows_stay_on_hydrate_path() {
    let (_dir, db, cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;

    // 0-byte uploaded row: hydrate materializes the empty local copy.
    seed_row(&db, "/empty.bin", false, 0);
    let mut file = fs
        .open(&DavPath::new("/empty.bin").expect("path"), read_options())
        .await
        .expect("open 0-byte row");
    assert!(file
        .read_bytes(10)
        .await
        .expect("read 0-byte row")
        .is_empty());

    // Pending upload whose only copy is local: hydrate's cache hit
    // serves it; open_read's handle assembly would say NotFound and the
    // dispatch must fall back instead of surfacing it.
    let pending = RelPath::new("/pending.bin").expect("valid rel path");
    db.upsert_file(&FileUpsert {
        is_uploaded: false,
        is_cached: true,
        ..shape_upsert(&pending, false, 10, 1_700_000_123.0)
    })
    .expect("seed pending row");
    seed_local(&cache_root, "/pending.bin", b"local-only");
    let mut file = fs
        .open(&DavPath::new("/pending.bin").expect("path"), read_options())
        .await
        .expect("open pending row");
    assert_eq!(
        file.read_bytes(10).await.expect("read pending row"),
        b"local-only".as_slice(),
        "the local-only copy serves the read"
    );

    assert!(
        mock.open_range_calls().is_empty(),
        "fallback rows never stream"
    );
    assert!(
        mock.open_calls().is_empty(),
        "neither fallback leg reached a whole-file download"
    );
}

/// 6e. SR1 dispatch: an encrypted row never streams even with RANGE_READ
///     on — `open_read` answers Hydrate and hydrate's password gate
///     refuses before any transport call (Forbidden, R-5).
#[tokio::test]
async fn open_read_encrypted_row_without_password_is_forbidden() {
    let (_dir, db, _cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    let rel = RelPath::new("/secret.bin").expect("valid rel path");
    db.upsert_file(&FileUpsert {
        telegram_msg_id: Some(1),
        is_encrypted: true,
        ..shape_upsert(&rel, false, 10, 1_700_000_123.0)
    })
    .expect("seed encrypted row");

    let err = fs
        .open(&DavPath::new("/secret.bin").expect("path"), read_options())
        .await
        .expect_err("encrypted row without a password");
    assert_eq!(err, FsError::Forbidden);
    assert!(
        mock.open_range_calls().is_empty(),
        "an encrypted row never streams"
    );
    assert!(
        mock.open_calls().is_empty(),
        "the password gate fires before any download work"
    );
}

/// 6f. SR1 error mapping: a row whose remote object has vanished maps
///     the window fetch's `StorageError::NotFound` to `FsError::NotFound`
///     (dav-server 404); the streaming path surfaces transport
///     failures, it never fabricates content.
#[tokio::test]
async fn open_read_maps_vanished_remote_to_not_found() {
    let (_dir, db, _cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    let content = pattern(100);
    let receipt = seed_remote(&mock, "/ghost.bin", &content, 2, 64).await;
    seed_uploaded_row(&db, "/ghost.bin", &content, &receipt, 64);
    // The remote object dies; the row stays.
    let handle = cloudkit_core::transport::RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: content.len() as u64,
        path: None,
    };
    mock.delete_remote(&handle)
        .await
        .expect("delete the remote object");

    let mut file = fs
        .open(&DavPath::new("/ghost.bin").expect("path"), read_options())
        .await
        .expect("open still resolves off the row");
    assert_eq!(
        file.read_bytes(10)
            .await
            .expect_err("the window fetch fails"),
        FsError::NotFound,
        "StorageError::NotFound maps to FsError::NotFound"
    );
}

/// 6g. WF0 cache-first at the adapter: a plaintext row that would
///     otherwise stream (RANGE_READ on, non-zero size) serves the LOCAL
///     bytes when the cache copy exists — the response body comes from
///     disk, the transport is never consulted.
#[tokio::test]
async fn open_read_cached_row_serves_local_copy_without_transport() {
    let (_dir, db, cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_remote_file(&db, &mock, "/warm.bin", &pattern(300), 64).await;
    // Divergent local bytes: any remote arrival is observable in the body.
    seed_local(&cache_root, "/warm.bin", b"stale-local-bytes");

    let mut file = fs
        .open(&DavPath::new("/warm.bin").expect("path"), read_options())
        .await
        .expect("open cached row");
    assert_eq!(
        file.read_bytes(100).await.expect("read"),
        b"stale-local-bytes".as_slice(),
        "the cached copy is the served byte truth (WF0 cache-first)"
    );
    assert!(
        mock.open_calls().is_empty() && mock.open_range_calls().is_empty(),
        "a cached row never touches the transport"
    );
}

/// 7. open write: bytes land in a `.{name}.tmp` staging sibling, flush
///    commits them — pending row, atomic rename onto the cache path, no
///    `.tmp` residue — and the drained queue uploads to the mock remote.
#[tokio::test]
async fn open_write_flushes_into_vfs_upload_path() {
    let (_dir, db, cache_root, mock, vfs, fs) = test_env(u64::MAX).await;
    // wsgidav parity: PUT requires the parent collection to exist.
    fs.create_dir(&DavPath::new("/uploads").expect("path"))
        .await
        .expect("create parent dir");

    let mut file = fs
        .open(
            &DavPath::new("/uploads/put.txt").expect("path"),
            write_options(Some(6)),
        )
        .await
        .expect("open for write");
    file.write_bytes(b"Web".to_vec().into())
        .await
        .expect("first write");
    file.write_bytes(b"DAV".to_vec().into())
        .await
        .expect("second write");
    file.flush().await.expect("flush commits");

    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let rel = RelPath::new("/uploads/put.txt").expect("valid rel path");
    let final_local = paths.local_path(&rel);
    assert_eq!(
        fs::read(&final_local).expect("read cache copy"),
        b"WebDAV",
        "flush renamed the staged bytes onto the cache path"
    );
    let mut all_files = Vec::new();
    collect_files(&cache_root, &mut all_files);
    assert!(
        all_files
            .iter()
            .all(|path| path.extension().is_none_or(|ext| ext != "tmp")),
        "no .tmp residue: {all_files:?}"
    );
    let row = db
        .get_file("/uploads/put.txt")
        .expect("db read")
        .expect("row exists");
    assert!(!row.is_uploaded, "row pending right after flush");
    assert!(row.is_cached);
    assert_eq!(row.size, 6);

    vfs.shutdown().await;

    let row = db
        .get_file("/uploads/put.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "drain uploaded the file");
    assert_eq!(mock.upload_calls().len(), 1, "one upload job");
    assert!(!final_local.exists(), "cache copy deleted after upload");
}

/// 8. open write 0-byte flush: the existing 0-byte put semantics — the
///    transport is never called and the row ends uploaded after the
///    drain (Explorer placeholder guard, compat contract 6).
#[tokio::test]
async fn open_write_zero_byte_flush_keeps_zero_semantics() {
    let (_dir, db, cache_root, mock, vfs, fs) = test_env(u64::MAX).await;

    let mut file = fs
        .open(
            &DavPath::new("/placeholder.txt").expect("path"),
            write_options(Some(0)),
        )
        .await
        .expect("open for write");
    file.flush().await.expect("flush empty file");

    vfs.shutdown().await;

    assert!(
        mock.upload_calls().is_empty(),
        "0-byte flush never touches the transport"
    );
    let row = db
        .get_file("/placeholder.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "0-byte counts as uploaded");
    assert_eq!(row.size, 0);
    let mut all_files = Vec::new();
    collect_files(&cache_root, &mut all_files);
    assert!(
        all_files.is_empty(),
        "neither final nor .tmp file lingers: {all_files:?}"
    );
}

/// 9. create_dir: visible in the listing right away (Python
///    `create_collection` parity), duplicates fail Exists, a missing
///    parent fails NotFound.
#[tokio::test]
async fn create_dir_visible_and_duplicate_exists() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;

    fs.create_dir(&DavPath::new("/newdir").expect("path"))
        .await
        .expect("create dir");

    let meta = fs
        .metadata(&DavPath::new("/newdir").expect("path"))
        .await
        .expect("dir metadata");
    assert!(meta.is_dir());
    let stream = fs
        .read_dir(
            &DavPath::new("/").expect("path"),
            dav_server::fs::ReadDirMeta::Data,
        )
        .await
        .expect("root listing");
    let entries: Vec<_> = stream.map(|e| e.expect("entry")).collect().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), b"newdir".to_vec());
    let row = db
        .get_file("/newdir")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_dir && row.is_uploaded && row.is_cached);

    let err = fs
        .create_dir(&DavPath::new("/newdir").expect("path"))
        .await
        .expect_err("duplicate create");
    assert_eq!(err, FsError::Exists);

    let err = fs
        .create_dir(&DavPath::new("/missing/child").expect("path"))
        .await
        .expect_err("missing parent");
    assert_eq!(err, FsError::NotFound);
}

/// Drain any wake permit stored by seeding writes (`notify_one` keeps a
/// permit when no waiter is enabled), so the assertion in each rings-test
/// can only pass on the operation under test — not on seeding echoes.
async fn drain_stale_wake_permits(notifier: &tokio::sync::Notify) {
    use std::time::Duration;

    while tokio::time::timeout(Duration::from_millis(50), notifier.notified())
        .await
        .is_ok()
    {}
}

/// MOVE rings the sync wake: every files-row mutation must ring so the
/// realtime sync pass runs promptly instead of waiting for the interval.
#[tokio::test]
async fn rename_rings_the_sync_wake() {
    use std::time::Duration;

    let (_dir, db, _cache_root, mock, vfs, fs) = test_env(u64::MAX).await;
    seed_remote_file(&db, &mock, "/wake-old.bin", b"payload", 2).await;

    let notifier = vfs.sync_notifier();
    drain_stale_wake_permits(&notifier).await;
    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();

    fs.rename(
        &DavPath::new("/wake-old.bin").expect("path"),
        &DavPath::new("/wake-new.bin").expect("path"),
    )
    .await
    .expect("rename file");

    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("rename must ring the sync wake");
}

/// DELETE rings the sync wake too (review High-1): a deletion is the
/// tombstone's origin — Explorer's remove_file is the main delete
/// path, and without a ring the tombstone waits for the sync interval.
/// The row is uploaded (pending rows are refused by the guard — the
/// wake is asserted on the deletable shape, not the guarded one).
#[tokio::test]
async fn remove_file_rings_the_sync_wake() {
    use std::time::Duration;

    let (_dir, db, _cache_root, mock, vfs, fs) = test_env(u64::MAX).await;
    seed_remote_file(&db, &mock, "/wake-doomed.bin", b"payload", 2).await;

    let notifier = vfs.sync_notifier();
    drain_stale_wake_permits(&notifier).await;
    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();

    fs.remove_file(&DavPath::new("/wake-doomed.bin").expect("path"))
        .await
        .expect("remove file");

    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("remove_file must ring the sync wake");
}

/// The directory face of the same contract (review High-1): an
/// Explorer folder delete is a files-row mutation like any other.
#[tokio::test]
async fn remove_dir_rings_the_sync_wake() {
    use std::time::Duration;

    let (_dir, db, _cache_root, _mock, vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/wake-empty-dir", true, 0);

    let notifier = vfs.sync_notifier();
    drain_stale_wake_permits(&notifier).await;
    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();

    fs.remove_dir(&DavPath::new("/wake-empty-dir").expect("path"))
        .await
        .expect("remove empty dir");

    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("remove_dir must ring the sync wake");
}

/// 10. remove_file: row and cached copy deleted; the remote is never
///     touched (Python `handle_delete` parity — no telegram delete).
#[tokio::test]
async fn remove_file_deletes_row_and_cache_without_remote_delete() {
    let (_dir, db, cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_remote_file(&db, &mock, "/doomed.bin", b"payload", 64).await;
    let local = seed_local(&cache_root, "/doomed.bin", b"payload");

    fs.remove_file(&DavPath::new("/doomed.bin").expect("path"))
        .await
        .expect("remove file");

    let err = fs
        .metadata(&DavPath::new("/doomed.bin").expect("path"))
        .await
        .expect_err("row gone");
    assert_eq!(err, FsError::NotFound);
    assert!(!local.exists(), "cache copy removed");
    assert!(
        mock.deleted().is_empty(),
        "Python parity: remove_file never deletes remote messages"
    );
    assert_eq!(
        mock.upload_calls().len(),
        1,
        "the seeding upload is the only transport write"
    );

    let err = fs
        .remove_file(&DavPath::new("/doomed.bin").expect("path"))
        .await
        .expect_err("remove missing");
    assert_eq!(err, FsError::NotFound);
}

/// 11. remove_dir: empty dirs delete fine; a non-empty dir fails Exists
///     with the children left intact (guards the orphaned-rows corruption
///     the Python baseline silently commits; dav-server's handler empties
///     children first for Depth-infinity deletes, so Explorer behavior
///     matches).
#[tokio::test]
async fn remove_dir_empty_ok_non_empty_exists() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/empty-dir", true, 0);
    seed_row(&db, "/full-dir", true, 0);
    seed_row(&db, "/full-dir/child.txt", false, 3);

    fs.remove_dir(&DavPath::new("/empty-dir").expect("path"))
        .await
        .expect("remove empty dir");
    let err = fs
        .metadata(&DavPath::new("/empty-dir").expect("path"))
        .await
        .expect_err("empty dir gone");
    assert_eq!(err, FsError::NotFound);

    let err = fs
        .remove_dir(&DavPath::new("/full-dir").expect("path"))
        .await
        .expect_err("non-empty dir rejected");
    assert_eq!(err, FsError::Exists);
    fs.metadata(&DavPath::new("/full-dir/child.txt").expect("path"))
        .await
        .expect("children untouched");
}

/// 11a (K4 / B3b 段二b): with the transport declaring `remote_delete`,
/// the WebDAV DELETE on a file deletes the remote object FIRST, then the
/// row and the cache copy (the same gate the core `Vfs::remove_file`
/// carries — the adapter must not be the hole in the three-face wiring).
#[tokio::test]
async fn remove_file_gated_remote_delete_removes_remote_first() {
    let caps = remote_delete_caps();
    let (_dir, db, cache_root, mock, _vfs, fs) = test_env_with_caps(u64::MAX, caps).await;
    seed_remote_file(&db, &mock, "/doomed.bin", b"payload", 64).await;
    let local = seed_local(&cache_root, "/doomed.bin", b"payload");
    let row = db
        .get_file("/doomed.bin")
        .expect("db read")
        .expect("row exists");

    fs.remove_file(&DavPath::new("/doomed.bin").expect("path"))
        .await
        .expect("remove file");

    let err = fs
        .metadata(&DavPath::new("/doomed.bin").expect("path"))
        .await
        .expect_err("row gone");
    assert_eq!(err, FsError::NotFound);
    assert!(!local.exists(), "cache copy removed");
    assert_eq!(
        mock.deleted(),
        vec![row.telegram_msg_id.expect("seeded row carries the msg id")],
        "the remote object died with the row (remote_delete=true)"
    );
}

/// 11b (K4): the WebDAV DELETE on a COLLECTION gates the same way — a
/// dir row that carries a backend handle has its remote object deleted
/// before the row dies.
#[tokio::test]
async fn remove_dir_gated_remote_delete_removes_remote_first() {
    let caps = remote_delete_caps();
    let (_dir, db, _cache_root, mock, _vfs, fs) = test_env_with_caps(u64::MAX, caps).await;
    // A dir row with a remote handle (a backend-materialized directory:
    // baidu fs_id / a local subtree root). Dir rows are born
    // uploaded+cached; the handle is what the gate consumes.
    let receipt = seed_remote(&mock, "/docs", b"dir-marker", 1, 64).await;
    seed_dir_row_with_handle(&db, "/docs", receipt.first_msg_id);

    fs.remove_dir(&DavPath::new("/docs").expect("path"))
        .await
        .expect("remove empty dir with remote handle");
    assert_eq!(
        mock.deleted(),
        vec![receipt.first_msg_id],
        "the dir's remote object died before the row"
    );
    assert!(
        db.get_file("/docs").expect("db read").is_none(),
        "the dir row is gone"
    );
}

/// 11c (K4): a refused remote dir delete (fails twice — the gate
/// retries once) aborts the collection delete with the row KEPT.
#[tokio::test]
async fn remove_dir_remote_refusal_keeps_row() {
    let refused = || Err(StorageError::Unavailable("backend down".into()));
    let caps = remote_delete_caps();
    let dir = tempfile::tempdir().expect("temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let mut builder = MockTransport::builder().capabilities(caps);
    builder = builder.delete_action(refused());
    builder = builder.delete_action(refused());
    let mock = Arc::new(builder.build());
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let vfs = Arc::new(Vfs::new(
        db.clone(),
        CacheManager::new(cache_root.clone(), u64::MAX),
        transport,
        test_cfg(),
    ));
    let fs = CyDriveFs::new(
        vfs.clone(),
        db.clone(),
        CacheManager::new(cache_root, u64::MAX),
    );

    let receipt = seed_remote(&mock, "/stuck", b"dir-marker", 1, 64).await;
    seed_dir_row_with_handle(&db, "/stuck", receipt.first_msg_id);

    let err = fs
        .remove_dir(&DavPath::new("/stuck").expect("path"))
        .await
        .expect_err("the refused remote dir delete must abort");
    assert_eq!(
        err,
        FsError::GeneralFailure,
        "the transport refusal maps to the generic failure"
    );
    assert!(
        db.get_file("/stuck").expect("db read").is_some(),
        "the dir row survives the refused remote delete"
    );
}

/// 12. rename (file): row, cache copy and chunk linkage move together —
///     the row keeps its id so chunk rows stay attached, the remote is
///     untouched (no re-upload, no delete), and hydrate works at the new
///     path.
#[tokio::test]
async fn rename_file_moves_row_cache_and_chunks() {
    let (_dir, db, cache_root, mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_remote_file(&db, &mock, "/old.bin", b"abcdefg", 3).await;
    let local = seed_local(&cache_root, "/old.bin", b"abcdefg");
    let old_row = db
        .get_file("/old.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(old_row.chunk_count, 3, "seeded multi-chunk");

    fs.rename(
        &DavPath::new("/old.bin").expect("path"),
        &DavPath::new("/renamed.bin").expect("path"),
    )
    .await
    .expect("rename file");

    let err = fs
        .metadata(&DavPath::new("/old.bin").expect("path"))
        .await
        .expect_err("old path gone");
    assert_eq!(err, FsError::NotFound);
    let new_row = db
        .get_file("/renamed.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        new_row.id, old_row.id,
        "row keeps its id (chunks stay linked)"
    );
    assert_eq!(
        db.get_chunks_by_file_id(new_row.id).expect("chunks").len(),
        3,
        "chunk rows survive the rename"
    );
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let rel = RelPath::new("/renamed.bin").expect("valid rel path");
    assert!(!local.exists(), "cache copy moved away from the old path");
    assert_eq!(
        fs::read(paths.local_path(&rel)).expect("read moved cache copy"),
        b"abcdefg"
    );
    assert_eq!(
        mock.upload_calls().len(),
        1,
        "only the seeding upload happened — rename never re-uploads"
    );
    assert!(
        mock.deleted().is_empty(),
        "rename never deletes on the remote"
    );

    let mut file = fs
        .open(&DavPath::new("/renamed.bin").expect("path"), read_options())
        .await
        .expect("open renamed file");
    assert_eq!(
        file.read_bytes(7).await.expect("read hydrated bytes"),
        b"abcdefg".as_slice(),
        "hydrate works through the new path and chunk linkage"
    );
}

/// 13. rename (dir): the subtree rows move (listing at the new path
///     shows the child), old paths disappear.
#[tokio::test]
async fn rename_dir_moves_subtree() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/docs", true, 0);
    seed_row(&db, "/docs/a.txt", false, 1);

    fs.rename(
        &DavPath::new("/docs").expect("path"),
        &DavPath::new("/books").expect("path"),
    )
    .await
    .expect("rename dir");

    let stream = fs
        .read_dir(
            &DavPath::new("/books").expect("path"),
            dav_server::fs::ReadDirMeta::Data,
        )
        .await
        .expect("listing at the new path");
    let entries: Vec<_> = stream.map(|e| e.expect("entry")).collect().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), b"a.txt".to_vec());
    for old in ["/docs", "/docs/a.txt"] {
        let err = fs
            .metadata(&DavPath::new(old).expect("path"))
            .await
            .expect_err("old path gone");
        assert_eq!(err, FsError::NotFound);
    }
}

/// 14. get_quota: used = DB total bytes, total = used + 10 TB (contract 6).
#[tokio::test]
async fn get_quota_matches_ten_tb_contract() {
    let (_dir, db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    seed_row(&db, "/a.bin", false, 10);
    seed_row(&db, "/b.bin", false, 32);
    seed_row(&db, "/ignored-dir", true, 99);

    let (used, total) = fs.get_quota().await.expect("quota");
    assert_eq!(used, 42, "used sums non-directory rows only");
    assert_eq!(total, Some(used + TEN_TB));
}

/// 15. copy stays NotImplemented (design doc: Explorer drag-copy goes
///     through PUT; regression guard against accidentally overriding it).
#[tokio::test]
async fn copy_is_not_implemented() {
    let (_dir, _db, _cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;
    let err = fs
        .copy(
            &DavPath::new("/a.txt").expect("path"),
            &DavPath::new("/b.txt").expect("path"),
        )
        .await
        .expect_err("copy not implemented");
    assert_eq!(err, FsError::NotImplemented);
}

/// 16 (plan F2 / review H2): DELETE of a pending upload whose local
///     cache copy still exists is refused — that copy is the only copy
///     of the bytes (nothing is on the remote yet) — surfacing as
///     `FsError::Forbidden` with both the row and the copy intact; a
///     ghost pending row (copy already vanished, bytes nowhere) stays
///     deletable, otherwise it could never be cleaned up. Same
///     adjudication as core `VfsError::UploadPending`, mapped onto the
///     adapter's own error surface.
#[tokio::test]
async fn remove_file_pending_upload_forbidden() {
    let (_dir, db, cache_root, _mock, _vfs, fs) = test_env(u64::MAX).await;

    // Pending row (is_uploaded = 0, cached) + its local copy in the
    // cache tree: the only copy of the bytes.
    let pending = RelPath::new("/uploading.bin").expect("valid rel path");
    db.upsert_file(&FileUpsert {
        is_uploaded: false,
        is_cached: true,
        ..shape_upsert(&pending, false, 6, 1_700_000_123.0)
    })
    .expect("seed pending row");
    let local = seed_local(&cache_root, "/uploading.bin", b"bytes");

    let err = fs
        .remove_file(&DavPath::new("/uploading.bin").expect("path"))
        .await
        .expect_err("pending upload with its only copy must be refused");
    assert_eq!(err, FsError::Forbidden);
    assert!(
        db.get_file("/uploading.bin")
            .expect("db read")
            .is_some_and(|row| !row.is_uploaded),
        "the pending row survives the refused delete"
    );
    assert!(
        local.exists(),
        "the only copy of the bytes survives the refused delete"
    );

    // Ghost pending row: the copy vanished — the row must be deletable.
    let ghost = RelPath::new("/ghost.bin").expect("valid rel path");
    db.upsert_file(&FileUpsert {
        is_uploaded: false,
        is_cached: true,
        ..shape_upsert(&ghost, false, 6, 1_700_000_123.0)
    })
    .expect("seed ghost row");
    // No local copy seeded for /ghost.bin.
    fs.remove_file(&DavPath::new("/ghost.bin").expect("path"))
        .await
        .expect("a ghost pending row is deletable");
    assert!(
        db.get_file("/ghost.bin").expect("db read").is_none(),
        "the ghost row is gone"
    );
}
