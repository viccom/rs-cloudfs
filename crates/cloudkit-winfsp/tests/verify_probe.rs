//! Regression tests for the external review findings
//! (docs/reports/2026-09-11-phase3-winfsp-enc-review.md), part of the
//! suite since the review-fixes batches (docs/plans/2026-09-11-review-fixes.md).
//!
//! The RB1 findings below assert the FIXED behavior (they were written
//! red first against the buggy tree, then turned green by the fix):
//! - C1: a case-only rename is a legal rename — row and cache copy move
//!   with it, nothing is destroyed or refused;
//! - H1: a grace-table entry whose size disagrees with the fresh row is
//!   stale (delete + recreate behind the same path) and is discarded.
//!
//! The remaining probes (H2/H3/H4/M1/M2/M3/M6) still assert the UNFIXED
//! bug behavior — they pass against the current tree on purpose and are
//! flipped to the fixed behavior by their own batches (RB2/RB3), the
//! same red-first way.
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

    fn staged_path(&self, rel: &str) -> PathBuf {
        let local = self.local_path(rel);
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .expect("file name");
        local.with_file_name(format!(".{name}.tmp"))
    }

    fn enqueued(&self) -> u64 {
        self.vfs.queue_stats().enqueued
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

/// A row whose stored rel_path is not a valid vpath (reachable only from an
/// external/legacy writer — Python baseline does no validation) makes the
/// open path PANIC at fs.rs:805 once resolve_row's parent-scan arm finds it.
#[test]
fn h2_open_panics_on_a_row_with_a_non_vpath_rel_path() {
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
        match h.open("/PATH") {
            Ok(_) => None,
            Err(error) => Some(format!("returned Err instead: {error:?}")),
        }
    }));
    std::panic::set_hook(prev_hook);
    let payload = outcome.expect_err("the open callback must panic");

    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string());
    println!("[H2] open() panicked with: {msg:?}");
    assert!(
        msg.starts_with("db rows carry canonical rel paths"),
        "H2 CONFIRMED: fs.rs:805 panic reached from the open callback (got {msg:?})"
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
// H4 — cleanup commit failure is silent AND the "discarded" comment lies
// =====================================================================

/// Close the upload queue first (enqueue will fail with QueueClosed). The
/// staged commit then: renames the sibling onto the FINAL cache path (bytes
/// survive there), upserts the pending row, fails the enqueue, removes the
/// (already renamed away) sibling as a no-op, and logs "the write was
/// discarded" — while the bytes and the row are both on disk.
#[test]
fn h4_failed_commit_keeps_the_bytes_at_the_final_path_and_the_row_pending() {
    let h = Harness::new();

    let a = h.create_file("/stuck.bin").expect("create");
    h.write(&a, 0, b"payload-bytes").expect("write");

    // Drain + close the queue so commit_put's enqueue fails.
    h.fs.bridge().block_on(h.vfs.shutdown());

    h.cleanup(&a);
    h.fs.close(a);

    let local = h.local_path("/stuck.bin");
    let row = h.row("/stuck.bin");
    println!(
        "[H4] local copy exists at FINAL path = {}, content = {:?}",
        local.exists(),
        fs::read(&local)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    );
    println!(
        "[H4] row = {:?}",
        row.as_ref().map(|r| (r.size, r.is_uploaded, r.is_cached))
    );
    println!(
        "[H4] staging sibling exists = {}",
        h.staged_path("/stuck.bin").exists()
    );
    println!("[H4] enqueued jobs = {}", h.enqueued());

    assert!(
        local.exists(),
        "H4 CONFIRMED (comment drift): the write was NOT discarded — the bytes \
         sit at the final cache path"
    );
    let row = row.expect("the pending row exists");
    assert_eq!(row.size, 13);
    assert!(
        !row.is_uploaded,
        "the row is pending forever (no job enqueued)"
    );
    assert_eq!(h.enqueued(), 0, "no upload job was ever accepted");
}

// =====================================================================
// M1 — the staging sibling of /foo is /.foo.tmp, someone else's cache path
// =====================================================================

/// A PENDING row named /.foo.tmp has its only copy of the bytes in the cache
/// tree; creating /foo stages at the same path and truncates it.
#[test]
fn m1_creating_foo_truncates_the_cache_copy_of_the_hidden_row_dot_foo_dot_tmp() {
    let h = Harness::new();
    // Pending row (uploaded=false): the cache copy is the ONLY copy.
    h.seed_row("/.foo.tmp", 6, false, None);
    let victim = h.seed_cache_copy("/.foo.tmp", b"SECRET");
    println!(
        "[M1] /.foo.tmp cache copy before = {:?} ({} bytes)",
        fs::read(&victim)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        fs::metadata(&victim).map(|m| m.len()).unwrap_or(0)
    );

    let _foo = h.create_file("/foo").expect("create /foo stages .foo.tmp");
    let after = fs::read(&victim).ok().map(|b| b.len());
    println!(
        "[M1] /.foo.tmp cache copy after create(/foo) = {after:?} bytes (staged sibling of /foo = {:?})",
        h.staged_path("/foo")
    );
    h.fs.close(_foo);
    assert_eq!(
        after,
        Some(0),
        "M1 CONFIRMED: create(/foo) truncated /.foo.tmp's cache copy — the only \
         copy of a pending row's bytes"
    );
}

// =====================================================================
// M2 — reserved device names / trailing dot & space survive vpath + cache
// =====================================================================

#[test]
fn m2_reserved_and_degenerate_names_in_vpath_and_on_disk() {
    let h = Harness::new();
    let cache_root = h.local_path("/");

    for name in [
        "/CON", "/nul", "/nul.txt", "/aux.txt", "/foo.", "/foo ", "/foo. ",
    ] {
        let rel = RelPath::new(name);
        println!(
            "[M2] RelPath::new({name:?}) -> {} (path {:?})",
            rel.is_ok(),
            rel.as_ref().ok().map(|r| r.as_str().to_string())
        );
    }

    // What std::fs actually does with the mapped paths under the cache root.
    for name in ["/nul", "/nul.txt", "/foo.", "/foo ", "/foo. "] {
        let rel = RelPath::new(name).expect("vpath accepts it");
        let local = h.vfs.local_path(&rel);
        let write = fs::write(&local, b"data1234");
        let exists = local.exists();
        let meta = fs::metadata(&local).map(|m| m.len()).ok();
        println!(
            "[M2] name={name:?} local={:?} write={write:?} exists={exists} metadata_len={meta:?}",
            local
        );
    }

    // What actually landed in the cache root directory.
    let mut names: Vec<String> = fs::read_dir(&cache_root)
        .expect("read cache root")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    println!("[M2] cache root now holds: {names:?}");

    // Cross-name contamination: a probe of the virtual "/foo." (an `is_cached`
    // probe maps it to <root>/foo. which Windows normalizes to <root>/foo).
    if names.iter().any(|n| n == "foo") {
        let probe = fs::metadata(h.local_path("/foo.")).map(|m| m.len()).ok();
        println!(
            "[M2] metadata(local_path(\"/foo.\")) -> {probe:?} (normalized onto the `foo` file)"
        );
        assert_eq!(
            probe,
            Some(8),
            "M2 CONFIRMED (trailing-dot aliasing): /foo. and /foo are the same disk file"
        );
    }
    let _ = names; // silence unused warnings on non-NTFS hosts
}

// =====================================================================
// M3 — a >255 wide-char name breaks DirInfo and the whole enumeration
// =====================================================================

#[test]
fn m3_overlong_name_fills_dir_info_with_an_error() {
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
    println!(
        "[M3] prepare_enumeration listed {} entries: {:?}",
        entries.len(),
        entries
            .iter()
            .map(|e: &DirEntry| e.name.len())
            .collect::<Vec<_>>()
    );

    for child in &entries {
        let mut entry = DirInfo::<255>::new();
        let filled = cloudkit_winfsp::fs::fill_dir_info(&mut entry, child);
        match filled {
            Ok(()) => println!("[M3] name len {} -> Ok", child.name.len()),
            Err(error) => println!(
                "[M3] name len {} -> Err {:?} (ntstatus {:#010x})",
                child.name.len(),
                error,
                error.to_ntstatus() as u32
            ),
        }
    }

    // The long entry must be the one that fails; the normal one must pass.
    let mut results = Vec::new();
    for child in &entries {
        let mut entry = DirInfo::<255>::new();
        results.push((
            child.name.len(),
            cloudkit_winfsp::fs::fill_dir_info(&mut entry, child).is_ok(),
        ));
    }
    assert!(
        results.contains(&(300, false)),
        "M3 CONFIRMED: the 300-char entry fails fill_dir_info (winfsp-rs caps the buffer at 255)"
    );
    assert!(
        results.contains(&(6, true)),
        "the normal entry still fills fine"
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
