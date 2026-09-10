//! `cloudkit-winfsp` — native WinFsp mount adapter over the VFS (L5).
//!
//! Thin-adapter discipline (Phase 3, K39): this crate owns a handle
//! table, the Win32/NTSTATUS translation and the mount lifecycle — every
//! read/write strategy stays in the semantic core (`cloudkit-core`), and
//! nothing here ever reaches into a driver crate (R1).
//!
//! Everything is behind `cfg(all(windows, feature = "winfsp"))`:
//! without the feature (or off Windows) this crate compiles to an empty
//! library and the workspace graph contains no winfsp dependency at all
//! (K38 license isolation — the bindings are GPL-3.0).
//!
//! Batch status (WF1 = skeleton + async bridge + readonly metadata face;
//! WF2 = the read path):
//! - [`fs::CloudFs`] answers `get_security_by_name` / `open` / `close` /
//!   `get_file_info` / `read_directory` / `get_volume_info` from the
//!   local metadata rows and an assembly-time volume snapshot (K44, zero
//!   network);
//! - WF2 adds the read path (K33 triple gate -> [`reader::WindowReader`]
//!   bounded windows or [`reader::LocalReader`] over the hydrate arm),
//!   the zero-side-effect `flush` and the K41 handle grace period;
//!   WF3 adds the staged write path and the filesystem operations; WF4
//!   the mount lifecycle, `winfsp_init` error surfacing and the K40
//!   fallback to WebDAV.
#![cfg_attr(not(all(windows, feature = "winfsp")), allow(unused))]

#[cfg(all(windows, feature = "winfsp"))]
pub mod bridge;
#[cfg(all(windows, feature = "winfsp"))]
pub mod error;
#[cfg(all(windows, feature = "winfsp"))]
pub mod fs;
#[cfg(all(windows, feature = "winfsp"))]
pub mod mount;
#[cfg(all(windows, feature = "winfsp"))]
pub mod reader;
#[cfg(all(windows, feature = "winfsp"))]
pub mod writer;
