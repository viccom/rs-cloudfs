//! Regression tests for the external review findings
//! (docs/reports/2026-09-11-phase3-winfsp-enc-review.md), part of the
//! suite since the review-fixes batches (docs/plans/2026-09-11-review-fixes.md).
//!
//! The RB1/RB2 findings below assert the FIXED behavior (each was written
//! red first against the buggy tree, then turned green by its fix):
//! - C1: a case-only rename is a legal rename — row and cache copy move
//!   with it, nothing is destroyed or refused;
//! - H1: a grace-table entry whose size disagrees with the fresh row is
//!   stale (delete + recreate behind the same path) and is discarded;
//! - H2: a db row with a non-vpath rel_path answers an error status, the
//!   callback never panics;
//! - H4: a failed cleanup commit keeps the bytes and the pending row, and
//!   the failure log says exactly that (never "the write was discarded");
//! - M1: /foo's staging sibling can never be another row's cache path
//!   (random sibling segment);
//! - M2: reserved device names and trailing dot/space map onto real,
//!   distinct cache files;
//! - M3: an overlong (>255 UTF-16 units) name is skipped by the
//!   enumeration instead of failing the whole directory.
//!
//! The remaining probes (H3/M6) still assert the UNFIXED bug behavior —
//! they pass against the current tree on purpose and are flipped by their
//! own batches (H3 has no batch yet, M6 is RB4), the same red-first way.
//!
//! Run:
//!   CARGO_TARGET_DIR='E:/Rs_Codes/rs-cloudfs/target' \
//!   LIBCLANG_PATH='D:/Python312/Lib/site-packages/clang/native' \
//!   cargo test -p cloudkit-winfsp --features winfsp --test verify_probe -- --nocapture

#![cfg(all(windows, feature = "winfsp"))]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{Capabilities, CloudTransport};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_winfsp::fs::{CloudFs, DirEntry, Handle};
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{DirInfo, FileInfo, FileSystemContext, OpenFileInfo};
use winfsp::{FspError, U16CStr};

// Driven only by the pending review-fix probes (RB2+); silenced until
// their batches land.
#[allow(dead_code)]
const CLEANUP_DELETE: u32 = FspCleanupFlags::FspCleanupDelete as u32;

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

