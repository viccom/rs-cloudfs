//! Integration tests for the WF3 write path and filesystem operations.
//!
//! Contract under test (plan §3-WF3 + K41/K43/K44/K45):
//! - `create` follows the K43 disposition→intent matrix: a directory
//!   request becomes a `files` row (`Vfs::create_dir`), a file request
//!   stages its bytes in a randomized hidden sibling
//!   (`.{name}.{rand8}.tmp`, review M1) of the final cache path, and an
//!   existing name is a collision;
//! - the staged bytes are committed **exactly once**, by `cleanup` (K41:
//!   `flush` has zero side effects) — `Vfs::put_staged` then publishes
//!   the atomic rename, the pending row and the queue entry, so the
//!   upload path is the same one every other surface uses;
//! - writes are offset-addressed and out-of-order writes survive
//!   byte-exact, holes included (a write past EOF zero-fills the gap);
//! - `overwrite` (the FSD's separate Overwrite transaction after an
//!   `FILE_OVERWRITTEN` create/open, `src/sys/create.c:1172-1228`)
//!   replaces the staged content; `set_file_size` resizes it and
//!   `set_basic_info`'s last-write time becomes the committed mtime;
//! - `rename` / `set_delete` / `cleanup(FspCleanupDelete)` mirror the
//!   WebDAV adapter's semantics (destination overwrite, directories
//!   refused, row + cache copy moved/deleted, the pending-upload guard
//!   keeps its `UploadPending` → `STATUS_SHARING_VIOLATION` mapping) and
//!   the volume label is process-level (K44).
//!
//! The harness deliberately leaves the mock transport **disconnected**:
//! uploads then cannot land, so "the row is pending, the local copy is
//! the only copy, the queue saw exactly one job" — the states this batch
//! commits to — are observable without racing the worker. The uploaded
//! arm is seeded directly where a test needs it.
//!
//! Everything here runs WITHOUT WinFsp installed: the DLL is only ever
//! reached through `DirBuffer` and the mount host, neither of which these
//! tests touch.

#![cfg(all(windows, feature = "winfsp"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{Capabilities, CloudTransport};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_winfsp::fs::{create_intent, CloudFs, CreateKind, Disposition, Handle};
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{FileInfo, FileSystemContext, OpenFileInfo, VolumeInfo};
use winfsp::{FspError, U16CStr};

/// `FspCleanupDelete` as the FSD passes it (winfsp.h:151).
const CLEANUP_DELETE: u32 = FspCleanupFlags::FspCleanupDelete as u32;

/// 2023-11-14T22:13:20Z as FILETIME — the mtime `set_basic_info` sets
/// (Unix 1_700_000_000 * 1e7 + the 1601→1970 offset).
const T_1_700_000_000_FILETIME: u64 = 133_444_736_000_000_000;

/// VfsConfig for these tests: tiny chunks, one worker, fast retry (the
/// same shape every adapter test uses).
fn test_cfg() -> VfsConfig {
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
    }
}

/// Range-capable capabilities (the mock's default shape) so a seeded
/// uploaded file opens on the streaming arm and needs no remote traffic.
fn range_caps() -> Capabilities {
    Capabilities {
        range_read: true,
        inbound: true,
        chat: true,
        ..Capabilities::none()
    }
}

/// Real temp environment: SQLite db + cache tree + **unconnected** mock
/// transport + Vfs + the adapter under test (L5 test code may depend on
/// anything but drivers — R1).
struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    mock: Arc<MockTransport>,
    vfs: Arc<Vfs>,
    fs: CloudFs,
    /// Kept alive (never read beyond construction): the bridge and the
    /// upload queue borrow this runtime, and dropping it would tear the
    /// worker threads out from under the adapter.
    _rt: tokio::runtime::Runtime,
}

