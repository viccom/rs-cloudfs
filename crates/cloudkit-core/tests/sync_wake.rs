//! RED-phase tests for the change-wakeup hooks of the quasi-realtime sync
//! batch (doorbell model, approved design 2026-09-05): every local `files`
//! table change must ring the VFS's sync [`tokio::sync::Notify`] so the CLI
//! sync task can run a pass immediately instead of waiting for the 300s
//! fallback tick.
//!
//! Hook contract under test:
//! - `Vfs::sync_notifier()` hands out the shared `Arc<Notify>`;
//! - `Vfs::wake_sync()` rings it (`notify_one` semantics: a wake arriving
//!   while a pass is in flight parks a permit and triggers a follow-up
//!   pass; several wakes coalesce into one pass — the desired merging);
//! - hooks fire at: `put` (enqueue accepted), `remove_file` (success),
//!   `create_dir`, `index_inbound` (row written) and the upload queue's
//!   `persist_success` / 0-byte persist (worker wrote the uploaded row —
//!   the "upload success pushes" key point);
//! - non-events must NOT ring: a failed `remove_file` (NotFound), and
//!   `hydrate` (its only db write flips the local-only `is_cached` flag,
//!   which the sync payload deliberately excludes).
//!
//! Determinism: each test builds its own Vfs (fresh `Notify`), so no
//! permit from an earlier test can leak into an assertion. Wake
//! assertions use `Notified::enable()` BEFORE the trigger, so the notify
//! is assigned to the asserted future even though nothing polled it yet
//! (the documented anti-lost-wakeup pattern).

use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::{CloudTransport, InboundFile, RemoteHandle, StorageError};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig, VfsError};

// ------------------------------------------------------------- helpers ---

/// VfsConfig for integration tests: one worker (deterministic order),
/// capacity 16, fast retry, no encryption (the established pattern of
/// `tests/vfs_ops.rs`).
fn test_cfg() -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 1900 * 1024 * 1024,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        hydrate_timeout: Duration::from_secs(30),
    }
}

/// Real temp environment: SQLite db + cache tree + pre-connected mock
/// transport, assembled into a Vfs. Returns the tempdir keeper too.
async fn test_vfs(mock: MockTransport) -> (tempfile::TempDir, Vfs, Arc<MetaDatabase>) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(dir.path().join("cache"), 1 << 20);
    let mock = Arc::new(mock);
    mock.connect().await.expect("pre-connect mock transport");
    let transport: Arc<dyn CloudTransport> = mock;
    let vfs = Vfs::new(Arc::clone(&db), cache, transport, test_cfg());
    (dir, vfs, db)
}

/// `test_vfs` with the default always-Ok mock script.
async fn ok_vfs() -> (tempfile::TempDir, Vfs, Arc<MetaDatabase>) {
    test_vfs(MockTransport::builder().build()).await
}

/// A valid virtual path.
fn rel(path: &str) -> RelPath {
    RelPath::new(path).expect("valid rel path")
}