fn range_caps() -> Capabilities {
    Capabilities {
        range_read: true,
        inbound: true,
        chat: true,
        ..Capabilities::none()
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    /// Read by the pending review-fix probes (RB2+); silenced until their
    /// batches land.
    #[allow(dead_code)]
    mock: Arc<MockTransport>,
    vfs: Arc<Vfs>,
    fs: CloudFs,
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
        let mock = Arc::new(MockTransport::builder().capabilities(range_caps()).build());
        let transport: Arc<dyn CloudTransport> = mock.clone();
        let _guard = rt.enter();
        let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg()));
        Self {
            _dir: dir,
            db,
            mock,
            fs: CloudFs::new(vfs.clone(), rt.handle().clone(), "cydrive-probe"),
            vfs,
            _rt: rt,
        }
    }

    fn create_file(&self, name: &str) -> winfsp::Result<Handle> {
        let wide = fsd_path(name);
        let mut info: OpenFileInfo = unsafe { std::mem::zeroed() };
        // FILE_CREATE (2) + FILE_NON_DIRECTORY_FILE (0x40)
        self.fs.create(
            U16CStr::from_slice(&wide).expect("name"),
            (2u32) << 24 | 0x40,
            0x0012_0089,
            0,
            None,
            0,
            None,
            false,
            &mut info,
        )
    }

    fn open(&self, name: &str) -> winfsp::Result<Handle> {
        let wide = fsd_path(name);
        let mut info: OpenFileInfo = unsafe { std::mem::zeroed() };
        self.fs
            .open(U16CStr::from_slice(&wide).expect("name"), 0, 0, &mut info)
    }

    fn write(&self, handle: &Handle, offset: u64, bytes: &[u8]) -> winfsp::Result<u32> {
        let mut info = FileInfo::default();
        self.fs
            .write(handle, bytes, offset, false, false, &mut info)
    }

    fn read(&self, handle: &Handle, offset: u64, len: usize) -> winfsp::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let filled = self.fs.read(handle, &mut buf, offset)? as usize;
        buf.truncate(filled);
        Ok(buf)
    }

    fn cleanup(&self, handle: &Handle) {
        self.fs.cleanup(handle, None, 0);
    }

    /// Driven only by the pending review-fix probes (RB2+); silenced until
    /// their batches land.
    #[allow(dead_code)]
    fn cleanup_delete(&self, handle: &Handle, name: &str) {
        let wide = fsd_path(name);
        self.fs.cleanup(
            handle,
            Some(U16CStr::from_slice(&wide).expect("name")),
            CLEANUP_DELETE,
        );
    }

    fn rename(&self, handle: &Handle, from: &str, to: &str, replace: bool) -> winfsp::Result<()> {
        let from_wide = fsd_path(from);
        let to_wide = fsd_path(to);
        self.fs.rename(
            handle,
            U16CStr::from_slice(&from_wide).expect("from"),
            U16CStr::from_slice(&to_wide).expect("to"),
            replace,
        )
    }

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

    /// Seeds a row whose stored rel_path is NOT a valid vpath (the shape only
    /// an external/legacy writer can produce — every in-tree writer validates).
    fn seed_raw_row(&self, rel_path: &str, name: &str, parent_dir: &str) {
        self.db
            .upsert_file(&FileUpsert {
                rel_path: rel_path.to_string(),
                name: name.to_string(),
                parent_dir: parent_dir.to_string(),
                size: 4,
                mtime: 1_700_000_000.0,
                sha256: None,
                is_dir: false,
                telegram_msg_id: Some(1),
                is_uploaded: true,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 1,
                mime_type: None,
            })
            .expect("seed raw row");
    }

    fn seed_cache_copy(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let rel_path = RelPath::new(rel).expect("valid rel path");
        let local = self.vfs.local_path(&rel_path);
        fs::create_dir_all(local.parent().expect("cache parent dir")).expect("create cache dirs");
        fs::write(&local, bytes).expect("write cache copy");
        local
    }

    fn seed_uploaded_file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        self.seed_row(rel, bytes.len() as i64, true, Some(1));
        self.seed_cache_copy(rel, bytes)
    }

    fn row(&self, rel: &str) -> Option<cloudkit_core::database::FileRecord> {
        self.db.get_file(rel).expect("db read")
    }

    fn local_path(&self, rel: &str) -> PathBuf {
        self.vfs
            .local_path(&RelPath::new(rel).expect("valid rel path"))
    }

    fn enqueued(&self) -> u64 {
        self.vfs.queue_stats().enqueued
    }
}

/// Captures THIS thread's tracing events into a shared buffer (review H4's
/// log-semantics assertions). `set_default` is thread-local, so the guard
/// only affects the calling test; events emitted inside the bridge's
/// runtime workers are not captured — every log asserted here fires on the
/// caller's thread.
fn capture_logs() -> (
    tracing::subscriber::DefaultGuard,
    Arc<std::sync::Mutex<Vec<u8>>>,
) {
    let buffer: Arc<std::sync::Mutex<Vec<u8>>> = Arc::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_writer(CaptureWriter(Arc::clone(&buffer)))
        .finish();
    (tracing::subscriber::set_default(subscriber), buffer)
}

/// A `MakeWriter` fanning formatted tracing events into the probe's buffer.
#[derive(Clone)]
struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer mutex")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn fsd_path(name: &str) -> Vec<u16> {
    name.replace('/', "\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn status_of<T>(result: winfsp::Result<T>) -> u32 {
    match result {
        Ok(_) => panic!("expected an error status, got success"),
        Err(FspError::NTSTATUS(status)) => status as u32,
        Err(other) => panic!("expected an NTSTATUS carrier, got {other:?}"),
    }
}

// =====================================================================
// C1 — case-only rename
// =====================================================================

