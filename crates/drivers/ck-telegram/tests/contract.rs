//! Byte-exact contract tests for the pure logic of `ck-telegram`
//! (caption snapshots, flood-wait parsing, range planning), frozen against
//! the Python baseline. See `docs/decisions.md` 2026-09-03 for the
//! chunk-naming erratum these expectations follow.

use ck_telegram::caption::{
    clean_rel_path, multi_part_caption, part_document_name, single_file_caption,
};
use ck_telegram::flood::parse_flood_wait;
use ck_telegram::range::{range_plan, RangePlan};

// ---------------------------------------------------------------- caption --

#[test]
fn single_caption_plain_matches_python_snapshot() {
    let caption = single_file_caption("/docs/a.bin", 13, false);
    assert_eq!(
        caption,
        "🚀 **CyDrive Cloud Backup**\n📁 Path: `/docs/a.bin`\n📦 Size: `0 KB`"
    );
}

#[test]
fn single_caption_encrypted_appends_suffix() {
    let caption = single_file_caption("/docs/a.bin", 13, true);
    assert_eq!(
        caption,
        "🚀 **CyDrive Cloud Backup**\n📁 Path: `/docs/a.bin`\n📦 Size: `0 KB` (🔒 AES Encrypted)"
    );
}

#[test]
fn single_caption_kb_is_integer_division() {
    // 2049 / 1024 = 2 by integer division.
    let caption = single_file_caption("/docs/a.bin", 2049, false);
    assert_eq!(
        caption,
        "🚀 **CyDrive Cloud Backup**\n📁 Path: `/docs/a.bin`\n📦 Size: `2 KB`"
    );
}

#[test]
fn multi_caption_first_of_three_matches_python_snapshot() {
    let caption = multi_part_caption("/movie/big.bin", 0, 3, 3);
    assert_eq!(
        caption,
        "🚀 **CyDrive Multi-Part Cloud Archive**\n📁 File: `/movie/big.bin`\n🧩 Part: `1/3` (0 KB)"
    );
}

#[test]
fn multi_caption_last_part_and_kb_rounding() {
    // 1049600 / 1024 = 1025 exactly; part_index 2 of 3 prints 1-based as 3/3.
    let caption = multi_part_caption("/movie/big.bin", 2, 3, 1_049_600);
    assert!(
        caption.contains("🧩 Part: `3/3` (1025 KB)"),
        "caption was: {caption}"
    );
}

#[test]
fn part_document_name_zero_based_three_digits() {
    assert_eq!(part_document_name("data.bin", 0), "data.bin.part000");
    assert_eq!(part_document_name("data.bin", 12), "data.bin.part012");
}

#[test]
fn clean_rel_path_normalizes_like_python() {
    assert_eq!(clean_rel_path("/a/b"), "/a/b");
    assert_eq!(clean_rel_path("a/b"), "/a/b");
    // Python (telegram_client.py:154) strips '/' BEFORE replacing '\', so a
    // backslash input keeps the prepended slash: "//a/b", double slash and all.
    // Faithful-to-baseline is the contract; a stronger normalization would be
    // an unapproved contract change (core RelPath rejects '\' at the type
    // level anyway, so this is defensive parity only).
    assert_eq!(clean_rel_path("\\a\\b"), "//a/b");
    assert_eq!(clean_rel_path("/a/"), "/a");
    assert_eq!(clean_rel_path("//x//"), "/x");
    assert_eq!(clean_rel_path("/"), "/");
}

// ------------------------------------------------------------------ flood --

#[test]
fn flood_wait_parses_seconds_suffix() {
    assert_eq!(parse_flood_wait("FLOOD_WAIT_30"), Some(30));
    assert_eq!(parse_flood_wait("FLOOD_WAIT_0"), Some(0));
}

#[test]
fn bare_flood_wait_is_zero() {
    assert_eq!(parse_flood_wait("FLOOD_WAIT"), Some(0));
}

#[test]
fn non_flood_names_are_none() {
    assert_eq!(parse_flood_wait("TIMEOUT"), None);
    assert_eq!(parse_flood_wait("FLOOD_WAIT_abc"), None);
    assert_eq!(parse_flood_wait(""), None);
    // PREMIUM variant is left to the caller's is() matching, not this parser.
    assert_eq!(parse_flood_wait("FLOOD_PREMIUM_WAIT_9"), None);
}

// ------------------------------------------------------------------ range --

#[test]
fn aligned_offset_skips_whole_chunks() {
    let plan = range_plan(8192, 100, 4096).unwrap();
    assert_eq!(
        plan,
        RangePlan {
            skip_chunks: 2,
            skip_bytes_in_first_chunk: 0,
            bytes_to_yield: 100,
        }
    );
}

#[test]
fn unaligned_offset_splits_head() {
    // 10000 = 2 * 4096 + 1808, so 1808 bytes must be discarded in chunk #2.
    // (The spec example said 1816; arithmetic gives 1808 and wins.)
    let plan = range_plan(10000, 5, 4096).unwrap();
    assert_eq!(
        plan,
        RangePlan {
            skip_chunks: 2,
            skip_bytes_in_first_chunk: 1808,
            bytes_to_yield: 5,
        }
    );
}

#[test]
fn zero_offset_zero_length() {
    let plan = range_plan(0, 0, 4096).unwrap();
    assert_eq!(
        plan,
        RangePlan {
            skip_chunks: 0,
            skip_bytes_in_first_chunk: 0,
            bytes_to_yield: 0,
        }
    );
}

#[test]
fn max_chunk_size_is_valid() {
    let chunk = 524_288; // 512 * 1024, the upper bound
    let offset = 524_288 * 3 + 7;
    let plan = range_plan(offset, 999, chunk).unwrap();
    assert_eq!(
        plan,
        RangePlan {
            skip_chunks: 3,
            skip_bytes_in_first_chunk: 7,
            bytes_to_yield: 999,
        }
    );
}

#[test]
fn invalid_chunk_sizes_rejected() {
    for bad in [0i32, -4096, 4095, 4097, 524_288 + 4096] {
        assert!(
            range_plan(0, 1, bad).is_err(),
            "chunk size {bad} should be rejected"
        );
    }
}
