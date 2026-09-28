//! Staged write state for one open file handle (WF3 / K41-K43).
//!
//! The write model is the WebDAV adapter's `StagedFile`
//! (`cloudkit-webdav` src/lib.rs:703-813) ported to WinFsp's callback
//! shape: bytes land in a `.{name}.tmp` sibling of the file's final
//! cache path (`Vfs::local_path`), and the ONLY commit point is the
//! FSD's cleanup/Release (K41) — [`StagedWriter::commit`] hands the
//! sibling to `Vfs::put_staged`, which renames it onto the cache path,
//! upserts the pending row and enqueues the upload. Flush never commits
//! (rclone's `read_write.go:189-198` rule): Windows, players and
//! scanners flush constantly.
//!
//! Differences from the WebDAV writer, all forced by the native API:
//!
//! - **Offset-addressed writes**: WinFsp hands `(offset, buffer)` per
//!   call, so there is no stream cursor — `write_at` positions every
//!   write itself. A write beyond the current end extends the staged
//!   file first (`set_len`), so the gap reads back as zeros, which is
//!   NTFS' own behavior for a sparse/hole write.
//! - **Owned behind a lock, not `&mut self`**: the FSD may drive one
//!   handle's callbacks from several dispatcher threads, so the writer
//!   lives in a `Mutex<Option<StagedWriter>>` on the handle (WF2's read
//!   state is an `Arc<AsyncMutex<_>>` only because the grace table
//!   shares it — a write state must NOT be shared or parked: a commit is
//!   destructive and happens exactly once).
//! - **Closing the handle is part of the commit**: Windows refuses some
//!   renames of open handles, so the staged file's handle is closed (and
//!   fsynced) before the rename inside `put_staged`.
//! - **The staging sibling carries a random segment** (review M1): the
//!   WebDAV spelling `.{name}.tmp` is a legal VIRTUAL path, so a remote
//!   holding an editor-temp-shaped name would make `/foo`'s staging file
//!   BE `/.foo.tmp`'s cache copy — staging truncated another pending
//!   row's only copy of the bytes. The random segment makes the sibling
//!   uncollidable; every cleanup path (commit, abort, Drop) removes the
//!   actual randomized name.
//! - **A failed commit keeps the bytes** (review H4): `put_staged`
//!   renates the sibling onto the final cache path BEFORE the row/enqueue
//!   tail, so on any failure that copy (or the pre-rename sibling) is the
//!   only copy of the bytes — nothing is removed, the landed row stays
//!   pending, and `Vfs::requeue_pending` revives it at the next boot.
//!
//! Pure local-file logic on purpose: nothing here calls into the WinFsp
//! DLL, so the whole write model is testable on a machine without WinFsp
//! installed (the same rule WF2's `reader.rs` follows).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use cloudkit_core::rel_path::RelPath;
use cloudkit_core::vfs::{Vfs, VfsError};

// The staged file is written with positioned writes (`write_at`, the
// Unix `pwrite` shape) and read back the same way, so neither direction
// touches the handle's own file pointer — the only cursor in this module
// is the one `set_len`/`metadata` need, and those live inside `std`.
#[cfg(windows)]
use std::os::windows::fs::FileExt as WindowsFileExt;

/// A fresh 8-hex-digit tag for a staging sibling: `RandomState`'s
/// per-instance OS seed runs through a fresh hasher — no extra
/// dependency, and two calls differ with overwhelming probability (32
/// bits against a collision that would need the same final path AND
/// overlapping writers).
fn random_tag() -> String {
    use std::hash::{BuildHasher, Hasher};
    let hasher = std::collections::hash_map::RandomState::new().build_hasher();
    format!("{:08x}", hasher.finish() as u32)
}

