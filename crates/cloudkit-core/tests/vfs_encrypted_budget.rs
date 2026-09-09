//! E-5 spec tests: the hydrate download budget of ENCRYPTED rows must not
//! be derived from the row's `size` — under the Python contract (R6) that
//! size is the PLAINTEXT length, while the remote artifact is the
//! ciphertext container (v1: salt + nonce + plaintext + tag; v2: 34 B
//! header + plaintext + n x 16 B tag), always LONGER than the plaintext.
//! A transport that enforces the [`RemoteHandle::total_size`] budget as a
//! hard cap (`ck-telegram` `open()` wraps its frames in
//! `serve_range(frames, 0, total_size)`) would trim the ciphertext short
//! and the final AEAD tag check would fail — the real-machine E-5 defect
//! (push ok, pull -> `decryption failed`, 2621440 B plaintext budget vs
//! 2621522 B container, 82 bytes short).
//!
//! The plain [`MockTransport`] historically ignored `total_size` entirely
//! (the unit-test blind spot that let this ship), so these tests drive the
//! VFS through [`BudgetTrimTransport`] — an honest, telegram-parity budget
//! enforcer that stays correct independently of the mock's own honesty.

use std::path::PathBuf;
use std::sync::Arc;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::EncryptionScheme;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{
    ByteStream, Capabilities, ChatCap, CloudTransport, InboundCap, RemoteHandle, StorageError,
    UploadJob, UploadReceipt,
};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use futures_util::StreamExt;

/// The v2 default crypto chunk (1 MiB) and container constants (mirrors
/// `tests/vfs_aead_v2.rs`).
const CRYPTO_CHUNK: usize = 1024 * 1024;
const TAG_SIZE: usize = 16;
const V2_HEADER: usize = 34;
/// v1 whole-file container overhead: 16 B salt + 12 B nonce + 16 B GCM tag.
const V1_OVERHEAD: usize = 44;

/// Deterministic payload: `n` bytes of a 251-cycle.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Telegram-parity budget enforcement (E-5): [`RemoteHandle::total_size`]
/// is a HARD cap on what `open()` yields — at most that many bytes flow,
/// any over-read is trimmed, a short read under the cap is tolerated
/// (exactly `ck-telegram::stream::serve_range`'s contract, as used by the
/// real `open()`). Everything else delegates to the inner mock untouched.
struct BudgetTrimTransport {
    inner: Arc<MockTransport>,
}

#[async_trait::async_trait]
impl CloudTransport for BudgetTrimTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        self.inner.connect().await
    }

    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        self.inner.upload(job).await
    }

    async fn upload_stream(
        &self,
        job: &UploadJob,
        data: ByteStream,
    ) -> Result<UploadReceipt, StorageError> {
        self.inner.upload_stream(job, data).await
    }

    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        let mut inner = self.inner.open(file).await?;
        let mut budget = file.total_size;
        let mut frames = Vec::new();
        while let Some(frame) = inner.next().await {
            let frame = frame?;
            if budget == 0 {
                break;
            }
            let take = frame.len().min(budget as usize);
            frames.push(Ok(frame.slice(..take)));
            budget -= take as u64;
        }
        Ok(Box::pin(futures_util::stream::iter(frames)))
    }

    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        self.inner.open_range(file, off, len).await
    }

    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
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

/// Vfs over a pre-connected, budget-honest transport with the given
/// scheme and password; returns (dir, db, cache-root, mock, vfs). The
/// cache root is returned because `Vfs::new` consumes the manager; tests
/// rebuild a cheap twin over the same root for path math.
async fn budget_env(
    scheme: EncryptionScheme,
    password: Option<String>,
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
    let mock = MockTransport::new();
    mock.connect().await.expect("pre-connect");
    let mock = Arc::new(mock);
    let transport: Arc<dyn CloudTransport> = Arc::new(BudgetTrimTransport {
        inner: Arc::clone(&mock),
    });
    let cfg = VfsConfig {
        chunk_size_bytes: 4096,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: password,
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
            mock.message(c.telegram_msg_id.expect("msg id"))
                .expect("stored chunk")
        })
        .collect()
}

