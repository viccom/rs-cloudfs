//! RED-phase tests for `cloudkit_core::upload_queue`. All bodies are
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
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::{
    Capabilities, ChatCap, CloudTransport, InboundCap, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_core::upload_queue::{
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
        encryption_password: None,
        encryption_scheme: cloudkit_core::config::EncryptionScheme::default(),
    }
}

/// [`test_cfg`] plus an encryption password: the queue stages an
/// encrypted copy for rows flagged `is_encrypted` (Python AND semantics:
/// both the config password and the row flag are required).
fn encrypted_cfg(chunk_size_bytes: u64, password: &str) -> UploadQueueConfig {
    let mut cfg = test_cfg(chunk_size_bytes);
    cfg.encryption_password = Some(password.to_string());
    // The callers below assert the v1 whole-file ciphertext math (44 B
    // GCM overhead, 16 B boundary chunks) — pin Gcm explicitly instead
    // of riding the config default, which is now AeadV2.
    cfg.encryption_scheme = cloudkit_core::config::EncryptionScheme::Gcm;
    cfg
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

/// Seeds a pending *encrypted* row (is_encrypted=true, the flag half of
/// the Python AND condition) plus its plaintext bytes in the mirrored
/// cache tree; returns the local cache path.
fn seed_encrypted_pending(
    db: &MetaDatabase,
    cache: &CacheManager,
    rel: &str,
    bytes: &[u8],
    chunk_count: i64,
) -> PathBuf {
    let rel_path = RelPath::new(rel).expect("valid rel path");
    let parent = rel_path.parent().expect("non-root path");
    db.upsert_file(&FileUpsert {
        rel_path: rel_path.as_str().to_string(),
        name: rel_path.name().to_string(),
        parent_dir: parent.as_str().to_string(),
        size: bytes.len() as i64,
        mtime: 1_700_000_000.0,
        sha256: None,
        is_dir: false,
        telegram_msg_id: None,
        is_uploaded: false,
        is_cached: true,
        is_encrypted: true,
        chunk_count,
        mime_type: Some("application/octet-stream".to_string()),
    })
    .expect("seed encrypted files row");
    let local = cache.local_path(&rel_path);
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
    let err = StorageError::Unavailable("socket closed".into());

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
    let err = StorageError::Unavailable("down".into());

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
            decide_retry(&policy, &StorageError::RateLimited { retry_after: Some(Duration::from_secs(0)) }, 1),
            RetryDecision::RetryAfter(d) if d == Duration::ZERO
        ),
        "FloodWait(0) retries immediately, not after initial_backoff"
    );
    // Above the max backoff: no clamping.
    assert!(
        matches!(
            decide_retry(&policy, &StorageError::RateLimited { retry_after: Some(Duration::from_secs(7)) }, 1),
            RetryDecision::RetryAfter(d) if d == Duration::from_secs(7)
        ),
        "FloodWait(7) sleeps 7s even beyond max_backoff"
    );
    // A huge streak of failures still never degrades a FloodWait.
    assert!(
        matches!(
            decide_retry(&policy, &StorageError::RateLimited { retry_after: Some(Duration::from_secs(7)) }, 999),
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
    let err = StorageError::Unavailable("chat gone".into());

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
            error: StorageError::RateLimited {
                retry_after: Some(Duration::from_secs(0)),
            },
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
            error: StorageError::Unavailable("x".into()),
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
            error: StorageError::Unavailable("down 1".into()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 2".into()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 3".into()),
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

/// 14. Small-file success (13 B <= the 100 MB cap): the success upsert
///     stores the sha256 of the local plaintext, computed after remote
///     success but before the cache copy is deleted — the Python
///     baseline behavior that makes the sha-based ETag (contract 6)
///     reachable for fresh uploads. The expected digest is recomputed
///     from the seeded file with the same `chunker::sha256_file`
///     (known plaintext through the tool, not a hardcoded hex).
#[tokio::test]
async fn success_computes_sha256_for_small_files() {
    let (_dir, db, cache, mock) = test_env().await;
    let local = seed_pending(&db, &cache, "/docs/hashed.bin", b"hello cydrive", 1);
    let expected_sha =
        cloudkit_core::chunker::sha256_file(&local).expect("hash the seeded plaintext");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/docs/hashed.bin", 13, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/docs/hashed.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        row.sha256,
        Some(expected_sha),
        "13-byte upload stores the plaintext sha256 in its row"
    );
}

/// 15. Oversized files skip the sha256 side-computation: the size gate is
///     the pure `should_hash`, tested at its boundaries (exactly 100 MB
///     hashes, 100 MB + 1 skips, 0 hashes — the Python baseline computes
///     the empty digest for 0-byte files). A full >100 MB upload is not
///     faked with a giant fixture; the gate's use on the success path is
///     covered by test 14 plus code review.
#[test]
fn oversized_files_skip_sha256() {
    use cloudkit_core::upload_queue::{should_hash, SHA256_MAX_BYTES};

    assert_eq!(
        SHA256_MAX_BYTES,
        100 * 1024 * 1024,
        "frozen Python baseline cap (telegram_client.py:160)"
    );
    assert!(should_hash(SHA256_MAX_BYTES), "exactly at the cap hashes");
    assert!(
        !should_hash(SHA256_MAX_BYTES + 1),
        "one byte over the cap skips"
    );
    assert!(should_hash(0), "0-byte files hash (empty digest)");
    assert!(
        !should_hash(u64::MAX),
        "no overflow surprise far over the cap"
    );
}

/// 16. Encrypted upload round-trip: 5 B plaintext at chunk_size 16 becomes
///     a 49 B ciphertext (44 B crypto overhead) split into 4 remote chunks;
///     concatenating the stored messages in chunk order yields ciphertext
///     that decrypts back to the plaintext. Salt/nonce are fresh per run,
///     so only the decrypt round-trip (not a fixed blob) can be asserted.
#[tokio::test]
async fn encrypted_upload_stores_decryptable_ciphertext() {
    let (_dir, db, cache, mock) = test_env().await;
    seed_encrypted_pending(&db, &cache, "/enc/a.bin", b"12345", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, encrypted_cfg(16, "pw"));

    handle
        .enqueue(job_for(&cache, "/enc/a.bin", 5, 1, 16))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/enc/a.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "encrypted upload succeeds");
    assert_eq!(
        row.chunk_count, 4,
        "ciphertext (49 B) split at 16 B per chunk"
    );

    let chunks = db.get_chunks_by_file_id(row.id).expect("read chunks");
    let mut joined = Vec::new();
    for chunk in &chunks {
        let id = chunk.telegram_msg_id.expect("chunk msg id");
        joined.extend_from_slice(&mock.message(id).expect("stored remote message"));
    }
    assert_eq!(joined.len(), 49, "5 B plaintext + 44 B crypto overhead");
    assert_eq!(
        cloudkit_core::crypto::decrypt("pw", &joined).expect("decrypt the joined ciphertext"),
        b"12345",
        "remote chunks concatenate into decryptable ciphertext"
    );
}