/// The cache-tree staging sibling for a final local path, with a random
/// segment (review M1): `dir/name.ext` ->
/// `dir/.name.ext.<rand8>.tmp`. The random segment is what keeps the
/// sibling from ever being another row's cache path — the fixed
/// `.{name}.tmp` spelling is a legal virtual path, so a remote could hold
/// a row whose cache copy sits exactly there (and staging truncated it).
///
/// The WebDAV writer converged on the same randomized spelling (review
/// M16, 2026-09-25); `cloudkit-core`'s own `tmp_sibling` (`vfs.rs:145`)
/// uses `name.ext.tmp` — a single-writer transient, still fixed-name.
pub fn staged_sibling(final_local: &Path) -> PathBuf {
    let name = final_local.file_name().map_or_else(
        || "cydrive".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    final_local.with_file_name(format!(".{name}.{}.tmp", random_tag()))
}

/// The staged bytes of one open file handle.
pub struct StagedWriter {
    /// The virtual path the commit will publish (`put_staged`).
    rel: RelPath,
    /// The staging sibling (`.{name}.{rand8}.tmp` next to the final cache
    /// path — review M1's random segment).
    staged: PathBuf,
    /// The open staging handle. `None` once the commit closed it (or the
    /// writer was aborted).
    file: Option<File>,
    /// The last write time the FSD asked for (`set_basic_info`), used as
    /// the committed row's mtime — the db's rows carry a single mtime and
    /// it is upload-owned otherwise.
    mtime: Option<f64>,
    /// The staging sibling is gone (committed or aborted): `Drop` must
    /// not try to remove it again.
    closed: bool,
}

impl std::fmt::Debug for StagedWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedWriter")
            .field("rel", &self.rel)
            .field("staged", &self.staged)
            .field("open", &self.file.is_some())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl StagedWriter {
    /// Starts an empty staging file for `rel` (the create / truncate
    /// arm): `final_local` is `Vfs::local_path(rel)`, whose parent
    /// directories are created here — `put_staged` would create them at
    /// commit time, but the staged file has to exist before the first
    /// write.
    pub fn create_empty(rel: RelPath, final_local: &Path) -> std::io::Result<Self> {
        Self::open_staged(rel, final_local, None)
    }

    /// Starts a staging file seeded with `source`'s bytes: the
    /// in-place-update arm (a handle opened for writing without an
    /// overwrite, so the file's existing content must survive the writes
    /// that follow). `source` is the plaintext cache path `Vfs::hydrate`
    /// answered with.
    pub fn create_seeded(rel: RelPath, final_local: &Path, source: &Path) -> std::io::Result<Self> {
        Self::open_staged(rel, final_local, Some(source))
    }

    /// Shared constructor: create the parent directories, seed the
    /// sibling (copy) when asked, then open it for writing.
    fn open_staged(rel: RelPath, final_local: &Path, seed: Option<&Path>) -> std::io::Result<Self> {
        let staged = staged_sibling(final_local);
        if let Some(parent) = staged.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(source) = seed {
            std::fs::copy(source, &staged)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(seed.is_none())
            .open(&staged)?;
        Ok(Self {
            rel,
            staged,
            file: Some(file),
            mtime: None,
            closed: false,
        })
    }

    /// The staging sibling's path (tests and diagnostics).
    pub fn staged_path(&self) -> &Path {
        &self.staged
    }

    /// The staged length in bytes — the file's EOF, which is also what
    /// the FSD reports back in every `FileInfo` after a write.
    pub fn len(&self) -> std::io::Result<u64> {
        Ok(self.handle()?.metadata()?.len())
    }

    /// Whether the staging file is empty (clippy's `len` companion).
    pub fn is_empty(&self) -> std::io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Records the mtime the FSD asked for (`set_basic_info`); it is used
    /// at commit time when present.
    pub fn set_mtime(&mut self, mtime: f64) {
        self.mtime = Some(mtime);
    }

    /// The recorded mtime, if the FSD ever set one.
    pub fn mtime(&self) -> Option<f64> {
        self.mtime
    }

    /// Writes `buf` at `offset`, extending the file first when the write
    /// reaches past EOF (the extension is zero-filled, so a hole reads
    /// back as zeros — Windows/NTFS sparse-write semantics).
    ///
    /// Returns the bytes actually written: `write_at` is a positioned
    /// write, so a partial write is possible in principle and the caller
    /// reports exactly what landed.
    pub fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            // A zero-byte write is a no-op — in particular it must not
            // extend the file to `offset`. The FSD sends one whenever a
            // `constrained_io` write is clamped away entirely.
            return Ok(0);
        }
        let end = offset.saturating_add(buf.len() as u64);
        let file = self.handle_mut()?;
        if file.metadata()?.len() < end {
            file.set_len(end)?;
        }
        file.seek_write(buf, offset)
    }

    /// Reads `[offset, offset + buf.len())` back out of the staging file
    /// (read-your-own-writes on a handle that has not committed yet).
    /// Returns a short read at EOF, exactly like the committed file will.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.len()?;
        if offset >= len {
            return Ok(0);
        }
        let limit = buf.len().min((len - offset) as usize);
        // A second handle on purpose: the staging handle is open for
        // writing and Windows shares it, and a fresh read handle cannot
        // disturb the positioned writes (`std`'s file pointer is not
        // shared between handles).
        let file = File::open(&self.staged)?;
        file.seek_read(&mut buf[..limit], offset)
    }

    /// Sets the staged length (`set_file_size`): truncates or extends
    /// (zero-filled) without touching the bytes below it.
    pub fn set_len(&mut self, size: u64) -> std::io::Result<()> {
        self.handle_mut()?.set_len(size)
    }

    /// The commit (K41/K43): fsync, close the staging handle, hand the
    /// sibling to `Vfs::put_staged` (atomic rename onto the cache path +
    /// pending row + enqueue). Called exactly once per writer because it
    /// consumes it; a failure surfaces the error for the caller to log
    /// (cleanup cannot report it to Windows) and KEEPS the bytes — review
    /// H4: `put_staged` renames the sibling onto the final cache path
    /// before the row/enqueue tail, so on any failure that copy (or, for
    /// a failure before the rename, the sibling itself) is the only copy
    /// of the bytes. Removing it would silently destroy the write; the
    /// landed row stays pending and `Vfs::requeue_pending` revives it at
    /// the next boot.
    pub async fn commit(mut self, vfs: &Vfs, mtime: f64) -> Result<(), VfsError> {
        if let Some(file) = self.file.take() {
            if let Err(error) = file.sync_all() {
                // The bytes never left the staging sibling — that sibling
                // is the only copy, so it stays (review H4). `closed`
                // stops Drop from removing it.
                self.closed = true;
                return Err(VfsError::Io(error));
            }
        }
        let result = vfs.put_staged(&self.rel, &self.staged, mtime).await;
        // Either way nothing is removed here: the sibling is gone (the
        // rename consumed it) or it holds the only copy of the bytes.
        self.closed = true;
        result
    }

    /// Discards the staged bytes (delete-on-close, or the create arm of a
    /// failed operation): close the handle, remove the sibling.
    pub fn abort(mut self) {
        self.file.take();
        self.closed = true;
        let _ = std::fs::remove_file(&self.staged);
    }

    /// The open staging handle, or an error once the writer was closed.
    fn handle(&self) -> std::io::Result<&File> {
        self.file
            .as_ref()
            .ok_or_else(|| std::io::Error::other("staged writer is closed"))
    }

    fn handle_mut(&mut self) -> std::io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("staged writer is closed"))
    }
}

