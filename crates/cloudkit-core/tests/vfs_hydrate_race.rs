//! P3 snapshot write-back race (inherited from rs-CyDrive, sync review
//! High-③ downgraded): `hydrate`/eviction used to rebuild the WHOLE row
//! from the snapshot read at hydrate start (`cached_upsert`) and upsert it
//! back just to flip `is_cached`. Any concurrent metadata update landing
//! inside the download window (a PUT overwrite recording a new size /
//! msg id, sync applying a remote version) was silently resurrected to
//! the stale snapshot values — and would then spread via push.
//!
//! The race window is exercised deterministically (no timers, no sleeps):
//! a wrapper transport parks inside `open()` until the test releases it,
//! so the concurrent upsert provably lands after the snapshot was taken
//! and before the flag flip writes back. Contract under test: the
//! completed hydration flips ONLY the cached flag; every other column of
//! the concurrently-updated row survives verbatim.

use std::sync::Arc;

use async_trait::async_trait;
use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{
    ByteStream, Capabilities, CloudTransport, RemoteHandle, StorageError, UploadJob,
    UploadReceipt,
};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};

/// VfsConfig for the race test (single worker, fast retry, no password).
fn race_cfg() -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 64,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
        hydrate_timeout: std::time::Duration::from_secs(180),
    }
}

/// Transport wrapper that parks the FIRST `open()` call: it signals
/// `entered` (the hydrate task is now inside the download, past its row
/// snapshot) and then waits on `go` before delegating to the inner
/// transport. Everything else is a pass-through. The parking is
/// channel/notify-based — no timers — so the interleaving below is
/// scheduling-deterministic on a current-thread runtime.
struct StallingOpenTransport {
    inner: Arc<MockTransport>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    go: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CloudTransport for StallingOpenTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        self.inner.connect().await
    }

    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        self.inner.upload(job).await
    }

    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        let _ = self.entered.send(());
        self.go.notified().await;
        self.inner.open(file).await
    }

    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        self.inner.open_range(file, off, len).await
    }

    async fn delete_remote(&self, msg_id: i32) -> Result<(), StorageError> {
        self.inner.delete_remote(msg_id).await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// Seeds one uploaded, uncached row (`/race.txt`, 9 bytes, one chunk at
/// the mock remote) and returns nothing — the test derives its
/// assertions from the db rows it upserts itself.
async fn seed_uploaded_uncached(db: &MetaDatabase, mock: &Arc<MockTransport>) {
    let rel_path = RelPath::new("/race.txt").expect("valid rel path");
    let dir = tempfile::tempdir().expect("seed scratch dir");
    let local_path = dir.path().join("race.txt");
    std::fs::write(&local_path, b"OLD-BYTES").expect("write seed scratch file");
    let receipt = mock
        .upload(&UploadJob {
            rel_path: rel_path.clone(),
            local_path,
            size: 9,
            chunk_count: 1,
            chunk_size: 64,
        })
        .await
        .expect("seed upload");
    let file_id = db
        .upsert_file(&FileUpsert {
            rel_path: "/race.txt".to_string(),
            name: "race.txt".to_string(),
            parent_dir: "/".to_string(),
            size: 9,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(i64::from(receipt.first_msg_id)),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: false,
            chunk_count: 1,
            mime_type: None,
        })
        .expect("seed files row");
    db.upsert_chunk(file_id, 0, i64::from(receipt.first_msg_id), 9, None)
        .expect("seed chunk row");
}

/// A metadata update landing inside the download window (the row-level
/// effect of a PUT overwrite: new size, new mtime, new chunk-0 msg id)
/// must survive the hydration's completion — which may only flip the
/// `is_cached` flag, never write any other column back.
#[tokio::test]
async fn hydrate_concurrent_update_survives_flag_flip() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_root = dir.path().join("cache");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(cache_root.clone(), 1 << 20);
    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    seed_uploaded_uncached(&db, &mock).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let go = Arc::new(tokio::sync::Notify::new());
    let transport: Arc<dyn CloudTransport> = Arc::new(StallingOpenTransport {
        inner: mock,
        entered: entered_tx,
        go: Arc::clone(&go),
    });
    let vfs = Arc::new(Vfs::new(Arc::clone(&db), cache, transport, race_cfg()));
    let rel = RelPath::new("/race.txt").expect("valid rel path");

    // Hydrate runs on a spawned task: it reads the row snapshot, misses
    // the cache and parks inside the wrapper's `open()`.
    let hydrate_vfs = Arc::clone(&vfs);
    let hydrate_rel = rel.clone();
    let task = tokio::spawn(async move { hydrate_vfs.hydrate(&hydrate_rel).await });
    entered_rx.recv().await.expect("hydrate parked inside open()");

    // Download window is open: a concurrent upsert overwrites the row's
    // metadata (PUT-overwrite shape — new size/mtime/msg id are all Some
    // or direct values so the coalesce columns actually change).
    db.upsert_file(&FileUpsert {
        rel_path: "/race.txt".to_string(),
        name: "race.txt".to_string(),
        parent_dir: "/".to_string(),
        size: 5,
        mtime: 1_800_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: Some(999),
        is_uploaded: true,
        is_cached: false,
        is_encrypted: false,
        chunk_count: 1,
        mime_type: None,
    })
    .expect("concurrent upsert inside the download window");

    // Release the download; the hydration completes against the OLD
    // remote bytes (the handle was opened before the overwrite — the
    // bytes are not the race's concern) and flips the cached flag.
    go.notify_one();
    let hydrated = task
        .await
        .expect("hydrate task joins")
        .expect("hydrate succeeds");
    let bytes = std::fs::read(&hydrated).expect("read hydrated copy");
    assert_eq!(bytes, b"OLD-BYTES", "remote bytes served through the old handle");
    vfs.shutdown().await;

    // The row must be the concurrently-updated row plus is_cached=true —
    // NOT the stale snapshot the hydrate started from.
    let row = db
        .get_file("/race.txt")
        .expect("db read")
        .expect("row exists");
    assert_eq!(row.size, 5, "concurrent size update survives the flag flip");
    assert_eq!(
        row.telegram_msg_id,
        Some(999),
        "concurrent msg id update survives the flag flip"
    );
    assert_eq!(
        row.mtime, 1_800_000_000.0,
        "concurrent mtime update survives the flag flip"
    );
    assert!(row.is_cached, "hydration still flips the cached flag");
    assert!(row.is_uploaded, "upload state carried by the concurrent row");
}
