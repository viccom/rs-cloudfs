//! CyDrive domain core.
//!
//! Pure domain layer: virtual-filesystem path semantics, SQLite metadata,
//! LRU cache management, client-side encryption and large-file chunking.
//! Remote storage is abstracted behind the `transport` trait seam; no
//! concrete network client lives here. See the design document at
//! `docs/rust-rewrite-design.md` (repository root).

pub mod bot;
pub mod cache;
pub mod chunker;
pub mod config;
pub mod credentials;
pub mod crypto;
pub mod database;
pub mod inbound;
pub mod logging;
pub mod rel_path;
pub mod sync;
pub mod transport;
pub mod upload_queue;
pub mod vfs;
