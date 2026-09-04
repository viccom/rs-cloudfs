//! Compile-only non-Linux stubs for the [`linux`] module surface —
//! same signatures, no behaviour. Keeps the workspace buildable on
//! Windows/macOS targets (the same cfg discipline as
//! `windows_stub.rs`; macOS's `mount_webdav` real implementation is
//! deliberately out of scope per the plan's YAGNI list).
//!
//! [`linux`]: crate::linux

use std::path::Path;

use crate::PlatformError;

/// The single message every stub returns.
const UNSUPPORTED: &str = "linux-only mount chain";

/// Stub answer: [`PlatformError::Unsupported`].
fn unsupported<T>() -> Result<T, PlatformError> {
    Err(PlatformError::Unsupported(UNSUPPORTED))
}

/// Stub: [`unsupported`].
pub fn mount_drive(_mount_point: &Path, _url: &str) -> Result<String, PlatformError> {
    unsupported()
}

/// Stub: [`unsupported`].
pub fn unmount_drive(_mount_point: &Path) -> Result<String, PlatformError> {
    unsupported()
}
