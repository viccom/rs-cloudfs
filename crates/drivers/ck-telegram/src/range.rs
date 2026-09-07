//! Range-request planning over grammers `DownloadIter` chunk streams.

/// Telegram download chunk bounds (grammers DownloadIter contract).
pub const MIN_CHUNK_SIZE: i32 = 4096;
pub const MAX_CHUNK_SIZE: i32 = 512 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("chunk size {0} must be a multiple of {MIN_CHUNK_SIZE} within {MIN_CHUNK_SIZE}..={MAX_CHUNK_SIZE}")]
pub struct InvalidChunkSize(i32);

/// How to serve `[offset, offset+length)` from a chunked download stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangePlan {
    pub skip_chunks: i32, // whole chunks to skip before the first needed one
    pub skip_bytes_in_first_chunk: u64, // bytes to discard inside the first yielded chunk
    pub bytes_to_yield: u64, // total bytes to produce
}

/// Plans how to serve `[offset, offset+length)`: `chunk_size` must be a
/// multiple of `MIN_CHUNK_SIZE` within `MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE`
/// (else `Err`); then `skip_chunks = offset / chunk_size`,
/// `skip_bytes_in_first_chunk = offset % chunk_size`, `bytes_to_yield = length`.
pub fn range_plan(
    offset: u64,
    length: u64,
    chunk_size: i32,
) -> Result<RangePlan, InvalidChunkSize> {
    if chunk_size % MIN_CHUNK_SIZE != 0 || !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size)
    {
        return Err(InvalidChunkSize(chunk_size));
    }
    let chunk_size = chunk_size as u64;
    Ok(RangePlan {
        skip_chunks: (offset / chunk_size) as i32,
        skip_bytes_in_first_chunk: offset % chunk_size,
        bytes_to_yield: length,
    })
}