/// Seeds one uploaded file row at `path` (chunks unnecessary for the wake
/// hooks under test).
fn seed_uploaded_row(db: &MetaDatabase, path: &str, msg_id: i64) {
    let rel_path = rel(path);
    let parent_dir = rel_path
        .parent()
        .map(|parent| parent.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir,
        size: 10,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(msg_id),
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("seed uploaded row");
}

/// Drain any wake permit stored by seeding writes (`notify_one` keeps a
/// permit when no waiter is enabled), so the assertion below can only
/// pass on the operation under test — not on seeding echoes.
async fn drain_stale_wake_permits(notifier: &tokio::sync::Notify) {
    while tokio::time::timeout(Duration::from_millis(50), notifier.notified())
        .await
        .is_ok()
    {}
}

// ------------------------------------------------------ positive hooks ---

/// `put` rings the wake once the enqueue is accepted (the enqueue-wake
/// hook fires synchronously, keeping `commit_put`'s no-await-after-enqueue
/// contract).
#[tokio::test]
async fn put_wakes_sync_after_enqueue() {
    let (_dir, vfs, _db) = ok_vfs().await;

    let notifier = vfs.sync_notifier();

    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();
    vfs.put(&rel("/hello.txt"), b"hello", 1_700_000_000.0)
        .await
        .expect("put accepted");
    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("put must ring the sync wake");
}

/// `remove_file` rings after the successful delete.
#[tokio::test]
async fn remove_file_wakes_after_success() {
    let (_dir, vfs, db) = ok_vfs().await;
    seed_uploaded_row(&db, "/bye.txt", 9);

    let notifier = vfs.sync_notifier();
    drain_stale_wake_permits(&notifier).await;

    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();
    vfs.remove_file(&rel("/bye.txt"))
        .await
        .expect("remove succeeds");
    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("remove_file must ring the sync wake");
}

/// `create_dir` rings once the directory row is written.
#[tokio::test]
async fn create_dir_wakes_after_row_write() {
    let (_dir, vfs, _db) = ok_vfs().await;

    let notifier = vfs.sync_notifier();

    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();
    vfs.create_dir(&rel("/docs")).expect("create_dir succeeds");
    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("create_dir must ring the sync wake");
}

/// `index_inbound` rings once the inbound row lands in the files table.
#[tokio::test]
async fn index_inbound_wakes_after_row_write() {
    let (_dir, vfs, _db) = ok_vfs().await;

    let notifier = vfs.sync_notifier();

    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();
    vfs.index_inbound(InboundFile {
        filename: "photo.jpg".to_string(),
        handle: RemoteHandle {
            first_msg_id: 5,
            chunk_msg_ids: vec![5],
            total_size: 10,
        },
    })
    .await
    .expect("index_inbound succeeds");
    tokio::time::timeout(Duration::from_secs(1), wake.as_mut())
        .await
        .expect("index_inbound must ring the sync wake");
}

/// The upload queue's success persist rings a SECOND wake (after put's
/// enqueue wake) — the "upload success pushes" hook. The enqueue wake is
/// consumed first, so the second completion can only come from the
/// worker's persist. The same scenario then proves `hydrate` never rings
/// (its db write flips only the local-only `is_cached` flag, which the
/// sync payload excludes).
#[tokio::test]
async fn upload_success_wakes_twice_and_hydrate_never_wakes() {
    let (_dir, vfs, _db) = ok_vfs().await;
    let path = rel("/twice.txt");

    // wake 1: enqueue acceptance (fires synchronously inside put)
    let notifier = vfs.sync_notifier();

    let enqueue_wake = notifier.notified();
    tokio::pin!(enqueue_wake);
    enqueue_wake.as_mut().enable();
    vfs.put(&path, b"payload", 1_700_000_000.0)
        .await
        .expect("put accepted");
    tokio::time::timeout(Duration::from_secs(1), enqueue_wake.as_mut())
        .await
        .expect("put must ring the enqueue wake");

    // wake 2: the worker's success persist (the drain waits for it)
    let notifier = vfs.sync_notifier();

    let persist_wake = notifier.notified();
    tokio::pin!(persist_wake);
    persist_wake.as_mut().enable();
    vfs.shutdown().await; // drain: upload -> persist_success -> ring
    tokio::time::timeout(Duration::from_secs(1), persist_wake.as_mut())
        .await
        .expect("upload success must ring its own sync wake");

    // hydrate (cache copy was deleted by the successful upload, so this
    // is the cold path with a real db write) must NOT ring.
    let notifier = vfs.sync_notifier();

    let hydrate_wake = notifier.notified();
    tokio::pin!(hydrate_wake);
    hydrate_wake.as_mut().enable();
    vfs.hydrate(&path)
        .await
        .expect("hydrate re-downloads the uploaded payload");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), hydrate_wake.as_mut())
            .await
            .is_err(),
        "hydrate must not ring the sync wake (is_cached is local-only)"
    );
}

// ------------------------------------------------------ negative hooks ---

/// A failed `remove_file` (row missing) must not ring.
#[tokio::test]
async fn failed_remove_does_not_wake() {
    let (_dir, vfs, _db) = ok_vfs().await;

    let notifier = vfs.sync_notifier();

    let wake = notifier.notified();
    tokio::pin!(wake);
    wake.as_mut().enable();
    let error = vfs
        .remove_file(&rel("/missing.txt"))
        .await
        .expect_err("no row at the path");
    assert!(matches!(error, VfsError::NotFound(_)), "{error:?}");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), wake.as_mut())
            .await
            .is_err(),
        "a failed remove must not ring the sync wake"
    );
}

/// A degraded upload (mock scripted failure, retries exhausted) reaches a
/// terminal state without ever ringing the success hook — only put's
/// enqueue wake fires. The mock consumes its script in order and an
/// exhausted script behaves as Ok, so the failure is scripted once per
/// attempt (`max_attempts = 3` in [`test_cfg`]).
#[tokio::test]
async fn degraded_upload_never_rings_the_success_hook() {
    let fail = || UploadAction::Fail {
        error: StorageError::Unavailable("injected upload failure".to_string()),
    };
    let (_dir, vfs, _db) = test_vfs(
        MockTransport::builder()
            .upload_action(fail())
            .upload_action(fail())
            .upload_action(fail())
            .build(),
    )
    .await;

    let notifier = vfs.sync_notifier();

    let enqueue_wake = notifier.notified();
    tokio::pin!(enqueue_wake);
    enqueue_wake.as_mut().enable();
    vfs.put(&rel("/doomed.txt"), b"payload", 1_700_000_000.0)
        .await
        .expect("put accepted");
    tokio::time::timeout(Duration::from_secs(1), enqueue_wake.as_mut())
        .await
        .expect("put must ring the enqueue wake");

    vfs.shutdown().await; // drain: retries -> degrade, no success persist

    let notifier = vfs.sync_notifier();

    let success_wake = notifier.notified();
    tokio::pin!(success_wake);
    success_wake.as_mut().enable();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), success_wake.as_mut())
            .await
            .is_err(),
        "a degraded upload must not ring the success hook"
    );
}
