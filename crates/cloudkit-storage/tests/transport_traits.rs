//! New-semantics spec tests for the transport trait family in L2
//! (Phase 1 Batch R, R-3 / foundation D2+D3 / interfaces §1):
//!
//! * the narrowed [`CloudTransport`] core face (storage operations plus
//!   `capabilities()`, with `as_inbound`/`as_chat` probes defaulting to
//!   `None` for storage-only transports — probing never panics);
//! * the [`InboundCap`] / [`ChatCap`] optional-capability traits;
//! * the StorageError convergence: every operation signature (and the
//!   connect gate) speaks [`StorageError`], never a transport-local
//!   error type.
//!
//! These tests are RED until the `cloudkit_storage::transport` module
//! lands; together with the migrated `tests/transport.rs` contract suite
//! upstream in cloudkit-core (unchanged path, re-exported) they pin the
//! split.

use std::path::PathBuf;

use cloudkit_storage::transport::mock::MockTransport;
use cloudkit_storage::transport::{
    ByteStream, ChatCap, CloudTransport, InboundCap, RemoteHandle, UploadJob,
};
use cloudkit_storage::{Capabilities, StorageError};
use cloudkit_storage::vpath::RelPath;
use futures_util::StreamExt;

/// A storage-only transport: the bare core face, no optional traits, no
/// capability bits — the shape a baidu/local-style backend would expose
/// before Phase 2 wires it into a StorageDriver.
struct StorageOnlyTransport;

#[async_trait::async_trait]
impl CloudTransport for StorageOnlyTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        Ok(())
    }
    async fn upload(&self, _job: &UploadJob) -> Result<cloudkit_storage::transport::UploadReceipt, StorageError> {
        unimplemented!("not exercised")
    }
    async fn open(&self, _file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        unimplemented!("not exercised")
    }
    async fn open_range(
        &self,
        _file: &RemoteHandle,
        _off: u64,
        _len: u64,
    ) -> Result<ByteStream, StorageError> {
        unimplemented!("not exercised")
    }
    async fn delete_remote(&self, _msg_id: i32) -> Result<(), StorageError> {
        unimplemented!("not exercised")
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }
}

/// 1. A storage-only transport gets `None` from both probes by default
///    (provided methods, no panic, no implementation required).
#[test]
fn default_probes_are_none_for_storage_only_transport() {
    let t = StorageOnlyTransport;
    assert!(t.as_inbound().is_none(), "as_inbound defaults to None");
    assert!(t.as_chat().is_none(), "as_chat defaults to None");
}

/// 2. `capabilities()` is a required core-face method and the mock
///    declares exactly INBOUND / CHAT / RANGE_READ — the bits the bot,
///    inbound-worker and range-reading tests exercise — and nothing else
///    (R4: honest, 宁缺勿滥).
#[test]
fn mock_declares_inbound_chat_range_read_only() {
    let caps = MockTransport::new().capabilities();
    assert!(caps.inbound, "INBOUND declared");
    assert!(caps.chat, "CHAT declared");
    assert!(caps.range_read, "RANGE_READ declared");
    assert!(
        !caps.resume
            && !caps.multipart
            && !caps.server_side_move
            && !caps.rapid_upload
            && !caps.authoritative_index
            && !caps.change_feed,
        "no other bit is declared: {caps:?}"
    );
}

/// 3. The mock returns `Some` from both probes and the probed trait
///    objects carry the full chat semantics (texts recorded in order).
#[tokio::test]
async fn mock_probes_carry_chat_semantics() {
    let t = MockTransport::new();
    assert!(t.as_inbound().is_some(), "mock implements InboundCap");
    let chat = t
        .as_chat()
        .expect("mock implements ChatCap");
    chat.send_text("hello").await.expect("send_text");
    chat.send_text("again").await.expect("send_text");
    assert_eq!(t.sent_texts(), vec!["hello".to_string(), "again".to_string()]);
}

/// 4. A ChatCap implementor that overrides nothing gets the taxonomy's
///    `Unsupported` from both chat methods (the old default returned a
///    transport-local "not supported" Remote error; the converged default
///    is `StorageError::Unsupported`).
#[tokio::test]
async fn chat_cap_defaults_are_unsupported() {
    struct NoChat;
    #[async_trait::async_trait]
    impl ChatCap for NoChat {}
    let t = NoChat;
    assert!(
        matches!(t.send_text("x").await, Err(StorageError::Unsupported)),
        "default send_text is Unsupported"
    );
    assert!(
        matches!(
            t.send_document("x.bin", b"x").await,
            Err(StorageError::Unsupported)
        ),
        "default send_document is Unsupported"
    );
}

/// 5. The connect gate speaks StorageError: operating before `connect()`
///    maps the old `NotConnected` onto the taxonomy's invalid-state
///    variant (`StorageError::Invalid`). The job's local file is never
///    touched on this path, so a phantom path is fine.
#[tokio::test]
async fn connect_gate_maps_to_storage_invalid() {
    let t = MockTransport::new();
    let job = UploadJob {
        rel_path: RelPath::new("/gate.txt").expect("valid rel path"),
        local_path: PathBuf::from("/nonexistent/phantom.bin"),
        size: 7,
        chunk_count: 1,
        chunk_size: 64,
    };
    let err = t.upload(&job).await.unwrap_err();
    assert!(
        matches!(err, StorageError::Invalid),
        "upload before connect: {err:?}"
    );
    let err = t.delete_remote(1).await.unwrap_err();
    assert!(
        matches!(err, StorageError::Invalid),
        "delete_remote before connect: {err:?}"
    );
}

/// 6. The chunk-naming contract helper (`{base}.part{idx:03}`, compat
///    contract 3) is available from L2 for both the mock and drivers.
#[test]
fn part_name_contract_lives_in_l2() {
    use cloudkit_storage::transport::part_name;
    assert_eq!(part_name("data.bin", 0), "data.bin.part000");
    assert_eq!(part_name("data.bin", 42), "data.bin.part042");
    // Min-width padding: index 1000 keeps growing past three digits.
    assert_eq!(part_name("data.bin", 1000), "data.bin.part1000");
}

/// 7. The mock's inbound stream drains through the probed `InboundCap`
///    trait object exactly once (drain-once contract preserved through
///    the split).
#[tokio::test]
async fn inbound_probe_preserves_drain_once_events() {
    use cloudkit_storage::transport::{InboundFile, IncomingEvent};
    let t = MockTransport::builder()
        .incoming(vec![IncomingEvent::File(InboundFile {
            filename: "photo.jpg".to_string(),
            handle: RemoteHandle {
                first_msg_id: 10,
                chunk_msg_ids: vec![10],
                total_size: 42,
            },
        })])
        .build();
    let inbound = t.as_inbound().expect("mock implements InboundCap");
    let mut stream = inbound.incoming();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("event ok"));
    }
    assert_eq!(events.len(), 1, "the scripted event arrives via the probe");
    let mut again = inbound.incoming();
    let second = again.next().await;
    assert!(second.is_none(), "second drain yields nothing new");
}
