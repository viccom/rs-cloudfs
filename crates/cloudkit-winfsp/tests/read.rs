//! Integration tests for the WF2 read path of [`CloudFs`].
//!
//! Contract under test (plan §3-WF2 + K41/K42/K45):
//! - `open` runs K33's triple-gate dispatch: a plaintext, non-zero row
//!   over a range-capable transport gets the bounded `open_range` window
//!   model; a cache-first hit (WF0) and every hydrate arm (encrypted
//!   row, range-incapable transport, 0-byte row) get a local-file
//!   reader; the password gate's `MissingPassword` is
//!   `STATUS_ACCESS_DENIED` at open;
//! - the window model is the one the WebDAV adapter already ships
//!   (K34): fetched only when the buffered window does not cover the
//!   requested offset, re-anchored at that offset, aggregated whole
//!   before serving (backpressure), short reads legal, never a byte past
//!   EOF, and never a full `open` on the streaming arm;
//! - `flush` is zero-side-effect (K41: Windows and every player call it
//!   constantly);
//! - a closed file handle parks its read state in the K41 grace table
//!   for a configurable window: a reopen inside it reuses the parked
//!   window (no new fetch), one after it rebuilds — and the table is
//!   bounded (oldest close evicted first);
//! - failures ride the K45 table: a directory read is
//!   `STATUS_NOT_A_DIRECTORY`, a transport failure is
//!   `STATUS_IO_DEVICE_ERROR` with an `error!` log line.
//!
//! Everything here runs WITHOUT WinFsp installed: the DLL is only ever
//! reached through `DirBuffer` and the mount host, neither of which these
//! tests touch.

#![cfg(all(windows, feature = "winfsp"))]

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::{MockTransport, OpenRangeAction};
use cloudkit_core::transport::{
    Capabilities, CloudTransport, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_winfsp::fs::{CloudFs, Handle};
use winfsp::filesystem::{FileInfo, FileSystemContext, OpenFileInfo};
use winfsp::{FspError, U16CStr};

/// Window every harness starts with: small enough that window
/// boundaries, re-anchors and multi-window reads are observable at byte
/// scale (production uses the 4 MiB K34 window).
const TEST_WINDOW: u64 = 16;

/// Grace period the harnesses that do not exercise expiry start with.
const TEST_GRACE: Duration = Duration::from_secs(30);

/// Grace table capacity for the harnesses that do not exercise
/// eviction.
const TEST_CAPACITY: usize = 64;

/// Deterministic payload bytes (`i % 251`), so a byte-exact mismatch
/// names the offset it happened at.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// VfsConfig for these tests: tiny chunks, one worker, fast retry,
/// optional encryption password (same shape as the other adapter tests).
fn test_cfg(password: Option<&str>) -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: 64,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: password.map(str::to_string),
        encryption_scheme: cloudkit_core::config::EncryptionScheme::Gcm,
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// The three capability bits the mock declares by default (range_read on).
fn range_caps() -> Capabilities {
    Capabilities {
        range_read: true,
        inbound: true,
        chat: true,
        ..Capabilities::none()
    }
}

/// A transport that declares NO `range_read`: every row must route to the
/// hydrate arm (K33's second gate).
fn no_range_caps() -> Capabilities {
    Capabilities {
        range_read: false,
        inbound: true,
        chat: true,
        ..Capabilities::none()
    }
}

/// Real temp environment: SQLite db + cache tree + pre-connected mock
/// transport + Vfs + the adapter under test (L5 test code may depend on
/// anything but drivers — R1).
struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    cache_root: PathBuf,
    mock: Arc<MockTransport>,
    fs: CloudFs,
    rt: tokio::runtime::Runtime,
}

impl Harness {
    /// The common harness: no password, [`TEST_WINDOW`] / [`TEST_GRACE`]
    /// / [`TEST_CAPACITY`].
    fn new(mock: MockTransport) -> Self {
        Self::with(mock, None, TEST_WINDOW, TEST_GRACE, TEST_CAPACITY)
    }