/// C1 direction 1: from is a case variant, to is the canonical spelling,
/// replace=true. A case-only rename is a legal rename: the row (identity,
/// size, remote linkage) and the cache copy survive untouched.
#[test]
fn c1_case_rename_to_canonical_spelling_with_replace_keeps_row_and_cache() {
    let h = Harness::new();
    h.seed_uploaded_file("/a.txt", b"precious-bytes");

    let handle =
        h.fs.open_handle(&RelPath::new("/a.txt").expect("rel"))
            .expect("open handle");

    let result = h.rename(&handle, "/A.TXT", "/a.txt", true);
    println!("[C1-dir1] rename(from=/A.TXT, to=/a.txt, replace=true) -> {result:?}");

    assert!(
        result.is_ok(),
        "a case-only rename is a legal rename, replace flag or not"
    );
    let survived = h
        .row("/a.txt")
        .expect("the row must survive a case-only rename");
    assert_eq!(survived.size, 14, "the row keeps its size");
    assert_eq!(
        survived.telegram_msg_id,
        Some(1),
        "the row keeps its remote linkage"
    );
    assert!(survived.is_uploaded, "the row stays uploaded");
    assert!(
        h.row("/A.TXT").is_none(),
        "no duplicate row appears at the variant spelling"
    );
    assert!(
        h.local_path("/a.txt").exists(),
        "the local cache copy must survive a case-only rename"
    );
    assert_eq!(
        fs::read(h.local_path("/a.txt")).expect("read the cache copy"),
        b"precious-bytes",
        "the cache copy keeps its bytes"
    );
}

/// C1 direction 1, replace=false: the same-name case rename is NOT a
/// destination collision (there is no distinct destination to replace) —
/// it succeeds like any case-only rename.
#[test]
fn c1_case_rename_to_canonical_spelling_without_replace_succeeds() {
    let h = Harness::new();
    h.seed_uploaded_file("/a.txt", b"precious-bytes");

    let handle =
        h.fs.open_handle(&RelPath::new("/a.txt").expect("rel"))
            .expect("open handle");

    let result = h.rename(&handle, "/A.TXT", "/a.txt", false);
    println!("[C1-dir1-noreplace] rename(from=/A.TXT, to=/a.txt, replace=false) -> {result:?}");
    assert!(
        result.is_ok(),
        "a case-only rename is not a destination collision"
    );
    let survived = h.row("/a.txt").expect("the row survives the rename");
    assert_eq!(survived.size, 14, "the row keeps its size");
    assert!(
        h.local_path("/a.txt").exists(),
        "the cache copy survives the rename"
    );
}

/// C1 direction 2 (the Explorer-typical shape: FSD delivers the source
/// upcased — see decisions 6b25ede — and the user types a differently-cased
/// name): the rename moves the row to the new spelling and the local cache
/// copy follows it intact — nothing is silently destroyed.
#[test]
fn c1_case_rename_to_variant_spelling_moves_row_and_keeps_cache() {
    let h = Harness::new();
    h.seed_uploaded_file("/Mixed.txt", b"case-by-case");

    let handle =
        h.fs.open_handle(&RelPath::new("/Mixed.txt").expect("rel"))
            .expect("open handle");

    let result = h.rename(&handle, "/MIXED.TXT", "/mixed.txt", true);
    println!("[C1-dir2] rename(from=/MIXED.TXT, to=/mixed.txt, replace=true) -> {result:?}");
    assert!(result.is_ok(), "the rename answered success");

    let survived = h
        .row("/mixed.txt")
        .expect("the row moved to the new spelling");
    assert_eq!(survived.size, 12, "the row keeps its size");
    assert_eq!(
        survived.telegram_msg_id,
        Some(1),
        "the row keeps its remote linkage"
    );
    assert!(h.row("/Mixed.txt").is_none(), "the old spelling is gone");

    let copy = fs::read(h.local_path("/mixed.txt"))
        .expect("the local cache copy must survive a case-only rename");
    assert_eq!(copy, b"case-by-case", "the cache copy keeps its bytes");
}

/// C1 companion: because the FSD delivers rename sources UPCASED (repo's own
/// real-machine evidence), the common "make the name uppercase" rename
/// arrives with from == to as raw strings. That is a case-only rename, not
/// an access denial: the row moves to the uppercase spelling and the cache
/// copy's case flips on disk.
#[test]
fn c1_case_rename_to_all_uppercase_succeeds_from_the_upcased_fsd_form() {
    let h = Harness::new();
    h.seed_uploaded_file("/a.txt", b"precious-bytes");

    let handle =
        h.fs.open_handle(&RelPath::new("/a.txt").expect("rel"))
            .expect("open handle");

    let result = h.rename(&handle, "/A.TXT", "/A.TXT", true);
    println!("[C1-upcase] rename(/A.TXT -> /A.TXT) -> {result:?}");
    assert!(
        result.is_ok(),
        "an upcased-source rename (from == to as raw strings) is a case-only \
         rename, not an access denial"
    );
    assert!(h.row("/a.txt").is_none(), "the old spelling is gone");
    let survived = h
        .row("/A.TXT")
        .expect("the row moved to the uppercase spelling");
    assert_eq!(survived.size, 14, "the row keeps its size");
    assert_eq!(
        survived.telegram_msg_id,
        Some(1),
        "the row keeps its remote linkage"
    );
    let copy = fs::read(h.local_path("/A.TXT"))
        .expect("the local cache copy must survive (its case flips with the rename)");
    assert_eq!(copy, b"precious-bytes", "the cache copy keeps its bytes");
}

