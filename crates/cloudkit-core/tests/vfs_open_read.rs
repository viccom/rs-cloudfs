//! SR0 / K33 tests for the streaming-read seam: [`Vfs::open_read`] gates
//! on the triple condition (plaintext row + transport `range_read` bit +
//! non-zero size) and otherwise answers an explicit
//! [`StreamSource::Hydrate`] fallback signal; the `Stream` arm carries the
//! row -> handle assembly (chunks first / row id fallback) with the
//! authoritative total size and the shared transport for bounded-window
//! reads.
//!
//! WF0 / K42 cache-first amendment: a cached plaintext copy routes to the
//! hydrate arm ahead of the triple gate (the hydrate path then serves the
//! local file with zero remote traffic); the password gate still fires
//! first, and admission itself never hydrates as a side effect.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{
    ByteStream, Capabilities, CloudTransport, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{StreamSource, Vfs, VfsConfig, VfsError};
use futures_util::StreamExt;

// ------------------------------------------------------------- helpers ---

/// VfsConfig for these tests: tiny chunk size (multi-chunk at byte
/// scales), one worker (deterministic order), fast retry, optional
/// encryption password (mirrors `vfs.rs`).
fn test_cfg(encryption_password: Option<&str>) -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 64,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: encryption_password.map(str::to_string),
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// Real temp environment: SQLite db + mirrored cache tree + the given
/// mock transport, pre-connected (mirrors `vfs.rs`).
async fn test_env_with_mock(
    mock: MockTransport,
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
    let cache = CacheManager::new(cache_root.clone(), 1 << 20);
    let mock = Arc::new(mock);
    mock.connect().await.expect("pre-connect mock transport");
    (dir, db, cache, cache_root, mock)
}

/// `test_env_with_mock` with the default always-Ok mock.
async fn test_env() -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    PathBuf,
    Arc<MockTransport>,
) {
    test_env_with_mock(MockTransport::new()).await
}

/// Pushes `bytes` to the mock remote as `rel` by calling
/// `CloudTransport::upload` directly (mirrors `vfs.rs`).
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

/// Inserts an uploaded `files` row for `rel` (mirrors `vfs.rs`).
fn seed_uploaded_row(
    db: &MetaDatabase,
    rel: &str,
    size: i64,
    chunk_count: i64,
    msg_id: Option<i64>,
    is_encrypted: bool,
) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir: rel_path
            .parent()
            .expect("non-root path")
            .as_str()
            .to_string(),
        size,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: msg_id,
        is_uploaded: true,
        is_cached: false,
        is_encrypted,
        chunk_count,
        mime_type: None,
    })
    .expect("seed files row");
}

/// The common streaming precondition in one shot: remote bytes at the
/// mock remote plus the matching uploaded row with per-chunk rows.
async fn seed_remote_file(
    db: &Arc<MetaDatabase>,
    mock: &Arc<MockTransport>,
    rel: &str,
    bytes: &[u8],
    chunk_size: u64,
) -> UploadReceipt {
    let chunk_count = if bytes.is_empty() {
        1
    } else {
        bytes.len().div_ceil(chunk_size as usize) as u32
    };
    let receipt = seed_remote(mock, rel, bytes, chunk_count, chunk_size).await;
    seed_uploaded_row(
        db,
        rel,
        bytes.len() as i64,
        receipt.chunk_msg_ids.len() as i64,
        Some(receipt.first_msg_id),
        false,
    );
    let row = db.get_file(rel).expect("db read").expect("row exists");
    for (index, &msg_id) in receipt.chunk_msg_ids.iter().enumerate() {
        let index = index as i64;
        let chunk_row_size = if index + 1 < receipt.chunk_msg_ids.len() as i64 {
            chunk_size as i64
        } else {
            bytes.len() as i64 - (chunk_size as i64) * (receipt.chunk_msg_ids.len() as i64 - 1)
        };
        db.upsert_chunk(row.id, index, msg_id, chunk_row_size, None)
            .expect("seed chunk row");
    }
    receipt
}