/// 17. Encrypted row shape (Python telegram_client.py:235-242): the row
///     keeps the *plaintext* size while `chunk_count` counts *ciphertext*
///     chunks, and every chunk row carries its ciphertext boundary sizes
///     (49 B at chunk_size 16 -> [16, 16, 16, 1]).
#[tokio::test]
async fn encrypted_row_keeps_plaintext_size_and_cipher_chunk_count() {
    let (_dir, db, cache, _mock) = test_env().await;
    seed_encrypted_pending(&db, &cache, "/enc/shape.bin", b"12345", 1);
    let transport: Arc<dyn CloudTransport> = _mock.clone();
    let handle = spawn_queue(db.clone(), transport, encrypted_cfg(16, "pw"));

    handle
        .enqueue(job_for(&cache, "/enc/shape.bin", 5, 1, 16))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/enc/shape.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded);
    assert_eq!(row.size, 5, "row size stays the plaintext size");
    assert_eq!(row.chunk_count, 4, "chunk_count counts ciphertext chunks");
    assert!(row.is_encrypted, "row keeps its encrypted flag");
    assert_eq!(row.telegram_msg_id, Some(1), "chunk-0 msg id recorded");

    let chunks = db.get_chunks_by_file_id(row.id).expect("read chunks");
    let sizes: Vec<i64> = chunks.iter().map(|c| c.size).collect();
    assert_eq!(
        sizes,
        vec![16, 16, 16, 1],
        "chunk rows carry ciphertext boundary sizes"
    );
}

