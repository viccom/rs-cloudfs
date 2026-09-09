//! RED-phase tests for the K4 remote-delete wiring (Phase 2 Batch B3b
//! 段二b): `Vfs::remove_file` gates on the transport's `remote_delete`
//! capability bit.
//!
//! Contract under test (plan §6 unit 4 / K4):
//!
//! - **bit on, remote delete succeeds** → the remote object dies FIRST,
//!   then the row and the cached copy (order is the contract: a local
//!   delete ahead of a remote failure would orphan the row's truth);
//! - **bit on, remote answers NotFound** → tolerated as success (the
//!   idempotent end state — the object is already gone);
//! - **bit on, remote fails otherwise** → the call aborts with an
//!   actionable error and the row + cache copy are KEPT; exactly one
//!   retry happens before giving up;
//! - **bit off** → zero change: the remote keeps its objects (the
//!   telegram/mock legacy semantics — the existing `vfs_ops` suite is
//!   the unchanged guard rail; the explicit-off case here pins that the
//!   gate keys on the BIT, not on the transport type).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{Capabilities, CloudTransport, RemoteHandle, StorageError};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig, VfsError};

// ------------------------------------------------------------- helpers ---

/// VfsConfig for these tests: one worker (deterministic order), fast
/// retry, no encryption (mirrors `vfs_ops`).
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

/// The mock capability face the K4 gate consumes: the three legacy bits
/// PLUS `remote_delete` (a baidu/local-shaped declaration).
fn remote_delete_caps() -> Capabilities {
    Capabilities {
        range_read: true,
        inbound: true,
        chat: true,
        remote_delete: true,
        ..Capabilities::none()
    }
}

/// Real temp environment: SQLite db + mirrored cache tree + the
/// scriptable, capability-injected mock transport, pre-connected.
async fn test_env(
    caps: Capabilities,
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
    let cache = CacheManager::new(cache_root.clone(), u64::MAX);
    let mock = Arc::new(MockTransport::builder().capabilities(caps).build());
    mock.connect().await.expect("pre-connect mock transport");
    (dir, db, cache, cache_root, mock)
}

fn build_vfs(db: &Arc<MetaDatabase>, cache: CacheManager, mock: &Arc<MockTransport>) -> Vfs {
    let transport: Arc<dyn CloudTransport> = mock.clone();
    Vfs::new(db.clone(), cache, transport, test_cfg())
}

/// Environment over a delete-scripted, remote_delete-capable mock: the
/// scripted outcomes are consumed one per `delete_remote` call (a
/// one-retry consumer eats two).
async fn scripted_delete_env(
    script: Vec<Result<(), StorageError>>,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    Arc<MockTransport>,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(cache_root.clone(), u64::MAX);
    let mut builder = MockTransport::builder().capabilities(remote_delete_caps());
    for outcome in script {
        builder = builder.delete_action(outcome);
    }
    let mock = Arc::new(builder.build());
    mock.connect().await.expect("pre-connect mock transport");
    (dir, db, cache, mock)
}

/// Polls the queue until `expected` uploads succeeded and the local
/// staging copy is gone (the `vfs_ops` wait helper — put + drain +
/// re-hydrate leaves an uploaded row that owns a cache copy again).
async fn wait_for_drained_uploads(vfs: &Vfs, paths: &CacheManager, rel: &RelPath, expected: u64) {
    for _ in 0..2500 {
        if vfs.queue_stats().succeeded >= expected && !paths.local_path(rel).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the queue never drained {expected} upload(s) for {rel}");
}

/// Puts one file, drains the upload and re-hydrates, leaving the
/// canonical deletable state: an uploaded row owning a cache copy.
async fn uploaded_file(vfs: &Vfs, paths: &CacheManager, rel: &RelPath, bytes: &[u8]) {
    vfs.put(rel, bytes, 1_700_000_000.0)
        .await
        .expect("put accepted");
    wait_for_drained_uploads(vfs, paths, rel, 1).await;
    vfs.hydrate(rel).await.expect("hydrate restores the copy");
}

// -------------------------------------------------------------- tests ---

/// Bit on + remote delete succeeds: the remote message dies, then the
/// row and the cached copy — all three observables land.
#[tokio::test]
async fn remote_delete_success_removes_remote_row_and_cache() {
    let (_dir, db, cache, cache_root, mock) = test_env(remote_delete_caps()).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock);
    let rel = RelPath::new("/gone.bin").expect("valid rel path");

    uploaded_file(&vfs, &paths, &rel, b"payload").await;
    vfs.remove_file(&rel).await.expect("remove_file accepted");

    assert!(
        db.get_file("/gone.bin").expect("db read").is_none(),
        "the files row is gone"
    );
    assert!(!paths.local_path(&rel).exists(), "the cache copy is gone");
    assert_eq!(
        mock.deleted(),
        vec![1],
        "the remote message was deleted first (mock ids start at 1)"
    );

    vfs.shutdown().await;
}