/// Writes a plaintext copy of `rel` straight into the mirrored cache tree
/// (WF0 precondition: `CacheManager::is_cached` sees a non-empty file).
/// Returns the on-disk path.
fn seed_cache_copy(cache_root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local = CacheManager::new(cache_root.to_path_buf(), 1 << 20).local_path(&rel_path);
    fs::create_dir_all(local.parent().expect("cache parent dir")).expect("create cache dirs");
    fs::write(&local, bytes).expect("write cache copy");
    local
}

/// Concatenates every chunk of a byte stream; propagates the first error.
async fn drain(stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// Builds the Vfs over the environment's pieces.
fn build_vfs(
    db: &Arc<MetaDatabase>,
    cache: CacheManager,
    mock: &Arc<MockTransport>,
    cfg: VfsConfig,
) -> Vfs {
    let transport: Arc<dyn CloudTransport> = mock.clone();
    Vfs::new(db.clone(), cache, transport, cfg)
}

// -------------------------------------------------------------- tests ----

/// 1. The all-clear gate: a plaintext, non-zero, chunked row over a
///    range-capable transport streams. The Stream arm carries the
///    authoritative total size (K35), the chunks-first handle and the
///    shared transport — and that handle really opens bounded windows
///    through `open_range` (observed by the mock recorder: the window
///    was requested, and no full `open` ever ran).
#[tokio::test]
async fn open_read_streams_plaintext_row_with_window_capable_handle() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;
    let receipt = seed_remote_file(&db, &mock, "/stream.bin", b"0123456789A", 4).await;
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/stream.bin").expect("valid rel path");

    let StreamSource::Stream {
        handle,
        total_size,
        transport,
    } = vfs.open_read(&rel).await.expect("stream admitted")
    else {
        panic!("plaintext row over a range-capable transport must stream");
    };
    assert_eq!(total_size, 11, "authoritative row size (K35)");
    assert_eq!(handle.first_msg_id, receipt.first_msg_id);
    assert_eq!(handle.chunk_msg_ids, receipt.chunk_msg_ids);
    assert_eq!(
        handle.path.as_ref().map(|p| p.as_str()),
        Some("/stream.bin")
    );

    // The handle opens a mid-file window through the carried transport.
    let window = drain(
        transport
            .open_range(&handle, 2, 5)
            .await
            .expect("open window"),
    )
    .await
    .expect("window bytes");
    assert_eq!(window, b"23456", "byte-exact mid-file slice");
    assert_eq!(mock.open_range_calls(), vec![(2, 5)], "window requested");
    assert!(mock.open_calls().is_empty(), "no full open ran");
}

/// 2. An encrypted row always falls back (K33: v1 GCM whole-file AEAD
///    can never be range-sliced; v2 likewise stays whole-file on the
///    read path). The password is configured, so this is a genuine
///    fallback signal, not the MissingPassword error.
#[tokio::test]
async fn open_read_falls_back_for_encrypted_row() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;
    // Ciphertext bytes are irrelevant — the gate returns before any I/O.
    let receipt = seed_remote(&mock, "/enc.bin", b"ciphertext-bytes", 1, 64).await;
    seed_uploaded_row(&db, "/enc.bin", 17, 1, Some(receipt.first_msg_id), true);
    let vfs = build_vfs(&db, cache, &mock, test_cfg(Some("pw")));
    let rel = RelPath::new("/enc.bin").expect("valid rel path");

    assert!(
        matches!(vfs.open_read(&rel).await, Ok(StreamSource::Hydrate)),
        "encrypted row falls back to hydrate"
    );
    assert!(mock.open_calls().is_empty() && mock.open_range_calls().is_empty());
}

/// 3. Password-gate parity with hydrate: an encrypted row with NO
///    configured password is the actionable `MissingPassword` error, not
///    a fallback (the caller must resolve the password, not retry).
#[tokio::test]
async fn open_read_encrypted_without_password_is_missing_password() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;
    let receipt = seed_remote(&mock, "/nopw.bin", b"ciphertext-bytes", 1, 64).await;
    seed_uploaded_row(&db, "/nopw.bin", 17, 1, Some(receipt.first_msg_id), true);
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/nopw.bin").expect("valid rel path");

    let err = vfs
        .open_read(&rel)
        .await
        .err()
        .expect("encrypted row without a password is an error");
    assert!(matches!(err, VfsError::MissingPassword), "{err:?}");
}