/// 18. Encrypted success stores the *plaintext* sha256 (Python
///     telegram_client.py:160: the digest is computed on the local
///     plaintext before encryption), recomputed here from the seeded file
///     with the same `chunker::sha256_file`.
#[tokio::test]
async fn encrypted_upload_hashes_plaintext_sha256() {
    let (_dir, db, cache, mock) = test_env().await;
    let local = seed_encrypted_pending(&db, &cache, "/enc/hash.bin", b"12345", 1);
    let expected_sha =
        cloudkit_core::chunker::sha256_file(&local).expect("hash the seeded plaintext");
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, encrypted_cfg(16, "pw"));

    handle
        .enqueue(job_for(&cache, "/enc/hash.bin", 5, 1, 16))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/enc/hash.bin")
        .expect("db read")
        .expect("row exists");
    assert_eq!(
        row.sha256,
        Some(expected_sha),
        "encrypted upload stores the plaintext digest, not a ciphertext one"
    );
}

/// 19. Encrypted success cleans up after itself: the `.enc.tmp` staging
///     file is gone (deleted on success and failure alike — the Python
///     `finally` semantics), the plaintext cache copy is deleted (success
///     only, the standing fix of the Python unconditional delete), and no
///     file of any kind survives in the cache tree.
#[tokio::test]
async fn encrypted_success_cleans_temp_and_cache() {
    let (dir, db, cache, mock) = test_env().await;
    let local = seed_encrypted_pending(&db, &cache, "/enc/clean.bin", b"12345", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, encrypted_cfg(16, "pw"));

    handle
        .enqueue(job_for(&cache, "/enc/clean.bin", 5, 1, 16))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    assert!(
        !local.exists(),
        "plaintext cache copy deleted after success"
    );
    let mut files = Vec::new();
    collect_files(&dir.path().join("cache"), &mut files);
    assert!(
        files.is_empty(),
        "cache tree holds no .enc.tmp/.tmp/other residue: {files:?}"
    );
}

/// 20. Encryption-off regression guard (contrast with test 16): with no
///     password configured the very same 5 B row uploads as plaintext —
///     the remote message equals the plaintext verbatim and the row is
///     not flagged encrypted.
#[tokio::test]
async fn encryption_disabled_uploads_plaintext_unchanged() {
    let (_dir, db, cache, mock) = test_env().await;
    let local = seed_pending(&db, &cache, "/plain/a.bin", b"12345", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(16));

    handle
        .enqueue(job_for(&cache, "/plain/a.bin", 5, 1, 16))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/plain/a.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded);
    assert!(!row.is_encrypted, "row not flagged encrypted");
    assert_eq!(row.chunk_count, 1, "5 B plaintext is a single chunk");
    assert_eq!(
        mock.message(1).expect("stored remote message"),
        b"12345",
        "remote holds the plaintext verbatim"
    );
    assert!(!local.exists(), "local copy deleted after success");
}

/// 21. The plaintext sha256 is computed BEFORE the first upload attempt,
///     not after remote success. Field bug (real-machine log): two writes
///     to the same path race — the first job's success-delete removes the
///     cache copy out from under the second job's post-upload hash, which
///     dies with os error 2 and stores None, losing the sha-ETag.
///     Harness: a delegating transport that deletes the local copy inside
///     the upload call (the exact loss window). The digest must already
///     be in hand when the upload completes, so the row still carries it.
struct DeleteOnUploadTransport {
    inner: Arc<MockTransport>,
}

