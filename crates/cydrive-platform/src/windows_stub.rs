//! Compile-only non-Windows stubs for the [`windows`] module surface —
//! same signatures, no behaviour. Keeps the workspace buildable on
//! Linux/macOS targets (repo rule; Python precedent `feaac0b`, where
//! `platform/windows.py` wraps its `winreg` import in `try/except`).
//!
//! [`windows`]: crate::windows

use crate::PlatformError;

/// The single message every stub returns.
const UNSUPPORTED: &str = "windows-only mount/registry";

/// Stub answer: [`PlatformError::Unsupported`].
fn unsupported<T>() -> Result<T, PlatformError> {
    Err(PlatformError::Unsupported(UNSUPPORTED))
}

/// No drives exist on this platform.
pub fn used_drive_letters() -> Vec<String> {
    Vec::new()
}

/// Stub: [`unsupported`].
pub fn ensure_webclient_service() -> Result<(), PlatformError> {
    unsupported()
}

/// Stub: [`unsupported`].
pub fn optimize_webdav_registry() -> Result<(), PlatformError> {
    unsupported()
}

/// Stub: [`unsupported`].
pub fn mount_drive(_letter: &str, _url: &str) -> Result<String, PlatformError> {
    unsupported()
}

/// Stub: [`unsupported`].
pub fn unmount_drive(_letter: &str) -> Result<(), PlatformError> {
    unsupported()
}
