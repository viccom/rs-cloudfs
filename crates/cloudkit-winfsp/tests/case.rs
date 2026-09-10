//! Case-insensitive path resolution (Windows semantics) on the WinFsp
//! adapter's metadata lookups.
//!
//! WinFsp's FSD resolves names case-insensitively and hands the adapter
//! whatever case form the caller (or its own upcase normalization for
//! rename sources) produced — e.g. a rename of `\_fsdbg_a.tmp` arrives
//! as `\_FSDBG_A.TMP`. The adapter must resolve any case spelling to
//! the canonical row (exact match first, then a case-insensitive scan
//! of the parent listing), the way the WebDAV face tolerates Explorer's
//! mixed-case paths through dav-server's own folding.
//!
//! Everything here runs WITHOUT WinFsp installed: only the DLL-free
//! callback halves are touched.

#![cfg(all(windows, feature = "winfsp"))]

use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::{Capabilities, CloudTransport};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_winfsp::fs::CloudFs;
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::FileSystemContext;
use winfsp::U16CStr;

/// `FspCleanupDelete` as the FSD passes it (winfsp.h:151).
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
        hydrate_timeout: Duration::from_secs(60),
    }
}

/// Range-capable mock (uploaded rows open on the streaming arm without
/// remote traffic).
fn range_caps() -> Capabilities {
    Capabilities {
        range_read: true,
        ..Capabilities::default()
    }
}

struct Harness {
    db: Arc<MetaDatabase>,
    vfs: Arc<Vfs>,
    fs: CloudFs,
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime");
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("temp db"));
        let cache = cloudkit_core::cache::CacheManager::new(dir.path().join("cache"), 1 << 30);
        let mock = Arc::new(MockTransport::builder().capabilities(range_caps()).build());
        let transport: Arc<dyn CloudTransport> = mock;
        let _guard = rt.enter();
        let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg()));
        Self {
            db,
            vfs: Arc::clone(&vfs),
            fs: CloudFs::new(vfs, rt.handle().clone(), "cydrive-test"),
            _rt: rt,
            _dir: dir,
        }
    }

    /// An uploaded file row with a local cache copy, so both the
    /// metadata arm and the streaming open work without the network.
    fn seed_uploaded(&self, rel: &str, bytes: &[u8]) {
        let path = RelPath::new(rel).expect("valid rel");
        let upsert = FileUpsert {
            rel_path: rel.to_string(),
            name: path.name().to_string(),
            parent_dir: path
                .parent()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| "/".to_string()),
            size: bytes.len() as i64,
            mtime: 1_700_000_000.0,
            sha256: None,
            is_dir: false,
            telegram_msg_id: None,
            is_uploaded: true,
            is_cached: true,
            is_encrypted: false,
            chunk_count: 0,
            mime_type: None,
        };
        self.db.upsert_file(&upsert).expect("seed row");
        let local = self.vfs.local_path(&path);
        std::fs::create_dir_all(local.parent().expect("parent")).expect("dirs");
        std::fs::write(&local, bytes).expect("seed cache copy");
    }

    fn row(&self, rel: &str) -> Option<cloudkit_core::database::FileRecord> {
        self.db.get_file(rel).expect("db read")
    }
}

/// The rename source arrives in a different case (the FSD's upcased
/// form): the row must resolve and the rename must land on the
/// canonical spelling.
#[test]
fn rename_resolves_upcased_source_to_canonical_row() {
    let h = Harness::new();
    h.seed_uploaded("/Mixed_Case.txt", b"case-by-case");

    let handle =
        h.fs.open_handle(&RelPath::new("/Mixed_Case.txt").expect("valid rel"))
            .expect("open canonical");

    h.fs.rename_entry(
        &handle,
        &RelPath::new("/MIXED_CASE.TXT").expect("valid rel"),
        &RelPath::new("/renamed_mixed.txt").expect("valid rel"),
        false,
    )
    .expect("case-insensitive rename");

    assert!(h.row("/renamed_mixed.txt").is_some(), "renamed row exists");
    assert!(h.row("/Mixed_Case.txt").is_none(), "old name is gone");
}

/// A plain open with a differently-cased name resolves the row too
/// (Explorer and Win32 apps pass arbitrary case forms).
#[test]
fn open_resolves_case_insensitive_variant() {
    let h = Harness::new();
    h.seed_uploaded("/Mixed_Case.txt", b"case-by-case");

    let handle =
        h.fs.open_handle(&RelPath::new("/mixed_case.TXT").expect("valid rel"))
            .expect("open case variant");
    assert_eq!(handle.meta().size, b"case-by-case".len() as u64);
}

/// A genuinely absent name stays NotFound — the fallback must not turn
/// misses into phantom rows.
#[test]
fn absent_name_stays_not_found() {
    let h = Harness::new();
    h.seed_uploaded("/Mixed_Case.txt", b"case-by-case");

    let err =
        h.fs.open_handle(&RelPath::new("/not_there.txt").expect("valid rel"))
            .err()
            .expect("must fail");
    // STATUS_OBJECT_NAME_NOT_FOUND.
    assert_eq!(err.to_ntstatus(), 0xC0000034_u32 as i32);
}

/// The cleanup delete arm gets the FSD's upcased spelling too: the row
/// must resolve and the delete must land (a silent not-found "success"
/// would leave the entry forever).
#[test]
fn cleanup_delete_resolves_upcased_name() {
    let h = Harness::new();
    h.seed_uploaded("/Mixed_Case.txt", b"case-by-case");

    let handle =
        h.fs.open_handle(&RelPath::new("/Mixed_Case.txt").expect("valid rel"))
            .expect("open canonical");

    // The FSD cleanup callback with `FspCleanupDelete` and an upcased
    // name, the shape `cmd del` / `Remove-Item` produce.
    let wide: Vec<u16> = "\\MIXED_CASE.TXT"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name = U16CStr::from_slice(&wide).expect("name");
    h.fs.cleanup(&handle, Some(name), CLEANUP_DELETE);

    assert!(h.row("/Mixed_Case.txt").is_none(), "row deleted");
}