// =====================================================================
// H1 — grace table stale reuse after delete + recreate
// =====================================================================

/// Park the read state of an 8-byte file, then delete the row and recreate
/// the same path with 64 bytes. A reopen inside the grace window must serve
/// the RECREATED content: the parked state's size predates the recreate, so
/// it is stale and discarded, never reused.
#[test]
fn h1_reopen_after_delete_and_recreate_serves_the_new_content() {
    let h = Harness::new();

    let old = vec![0xAAu8; 8];
    let new = vec![0xBBu8; 64];
    h.seed_uploaded_file("/victim.bin", &old);

    // Open (eagerly acquires a LocalReader over the 8-byte cache copy) and
    // close (parks the state for DEFAULT_HANDLE_GRACE = 5s).
    let first = h.open("/victim.bin").expect("open");
    assert_eq!(h.read(&first, 0, 8).expect("read old"), old);
    h.fs.close(first);

    // Delete + recreate at the same path (the row via the db, the cache copy
    // in place — the shapes a delete/recreate cycle produces).
    h.db.delete_file("/victim.bin").expect("delete row");
    h.seed_row("/victim.bin", new.len() as i64, true, Some(2));
    h.seed_cache_copy("/victim.bin", &new);

    // Immediate reopen: within the grace window.
    let second = h.open("/victim.bin").expect("reopen");
    let meta_size = second.meta().size;
    let served = h.read(&second, 0, 128).expect("read after recreate");
    h.fs.close(second);

    assert_eq!(meta_size, 64, "the fresh row carries the new size");
    assert_eq!(
        served, new,
        "H1 FIXED: a reopen inside the grace window must serve the recreated \
         file's content, never the parked state's stale EOF"
    );
}

/// H1 variant with the EXACT FSD callback order: cleanup (commit point, must
/// not touch the read state) THEN close (park). The stale parked state must
/// still be discarded on a reopen that follows a delete + recreate.
#[test]
fn h1_stale_grace_state_is_discarded_through_the_cleanup_then_close_lifecycle() {
    let h = Harness::new();

    let old = vec![0xAAu8; 8];
    let new = vec![0xBBu8; 64];
    h.seed_uploaded_file("/victim2.bin", &old);

    let first = h.open("/victim2.bin").expect("open");
    assert_eq!(h.read(&first, 0, 8).expect("read old"), old);
    h.cleanup(&first); // the FSD always posts cleanup...
    h.fs.close(first); // ...then close parks the read state

    h.db.delete_file("/victim2.bin").expect("delete row");
    h.seed_row("/victim2.bin", new.len() as i64, true, Some(2));
    h.seed_cache_copy("/victim2.bin", &new);

    let second = h.open("/victim2.bin").expect("reopen");
    let meta_size = second.meta().size;
    let served = h.read(&second, 0, 128).expect("read after recreate");
    h.fs.close(second);

    assert_eq!(meta_size, 64, "the fresh row carries the new size");
    assert_eq!(
        served, new,
        "H1 FIXED (cleanup+close order): the parked state must not serve the \
         old EOF after a delete + recreate behind the same path"
    );
}

// =====================================================================
// H2 — `.expect` panics on a db row with a non-vpath rel_path
// =====================================================================

