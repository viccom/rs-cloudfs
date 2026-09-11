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
use cloudkit_core::transport::{
    ByteStream, Capabilities, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
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
    // Streaming contract (2026-09-10 fix): the header parses under any
    // password, so the call itself succeeds; the per-chunk
    // authentication failure surfaces on the FIRST frame poll (every
    // face polls frames immediately, so callers still see the
    // actionable error before any body byte).
    let stream = wrapper
        .open_range(&handle, 0, 8)
        .await
        .expect("open is lazy; the failure surfaces on the first frame");
    let err = drain(stream)
        .await
        .expect_err("reads under a wrong password must fail");
    assert!(
        matches!(err, StorageError::Unavailable(ref msg) if msg.contains("password")),
        "{err}"
    );
}

// --------------------------------------------- streaming windows (fix) ----

/// A byte-gated inner transport for the streaming tests: serves every
/// `open_range` span as ≤64 KiB frames, but a frame covering absolute
/// container bytes `..end` leaves only once the test has allowed at least
/// `end` — progress is observable and holdable at any span boundary.
struct GatedTransport {
    container: bytes::Bytes,
    gate: Arc<Gate>,
    calls: std::sync::Mutex<Vec<(u64, u64)>>,
}

/// The shared release valve: an absolute container-offset budget plus a
/// waker for the frames waiting on it.
struct Gate {
    allowed: std::sync::Mutex<u64>,
    notify: tokio::sync::Notify,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            allowed: std::sync::Mutex::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    /// Raises the byte budget to at least `budget` (absolute container
    /// bytes) and wakes every waiting frame.
    fn allow(&self, budget: u64) {
        let mut allowed = self.allowed.lock().expect("gate lock");
        if budget > *allowed {
            *allowed = budget;
        }
        self.notify.notify_waiters();
    }

    /// Waits until frames ending at absolute offset `needed` may flow.
    async fn wait_until(&self, needed: u64) {
        loop {
            // Register the waiter BEFORE re-checking, so an `allow`
            // racing between the two lines still wakes us.
            let notified = self.notify.notified();
            if *self.allowed.lock().expect("gate lock") >= needed {
                return;
            }
            notified.await;
        }
    }
}

impl GatedTransport {
    fn calls(&self) -> Vec<(u64, u64)> {
        self.calls.lock().expect("calls lock").clone()
    }
}

#[async_trait::async_trait]
impl CloudTransport for GatedTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn upload(&self, _job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        self.open_range(file, 0, self.container.len() as u64).await
    }

    async fn open_range(
        &self,
        _file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        self.calls.lock().expect("calls lock").push((off, len));
        let total = self.container.len() as u64;
        let start = off.min(total);
        let end = off.saturating_add(len).min(total);
        let data = self.container.slice(start as usize..end as usize);
        const FRAME: usize = 64 * 1024;
        let gate = Arc::clone(&self.gate);
        let stream = futures_util::stream::unfold(
            (data, start, 0usize, gate),
            |(data, start, pos, gate)| async move {
                if pos >= data.len() {
                    return None;
                }
                let hi = (pos + FRAME).min(data.len());
                gate.wait_until(start + hi as u64).await;
                Some((Ok(data.slice(pos..hi)), (data, start, hi, gate)))
            },
        );
        Ok(Box::pin(stream))
    }

    async fn delete_remote(&self, _handle: &RemoteHandle) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            ..Capabilities::none()
        }
    }
}

