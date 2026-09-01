//! CyDrive domain core.
//!
//! Pure domain layer: virtual-filesystem path semantics, SQLite metadata,
//! LRU cache management, client-side encryption and large-file chunking.
//! No network or transport concerns live here; see the design document at
//! `docs/rust-rewrite-design.md` (repository root).
