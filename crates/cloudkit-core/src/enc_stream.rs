//! Decrypting read-arm wrapper (K47, Phase 3.5-a): presents one encrypted
//! remote object to the streaming faces as if it were plaintext.
//!
//! Position (PCFS CryptoWrapper analogue): the wrapper implements
//! [`CloudTransport`] over an inner transport + one bound
//! [`RemoteHandle`]; the faces (`webdav` RangeFile, `winfsp` WindowReader)
//! keep plain-coordinate windows and never learn about encryption. Each
//! plaintext window maps to inner ciphertext span reads of at most
//! [`STREAM_WINDOW`] plaintext bytes (the inner drivers self-bound their
//! own fetches further, e.g. baidu ≤4 MiB), then chunk-by-chunk
//! STREAM-AEAD decryption with per-chunk authentication — strictly
//! stronger than PCFS's unauthenticated CTR.
//!
//! Streaming (K47 semantics, fixed 2026-09-10): a long `open_range` is
//! served as an async stream that steps the request in [`STREAM_WINDOW`]
//! plaintext windows — one bounded inner span pull + decrypt + one frame
//! per step, frames produced as the steps complete. A consumer asking
//! for the whole file sees its first bytes after the FIRST bounded span,
//! never after the whole object (the pre-fix wrapper aggregated the full
//! request span inside the call, stalling open-ended playback requests
//! behind the full-ciphertext download).
//!
//! Laziness: the 34-byte header pull and the PBKDF2 derivation happen on
//! the FIRST `open_range`, not at construction (open stays free of
//! network/KDF cost; a corrupt header surfaces as a call error, a wrong
//! password as a first-frame error — every face polls frames
//! immediately, so callers see the actionable error before any body
//! byte). The derived window is cached in a [`OnceLock`] — the
//! ~tens-of-ms one-shot derivation is accepted per plan §3 (K41 grace +
//! LRU amortize repeated opens; same sync-CPU-in-async shape as
//! hydrate's inline decryption).

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use futures_util::StreamExt;