/// H2 FIXED: a row whose stored rel_path is not a valid vpath (reachable
/// only from an external/legacy writer — Python baseline does no
/// validation) must make the open callback answer
/// STATUS_OBJECT_NAME_INVALID, never panic.
#[test]
fn h2_open_answers_an_error_for_a_row_with_a_non_vpath_rel_path() {
    let h = Harness::new();
    // rel_path with a backslash can never come out of RelPath::new, and the
    // db layer does not validate; name/parent_dir make the parent-scan arm
    // of resolve_row find it for any case spelling of "path".
    h.seed_raw_row("bad\\path", "path", "/");

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the default backtrace
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The case-variant spelling forces resolve_row into the parent-scan
        // arm (exact get_file("/PATH") misses), which returns the bad row.
        h.open("/PATH")
    }));
    std::panic::set_hook(prev_hook);

    let reported = match outcome {
        Ok(Ok(_)) => "answered Ok".to_string(),
        Ok(Err(FspError::NTSTATUS(status))) => {
            format!("answered NTSTATUS {:#010x}", status as u32)
        }
        Ok(Err(other)) => format!("answered {other:?}"),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            format!("PANICKED with {msg:?}")
        }
    };
    println!("[H2] open(/PATH) with a dirty row -> {reported}");
    assert_eq!(
        reported, "answered NTSTATUS 0xc0000033",
        "H2 FIXED: the open callback must answer STATUS_OBJECT_NAME_INVALID \
         (0xC0000033) for a db row whose rel_path is not a valid vpath — \
         instead it {reported}"
    );
}

// =====================================================================
// H3 — rename into a path another handle holds staged (adapter-level)
// =====================================================================

/// Handle A stages bytes at /x (no row yet); handle B renames /src -> /x.
/// Every guard in rename_entry passes (the staged state of OTHER handles is
/// invisible), so B's row lands at /x — and A's cleanup then overwrites it.
#[test]
fn h3_rename_into_a_staged_path_overwrites_the_migrated_row_at_cleanup() {
    let h = Harness::new();
    h.seed_uploaded_file("/src", b"SOURCE-BYTES");

    // A: create /x, stage bytes, do NOT commit (no cleanup).
    let a = h.create_file("/x").expect("create /x");
    h.write(&a, 0, b"STAGED-NEW").expect("stage bytes");

    // B: open the source and rename it onto /x.
    let b =
        h.fs.open_handle(&RelPath::new("/src").expect("rel"))
            .expect("open /src");
    let result = h.rename(&b, "/src", "/x", false);
    println!("[H3] rename(/src -> /x, replace=false) while /x is staged -> {result:?}");
    assert!(result.is_ok(), "every adapter guard passed");

    let migrated = h.row("/x").expect("B's row landed at /x");
    println!(
        "[H3] after rename: row(/x).size={} msg_id={:?}, row(/src)={:?}",
        migrated.size,
        migrated.telegram_msg_id.map(|i| i.to_string()),
        h.row("/src").is_some()
    );

    // A's cleanup commits the staged bytes over B's migrated row.
    h.cleanup(&a);
    let final_row = h.row("/x").expect("row still at /x");
    let final_copy = fs::read(h.local_path("/x")).expect("cache copy");
    println!(
        "[H3] after A's cleanup: row(/x).size={} (staged len=10) msg_id={:?}, cache copy={:?}",
        final_row.size,
        final_row.telegram_msg_id.map(|i| i.to_string()),
        String::from_utf8_lossy(&final_copy)
    );
    h.fs.close(b);
    h.fs.close(a);

    assert_eq!(final_row.size, 10, "A's staged length owns the row");
    assert_eq!(final_copy, b"STAGED-NEW", "A's staged bytes own the cache");
    assert_eq!(
        final_row.telegram_msg_id,
        Some(1),
        "H3 CONFIRMED: the row is a chimera — A's size with B's remote msg id \
         (the SOURCE bytes are no longer reachable through the mount)"
    );
}

// =====================================================================
// H4 — cleanup commit failure semantics
// =====================================================================