#[async_trait::async_trait]
impl CloudTransport for DeleteOnUploadTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        self.inner.connect().await
    }

    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        let receipt = self.inner.upload(job).await?;
        let _ = fs::remove_file(&job.local_path);
        Ok(receipt)
    }

    async fn open(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open(file).await
    }

    async fn open_range(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open_range(file, off, len).await
    }

    async fn delete_remote(
        &self,
        handle: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<(), StorageError> {
        self.inner.delete_remote(handle).await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn as_inbound(&self) -> Option<&dyn InboundCap> {
        self.inner.as_inbound()
    }

    fn as_chat(&self) -> Option<&dyn ChatCap> {
        self.inner.as_chat()
    }
}

#[tokio::test]
async fn sha256_computed_before_upload_survives_local_delete() {
    let (_dir, db, cache, mock) = test_env().await;
    let local = seed_pending(&db, &cache, "/race/hashed.bin", b"hello cydrive", 1);
    let expected_sha =
        cloudkit_core::chunker::sha256_file(&local).expect("hash the seeded plaintext");
    let transport: Arc<dyn CloudTransport> = Arc::new(DeleteOnUploadTransport {
        inner: mock.clone(),
    });
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/race/hashed.bin", 13, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    let row = db
        .get_file("/race/hashed.bin")
        .expect("db read")
        .expect("row exists");
    assert!(row.is_uploaded, "upload itself succeeded");
    assert_eq!(
        row.sha256,
        Some(expected_sha),
        "digest computed before the upload survives the local delete"
    );
}

// ---------------------------------------------------------------------------
// Tier-1 degradation notification — plan contract C9: when a job degrades
// (retry exhaustion), the queue sends exactly one best-effort bot notice
// via `send_text`; a failing notification must never break the degrade
// path. The notice itself names the file, reports the failure and the
// attempt count ("upload failed after {n} attempts").
// ---------------------------------------------------------------------------

/// 22. Degradation sends exactly one `send_text` notice carrying the
///     rel_path, the word "failed" and the attempt count (3 ==
///     max_attempts). The row stays pending and the local copy is kept,
///     as the notice promises.
#[tokio::test]
async fn degrade_sends_bot_notification() {
    let mock = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 1".into()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 2".into()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 3".into()),
        })
        .build();
    let (_dir, db, cache, mock) = test_env_with_mock(mock).await;
    let local = seed_pending(&db, &cache, "/notified.bin", b"notify me", 1);
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/notified.bin", 9, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    assert_eq!(handle.stats().degraded, 1, "the job degraded");
    let texts = mock.sent_texts();
    assert_eq!(
        texts.len(),
        1,
        "exactly one degradation notice, no retry chatter: {texts:?}"
    );
    assert!(
        texts[0].contains("/notified.bin"),
        "the notice names the file: {}",
        texts[0]
    );
    assert!(
        texts[0].contains("failed"),
        "the notice reports the failure: {}",
        texts[0]
    );
    assert!(
        texts[0].contains("after 3 attempts"),
        "the attempt count equals max_attempts: {}",
        texts[0]
    );
    assert!(local.exists(), "degraded local file is kept");
    let row = db
        .get_file("/notified.bin")
        .expect("db read")
        .expect("row exists");
    assert!(!row.is_uploaded, "row stays pending after degradation");
}

/// Harness for test 23: a fully delegating wrapper whose `send_text`
/// always errors (counting attempts, so the red phase proves the notice
/// is even attempted); every other operation passes through untouched.
struct BrokenSendTextTransport {
    inner: Arc<MockTransport>,
    send_text_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl CloudTransport for BrokenSendTextTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        self.inner.connect().await
    }

    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        self.inner.upload(job).await
    }

    async fn open(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open(file).await
    }

    async fn open_range(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open_range(file, off, len).await
    }

    async fn delete_remote(
        &self,
        handle: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<(), StorageError> {
        self.inner.delete_remote(handle).await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn as_inbound(&self) -> Option<&dyn InboundCap> {
        self.inner.as_inbound()
    }

    fn as_chat(&self) -> Option<&dyn ChatCap> {
        Some(self)
    }
}

/// The broken reply surface itself (ChatCap since the Batch R split): the
/// attempt counter lives on the wrapper, the failure is what test 23
/// exercises.
#[async_trait::async_trait]
impl ChatCap for BrokenSendTextTransport {
    async fn send_text(&self, _text: &str) -> Result<(), StorageError> {
        self.send_text_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(StorageError::Unavailable("send_text is broken".into()))
    }
}

/// 23. A failing degradation notification is swallowed: the worker still
///     counts the degrade, keeps the row pending and the local copy, and
///     shuts down cleanly — no panic, no lost terminal state. The attempt
///     counter pins that the notice was attempted exactly once.
#[tokio::test]
async fn degrade_notification_failure_is_swallowed() {
    let mock = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 1".into()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 2".into()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("down 3".into()),
        })
        .build();
    let (_dir, db, cache, mock) = test_env_with_mock(mock).await;
    let local = seed_pending(&db, &cache, "/swallowed.bin", b"swallow me", 1);
    let send_text_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport: Arc<dyn CloudTransport> = Arc::new(BrokenSendTextTransport {
        inner: mock.clone(),
        send_text_calls: Arc::clone(&send_text_calls),
    });
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    handle
        .enqueue(job_for(&cache, "/swallowed.bin", 10, 1, 64))
        .await
        .expect("enqueue");
    handle.shutdown().await;

    assert_eq!(
        send_text_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the degrade path attempted exactly one notification"
    );
    assert_eq!(handle.stats().degraded, 1, "the degrade still counted");
    assert_eq!(handle.stats().succeeded, 0);
    let row = db
        .get_file("/swallowed.bin")
        .expect("db read")
        .expect("row exists");
    assert!(!row.is_uploaded, "row stays pending");
    assert!(local.exists(), "local copy kept");
}

