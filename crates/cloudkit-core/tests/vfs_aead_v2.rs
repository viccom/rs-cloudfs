//! RED-phase spec tests for the E-3 v2 wiring: with
//! `encryption_scheme = aead_v2` configured, the upload path streams the
//! plaintext through the v2 chunked-AEAD encryptor straight into the
//! transport (zero `.enc.tmp`, bounded per-frame memory), and hydration
//! dispatches on the row's scheme (`gcm` -> frozen v1 whole-file path
//! unchanged; `aead_v2` -> streaming decrypt; anything else -> an
//! actionable error).

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::EncryptionScheme;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::{CloudTransport, StorageError, UploadJob};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_crypto::{AeadV2, CryptoScheme};
use rusqlite::Connection;

/// The v2 default crypto chunk (1 MiB) — the upload path must feed the
/// transport in frames no larger than one crypto chunk + its tag.
const CRYPTO_CHUNK: usize = 1024 * 1024;
const TAG_SIZE: usize = 16;
/// v2 container header size (`CKCRYPT2` magic + version + salt + ...).
const V2_HEADER: usize = 34;

/// Deterministic payload: `n` bytes of a 251-cycle.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Recursively collects every file path under `root` whose name carries
/// the given suffix.
fn files_with_suffix(root: &std::path::Path, suffix: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if entry.file_name().to_string_lossy().ends_with(suffix) {
                hits.push(path);
            }
        }
    }
    hits
}

/// Vfs over a pre-connected mock transport with the given scheme and a
/// password; returns (dir, db, cache-root, mock, vfs). The cache root is
/// returned because `Vfs::new` consumes the manager; tests rebuild a
/// cheap twin over the same root for path math.
async fn vfs_env(
    scheme: EncryptionScheme,
    mock: MockTransport,
) -> (
    tempfile::TempDir,
    Arc<MetaDatabase>,
    PathBuf,
    Arc<MockTransport>,
    Arc<Vfs>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("db"));
    let cache_root = dir.path().join("cache");
    let cache = CacheManager::new(cache_root.clone(), 1 << 30);
    mock.connect().await.expect("pre-connect");
    let mock = Arc::new(mock);
    let transport: Arc<dyn CloudTransport> = Arc::clone(&mock) as _;
    let cfg = VfsConfig {
        chunk_size_bytes: 4096,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: Some("pw".to_string()),
        encryption_scheme: scheme,
        hydrate_timeout: std::time::Duration::from_secs(60),
    };
    let vfs = Arc::new(Vfs::new(Arc::clone(&db), cache, transport, cfg));
    (dir, db, cache_root, mock, vfs)
}

/// A cache twin for path math over the same root the VFS uses.
fn cache_twin(root: &std::path::Path) -> CacheManager {
    CacheManager::new(root.to_path_buf(), u64::MAX)
}

/// Concatenates the remote ciphertext of `rel` from the mock store.
fn remote_bytes(db: &MetaDatabase, mock: &MockTransport, rel: &str) -> Vec<u8> {
    let row = db.get_file(rel).expect("read").expect("row");
    let chunks = db.get_chunks_by_file_id(row.id).expect("chunks");
    assert!(!chunks.is_empty(), "uploaded rows carry chunk records");
    chunks
        .iter()
        .flat_map(|c| {
            mock.message(
                i32::try_from(c.telegram_msg_id.expect("msg id")).expect("narrow"),
            )
            .expect("stored chunk")
        })
        .collect()
}

