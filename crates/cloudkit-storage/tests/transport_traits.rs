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
use cloudkit_storage::transport::{ByteStream, ChatCap, CloudTransport, RemoteHandle, UploadJob};
use cloudkit_storage::vpath::RelPath;
use cloudkit_storage::{Capabilities, StorageError};
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
    async fn upload(
        &self,
        _job: &UploadJob,
    ) -> Result<cloudkit_storage::transport::UploadReceipt, StorageError> {
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
    let chat = t.as_chat().expect("mock implements ChatCap");
    chat.send_text("hello").await.expect("send_text");
    chat.send_text("again").await.expect("send_text");
    assert_eq!(
        t.sent_texts(),
        vec!["hello".to_string(), "again".to_string()]
    );
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

/// 8. The declared capability bits are injectable through the builder
///    (R-5: consumer degrade tests need a transport that *declares* bits
///    off — e.g. a storage-only backend with no RANGE_READ — while the
///    trait probes stay on, mirroring how a real driver's bit declaration
///    and optional-trait impls are two separate faces). The default
///    (plain `new` and a bare builder) keeps the three-bit declaration
///    test 2 pins.
#[test]
fn builder_overrides_declared_capabilities() {
    let injected = Capabilities::none();
    let t = MockTransport::builder().capabilities(injected).build();
    assert_eq!(
        t.capabilities(),
        injected,
        "the injected all-off declaration wins"
    );
    let injected = Capabilities {
        range_read: false,
        inbound: true,
        chat: false,
        ..Capabilities::none()
    };
    let t = MockTransport::builder().capabilities(injected).build();
    assert_eq!(t.capabilities(), injected, "a partial declaration wins");

    // Defaults unchanged: the bare builder still declares the three bits
    // exactly like plain `new()` (test 2 stays pinned).
    assert_eq!(
        MockTransport::builder().build().capabilities(),
        MockTransport::new().capabilities(),
        "bare builder default matches plain new()"
    );
}

/// Uploads `bytes` through the streaming face (no disk file needed) and
/// returns the receipt. Guard-rail helper for tests 9/10: the stream face
/// is behaviorally identical to the file face from the chunk plan onwards
/// (the mock's own contract), so receipts it produces are the real shape.
async fn stream_upload_bytes(
    t: &MockTransport,
    rel: &str,
    bytes: &'static [u8],
) -> cloudkit_storage::transport::UploadReceipt {
    let job = UploadJob {
        rel_path: RelPath::new(rel).expect("valid rel path"),
        local_path: PathBuf::new(),
        size: bytes.len() as u64,
        chunk_count: 1,
        chunk_size: 64,
    };
    let data: ByteStream = Box::pin(futures_util::stream::iter(vec![Ok(
        bytes::Bytes::from_static(bytes),
    )]));
    t.upload_stream(&job, data).await.expect("stream upload")
}

/// Drains a [`ByteStream`] to bytes; frames are infallible here (mock).
async fn drain(mut stream: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame.expect("frame ok"));
    }
    out
}

/// 9. K2 guard rail: `RemoteHandle::path` is an additive field — the
///    storage faces key on message ids, never on the path. Handles that
///    differ only in `path` (`None` vs `Some`) serve identical bytes
///    through `open`/`open_range`, so a path-addressed driver (local) can
///    carry its locator in the handle without perturbing the
///    telegram-era behavior of id-keyed backends.
///
/// RED-phase note (Batch B3a): this test fails to *compile* until the
/// field exists — the intentional guard-rail shape for an additive-type
/// evolution (see the batch's tracking entry).
#[tokio::test]
async fn remote_handle_path_is_additive_on_the_storage_faces() {
    let t = MockTransport::new();
    t.connect().await.expect("connect");
    let receipt = stream_upload_bytes(&t, "/guard.bin", b"0123456789").await;

    let no_path = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: receipt.uploaded_bytes,
        path: None,
    };
    let relocated = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: receipt.uploaded_bytes,
        path: Some(RelPath::new("/relocated/elsewhere.bin").expect("valid path")),
    };

    assert!(no_path.path.is_none(), "explicit None stays None");
    assert_eq!(
        relocated.path.as_ref().map(|p| p.as_str()),
        Some("/relocated/elsewhere.bin"),
        "the path payload round-trips untouched"
    );

    // Identical bytes regardless of the path payload.
    assert_eq!(
        drain(t.open(&no_path).await.expect("open: no path")).await,
        b"0123456789".to_vec()
    );
    assert_eq!(
        drain(t.open(&relocated).await.expect("open: some path")).await,
        b"0123456789".to_vec()
    );
    // Range parity too.
    assert_eq!(
        drain(t.open_range(&no_path, 2, 5).await.expect("range: no path")).await,
        b"23456".to_vec()
    );
    assert_eq!(
        drain(
            t.open_range(&relocated, 2, 5)
                .await
                .expect("range: some path")
        )
        .await,
        b"23456".to_vec()
    );
}

/// 10. K1/K3 guard rails: handle ids are i64 end-to-end (an id beyond the
///     i32 range flows through handle-shaped calls without narrowing —
///     it simply behaves like any unknown id), and `delete_remote` takes
///     the whole `&RemoteHandle`, deleting the handle's messages.
#[tokio::test]
async fn handle_ids_are_i64_and_delete_remote_takes_the_handle() {
    let t = MockTransport::new();
    t.connect().await.expect("connect");

    // An i64-range id the mock never stored: NotFound, not a narrowing
    // panic or a compile-time i32 conversion.
    let beyond_i32 = RemoteHandle {
        first_msg_id: 4_000_000_000,
        chunk_msg_ids: vec![4_000_000_000],
        total_size: 1,
        path: None,
    };
    assert!(
        matches!(
            t.delete_remote(&beyond_i32).await,
            Err(StorageError::NotFound)
        ),
        "unknown i64 id is a plain NotFound"
    );

    // Happy path: delete through a receipt-shaped handle; the i64 ids in
    // the deleted log match the receipt verbatim.
    let receipt = stream_upload_bytes(&t, "/gone.bin", b"xyz").await;
    let handle = RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: receipt.uploaded_bytes,
        path: None,
    };
    t.delete_remote(&handle).await.expect("delete via handle");
    assert_eq!(t.deleted(), vec![receipt.first_msg_id]);
    assert!(t.message(receipt.first_msg_id).is_none(), "message gone");
}
