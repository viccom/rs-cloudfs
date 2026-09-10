//! K47 decrypting-transport tests (Phase 3.5-a E2): the
//! [`DecryptingTransport`] wrapper translates plaintext windows into
//! ciphertext spans over one bound remote object — lazy header pull +
//! one-shot key derivation on the first read, per-chunk AEAD
//! authentication, EOF clamping in plaintext coordinates — while the
//! write face and capability declaration delegate to the inner transport.
//!
//! Containers are real aead_v2 encodings (64 KiB crypto chunks — the
//! format guardrail floor) stored on the shared [`MockTransport`]; the
//! mock's `open_range_calls()` recorder pins the inner-read coordinates.

use std::sync::Arc;

use cloudkit_core::enc_stream::DecryptingTransport;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob};
use cloudkit_crypto::AeadV2;
use futures_util::StreamExt;

/// Crypto chunk size for test containers (the format guardrail floor).
const CS: u64 = 64 * 1024;
const PW: &str = "enc-stream 测试密码 🔐";
/// v2 container header size.
const HEADER: u64 = 34;

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Concatenates every frame of a byte stream; propagates the first error.
async fn drain(stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

/// Environment for one encrypted remote object: a pre-connected mock
/// holding the container as a single message, the production-shaped
/// handle (u64::MAX total_size sentinel — encrypted rows never budget
/// reads by it) and the wrapper over it. The container is ALWAYS
/// encrypted under [`PW`]; `wrapper_password` is what the wrapper will
/// try to decrypt with (differ only for wrong-password tests). Returns
/// the plaintext too.
async fn env(
    plain_len: usize,
    wrapper_password: &str,
) -> (
    Vec<u8>,
    Arc<MockTransport>,
    RemoteHandle,
    DecryptingTransport,
) {
    let plain = pattern(plain_len);
    let ct = AeadV2::with_chunk_size(CS as usize)
        .expect("chunk size is the guardrail floor")
        .encrypt(PW, &plain);

    let mock = Arc::new(MockTransport::new());
    mock.connect().await.expect("pre-connect mock transport");
    let dir = tempfile::tempdir().expect("seed scratch dir");
    let rel_path = RelPath::new("/container.bin").expect("valid rel path");
    let local_path = dir.path().join("container.bin");
    std::fs::write(&local_path, &ct).expect("write seed scratch file");
    let receipt = mock
        .upload(&UploadJob {
            rel_path,
            local_path,
            size: ct.len() as u64,
            chunk_count: 1,
            chunk_size: ct.len() as u64,
        })
        .await
        .expect("seed upload");

    let handle = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids,
        total_size: u64::MAX,
        path: None,
    };
    let wrapper = DecryptingTransport::new(
        mock.clone(),
        handle.clone(),
        plain_len as u64,
        wrapper_password.into(),
    );
    (plain, mock, handle, wrapper)
}

// --------------------------------------------------------- read face ------

#[tokio::test]
async fn random_windows_match_full_decrypt() {
    // 2.5 chunks + spill: cross-chunk windows and tail-chunk edges all in
    // one container.
    let plain_len = (2 * CS + CS / 2 + 100) as usize;
    let (plain, _mock, handle, wrapper) = env(plain_len, PW).await;

    // Deterministic LCG window sweep: mixed alignments, 1-byte windows,
    // whole-file, EOF edge.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 16
    };
    for _ in 0..30 {
        let s = next() % plain_len as u64;
        let e = s + 1 + next() % (plain_len as u64 - s);
        let got = drain(
            wrapper
                .open_range(&handle, s, e - s)
                .await
                .expect("open window"),
        )
        .await
        .expect("window bytes");
        assert_eq!(got, plain[s as usize..e as usize], "window [{s}, {e})");
    }
    // 1-byte window at a chunk boundary and the whole file.
    let one = drain(wrapper.open_range(&handle, CS - 1, 1).await.expect("open"))
        .await
        .expect("bytes");
    assert_eq!(one, plain[(CS - 1) as usize..CS as usize]);
    let whole = drain(wrapper.open(&handle).await.expect("open"))
        .await
        .expect("bytes");
    assert_eq!(whole, plain, "full-file read decrypts everything");
}

