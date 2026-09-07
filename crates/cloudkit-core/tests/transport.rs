//! RED-phase tests for `cloudkit_core::transport` (CloudTransport trait +
//! MockTransport). All bodies are expected to panic with "not yet
//! implemented" until the GREEN phase lands.
//!
//! Contract under test (design doc "核心抽象：CloudTransport" + compat
//! contracts 3/5): connection gating, msg_id allocation from 1, chunk
//! naming `{name}.part{NNN}` (name = rel_path basename incl. extension,
//! 3-digit zero-padded, 0-based index), captions
//! containing the rel_path and 1-based `i/n`, `telegram_msg_id` = chunk 0's
//! msg_id,
//! scripted error injection (FloodWait retry, FailAfterChunks), byte-exact
//! open / open_range slicing, delete semantics, and drain-once incoming
//! events.

use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, UploadAction};
use cloudkit_core::transport::{
    ByteStream, CloudTransport, InboundFile, IncomingEvent, IncomingStream, RemoteHandle,
    TransportError, UploadJob,
};
use futures_util::StreamExt;
use std::fs;
use std::path::PathBuf;

/// Concatenates every chunk of a byte stream; propagates the first error.
async fn drain(stream: ByteStream) -> Result<Vec<u8>, TransportError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// Collects every event of an incoming stream; propagates the first error.
async fn drain_events(stream: IncomingStream) -> Result<Vec<IncomingEvent>, TransportError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(event) = stream.next().await {
        out.push(event?);
    }
    Ok(out)
}

/// Writes `data` to a real temp file and returns (kept-alive dir, path).
fn write_temp_file(name: &str, data: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join(name);
    fs::write(&path, data).expect("write temp file");
    (dir, path)
}

/// Builds an UploadJob for `rel` with the given chunk plan.
fn job_for(
    rel: &str,
    local_path: PathBuf,
    size: u64,
    chunk_count: u32,
    chunk_size: u64,
) -> UploadJob {
    UploadJob {
        rel_path: RelPath::new(rel).expect("valid rel path"),
        local_path,
        size,
        chunk_count,
        chunk_size,
    }
}

/// A handle pointing at the given chunk ids (single-chunk convenience).
fn handle_for(first_msg_id: i32, total_size: u64) -> RemoteHandle {
    RemoteHandle {
        first_msg_id,
        chunk_msg_ids: vec![first_msg_id],
        total_size,
    }
}

/// 1. Without connect(), every mutating operation is rejected.
#[tokio::test]
async fn operations_before_connect_return_not_connected() {
    let (_dir, path) = write_temp_file("gate.txt", b" gated ");
    let t = MockTransport::new();
    let job = job_for("/gate.txt", path, 7, 1, 64);

    let err = t.upload(&job).await.unwrap_err();
    assert!(
        matches!(err, TransportError::NotConnected),
        "upload: {err:?}"
    );

    // ByteStream has no Debug, so assert on the Result directly.
    assert!(
        matches!(
            t.open(&handle_for(1, 7)).await,
            Err(TransportError::NotConnected)
        ),
        "open"
    );
    assert!(
        matches!(
            t.open_range(&handle_for(1, 7), 0, 3).await,
            Err(TransportError::NotConnected)
        ),
        "open_range"
    );

    let err = t.delete_remote(1).await.unwrap_err();
    assert!(
        matches!(err, TransportError::NotConnected),
        "delete_remote: {err:?}"
    );
}

/// 2. A scripted connect failure is passed through and the gate stays shut.
#[tokio::test]
async fn scripted_connect_failure_passes_through_and_keeps_gate_closed() {
    let (_dir, path) = write_temp_file("gate2.txt", b"still gated");
    let t = MockTransport::builder()
        .connect_result(Err(TransportError::Disconnected("auth rejected".into())))
        .build();

    let err = t.connect().await.unwrap_err();
    assert!(
        matches!(err, TransportError::Disconnected(ref msg) if msg == "auth rejected"),
        "connect: {err:?}"
    );

    let job = job_for("/gate2.txt", path, 12, 1, 64);
    let err = t.upload(&job).await.unwrap_err();
    assert!(
        matches!(err, TransportError::NotConnected),
        "upload: {err:?}"
    );
}