impl Harness {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let dir = tempfile::tempdir().expect("temp dir");
        let cache_root = dir.path().join("cache");
        let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
        let cache = cloudkit_core::cache::CacheManager::new(cache_root, 1 << 30);
        // No `connect()`: every upload attempt fails, so a committed row
        // stays pending with its local copy intact — the deterministic
        // state these tests assert on.
        let mock = Arc::new(MockTransport::builder().capabilities(range_caps()).build());
        let transport: Arc<dyn CloudTransport> = mock.clone();
        let _guard = rt.enter();
        let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg()));
        Self {
            _dir: dir,
            db,
            mock,
            fs: CloudFs::new(vfs.clone(), rt.handle().clone(), "cydrive-test"),
            vfs,
            _rt: rt,
        }
    }

    /// The FSD `create` callback.
    fn create(
        &self,
        name: &str,
        create_options: u32,
        file_attributes: u32,
    ) -> winfsp::Result<Handle> {
        let wide = fsd_path(name);
        let mut info: OpenFileInfo = unsafe { std::mem::zeroed() };
        self.fs.create(
            U16CStr::from_slice(&wide).expect("name"),
            create_options,
            0x0012_0089, // GENERIC_READ|WRITE|DELETE-ish; the adapter ignores it
            file_attributes,
            None,
            0,
            None,
            false,
            &mut info,
        )
    }

    /// A file create with the Win32 `CREATE_NEW` shape (FILE_CREATE +
    /// FILE_NON_DIRECTORY_FILE).
    fn create_file(&self, name: &str) -> winfsp::Result<Handle> {
        self.create(name, (Disposition::Create as u32) << 24 | 0x40, 0)
    }

    /// A directory create (FILE_CREATE + FILE_DIRECTORY_FILE +
    /// FILE_ATTRIBUTE_DIRECTORY, the shape the FSD's kernel side forms —
    /// `src/sys/create.c:576-579`).
    fn create_dir_entry(&self, name: &str) -> winfsp::Result<Handle> {
        self.create(name, (Disposition::Create as u32) << 24 | 0x01, 0x0010)
    }

    /// The FSD `open` callback, with the create/open response going to a
    /// scratch `OpenFileInfo`.
    fn open(&self, name: &str) -> winfsp::Result<Handle> {
        let wide = fsd_path(name);
        let mut info: OpenFileInfo = unsafe { std::mem::zeroed() };
        self.fs
            .open(U16CStr::from_slice(&wide).expect("name"), 0, 0, &mut info)
    }

    /// The FSD `write` callback with a scratch `FileInfo`.
    fn write(
        &self,
        handle: &Handle,
        offset: u64,
        bytes: &[u8],
        write_to_eof: bool,
        constrained_io: bool,
    ) -> winfsp::Result<u32> {
        let mut info = FileInfo::default();
        self.fs.write(
            handle,
            bytes,
            offset,
            write_to_eof,
            constrained_io,
            &mut info,
        )
    }

    /// The FSD `write` callback, also returning the `FileInfo` the FSD
    /// feeds back into the file node.
    fn write_with_info(
        &self,
        handle: &Handle,
        offset: u64,
        bytes: &[u8],
    ) -> winfsp::Result<FileInfo> {
        let mut info = FileInfo::default();
        self.fs
            .write(handle, bytes, offset, false, false, &mut info)?;
        Ok(info)
    }

    /// The FSD `cleanup` callback (no delete flag).
    fn cleanup(&self, handle: &Handle) {
        self.fs.cleanup(handle, None, 0);
    }

    /// The FSD `cleanup` callback with `FspCleanupDelete`.
    fn cleanup_delete(&self, handle: &Handle, name: &str) {
        let wide = fsd_path(name);
        self.fs.cleanup(
            handle,
            Some(U16CStr::from_slice(&wide).expect("name")),
            CLEANUP_DELETE,
        );
    }

    /// The FSD `read` callback.
    fn read(&self, handle: &Handle, offset: u64, len: usize) -> winfsp::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let filled = self.fs.read(handle, &mut buf, offset)? as usize;
        assert!(filled <= buf.len(), "read reported more than the buffer");
        buf.truncate(filled);
        Ok(buf)
    }

    /// The FSD `rename` callback.
    fn rename(
        &self,
        handle: &Handle,
        from: &str,
        to: &str,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let from_wide = fsd_path(from);
        let to_wide = fsd_path(to);
        self.fs.rename(
            handle,
            U16CStr::from_slice(&from_wide).expect("from"),
            U16CStr::from_slice(&to_wide).expect("to"),
            replace_if_exists,
        )
    }

    /// The FSD `set_delete` callback.
    fn set_delete(&self, handle: &Handle, name: &str, delete: bool) -> winfsp::Result<()> {
        let wide = fsd_path(name);
        self.fs
            .set_delete(handle, U16CStr::from_slice(&wide).expect("name"), delete)
    }

    /// Runs one full create → write → cleanup cycle for `rel` and returns
    /// the committed bytes (used to build the pending-upload states).
    fn commit_new_file(&self, rel: &str, bytes: &[u8]) -> Handle {
        let handle = self.create_file(rel).expect("create");
        self.write(&handle, 0, bytes, false, false).expect("write");
        self.cleanup(&handle);
        handle
    }

    /// Seeds one `files` row (the uploaded shape: an existing file the
    /// write path may open, overwrite or delete).
    fn seed_row(&self, rel: &str, size: i64, is_uploaded: bool, msg_id: Option<i64>) {
        let rel_path = RelPath::new(rel).expect("valid rel path");
        self.db
            .upsert_file(&FileUpsert {
                rel_path: rel_path.as_str().to_string(),
                name: rel_path.name().to_string(),
                parent_dir: rel_path
                    .parent()
                    .map(|parent| parent.as_str().to_string())
                    .unwrap_or_else(|| "/".to_string()),
                size,
                mtime: 1_700_000_000.0,
                sha256: None,
                is_dir: false,
                telegram_msg_id: msg_id,
                is_uploaded,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 1,
                mime_type: None,
            })
            .expect("seed files row");
    }

    /// Writes plaintext bytes straight into the mirrored cache tree (the
    /// uploaded-with-a-local-copy precondition).
    fn seed_cache_copy(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let rel_path = RelPath::new(rel).expect("valid rel path");
        let local = self.vfs.local_path(&rel_path);
        fs::create_dir_all(local.parent().expect("cache parent dir")).expect("create cache dirs");
        fs::write(&local, bytes).expect("write cache copy");
        local
    }

    /// An uploaded file with a local copy: row + cache bytes.
    fn seed_uploaded_file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        self.seed_row(rel, bytes.len() as i64, true, Some(1));
        self.seed_cache_copy(rel, bytes)
    }

    /// The row for `rel`, if any.
    fn row(&self, rel: &str) -> Option<cloudkit_core::database::FileRecord> {
        self.db.get_file(rel).expect("db read")
    }

    /// The local cache path of `rel`.
    fn local_path(&self, rel: &str) -> PathBuf {
        self.vfs
            .local_path(&RelPath::new(rel).expect("valid rel path"))
    }

    /// Every staging sibling currently under the cache root (recursive).
    /// The siblings carry a random segment (review M1), so tests observe
    /// them by shape — hidden `.name.<rand>.tmp` files — instead of
    /// predicting the name.
    fn stage_siblings(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        collect_stage_siblings(&self.local_path("/"), &mut found);
        found
    }

    /// Jobs the VFS accepted into the upload queue.
    fn enqueued(&self) -> u64 {
        self.vfs.queue_stats().enqueued
    }

    /// Bounded wait until the queue has attempted at least `expected`
    /// uploads (the worker runs on the injected runtime, so "the job was
    /// picked up" is observable, not instant).
    fn wait_for_upload_attempts(&self, expected: usize) {
        for _ in 0..2000 {
            if self.mock.upload_calls().len() >= expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!(
            "the queue never attempted {expected} upload(s); saw {}",
            self.mock.upload_calls().len()
        );
    }
}