    /// The same assembly with the adapter's tuning seams injected.
    fn with(
        mock: MockTransport,
        password: Option<&str>,
        window: u64,
        grace: Duration,
        capacity: usize,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let dir = tempfile::tempdir().expect("temp dir");
        let cache_root = dir.path().join("cache");
        let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
        let cache = CacheManager::new(cache_root.clone(), 1 << 30);
        let mock = Arc::new(mock);
        // Vfs::new spawns its upload queue: a runtime must be in scope.
        let _guard = rt.enter();
        rt.block_on(mock.connect()).expect("pre-connect mock");
        let transport: Arc<dyn CloudTransport> = mock.clone();
        let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg(password)));
        let fs = CloudFs::new(vfs, rt.handle().clone(), "cydrive-test")
            .with_stream_window(window)
            .with_handle_grace(grace)
            .with_grace_capacity(capacity);
        Self {
            _dir: dir,
            db,
            cache_root,
            mock,
            fs,
            rt,
        }
    }

    /// Pushes `bytes` to the mock remote as `rel` in one message; returns
    /// the receipt (its single msg id addresses the whole payload).
    fn seed_remote(&self, rel: &str, bytes: &[u8]) -> UploadReceipt {
        let dir = tempfile::tempdir().expect("seed scratch dir");
        let rel_path = RelPath::new(rel).expect("valid rel path");
        let local = dir.path().join(rel_path.name());
        fs::write(&local, bytes).expect("write seed scratch file");
        self.rt
            .block_on(self.mock.upload(&UploadJob {
                rel_path,
                local_path: local,
                size: bytes.len() as u64,
                chunk_count: 1,
                chunk_size: bytes.len().max(1) as u64,
            }))
            .expect("seed upload to the mock remote")
    }

    /// Inserts one `files` row (`msg_id` = the row's remote handle, the
    /// single-chunk shape `remote_handle_for` falls back to).
    fn seed_row(&self, rel: &str, size: i64, is_encrypted: bool, msg_id: Option<i64>) {
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
                is_uploaded: true,
                is_cached: false,
                is_encrypted,
                chunk_count: 1,
                mime_type: None,
            })
            .expect("seed files row");
    }

    /// The common streaming precondition: remote bytes plus the matching
    /// plaintext row.
    fn seed_remote_file(&self, rel: &str, bytes: &[u8]) -> UploadReceipt {
        let receipt = self.seed_remote(rel, bytes);
        self.seed_row(rel, bytes.len() as i64, false, Some(receipt.first_msg_id));
        receipt
    }

    /// The K47 streaming precondition (Phase 3.5-a E3): the ciphertext
    /// CONTAINER rides the remote, the row carries the PLAINTEXT size
    /// (K35), `is_encrypted` and the explicit `aead_v2` scheme column —
    /// the exact shape the K47 stream gate consumes.
    fn seed_aead_v2_file(&self, rel: &str, plaintext: &[u8], password: &str) -> UploadReceipt {
        let container = cloudkit_core::crypto::AeadV2::new().encrypt(password, plaintext);
        let receipt = self.seed_remote(rel, &container);
        let rel_path = RelPath::new(rel).expect("valid rel path");
        self.db
            .upsert_file_scheme(
                &FileUpsert {
                    rel_path: rel_path.as_str().to_string(),
                    name: rel_path.name().to_string(),
                    parent_dir: "/".to_string(),
                    size: plaintext.len() as i64,
                    mtime: 1_700_000_000.0,
                    sha256: None,
                    is_dir: false,
                    telegram_msg_id: Some(receipt.first_msg_id),
                    is_uploaded: true,
                    is_cached: false,
                    is_encrypted: true,
                    chunk_count: 1,
                    mime_type: None,
                },
                "aead_v2",
            )
            .expect("seed aead_v2 row");
        receipt
    }

    /// One directory row.
    fn seed_dir(&self, rel: &str) {
        let rel_path = RelPath::new(rel).expect("valid rel path");
        self.db
            .upsert_file(&FileUpsert {
                rel_path: rel_path.as_str().to_string(),
                name: rel_path.name().to_string(),
                parent_dir: "/".to_string(),
                size: 0,
                mtime: 1_700_000_000.0,
                sha256: None,
                is_dir: true,
                telegram_msg_id: None,
                is_uploaded: false,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 0,
                mime_type: None,
            })
            .expect("seed dir row");
    }

    /// Writes a plaintext copy of `rel` straight into the mirrored cache
    /// tree (the WF0 cache-first precondition).
    fn seed_cache_copy(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let rel_path = RelPath::new(rel).expect("valid rel path");
        let local = CacheManager::new(self.cache_root.clone(), 1 << 30).local_path(&rel_path);
        fs::create_dir_all(local.parent().expect("cache parent dir")).expect("create cache dirs");
        fs::write(&local, bytes).expect("write cache copy");
        local
    }

    /// The FSD `open` callback, with the create/open response going to a
    /// scratch `OpenFileInfo`.
    fn open(&self, name: &str) -> winfsp::Result<Handle> {
        let wide = fsd_path(name);
        let mut info: OpenFileInfo = unsafe { std::mem::zeroed() };
        self.fs
            .open(U16CStr::from_slice(&wide).expect("name"), 0, 0, &mut info)
    }

    /// The FSD `read` callback, allocating the buffer WinFsp would hand
    /// in and returning exactly the bytes it reported.
    fn read(&self, handle: &Handle, offset: u64, len: usize) -> winfsp::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let filled = self.fs.read(handle, &mut buf, offset)? as usize;
        assert!(filled <= buf.len(), "read reported more than the buffer");
        buf.truncate(filled);
        Ok(buf)
    }
}