impl Drop for StagedWriter {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        // A handle that dies without a commit must not leave the staging
        // sibling in the cache tree. Close the handle first (Windows
        // refuses deleting a file with an open handle), then remove.
        self.file.take();
        self.closed = true;
        let _ = std::fs::remove_file(&self.staged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic payload bytes (`i % 251`).
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn rel(name: &str) -> RelPath {
        RelPath::new(name).expect("valid rel path")
    }

    /// The staging sibling hides under a randomized hidden name
    /// (`.{name}.{rand8}.tmp`): hidden, `.tmp`-suffixed, carrying the
    /// final name and a fresh random segment per call, in the same
    /// directory (review M1).
    #[test]
    fn staged_sibling_hides_under_a_randomized_name() {
        let final_local = Path::new("C:\\cache\\docs\\report.bin");
        let first = staged_sibling(final_local);
        let first_name = first
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(
            first_name.starts_with(".report.bin."),
            "hidden name: {first_name}"
        );
        assert!(first_name.ends_with(".tmp"), "tmp-suffixed: {first_name}");
        assert_eq!(first.parent(), final_local.parent(), "same directory");

        // The random segment is 8 hex digits between the final name and
        // the suffix, and it differs call to call.
        let segment = first_name
            .strip_prefix(".report.bin.")
            .expect("prefix")
            .strip_suffix(".tmp")
            .expect("suffix");
        assert_eq!(segment.len(), 8, "rand8 segment: {first_name}");
        assert!(
            segment.chars().all(|c| c.is_ascii_hexdigit()),
            "hex segment: {first_name}"
        );
        assert_ne!(
            first,
            staged_sibling(final_local),
            "two staging siblings of the same path must not share a name"
        );
    }

    /// M1 at the writer level: staging `/foo` must never touch a file
    /// spelled `.{name}.tmp` next to it — the old fixed sibling spelling
    /// is a legal virtual path whose cache copy could sit exactly there.
    #[test]
    fn staging_foo_does_not_touch_a_dot_foo_dot_tmp_neighbor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("foo");
        let neighbor = dir.path().join(".foo.tmp");
        std::fs::write(&neighbor, b"SECRET").expect("seed the neighbor copy");

        let mut writer =
            StagedWriter::create_empty(rel("/foo"), &final_local).expect("create staged");
        assert_ne!(
            writer.staged_path().file_name(),
            neighbor.file_name(),
            "the randomized sibling must differ from the old fixed spelling"
        );
        writer.write_at(0, b"new-bytes").expect("write");
        assert_eq!(writer.len().expect("len"), 9);

        assert_eq!(
            std::fs::read(&neighbor).expect("the neighbor copy"),
            b"SECRET",
            "staging /foo must not truncate or rewrite .foo.tmp"
        );
        writer.abort();
        assert_eq!(
            std::fs::read(&neighbor).expect("the neighbor copy"),
            b"SECRET",
            "aborting /foo's staging must not remove .foo.tmp either"
        );
    }

