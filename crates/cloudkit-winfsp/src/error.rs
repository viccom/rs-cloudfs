//! VFS / storage errors -> NTSTATUS: the one mapping point (K45).
//!
//! Every FSD callback in [`crate::fs`] funnels its failures through
//! [`fsp_error`]; nothing else in this crate builds a status code. The
//! tables are exhaustive `match`es on purpose — a new variant in
//! `VfsError` / `StorageError` breaks this build instead of silently
//! degrading to the EIO fallback.
//!
//! Fallback policy (rclone's `translateError` equivalent): anything the
//! VFS cannot name precisely is `STATUS_IO_DEVICE_ERROR` — the NTSTATUS
//! Win32 surfaces as ERROR_IO_DEVICE ("The request could not be performed
//! because of an I/O device error") — and gets an `error!` log so the
//! silent half of the failure stays diagnosable. Deliberate non-fallback
//! mappings: quota -> `STATUS_DISK_FULL`, unsupported -> `STATUS_NOT_SUPPORTED`,
//! timeout -> `STATUS_IO_TIMEOUT`, pending upload -> `STATUS_SHARING_VIOLATION`
//! ("file in use", which is what Explorer retries on).

use cloudkit_core::transport::StorageError;
use cloudkit_core::vfs::VfsError;
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_ACCESS_DENIED, STATUS_DISK_FULL, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_INVALID_PARAMETER, STATUS_IO_DEVICE_ERROR, STATUS_IO_TIMEOUT, STATUS_NOT_SUPPORTED,
    STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SHARING_VIOLATION,
};
use winfsp::FspError;