/// 3. Single-chunk upload: receipt fields, exact bytes, plain name,
///    caption carries rel_path and "1/1" (contract 3).
#[tokio::test]
async fn single_chunk_upload_stores_bytes_name_and_caption() {
    let (_dir, path) = write_temp_file("hello.txt", b"hello cydrive");
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    let job = job_for("/docs/hello.txt", path, 13, 1, 64);
    let receipt = t.upload(&job).await.expect("upload");

    assert_eq!(receipt.chunk_msg_ids.len(), 1);
    assert_eq!(
        receipt.first_msg_id, receipt.chunk_msg_ids[0],
        "contract 5: first_msg_id is chunk 0's msg_id"
    );
    assert_eq!(receipt.first_msg_id, 1, "msg ids start at 1");
    assert_eq!(receipt.uploaded_bytes, 13);

    assert_eq!(
        t.message(receipt.first_msg_id),
        Some(b"hello cydrive".to_vec()),
        "stored bytes must match the file byte-for-byte"
    );
    assert_eq!(t.message_names(), vec!["hello.txt".to_string()]);
    let captions = t.message_captions();
    assert_eq!(captions.len(), 1);
    assert!(
        captions[0].contains("/docs/hello.txt"),
        "caption carries rel_path: {:?}",
        captions[0]
    );
    assert!(
        captions[0].contains("1/1"),
        "caption carries i/n: {:?}",
        captions[0]
    );
}

/// 4. Multi-chunk upload (7 bytes / chunk_size 3 -> 3 chunks): consecutive
///    msg ids, `{name}.part{NNN}` names (rel_path basename incl.
///    extension), per-chunk captions (contract 3).
#[tokio::test]
async fn multi_chunk_upload_names_and_captions_follow_contract() {
    let (_dir, path) = write_temp_file("data.bin", b"abcdefg");
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    let job = job_for("/docs/data.bin", path, 7, 3, 3);
    let receipt = t.upload(&job).await.expect("upload");

    assert_eq!(receipt.chunk_msg_ids, vec![1, 2, 3]);
    assert_eq!(receipt.first_msg_id, 1);
    assert_eq!(receipt.uploaded_bytes, 7);
    assert_eq!(
        t.message_names(),
        vec![
            "data.bin.part000".to_string(),
            "data.bin.part001".to_string(),
            "data.bin.part002".to_string(),
        ]
    );
    assert_eq!(t.message(1), Some(b"abc".to_vec()));
    assert_eq!(t.message(2), Some(b"def".to_vec()));
    assert_eq!(t.message(3), Some(b"g".to_vec()));

    let captions = t.message_captions();
    assert_eq!(captions.len(), 3);
    for (i, caption) in captions.iter().enumerate() {
        let index = i + 1;
        assert!(
            caption.contains("/docs/data.bin"),
            "caption {index} carries rel_path: {caption:?}"
        );
        assert!(
            caption.contains(&format!("{index}/3")),
            "caption {index} carries i/n: {caption:?}"
        );
    }
}

/// 5. chunk_count that disagrees with the real split fails with Remote and
///    stores nothing.
#[tokio::test]
async fn chunk_count_mismatch_fails_without_storing_messages() {
    let (_dir, path) = write_temp_file("plan.bin", b"abcdefg");
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    // 7 bytes / chunk_size 3 really splits into 3 chunks; claim 2.
    let job = job_for("/plan.bin", path, 7, 2, 3);
    let err = t.upload(&job).await.unwrap_err();
    assert!(
        matches!(&err, TransportError::Remote(msg) if msg.contains("chunk plan mismatch")),
        "expected Remote(chunk plan mismatch ...), got: {err:?}"
    );

    assert!(t.message_names().is_empty(), "no names stored");
    assert!(t.message_captions().is_empty(), "no captions stored");
    assert_eq!(t.message(1), None, "no message stored");
}

