//! RED-phase tests for `cydrive_core::upload_queue`. All bodies are
//! expected to panic with "not yet implemented" until the GREEN phase
//! lands.
//!
//! Contract under test (design doc «上传队列（core）» + compat contracts
//! 6/7): 0-byte jobs skip the transport, FloodWait sleeps the exact
//! server-provided seconds and retries the whole upload without counting
//! toward degradation, other failures back off exponentially (capped at
//! `max_backoff`) and degrade after `max_attempts` consecutive failures,
//! the local cache copy is deleted only after success (fixing the Python
//! unconditional-delete bug), and `requeue_pending` re-enqueues pending
//! rows whose local copy still exists.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cydrive_core::cache::CacheManager;
use cydrive_core::database::{FileUpsert, MetaDatabase};
use cydrive_core::rel_path::RelPath;
use cydrive_core::transport::mock::{MockTransport, UploadAction};
use cydrive_core::transport::{CloudTransport, TransportError, UploadJob};
use cydrive_core::upload_queue::{
    decide_retry, spawn_queue, QueueError, QueueStats, RetryDecision, RetryPolicy,
    UploadQueueConfig,
};

/// The retry policy every queue integration test runs with: sub-millis
/// backoffs (tests stay fast) and degradation after 3 consecutive
/// non-FloodWait failures.
fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
        max_attempts: 3,
    }
}

/// Queue config for integration tests: one worker (deterministic call
/// order) and the per-test chunk size.
fn test_cfg(chunk_size_bytes: u64) -> UploadQueueConfig {
    UploadQueueConfig {
        workers: 1,
        queue_capacity: 16,
        retry: fast_retry(),
        chunk_size_bytes,
    }
}

/// Real temp environment: SQLite db + cache tree + pre-connected mock
/// transport. The first tuple item keeps the temp dir alive.
async fn test_env_with_mock(
    mock: MockTransport,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    Arc<MockTransport>,
) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
    let cache = CacheManager::new(dir.path().join("cache"), 1 << 20);
    let mock = Arc::new(mock);
    // The queue itself never calls connect(); pre-connect the shared mock
    // so its upload gate is open for the workers.
    mock.connect().await.expect("pre-connect mock transport");
    (dir, db, cache, mock)
}

/// `test_env_with_mock` with the default always-Ok mock script.
async fn test_env() -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    CacheManager,
    Arc<MockTransport>,
) {
    test_env_with_mock(MockTransport::builder().build()).await
}

/// Inserts a `files` row with plausible real-world field values.
fn seed_row(
    db: &MetaDatabase,
    rel: &str,
    size: i64,
    chunk_count: i64,
    is_uploaded: bool,
    telegram_msg_id: Option<i64>,
) {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent = rel_path.parent().expect("non-root path");
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir: parent.as_str().to_string(),
        size,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id,
        is_uploaded,
        is_cached: !is_uploaded,
        is_encrypted: false,
        chunk_count,
        mime_type: Some("application/octet-stream".to_string()),
    })
    .expect("seed files row");
}

/// Seeds a pending row (is_uploaded=false, is_cached=true) plus its real
/// bytes in the mirrored cache tree; returns the local cache path.
fn seed_pending(
    db: &MetaDatabase,
    cache: &CacheManager,
    rel: &str,
    bytes: &[u8],
    chunk_count: i64,
) -> PathBuf {
    seed_row(db, rel, bytes.len() as i64, chunk_count, false, None);
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local = cache.local_path(&rel_path);
    fs::create_dir_all(local.parent().expect("local parent dir")).expect("create cache dirs");
    fs::write(&local, bytes).expect("write cache file");
    local
}

/// The UploadJob a worker would derive for a seeded row.
fn job_for(
    cache: &CacheManager,
    rel: &str,
    size: u64,
    chunk_count: u32,
    chunk_size: u64,
) -> UploadJob {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let local_path = cache.local_path(&rel_path);
    UploadJob {
        rel_path,
        local_path,
        size,
        chunk_count,
        chunk_size,
    }
}

