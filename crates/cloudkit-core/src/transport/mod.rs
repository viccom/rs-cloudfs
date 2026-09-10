//! Transport seam between the domain core and a remote storage backend.
//!
//! The trait family has lived in L2 (`cloudkit_storage::transport`) since
//! Batch R (Phase 1): the core never links a concrete network client, it
//! depends on the [`CloudTransport`] trait only, and drivers must not
//! depend on core (architecture §1.5 transitional exemption, resolved).
//! This module re-exports the family plus its contract types and the
//! shared mock so every existing `cloudkit_core::transport::*` reference
//! keeps working unchanged.
//!
//! Error note (D2 convergence): the old `TransportError` is gone — all
//! operations speak [`cloudkit_storage::StorageError`]. The variant
//! mapping is documented at `cloudkit_storage::transport`'s module docs
//! (FloodWait → `RateLimited { retry_after }`, NotConnected → `Invalid`,
//! Disconnected/Remote → `Unavailable`, NotFound(id) → `NotFound`).

pub use cloudkit_storage::transport::{
    part_name, ByteStream, ChatCap, CloudTransport, InboundCap, InboundFile, IncomingEvent,
    IncomingStream, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
// The capability-set type the trait family's `capabilities()` speaks.
pub use cloudkit_storage::Capabilities;

/// Shared in-memory test transport (re-exported from L2).
pub mod mock {
    pub use cloudkit_storage::transport::mock::{
        MockTransport, MockTransportBuilder, OpenRangeAction, UploadAction,
    };
}