use crate::transport::{
    ByteStream, Capabilities, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_crypto::v2::{AeadV2Window, HEADER_SIZE};

/// Plaintext step of a streamed multi-window read: at most this many
/// plaintext bytes per inner span pull (one frame per step), so a
/// long-range consumer gets its first frame after the first bounded
/// span instead of the whole object. 4 MiB mirrors the faces' window
/// sizes and stays within the baidu bounded-slice ceiling once the
/// per-chunk tags spill past it.
const STREAM_WINDOW: u64 = 4 * 1024 * 1024;

/// Aggregates a [`ByteStream`] into one buffer (the bounded spans the
/// wrapper pulls; never the whole object).
async fn aggregate(mut stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

/// The decrypted window as a single-frame [`ByteStream`] (faces aggregate
/// frames; a bounded plaintext window is at most one face-requested
/// slice).
fn single_frame(bytes: Vec<u8>) -> ByteStream {
    Box::pin(futures_util::stream::once(async move {
        Ok(bytes::Bytes::from(bytes))
    }))
}

/// A [`CloudTransport`] view of ONE encrypted remote object: translates
/// plaintext window requests into ciphertext spans, decrypting
/// chunk-by-chunk (per-chunk AEAD authentication) before the bytes leave
/// the transport seam. Read-arm only — upload/delete delegate to the
/// inner transport.
pub struct DecryptingTransport {
    inner: Arc<dyn CloudTransport>,
    handle: RemoteHandle,
    /// Plaintext length of the object (K35 authority). Encrypted handles
    /// carry the `u64::MAX` ciphertext sentinel — the wrapper never reads
    /// `handle.total_size`.
    plain_len: u64,
    password: String,
    /// Parse + key derivation result, filled on the first read. A failed
    /// initialization is NOT cached: transient inner-transport errors
    /// retry, and each attempt re-pulls only the 34-byte header.
    window: OnceLock<Arc<AeadV2Window>>,
}

impl DecryptingTransport {
    /// Wraps `inner` for the object `handle` whose PLAINTEXT length is
    /// `plain_len` (K35 authority), decrypting with `password`.
    pub fn new(
        inner: Arc<dyn CloudTransport>,
        handle: RemoteHandle,
        plain_len: u64,
        password: String,
    ) -> Self {
        Self {
            inner,
            handle,
            plain_len,
            password,
            window: OnceLock::new(),
        }
    }

    /// [`Self::new`] with the container window **already derived** by the
    /// caller's first-read validation（Phase 8-B EB2 / B2）：`Vfs::open_read`
    /// pulls the 34-byte header ahead of any response byte, validates the
    /// magic, repairs the row and hands the parsed window over — so the
    /// header read + PBKDF2 derivation that this wrapper would otherwise
    /// run on its first `open_range` happen **exactly once** (B4:
    /// validation attaches to the same read, zero extra round trips; the
    /// `OnceLock` falls straight through to the seeded window).
    pub fn new_with_window(
        inner: Arc<dyn CloudTransport>,
        handle: RemoteHandle,
        plain_len: u64,
        password: String,
        window: Arc<AeadV2Window>,
    ) -> Self {
        let lock = OnceLock::new();
        let _seeded = lock.set(window);
        Self {
            inner,
            handle,
            plain_len,
            password,
            window: lock,
        }
    }

    /// Pulls the container header once, parses it and derives the key
    /// once (PBKDF2). Structural failures (bad magic, guardrail violations,
    /// short header) are actionable [`StorageError::Unavailable`]s.
    async fn window(&self) -> Result<Arc<AeadV2Window>, StorageError> {
        if let Some(window) = self.window.get() {
            return Ok(Arc::clone(window));
        }
        let header = aggregate(
            self.inner
                .open_range(&self.handle, 0, HEADER_SIZE as u64)
                .await?,
        )
        .await?;
        let window = Arc::new(
            AeadV2Window::open(&self.password, &header).map_err(|error| {
                StorageError::Unavailable(format!(
                    "encrypted object header unreadable ({} bytes pulled): {error} — \
                     the remote artifact is not a valid aead_v2 container",
                    header.len()
                ))
            })?,
        );
        // Concurrent first readers may race here; the loser's window is
        // dropped — correct, just an extra derivation.
        Ok(Arc::clone(self.window.get_or_init(|| window)))
    }
}

/// The per-step state of a streamed multi-window read (the async-stream
/// half of [`DecryptingTransport::open_range`]): walks `[off, end)` in
/// [`STREAM_WINDOW`] plaintext windows, one bounded span pull + decrypt +
/// frame per step. No task is spawned — the stream owns this state, so a
/// consumer that drops mid-stream cancels the fetch at the next step
/// boundary.
struct WindowSteps {
    inner: Arc<dyn CloudTransport>,
    handle: RemoteHandle,
    window: Arc<AeadV2Window>,
    /// Plaintext total (K35 authority) — the chunk-layout + EOF bound.
    total: u64,
    /// Number of chunks the container spans (final-chunk flag math).
    n_chunks: u64,
    /// Next window start (plaintext coordinates); `>= end` = finished.
    pos: u64,
    /// Request end, already clamped to the plaintext total.
    end: u64,
}

impl WindowSteps {
    /// Pulls + decrypts the next ≤[`STREAM_WINDOW`] plaintext step as one
    /// frame. On failure the walk is parked at `end` (terminal) so a
    /// consumer that keeps polling after an error frame gets no silent
    /// retry of the same span.
    async fn next_window(&mut self) -> Result<bytes::Bytes, StorageError> {
        let chunk_size = self.window.chunk_size() as u64;
        let step_end = self.pos.saturating_add(STREAM_WINDOW).min(self.end);
        let first = self.pos / chunk_size;
        let last = (step_end - 1) / chunk_size;
        let (span_off, span_len) = self.window.ciphertext_span(first, last, self.total);

        // One inner read for this step's span; the inner drivers
        // self-bound their window sizes (e.g. baidu's ≤4 MiB bounded
        // downloads).
        let ciphertext = aggregate(
            self.inner
                .open_range(&self.handle, span_off, span_len)
                .await?,
        )
        .await?;
        if ciphertext.len() as u64 != span_len {
            return Err(StorageError::Unavailable(format!(
                "encrypted span short: inner transport returned {} of {span_len} bytes \
                 (container truncated?)",
                ciphertext.len()
            )));
        }

        let mut plain = Vec::with_capacity((step_end - self.pos) as usize);
        for index in first..=last {
            let (chunk_off, chunk_len) = self.window.ciphertext_span(index, index, self.total);
            let base = (chunk_off - span_off) as usize;
            plain.extend(
                self.window
                    .decrypt_chunk(
                        index,
                        index + 1 == self.n_chunks,
                        &ciphertext[base..base + chunk_len as usize],
                    )
                    .map_err(|error| {
                        StorageError::Unavailable(format!(
                            "chunk {index} of the encrypted object failed authentication: {error} \
                     (wrong encryption password or corrupted ciphertext)"
                        ))
                    })?,
            );
        }
        let skip = (self.pos - first * chunk_size) as usize;
        let take = (step_end - self.pos) as usize;
        self.pos = step_end;
        // `end` may land mid-chunk: the decrypted chunk tail beyond the
        // step (and the request) is dropped here.
        let mut frame = plain.split_off(skip);
        frame.truncate(take);
        Ok(bytes::Bytes::from(frame))
    }
}

/// The in-flight step future of a [`WindowStream`]: pull + decrypt of
/// one window, returning the (re-owned) walk state alongside the frame.
/// Async-trait futures are `Send` but not `Sync`, and [`ByteStream`]
/// requires `Sync` — the surrounding mutex is structural (only the
/// owning stream locks it, never across an await).
type StepFuture = Pin<
    Box<dyn std::future::Future<Output = (WindowSteps, Result<bytes::Bytes, StorageError>)> + Send>,
>;

/// The [`futures_core::Stream`] over a [`WindowSteps`] walk (a
/// `stream::unfold` cannot express this: the async-trait `open_range`
/// futures are `Send` but not `Sync`, and [`ByteStream`] requires
/// `Sync`). One poll drives at most one window pull; the state is owned
/// by the stream itself, so a consumer that drops mid-walk cancels the
/// fetch at the current step — nothing is spawned, nothing leaks.
struct WindowStream {
    /// The window walk; `None` only while a step future owns it.
    steps: Option<WindowSteps>,
    /// The in-flight step (span pull + decrypt), created on demand.
    inflight: std::sync::Mutex<Option<StepFuture>>,
}

impl futures_core::Stream for WindowStream {
    type Item = Result<bytes::Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut inflight = this.inflight.lock().expect("window stream lock");
        if inflight.is_none() {
            let Some(mut steps) = this.steps.take() else {
                return Poll::Ready(None);
            };
            if steps.pos >= steps.end {
                this.steps = Some(steps);
                return Poll::Ready(None);
            }
            *inflight = Some(Box::pin(async move {
                let result = steps.next_window().await;
                (steps, result)
            }));
        }
        let Some(future) = inflight.as_mut() else {
            unreachable!("the arm above always installs the step future");
        };
        match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready((steps, result)) => {
                *inflight = None;
                match result {
                    Ok(frame) => {
                        this.steps = Some(steps);
                        Poll::Ready(Some(Ok(frame)))
                    }
                    Err(error) => {
                        // Park at the end: one error frame per failed
                        // read, no silent same-span retries.
                        let mut steps = steps;
                        steps.pos = steps.end;
                        this.steps = Some(steps);
                        Poll::Ready(Some(Err(error)))
                    }
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl CloudTransport for DecryptingTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        self.inner.connect().await
    }

    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        self.inner.upload(job).await
    }

    /// Full-file read = the whole plaintext window `open_range(0,
    /// plain_len)`. The wrapper is bound to one object; callers must pass
    /// the handle it was built with (the faces carry exactly that one).
    async fn open(&self, _file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        self.open_range(&self.handle, 0, self.plain_len).await
    }

    /// Serves the plaintext window `[off, off + len)` clamped to the
    /// plaintext total as a STREAMING body: one frame per ≤[`STREAM_WINDOW`]
    /// plaintext step, each step one bounded inner ciphertext-span read +
    /// per-chunk authenticated decryption + window slice. The header pull
    /// and key derivation happen here (call-point errors: unreadable
    /// header); the span pulls and decryptions happen on the frames (a
    /// wrong password or corrupted chunk surfaces on the frame that hits
    /// it — every face polls frames immediately).
    async fn open_range(
        &self,
        _file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        let total = self.plain_len;
        let end = total.min(off.saturating_add(len));
        if off >= end {
            // Empty window: no header pull, no key derivation, no span.
            return Ok(single_frame(Vec::new()));
        }
        let window = self.window().await?;
        let n_chunks = window.n_chunks_for_plain(total);
        Ok(Box::pin(WindowStream {
            steps: Some(WindowSteps {
                inner: Arc::clone(&self.inner),
                handle: self.handle.clone(),
                window,
                total,
                n_chunks,
                pos: off,
                end,
            }),
            inflight: std::sync::Mutex::new(None),
        }))
    }

    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
        self.inner.delete_remote(handle).await
    }

    /// Pass-through (R4 honesty): the wrapper adds no capability of its
    /// own — it can only serve ranges where the inner transport can.
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}