/// 1. First non-FloodWait failure (n=1) retries after exactly the
///    initial backoff.
#[tokio::test]
async fn first_non_flood_failure_retries_with_initial_backoff() {
    let policy = RetryPolicy {
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(5),
        max_attempts: 5,
    };
    let err = TransportError::Disconnected("socket closed".into());

    assert!(
        matches!(
            decide_retry(&policy, &err, 1),
            RetryDecision::RetryAfter(d) if d == Duration::from_secs(1)
        ),
        "n=1 backs off by exactly initial_backoff"
    );
}

/// 2. Backoff doubles per consecutive failure and caps at max_backoff
///    (pure function: nothing sleeps here).
#[tokio::test]
async fn backoff_doubles_and_caps_at_max() {
    let policy = RetryPolicy {
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(5),
        max_attempts: 20,
    };
    let err = TransportError::Disconnected("down".into());

    assert!(
        matches!(
            decide_retry(&policy, &err, 2),
            RetryDecision::RetryAfter(d) if d == Duration::from_secs(2)
        ),
        "n=2 doubles the initial backoff"
    );
    // 1s * 2^9 = 512s, far above the 5s cap.
    assert!(
        matches!(
            decide_retry(&policy, &err, 10),
            RetryDecision::RetryAfter(d) if d == Duration::from_secs(5)
        ),
        "n=10 is clamped to max_backoff"
    );
}

/// 3. FloodWait uses the server seconds exactly — shorter than the
///    initial backoff or longer than the cap — and never degrades, no
///    matter the failure streak.
#[tokio::test]
async fn flood_wait_uses_server_seconds_exactly_and_never_degrades() {
    let policy = RetryPolicy {
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(5),
        max_attempts: 2,
    };

    // Below the initial backoff: 0s still means RetryAfter(0).
    assert!(
        matches!(
            decide_retry(&policy, &TransportError::FloodWait { seconds: 0 }, 1),
            RetryDecision::RetryAfter(d) if d == Duration::ZERO
        ),
        "FloodWait(0) retries immediately, not after initial_backoff"
    );
    // Above the max backoff: no clamping.
    assert!(
        matches!(
            decide_retry(&policy, &TransportError::FloodWait { seconds: 7 }, 1),
            RetryDecision::RetryAfter(d) if d == Duration::from_secs(7)
        ),
        "FloodWait(7) sleeps 7s even beyond max_backoff"
    );
    // A huge streak of failures still never degrades a FloodWait.
    assert!(
        matches!(
            decide_retry(&policy, &TransportError::FloodWait { seconds: 7 }, 999),
            RetryDecision::RetryAfter(d) if d == Duration::from_secs(7)
        ),
        "FloodWait never degrades, regardless of consecutive_failures"
    );
}

/// 4. Non-FloodWait failure at n == max_attempts degrades; below it still
///    retries.
#[tokio::test]
async fn non_flood_failure_at_max_attempts_degrades() {
    let policy = RetryPolicy {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
        max_attempts: 3,
    };
    let err = TransportError::Remote("chat gone".into());

    assert!(
        matches!(decide_retry(&policy, &err, 3), RetryDecision::Degrade),
        "n == max_attempts degrades"
    );
    assert!(
        matches!(decide_retry(&policy, &err, 2), RetryDecision::RetryAfter(_)),
        "n below max_attempts still retries"
    );
}