/// FSD path string -> the wide NUL-terminated form the callbacks receive.
///
/// The tests speak the virtual `/`-separated paths and this converts them
/// to the FSD's own spelling (`\dir\file`), which is what
/// `open`/`create`/`rename`/`set_delete` actually hand in.
fn fsd_path(name: &str) -> Vec<u16> {
    name.replace('/', "\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// Recursively collects the hidden `.tmp` staging siblings under `dir` —
/// the shape `StagedWriter`'s randomized siblings always have.
fn collect_stage_siblings(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_stage_siblings(&path, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') && name.ends_with(".tmp") {
                out.push(path);
            }
        }
    }
}

/// The NTSTATUS a failing callback answered with (the tests pin raw
/// values — they are the Win32 ABI).
fn status_of<T>(result: winfsp::Result<T>) -> u32 {
    match result {
        Ok(_) => panic!("expected an error status, got success"),
        Err(FspError::NTSTATUS(status)) => status as u32,
        Err(other) => panic!("expected an NTSTATUS carrier, got {other:?}"),
    }
}

// ------------------------------------------------------- disposition ---

/// The K43 matrix, grid by grid: every disposition byte the FSD can
/// dispatch (`fsop.c:918-934`) with the three decisions it drives, plus
/// the Win32 names each one stands for.
#[test]
fn disposition_intent_matrix_is_pinned_cell_by_cell() {
    // (high byte, expected intent) — the disposition constants are the NT
    // ABI values (windows::Win32::Wdk::Storage::FileSystem).
    let table: &[(u32, Disposition, bool, bool, bool, bool)] = &[
        // Supersede (FILE_SUPERSEDE, 0): create-or-replace. Only the
        // FILE_OVERWRITE_IF/SUPERSEDE pair reports itself as supersede.
        (0, Disposition::Supersede, false, true, false, true),
        // Open (FILE_OPEN, 1) = Win32 OPEN_EXISTING / TRUNCATE_EXISTING.
        (1, Disposition::Open, false, false, true, false),
        // Create (FILE_CREATE, 2) = Win32 CREATE_NEW.
        (2, Disposition::Create, true, false, false, false),
        // OpenIf (FILE_OPEN_IF, 3) = Win32 OPEN_ALWAYS.
        (3, Disposition::OpenIf, false, true, false, false),
        // Overwrite (FILE_OVERWRITE, 4).
        (4, Disposition::Overwrite, false, false, true, true),
        // OverwriteIf (FILE_OVERWRITE_IF, 5) = Win32 CREATE_ALWAYS.
        (5, Disposition::OverwriteIf, false, true, false, true),
    ];
    for &(byte, disposition, create_new, create_if_missing, must_exist, truncate) in table {
        assert_eq!(
            disposition as u32, byte,
            "the enum discriminant IS the ABI disposition byte"
        );
        let intent = create_intent(byte << 24, 0).expect("known disposition");
        assert_eq!(intent.disposition, disposition, "disposition byte {byte}");
        assert_eq!(intent.kind, CreateKind::File, "disposition byte {byte}");
        assert_eq!(intent.create_new, create_new, "create_new for {byte}");
        assert_eq!(
            intent.create_if_missing, create_if_missing,
            "create_if_missing for {byte}"
        );
        assert_eq!(intent.must_exist, must_exist, "must_exist for {byte}");
        assert_eq!(intent.truncate, truncate, "truncate for {byte}");
    }
}

/// The two raw `create_options` values the spike observed on a directory
/// open: both disposition `FILE_OPEN`, one carrying `FILE_DIRECTORY_FILE`
/// and one not. The matrix reports what the bits say (that one is a
/// directory request, the other is not) — which is exactly why the open
/// path resolves the kind from the row and never from these bits.
#[test]
fn open_option_bits_are_recorded_not_trusted() {
    let with_dir_bit = create_intent(0x0100_4021, 0).expect("FILE_OPEN");
    assert_eq!(with_dir_bit.disposition, Disposition::Open);
    assert_eq!(with_dir_bit.kind, CreateKind::Directory);

    let without_dir_bit = create_intent(0x0120_4000, 0).expect("FILE_OPEN");
    assert_eq!(without_dir_bit.disposition, Disposition::Open);
    assert_eq!(without_dir_bit.kind, CreateKind::File);
}

/// Directory requests are recognised by either bit (the FSD sets the
/// attribute from the option, `src/sys/create.c:576-579`), and the
/// kernel's own contradiction check is mirrored.
#[test]
fn directory_intent_uses_both_bits_and_rejects_contradictions() {
    let by_option = create_intent((Disposition::OpenIf as u32) << 24 | 0x01, 0).expect("intent");
    assert_eq!(by_option.kind, CreateKind::Directory);
    let by_attribute = create_intent((Disposition::OpenIf as u32) << 24, 0x10).expect("intent");
    assert_eq!(by_attribute.kind, CreateKind::Directory);

    // FILE_DIRECTORY_FILE | FILE_NON_DIRECTORY_FILE is invalid
    // (src/sys/create.c:393).
    let both = create_intent((Disposition::Create as u32) << 24 | 0x41, 0);
    assert_eq!(status_of(both), 0xC000_000D, "STATUS_INVALID_PARAMETER");
}