/// 6. Scripted FloodWait then Ok: first call fails with the exact wait,
///    retry succeeds, both calls are recorded.
#[tokio::test]
async fn flood_wait_script_fails_then_retry_succeeds() {
    let (_dir, path) = write_temp_file("flood.bin", b"flooding");
    let t = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: TransportError::FloodWait { seconds: 30 },
        })
        .upload_action(UploadAction::Ok)
        .build();
    t.connect().await.expect("connect");

    let job = job_for("/flood.bin", path, 8, 1, 64);
    let err = t.upload(&job).await.unwrap_err();
    assert!(
        matches!(err, TransportError::FloodWait { seconds: 30 }),
        "first upload: {err:?}"
    );

    let receipt = t.upload(&job).await.expect("retry upload succeeds");
    assert_eq!(receipt.uploaded_bytes, 8);
    assert_eq!(t.upload_calls().len(), 2, "both calls recorded");
}

/// 7. FailAfterChunks stores the prefix for real; once the script is
///    exhausted the retry stores all chunks again under fresh msg ids.
#[tokio::test]
async fn fail_after_chunks_persists_prefix_then_retry_completes() {
    let (_dir, path) = write_temp_file("clip.bin", b"abcdefg");
    let t = MockTransport::builder()
        .upload_action(UploadAction::FailAfterChunks {
            chunks: 2,
            error: TransportError::Disconnected("mid-upload".into()),
        })
        .build();
    t.connect().await.expect("connect");

    let job = job_for("/docs/clip.bin", path, 7, 3, 3);
    let err = t.upload(&job).await.unwrap_err();
    assert!(
        matches!(err, TransportError::Disconnected(_)),
        "first upload: {err:?}"
    );

    // The two prefix chunks were really stored.
    assert_eq!(
        t.message_names(),
        vec![
            "clip.bin.part000".to_string(),
            "clip.bin.part001".to_string(),
        ]
    );
    assert_eq!(t.message_captions().len(), 2);
    assert_eq!(t.message(1), Some(b"abc".to_vec()));
    assert_eq!(t.message(2), Some(b"def".to_vec()));

    // Script exhausted -> next upload behaves as Ok; msg ids keep counting.
    let receipt = t.upload(&job).await.expect("retry after exhausted script");
    assert_eq!(receipt.chunk_msg_ids, vec![3, 4, 5]);
    let names = t.message_names();
    assert_eq!(names.len(), 5);
    assert_eq!(
        &names[2..],
        &[
            "clip.bin.part000".to_string(),
            "clip.bin.part001".to_string(),
            "clip.bin.part002".to_string(),
        ]
    );
}

/// 8. open() streams back the exact original bytes of a chunked upload.
#[tokio::test]
async fn open_round_trips_full_chunked_file() {
    let (_dir, path) = write_temp_file("roundtrip.bin", b"abcdefg");
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    let job = job_for("/roundtrip.bin", path, 7, 3, 3);
    let receipt = t.upload(&job).await.expect("upload");

    let handle = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: 7,
    };
    let bytes = drain(t.open(&handle).await.expect("open"))
        .await
        .expect("stream ok");
    assert_eq!(bytes, b"abcdefg");
}

/// 9. open() with an unknown msg_id fails with NotFound(id).
#[tokio::test]
async fn open_unknown_msg_id_returns_not_found() {
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    let handle = handle_for(999, 3);
    assert!(
        matches!(
            t.open(&handle).await,
            Err(TransportError::NotFound(id)) if id == 999
        ),
        "open with unknown msg id"
    );
}

