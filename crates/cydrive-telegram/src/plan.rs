//! Upload planning: turning an [`UploadJob`] into the exact sequence of
//! remote `send_file` calls (document names, captions, byte counts) the
//! Python baseline's `upload_file` performs.

use cydrive_core::transport::UploadJob;

/// One send_file call planned from an upload job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSend {
    pub document_name: String, // remote document filename
    pub caption: String,       // exact python-format caption
    pub byte_len: u64,         // bytes this chunk will carry
}

/// Plans the remote sends for `job` the way Python upload_file does:
/// - 0-byte job -> empty vec (upload skipped upstream, python never sends)
/// - single chunk (job.chunk_count == 1) -> one ChunkSend: name = rel basename,
///   caption = caption::single_file_caption(clean_rel, job.size, is_encrypted)
/// - multi chunk -> one per part, 0-based: name = caption::part_document_name(basename, i),
///   caption = caption::multi_part_caption(clean_rel, i, chunk_count, part_len),
///   part_len = chunk_size except last = job.size - chunk_size*(n-1)
#[allow(unused_variables)]
pub fn plan_chunk_sends(job: &UploadJob, is_encrypted: bool) -> Vec<ChunkSend> {
    todo!()
}
