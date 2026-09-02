//! Range-serving stream adapter over grammers `DownloadIter` chunk
//! iterators (the glue between [`range::range_plan`] and `ByteStream`).

// TransportError is carried by the ByteStream item type and will be named by
// the green implementation; the stub body cannot reference it yet.
#[allow(unused_imports)]
use cydrive_core::transport::{ByteStream, TransportError};

/// Wraps a (whole-chunk-skipped) download iterator into a ByteStream serving
/// exactly `bytes_to_yield` bytes after discarding `skip_bytes_in_first` from
/// the first chunk. Chunk boundaries are NOT preserved: output Bytes frames
/// are whatever remains of each input chunk after skip/take bookkeeping
/// (frames are non-empty). Yields at most bytes_to_yield bytes; stops early
/// (no error) when the iterator ends first. TransportError is never produced
/// by this adapter itself.
#[allow(unused_variables)]
pub fn serve_range<I>(iter: I, skip_bytes_in_first: u64, bytes_to_yield: u64) -> ByteStream
where
    I: Iterator<Item = Vec<u8>> + Send + Sync + 'static,
{
    todo!()
}