/// H4 FIXED: with the queue closed (enqueue fails), the failed commit
/// keeps the bytes at the final cache path (the rename already happened),
/// leaves the row pending for the boot-time requeue, leaves no staging
/// sibling behind, and the failure log states exactly that — it never
/// claims "the write was discarded".
#[test]
fn h4_failed_commit_keeps_the_bytes_reports_truthfully_and_leaves_the_row_pending() {
    let h = Harness::new();

    let a = h.create_file("/stuck.bin").expect("create");
    h.write(&a, 0, b"payload-bytes").expect("write");

    // Drain + close the queue so commit_put's enqueue fails.
    h.fs.bridge().block_on(h.vfs.shutdown());

    let (log_guard, logs) = capture_logs();
    h.cleanup(&a);
    drop(log_guard);
    h.fs.close(a);
    let log = String::from_utf8_lossy(&logs.lock().expect("log buffer mutex")).into_owned();
    println!("[H4] failure log: {log:?}");

    // The bytes are kept at the final cache path.
    let local = h.local_path("/stuck.bin");
    let content = fs::read(&local);
    assert!(
        matches!(&content, Ok(bytes) if bytes == b"payload-bytes"),
        "H4 FIXED: a failed commit keeps the bytes at the final cache path; \
         read back {content:?} at {local:?}"
    );
    // The row stays pending — the boot-time requeue heals it.
    let row = h.row("/stuck.bin").expect("the pending row exists");
    assert!(
        !row.is_uploaded,
        "the row stays pending (no job was ever accepted)"
    );
    assert!(row.is_cached, "the cache copy backs the pending row");
    assert_eq!(h.enqueued(), 0, "no upload job was ever accepted");

    // No staging sibling survives under any spelling.
    let leftovers: Vec<String> = fs::read_dir(h.local_path("/"))
        .expect("read cache root")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no staging sibling may survive a consumed commit; found {leftovers:?}"
    );

    // The failure log must state the truthful semantics.
    assert!(
        log.contains("kept at the final cache path"),
        "H4 FIXED: the failure log must say the bytes were kept at the \
         final cache path; log: {log:?}"
    );
    assert!(
        log.contains("requeued at the next boot"),
        "the log must say the pending row is requeued at the next boot; \
         log: {log:?}"
    );
    assert!(
        !log.contains("discarded"),
        "H4 FIXED: the log must not claim the write was discarded — the \
         bytes and the row are both on disk; log: {log:?}"
    );
}

// =====================================================================
// M1 — /foo's staging sibling must never be another row's cache path
// =====================================================================

/// M1 FIXED: the staging sibling carries a random segment, so creating
/// /foo stages in its own private file — the cache copy of a pending row
/// named /.foo.tmp (the old deterministic sibling spelling) is untouched
/// through staging AND commit.
#[test]
fn m1_creating_foo_leaves_the_hidden_dot_foo_dot_tmp_row_copy_untouched() {
    let h = Harness::new();
    // Pending row (uploaded=false): the cache copy is the ONLY copy.
    h.seed_row("/.foo.tmp", 6, false, None);
    let victim = h.seed_cache_copy("/.foo.tmp", b"SECRET");

    let foo = h.create_file("/foo").expect("create /foo");
    // The staged sibling of /foo must be a private file: writes land and
    // read back without ever touching /.foo.tmp's bytes.
    h.write(&foo, 0, b"new-bytes").expect("stage bytes");
    let staged_read = h.read(&foo, 0, 9).expect("read your own writes");
    let victim_after_stage = fs::read(&victim);
    h.cleanup(&foo);
    h.fs.close(foo);
    let victim_after_commit = fs::read(&victim);

    assert_eq!(
        staged_read, b"new-bytes",
        "staging works on its own sibling"
    );
    assert!(
        matches!(&victim_after_stage, Ok(bytes) if bytes == b"SECRET"),
        "M1 FIXED: create(/foo) must stage in a sibling that cannot be \
         another row's cache path — /.foo.tmp's only copy read back as \
         {victim_after_stage:?} after staging"
    );
    assert!(
        matches!(&victim_after_commit, Ok(bytes) if bytes == b"SECRET"),
        "M1 FIXED: the commit must not disturb /.foo.tmp's copy either; \
         read back {victim_after_commit:?}"
    );
    assert!(
        h.row("/.foo.tmp").is_some(),
        "the hidden row is untouched by /foo's lifecycle"
    );
}

// =====================================================================
// M2 — reserved device names / trailing dot & space map to safe,
//      distinct cache files
// =====================================================================

