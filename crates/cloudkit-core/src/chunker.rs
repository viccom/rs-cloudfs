//! Large-file splitting/merging aligned with the Python `chunker.py`.
//!
//! Part naming is the compatibility contract: `{base}.part{idx:03}` with a
//! zero-padded 3-digit index starting at 0. The chunking threshold is
//! `size > chunk_mb * 1024 * 1024` (strictly greater).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Reads once into `buf`, retrying on `ErrorKind::Interrupted`; returns the
/// number of bytes read (`0` signals end of input).
fn read_retry(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Lowercase hex encoding, matching `hashlib.hexdigest()` output.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        out.push(HEX_DIGITS[usize::from(byte & 0x0F)] as char);
    }
    out
}

/// Read/write buffer size used when streaming through files (10 MB).
pub const CHUNK_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// Metadata describing one written chunk part.
#[derive(Debug, Clone)]
pub struct PartInfo {
    /// Zero-based part index (`part000`, `part001`, ...).
    pub index: usize,
    /// Path of the written part file.
    pub path: PathBuf,
    /// Size of the part in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the part bytes.
    pub sha256: String,
}

/// Whether `size` exceeds the single-upload limit implied by `chunk_mb`
/// (`size > chunk_mb * 1024 * 1024`).
pub fn needs_chunking(size: u64, chunk_mb: u64) -> bool {
    size > chunk_mb.saturating_mul(1024 * 1024)
}

/// Part file name for `base_name` and `chunk_index`:
/// `{base}.part{idx:03}` (min-width padding, so index 1000 keeps growing).
///
/// Lives in L2 since Batch R (`cloudkit_storage::transport::part_name` —
/// the chunk-naming compat contract is shared by the mock and the telegram
/// driver); re-exported here so `cloudkit_core::chunker::part_name` keeps
/// working.
pub use cloudkit_storage::transport::part_name;

/// Splits `input` into `ceil(size / chunk_mb MiB)` parts inside `out_dir`
/// (at least one part for a non-empty file); each `sha256` is lowercase hex.
///
/// Mirrors the Python `FileChunker.split_file`: parts are streamed through a
/// [`CHUNK_BUFFER_SIZE`] buffer, an empty input yields zero parts (the empty
/// candidate part file is removed again).
pub fn split_file(input: &Path, out_dir: &Path, chunk_mb: u64) -> std::io::Result<Vec<PartInfo>> {
    fs::create_dir_all(out_dir)?;
    let base_name = input.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "input file name must be valid UTF-8",
        )
    })?;
    let chunk_bytes = chunk_mb.saturating_mul(1024 * 1024) as usize;

    let mut input = File::open(input)?;
    let mut buf = vec![0u8; CHUNK_BUFFER_SIZE];
    let mut parts = Vec::new();
    let mut index = 0usize;

    loop {
        let path = out_dir.join(part_name(base_name, index));
        let mut part = File::create(&path)?;
        let mut hasher = Sha256::new();
        let mut written = 0usize;
        while written < chunk_bytes {
            let want = (chunk_bytes - written).min(CHUNK_BUFFER_SIZE);
            let n = read_retry(&mut input, &mut buf[..want])?;
            if n == 0 {
                break;
            }
            part.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            written += n;
        }
        drop(part);
        if written == 0 {
            // Exhausted input before this part began: no empty trailing part.
            fs::remove_file(&path)?;
            break;
        }
        parts.push(PartInfo {
            index,
            path,
            size: written as u64,
            sha256: hex_lower(&hasher.finalize()),
        });
        index += 1;
    }
    Ok(parts)
}

/// Concatenates `parts` in order into `output`.
pub fn merge_chunks(parts: &[PathBuf], output: &Path) -> std::io::Result<()> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut out = File::create(output)?;
    let mut buf = vec![0u8; CHUNK_BUFFER_SIZE];
    for part_path in parts {
        let mut part = File::open(part_path)?;
        loop {
            let n = read_retry(&mut part, &mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
    }
    out.flush()
}

/// Lowercase hex SHA-256 of a file, streamed with [`CHUNK_BUFFER_SIZE`].
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_BUFFER_SIZE];
    loop {
        let n = read_retry(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}