/// A disposition byte outside the six the FSD dispatches on is invalid
/// (`fsop.c:933`) — never silently treated as create or open.
#[test]
fn unknown_disposition_is_invalid() {
    for byte in [6u32, 7, 0xff] {
        let intent = create_intent(byte << 24, 0);
        assert_eq!(
            status_of(intent),
            0xC000_000D,
            "disposition byte {byte} must be invalid"
        );
    }
}

// ---------------------------------------------------------- lifecycle ---

/// The full cycle: create → out-of-order writes → cleanup (commit) →
/// the queue holds exactly one job, the row is pending at the written
/// size, the cache copy is byte-exact, a reopen reads it back, and a
/// second cleanup changes nothing (the commit happens exactly once).
#[test]
fn create_write_cleanup_commits_once_and_reopens_byte_exact() {
    let h = Harness::new();
    let content = b"ABCDEFGHIJKLMNOP".to_vec();

    let handle = h.create_file("/new.bin").expect("create");
    assert!(handle.has_pending_write(), "create stages an empty writer");
    assert_eq!(
        h.stage_siblings().len(),
        1,
        "the staging sibling exists (under its random name)"
    );
    assert!(h.row("/new.bin").is_none(), "nothing is published yet");

    // ④ Out-of-order writes: the middle, then the head, then the seam.
    assert_eq!(
        h.write(&handle, 8, &content[8..], false, false).expect("w"),
        8
    );
    assert_eq!(
        h.write(&handle, 0, &content[0..4], false, false)
            .expect("w"),
        4
    );
    // The FSD feeds this FileInfo back into the file node after every
    // write: it must carry the live staged length.
    let info = h.write_with_info(&handle, 4, &content[4..8]).expect("w");
    assert_eq!(info.file_size, 16, "the write response reports the size");

    h.cleanup(&handle);
    assert!(!handle.has_pending_write(), "cleanup consumed the writer");

    // ① exactly one queued job (and it was actually attempted).
    assert_eq!(h.enqueued(), 1, "one upload was enqueued");
    h.wait_for_upload_attempts(1);

    // ② a pending row at the written size.
    let row = h.row("/new.bin").expect("row exists after the commit");
    assert!(!row.is_uploaded, "the row is pending");
    assert_eq!(row.size, content.len() as i64, "the row carries the size");

    // ③ the local copy is byte-exact, and the staging sibling is gone.
    let cached = fs::read(h.local_path("/new.bin")).expect("cache copy");
    assert_eq!(cached, content, "the committed bytes are byte-exact");
    assert!(h.stage_siblings().is_empty(), "no staging sibling left");

    // ④ a reopen reads the committed bytes back.
    let reopened = h.open("/new.bin").expect("reopen");
    assert_eq!(h.read(&reopened, 0, 16).expect("read"), content);
    assert_eq!(
        h.read(&reopened, 8, 8).expect("read tail"),
        content[8..].to_vec()
    );
    h.fs.close(reopened);

    // ⑤ cleanup is idempotent: no second commit, no second enqueue.
    h.cleanup(&handle);
    assert_eq!(h.enqueued(), 1, "a repeated cleanup must not re-enqueue");
    h.fs.close(handle);
}

/// A write past EOF extends the file first: the gap reads back as zeros
/// and the committed row/cache copy agree byte-for-byte.
#[test]
fn out_of_order_and_hole_writes_land_byte_exact() {
    let h = Harness::new();
    let handle = h.create_file("/sparse.bin").expect("create");

    assert_eq!(h.write(&handle, 1024, b"tail", false, false).expect("w"), 4);
    assert_eq!(h.write(&handle, 0, b"head", false, false).expect("w"), 4);
    h.cleanup(&handle);

    let mut expected = vec![0u8; 1028];
    expected[..4].copy_from_slice(b"head");
    expected[1024..].copy_from_slice(b"tail");
    assert_eq!(
        fs::read(h.local_path("/sparse.bin")).expect("cache copy"),
        expected
    );
    assert_eq!(h.row("/sparse.bin").expect("row").size, 1028);
}

/// `write_to_eof` appends at the current end (`Offset` is -1 then — the
/// FSD's own convention, `src/dll/fsop.c:1053`).
#[test]
fn write_to_eof_appends_at_the_staged_end() {
    let h = Harness::new();
    let handle = h.create_file("/append.bin").expect("create");
    h.write(&handle, 0, b"first", false, false).expect("w");
    assert_eq!(
        h.write(&handle, u64::MAX, b"-second", true, false)
            .expect("w"),
        7
    );
    h.cleanup(&handle);
    assert_eq!(
        fs::read(h.local_path("/append.bin")).expect("cache copy"),
        b"first-second"
    );
}

/// `constrained_io` means "must not extend the file" (winfsp.h:460-462):
/// the write is clamped to the staged EOF instead of growing it.
#[test]
fn constrained_write_does_not_extend_the_file() {
    let h = Harness::new();
    let handle = h.create_file("/constrained.bin").expect("create");
    h.write(&handle, 0, b"1234", false, false).expect("w");

    assert_eq!(
        h.write(&handle, 2, b"WXYZ", false, true).expect("w"),
        2,
        "only the bytes up to EOF are written"
    );
    assert_eq!(
        h.write(&handle, 9, b"far", false, true).expect("w"),
        0,
        "a write entirely past EOF writes nothing"
    );
    h.cleanup(&handle);
    assert_eq!(
        fs::read(h.local_path("/constrained.bin")).expect("cache copy"),
        b"12WX",
        "the clamped bytes landed, the rest of the file is untouched"
    );
}

