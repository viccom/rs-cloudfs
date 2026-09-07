//! Virtual path type for the CyDrive VFS.
//!
//! The implementation lives in L2 ([`cloudkit_storage::vpath`]) since
//! Batch R (Phase 1): the `UploadJob` contract carried by the transport
//! trait family needs it there, and removing the ck-telegram→core
//! reverse dependency required the type to be nameable below core. This
//! module re-exports it so every existing `crate::rel_path::RelPath` /
//! `cloudkit_core::rel_path::RelPath` reference keeps working unchanged.

pub use cloudkit_storage::vpath::{PathError, RelPath};