/// 5. Single-chunk success: row flipped to uploaded, chunk row written
///    with the receipt data, cache copy deleted, stats counted.
#[tokio::test]
async fn success_marks_db_chunks_and_deletes_cache() {
    let (_dir, db, cache, mock) = test_env().await;
    let local = seed_pending(&db, &cache, "/docs/hello.bin", b"hello cydrive", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/docs/hello.bin", 13, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/docs/hello.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded);
    assert_eq!(row.telegram_msg_id, Some(1));
    assert!(!row.is_cached, "cache flag cleared on success");
    assert_eq!(row.chunk_count, 1, "chunk_count refreshed from the receipt");

    let chunks = db.get_chunks_by_file_id(row.id).expect("read chunks");
    assert_eq!(chunks.len(), 1, "one chunk row");
    assert_eq!(chunks[0].chunk_index, 0);
    assert_eq!(chunks[0].telegram_msg_id, Some(1));
    assert_eq!(chunks[0].size, 13);

    assert!(!local.exists(), "local cache copy deleted after success");
    assert_eq!(
        handle.stats(),
        QueueStats {
            enqueued: 1,
            succeeded: 1,
            degraded: 0,
            retries: 0,
        }
    );
}

/// 6. Multi-chunk success (7 B / chunk_size 3): one row per chunk, msg
///    ids 1..=3, the last chunk carrying the remainder byte.
#[tokio::test]
async fn multi_chunk_success_records_per_chunk_rows() {
    let (_dir, db, cache, mock) = test_env().await;
    seed_pending(&db, &cache, "/docs/data.bin", b"abcdefg", 3);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(3));

    handle
        .enqueue(job_for(&cache, "/docs/data.bin", 7, 3, 3))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/docs/data.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded);
    assert_eq!(row.telegram_msg_id, Some(1), "contract 5: chunk 0's msg id");
    assert_eq!(row.size, 7, "size refreshed from the receipt");

    let chunks = db.get_chunks_by_file_id(row.id).expect("read chunks");
    let rows: Vec<(i64, Option<i64>, i64)> = chunks
        .iter()
        .map(|c| (c.chunk_index, c.telegram_msg_id, c.size))
        .collect();
    assert_eq!(
        rows,
        vec![(0, Some(1), 3), (1, Some(2), 3), (2, Some(3), 1)],
        "per-chunk rows keep their own sizes, last chunk is the remainder"
    );
}

/// 7. FloodWait: sleep the scripted 0s, retry the whole upload, succeed.
#[tokio::test]
async fn flood_wait_sleeps_then_retries_whole_upload() {
    let mock = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: TransportError::FloodWait { seconds: 0 },
        })
        .upload_action(UploadAction::Ok)
        .build();
    let (_dir, db, cache, mock) = test_env_with_mock(mock).await;
    let local = seed_pending(&db, &cache, "/flood.bin", b"flooding", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/flood.bin", 8, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/flood.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "FloodWait retry ends in success");
    assert_eq!(mock.upload_calls().len(), 2, "whole upload retried once");
    assert_eq!(handle.stats().retries, 1);
    assert!(
        !local.exists(),
        "cache copy deleted after the eventual success"
    );
}

/// 8. Transient non-FloodWait failure: back off once, then succeed.
#[tokio::test]
async fn transient_error_backs_off_then_succeeds() {
    let mock = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: TransportError::Disconnected("x".into()),
        })
        .upload_action(UploadAction::Ok)
        .build();
    let (_dir, db, cache, mock) = test_env_with_mock(mock).await;
    let local = seed_pending(&db, &cache, "/transient.bin", b"transient", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/transient.bin", 9, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/transient.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "backoff retry ends in success");
    assert_eq!(handle.stats().retries, 1);
    assert!(!local.exists());
}

/// 9. Three consecutive failures (= max_attempts) degrade the job: no
///    fourth upload attempt (even though the exhausted script would
///    answer Ok), local file kept, row stays pending.
#[tokio::test]
async fn consecutive_failures_degrade_and_stop_retrying() {
    let mock = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: TransportError::Disconnected("down 1".into()),
        })
        .upload_action(UploadAction::Fail {
            error: TransportError::Disconnected("down 2".into()),
        })
        .upload_action(UploadAction::Fail {
            error: TransportError::Disconnected("down 3".into()),
        })
        .build();
    let (_dir, db, cache, mock) = test_env_with_mock(mock).await;
    let local = seed_pending(&db, &cache, "/doomed.bin", b"doomed!", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/doomed.bin", 7, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    assert_eq!(
        mock.upload_calls().len(),
        3,
        "degraded after max_attempts: no 4th call even though the script is exhausted"
    );
    let row = db
        .get_file("/doomed.bin")
        .expect("db read")
        .expect("row exists");
    assert!(!row.is_uploaded, "row stays pending after degradation");
    assert!(local.exists(), "degraded local file is kept");
    assert_eq!(
        handle.stats(),
        QueueStats {
            enqueued: 1,
            succeeded: 0,
            degraded: 1,
            retries: 2,
        }
    );
}