/// A multi-window read STREAMS: the first plaintext frame flows as soon
/// as the FIRST bounded span is served — not after the whole request
/// span. The playback shape from the field report: a mid-chunk offset
/// with an effectively unbounded length (open-ended `Range: bytes=N-`).
#[tokio::test]
async fn multi_window_reads_stream_first_frame_after_first_span() {
    // 2 × 4 MiB windows + a 123-byte non-aligned tail, read from CS/2
    // (mid-chunk offset) to EOF.
    let plain_len = 8 * 1024 * 1024 + (CS / 2 + 123) as usize;
    let plain = pattern(plain_len);
    let ct = AeadV2::with_chunk_size(CS as usize)
        .expect("chunk size is the guardrail floor")
        .encrypt(PW, &plain);

    let gate = Gate::new();
    let inner = Arc::new(GatedTransport {
        container: bytes::Bytes::from(ct),
        gate: Arc::clone(&gate),
        calls: std::sync::Mutex::new(Vec::new()),
    });
    let handle = RemoteHandle {
        first_msg_id: 1,
        chunk_msg_ids: vec![1],
        total_size: u64::MAX,
        path: None,
    };
    let wrapper = DecryptingTransport::new(
        Arc::clone(&inner) as Arc<dyn CloudTransport>,
        handle.clone(),
        plain_len as u64,
        PW.into(),
    );

    let container_len = inner.container.len() as u64;
    // Window 1 from CS/2 covers plaintext chunks 0..=64 (a 4 MiB window
    // starting mid-chunk spans 65 of the 64 KiB container chunks): one
    // bounded ciphertext span of 65 full strides, none of them the tail
    // chunk.
    let span1 = 65 * (CS + 16);

    // Only the 34-byte header may flow. `open_range` itself must come
    // back once the header (and only the header) is served — under the
    // aggregate-the-whole-span semantics this await blocks until the
    // entire span is on hand, and the timeout is exactly the red.
    gate.allow(HEADER);
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wrapper.open_range(&handle, CS / 2, 40 * 1024 * 1024),
    )
    .await
    .expect("open_range returns after the header pull, not the whole span")
    .expect("open ok");

    // Release exactly the first bounded span: the first plaintext frame
    // must flow NOW, without a single byte of the second span.
    gate.allow(HEADER + span1);
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .expect("first frame flows once the first span is served")
        .expect("the stream has a first frame")
        .expect("first frame ok");
    assert_eq!(
        first.as_ref(),
        &plain[CS as usize / 2..CS as usize / 2 + 4 * 1024 * 1024],
        "first frame = the first 4 MiB window, sliced from the mid-chunk offset"
    );
    assert_eq!(
        inner.calls(),
        vec![(0, HEADER), (HEADER, span1)],
        "exactly the header pull + the first bounded span so far"
    );

    // Release the rest: the remaining windows drain frame by frame and
    // concatenate to the exact plaintext slice (tail truncation
    // included).
    gate.allow(container_len);
    let mut frame_lens = vec![first.len()];
    let mut all = first.to_vec();
    while let Some(frame) = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
        .await
        .expect("no hang draining the remaining windows")
    {
        let frame = frame.expect("frame ok");
        frame_lens.push(frame.len());
        all.extend_from_slice(&frame);
    }
    assert_eq!(
        frame_lens,
        vec![4 * 1024 * 1024, 4 * 1024 * 1024, 123],
        "windows step 4 MiB / 4 MiB / the 123-byte tail"
    );
    assert_eq!(
        all,
        plain[CS as usize / 2..],
        "windows concatenate to the exact slice"
    );

    let calls = inner.calls();
    assert_eq!(calls.len(), 4, "header + three span pulls: {calls:?}");
    for &(off, len) in &calls[1..] {
        assert!(
            len <= 5 * 1024 * 1024,
            "span ({off}, {len}) stays bounded (~4 MiB + tag spill)"
        );
    }
}

/// A whole-file-sized window splits into bounded inner spans (never one
/// whole-container pull) with the shared [`MockTransport`] pinning the
/// exact call shape: 8 MiB exact = header + two 64-stride spans.
#[tokio::test]
async fn wide_open_range_splits_into_bounded_inner_spans() {
    let plain_len = 8 * 1024 * 1024;
    let (plain, mock, handle, wrapper) = env(plain_len, PW).await;

    let got = drain(
        wrapper
            .open_range(&handle, 0, plain_len as u64)
            .await
            .expect("open"),
    )
    .await
    .expect("bytes");
    assert_eq!(got, plain, "the whole-file window decrypts everything");

    let span = 64 * (CS + 16);
    assert_eq!(
        mock.open_range_calls(),
        vec![(0, HEADER), (HEADER, span), (HEADER + span, span)],
        "two bounded span pulls, never one whole-container read"
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