// ---------------------------------------------------------------------------
// 24. MiniRedir 空 PUT 工件竞态（field log 2026-09-09，baidu demo
//     readme.txt）。Explorer/cp 的 MiniRedir 小文件链以「空 PUT → LOCK →
//     完整 PUT」开路：空 PUT 的 0 字节任务若在完整 PUT 更新行**之后**才被
//     worker 处理，persist_zero_byte 会用行的现值（已非 0）把行标记为已
//     上传——而远端什么都没有（幽灵上传），且 delete_local_copy 把完整
//     任务还要读的缓存副本删掉（现场日志：5× os error 2 后降级，无法
//     自愈）。process_job 顶部的行读取是新鲜的，row.size 即当前真相：
//     非 0 行上的 0 字节任务是过期工件，必须跳过——不持久化、不删缓存。
//     Harness：门控诚实传输——首个 upload 停在 Notify 门上（保证两个 PUT
//     都在任何 0 字节任务被处理前落定，把现场的不幸顺序变成确定性），
//     upload 内真实读取 local_path（真后端全体如此；mock 的慷慨会掩盖
//     缓存被删）。
// ---------------------------------------------------------------------------

struct GatedHonestUploadTransport {
    inner: Arc<MockTransport>,
    gate: Arc<tokio::sync::Notify>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    first_upload_seen: AtomicBool,
}

#[async_trait::async_trait]
impl CloudTransport for GatedHonestUploadTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        self.inner.connect().await
    }

    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        if !self
            .first_upload_seen
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = self.entered.send(());
            self.gate.notified().await;
        }
        // 真实后端（baidu/telegram/local）都读本地缓存副本；诚实读取
        // 让「缓存已被工件任务删除」以 io 错误浮出，而非被 mock 掩盖。
        std::fs::read(&job.local_path).map_err(|e| StorageError::Io(e.to_string()))?;
        self.inner.upload(job).await
    }

    async fn open(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open(file).await
    }

    async fn open_range(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open_range(file, off, len).await
    }

    async fn delete_remote(
        &self,
        handle: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<(), StorageError> {
        self.inner.delete_remote(handle).await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn as_inbound(&self) -> Option<&dyn InboundCap> {
        self.inner.as_inbound()
    }

    fn as_chat(&self) -> Option<&dyn ChatCap> {
        self.inner.as_chat()
    }
}

#[tokio::test]
async fn stale_empty_put_artifact_neither_phantom_uploads_nor_deletes_cache() {
    let (_dir, db, cache, mock) = test_env().await;
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Notify::new());
    let transport: Arc<dyn CloudTransport> = Arc::new(GatedHonestUploadTransport {
        inner: mock.clone(),
        gate: gate.clone(),
        entered: entered_tx,
        first_upload_seen: AtomicBool::new(false),
    });
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    // 任务 1：无关真实上传——worker 停在门上，后续任务全部排队。
    seed_pending(&db, &cache, "/busy.bin", b"busy!", 1);
    handle
        .enqueue(job_for(&cache, "/busy.bin", 5, 1, 64))
        .await
        .expect("enqueue busy");
    entered_rx.recv().await.expect("worker entered the gate");

    // MiniRedir 空 PUT 工件：行(size 0) + 空缓存副本 + 任务 A。
    let f_local = seed_pending(&db, &cache, "/f.bin", b"", 1);
    handle
        .enqueue(job_for(&cache, "/f.bin", 0, 1, 64))
        .await
        .expect("enqueue artifact");

    // 完整 PUT：缓存重写 + 行 size 0→3（pending）+ 任务 B。
    std::fs::write(&f_local, b"abc").expect("rewrite cache copy");
    seed_row(&db, "/f.bin", 3, 1, false, None);
    handle
        .enqueue(job_for(&cache, "/f.bin", 3, 1, 64))
        .await
        .expect("enqueue full");

    gate.notify_waiters();
    handle.shutdown().await;

    let row = db.get_file("/f.bin").expect("db read").expect("row exists");
    assert!(row.is_uploaded, "the full PUT must land");
    assert_eq!(
        row.telegram_msg_id,
        Some(2),
        "stale artifact must not phantom-upload: the row is identified by the \
         REAL job's receipt (busy=1, full=2); None means the 0-byte job marked \
         it uploaded without any remote artifact"
    );
    let chunks = db.get_chunks_by_file_id(row.id).expect("read chunks");
    assert_eq!(chunks.len(), 1, "the real job's chunk row exists");
    assert_eq!(chunks[0].telegram_msg_id, Some(2));
}