#[tokio::test]
async fn aead_v2_upload_streams_and_hydrates_roundtrip() {
    // 8.5 MiB spans 9 crypto chunks — far beyond any single frame a
    // streaming implementation is allowed to buffer.
    let n = 8 * CRYPTO_CHUNK + CRYPTO_CHUNK / 2;
    let plaintext = pattern(n);
    let (dir, db, cache_root, mock, vfs) =
        vfs_env(EncryptionScheme::AeadV2, MockTransport::new()).await;

    let rel = RelPath::new("/big.bin").expect("rel");
    vfs.put(&rel, &plaintext, 1.0).await.expect("put");
    vfs.shutdown().await; // drain the queue to a terminal state

    // Row metadata: uploaded, flagged encrypted, scheme recorded.
    let row = db.get_file("/big.bin").expect("read").expect("row");
    assert!(row.is_uploaded);
    assert!(row.is_encrypted);
    assert_eq!(row.encryption_scheme, "aead_v2");
    assert_eq!(row.size, n as i64, "row keeps the plaintext size");

    // The upload went through the STREAM face, not the file face.
    assert_eq!(mock.stream_upload_calls().len(), 1, "one stream upload");
    assert!(
        mock.upload_calls().is_empty(),
        "v2 must not stage a ciphertext file for transport.upload()"
    );

    // Zero .enc.tmp anywhere in the cache tree.
    assert!(
        files_with_suffix(&cache_root, ".enc.tmp").is_empty(),
        "v2 upload creates no .enc.tmp staging file"
    );

    // Memory-granularity evidence: 8.5 MiB of plaintext flowed through in
    // frames bounded by one crypto chunk + tag (the crypto-side
    // granularity contract, now pinned at the transport seam).
    let peak = mock.max_stream_frame();
    assert!(peak > 0, "the stream face observed frames");
    assert!(
        peak <= CRYPTO_CHUNK + TAG_SIZE,
        "peak frame {} exceeds one crypto chunk + tag",
        peak
    );

    // The remote bytes are a genuine v2 container that decrypts back to
    // the plaintext (container check, independent of the hydrate path).
    let ciphertext = remote_bytes(&db, &mock, "/big.bin");
    assert_eq!(ciphertext.len(), V2_HEADER + n + 9 * TAG_SIZE, "v2 size formula");
    let scheme = AeadV2::new();
    let mut decrypted = Vec::new();
    scheme
        .decrypt_stream("pw", &mut ciphertext.as_slice(), &mut decrypted)
        .expect("container decrypts");
    assert_eq!(decrypted, plaintext);

    // Hydrate (cache miss forced by removing the local copy) returns the
    // plaintext bytes through the v2 streaming path.
    std::fs::remove_file(cache_twin(&cache_root).local_path(&rel)).expect("drop cache copy");
    let path = vfs.hydrate(&rel).await.expect("hydrate");
    let mut roundtrip = Vec::new();
    std::fs::File::open(&path)
        .expect("open hydrated")
        .read_to_end(&mut roundtrip)
        .expect("read hydrated");
    assert_eq!(roundtrip, plaintext, "hydrate roundtrip is byte-exact");
    // No staging residue of either family survives a successful hydrate.
    assert!(
        files_with_suffix(&cache_root, ".tmp").is_empty(),
        "no .tmp residue after hydrate"
    );
    let _ = dir;
}

#[tokio::test]
async fn aead_v2_upload_failure_keeps_plaintext_and_leaves_no_enc_tmp() {
    // Guard (green from the start on the v1 path — same failure
    // semantics): every attempt fails, then degrade. The plaintext
    // cache copy must survive and no .enc.tmp may ever appear — on v2
    // there is no staging file even mid-failure. Three scripted
    // failures match the env's max_attempts = 3 so the script cannot
    // run dry into its implicit Ok default.
    let mock = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("scripted".to_string()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("scripted".to_string()),
        })
        .upload_action(UploadAction::Fail {
            error: StorageError::Unavailable("scripted".to_string()),
        })
        .build();
    let (_dir, db, cache_root, _mock, vfs) = vfs_env(EncryptionScheme::AeadV2, mock).await;

    let rel = RelPath::new("/f.bin").expect("rel");
    vfs.put(&rel, b"payload", 1.0).await.expect("put");
    vfs.shutdown().await; // drain to the degraded terminal state

    let row = db.get_file("/f.bin").expect("read").expect("row");
    assert!(!row.is_uploaded, "degraded row stays pending");
    assert!(
        cache_twin(&cache_root).local_path(&rel).exists(),
        "plaintext copy survives the failed upload"
    );
    assert!(
        files_with_suffix(&cache_root, ".enc.tmp").is_empty(),
        "no .enc.tmp on the failure path either"
    );
    assert_eq!(vfs.queue_stats().degraded, 1);
}