/// FSD path string -> the wide NUL-terminated form the callbacks receive.
fn fsd_path(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
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

/// Minimal capture subscriber (no extra dependency): records
/// `LEVEL: message` for every event dispatched on this thread, so the
/// K45 "never silent" clause can be asserted instead of assumed.
struct LevelCapture {
    events: Arc<Mutex<Vec<String>>>,
}

impl tracing::Subscriber for LevelCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.0 = value.to_string();
                }
            }

            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("{}: {}", event.metadata().level(), message.0));
    }
}

/// WF2's core window contract, end to end through the FSD callbacks: a
/// plaintext row over a range-capable transport is served from bounded
/// `open_range` windows — anchored at the requested offset, reused inside
/// a window, re-anchored when a read leaves it, clamped at EOF, empty at
/// or past EOF — and never through a full `open`.
#[test]
fn stream_reads_fetch_bounded_windows_anchored_at_the_request() {
    let h = Harness::new(MockTransport::builder().capabilities(range_caps()).build());
    let content = pattern(64);
    h.seed_remote_file("/stream.bin", &content);
    let handle = h.open("\\stream.bin").expect("open");

    // ① offset 0: exactly one window, anchored at 0.
    assert_eq!(h.read(&handle, 0, 4).expect("read"), &content[0..4]);
    assert_eq!(h.mock.open_range_calls(), vec![(0, TEST_WINDOW)]);

    // ② inside the buffered window: no new fetch.
    assert_eq!(h.read(&handle, 2, 4).expect("read"), &content[2..6]);
    assert_eq!(h.mock.open_range_calls(), vec![(0, TEST_WINDOW)]);

    // ③ leaving the window re-anchors AT the requested offset.
    assert_eq!(h.read(&handle, 16, 1).expect("read"), &content[16..17]);
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, TEST_WINDOW), (16, TEST_WINDOW)]
    );

    // ④ a far offset anchors there, and ⑥ the window clamps to EOF
    // (min(window, total - offset) = 4), so the read is a legal short one.
    assert_eq!(h.read(&handle, 60, 10).expect("read"), &content[60..64]);
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, TEST_WINDOW), (16, TEST_WINDOW), (60, 4)]
    );

    // ⑤ at and past EOF: empty, and still no further fetch.
    assert_eq!(
        h.read(&handle, 64, 4).expect("read at EOF"),
        Vec::<u8>::new()
    );
    assert_eq!(
        h.read(&handle, 4096, 4).expect("read past EOF"),
        Vec::<u8>::new()
    );
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, TEST_WINDOW), (16, TEST_WINDOW), (60, 4)]
    );

    // ⑧ the streaming arm never touches the full-open face.
    assert!(
        h.mock.open_calls().is_empty(),
        "windowed reads must never full-open"
    );
}