    /// A positioned write below EOF lands byte-exact and leaves the rest
    /// of the file alone.
    #[test]
    fn write_at_overwrites_in_place() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("file.bin");
        let mut writer =
            StagedWriter::create_empty(rel("/file.bin"), &final_local).expect("create staged");

        assert_eq!(writer.len().expect("len"), 0);
        assert_eq!(writer.write_at(0, b"ABCDEFGH").expect("write 1"), 8);
        assert_eq!(writer.write_at(2, b"xy").expect("write 2"), 2);
        assert_eq!(writer.len().expect("len"), 8);

        let mut out = [0u8; 8];
        assert_eq!(writer.read_at(0, &mut out).expect("read back"), 8);
        assert_eq!(&out, b"ABxyEFGH");
    }

    /// A write past EOF extends the file first: the gap reads back as
    /// zeros (the sparse-write shape Windows consumers expect).
    #[test]
    fn write_at_past_eof_zero_fills_the_gap() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("sparse.bin");
        let mut writer =
            StagedWriter::create_empty(rel("/sparse.bin"), &final_local).expect("create staged");

        assert_eq!(writer.write_at(8, b"tail").expect("write"), 4);
        assert_eq!(writer.len().expect("len"), 12);

        let mut out = vec![0u8; 12];
        assert_eq!(writer.read_at(0, &mut out).expect("read back"), 12);
        assert_eq!(&out[..8], &[0u8; 8], "the hole is zero-filled");
        assert_eq!(&out[8..], b"tail");
    }

    /// The seeded arm starts from the source's exact bytes (in-place
    /// update of an existing file) and writes on top of them.
    #[test]
    fn seeded_writer_starts_from_the_existing_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("cached.bin");
        let bytes = pattern(32);
        std::fs::write(&source, &bytes).expect("seed source");
        let final_local = dir.path().join("cached.bin");

        let mut writer = StagedWriter::create_seeded(rel("/cached.bin"), &final_local, &source)
            .expect("create staged");
        assert_eq!(writer.len().expect("len"), 32);

        assert_eq!(writer.write_at(4, b"XXXX").expect("write"), 4);
        let mut out = vec![0u8; 32];
        assert_eq!(writer.read_at(0, &mut out).expect("read back"), 32);
        assert_eq!(&out[..4], &bytes[..4]);
        assert_eq!(&out[4..8], b"XXXX");
        assert_eq!(&out[8..], &bytes[8..]);
    }

    /// `set_len` truncates and extends; the extension is zeros.
    #[test]
    fn set_len_truncates_and_extends() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("size.bin");
        let mut writer =
            StagedWriter::create_empty(rel("/size.bin"), &final_local).expect("create staged");

        writer.write_at(0, b"0123456789").expect("write");
        writer.set_len(4).expect("truncate");
        assert_eq!(writer.len().expect("len"), 4);
        let mut out = [0u8; 8];
        assert_eq!(writer.read_at(0, &mut out).expect("read"), 4);
        assert_eq!(&out[..4], b"0123");

        writer.set_len(8).expect("extend");
        assert_eq!(writer.len().expect("len"), 8);
        let mut grown = [9u8; 8];
        assert_eq!(writer.read_at(0, &mut grown).expect("read"), 8);
        assert_eq!(&grown[..4], b"0123");
        assert_eq!(&grown[4..], &[0u8; 4], "the extension is zero-filled");
    }

    /// Reads stop at the staged EOF (short read) instead of reporting
    /// bytes that are not there.
    #[test]
    fn read_at_stops_at_the_staged_eof() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("eof.bin");
        let mut writer =
            StagedWriter::create_empty(rel("/eof.bin"), &final_local).expect("create staged");
        writer.write_at(0, b"abcd").expect("write");

        let mut buf = [0u8; 8];
        assert_eq!(writer.read_at(2, &mut buf).expect("read"), 2);
        assert_eq!(&buf[..2], b"cd");
        assert_eq!(writer.read_at(4, &mut buf).expect("read at EOF"), 0);
        assert_eq!(writer.read_at(99, &mut buf).expect("read past EOF"), 0);
    }

    /// `abort` removes the sibling (delete-on-close's discard).
    #[test]
    fn abort_removes_the_staging_sibling() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("aborted.bin");
        let mut writer =
            StagedWriter::create_empty(rel("/aborted.bin"), &final_local).expect("create staged");
        let staged = writer.staged_path().to_path_buf();
        writer.write_at(0, b"discard me").expect("write");
        assert!(staged.exists());

        writer.abort();
        assert!(!staged.exists(), "abort must remove the staging sibling");
        assert!(
            !final_local.exists(),
            "abort must never publish the staged bytes"
        );
    }

    /// A zero-byte write is a no-op and must not extend the file: the
    /// FSD sends one when a constrained write is clamped away entirely.
    #[test]
    fn zero_byte_write_does_not_extend_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("empty-write.bin");
        let mut writer = StagedWriter::create_empty(rel("/empty-write.bin"), &final_local)
            .expect("create staged");
        writer.write_at(0, b"1234").expect("write");

        assert_eq!(writer.write_at(9, b"").expect("empty write"), 0);
        assert_eq!(writer.len().expect("len"), 4, "EOF is untouched");
        let mut out = [0u8; 4];
        assert_eq!(writer.read_at(0, &mut out).expect("read"), 4);
        assert_eq!(&out, b"1234");
    }

    /// A writer dropped without a commit does not leak the sibling
    /// (a handle can die between the last write and cleanup) — and the
    /// sibling removed is the actual randomized name.
    #[test]
    fn drop_without_commit_removes_the_staging_sibling() {
        let dir = tempfile::tempdir().expect("temp dir");
        let final_local = dir.path().join("dropped.bin");
        {
            let mut writer = StagedWriter::create_empty(rel("/dropped.bin"), &final_local)
                .expect("create staged");
            writer.write_at(0, b"never committed").expect("write");
            assert!(writer.staged_path().exists());
            let staged = writer.staged_path().to_path_buf();
            drop(writer);
            assert!(!staged.exists(), "drop must remove the staging sibling");
        }
        assert_eq!(
            std::fs::read_dir(dir.path())
                .expect("read dir")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            Vec::<String>::new(),
            "the cache directory holds no leftovers"
        );
    }

    /// H4 at the writer level: a commit whose enqueue fails keeps the
    /// bytes wherever the rename left them. With the queue closed,
    /// `put_staged` renames the sibling onto the final path and fails the
    /// enqueue — the bytes stay at the FINAL path and nothing is removed
    /// (review H4: the old "the write was discarded" semantics were false).
    #[tokio::test]
    async fn failed_commit_keeps_the_migrated_bytes() {
        use std::sync::Arc;
        use std::time::Duration;

        use cloudkit_core::cache::CacheManager;
        use cloudkit_core::database::MetaDatabase;
        use cloudkit_core::transport::mock::MockTransport;
        use cloudkit_core::transport::{Capabilities, CloudTransport};
        use cloudkit_core::upload_queue::RetryPolicy;
        use cloudkit_core::vfs::VfsConfig;

        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("db"));
        let cache = CacheManager::new(dir.path().join("cache"), 1 << 30);
        let mock = Arc::new(
            MockTransport::builder()
                .capabilities(Capabilities {
                    range_read: true,
                    inbound: true,
                    chat: true,
                    ..Capabilities::none()
                })
                .build(),
        );
        let transport: Arc<dyn CloudTransport> = mock.clone();
        let vfs = Vfs::new(
            db,
            cache,
            transport,
            VfsConfig {
                chunk_size_bytes: 64,
                workers: 1,
                queue_capacity: 16,
                retry: RetryPolicy {
                    initial_backoff: Duration::from_millis(1),
                    max_backoff: Duration::from_millis(2),
                    max_attempts: 3,
                },
                encryption_password: None,
                encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
                hydrate_timeout: Duration::from_secs(180),
            },
        );
        vfs.shutdown().await; // the enqueue will fail

        let final_local = dir.path().join("cache").join("stuck.bin");
        let mut writer =
            StagedWriter::create_empty(rel("/stuck.bin"), &final_local).expect("create staged");
        writer.write_at(0, b"payload-bytes").expect("stage bytes");
        let staged = writer.staged_path().to_path_buf();

        let result = writer.commit(&vfs, 0.0).await;
        assert!(result.is_err(), "the closed queue must fail the commit");

        // The bytes survived the rename onto the final path and stayed.
        assert_eq!(
            std::fs::read(&final_local).expect("the kept bytes"),
            b"payload-bytes",
            "H4 FIXED: the migrated bytes must stay at the final cache path"
        );
        assert!(
            !staged.exists(),
            "the sibling was consumed by the rename, not by a removal"
        );
    }
}