#[tokio::test]
async fn inner_reads_header_first_then_container_bounded_spans() {
    let plain_len = (CS + 500) as usize;
    let (_plain, mock, handle, wrapper) = env(plain_len, PW).await;
    let container_len = HEADER + plain_len as u64 + 2 * 16; // 2 chunks

    drain(wrapper.open_range(&handle, 10, 20).await.expect("open"))
        .await
        .expect("bytes");

    let calls = mock.open_range_calls();
    assert_eq!(calls.len(), 2, "one header pull + one span pull");
    assert_eq!(
        calls[0],
        (0, HEADER),
        "the FIRST inner read is the 34-byte header"
    );
    let (span_off, span_len) = calls[1];
    assert!(
        span_off >= HEADER && span_off + span_len <= container_len,
        "span [{span_off}, +{span_len}) stays within the container ({container_len})"
    );

    // The window state is cached: a second read adds exactly one inner
    // call (no repeated header pull, no repeated key derivation).
    drain(wrapper.open_range(&handle, 0, 5).await.expect("open"))
        .await
        .expect("bytes");
    assert_eq!(mock.open_range_calls().len(), 3, "no second header pull");
}

#[tokio::test]
async fn eof_clamps_and_beyond_total_is_empty() {
    let plain_len = (CS + 1000) as usize;
    let (plain, _mock, handle, wrapper) = env(plain_len, PW).await;

    // Overrun clamps to the plaintext total (K35 authority).
    let tail = drain(
        wrapper
            .open_range(&handle, plain_len as u64 - 10, 100)
            .await
            .expect("open"),
    )
    .await
    .expect("bytes");
    assert_eq!(
        tail,
        plain[plain_len - 10..],
        "EOF clamp in plain coordinates"
    );

    // At/past the total: an empty stream, zero inner traffic beyond the
    // (already cached) header.
    let (_plain2, mock2, handle2, wrapper2) = env(64, PW).await;
    let empty = drain(wrapper2.open_range(&handle2, 64, 10).await.expect("open"))
        .await
        .expect("bytes");
    assert!(empty.is_empty(), "past-EOF window is empty");
    let beyond = drain(
        wrapper2
            .open_range(&handle2, u64::MAX, 10)
            .await
            .expect("open"),
    )
    .await
    .expect("bytes");
    assert!(beyond.is_empty(), "u64::MAX offset is empty");
    assert!(
        mock2.open_range_calls().len() <= 1,
        "no span pull for empty windows"
    );
}

#[tokio::test]
async fn wrong_password_fails_closed_with_actionable_error() {
    let (_plain, _mock, handle, wrapper) = env((CS + 7) as usize, "wrong password").await;
    // The wrapper decrypts the whole window inside `open_range` (the
    // returned stream is a prepared plaintext frame), so the
    // authentication failure surfaces on the call itself.
    let err = wrapper
        .open_range(&handle, 0, 8)
        .await
        .err()
        .expect("reads under a wrong password must fail");
    assert!(
        matches!(err, StorageError::Unavailable(ref msg) if msg.contains("password")),
        "{err}"
    );
}

// --------------------------------------------------- delegation face ------

#[tokio::test]
async fn upload_delete_and_capabilities_delegate_to_inner() {
    let (plain, mock, handle, wrapper) = env(64, PW).await;

    // Upload face: the job reaches the inner mock unchanged.
    let dir = tempfile::tempdir().expect("scratch dir");
    let local = dir.path().join("job.bin");
    std::fs::write(&local, &plain).expect("scratch file");
    let rel = RelPath::new("/delegated.bin").expect("valid rel path");
    wrapper
        .upload(&UploadJob {
            rel_path: rel.clone(),
            local_path: local.clone(),
            size: plain.len() as u64,
            chunk_count: 1,
            chunk_size: plain.len() as u64,
        })
        .await
        .expect("upload delegates");
    assert_eq!(
        mock.upload_calls().len(),
        2,
        "upload reached the inner mock (1 container seed + 1 delegated job)"
    );

    // Capability declaration passes through (R4 honesty: the wrapper adds
    // no capability of its own).
    assert_eq!(wrapper.capabilities(), mock.capabilities());

    // Delete delegates to the inner mock's delete_remote.
    wrapper
        .delete_remote(&handle)
        .await
        .expect("delete delegates");
    assert_eq!(mock.deleted(), handle.chunk_msg_ids, "inner delete ran");
}

// -------------------------------------------------- threading contract ----

#[test]
fn wrapper_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DecryptingTransport>();
}
