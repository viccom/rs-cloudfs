//! Local test-file generation (OS temp dir) and 4 MiB block hashing.
//!
//! Content strategy: first block pseudo-random (LCG seeded from the clock)
//! so each file's content hash is unique (avoids accidental rapid-upload
//! polluting throughput/resume measurements); remaining blocks are a cheap
//! rotating pattern.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use md5::{Digest, Md5};

/// Baidu PCS upload block size (official SDK + PCFS: 4 MiB).
pub const BLOCK: u64 = 4 * 1024 * 1024;

pub struct Lcg(pub u64);

impl Lcg {
    pub fn new_seeded() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Lcg(nanos ^ 0xA0761D6478BD642F)
    }
    pub fn fill(&mut self, buf: &mut [u8]) {
        let mut x = self.0;
        for b in buf.iter_mut() {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 33) as u8;
        }
        self.0 = x;
    }
}

/// Generate `size` bytes at `path`: random first block, pattern elsewhere.
pub fn gen_file(path: &Path, size: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f =
        std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut rng = Lcg::new_seeded();
    let chunk: u64 = 1024 * 1024;
    let mut buf = vec![0u8; chunk as usize];
    let mut written: u64 = 0;
    let mut pattern: u8 = 0;
    while written < size {
        let n = chunk.min(size - written) as usize;
        if written < BLOCK {
            rng.fill(&mut buf[..n]);
            // keep the LCG state advancing consistently regardless of n
        } else {
            for b in buf[..n].iter_mut() {
                *b = pattern;
                pattern = pattern.wrapping_add(7);
            }
        }
        f.write_all(&buf[..n])?;
        written += n as u64;
    }
    Ok(())
}

/// One sequential pass computing the 4 MiB block MD5 list.
pub fn block_md5s(path: &Path, size: u64) -> Result<Vec<String>> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; BLOCK as usize];
    let mut remaining = size;
    while remaining > 0 {
        let n = BLOCK.min(remaining) as usize;
        f.read_exact(&mut buf[..n])
            .with_context(|| format!("read block at {}", size - remaining))?;
        let mut h = Md5::new();
        h.update(&buf[..n]);
        out.push(hex::encode(h.finalize()));
        remaining -= n as u64;
    }
    Ok(out)
}

/// Read an exact byte range of a local file (verification helper).
pub fn read_range(path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    std::io::Read::read_exact(&mut f, &mut buf)?;
    Ok(buf)
}
