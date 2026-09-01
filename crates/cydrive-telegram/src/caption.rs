//! Caption text and remote document naming, byte-exact against the Python
//! baseline (`telegram_client.py`). Captions are the only remote-side link
//! between uploaded parts and their original virtual path.

/// Normalizes a virtual path exactly like Python telegram_client.py:154:
/// "/" + rel.strip("/").replace("\\", "/")  (empty segments collapse via strip only at ends)
// TODO(M2 green): drop the allows once the parameters are read by the real bodies.
#[allow(unused_variables)]
pub fn clean_rel_path(rel: &str) -> String {
    todo!()
}

/// Single-file caption, Python :249-254. Exact format (␊ = "\n"):
/// "🚀 **CyDrive Cloud Backup**␊📁 Path: `{clean_rel}`␊📦 Size: `{size_bytes / 1024} KB`"
/// + " (🔒 AES Encrypted)" when is_encrypted. KB is integer division.
#[allow(unused_variables)]
pub fn single_file_caption(clean_rel: &str, size_bytes: u64, is_encrypted: bool) -> String {
    todo!()
}

/// Multi-part caption, Python :205-209 (part_index is 0-based, printed 1-based):
/// "🚀 **CyDrive Multi-Part Cloud Archive**␊📁 File: `{clean_rel}`␊🧩 Part: `{part_index+1}/{part_count}` ({part_size_bytes / 1024} KB)"
#[allow(unused_variables)]
pub fn multi_part_caption(
    clean_rel: &str,
    part_index: usize,
    part_count: usize,
    part_size_bytes: u64,
) -> String {
    todo!()
}

/// Remote document name of one part: `{file_name}.part{part_index:03}` (0-based, Python :204).
#[allow(unused_variables)]
pub fn part_document_name(file_name: &str, part_index: usize) -> String {
    todo!()
}