/// 4. A transport that declares no `range_read` falls every row back to
///    hydrate (K33 second gate; the R-5 capability probe).
#[tokio::test]
async fn open_read_falls_back_when_range_read_not_declared() {
    let caps = Capabilities {
        range_read: false,
        inbound: true,
        chat: true,
        ..Capabilities::none()
    };
    let (_dir, db, cache, _cache_root, mock) =
        test_env_with_mock(MockTransport::builder().capabilities(caps).build()).await;
    seed_remote_file(&db, &mock, "/norange.bin", b"0123456789A", 4).await;
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/norange.bin").expect("valid rel path");

    assert!(
        matches!(vfs.open_read(&rel).await, Ok(StreamSource::Hydrate)),
        "range-incapable transport falls back to hydrate"
    );
}

/// 5. A 0-byte row falls back (K33 third gate): there are no remote
///    bytes to window; the hydrate path materializes the empty copy.
#[tokio::test]
async fn open_read_falls_back_for_zero_byte_row() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;
    seed_uploaded_row(&db, "/empty.bin", 0, 0, None, false);
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/empty.bin").expect("valid rel path");

    assert!(
        matches!(vfs.open_read(&rel).await, Ok(StreamSource::Hydrate)),
        "0-byte row falls back to hydrate"
    );
}

/// 6. Row lookup semantics match hydrate: a missing row is NotFound, a
///    directory row is IsDirectory.
#[tokio::test]
async fn open_read_missing_row_and_dir_row_rejected() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));

    let missing = RelPath::new("/missing.bin").expect("valid rel path");
    let err = vfs
        .open_read(&missing)
        .await
        .err()
        .expect("missing row is an error");
    assert!(
        matches!(err, VfsError::NotFound(ref p) if p == "/missing.bin"),
        "{err:?}"
    );

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
    let err = vfs
        .open_read(&dir_rel)
        .await
        .err()
        .expect("dir row is an error");
    assert!(
        matches!(err, VfsError::IsDirectory(ref p) if p == "/somedir"),
        "{err:?}"
    );
}

/// 7. Handle assembly fallback: a row with no per-chunk rows streams on
///    the row's own chunk-0 msg id (the single-chunk shape — the same
///    id set hydrate assembles).
#[tokio::test]
async fn open_read_row_msg_id_fallback_when_no_chunk_rows() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;
    let receipt = seed_remote(&mock, "/single.bin", b"0123456789A", 1, 64).await;
    seed_uploaded_row(&db, "/single.bin", 11, 1, Some(receipt.first_msg_id), false);
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/single.bin").expect("valid rel path");

    let StreamSource::Stream { handle, .. } = vfs.open_read(&rel).await.expect("stream admitted")
    else {
        panic!("row-id fallback row must stream");
    };
    assert_eq!(handle.chunk_msg_ids, vec![receipt.first_msg_id]);
    assert_eq!(handle.first_msg_id, receipt.first_msg_id);
}

/// 8. WF0 cache-first (K42) supersedes the K34 no-cache clause for the
///    cached leg: a cached plaintext row routes to the hydrate arm (the
///    local plaintext then serves the read), while the cold row still
///    streams — and admission itself never hydrates as a side effect (no
///    cache copy appears, the mock sees no full `open`).
#[tokio::test]
async fn open_read_cache_first_cached_serves_locally_cold_stays_cold() {
    let (_dir, db, cache, cache_root, mock) = test_env().await;
    seed_remote_file(&db, &mock, "/cached.bin", b"0123456789A", 4).await;
    seed_remote_file(&db, &mock, "/cold.bin", b"ABCDEFGHIJK", 4).await;

    // Pre-seed a local copy for /cached.bin (is_cached on disk).
    let cached_rel = RelPath::new("/cached.bin").expect("valid rel path");
    let cached_local = seed_cache_copy(&cache_root, "/cached.bin", b"stale-local-bytes");

    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let cold_rel = RelPath::new("/cold.bin").expect("valid rel path");

    // Cached row: cache-first admission (WF0/K42) — the hydrate arm is
    // the local plaintext serve, with the remote never consulted.
    assert!(
        matches!(vfs.open_read(&cached_rel).await, Ok(StreamSource::Hydrate)),
        "cached row falls back to the local plaintext (WF0 cache-first)"
    );
    assert_eq!(
        vfs.hydrate(&cached_rel)
            .await
            .expect("hydrate serves the cache copy"),
        cached_local,
        "the hydrate arm answers the cached path"
    );
    assert_eq!(
        fs::read(&cached_local).expect("read cache copy"),
        b"stale-local-bytes",
        "cache-first is a pure routing probe — the copy is untouched"
    );
    // Cold row: streaming admission must not hydrate.
    assert!(
        matches!(
            vfs.open_read(&cold_rel).await,
            Ok(StreamSource::Stream { .. })
        ),
        "cold row streams"
    );
    assert!(
        !CacheManager::new(cache_root.clone(), 1 << 20)
            .local_path(&cold_rel)
            .exists(),
        "no cache copy materialized for the cold row (admission stays side-effect free)"
    );
    assert!(
        mock.open_calls().is_empty() && mock.open_range_calls().is_empty(),
        "cached and cold admission consult no remote face"
    );
}

