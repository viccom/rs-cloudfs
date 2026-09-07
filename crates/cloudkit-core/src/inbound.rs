//! Inbound indexing worker (Python `_process_incoming_file` baseline,
//! `telegram_client.py:55-85`).
//!
//! Remote files that arrive through the transport's update stream are
//! indexed metadata-only: the payload stays on the remote until a
//! hydration asks for it (Zero-Disk / "Pure Cloud"). The worker drives
//! the whole stream lifecycle — a bad event or a failed row upsert only
//! warns, it never kills the worker.

use std::sync::Arc;

use futures_util::stream::StreamExt;

use crate::transport::{CloudTransport, IncomingEvent};
use crate::vfs::Vfs;

/// Handle to the running inbound worker; interior state is private.
pub struct InboundWorkerHandle {
    /// The worker task. Shutdown is abort-then-join rather than a
    /// CancellationToken: the mock transports used in tests hand out a
    /// drain-once finite stream, so the task usually finishes on its
    /// own and aborting it is a no-op — abort+join uniformly covers
    /// both the finished and the still-streaming cases. The join also
    /// makes a worker panic observable at shutdown instead of dying
    /// silently.
    task: tokio::task::JoinHandle<()>,
}

impl InboundWorkerHandle {
    /// Stops the worker and joins its task. Consuming `self` makes the
    /// call once-per-handle by construction (idempotent in effect).
    pub async fn shutdown(self) {
        // Aborting a task that already finished is a no-op, so this is
        // safe regardless of whether the incoming stream has ended.
        self.task.abort();
        // JoinError::cancelled is the expected outcome here; a panic in
        // the worker surfaces through the join instead of vanishing.
        if let Err(join_error) = self.task.await {
            if !join_error.is_cancelled() {
                tracing::warn!(%join_error, "inbound worker task panicked");
            }
        }
    }
}

/// Spawns the inbound worker: consumes `transport.incoming()`, indexes
/// [`IncomingEvent::File`] events into the VFS at the root (metadata
/// only, Python baseline) and dispatches [`IncomingEvent::Command`]
/// events to the bot command handler (replies go back over the same
/// transport). `Err` events and failed command handling only warn — a bad
/// event never kills the worker.
pub fn spawn_inbound_worker(
    vfs: Arc<Vfs>,
    transport: Arc<dyn CloudTransport>,
    drive_letter: String,
) -> InboundWorkerHandle {
    let task = tokio::spawn(async move {
        let mut stream = transport.incoming();
        while let Some(event) = stream.next().await {
            match event {
                Ok(IncomingEvent::File(file)) => {
                    // Success logging lives in `Vfs::index_inbound` (it
                    // owns the row that landed); here only failures
                    // surface.
                    if let Err(error) = vfs.index_inbound(file).await {
                        tracing::warn!(
                            %error,
                            "indexing an inbound remote file failed; skipping it"
                        );
                    }
                }
                Ok(IncomingEvent::Command { text }) => {
                    // Bot command dispatch: the reply target is the
                    // configured chat (send_text/send_document on the
                    // same transport). A failed reply only warns — the
                    // worker keeps consuming events.
                    if let Err(error) = crate::bot::handle_command(
                        &vfs.db(),
                        &vfs,
                        transport.as_ref(),
                        &drive_letter,
                        &text,
                    )
                    .await
                    {
                        tracing::warn!(%error, %text, "bot command failed; continuing");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "inbound stream error; continuing");
                }
            }
        }
    });
    InboundWorkerHandle { task }
}