/// H2（幽灵 outstanding）：过期空 PUT 工件的 skip 路径也必须落到某个
/// 终态计数器——工件任务入队时已计入 enqueued，若 skip 时既不计
/// succeeded 也不计 degraded，则 enqueued − (succeeded + degraded)
/// 恒 ≥ 1，REMOVE 的排空判据永远等不到归零（LIST 的 pending 同理）。
/// 裁决：计入 degraded（本队列「未上传即终态」的既有语义，与无行/
/// 元数据读失败路径一致）；db 行归补全的 full PUT 任务所有。
#[tokio::test]
async fn stale_empty_put_artifact_reaches_terminal_queue_state() {
    let (_dir, db, cache, mock) = test_env().await;
    let transport: Arc<dyn CloudTransport> = mock.clone();
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    // MiniRedir 空 PUT 工件（行 size=0 + 空缓存副本），随后补全的
    // full PUT 已把行抢先更新为 size=3（真实链序：empty PUT → LOCK →
    // full PUT）；缓存副本重写为完整内容——skip 路径无权删它。
    let local = seed_pending(&db, &cache, "/f.bin", b"", 1);
    fs::write(&local, b"abc").expect("rewrite cache copy");
    seed_row(&db, "/f.bin", 3, 1, false, None);

    handle
        .enqueue(job_for(&cache, "/f.bin", 0, 1, 64))
        .await
        .expect("enqueue artifact");
    handle.shutdown().await;

    let stats = handle.stats();
    assert_eq!(stats.enqueued, 1);
    assert_eq!(stats.degraded, 1, "skipped artifact counts as degraded");
    assert_eq!(
        stats.succeeded + stats.degraded,
        stats.enqueued,
        "every enqueued job must reach a terminal state (the drain predicate)"
    );
    assert!(mock.upload_calls().is_empty(), "transport never called");
    let row = db.get_file("/f.bin").expect("db read").expect("row exists");
    assert!(
        !row.is_uploaded,
        "the superseding full PUT owns the outcome"
    );
    assert!(local.exists(), "cache copy kept for the superseding job");
}

// ---------------------------------------------------------------------------
// 25. K58-M1（worker panic 隔离）：`process_job` 内任一 panic（最现实形
//     态 = 驱动 `transport.upload` 的 bug）一旦把 worker task 带死，该
//     job 便永无终态计数——入队时已计 enqueued、succeeded/degraded 永不
//     落，`outstanding()` 恒 ≥ 1：K57 把它用作 REMOVE/REBUILD/DESTROY
//     的排空硬门禁，卷从此卸不掉、rebuild 拒收，直至重启。worker_loop
//     必须把 panic 转成该 job 的 degraded 终态（与 H2 的 skip 补
//     degraded 同族——本队列「放弃该 job」的既有语义：本地副本保留、
//     行保持 pending）并继续下一个 job：worker 不死。
//     Harness：委派包装 transport——upload 对指定 rel_path 首行 panic
//     （驱动 bug 注入缝，attempts 计数证明 job 确已抵达传输层），其余
//     调用原样透传：第二个文件走同一条 worker，钉「存活」。
//     注：mock 的 panic 消息经默认 hook 打到 stderr（libtest 只捕获测试
//     线程自身，tokio worker 线程上的 panic 不在捕获内）——接受该输出，
//     不静音全局 panic hook（跨测试进程级状态，静音会掩盖真实失败）。
// ---------------------------------------------------------------------------

