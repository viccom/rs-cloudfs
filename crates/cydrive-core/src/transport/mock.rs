//! In-memory [`CloudTransport`](super::CloudTransport) for tests.
//!
//! Scriptable error injection (connect failures, FloodWait, mid-upload
//! disconnects) plus inspection APIs so tests can assert on stored
//! messages, remote names, captions and upload calls.
//!
//! RED phase stub: every function body is `todo!()`.

use super::{
    ByteStream, CloudTransport, IncomingEvent, IncomingStream, RemoteHandle, TransportError,
    UploadJob, UploadReceipt,
};

/// Scripted outcome of one `upload` call.
pub enum UploadAction {
    /// Upload all chunks and store them.
    Ok,
    /// Fail the call with `error` before any chunk is stored.
    Fail {
        /// Error to return.
        error: TransportError,
    },
    /// Store the first `chunks` chunks for real, then fail with `error`.
    FailAfterChunks {
        /// Number of leading chunks to store before failing.
        chunks: usize,
        /// Error to return after the prefix is stored.
        error: TransportError,
    },
}

/// In-memory transport; interior state is private.
pub struct MockTransport {}

impl MockTransport {
    /// Default transport: connect succeeds, uploads always succeed, no
    /// incoming events.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        todo!()
    }

    /// Starts building a transport with scripted behavior.
    pub fn builder() -> MockTransportBuilder {
        todo!()
    }

    /// Stored bytes of the message `msg_id`, if present.
    #[allow(unused_variables)] // RED stub: body is todo!()
    pub fn message(&self, msg_id: i32) -> Option<Vec<u8>> {
        todo!()
    }

    /// Remote document names of all sent messages, in order.
    pub fn message_names(&self) -> Vec<String> {
        todo!()
    }

    /// Captions of all sent messages, in order.
    pub fn message_captions(&self) -> Vec<String> {
        todo!()
    }

    /// Snapshot of every `upload` call's job, in order.
    pub fn upload_calls(&self) -> Vec<UploadJob> {
        todo!()
    }

    /// msg_ids successfully deleted, in order.
    pub fn deleted(&self) -> Vec<i32> {
        todo!()
    }
}

#[async_trait::async_trait]
impl CloudTransport for MockTransport {
    async fn connect(&self) -> Result<(), TransportError> {
        todo!()
    }

    #[allow(unused_variables)] // RED stub: body is todo!()
    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, TransportError> {
        todo!()
    }

    #[allow(unused_variables)] // RED stub: body is todo!()
    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, TransportError> {
        todo!()
    }

    #[allow(unused_variables)] // RED stub: body is todo!()
    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, TransportError> {
        todo!()
    }

    #[allow(unused_variables)] // RED stub: body is todo!()
    async fn delete_remote(&self, msg_id: i32) -> Result<(), TransportError> {
        todo!()
    }

    fn incoming(&self) -> IncomingStream {
        todo!()
    }
}

/// Builder for [`MockTransport`]; interior state is private.
pub struct MockTransportBuilder {}

impl MockTransportBuilder {
    /// Sets the result of the first `connect` call (default `Ok`).
    #[allow(unused_variables)] // RED stub: body is todo!()
    pub fn connect_result(self, result: Result<(), TransportError>) -> Self {
        todo!()
    }

    /// Appends a scripted upload outcome; scripts are consumed in order and
    /// exhausted scripts behave as [`UploadAction::Ok`].
    #[allow(unused_variables)] // RED stub: body is todo!()
    pub fn upload_action(self, action: UploadAction) -> Self {
        todo!()
    }

    /// Sets the events yielded by `incoming` (drained once).
    #[allow(unused_variables)] // RED stub: body is todo!()
    pub fn incoming(self, events: Vec<IncomingEvent>) -> Self {
        todo!()
    }

    /// Finishes the transport.
    pub fn build(self) -> MockTransport {
        todo!()
    }
}