/// ⑦ A read larger than the window is served by filling window after
/// window (never past EOF), and the bytes are byte-exact across the
/// seams.
#[test]
fn a_large_read_spans_windows_in_order_and_never_passes_eof() {
    let h = Harness::with(
        MockTransport::builder().capabilities(range_caps()).build(),
        None,
        1024,
        TEST_GRACE,
        TEST_CAPACITY,
    );
    let content = pattern(4096);
    h.seed_remote_file("/big.bin", &content);
    let handle = h.open("\\big.bin").expect("open");

    // 3 KiB request through 1 KiB windows: three fetches, in order.
    assert_eq!(h.read(&handle, 0, 3072).expect("read"), &content[0..3072]);
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, 1024), (1024, 1024), (2048, 1024)]
    );

    // The tail read reuses the third window, then fetches the last one.
    assert_eq!(
        h.read(&handle, 2048, 2048).expect("read"),
        &content[2048..4096]
    );
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, 1024), (1024, 1024), (2048, 1024), (3072, 1024)]
    );
    for (offset, len) in h.mock.open_range_calls() {
        assert!(
            offset + len <= content.len() as u64,
            "a window ran past EOF: ({offset}, {len})"
        );
    }
}

/// ⑨ WF0 cache-first through the adapter: a cached row serves the LOCAL
/// plaintext, with the remote never consulted (no window, no full open).
#[test]
fn cached_rows_serve_local_bytes_with_zero_remote_work() {
    let h = Harness::new(MockTransport::builder().capabilities(range_caps()).build());
    h.seed_remote_file("/cached.bin", &pattern(64));
    let local = b"local-copy-wins";
    h.seed_cache_copy("/cached.bin", local);

    let handle = h.open("\\cached.bin").expect("open");
    assert_eq!(
        h.read(&handle, 0, local.len()).expect("read"),
        local,
        "the served bytes are the cache copy, not the remote payload"
    );
    assert_eq!(h.read(&handle, 7, 4).expect("read"), &local[7..11]);
    assert!(
        h.mock.open_range_calls().is_empty(),
        "a cache hit must not open a window"
    );
    assert!(
        h.mock.open_calls().is_empty(),
        "a cache hit must not consult the remote at all"
    );
}

/// ⑩ The encrypted arm (K33: whole-file AEAD can never be range-sliced)
/// hydrates to a local plaintext file and serves through it — the
/// full-open face once, the range face never.
#[test]
fn encrypted_rows_hydrate_to_a_local_file_without_windows() {
    let h = Harness::with(
        MockTransport::builder()
            .capabilities(no_range_caps())
            .build(),
        Some("pw"),
        TEST_WINDOW,
        TEST_GRACE,
        TEST_CAPACITY,
    );
    let plaintext = b"secret payload bytes".to_vec();
    let ciphertext = cloudkit_core::crypto::encrypt("pw", &plaintext);
    let receipt = h.seed_remote("/enc.bin", &ciphertext);
    // Row size is the PLAINTEXT length (the Python contract); the remote
    // artifact is the longer ciphertext container.
    h.seed_row(
        "/enc.bin",
        plaintext.len() as i64,
        true,
        Some(receipt.first_msg_id),
    );

    let handle = h.open("\\enc.bin").expect("open");
    assert_eq!(
        h.read(&handle, 0, plaintext.len()).expect("read"),
        plaintext
    );
    assert_eq!(h.read(&handle, 7, 4).expect("read"), &plaintext[7..11]);
    assert!(
        h.mock.open_range_calls().is_empty(),
        "an encrypted row can never be windowed"
    );
    assert_eq!(
        h.mock.open_calls().len(),
        1,
        "hydrate downloaded through the full-open face exactly once"
    );
}