struct ExplodingUploadTransport {
    inner: Arc<MockTransport>,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl CloudTransport for ExplodingUploadTransport {
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        if job.rel_path.as_str() == "/boom.bin" {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("worker exploded: K58_M1_MARKER");
        }
        self.inner.upload(job).await
    }

    async fn connect(&self) -> Result<(), StorageError> {
        self.inner.connect().await
    }

    async fn open(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open(file).await
    }

    async fn open_range(
        &self,
        file: &cloudkit_core::transport::RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        self.inner.open_range(file, off, len).await
    }

    async fn delete_remote(
        &self,
        handle: &cloudkit_core::transport::RemoteHandle,
    ) -> Result<(), StorageError> {
        self.inner.delete_remote(handle).await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn as_inbound(&self) -> Option<&dyn InboundCap> {
        self.inner.as_inbound()
    }

    fn as_chat(&self) -> Option<&dyn ChatCap> {
        self.inner.as_chat()
    }
}

#[tokio::test]
async fn panicking_upload_degrades_and_keeps_the_worker_alive() {
    let (_dir, db, cache, mock) = test_env().await;
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport: Arc<dyn CloudTransport> = Arc::new(ExplodingUploadTransport {
        inner: mock.clone(),
        attempts: Arc::clone(&attempts),
    });
    let handle = spawn_queue(db.clone(), transport, test_cfg(64));

    // 文件 A：transport.upload panic——该 job 仍必须到达终态。
    let boom_local = seed_pending(&db, &cache, "/boom.bin", b"boom", 1);
    handle
        .enqueue(job_for(&cache, "/boom.bin", 4, 1, 64))
        .await
        .expect("enqueue boom");

    // 轮询窗口（≤2s）：panic 确已抵达传输层且 outstanding 归零。现状
    // （红）：worker 死、终态永不落、outstanding 恒 1。
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let stats = handle.stats();
        if stats.outstanding() == 0
            && stats.degraded == 1
            && attempts.load(std::sync::atomic::Ordering::SeqCst) >= 1
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "panic never reached a terminal counter (ghost outstanding): \
             stats={:?} attempts={}",
            handle.stats(),
            attempts.load(std::sync::atomic::Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the panicked job degrades on its first attempt: no retry ladder for panics"
    );

    // worker 不死：第二个文件经同一 worker 正常上传成功。
    seed_pending(&db, &cache, "/after.bin", b"after", 1);
    handle
        .enqueue(job_for(&cache, "/after.bin", 5, 1, 64))
        .await
        .expect("enqueue after");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if handle.stats().succeeded == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the worker died with the panicking job: the second file never \
             uploaded (stats={:?})",
            handle.stats()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // panic 的 job = degraded 语义：本地副本保留、行保持 pending。
    assert!(
        boom_local.exists(),
        "panicked job keeps its local copy (degraded semantics)"
    );
    let boom_row = db
        .get_file("/boom.bin")
        .expect("db read")
        .expect("row exists");
    assert!(
        !boom_row.is_uploaded,
        "panicked job stays pending (degraded semantics)"
    );
    let after_row = db
        .get_file("/after.bin")
        .expect("db read")
        .expect("row exists");
    assert!(
        after_row.is_uploaded,
        "the surviving worker uploaded the second file"
    );

    handle.shutdown().await;
    assert_eq!(
        handle.stats(),
        QueueStats {
            enqueued: 2,
            succeeded: 1,
            degraded: 1,
            retries: 0,
        },
        "panic lands in exactly one terminal counter; the queue fully drains"
    );
}