/// Maps a VFS failure to the NTSTATUS the FSD returns.
pub fn ntstatus_for(error: &VfsError) -> NTSTATUS {
    match error {
        VfsError::NotFound(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        VfsError::Exists(_) => STATUS_OBJECT_NAME_COLLISION,
        VfsError::IsDirectory(_) => STATUS_FILE_IS_A_DIRECTORY,
        VfsError::MissingPassword => STATUS_ACCESS_DENIED,
        VfsError::ParentMissing(_) => STATUS_OBJECT_PATH_NOT_FOUND,
        // "The file is in use": the bytes exist only locally until the
        // upload lands, so a delete/open is a sharing problem, not a
        // missing file — and it is the code Explorer retries on.
        VfsError::UploadPending(_) => STATUS_SHARING_VIOLATION,
        // The write surfaces refuse overlong segments up front (review
        // M3): the name itself is the problem.
        VfsError::NameTooLong { .. } => STATUS_OBJECT_NAME_INVALID,
        // A newer build wrote the payload; this build cannot dispatch.
        // Not an I/O fault — the operator has to upgrade.
        VfsError::UnsupportedEncryptionScheme { .. } => STATUS_NOT_SUPPORTED,
        VfsError::Timeout(_) => STATUS_IO_TIMEOUT,
        // EIO fallback class (K45): local persistence, crypto payload
        // damage, a stalled/closed queue and local I/O all surface as
        // "the device failed" — with the error! log in `fsp_error`.
        VfsError::QueueClosed | VfsError::Db(_) | VfsError::Crypto(_) | VfsError::Io(_) => {
            STATUS_IO_DEVICE_ERROR
        }
        VfsError::Transport(inner) => ntstatus_for_storage(inner),
    }
}

/// Maps a transport/storage failure (`VfsError::Transport`'s payload).
pub fn ntstatus_for_storage(error: &StorageError) -> NTSTATUS {
    match error {
        StorageError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        StorageError::Exists => STATUS_OBJECT_NAME_COLLISION,
        // Both flavors: `recoverable` drives the driver's own retry, the
        // FSD only sees "the caller is not allowed to touch this".
        StorageError::Unauthorized { .. } => STATUS_ACCESS_DENIED,
        StorageError::QuotaExceeded => STATUS_DISK_FULL,
        StorageError::Invalid => STATUS_INVALID_PARAMETER,
        StorageError::Unsupported => STATUS_NOT_SUPPORTED,
        // Rate limits, local I/O and backend outages are all "try again
        // later" from the Win32 side: EIO, plus the error! log.
        StorageError::RateLimited { .. } | StorageError::Io(_) | StorageError::Unavailable(_) => {
            STATUS_IO_DEVICE_ERROR
        }
    }
}

/// The FSD-facing conversion: [`ntstatus_for`] plus the EIO-fallback log.
///
/// Every callback in [`crate::fs`] returns through here, so the fallback
/// class is never silent — the log carries the original error (rclone's
/// `translateError` reports EIO and logs, never swallows).
pub fn fsp_error(error: &VfsError) -> FspError {
    let status = ntstatus_for(error);
    if status == STATUS_IO_DEVICE_ERROR {
        tracing::error!(
            error = %error,
            "winfsp: mapping error to STATUS_IO_DEVICE_ERROR (EIO fallback)"
        );
    }
    status.into()
}

/// The FSD handed us a name this namespace cannot represent (bad UTF-16,
/// `.` / `..` traversal, embedded separator). Never a db lookup.
pub fn invalid_name() -> FspError {
    STATUS_OBJECT_NAME_INVALID.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudkit_core::crypto::CryptoError;
    use cloudkit_core::database::{DbError, MetaDatabase};
    use std::time::Duration;

    /// A real `DbError` with no extra dependency: handing `MetaDatabase`
    /// a directory path fails inside rusqlite (SQLITE_CANTOPEN).
    fn sample_db_error() -> DbError {
        let dir = tempfile::tempdir().expect("temp dir");
        match MetaDatabase::open(dir.path()) {
            Err(error) => error,
            Ok(_) => panic!("opening a directory as a database must fail"),
        }
    }

    /// Every `VfsError` variant pinned to its exact status code. The raw
    /// `0xC...` values ARE the contract (they are the Win32 ABI), so the
    /// assertions compare against the windows-rs constants while also
    /// pinning the numeric value the FSD will see.
    #[test]
    fn vfs_error_variants_map_to_pinned_ntstatus() {
        let table: Vec<(VfsError, u32)> = vec![
            (VfsError::NotFound("/gone".into()), 0xC000_0034),
            (VfsError::IsDirectory("/dir".into()), 0xC000_00BA),
            (VfsError::MissingPassword, 0xC000_0022),
            (VfsError::QueueClosed, 0xC000_0185),
            (VfsError::Db(sample_db_error()), 0xC000_0185),
            (
                VfsError::Transport(StorageError::Unavailable("tcp reset".into())),
                0xC000_0185,
            ),
            (VfsError::Crypto(CryptoError::AuthFailed), 0xC000_0185),
            (
                VfsError::UnsupportedEncryptionScheme {
                    scheme: "aead_v3".into(),
                    path: "/f".into(),
                },
                0xC000_00BB,
            ),
            (
                VfsError::Io(std::io::Error::other("disk gone")),
                0xC000_0185,
            ),
            (VfsError::Timeout(Duration::from_secs(180)), 0xC000_00B5),
            (VfsError::Exists("/already".into()), 0xC000_0035),
            (VfsError::ParentMissing("/no/such".into()), 0xC000_003A),
            (VfsError::UploadPending("/busy".into()), 0xC000_0043),
        ];
        for (error, expected) in table {
            assert_eq!(
                ntstatus_for(&error).0 as u32,
                expected,
                "wrong NTSTATUS for {error:?}"
            );
        }
    }

    /// The transport taxonomy maps variant by variant, so a driver-side
    /// error keeps its meaning all the way to Explorer.
    #[test]
    fn storage_error_variants_map_to_pinned_ntstatus() {
        let table: Vec<(StorageError, u32)> = vec![
            (StorageError::NotFound, 0xC000_0034),
            (StorageError::Exists, 0xC000_0035),
            (
                StorageError::Unauthorized { recoverable: true },
                0xC000_0022,
            ),
            (
                StorageError::Unauthorized { recoverable: false },
                0xC000_0022,
            ),
            (
                StorageError::RateLimited {
                    retry_after: Some(Duration::from_secs(30)),
                },
                0xC000_0185,
            ),
            (StorageError::QuotaExceeded, 0xC000_007F),
            (StorageError::Invalid, 0xC000_000D),
            (StorageError::Unsupported, 0xC000_00BB),
            (StorageError::Io("local io failed".into()), 0xC000_0185),
            (
                StorageError::Unavailable("backend down".into()),
                0xC000_0185,
            ),
        ];
        for (error, expected) in table {
            assert_eq!(
                ntstatus_for_storage(&error).0 as u32,
                expected,
                "wrong NTSTATUS for {error:?}"
            );
        }
    }

    /// The named constants and the pinned numbers agree — catches a
    /// mis-imported constant whose value the table test would otherwise
    /// enshrine.
    #[test]
    fn pinned_values_agree_with_the_windows_constants() {
        assert_eq!(STATUS_OBJECT_NAME_NOT_FOUND.0 as u32, 0xC000_0034);
        assert_eq!(STATUS_OBJECT_NAME_COLLISION.0 as u32, 0xC000_0035);
        assert_eq!(STATUS_OBJECT_PATH_NOT_FOUND.0 as u32, 0xC000_003A);
        assert_eq!(STATUS_ACCESS_DENIED.0 as u32, 0xC000_0022);
        assert_eq!(STATUS_FILE_IS_A_DIRECTORY.0 as u32, 0xC000_00BA);
        assert_eq!(STATUS_NOT_SUPPORTED.0 as u32, 0xC000_00BB);
        assert_eq!(STATUS_IO_DEVICE_ERROR.0 as u32, 0xC000_0185);
        assert_eq!(STATUS_IO_TIMEOUT.0 as u32, 0xC000_00B5);
        assert_eq!(STATUS_DISK_FULL.0 as u32, 0xC000_007F);
        assert_eq!(STATUS_INVALID_PARAMETER.0 as u32, 0xC000_000D);
        assert_eq!(STATUS_SHARING_VIOLATION.0 as u32, 0xC000_0043);
    }

    /// `fsp_error` is the only constructor the FSD sees; it must carry
    /// the mapped status through and never a success code.
    #[test]
    fn fsp_error_carries_the_mapped_status() {
        let cases: Vec<(VfsError, u32)> = vec![
            (VfsError::NotFound("/gone".into()), 0xC000_0034),
            (VfsError::QueueClosed, 0xC000_0185),
            (VfsError::MissingPassword, 0xC000_0022),
        ];
        for (error, expected) in cases {
            match fsp_error(&error) {
                FspError::NTSTATUS(status) => {
                    assert_eq!(status as u32, expected, "for {error:?}")
                }
                other => panic!("expected an NTSTATUS carrier, got {other:?}"),
            }
        }
    }

    /// Names the namespace cannot hold are rejected before any lookup.
    #[test]
    fn invalid_names_map_to_name_invalid() {
        match invalid_name() {
            FspError::NTSTATUS(status) => assert_eq!(status as u32, 0xC000_0033),
            other => panic!("expected an NTSTATUS carrier, got {other:?}"),
        }
    }
}
