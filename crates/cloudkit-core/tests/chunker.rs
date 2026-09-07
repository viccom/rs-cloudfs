//! RED-phase tests for `cloudkit_core::chunker`.
//!
//! Contract under test (aligned with Python `chunker.py`): part naming
//! `{base}.part{idx:03}` (zero-padded 3 digits, index from 0, wider for
//! indices of 1000 or more), threshold `size > chunk_mb * 1024 * 1024`
//! (strictly greater), merge concatenates parts in the given order, and
//! SHA-256 digests are lowercase hex.

use cloudkit_core::chunker::{
    merge_chunks, needs_chunking, part_name, sha256_file, split_file, PartInfo, CHUNK_BUFFER_SIZE,
};
use std::fs;
use std::path::{Path, PathBuf};

const MB: u64 = 1024 * 1024;

/// SHA-256 of the empty input (NIST vector).
const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// SHA-256 of the ASCII string "abc" (NIST vector).
const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// Deterministic xorshift64* pseudo-random bytes so part boundaries are not
/// coincidentally similar; keeps the test self-contained (no `rand` needed).
fn pseudo_random_bytes(n: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for b in state.to_le_bytes() {
            if out.len() == n {
                break;
            }
            out.push(b);
        }
    }
    out
}

fn write_file(path: &Path, data: &[u8]) {
    fs::write(path, data).expect("write test file");
}

fn assert_part(part: &PartInfo, dir: &Path, base: &str, index: usize, expected_bytes: &[u8]) {
    let expected_name = format!("{base}.part{index:03}");
    assert_eq!(part.index, index, "part index mismatch for {expected_name}");
    assert_eq!(
        part.path.file_name().and_then(|n| n.to_str()),
        Some(expected_name.as_str()),
        "part file name mismatch"
    );
    assert_eq!(
        part.path.parent(),
        Some(dir),
        "part must be written into out_dir"
    );
    assert_eq!(part.size, expected_bytes.len() as u64, "part size mismatch");

    // Strongest check: part bytes equal the corresponding source slice.
    let on_disk = fs::read(&part.path).expect("part file must exist");
    assert_eq!(on_disk, expected_bytes, "part bytes mismatch");

    // Cross-check the reported digest against `sha256_file` (itself pinned
    // to NIST vectors below) — two independent code paths must agree.
    let digest = sha256_file(&part.path).expect("hash part file");
    assert_eq!(
        part.sha256, digest,
        "PartInfo.sha256 must equal the file digest"
    );
    assert_eq!(
        part.sha256,
        part.sha256.to_lowercase(),
        "digest must be lowercase hex"
    );
}

// ---------------------------------------------------------- thresholds ---

#[test]
fn needs_chunking_threshold_is_strictly_greater() {
    // Constant contract (kept here so this test exercises todo! code too).
    assert_eq!(CHUNK_BUFFER_SIZE, 10 * 1024 * 1024);
    let chunk = 1900;
    assert!(
        !needs_chunking(1900 * MB, chunk),
        "exactly at limit: single file"
    );
    assert!(
        needs_chunking(1900 * MB + 1, chunk),
        "one byte over the limit"
    );
    assert!(!needs_chunking(1900 * MB - 1, chunk), "under the limit");
    assert!(!needs_chunking(0, chunk), "empty file never needs chunking");
    assert!(!needs_chunking(0, 0));
}

// ------------------------------------------------------------ part_name ---

#[test]
fn part_name_is_zero_padded_three_digits() {
    assert_eq!(part_name("video.mp4", 0), "video.mp4.part000");
    assert_eq!(part_name("archive.tar", 12), "archive.tar.part012");
    assert_eq!(part_name("blob.bin", 999), "blob.bin.part999");
}

#[test]
fn part_name_grows_past_three_digits_like_python_format() {
    // Python `f"{idx:03d}"` is a minimum width; so is `{:03}` here.
    assert_eq!(part_name("huge.iso", 1000), "huge.iso.part1000");
}

// -------------------------------------------------------------- hashing ---

#[test]
fn sha256_file_known_vectors() {
    let dir = tempfile::tempdir().unwrap();

    let empty = dir.path().join("empty.bin");
    write_file(&empty, b"");
    assert_eq!(sha256_file(&empty).unwrap(), SHA256_EMPTY);

    let abc = dir.path().join("abc.txt");
    write_file(&abc, b"abc");
    assert_eq!(sha256_file(&abc).unwrap(), SHA256_ABC);
}

// ---------------------------------------------------------------- split ---

#[test]
fn split_5mb_into_3_parts_of_2_2_1_mb() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("parts");
    let input = dir.path().join("video.bin");

    let chunk = 2 * MB as usize;
    let data = pseudo_random_bytes(5 * MB as usize);
    write_file(&input, &data);

    let parts = split_file(&input, &out_dir, 2).expect("split must succeed");
    assert_eq!(parts.len(), 3, "5MB at 2MB chunks must yield 3 parts");

    assert_part(&parts[0], &out_dir, "video.bin", 0, &data[0..chunk]);
    assert_part(&parts[1], &out_dir, "video.bin", 1, &data[chunk..2 * chunk]);
    assert_part(&parts[2], &out_dir, "video.bin", 2, &data[2 * chunk..]);
}

#[test]
fn split_small_file_yields_single_part() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("parts");
    let input = dir.path().join("tiny.txt");

    let data = pseudo_random_bytes(1024);
    write_file(&input, &data);

    let parts = split_file(&input, &out_dir, 2).expect("split must succeed");
    assert_eq!(parts.len(), 1, "files under one chunk still produce 1 part");
    assert_part(&parts[0], &out_dir, "tiny.txt", 0, &data);
}

#[test]
fn split_file_smaller_than_buffer_still_correct() {
    // 10MB random input exercises multiple CHUNK_BUFFER_SIZE passes while
    // staying fast; 2MB chunks over 10MB -> exactly 5 equal parts.
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("parts");
    let input = dir.path().join("big.bin");

    let data = pseudo_random_bytes(10 * MB as usize);
    write_file(&input, &data);

    let parts = split_file(&input, &out_dir, 2).expect("split must succeed");
    assert_eq!(parts.len(), 5);
    let chunk = 2 * MB as usize;
    for (i, part) in parts.iter().enumerate() {
        assert_part(
            part,
            &out_dir,
            "big.bin",
            i,
            &data[i * chunk..(i + 1) * chunk],
        );
    }
}

// ---------------------------------------------------------------- merge ---

#[test]
fn merge_restores_original_bytes_and_digest() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("parts");
    let input = dir.path().join("video.bin");
    let merged = dir.path().join("merged.bin");

    let data = pseudo_random_bytes(5 * MB as usize);
    write_file(&input, &data);

    let parts = split_file(&input, &out_dir, 2).expect("split must succeed");
    let part_paths: Vec<PathBuf> = parts.iter().map(|p| p.path.clone()).collect();
    merge_chunks(&part_paths, &merged).expect("merge must succeed");

    assert_eq!(fs::read(&merged).unwrap(), data, "merged bytes must match");
    assert_eq!(
        sha256_file(&merged).unwrap(),
        sha256_file(&input).unwrap(),
        "merged digest must match the original"
    );
}

#[test]
fn merge_concatenates_in_given_order() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.part000");
    let b = dir.path().join("a.part001");
    write_file(&a, b"AAAA");
    write_file(&b, b"BBBB");

    let merged = dir.path().join("out.bin");
    merge_chunks(&[a, b], &merged).expect("merge must succeed");
    assert_eq!(fs::read(&merged).unwrap(), b"AAAABBBB");
}