/// `flush` is zero-side-effect (K41): Windows and every player call it
/// constantly, and the commit belongs to cleanup alone.
#[test]
fn flush_does_not_commit() {
    let h = Harness::new();
    let handle = h.create_file("/flushed.bin").expect("create");
    h.write(&handle, 0, b"pending bytes", false, false)
        .expect("w");

    let mut info = FileInfo::default();
    h.fs.flush(Some(&handle), &mut info).expect("flush");
    h.fs.flush(None, &mut info).expect("volume flush");

    assert!(h.row("/flushed.bin").is_none(), "flush must not publish");
    assert_eq!(h.enqueued(), 0, "flush must not enqueue");
    assert_eq!(
        h.stage_siblings().len(),
        1,
        "the staged bytes survive a flush"
    );
    h.cleanup(&handle);
    assert_eq!(h.enqueued(), 1);
}

/// A handle can read back what it wrote before the commit (the staged
/// file is the handle's own view of the bytes).
#[test]
fn read_your_own_writes_before_the_commit() {
    let h = Harness::new();
    let handle = h.create_file("/rw.bin").expect("create");
    h.write(&handle, 0, b"humble writer", false, false)
        .expect("w");

    assert_eq!(
        h.read(&handle, 0, 6).expect("read back"),
        b"humble".to_vec()
    );
    assert_eq!(
        h.read(&handle, 7, 6).expect("read back"),
        b"writer".to_vec()
    );
    assert_eq!(
        h.read(&handle, 13, 8).expect("read at EOF"),
        Vec::<u8>::new()
    );

    // After the commit the same handle still reads the same bytes (the
    // read state is acquired lazily, off the committed cache copy).
    h.cleanup(&handle);
    assert_eq!(
        h.read(&handle, 0, 6).expect("read after commit"),
        b"humble".to_vec()
    );
}

/// A directory handle refuses `write` (nothing to write into).
#[test]
fn write_on_a_directory_handle_is_refused() {
    let h = Harness::new();
    let dir = h.create_dir_entry("/docs").expect("create dir");
    assert_eq!(
        status_of(h.write(&dir, 0, b"data", false, false)),
        0xC000_0010,
        "STATUS_INVALID_DEVICE_REQUEST"
    );
}

// ----------------------------------------------------- create shapes ---

/// Creating a name that already exists is a collision (`FILE_CREATE`),
/// and the FSD turns that into `STATUS_FILE_IS_A_DIRECTORY` itself when
/// the caller asked for a file and found a directory
/// (`FspFileSystemOpCreate_CollisionCheck`).
#[test]
fn create_colliding_with_an_existing_row_is_a_collision() {
    let h = Harness::new();
    h.seed_uploaded_file("/taken.txt", b"existing");

    let result = h.create_file("/taken.txt");
    assert_eq!(
        status_of(result),
        0xC000_0035,
        "STATUS_OBJECT_NAME_COLLISION"
    );
    assert_eq!(h.enqueued(), 0, "a refused create stages nothing");
}

/// A directory request becomes a `files` row (`Vfs::create_dir`
/// semantics): the root and existing names collide, a missing parent is
/// `STATUS_OBJECT_PATH_NOT_FOUND`, and nested creates work once the
/// parent exists.
#[test]
fn create_dir_creates_the_row_and_repeats_collide() {
    let h = Harness::new();

    let handle = h.create_dir_entry("/docs").expect("create dir");
    let row = h.row("/docs").expect("dir row");
    assert!(row.is_dir, "the row is a directory");
    assert!(
        row.is_uploaded && row.is_cached,
        "dir rows are born published"
    );
    assert!(!handle.has_pending_write(), "directories stage no bytes");
    h.fs.close(handle);

    assert_eq!(
        status_of(h.create_dir_entry("/docs")),
        0xC000_0035,
        "a repeated create collides"
    );
    assert_eq!(
        status_of(h.create_dir_entry("/missing/sub")),
        0xC000_003A,
        "STATUS_OBJECT_PATH_NOT_FOUND for a missing parent"
    );
    assert_eq!(
        status_of(h.create_dir_entry("/")),
        0xC000_0035,
        "the root always exists"
    );

    h.create_dir_entry("/docs/sub").expect("nested create");
    assert!(h.row("/docs/sub").expect("nested row").is_dir);
}

/// A file create under a missing parent is refused too (the staged
/// sibling must not be created under a directory the volume does not
/// have).
#[test]
fn create_file_under_a_missing_parent_is_refused() {
    let h = Harness::new();
    assert_eq!(
        status_of(h.create_file("/no/such/file.bin")),
        0xC000_003A,
        "STATUS_OBJECT_PATH_NOT_FOUND"
    );
    assert_eq!(h.enqueued(), 0);
}

