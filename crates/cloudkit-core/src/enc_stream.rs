//! Decrypting read-arm wrapper (K47, Phase 3.5-a): presents one encrypted
//! remote object to the streaming faces as if it were plaintext.
//!
//! Position (PCFS CryptoWrapper analogue): the wrapper implements
//! [`CloudTransport`] over an inner transport + one bound
//! [`RemoteHandle`]; the faces (`webdav` RangeFile, `winfsp` WindowReader)
//! keep plain-coordinate windows and never learn about encryption. Each
//! plaintext window maps to ONE inner ciphertext span read (the inner
//! drivers self-bound their windows, e.g. baidu ≤4 MiB), then
//! chunk-by-chunk STREAM-AEAD decryption with per-chunk authentication —
//! strictly stronger than PCFS's unauthenticated CTR.
//!
//! Laziness: the 34-byte header pull and the PBKDF2 derivation happen on
//! the FIRST `open_range`, not at construction (open stays free of
//! network/KDF cost; a wrong password or corrupt header surfaces as a
//! first-read error). The derived window is cached in a [`OnceLock`] —
//! the ~tens-of-ms one-shot derivation is accepted per plan §3 (K41
//! grace + LRU amortize repeated opens; same sync-CPU-in-async shape as
//! hydrate's inline decryption).

use std::sync::{Arc, OnceLock};

use futures_util::StreamExt;

use crate::transport::{
    ByteStream, Capabilities, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_crypto::v2::{AeadV2Window, HEADER_SIZE};

/// Aggregates a [`ByteStream`] into one buffer (every face aggregates
/// frames; the wrapper decrypts whole windows).
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
    window: OnceLock<AeadV2Window>,
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

    /// Pulls the container header once, parses it and derives the key
    /// once (PBKDF2). Structural failures (bad magic, guardrail violations,
    /// short header) are actionable [`StorageError::Unavailable`]s.
    async fn window(&self) -> Result<&AeadV2Window, StorageError> {
        if let Some(window) = self.window.get() {
            return Ok(window);
        }
        let header = aggregate(
            self.inner
                .open_range(&self.handle, 0, HEADER_SIZE as u64)
                .await?,
        )
        .await?;
        let window = AeadV2Window::open(&self.password, &header).map_err(|error| {
            StorageError::Unavailable(format!(
                "encrypted object header unreadable ({} bytes pulled): {error} — \
                 the remote artifact is not a valid aead_v2 container",
                header.len()
            ))
        })?;
        // Concurrent first readers may race here; the loser's window is
        // dropped — correct, just an extra derivation.
        Ok(self.window.get_or_init(|| window))
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
    /// plaintext total: one inner ciphertext-span read, per-chunk
    /// authenticated decryption, window slice.
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

        let chunk_size = window.chunk_size() as u64;
        let first = off / chunk_size;
        let last = (end - 1) / chunk_size;
        let n_chunks = window.n_chunks_for_plain(total);
        let (span_off, span_len) = window.ciphertext_span(first, last, total);

        // One inner read for the whole span; the inner drivers self-bound
        // their window sizes (e.g. baidu's ≤4 MiB bounded downloads).
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

        let mut plain = Vec::with_capacity((end - off) as usize);
        for index in first..=last {
            let (chunk_off, chunk_len) = window.ciphertext_span(index, index, total);
            let base = (chunk_off - span_off) as usize;
            plain.extend(
                window
                    .decrypt_chunk(
                        index,
                        index + 1 == n_chunks,
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
        let skip = (off - first * chunk_size) as usize;
        let take = (end - off) as usize;
        Ok(single_frame(plain[skip..skip + take].to_vec()))
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