/// 10. open_range() slices the concatenated stream; the tail clamps to EOF
///     and off >= EOF yields an empty stream without error.
#[tokio::test]
async fn open_range_slices_middle_clamps_tail_and_empty_beyond_eof() {
    let (_dir, path) = write_temp_file("range.bin", b"abcdefg");
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    let job = job_for("/range.bin", path, 7, 3, 3);
    let receipt = t.upload(&job).await.expect("upload");
    let handle = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: 7,
    };

    let bytes = drain(t.open_range(&handle, 2, 3).await.expect("open_range"))
        .await
        .expect("stream ok");
    assert_eq!(bytes, b"cde", "exact middle slice");

    let bytes = drain(t.open_range(&handle, 5, 100).await.expect("open_range"))
        .await
        .expect("stream ok");
    assert_eq!(bytes, b"fg", "tail clamps to EOF");

    let bytes = drain(t.open_range(&handle, 7, 3).await.expect("open_range"))
        .await
        .expect("stream ok");
    assert!(bytes.is_empty(), "off >= EOF yields an empty stream");
}

/// 11. delete_remote() removes the message, is recorded, and unknown ids
///     fail with NotFound.
#[tokio::test]
async fn delete_remote_removes_message_and_rejects_unknown_ids() {
    let (_dir, path) = write_temp_file("del.txt", b"delete me");
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    let job = job_for("/del.txt", path, 9, 1, 64);
    let receipt = t.upload(&job).await.expect("upload");
    let handle = handle_for(receipt.first_msg_id, 9);

    t.delete_remote(receipt.first_msg_id)
        .await
        .expect("delete stored message");
    assert_eq!(t.deleted(), vec![receipt.first_msg_id]);

    assert!(
        matches!(
            t.open(&handle).await,
            Err(TransportError::NotFound(id)) if id == receipt.first_msg_id
        ),
        "open after delete must fail NotFound"
    );

    let err = t.delete_remote(4242).await.unwrap_err();
    assert!(
        matches!(err, TransportError::NotFound(id) if id == 4242),
        "delete unknown: {err:?}"
    );
}

/// 12. incoming() drains scripted events once, in order; the second call
///     yields nothing new. Not gated by connect().
#[tokio::test]
async fn incoming_drains_scripted_events_once_in_order() {
    let t = MockTransport::builder()
        .incoming(vec![
            IncomingEvent::File(InboundFile {
                filename: "photo.jpg".to_string(),
                handle: RemoteHandle {
                    first_msg_id: 10,
                    chunk_msg_ids: vec![10],
                    total_size: 42,
                },
            }),
            IncomingEvent::Command {
                text: "/stats".to_string(),
            },
        ])
        .build();

    let events = drain_events(t.incoming()).await.expect("first drain");
    assert_eq!(events.len(), 2, "both scripted events arrive");
    match &events[0] {
        IncomingEvent::File(f) => {
            assert_eq!(f.filename, "photo.jpg");
            assert_eq!(f.handle.first_msg_id, 10);
            assert_eq!(f.handle.chunk_msg_ids, vec![10]);
            assert_eq!(f.handle.total_size, 42);
        }
        other => panic!("first event must be File, got {other:?}"),
    }
    match &events[1] {
        IncomingEvent::Command { text } => assert_eq!(text, "/stats"),
        other => panic!("second event must be Command, got {other:?}"),
    }

    let again = drain_events(t.incoming()).await.expect("second drain");
    assert!(again.is_empty(), "second drain yields no new events");
}

/// 13. upload_calls() snapshots every job's fields.
#[tokio::test]
async fn upload_calls_snapshot_records_job_fields() {
    let (_dir, path) = write_temp_file("snap.bin", b"12345");
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    let job = job_for("/snap.bin", path.clone(), 5, 1, 64);
    t.upload(&job).await.expect("upload");

    let calls = t.upload_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].rel_path, RelPath::new("/snap.bin").unwrap());
    assert_eq!(calls[0].local_path, path);
    assert_eq!(calls[0].size, 5);
    assert_eq!(calls[0].chunk_count, 1);
    assert_eq!(calls[0].chunk_size, 64);
}