/// A create whose name exceeds the mount layer's 255-UTF-16-unit
/// enumeration cap is refused at the create face with
/// STATUS_OBJECT_NAME_INVALID — publishing such a row would give it a
/// staging state it can never enumerate again (review M3).
#[test]
fn create_refuses_a_name_longer_than_the_enumeration_cap() {
    let h = Harness::new();
    let long = format!("/{}", "A".repeat(300));
    let outcome = h.create_file(&long);
    let reported = match &outcome {
        Ok(_) => "created".to_string(),
        Err(FspError::NTSTATUS(status)) => format!("status {status:#010x}"),
        Err(other) => format!("{other:?}"),
    };
    assert_eq!(
        reported, "status 0xc0000033",
        "M3 FIXED: a create past the 255-UTF-16-unit enumeration cap must \
         be refused with STATUS_OBJECT_NAME_INVALID; got {reported}"
    );
    assert!(
        h.row(&long).is_none(),
        "no row may be published for an overlong name"
    );
    assert!(
        h.stage_siblings().is_empty(),
        "no staging sibling may survive the refusal"
    );
    assert_eq!(h.enqueued(), 0, "nothing was queued");

    // The directory arm is refused through the same face.
    let long_dir = format!("/{}", "D".repeat(300));
    assert_eq!(
        status_of(h.create_dir_entry(&long_dir)),
        0xC000_0033,
        "STATUS_OBJECT_NAME_INVALID for the directory arm too"
    );
    assert!(h.row(&long_dir).is_none(), "no directory row either");
}

// ------------------------------------------------------------ overwrite ---

/// CREATE_ALWAYS on an existing file is two FSD transactions: the open
/// returns `FILE_OVERWRITTEN`, then the kernel posts the Overwrite
/// request (`src/sys/create.c:1172-1228`) — which is what replaces the
/// content and what this test drives.
#[test]
fn overwrite_replaces_existing_content_and_queues_once() {
    let h = Harness::new();
    h.seed_uploaded_file("/replace.txt", b"0123456789");

    // The FSD's open for FILE_OVERWRITE_IF (disposition 5).
    let handle = h.open("/replace.txt").expect("open existing");
    let mut info = FileInfo::default();
    h.fs.overwrite(&handle, 0, false, 0, None, &mut info)
        .expect("overwrite");
    assert_eq!(info.file_size, 0, "the overwrite truncates");

    h.write(&handle, 0, b"new", false, false).expect("write");
    h.cleanup(&handle);

    assert_eq!(h.enqueued(), 1, "only the replacement is enqueued");
    let row = h.row("/replace.txt").expect("row");
    assert_eq!(row.size, 3, "the row carries the new size");
    assert!(!row.is_uploaded, "the replacement is pending");
    assert_eq!(
        fs::read(h.local_path("/replace.txt")).expect("copy"),
        b"new"
    );
}

/// A created file that is closed with the delete flag leaves nothing
/// behind: no row, no cache copy, no staging sibling, no queued job.
#[test]
fn delete_on_close_discards_a_created_file() {
    let h = Harness::new();
    let handle = h.create_file("/temp.bin").expect("create");
    h.write(&handle, 0, b"scratch", false, false)
        .expect("write");

    h.cleanup_delete(&handle, "/temp.bin");

    assert!(h.row("/temp.bin").is_none(), "no row survives");
    assert!(
        !h.local_path("/temp.bin").exists(),
        "no cache copy survives"
    );
    assert!(h.stage_siblings().is_empty(), "no staging sibling");
    assert_eq!(h.enqueued(), 0, "nothing was queued");
}

// ------------------------------------------------------------ set_* ---

/// `set_file_size` resizes the staged bytes (truncate and zero-fill
/// extend) and the committed row/cache copy follow.
#[test]
fn set_file_size_resizes_the_staged_bytes() {
    let h = Harness::new();
    let handle = h.create_file("/size.bin").expect("create");
    h.write(&handle, 0, b"0123456789", false, false).expect("w");

    let mut info = FileInfo::default();
    h.fs.set_file_size(&handle, 4, false, &mut info)
        .expect("truncate");
    assert_eq!(info.file_size, 4, "the response reports the new size");

    h.fs.set_file_size(&handle, 6, false, &mut info)
        .expect("extend");
    assert_eq!(info.file_size, 6);
    h.cleanup(&handle);

    assert_eq!(
        fs::read(h.local_path("/size.bin")).expect("copy"),
        b"0123\0\0",
        "the extension is zero-filled"
    );
    assert_eq!(h.row("/size.bin").expect("row").size, 6);
}

/// Win32 `TRUNCATE_EXISTING` arrives as an open of an existing file plus
/// `SetEndOfFile(0)`: the staged writer is materialised from the current
/// bytes and then truncated, and the commit publishes the empty file.
#[test]
fn set_file_size_on_an_opened_file_truncates_to_empty() {
    let h = Harness::new();
    h.seed_uploaded_file("/truncate.txt", b"0123456789");

    let handle = h.open("/truncate.txt").expect("open");
    let mut info = FileInfo::default();
    h.fs.set_file_size(&handle, 0, false, &mut info)
        .expect("truncate");
    assert_eq!(info.file_size, 0);
    h.cleanup(&handle);

    let row = h.row("/truncate.txt").expect("row");
    assert_eq!(row.size, 0, "the row is empty");
    assert_eq!(fs::read(h.local_path("/truncate.txt")).expect("copy"), b"");
    assert_eq!(h.enqueued(), 1, "the truncation is one queued job");
}

/// An allocation-size set never extends EOF (winfsp.h's rule: allocation
/// size is a hint, the file size is the data) and a redundant file-size
/// set touches nothing.
#[test]
fn allocation_size_set_does_not_extend_eof() {
    let h = Harness::new();
    let handle = h.create_file("/alloc.bin").expect("create");
    h.write(&handle, 0, b"1234", false, false).expect("w");

    let mut info = FileInfo::default();
    h.fs.set_file_size(&handle, 4096, true, &mut info)
        .expect("alloc");
    assert_eq!(info.file_size, 4, "EOF is unchanged by an allocation set");
    h.cleanup(&handle);
    assert_eq!(h.row("/alloc.bin").expect("row").size, 4);
}