/// v1 (gcm) encrypted roundtrip under a budget-honest transport: the row
/// keeps its PLAINTEXT size (Python contract, R6), the remote artifact is
/// the 44-bytes-longer v1 ciphertext — and hydrate must still download
/// the FULL ciphertext and decrypt it. Before the fix the plaintext-sized
/// budget trimmed the ciphertext and v1 decryption failed the same way v2
/// did on the real machine (the v1 defect is latent only because no
/// real-machine pull ever exercised an encrypted row before E-5).
#[tokio::test]
async fn v1_encrypted_plaintext_sized_row_hydrates_over_budget_transport() {
    let n = 100;
    let plaintext = pattern(n);
    let (_dir, db, cache_root, _mock, vfs) =
        budget_env(EncryptionScheme::Gcm, Some("pw".to_string())).await;

    let rel = RelPath::new("/v1.bin").expect("rel");
    vfs.put(&rel, &plaintext, 1.0).await.expect("put");
    vfs.shutdown().await;

    // Python contract pinned: the row records the PLAINTEXT length.
    let row = db.get_file("/v1.bin").expect("read").expect("row");
    assert!(row.is_uploaded);
    assert!(row.is_encrypted);
    assert_eq!(row.size, n as i64, "row keeps the plaintext size (R6)");

    // The remote artifact is the v1 ciphertext container (longer).
    let ciphertext = remote_bytes(&db, &_mock, "/v1.bin");
    assert_eq!(ciphertext.len(), n + V1_OVERHEAD, "v1 container length");

    // Cache miss forces a hydrate through the budget-honest transport.
    let _ = std::fs::remove_file(cache_twin(&cache_root).local_path(&rel));
    let path = vfs.hydrate(&rel).await.expect("v1 hydrate under budget");
    let roundtrip = std::fs::read(&path).expect("read hydrated");
    assert_eq!(roundtrip, plaintext, "hydrate roundtrip is byte-exact");
}

/// v2 (aead_v2) encrypted roundtrip under a budget-honest transport, at
/// the exact E-5 real-machine shape: 2621440 B plaintext -> 2621522 B
/// container (34 + 3 crypto chunks), the old plaintext-sized budget
/// trimmed 82 bytes and the final GCM tag failed. The row keeps its
/// plaintext size; hydrate must pull the whole container and stream-decrypt.
#[tokio::test]
async fn v2_encrypted_plaintext_sized_row_hydrates_over_budget_transport() {
    let n = 2 * CRYPTO_CHUNK + CRYPTO_CHUNK / 2; // 2621440 — the E-5 size
    let plaintext = pattern(n);
    let (_dir, db, cache_root, mock, vfs) =
        budget_env(EncryptionScheme::AeadV2, Some("pw".to_string())).await;

    let rel = RelPath::new("/v2.bin").expect("rel");
    vfs.put(&rel, &plaintext, 1.0).await.expect("put");
    vfs.shutdown().await;

    // Python contract pinned: the row records the PLAINTEXT length, and
    // the upload went through the streaming face.
    let row = db.get_file("/v2.bin").expect("read").expect("row");
    assert!(row.is_uploaded);
    assert!(row.is_encrypted);
    assert_eq!(row.encryption_scheme, "aead_v2");
    assert_eq!(row.size, n as i64, "row keeps the plaintext size (R6)");
    assert_eq!(mock.stream_upload_calls().len(), 1, "one stream upload");

    // The remote artifact is the v2 container: header + plaintext + one
    // tag per crypto chunk (3 chunks at this size).
    let ciphertext = remote_bytes(&db, &mock, "/v2.bin");
    assert_eq!(
        ciphertext.len(),
        V2_HEADER + n + 3 * TAG_SIZE,
        "v2 container length"
    );

    // Cache miss forces a hydrate through the budget-honest transport:
    // before the fix this failed with Crypto(AuthFailed) — the offline
    // mirror of the real-machine E-5 symptom.
    let _ = std::fs::remove_file(cache_twin(&cache_root).local_path(&rel));
    let path = vfs.hydrate(&rel).await.expect("v2 hydrate under budget");
    let roundtrip = std::fs::read(&path).expect("read hydrated");
    assert_eq!(roundtrip, plaintext, "hydrate roundtrip is byte-exact");
}

/// Control: UNENCRYPTED rows keep the row-size budget — the remote
/// artifact IS the row's bytes, so the budget-honest transport must pass
/// them through unchanged (the fix only unbounds ENCRYPTED rows).
#[tokio::test]
async fn plaintext_row_hydrates_within_its_row_size_budget() {
    let plaintext = pattern(200);
    let (_dir, db, cache_root, _mock, vfs) = budget_env(EncryptionScheme::Gcm, None).await;

    let rel = RelPath::new("/plain.bin").expect("rel");
    vfs.put(&rel, &plaintext, 1.0).await.expect("put");
    vfs.shutdown().await;

    let row = db.get_file("/plain.bin").expect("read").expect("row");
    assert!(row.is_uploaded);
    assert!(!row.is_encrypted);
    assert_eq!(row.size, plaintext.len() as i64);

    let _ = std::fs::remove_file(cache_twin(&cache_root).local_path(&rel));
    let path = vfs.hydrate(&rel).await.expect("plaintext hydrate");
    let roundtrip = std::fs::read(&path).expect("read hydrated");
    assert_eq!(roundtrip, plaintext, "budget equals artifact for plaintext");
}