/// 10. 0-byte job: the transport is never called (contract: 0-byte
///     uploads skip the remote); the row is persisted as success without
///     a msg id and the empty local file is removed.
#[tokio::test]
async fn zero_byte_job_skips_transport() {
    let (_dir, db, cache, mock) = test_env().await;
    let local = seed_pending(&db, &cache, "/empty.txt", b"", 0);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/empty.txt", 0, 0, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    assert!(mock.upload_calls().is_empty(), "transport never called");
    let row = db
        .get_file("/empty.txt")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "0-byte counts as uploaded");
    assert_eq!(row.telegram_msg_id, None);
    assert_eq!(row.chunk_count, 0, "row chunk_count kept");
    assert!(!local.exists(), "empty local copy deleted");
    assert_eq!(handle.stats().succeeded, 1);
}

/// 11. Job whose rel_path has no DB row: degrade without touching the
///     remote; there is no local file to delete, and nothing panics.
#[tokio::test]
async fn unknown_rel_path_degrades_without_upload() {
    let (_dir, db, cache, mock) = test_env().await;
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/ghost.txt", 5, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    assert!(
        mock.upload_calls().is_empty(),
        "no remote call for an unknown row"
    );
    assert_eq!(handle.stats().degraded, 1);
    assert!(
        db.get_file("/ghost.txt").expect("db read").is_none(),
        "unknown rows are not created"
    );
}

/// 12. enqueue() after shutdown() fails with QueueError::Closed.
#[tokio::test]
async fn enqueue_after_shutdown_returns_closed() {
    let (_dir, db, cache, mock) = test_env().await;
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle.shutdown().await;

    let result = handle.enqueue(job_for(&cache, "/late.txt", 5, 1, 64)).await;
    assert!(
        matches!(result, Err(QueueError::Closed)),
        "enqueue after shutdown must be Closed, got: {result:?}"
    );
}

/// 13. requeue_pending: only pending rows with an existing local copy are
///     re-enqueued; rows without one and already-uploaded rows are
///     skipped.
#[tokio::test]
async fn requeue_pending_skips_missing_local_and_uploads_rest() {
    let (_dir, db, cache, mock) = test_env().await;
    seed_pending(&db, &cache, "/keep.bin", b"payload", 1);
    let gone_local = seed_pending(&db, &cache, "/gone.bin", b"lost", 1);
    fs::remove_file(&gone_local).expect("drop the local copy of B");
    // An already-uploaded row must not be re-enqueued.
    seed_row(&db, "/done.bin", 3, 1, true, Some(77));

    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));
    let enqueued = handle
        .requeue_pending(&cache)
        .await
        .expect("requeue pending");
    assert_eq!(enqueued, 1, "only the row with a local file is enqueued");
    handle.shutdown().await;

    let keep = db
        .get_file("/keep.bin")
        .expect("db read")
        .expect("row exists");
    assert!(keep.is_uploaded, "A uploaded through the requeued job");
    let gone = db
        .get_file("/gone.bin")
        .expect("db read")
        .expect("row exists");
    assert!(
        !gone.is_uploaded,
        "B stays pending: no local copy to upload"
    );
    let done = db
        .get_file("/done.bin")
        .expect("db read")
        .expect("row exists");
    assert!(done.is_uploaded);
    assert_eq!(
        done.telegram_msg_id,
        Some(77),
        "already-uploaded row untouched"
    );
}