/// `set_basic_info`'s last-write time becomes the committed row's mtime
/// (the db has one mtime column); a zero field means "leave it alone".
#[test]
fn set_basic_info_mtime_lands_in_the_committed_row() {
    let h = Harness::new();
    let handle = h.create_file("/timed.bin").expect("create");
    h.write(&handle, 0, b"bytes", false, false).expect("w");

    let mut info = FileInfo::default();
    h.fs.set_basic_info(&handle, 0, 0, 0, T_1_700_000_000_FILETIME, 0, &mut info)
        .expect("set_basic_info");
    assert_eq!(
        info.last_write_time, T_1_700_000_000_FILETIME,
        "the response echoes the staged mtime"
    );
    h.cleanup(&handle);

    assert_eq!(
        h.row("/timed.bin").expect("row").mtime,
        1_700_000_000.0,
        "the committed row carries the requested mtime"
    );
}

/// A `set_basic_info` with all-zero times changes nothing (the header's
/// "0 means do not change") and still answers with the current stat.
#[test]
fn set_basic_info_with_zero_times_is_a_no_op() {
    let h = Harness::new();
    let handle = h.create_file("/untimed.bin").expect("create");
    h.write(&handle, 0, b"bytes", false, false).expect("w");

    let mut info = FileInfo::default();
    h.fs.set_basic_info(&handle, 0, 0, 0, 0, 0, &mut info)
        .expect("set_basic_info");
    assert_eq!(info.file_size, 5, "the stat still reports the staged size");
    h.cleanup(&handle);

    let row = h.row("/untimed.bin").expect("row");
    assert!(
        row.mtime > 1_700_000_000.0,
        "the commit took the wall clock"
    );
}

// ------------------------------------------------------------- rename ---

/// Rename moves the row in place (id/chunk linkage preserved) and the
/// cache copy follows; the old path is gone and the new one reads back.
#[test]
fn rename_moves_the_row_and_the_cache_copy() {
    let h = Harness::new();
    h.seed_uploaded_file("/from.txt", b"payload");

    let handle = h.open("/from.txt").expect("open");
    h.rename(&handle, "/from.txt", "/to.txt", false)
        .expect("rename");

    assert!(h.row("/from.txt").is_none(), "the old row is gone");
    let moved = h.row("/to.txt").expect("the new row exists");
    assert_eq!(moved.size, 7);
    assert!(moved.is_uploaded, "the upload state moved with the row");
    assert_eq!(
        fs::read(h.local_path("/to.txt")).expect("cache copy moved"),
        b"payload"
    );
    assert!(!h.local_path("/from.txt").exists(), "the old copy is gone");

    assert_eq!(
        status_of(h.open("/from.txt")),
        0xC000_0034,
        "STATUS_OBJECT_NAME_NOT_FOUND at the old name"
    );
    let reopened = h.open("/to.txt").expect("open the new name");
    assert_eq!(h.read(&reopened, 0, 7).expect("read"), b"payload".to_vec());
}

/// The destination is only replaced when the FSD says so
/// (`ReplaceIfExists`): otherwise the collision is reported and nothing
/// moves. A directory destination is always a collision.
#[test]
fn rename_overwrites_the_destination_only_when_asked() {
    let h = Harness::new();
    h.seed_uploaded_file("/src.txt", b"source");
    h.seed_uploaded_file("/dst.txt", b"destination");

    let handle = h.open("/src.txt").expect("open");
    assert_eq!(
        status_of(h.rename(&handle, "/src.txt", "/dst.txt", false)),
        0xC000_0035,
        "STATUS_OBJECT_NAME_COLLISION without ReplaceIfExists"
    );
    assert_eq!(
        fs::read(h.local_path("/dst.txt")).expect("copy"),
        b"destination",
        "a refused rename changes nothing"
    );

    h.rename(&handle, "/src.txt", "/dst.txt", true)
        .expect("rename over");
    assert!(h.row("/src.txt").is_none());
    assert_eq!(h.row("/dst.txt").expect("row").size, 6);
    assert_eq!(
        fs::read(h.local_path("/dst.txt")).expect("copy"),
        b"source",
        "the destination was replaced by the source's bytes"
    );

    // A directory destination is refused even with replace on.
    h.create_dir_entry("/dir-dst").expect("create dir");
    h.seed_uploaded_file("/src2.txt", b"x");
    let handle = h.open("/src2.txt").expect("open");
    assert_eq!(
        status_of(h.rename(&handle, "/src2.txt", "/dir-dst", true)),
        0xC000_0035,
        "a directory destination collides"
    );
}

/// A rename into a missing directory is `STATUS_OBJECT_PATH_NOT_FOUND`,
/// and renaming a pending upload away from its queued path is refused
/// with the shared "file in use" status (the queue resolves the job by
/// its `rel_path`, so moving it would orphan the only copy of the
/// bytes).
#[test]
fn rename_guards_missing_parents_and_pending_uploads() {
    let h = Harness::new();
    h.seed_uploaded_file("/a.txt", b"payload");
    let handle = h.open("/a.txt").expect("open");
    assert_eq!(
        status_of(h.rename(&handle, "/a.txt", "/missing/b.txt", false)),
        0xC000_003A,
        "STATUS_OBJECT_PATH_NOT_FOUND"
    );

    // A real pending row: created, written and committed while the
    // transport is unreachable, so the local copy is the only copy.
    let pending = h.commit_new_file("/pending.txt", b"not uploaded yet");
    let row = h.row("/pending.txt").expect("pending row");
    assert!(!row.is_uploaded, "the row is pending");

    let handle = h.open("/pending.txt").expect("open the pending row");
    assert_eq!(
        status_of(h.rename(&handle, "/pending.txt", "/moved.txt", false)),
        0xC000_0043,
        "STATUS_SHARING_VIOLATION for a pending upload"
    );
    assert!(h.row("/pending.txt").is_some(), "the row was kept");
    h.fs.close(pending);
}