/// ⑪ The password gate fires at open: an encrypted row with no configured
/// password is `STATUS_ACCESS_DENIED`, not a degraded read, and nothing
/// reaches the transport.
#[test]
fn encrypted_rows_without_a_password_are_access_denied() {
    let h = Harness::with(
        MockTransport::builder()
            .capabilities(no_range_caps())
            .build(),
        None,
        TEST_WINDOW,
        TEST_GRACE,
        TEST_CAPACITY,
    );
    let plaintext = b"secret".to_vec();
    let ciphertext = cloudkit_core::crypto::encrypt("pw", &plaintext);
    let receipt = h.seed_remote("/locked.bin", &ciphertext);
    h.seed_row(
        "/locked.bin",
        plaintext.len() as i64,
        true,
        Some(receipt.first_msg_id),
    );

    let status = status_of(h.open("\\locked.bin"));
    assert_eq!(status, 0xC000_0022, "STATUS_ACCESS_DENIED");
    assert!(h.mock.open_calls().is_empty() && h.mock.open_range_calls().is_empty());
}

/// ⑫ K41: `flush` has zero side effects — the buffered window, the bytes
/// it serves and the transport call log are all untouched (Windows and
/// players flush constantly; an error or a fetch here would be a
/// self-inflicted regression).
#[test]
fn flush_has_no_side_effects() {
    let h = Harness::new(MockTransport::builder().capabilities(range_caps()).build());
    let content = pattern(64);
    h.seed_remote_file("/flush.bin", &content);
    let handle = h.open("\\flush.bin").expect("open");
    assert_eq!(h.read(&handle, 4, 4).expect("read"), &content[4..8]);
    let before = h.mock.open_range_calls();

    let mut info = FileInfo::default();
    h.fs.flush(Some(&handle), &mut info).expect("file flush");
    h.fs.flush(None, &mut info).expect("volume flush");

    // The window still covers the same bytes: no refetch, no drift.
    assert_eq!(h.read(&handle, 4, 4).expect("read"), &content[4..8]);
    assert_eq!(h.read(&handle, 6, 4).expect("read"), &content[6..10]);
    assert_eq!(
        h.mock.open_range_calls(),
        before,
        "flush must not fetch (and must not drop the window)"
    );
}

/// ⑬ K41 handle grace: a reopen inside the grace window reuses the parked
/// read state (the buffered window survives the close — no new fetch);
/// after the window has passed the reopen builds a fresh state and
/// fetches again.
#[test]
fn handle_grace_reuses_read_state_inside_the_window_and_rebuilds_after() {
    let grace = Duration::from_millis(200);
    let h = Harness::with(
        MockTransport::builder().capabilities(range_caps()).build(),
        None,
        TEST_WINDOW,
        grace,
        TEST_CAPACITY,
    );
    let content = pattern(64);
    h.seed_remote_file("/grace.bin", &content);

    let first = h.open("\\grace.bin").expect("open");
    assert_eq!(h.read(&first, 0, 8).expect("read"), &content[0..8]);
    assert_eq!(h.mock.open_range_calls().len(), 1);
    h.fs.close(first);

    // Inside the window: the parked window is reused verbatim.
    let second = h.open("\\grace.bin").expect("reopen");
    assert_eq!(h.read(&second, 0, 8).expect("read"), &content[0..8]);
    assert_eq!(
        h.mock.open_range_calls().len(),
        1,
        "a reopen inside the grace window must reuse the parked read state"
    );
    h.fs.close(second);

    std::thread::sleep(grace + Duration::from_millis(100));
    let third = h
        .open("\\grace.bin")
        .expect("reopen after the grace window");
    assert_eq!(h.read(&third, 0, 8).expect("read"), &content[0..8]);
    assert_eq!(
        h.mock.open_range_calls().len(),
        2,
        "after the grace window the reopen must build a fresh state"
    );
}

