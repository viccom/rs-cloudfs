//! RED-phase spec tests for `CloudTransport::upload_stream` (Batch E /
//! E-3, foundation D7): the streaming upload face the v2 chunked-AEAD
//! encryption path feeds. The trait method is provided with an
//! `Unsupported` default (capability-probe evolution rule, interfaces
//! §1), and the shared mock implements it by collecting the stream
//! frames into its store with the same chunk-plan and script semantics
//! as `upload`.

use std::time::Duration;

use cloudkit_storage::transport::mock::{MockTransport, UploadAction};
use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_storage::vpath::RelPath;
use cloudkit_storage::Capabilities;
use futures_util::stream;

/// A minimal transport that implements only the required methods and
/// deliberately does NOT override `upload_stream` — pinning the provided
/// default: `Unsupported`, never a panic.
struct DefaultFaceTransport;

#[async_trait::async_trait]
impl CloudTransport for DefaultFaceTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        Ok(())
    }
    async fn upload(&self, _job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        Err(StorageError::Unsupported)
    }
    async fn open(&self, _file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }
    async fn open_range(
        &self,
        _file: &RemoteHandle,
        _off: u64,
        _len: u64,
    ) -> Result<ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }
    async fn delete_remote(&self, _msg_id: i32) -> Result<(), StorageError> {
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }
}

fn bytes_stream(frames: Vec<Vec<u8>>) -> ByteStream {
    Box::pin(stream::iter(
        frames.into_iter().map(|f| Ok(bytes::Bytes::from(f))),
    ))
}

fn job(size: u64, chunk_size: u64, chunk_count: u32) -> UploadJob {
    UploadJob {
        rel_path: RelPath::new("/stream.bin").expect("rel path"),
        local_path: std::path::PathBuf::from("/proc/self/fd/none"),
        size,
        chunk_count,
        chunk_size,
    }
}

#[tokio::test]
async fn default_upload_stream_is_unsupported_not_a_panic() {
    let transport = DefaultFaceTransport;
    let err = transport
        .upload_stream(&job(3, 4, 1), bytes_stream(vec![b"abc".to_vec()]))
        .await
        .expect_err("the provided default must decline");
    assert!(matches!(err, StorageError::Unsupported), "got: {err:?}");
}

#[tokio::test]
async fn mock_upload_stream_collects_frames_into_chunked_messages() {
    let mock = MockTransport::new();
    mock.connect().await.expect("connect");
    // 10 bytes in three frames, chunk plan 4/4/2.
    let receipt = mock
        .upload_stream(
            &job(10, 4, 3),
            bytes_stream(vec![vec![1, 2, 3, 4, 5], vec![6, 7, 8, 9], vec![10]]),
        )
        .await
        .expect("stream upload");
    assert_eq!(receipt.uploaded_bytes, 10);
    assert_eq!(receipt.chunk_msg_ids.len(), 3, "chunk plan honored");
    assert_eq!(receipt.first_msg_id, receipt.chunk_msg_ids[0]);
    let joined: Vec<u8> = receipt
        .chunk_msg_ids
        .iter()
        .flat_map(|id| mock.message(*id).expect("stored"))
        .collect();
    assert_eq!(
        joined,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        "frames concatenated in order"
    );
    // The stream face records its own call log, separate from upload().
    assert_eq!(mock.stream_upload_calls().len(), 1);
    assert!(
        mock.upload_calls().is_empty(),
        "the stream face must not log into upload()"
    );
}

#[tokio::test]
async fn mock_upload_stream_enforces_the_chunk_plan() {
    let mock = MockTransport::new();
    mock.connect().await.expect("connect");
    // Stream carries 9 bytes but the plan says 10 — same mismatch
    // semantics as the file face.
    let err = mock
        .upload_stream(
            &job(10, 4, 3),
            bytes_stream(vec![vec![1, 2, 3, 4, 5], vec![6, 7, 8, 9]]),
        )
        .await
        .expect_err("plan mismatch must fail");
    assert!(matches!(err, StorageError::Unavailable(_)), "got: {err:?}");
}

#[tokio::test]
async fn mock_upload_stream_records_the_largest_frame() {
    let mock = MockTransport::new();
    mock.connect().await.expect("connect");
    mock.upload_stream(
        &job(6, 3, 2),
        bytes_stream(vec![vec![1, 2], vec![3, 4, 5], vec![6]]),
    )
    .await
    .expect("stream upload");
    assert_eq!(
        mock.max_stream_frame(),
        3,
        "peak frame size is observable for memory assertions"
    );
}

#[tokio::test]
async fn mock_upload_stream_requires_connect() {
    let mock = MockTransport::new();
    let err = mock
        .upload_stream(&job(3, 4, 1), bytes_stream(vec![b"abc".to_vec()]))
        .await
        .expect_err("not connected");
    assert!(matches!(err, StorageError::Invalid), "got: {err:?}");
}

#[tokio::test]
async fn mock_upload_stream_plays_the_upload_script() {
    // FailAfterChunks stores the prefix, then fails — the retry/degrade
    // machinery of the queue sees a transport failure mid-upload exactly
    // like the file face.
    let mock: MockTransport = MockTransport::builder()
        .upload_action(UploadAction::FailAfterChunks {
            chunks: 1,
            error: StorageError::Unavailable("boom mid-stream".to_string()),
        })
        .build();
    mock.connect().await.expect("connect");
    let err = mock
        .upload_stream(&job(4, 2, 2), bytes_stream(vec![vec![1, 2], vec![3, 4]]))
        .await
        .expect_err("scripted failure");
    assert!(matches!(err, StorageError::Unavailable(_)), "got: {err:?}");

    // The stored prefix is retrievable (partial-send observability).
    let stored: Vec<u8> = (1..)
        .map(|id| mock.message(id))
        .take_while(|m| m.is_some())
        .flat_map(|m| m.expect("stored prefix"))
        .collect();
    assert_eq!(stored, vec![1, 2], "the scripted prefix landed");

    // A plain Fail action fails before storing anything.
    let mock: MockTransport = MockTransport::builder()
        .upload_action(UploadAction::Fail {
            error: StorageError::RateLimited {
                retry_after: Some(Duration::from_millis(1)),
            },
        })
        .build();
    mock.connect().await.expect("connect");
    let err = mock
        .upload_stream(&job(4, 2, 2), bytes_stream(vec![vec![1, 2], vec![3, 4]]))
        .await
        .expect_err("scripted failure");
    assert!(
        matches!(
            err,
            StorageError::RateLimited {
                retry_after: Some(_)
            }
        ),
        "got: {err:?}"
    );
}
