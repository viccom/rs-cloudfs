//! In-memory [`CloudTransport`](super::CloudTransport) for tests.
//!
//! Scriptable error injection (connect failures, rate limits, mid-upload
//! disconnects) plus inspection APIs so tests can assert on stored
//! messages, remote names, captions and upload calls.
//!
//! The frozen behavior contract is pinned by `cloudkit-core`'s
//! `tests/transport.rs` (re-exported path); the trait-family split
//! semantics by this crate's `tests/transport_traits.rs`.

use super::{
    ByteStream, Capabilities, ChatCap, CloudTransport, InboundCap, IncomingEvent, IncomingStream,
    RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use bytes::Bytes;
use futures_core::Stream;
use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Scripted outcome of one `upload` call.
pub enum UploadAction {
    /// Upload all chunks and store them.
    Ok,
    /// Fail the call with `error` before any chunk is stored.
    Fail {
        /// Error to return.
        error: StorageError,
    },
    /// Store the first `chunks` chunks for real, then fail with `error`.
    FailAfterChunks {
        /// Number of leading chunks to store before failing.
        chunks: usize,
        /// Error to return after the prefix is stored.
        error: StorageError,
    },
}

/// Interior state of [`MockTransport`]; guarded by a single mutex.
///
/// Guards are never held across an await point (and never need to be:
/// all mock I/O is synchronous), so a plain `std::sync::Mutex` suffices.
impl Default for MockState {
    fn default() -> Self {
        Self {
            messages: BTreeMap::new(),
            // msg ids are allocated starting from 1 (contract 5).
            next_msg_id: 1,
            connected: false,
            connect_result: None,
            upload_script: VecDeque::new(),
            incoming_events: Vec::new(),
            upload_calls: Vec::new(),
            deleted: Vec::new(),
            open_delay: Duration::ZERO,
            sent_texts: Vec::new(),
            sent_documents: Vec::new(),
        }
    }
}

struct MockState {
    /// msg_id -> (stored bytes, remote document name, caption).
    messages: BTreeMap<i32, (Vec<u8>, String, String)>,
    /// Next msg_id to hand out; ids start at 1 (contract 5).
    next_msg_id: i32,
    /// Whether `connect()` has succeeded at least once.
    connected: bool,
    /// Scripted result of the first `connect()` call; `None` means Ok.
    connect_result: Option<Result<(), StorageError>>,
    /// Scripted upload outcomes, consumed in order.
    upload_script: VecDeque<UploadAction>,
    /// Events handed out by `incoming()`; drained on the first call.
    /// Results so a scripted transport error can interleave with events
    /// (the inbound worker must survive `Err` frames).
    incoming_events: Vec<Result<IncomingEvent, StorageError>>,
    /// Snapshot of every `upload()` job, in call order.
    upload_calls: Vec<UploadJob>,
    /// msg_ids successfully deleted, in order.
    deleted: Vec<i32>,
    /// Artificial pre-stream delay injected by `open`/`open_range`
    /// (tests simulate a stalling remote); zero by default.
    open_delay: Duration,
    /// Bot reply texts recorded by `send_text`, in call order.
    sent_texts: Vec<String>,
    /// Documents recorded by `send_document`, in call order.
    sent_documents: Vec<(String, Vec<u8>)>,
}

/// In-memory transport; interior state is private.
pub struct MockTransport {
    state: Arc<Mutex<MockState>>,
}

impl MockTransport {
    /// Default transport: connect succeeds, uploads always succeed, no
    /// incoming events.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState::default())),
        }
    }

    /// Starts building a transport with scripted behavior.
    pub fn builder() -> MockTransportBuilder {
        MockTransportBuilder {
            connect_result: None,
            upload_script: VecDeque::new(),
            incoming_events: Vec::new(),
            open_delay: Duration::ZERO,
        }
    }

    /// Locks the interior state; a poisoned lock surfaces as `Unavailable`.
    fn lock(&self) -> Result<MutexGuard<'_, MockState>, StorageError> {
        self.state
            .lock()
            .map_err(|_| StorageError::Unavailable("mock state lock poisoned".to_string()))
    }

    /// Stored bytes of the message `msg_id`, if present.
    pub fn message(&self, msg_id: i32) -> Option<Vec<u8>> {
        self.lock()
            .ok()?
            .messages
            .get(&msg_id)
            .map(|(bytes, _, _)| bytes.clone())
    }

    /// Remote document names of all sent messages, in order.
    pub fn message_names(&self) -> Vec<String> {
        self.lock()
            .map(|state| {
                state
                    .messages
                    .values()
                    .map(|(_, name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Captions of all sent messages, in order.
    pub fn message_captions(&self) -> Vec<String> {
        self.lock()
            .map(|state| {
                state
                    .messages
                    .values()
                    .map(|(_, _, caption)| caption.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Snapshot of every `upload` call's job, in order.
    pub fn upload_calls(&self) -> Vec<UploadJob> {
        self.lock()
            .map(|state| state.upload_calls.clone())
            .unwrap_or_default()
    }

    /// msg_ids successfully deleted, in order.
    pub fn deleted(&self) -> Vec<i32> {
        self.lock()
            .map(|state| state.deleted.clone())
            .unwrap_or_default()
    }

    /// Bot reply texts recorded by `send_text`, in call order.
    pub fn sent_texts(&self) -> Vec<String> {
        self.lock()
            .map(|state| state.sent_texts.clone())
            .unwrap_or_default()
    }

    /// Documents recorded by `send_document`, in call order.
    pub fn sent_documents(&self) -> Vec<(String, Vec<u8>)> {
        self.lock()
            .map(|state| state.sent_documents.clone())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl CloudTransport for MockTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        let mut state = self.lock()?;
        match state.connect_result.take() {
            // Scripted failure passes through and keeps the gate shut.
            Some(Err(error)) => Err(error),
            Some(Ok(())) | None => {
                state.connected = true;
                Ok(())
            }
        }
    }

    async fn upload(&self, job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        {
            let mut state = self.lock()?;
            state.upload_calls.push(job.clone());
            if !state.connected {
                return Err(StorageError::Invalid);
            }
        }

        // Read the real file; I/O failures surface as `StorageError::Io`.
        let data = std::fs::read(&job.local_path)?;

        let mut state = self.lock()?;
        let total = data.len() as u64;
        // Defensive: a plan with chunk_size 0 would otherwise divide by zero.
        let chunk_size = job.chunk_size.max(1);
        let actual_chunks = if total == 0 {
            1
        } else {
            total.div_ceil(chunk_size)
        };
        if actual_chunks != u64::from(job.chunk_count) {
            return Err(StorageError::Unavailable(format!(
                "chunk plan mismatch: {total} bytes at chunk_size {} split into \
                 {actual_chunks} chunks, but the job planned {}",
                job.chunk_size, job.chunk_count
            )));
        }

        // Consume the script in order; an exhausted script behaves as Ok.
        let action = match state.upload_script.pop_front() {
            Some(action) => action,
            None => UploadAction::Ok,
        };
        let total_chunks = actual_chunks as usize;
        let chunks_to_store = match action {
            UploadAction::Ok => total_chunks,
            UploadAction::Fail { error } => return Err(error),
            UploadAction::FailAfterChunks { chunks, error } => {
                let prefix = chunks.min(total_chunks);
                store_chunks(&mut state, job, &data, total_chunks, prefix);
                return Err(error);
            }
        };
        let chunk_msg_ids = store_chunks(&mut state, job, &data, total_chunks, chunks_to_store);
        let first_msg_id = chunk_msg_ids
            .first()
            .copied()
            .ok_or_else(|| StorageError::Unavailable("upload stored no chunks".to_string()))?;
        Ok(UploadReceipt {
            first_msg_id,
            chunk_msg_ids,
            uploaded_bytes: total,
        })
    }

    async fn open(&self, file: &RemoteHandle) -> Result<ByteStream, StorageError> {
        let (delay, data) = {
            let state = self.lock()?;
            if !state.connected {
                return Err(StorageError::Invalid);
            }
            (
                state.open_delay,
                concat_chunks(&state, &file.chunk_msg_ids)?,
            )
        };
        // Guard dropped before sleeping: locks never span an await.
        tokio::time::sleep(delay).await;
        Ok(frame_stream(vec![Ok(Bytes::from(data))]))
    }

    async fn open_range(
        &self,
        file: &RemoteHandle,
        off: u64,
        len: u64,
    ) -> Result<ByteStream, StorageError> {
        let (delay, data) = {
            let state = self.lock()?;
            if !state.connected {
                return Err(StorageError::Invalid);
            }
            let data = concat_chunks(&state, &file.chunk_msg_ids)?;
            let total = data.len() as u64;
            let slice = if off >= total {
                Vec::new()
            } else {
                let end = off.saturating_add(len).min(total);
                data[off as usize..end as usize].to_vec()
            };
            (state.open_delay, slice)
        };
        // Guard dropped before sleeping: locks never span an await.
        tokio::time::sleep(delay).await;
        Ok(frame_stream(vec![Ok(Bytes::from(data))]))
    }

    async fn delete_remote(&self, msg_id: i32) -> Result<(), StorageError> {
        let mut state = self.lock()?;
        if !state.connected {
            return Err(StorageError::Invalid);
        }
        match state.messages.remove(&msg_id) {
            Some(_) => {
                state.deleted.push(msg_id);
                Ok(())
            }
            None => Err(StorageError::NotFound),
        }
    }

    /// Mock declares exactly the three bits the upstream test suites
    /// exercise (bot replies, inbound events, range slicing); everything
    /// else stays off (R4: 宁缺勿滥 — this is test infrastructure, not a
    /// claim about any real backend).
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            inbound: true,
            chat: true,
            ..Capabilities::none()
        }
    }

    fn as_inbound(&self) -> Option<&dyn InboundCap> {
        Some(self)
    }

    fn as_chat(&self) -> Option<&dyn ChatCap> {
        Some(self)
    }
}

impl InboundCap for MockTransport {
    fn incoming(&self) -> IncomingStream {
        let events = self
            .lock()
            .map(|mut state| std::mem::take(&mut state.incoming_events))
            .unwrap_or_default();
        frame_stream(events)
    }
}

#[async_trait::async_trait]
impl ChatCap for MockTransport {
    async fn send_text(&self, text: &str) -> Result<(), StorageError> {
        let mut state = self.lock()?;
        state.sent_texts.push(text.to_string());
        Ok(())
    }

    async fn send_document(&self, name: &str, bytes: &[u8]) -> Result<(), StorageError> {
        let mut state = self.lock()?;
        state
            .sent_documents
            .push((name.to_string(), bytes.to_vec()));
        Ok(())
    }
}

/// Allocates msg_ids and stores the `count` leading chunks of `data`
/// following the naming and caption contract (contract 3). Returns the
/// allocated ids in order.
///
/// Naming: `{name}.part{NNN}` (0-based index, 3-digit zero-padded) for
/// multi-chunk uploads, the plain file name for single chunks, where
/// `name` is the rel_path basename including its extension (Python
/// baseline: `os.path.basename` of the virtual path). Every chunk caption
/// carries the rel_path and a 1-based `i/n` part marker (Python baseline:
/// enumerate is 0-based for names, captions print `idx + 1`).
fn store_chunks(
    state: &mut MockState,
    job: &UploadJob,
    data: &[u8],
    total_chunks: usize,
    count: usize,
) -> Vec<i32> {
    // Naming source is the virtual path's full basename (extension
    // included), mirroring the Python baseline (telegram_client.py:155).
    let file_name = job.rel_path.name();
    let chunk_size = job.chunk_size.max(1);

    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let start = ((index as u64)
            .saturating_mul(chunk_size)
            .min(data.len() as u64)) as usize;
        let end = ((index as u64 + 1)
            .saturating_mul(chunk_size)
            .min(data.len() as u64)) as usize;
        let name = if total_chunks > 1 {
            super::part_name(file_name, index)
        } else {
            file_name.to_owned()
        };
        let caption = format!(
            "{} (part {}/{})",
            job.rel_path.as_str(),
            index + 1,
            total_chunks
        );
        let msg_id = state.next_msg_id;
        state.next_msg_id += 1;
        state
            .messages
            .insert(msg_id, (data[start..end].to_vec(), name, caption));
        ids.push(msg_id);
    }
    ids
}

/// Concatenates the stored bytes of `chunk_msg_ids` in order; an unknown
/// id fails with `NotFound`.
fn concat_chunks(state: &MockState, chunk_msg_ids: &[i32]) -> Result<Vec<u8>, StorageError> {
    let mut data = Vec::new();
    for &msg_id in chunk_msg_ids {
        match state.messages.get(&msg_id) {
            Some((bytes, _, _)) => data.extend_from_slice(bytes),
            None => return Err(StorageError::NotFound),
        }
    }
    Ok(data)
}

/// Wraps fully-buffered frames into the boxed stream shapes used by
/// [`ByteStream`] and [`IncomingStream`].
fn frame_stream<T>(
    frames: Vec<Result<T, StorageError>>,
) -> Pin<Box<dyn Stream<Item = Result<T, StorageError>> + Send + Sync>>
where
    T: Send + Sync + 'static,
{
    Box::pin(futures_util::stream::iter(frames))
}

/// Builder for [`MockTransport`]; interior state is private.
pub struct MockTransportBuilder {
    connect_result: Option<Result<(), StorageError>>,
    upload_script: VecDeque<UploadAction>,
    incoming_events: Vec<Result<IncomingEvent, StorageError>>,
    open_delay: Duration,
}

impl MockTransportBuilder {
    /// Sets the result of the first `connect` call (default `Ok`).
    pub fn connect_result(mut self, result: Result<(), StorageError>) -> Self {
        self.connect_result = Some(result);
        self
    }

    /// Sets an artificial delay `open`/`open_range` sleep before
    /// returning the stream (simulates a stalling remote); default zero.
    pub fn open_delay(mut self, delay: Duration) -> Self {
        self.open_delay = delay;
        self
    }

    /// Appends a scripted upload outcome; scripts are consumed in order and
    /// exhausted scripts behave as [`UploadAction::Ok`].
    pub fn upload_action(mut self, action: UploadAction) -> Self {
        self.upload_script.push_back(action);
        self
    }

    /// Sets the events yielded by `incoming` (drained once).
    pub fn incoming(mut self, events: Vec<IncomingEvent>) -> Self {
        self.incoming_events = events.into_iter().map(Ok).collect();
        self
    }

    /// Sets the results yielded by `incoming` (drained once), errors
    /// included — the scripted-transport-error counterpart of
    /// [`MockTransportBuilder::incoming`].
    pub fn incoming_results(mut self, events: Vec<Result<IncomingEvent, StorageError>>) -> Self {
        self.incoming_events = events;
        self
    }

    /// Finishes the transport.
    pub fn build(self) -> MockTransport {
        MockTransport {
            state: Arc::new(Mutex::new(MockState {
                connect_result: self.connect_result,
                upload_script: self.upload_script,
                incoming_events: self.incoming_events,
                open_delay: self.open_delay,
                ..MockState::default()
            })),
        }
    }
}