/// The grace table is bounded: at capacity the entry closest to expiry
/// (the oldest close) is evicted. Pinned through the fetch log — the
/// offsets distinguish the two files, and the surviving entry is checked
/// first so the assertion also fails if reuse stops working at all.
#[test]
fn the_grace_table_evicts_the_oldest_close_at_capacity() {
    let h = Harness::with(
        MockTransport::builder().capabilities(range_caps()).build(),
        None,
        TEST_WINDOW,
        TEST_GRACE,
        1,
    );
    let content = pattern(64);
    h.seed_remote_file("/a.bin", &content);
    h.seed_remote_file("/b.bin", &content);

    let a = h.open("\\a.bin").expect("open a");
    h.read(&a, 0, 4).expect("read a");
    h.fs.close(a);

    let b = h.open("\\b.bin").expect("open b");
    h.read(&b, 32, 4).expect("read b");
    h.fs.close(b); // capacity 1: closing b evicts a's entry
    let parked = h.mock.open_range_calls();

    // b is the survivor: its parked window still covers the read.
    let b_again = h.open("\\b.bin").expect("reopen b");
    h.read(&b_again, 32, 4).expect("read b again");
    h.fs.close(b_again);
    assert_eq!(
        h.mock.open_range_calls(),
        parked,
        "the newest close must stay parked (reused, not refetched)"
    );

    // a was evicted: its reopen builds a fresh state and fetches again.
    let a_again = h.open("\\a.bin").expect("reopen a");
    h.read(&a_again, 0, 4).expect("read a again");
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, TEST_WINDOW), (32, TEST_WINDOW), (0, TEST_WINDOW)],
        "the evicted entry must refetch on reopen"
    );
}

/// ⑭ A directory handle has no read state: `read` answers
/// `STATUS_NOT_A_DIRECTORY` before any transport work.
#[test]
fn reading_a_directory_is_refused() {
    let h = Harness::new(MockTransport::builder().capabilities(range_caps()).build());
    h.seed_dir("/docs");
    let handle = h.open("\\docs").expect("open dir");

    let mut buf = [0u8; 4];
    let status = status_of(h.fs.read(&handle, &mut buf, 0));
    assert_eq!(status, 0xC000_0103, "STATUS_NOT_A_DIRECTORY");
    assert!(h.mock.open_calls().is_empty() && h.mock.open_range_calls().is_empty());
}

/// ⑮ K45: a failed window fetch is `STATUS_IO_DEVICE_ERROR` AND leaves an
/// `error!` log line — the EIO fallback is never silent.
#[test]
fn a_transport_failure_maps_to_io_device_error_and_logs() {
    let mock = MockTransport::builder()
        .capabilities(range_caps())
        .open_range_action(OpenRangeAction::Fail {
            error: StorageError::Unavailable("backend down".into()),
        })
        .build();
    let h = Harness::new(mock);
    let content = pattern(64);
    h.seed_remote_file("/broken.bin", &content);
    let handle = h.open("\\broken.bin").expect("open");

    let events = Arc::new(Mutex::new(Vec::new()));
    let status = tracing::subscriber::with_default(
        LevelCapture {
            events: Arc::clone(&events),
        },
        || {
            let mut buf = [0u8; 8];
            status_of(h.fs.read(&handle, &mut buf, 0))
        },
    );
    assert_eq!(status, 0xC000_0185, "STATUS_IO_DEVICE_ERROR");

    let events = events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        events
            .iter()
            .any(|line| line.starts_with("ERROR") && line.contains("STATUS_IO_DEVICE_ERROR")),
        "the EIO fallback must log: {events:?}"
    );
}

// --------------------------------------------- K47 (Phase 3.5-a E3) ------

/// aead_v2 container geometry: the [`cloudkit_core::crypto::AeadV2::new`]
/// default crypto chunk (1 MiB), the 16-byte GCM tag per chunk and the
/// 34-byte container header. The seeded plaintext spans 2 full chunks
/// plus a non-exact tail (3 chunks), so windowed reads cross
/// crypto-chunk boundaries by construction (the 16-byte face windows are
/// tiny NEXT to the crypto chunks — the two window layers are
/// independent by design).
const V2_CHUNK: u64 = 1024 * 1024;
const V2_TAG: u64 = 16;
const V2_HEADER: u64 = 34;
const V2_TAIL: u64 = 700_000;

