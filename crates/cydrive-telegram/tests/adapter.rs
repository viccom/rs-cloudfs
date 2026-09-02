//! RED-phase spec tests for the three pure adapter modules of
//! `cydrive-telegram`: `plan` (upload send planning from an UploadJob),
//! `stream` (range-serving adapter over download iterators) and `config`
//! (baseline transport constants, contract 3). The tested functions are
//! `todo!()` stubs: every test here must fail with "not yet implemented"
//! until the GREEN implementation lands.

use std::path::PathBuf;
use std::task::{Context, Poll, Waker};

use cydrive_core::rel_path::RelPath;
use cydrive_core::transport::{ByteStream, UploadJob};
use cydrive_telegram::caption::{multi_part_caption, single_file_caption};
use cydrive_telegram::config::{
    TransportConfig, DEFAULT_API_HASH, DEFAULT_API_ID, DEFAULT_SESSION_STEM,
};
use cydrive_telegram::plan::{plan_chunk_sends, ChunkSend};
use cydrive_telegram::stream::serve_range;

/// Builds a pure (no disk access) UploadJob with the given chunk plan.
fn job_for(rel: &str, size: u64, chunk_count: u32, chunk_size: u64) -> UploadJob {
    UploadJob {
        rel_path: RelPath::new(rel).expect("valid rel path"),
        local_path: PathBuf::new(),
        size,
        chunk_count,
        chunk_size,
    }
}

/// Drives `stream` to completion synchronously by manually polling with a
/// no-op waker — the adapter is backed by an in-memory iterator and never
/// returns `Pending`. Manual polling (no async runtime) keeps this crate's
/// dev-dependencies free of tokio. `poll_next` resolves through the trait
/// object's principal trait (`dyn Stream`), so not even futures-core needs
/// to be a dev-dependency.
fn drain_sync(stream: ByteStream) -> Vec<u8> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut stream = stream;
    let mut out = Vec::new();
    loop {
        match stream.as_mut().poll_next(&mut cx) {
            Poll::Ready(Some(Ok(chunk))) => out.extend_from_slice(&chunk),
            Poll::Ready(Some(Err(e))) => panic!("unexpected transport error: {e}"),
            Poll::Ready(None) => return out,
            Poll::Pending => continue,
        }
    }
}

// ------------------------------------------------------------------- plan --

#[test]
fn plan_single_chunk_uses_basename_and_single_caption() {
    let job = job_for("/docs/a.txt", 13, 1, 64);
    let sends = plan_chunk_sends(&job, false);
    assert_eq!(sends.len(), 1);
    assert_eq!(
        sends[0],
        ChunkSend {
            document_name: "a.txt".to_string(),
            caption: single_file_caption("/docs/a.txt", 13, false),
            byte_len: 13,
        }
    );
}

#[test]
fn plan_single_chunk_encrypted_flag_flows_into_caption() {
    let job = job_for("/docs/a.txt", 13, 1, 64);
    let sends = plan_chunk_sends(&job, true);
    assert_eq!(sends.len(), 1);
    assert!(
        sends[0].caption.contains(" (🔒 AES Encrypted)"),
        "caption was: {}",
        sends[0].caption
    );
}

#[test]
fn plan_multi_chunk_three_parts_match_contract() {
    // 7 bytes over chunk_size 3 -> parts of 3/3/1 bytes (last is remainder).
    let job = job_for("/movie/data.bin", 7, 3, 3);
    let sends = plan_chunk_sends(&job, false);

    let names: Vec<&str> = sends.iter().map(|s| s.document_name.as_str()).collect();
    assert_eq!(
        names,
        ["data.bin.part000", "data.bin.part001", "data.bin.part002"]
    );
    let lens: Vec<u64> = sends.iter().map(|s| s.byte_len).collect();
    assert_eq!(lens, [3, 3, 1]);

    let expected = [(0, 3), (1, 3), (2, 1)];
    for (send, (idx, part_len)) in sends.iter().zip(expected) {
        assert_eq!(
            send.caption,
            multi_part_caption("/movie/data.bin", idx, 3, part_len),
            "caption mismatch on part index {idx}"
        );
    }
}

#[test]
fn plan_zero_byte_is_empty() {
    // Python never sends 0-byte files (upload is skipped upstream); the
    // planner mirrors that with an empty plan.
    let job = job_for("/docs/empty.txt", 0, 0, 64);
    let sends = plan_chunk_sends(&job, false);
    assert!(sends.is_empty());
}

// ----------------------------------------------------------------- stream --

#[test]
fn range_full_pass_through() {
    let chunks = [b"ab".to_vec(), b"cd".to_vec()];
    let out = drain_sync(serve_range(chunks.into_iter(), 0, 4));
    assert_eq!(out, b"abcd");
}

#[test]
fn range_head_skip_partial_first_chunk() {
    let chunks = [b"abc".to_vec(), b"def".to_vec()];
    let out = drain_sync(serve_range(chunks.into_iter(), 2, 3));
    assert_eq!(out, b"cde");
}

#[test]
fn range_take_stops_mid_chunk() {
    // Only 1 byte of the second chunk is taken; the rest is discarded.
    let chunks = [b"abc".to_vec(), b"def".to_vec()];
    let out = drain_sync(serve_range(chunks.into_iter(), 0, 4));
    assert_eq!(out, b"abcd");
}

#[test]
fn range_iterator_ends_early_no_error() {
    // Asked for 10 bytes, iterator only has 2 left: short read, no error.
    let chunks = [b"abc".to_vec()];
    let out = drain_sync(serve_range(chunks.into_iter(), 1, 10));
    assert_eq!(out, b"bc");
}

#[test]
fn range_zero_yield_is_empty() {
    let chunks = [b"abc".to_vec(), b"def".to_vec()];
    let out = drain_sync(serve_range(chunks.into_iter(), 0, 0));
    assert!(out.is_empty());
}

// ----------------------------------------------------------------- config --

#[test]
fn config_defaults_match_baseline_contract() {
    // Contract 3 constants, verbatim from the Python baseline config.py:12-13.
    assert_eq!(DEFAULT_API_ID, 6);
    assert_eq!(DEFAULT_API_HASH, "eb06d4abfb49dc3eeb1aeb98ae0f581e");
    assert_eq!(DEFAULT_SESSION_STEM, "cynet_bot_session");

    let cfg = TransportConfig::default();
    assert_eq!(cfg.api_id, 6);
    assert_eq!(cfg.api_hash, "eb06d4abfb49dc3eeb1aeb98ae0f581e");
    assert_eq!(cfg.bot_token, "");
    assert_eq!(cfg.chat_id, 0);
    assert_eq!(cfg.session_path, PathBuf::from("cynet_bot_session.session"));
}