/// M2 FIXED: `CacheManager::local_path` sanitizes the disk mapping only —
/// a reserved-stem name (`/nul.txt`) writes a REAL cache file whose bytes
/// read back, and the three distinct trailing dot/space vpaths map onto
/// three distinct disk files (the vpath and the db keep the original
/// spelling).
#[test]
fn m2_reserved_and_degenerate_names_map_to_real_distinct_cache_files() {
    let h = Harness::new();

    // Reserved device stem with an extension: the unmapped form
    // `nul.txt` writes into the NUL device — the write "succeeds" and the
    // file never exists, so every read re-downloads forever.
    let nul = RelPath::new("/nul.txt").expect("vpath accepts it");
    assert_eq!(nul.as_str(), "/nul.txt", "the vpath itself is untouched");
    let nul_local = h.vfs.local_path(&nul);
    fs::write(&nul_local, b"data1234").expect("write the cache copy");
    let nul_read = fs::read(&nul_local);
    assert!(
        matches!(&nul_read, Ok(bytes) if bytes == b"data1234"),
        "M2 FIXED: bytes written under /nul.txt must land in a real cache \
         file and read back; got {nul_read:?} at {nul_local:?}"
    );

    // Trailing dot / space variants are DISTINCT vpaths; each one's cache
    // copy must be its own disk file holding its own bytes (the unmapped
    // forms all normalize onto the same `foo` file — cross-row cache
    // pollution).
    let written: Vec<(&str, &[u8])> = vec![
        ("/foo", b"plain".as_slice()),
        ("/foo.", b"one".as_slice()),
        ("/foo ", b"two".as_slice()),
        ("/foo. ", b"three".as_slice()),
    ];
    for (name, bytes) in &written {
        let rel = RelPath::new(name).expect("vpath accepts it");
        fs::write(h.vfs.local_path(&rel), bytes).expect("write the cache copy");
    }
    for (name, bytes) in &written {
        let rel = RelPath::new(name).expect("vpath accepts it");
        let local = h.vfs.local_path(&rel);
        let read = fs::read(&local);
        assert!(
            matches!(&read, Ok(got) if got == *bytes),
            "M2 FIXED: the cache copy of {name:?} must live on its own disk \
             file; read back {read:?} (expected {bytes:?}) at {local:?}"
        );
    }
}

// =====================================================================
// M3 — a >255 wide-char name is skipped, never fails the enumeration
// =====================================================================

/// M3 FIXED: the enumeration of a directory holding a 300-wide-char name
/// SUCCEEDS — the overlong entry is skipped (`fill_dir_info` answers
/// `Ok(false)` for it, warn-logged) while the legal entries fill
/// (`Ok(true)`). The fill outcome is Debug-formatted, so the assertions
/// read identically before and after the fix.
#[test]
fn m3_overlong_name_is_skipped_without_failing_the_enumeration() {
    let h = Harness::new();
    let long_name = "A".repeat(300);
    h.seed_row(&format!("/{long_name}"), 4, true, Some(1));
    h.seed_row("/ok.txt", 4, true, Some(2));

    let root =
        h.fs.open_handle(&RelPath::new("/").expect("root"))
            .expect("root handle");
    let entries =
        h.fs.prepare_enumeration(&root, true)
            .expect("prepare_enumeration is fine")
            .expect("Some(entries)");
    // The listing itself still carries every row (the db is untouched —
    // only the enumeration fill decides enumerability).
    assert!(
        entries.iter().any(|e: &DirEntry| e.name.len() == 300),
        "the listing carries the overlong row"
    );

    let mut results: Vec<(usize, String)> = Vec::new();
    for child in &entries {
        let mut entry = DirInfo::<255>::new();
        let reported = match cloudkit_winfsp::fs::fill_dir_info(&mut entry, child) {
            Ok(written) => format!("ok({written:?})"),
            Err(error) => format!("err({error:?})"),
        };
        results.push((child.name.len(), reported));
    }
    println!("[M3] fill results: {results:?}");

    assert!(
        results.contains(&(300, "ok(false)".to_string())),
        "M3 FIXED: the 300-wide-char entry must be skipped (Ok(false)) \
         without an error; got {results:?}"
    );
    assert!(
        results.contains(&(6, "ok(true)".to_string())),
        "the legal entry still fills (Ok(true)); got {results:?}"
    );
    assert!(
        results
            .iter()
            .all(|(_, reported)| reported.starts_with("ok(")),
        "no entry may fail the directory enumeration; got {results:?}"
    );
    h.fs.close(root);
}

// =====================================================================
// M6 — open of /missing/file reports NAME_NOT_FOUND, not PATH_NOT_FOUND
// =====================================================================

#[test]
fn m6_open_under_a_missing_parent_reports_name_not_found() {
    let h = Harness::new();
    h.seed_uploaded_file("/real.txt", b"x");

    let status = status_of(h.open("/missing/file.txt"));
    println!(
        "[M6] open(/missing/file.txt) status = {status:#010x} \
         (0xC0000034=NAME_NOT_FOUND, Windows expects 0xC000003A=PATH_NOT_FOUND)"
    );
    assert_eq!(status, 0xC000_0034, "STATUS_OBJECT_NAME_NOT_FOUND");
}