/// K47: an aead_v2 row over a range-capable transport opens as the
/// WINDOW streaming handle — the behavioral proof the K33 gate used for
/// the plaintext arm, now on the encrypted arm: reads are byte-exact
/// across crypto-chunk boundaries, every inner call is the 34-byte
/// header pull or a bounded ciphertext span, and the full-open face is
/// never touched (hydrate answers with the same bytes — the call shape
/// is the stream/hydrate discriminator).
#[test]
fn aead_v2_rows_stream_windows_across_crypto_chunks() {
    let h = Harness::with(
        MockTransport::builder().capabilities(range_caps()).build(),
        Some("pw"),
        TEST_WINDOW,
        TEST_GRACE,
        TEST_CAPACITY,
    );
    let plain_len = (2 * V2_CHUNK + V2_TAIL) as usize;
    let plaintext = pattern(plain_len);
    h.seed_aead_v2_file("/vault.bin", &plaintext, "pw");

    let handle = h.open("\\vault.bin").expect("open the aead_v2 row");

    // ① The first read: byte-exact, and the inner calls are exactly the
    //    wrapper's header pull + the one-chunk ciphertext span.
    assert_eq!(h.read(&handle, 0, 8).expect("read"), &plaintext[0..8]);
    let chunk = V2_CHUNK + V2_TAG;
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, V2_HEADER), (V2_HEADER, chunk)],
        "streaming shape: header first, then the chunk-0 span"
    );
    assert!(
        h.mock.open_calls().is_empty(),
        "the window arm must never full-open (hydrate discriminator)"
    );

    // ② A read crossing the crypto-chunk boundary (offset CS-8, 40 bytes
    //    spanning into chunk 1): byte-exact across the seam, served by
    //    three 16-byte face windows whose spans cover chunks 0..=1 then
    //    chunk 1 twice — no re-pulled header, no whole-container pull.
    assert_eq!(
        h.read(&handle, V2_CHUNK - 8, 40).expect("cross-chunk read"),
        &plaintext[(V2_CHUNK - 8) as usize..(V2_CHUNK + 32) as usize],
        "byte-exact across the crypto-chunk boundary"
    );
    assert_eq!(
        h.mock.open_range_calls(),
        vec![
            (0, V2_HEADER),
            (V2_HEADER, chunk),
            (V2_HEADER, 2 * chunk),
            (V2_HEADER + chunk, chunk),
            (V2_HEADER + chunk, chunk),
        ],
        "cross-chunk shape: one span per face window, coordinates in the \
         ciphertext container"
    );
    assert!(h.mock.open_calls().is_empty());
}

/// K47 on the K41 grace path: open → close → immediate reopen reuses the
/// parked read state, so the reopened handle serves without a single new
/// inner call — the 34-byte header (and its PBKDF2 derivation) is pulled
/// exactly once across the churn. The exact-call-vector assert is what
/// makes this fail under a hydrate regression: there the vector is empty
/// AND the full-open face fires instead.
#[test]
fn aead_v2_grace_reopen_reuses_the_parked_state_header_pulled_once() {
    let h = Harness::with(
        MockTransport::builder().capabilities(range_caps()).build(),
        Some("pw"),
        TEST_WINDOW,
        TEST_GRACE,
        TEST_CAPACITY,
    );
    let plain_len = (2 * V2_CHUNK + V2_TAIL) as usize;
    let plaintext = pattern(plain_len);
    h.seed_aead_v2_file("/vault.bin", &plaintext, "pw");

    let first = h.open("\\vault.bin").expect("open");
    assert_eq!(h.read(&first, 0, 8).expect("read"), &plaintext[0..8]);
    let chunk = V2_CHUNK + V2_TAG;
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, V2_HEADER), (V2_HEADER, chunk)],
        "the first read pulls the header exactly once + one span"
    );
    h.fs.close(first);

    let second = h.open("\\vault.bin").expect("reopen inside the grace");
    assert_eq!(
        h.read(&second, 0, 8).expect("read after reopen"),
        &plaintext[0..8]
    );
    assert_eq!(
        h.mock.open_range_calls(),
        vec![(0, V2_HEADER), (V2_HEADER, chunk)],
        "the grace reopen must reuse the parked state — no new header \
         pull, no new span"
    );
    assert!(
        h.mock.open_calls().is_empty(),
        "the reopened handle still never hydrates"
    );
}