/// Bit on + the remote object is ALREADY gone: NotFound is the desired
/// end state, so the delete succeeds and the row still dies.
#[tokio::test]
async fn remote_delete_not_found_is_tolerated_as_success() {
    let (_dir, db, cache, cache_root, mock) = test_env(remote_delete_caps()).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock);
    let rel = RelPath::new("/pre-removed.bin").expect("valid rel path");

    uploaded_file(&vfs, &paths, &rel, b"payload").await;
    // Remove the remote object out-of-band (e.g. another client deleted
    // it): the row's handle now points at nothing.
    let row = db
        .get_file("/pre-removed.bin")
        .expect("db read")
        .expect("row");
    let handle = RemoteHandle {
        first_msg_id: row.telegram_msg_id.expect("uploaded row carries the id"),
        chunk_msg_ids: Vec::new(),
        total_size: row.size.max(0) as u64,
        path: Some(rel.clone()),
    };
    mock.delete_remote(&handle)
        .await
        .expect("out-of-band remote delete");

    vfs.remove_file(&rel)
        .await
        .expect("remove_file must tolerate the already-gone remote object (idempotent end state)");
    assert!(
        db.get_file("/pre-removed.bin").expect("db read").is_none(),
        "the row is gone despite the remote NotFound"
    );

    vfs.shutdown().await;
}

/// Bit on + the remote refuses twice: the call fails with an actionable
/// error naming the path and the kept row, and BOTH the row and its
/// cache copy survive.
#[tokio::test]
async fn remote_delete_failure_keeps_row_with_actionable_error() {
    let refused = || Err(StorageError::Unavailable("backend down".into()));
    let (dir, db, cache, mock) = scripted_delete_env(vec![refused(), refused()]).await;
    let paths = CacheManager::new(dir.path().join("cache"), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock);
    let rel = RelPath::new("/kept.bin").expect("valid rel path");

    uploaded_file(&vfs, &paths, &rel, b"payload").await;

    let error = vfs
        .remove_file(&rel)
        .await
        .expect_err("the refused remote delete must abort the removal");
    let message = error.to_string();
    assert!(
        matches!(error, VfsError::Transport(_)),
        "the refusal surfaces as a transport error, got: {error:?}"
    );
    assert!(
        message.contains("/kept.bin"),
        "the error names the path, got: {message}"
    );
    assert!(
        message.to_lowercase().contains("kept"),
        "the error states the row was kept, got: {message}"
    );

    assert!(
        db.get_file("/kept.bin").expect("db read").is_some(),
        "the row survives the refused remote delete"
    );
    assert!(
        paths.local_path(&rel).exists(),
        "the cache copy survives the refused remote delete"
    );

    vfs.shutdown().await;
}

/// Bit on + the remote fails ONCE then accepts the retry: the delete
/// completes (exactly one retry — two scripted outcomes consumed).
#[tokio::test]
async fn remote_delete_retries_once_and_then_succeeds() {
    let transient = Err(StorageError::RateLimited { retry_after: None });
    let (dir, db, cache, mock) = scripted_delete_env(vec![transient]).await;
    let paths = CacheManager::new(dir.path().join("cache"), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock);
    let rel = RelPath::new("/retry.bin").expect("valid rel path");

    uploaded_file(&vfs, &paths, &rel, b"payload").await;
    vfs.remove_file(&rel)
        .await
        .expect("the transient remote failure must be retried once");

    assert!(
        db.get_file("/retry.bin").expect("db read").is_none(),
        "the row is gone after the retry succeeded"
    );
    assert_eq!(mock.deleted(), vec![1], "the retry performed the deletion");

    vfs.shutdown().await;
}

/// Bit OFF (a telegram-shaped declaration): the remote keeps its
/// messages even though the transport CAN delete (the mock's
/// delete_remote works) — the gate keys on the declared bit alone.
#[tokio::test]
async fn capability_off_keeps_remote_objects_zero_change() {
    let legacy_caps = Capabilities {
        range_read: true,
        inbound: true,
        chat: true,
        ..Capabilities::none()
    };
    let (_dir, db, cache, cache_root, mock) = test_env(legacy_caps).await;
    let paths = CacheManager::new(cache_root.clone(), u64::MAX);
    let vfs = build_vfs(&db, cache, &mock);
    let rel = RelPath::new("/legacy.bin").expect("valid rel path");

    uploaded_file(&vfs, &paths, &rel, b"payload").await;
    vfs.remove_file(&rel).await.expect("remove_file accepted");

    assert!(
        db.get_file("/legacy.bin").expect("db read").is_none(),
        "the row is gone (legacy semantics)"
    );
    assert!(
        mock.deleted().is_empty(),
        "the remote message is KEPT: the off bit means zero change"
    );
    assert_eq!(
        mock.message_names(),
        vec!["legacy.bin".to_string()],
        "the remote object is still there"
    );

    vfs.shutdown().await;
}
