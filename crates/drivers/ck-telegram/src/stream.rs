//! Range-serving stream adapter over grammers `DownloadIter` chunk
//! iterators (the glue between [`crate::range::range_plan`] and `ByteStream`).

use bytes::Bytes;
use cloudkit_storage::transport::{ByteStream, StorageError};
use futures_core::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Wraps a (whole-chunk-skipped) download iterator into a ByteStream serving
/// exactly `bytes_to_yield` bytes after discarding `skip_bytes_in_first` from
/// the first chunk. Chunk boundaries are NOT preserved: output Bytes frames
/// are whatever remains of each input chunk after skip/take bookkeeping
/// (frames are non-empty). Yields at most bytes_to_yield bytes; stops early
/// (no error) when the iterator ends first. StorageError is never produced
/// by this adapter itself.
pub fn serve_range<I>(iter: I, skip_bytes_in_first: u64, bytes_to_yield: u64) -> ByteStream
where
    I: Iterator<Item = Vec<u8>> + Send + Sync + 'static,
{
    Box::pin(RangeStream {
        // Boxed so the state machine is Unpin regardless of the backing
        // iterator: poll_next then never needs unsafe pin projection.
        iter: Box::new(iter),
        skip: skip_bytes_in_first,
        take: bytes_to_yield,
    })
}

/// In-memory skip/take state machine over pre-buffered download chunks.
struct RangeStream {
    iter: Box<dyn Iterator<Item = Vec<u8>> + Send + Sync>,
    /// Bytes still to discard (only at the stream head; may span chunks).
    skip: u64,
    /// Bytes still to yield; reaching 0 ends the stream without draining
    /// the remaining iterator items.
    take: u64,
}

impl RangeStream {
    /// Pulls the next non-empty frame, or None when the take budget is
    /// spent or the iterator is exhausted. Zero-copy: frames are `Bytes`
    /// slices of the pulled chunk.
    fn next_frame(&mut self) -> Option<Bytes> {
        if self.take == 0 {
            return None;
        }
        for chunk in self.iter.by_ref() {
            let mut frame = Bytes::from(chunk);
            if self.skip > 0 {
                let skip = self.skip.min(frame.len() as u64) as usize;
                self.skip -= skip as u64;
                frame = frame.slice(skip..);
                if frame.is_empty() {
                    // The skip window swallowed this whole chunk.
                    continue;
                }
            }
            let keep = self.take.min(frame.len() as u64) as usize;
            self.take -= keep as u64;
            return Some(frame.slice(..keep));
        }
        // Iterator ended before the take budget was spent: short read,
        // never an error.
        None
    }
}

impl Stream for RangeStream {
    type Item = Result<Bytes, StorageError>;

    /// The backing iterator is fully buffered in memory, so every poll
    /// resolves immediately; `Pending` is never returned.
    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.next_frame().map(Ok))
    }
}

/// Adapts a [`ByteStream`] (futures `Stream` of `Bytes` frames) into a
/// `tokio::io::AsyncRead` — the v2 streaming-upload bridge (Batch E /
/// E-3): the queue hands the ciphertext over as a frames stream, while
/// grammers' `Client::upload_stream` consumes an `AsyncRead`. Buffers at
/// most one frame at a time, so the memory profile of a streaming v2
/// upload stays at one crypto chunk per hop regardless of file size.
pub struct StreamReader {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send + Sync>>,
    pending: Bytes,
    done: bool,
}

impl StreamReader {
    /// Wraps `stream`; read it to EOF (a `StorageError` frame surfaces as
    /// an `io::Error` on the reading side).
    pub fn new(stream: ByteStream) -> Self {
        Self {
            stream,
            pending: Bytes::new(),
            done: false,
        }
    }
}

impl tokio::io::AsyncRead for StreamReader {
    /// Serves the pending frame slice first, then polls the backing
    /// stream for the next frame. EOF only after the stream ends AND the
    /// pending frame is drained. The struct is `Unpin` by construction
    /// (boxed stream + `Bytes`), so plain field access is safe.
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.pending.is_empty() {
                let n = self.pending.len().min(buf.remaining());
                // `split_to` hands out the head slice and keeps the rest —
                // zero-copy on the Bytes refcount either way.
                let head = self.pending.split_to(n);
                buf.put_slice(&head);
                return Poll::Ready(Ok(()));
            }
            if self.done {
                return Poll::Ready(Ok(()));
            }
            match self.stream.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(frame))) => self.pending = frame,
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(std::io::Error::other(error.to_string())));
                }
                Poll::Ready(None) => self.done = true,
            }
        }
    }
}