#[tokio::test]
async fn gcm_row_hydrates_through_the_frozen_v1_path_under_a_v2_config() {
    // Guard: a row encrypted with v1 (scheme column "gcm") hydrates via
    // the frozen whole-file path even while the config says aead_v2 —
    // the read path dispatches on the row, never on the config.
    let (dir, db, _cache_root, mock, vfs) =
        vfs_env(EncryptionScheme::AeadV2, MockTransport::new()).await;

    // Hand-build a v1-encrypted uploaded row: encrypt with the frozen v1
    // primitive, store the ciphertext as one mock message, write the
    // files/chunks rows with scheme gcm.
    let plaintext = b"gcm legacy payload".to_vec();
    let ciphertext = cloudkit_crypto::v1::encrypt("pw", &plaintext);
    let rel = RelPath::new("/legacy.bin").expect("rel");
    let seed_path = dir.path().join("seed.v1");
    std::fs::write(&seed_path, &ciphertext).expect("write seed");
    let receipt = mock
        .upload(&UploadJob {
            rel_path: rel.clone(),
            local_path: seed_path,
            size: ciphertext.len() as u64,
            chunk_count: 1,
            chunk_size: 4096,
        })
        .await
        .expect("seed upload");
    let msg_id = i64::from(receipt.first_msg_id);

    db.upsert_file_scheme(
        &FileUpsert {
            rel_path: "/legacy.bin".to_string(),
            name: "legacy.bin".to_string(),
            parent_dir: "/".to_string(),
            size: plaintext.len() as i64,
            mtime: 1.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: Some(msg_id),
            is_uploaded: true,
            is_cached: false,
            is_encrypted: true,
            chunk_count: 1,
            mime_type: None,
        },
        "gcm",
    )
    .expect("seed row");
    let row = db.get_file("/legacy.bin").expect("read").expect("row");
    db.upsert_chunk(row.id, 0, msg_id, ciphertext.len() as i64, None)
        .expect("seed chunk");

    let path = vfs.hydrate(&rel).await.expect("v1 hydrate under v2 config");
    let mut roundtrip = Vec::new();
    std::fs::File::open(&path)
        .expect("open")
        .read_to_end(&mut roundtrip)
        .expect("read");
    assert_eq!(roundtrip, plaintext, "v1 rows keep the frozen decrypt path");
}

#[tokio::test]
async fn unknown_scheme_hydrate_fails_with_an_actionable_error() {
    let (dir, _db, cache_root, _mock, vfs) =
        vfs_env(EncryptionScheme::AeadV2, MockTransport::new()).await;

    // A normal v2 upload first (gives us a real encrypted row)...
    let rel = RelPath::new("/odd.bin").expect("rel");
    vfs.put(&rel, b"payload", 1.0).await.expect("put");
    vfs.shutdown().await;
    // ...then corrupt the row's scheme to a value this build cannot know.
    {
        let conn = Connection::open(dir.path().join("meta.db")).expect("raw open");
        conn.execute(
            "UPDATE files SET encryption_scheme = 'rot13' WHERE rel_path = '/odd.bin'",
            [],
        )
        .expect("scheme overwrite");
    }
    // The successful upload already removed the local copy (hydrate is
    // a guaranteed miss); tolerate a still-present copy for robustness.
    let _ = std::fs::remove_file(cache_twin(&cache_root).local_path(&rel));
    let err = vfs.hydrate(&rel).await.expect_err("unknown scheme must fail");
    let message = format!("{err}");
    assert!(
        message.contains("rot13") && message.contains("gcm") && message.contains("aead_v2"),
        "the error names the bad value and both known schemes: {message}"
    );
}