/// Renaming a path that still has uncommitted staged bytes is refused:
/// the staging sibling cannot follow the rename.
#[test]
fn rename_refuses_a_handle_with_uncommitted_bytes() {
    let h = Harness::new();
    let handle = h.create_file("/inflight.bin").expect("create");
    h.write(&handle, 0, b"half written", false, false)
        .expect("w");

    assert_eq!(
        status_of(h.rename(&handle, "/inflight.bin", "/renamed.bin", false)),
        0xC000_0043,
        "STATUS_SHARING_VIOLATION while bytes are staged"
    );
    h.cleanup(&handle);
    assert!(h.row("/inflight.bin").is_some(), "the commit still landed");
}

// ------------------------------------------------------------- delete ---

/// `set_delete` marks the handle and `cleanup(FspCleanupDelete)` does the
/// deleting: the row and the cache copy both go.
#[test]
fn delete_on_close_removes_the_row_and_the_cache_copy() {
    let h = Harness::new();
    h.seed_uploaded_file("/gone.txt", b"payload");
    h.seed_uploaded_file("/kept.txt", b"kept");

    let handle = h.open("/gone.txt").expect("open");
    assert!(!handle.delete_mark(), "no delete flag before set_delete");
    h.set_delete(&handle, "/gone.txt", true)
        .expect("set_delete");
    assert!(handle.delete_mark(), "the delete-on-close mark is set");
    h.fs.close(handle);

    let handle = h.open("/gone.txt").expect("open again");
    h.cleanup_delete(&handle, "/gone.txt");

    assert!(h.row("/gone.txt").is_none(), "the row is gone");
    assert!(
        !h.local_path("/gone.txt").exists(),
        "the cache copy is gone"
    );
    h.fs.close(handle);

    // set_delete(false) clears the mark without deleting anything.
    let handle = h.open("/kept.txt").expect("open");
    h.set_delete(&handle, "/kept.txt", false).expect("clear");
    assert!(!handle.delete_mark(), "the mark is cleared");
    h.cleanup(&handle);
    assert!(h.row("/kept.txt").is_some(), "nothing was deleted");
}

/// The pending-upload guard (the same adjudication as `Vfs::remove_file`
/// and the WebDAV adapter): a pending row whose local copy still exists
/// is refused — deleting it would orphan the only copy of the bytes.
/// The refusal is reportable because `set_delete` is a callback that
/// returns a status (`can_delete` is not exposed by winfsp-rs 0.13).
#[test]
fn set_delete_refuses_a_pending_upload() {
    let h = Harness::new();
    let handle = h.commit_new_file("/pending-del.txt", b"only local copy");
    assert!(!h.row("/pending-del.txt").expect("row").is_uploaded);
    h.fs.close(handle);

    let handle = h.open("/pending-del.txt").expect("open");
    assert_eq!(
        status_of(h.set_delete(&handle, "/pending-del.txt", true)),
        0xC000_0043,
        "STATUS_SHARING_VIOLATION for a pending upload"
    );
    assert!(!handle.delete_mark(), "a refused delete is not marked");
    h.cleanup_delete(&handle, "/pending-del.txt");
    assert!(
        h.row("/pending-del.txt").is_some(),
        "the pending row survives the delete attempt"
    );
    assert!(
        h.local_path("/pending-del.txt").exists(),
        "and so does its copy"
    );
}

/// A non-empty directory is refused (what `RemoveDirectory` reports),
/// an empty one deletes.
#[test]
fn delete_refuses_a_non_empty_directory() {
    let h = Harness::new();
    h.create_dir_entry("/parent").expect("create dir");
    h.create_dir_entry("/parent/child")
        .expect("create nested dir");

    let parent = h.open("/parent").expect("open dir");
    assert_eq!(
        status_of(h.set_delete(&parent, "/parent", true)),
        0xC000_0101,
        "STATUS_DIRECTORY_NOT_EMPTY"
    );

    let child = h.open("/parent/child").expect("open nested dir");
    h.set_delete(&child, "/parent/child", true)
        .expect("empty ok");
    h.cleanup_delete(&child, "/parent/child");
    assert!(h.row("/parent/child").is_none(), "the empty dir is gone");
}

// ------------------------------------------------------------- volume ---

/// The volume label is process-level (K44): `set_volume_label` renames
/// the mounted volume in place and `get_volume_info` renders the renamed
/// snapshot. The label's wide encoding inside `VolumeInfo` is winfsp-rs'
/// `set_volume_label` (no getter is exposed), so the adapter's contract
/// ends at the snapshot it renders from — same assertion shape the WF1
/// metadata tests use.
#[test]
fn volume_label_is_process_level() {
    let h = Harness::new();
    assert_eq!(h.fs.volume().label, "cydrive-test");

    let label = fsd_path("renamed");
    let mut info: VolumeInfo = unsafe { std::mem::zeroed() };
    h.fs.set_volume_label(U16CStr::from_slice(&label).expect("label"), &mut info)
        .expect("set_volume_label");

    assert_eq!(h.fs.volume().label, "renamed", "the snapshot follows");
    let mut read_back: VolumeInfo = unsafe { std::mem::zeroed() };
    h.fs.get_volume_info(&mut read_back).expect("volume info");
    assert_eq!(read_back.total_size, h.fs.volume().total_size);
    assert_eq!(read_back.free_size, h.fs.volume().free_size);
}
