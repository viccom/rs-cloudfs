//! Range-serving stream adapter over grammers `DownloadIter` chunk
//! iterators (the glue between [`crate::range::range_plan`] and `ByteStream`).

use bytes::Bytes;
use cloudkit_core::transport::{ByteStream, TransportError};
use futures_core::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Wraps a (whole-chunk-skipped) download iterator into a ByteStream serving
/// exactly `bytes_to_yield` bytes after discarding `skip_bytes_in_first` from
/// the first chunk. Chunk boundaries are NOT preserved: output Bytes frames
/// are whatever remains of each input chunk after skip/take bookkeeping
/// (frames are non-empty). Yields at most bytes_to_yield bytes; stops early
/// (no error) when the iterator ends first. TransportError is never produced
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
    type Item = Result<Bytes, TransportError>;

    /// The backing iterator is fully buffered in memory, so every poll
    /// resolves immediately; `Pending` is never returned.
    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.next_frame().map(Ok))
    }
}