/// 9. WF0 cache-first (K42): a cached plaintext row that would otherwise
///    qualify for streaming (RANGE_READ on, non-zero size) still routes to
///    hydrate — the local copy wins over the remote, byte-verbatim.
#[tokio::test]
async fn open_read_cached_plaintext_row_routes_to_local_copy() {
    let (_dir, db, cache, cache_root, mock) = test_env().await;
    seed_remote_file(&db, &mock, "/warm.bin", b"remote-version", 4).await;
    let cached = seed_cache_copy(&cache_root, "/warm.bin", b"cached-local-copy");
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/warm.bin").expect("valid rel path");

    assert!(
        matches!(vfs.open_read(&rel).await, Ok(StreamSource::Hydrate)),
        "a cached plaintext row serves locally (WF0 cache-first)"
    );
    assert_eq!(
        fs::read(&cached).expect("read cache copy"),
        b"cached-local-copy",
        "the local plaintext is the served byte truth"
    );
    assert!(
        mock.open_calls().is_empty() && mock.open_range_calls().is_empty(),
        "cache-first must consult no remote face"
    );
}

/// 10. Regression pin: the same plaintext row WITHOUT a cached copy keeps
///     streaming (WF0 must not swallow the cold case), and the two rows
///     differ only in the cache probe — the remote never saw either
///     admission.
#[tokio::test]
async fn open_read_uncached_plaintext_row_still_streams() {
    let (_dir, db, cache, _cache_root, mock) = test_env().await;
    seed_remote_file(&db, &mock, "/cold.bin", b"0123456789A", 4).await;
    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/cold.bin").expect("valid rel path");

    assert!(
        matches!(vfs.open_read(&rel).await, Ok(StreamSource::Stream { .. })),
        "an uncached plaintext row still streams (cold regression pin)"
    );
    assert!(
        mock.open_calls().is_empty() && mock.open_range_calls().is_empty(),
        "admission itself performs no remote I/O"
    );
}

/// 11. WF0 ordering red line: the password gate fires BEFORE the cache
///     probe — an encrypted row without a configured password stays
///     `MissingPassword` even though a decrypted copy sits in the cache;
///     the local plaintext never becomes an unauthenticated backdoor.
#[tokio::test]
async fn open_read_encrypted_cached_row_without_password_is_missing_password() {
    let (_dir, db, cache, cache_root, mock) = test_env().await;
    let receipt = seed_remote(&mock, "/encached.bin", b"ciphertext-bytes", 1, 64).await;
    seed_uploaded_row(
        &db,
        "/encached.bin",
        17,
        1,
        Some(receipt.first_msg_id),
        true,
    );
    seed_cache_copy(&cache_root, "/encached.bin", b"decrypted-local-copy");

    let vfs = build_vfs(&db, cache, &mock, test_cfg(None));
    let rel = RelPath::new("/encached.bin").expect("valid rel path");
    let err = vfs
        .open_read(&rel)
        .await
        .err()
        .expect("encrypted row without a password stays an error");
    assert!(matches!(err, VfsError::MissingPassword), "{err:?}");
    assert!(
        mock.open_calls().is_empty() && mock.open_range_calls().is_empty(),
        "the password gate fires before any remote work"
    );
}
