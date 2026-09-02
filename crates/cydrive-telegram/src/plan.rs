//! Upload planning: turning an [`UploadJob`] into the exact sequence of
//! remote `send_file` calls (document names, captions, byte counts) the
//! Python baseline's `upload_file` performs.

use crate::caption::{clean_rel_path, multi_part_caption, part_document_name, single_file_caption};
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
///
/// Pure arithmetic over the job's chunk plan: never touches `local_path` or
/// any other disk state, so it is safe to call from any thread at any time.
pub fn plan_chunk_sends(job: &UploadJob, is_encrypted: bool) -> Vec<ChunkSend> {
    // Python never sends 0-byte files (the upload is skipped upstream).
    if job.size == 0 {
        return Vec::new();
    }
    let clean_rel = clean_rel_path(job.rel_path.as_str());
    let base_name = job.rel_path.name();
    let part_count = job.chunk_count as usize;
    if job.chunk_count == 1 {
        return vec![ChunkSend {
            document_name: base_name.to_owned(),
            caption: single_file_caption(&clean_rel, job.size, is_encrypted),
            byte_len: job.size,
        }];
    }
    (0..part_count)
        .map(|index| {
            // Bytes remaining at this part's offset, capped at one chunk:
            // exactly `chunk_size` for every part but the last, which
            // carries the remainder `size - chunk_size * (n - 1)`.
            let byte_len = job
                .size
                .saturating_sub(job.chunk_size.saturating_mul(index as u64))
                .min(job.chunk_size);
            ChunkSend {
                document_name: part_document_name(base_name, index),
                caption: multi_part_caption(&clean_rel, index, part_count, byte_len),
                byte_len,
            }
        })
        .collect()
}
