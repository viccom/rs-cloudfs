//! Integration tests for the WF1 readonly metadata face of [`CloudFs`].
//!
//! Contract under test (plan §3-WF1 + K44/K45):
//! - every metadata answer comes off the local `files` rows or the
//!   assembly-time volume snapshot — a full metadata walk (security,
//!   open, stat, list, volume) must leave the transport's call log
//!   completely empty;
//! - the stat data is real: exact sizes, attributes and FILETIME values
//!   from the seeded rows;
//! - directory entries count no trailing NUL into their `Size` (the
//!   spike's worst lesson — a NUL there makes the FSD re-enumerate
//!   forever).
//!
//! Everything here runs WITHOUT WinFsp installed: the DLL is only ever
//! reached through `DirBuffer`, which these tests never acquire. The
//! real enumeration glue (acquire/fill/read on a mounted volume) is
//! WF4's `#[ignore]` real-machine test.
#![cfg(all(windows, feature = "winfsp"))]

use std::sync::Arc;
use std::time::Duration;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::{FileUpsert, MetaDatabase};
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::mock::MockTransport;
use cloudkit_core::transport::Capabilities;
use cloudkit_core::transport::CloudTransport;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_winfsp::fs::{
    fill_dir_info, rel_from_winfsp, unix_to_filetime, CloudFs, DirEntry, Meta, VOLUME_HEADROOM,
};
use winfsp::filesystem::{DirInfo, FileInfo, FileSystemContext, VolumeInfo};
use winfsp::U16CStr;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0020;

/// 2023-11-14T22:13:20Z as FILETIME (Unix 1_700_000_000 * 1e7 + the
/// 1601->1970 offset) — pinned so a conversion drift cannot hide.
const T_1_700_000_000_FILETIME: u64 = 133_444_736_000_000_000;

/// VfsConfig for integration tests: tiny chunks, one worker, fast retry
/// (same shape the cloudkit-webdav adapter tests use).
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

/// Real temp environment: SQLite db + cache tree + pre-connected mock
/// transport + Vfs + the adapter under test (L5 test code may depend on
/// anything but drivers — R1).
struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    mock: Arc<MockTransport>,
    vfs: Arc<Vfs>,
    fs: CloudFs,
    rt: tokio::runtime::Runtime,
}

impl Harness {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
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
        // Vfs::new spawns its upload queue: a runtime must be in scope.
        let _guard = rt.enter();
        rt.block_on(mock.connect()).expect("pre-connect mock");
        let transport: Arc<dyn CloudTransport> = mock.clone();
        let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg()));
        let fs = CloudFs::new(vfs.clone(), rt.handle().clone(), "cydrive-test");
        Self {
            _dir: dir,
            db,
            mock,
            vfs,
            fs,
            rt,
        }
    }

    /// Inserts one row of any shape; returns its rowid (the file index).
    fn seed(&self, rel: &str, is_dir: bool, size: i64, mtime: f64) -> i64 {
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
                mtime,
                sha256: None,
                is_dir,
                telegram_msg_id: None,
                is_uploaded: true,
                is_cached: false,
                is_encrypted: false,
                chunk_count: 0,
                mime_type: None,
            })
            .expect("seed row")
    }

    /// K44's core promise: the metadata face cannot reach the network.
    /// Every observable transport interaction must still be empty.
    fn assert_no_transport_calls(&self, what: &str) {
        let mock = &self.mock;
        assert!(mock.open_calls().is_empty(), "{what}: open_calls");
        assert!(
            mock.open_range_calls().is_empty(),
            "{what}: open_range_calls"
        );
        assert!(mock.upload_calls().is_empty(), "{what}: upload_calls");
        assert!(
            mock.stream_upload_calls().is_empty(),
            "{what}: stream_upload_calls"
        );
        assert!(mock.deleted().is_empty(), "{what}: deleted");
        assert!(mock.sent_texts().is_empty(), "{what}: sent_texts");
        assert!(mock.sent_documents().is_empty(), "{what}: sent_documents");
        assert!(mock.message_names().is_empty(), "{what}: message_names");
    }
}

/// FSD path string -> the wide NUL-terminated form the callbacks receive.
fn fsd_path(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

fn rel(name: &str) -> RelPath {
    RelPath::new(name).expect("valid rel path")
}

/// `FSP_FSCTL_DIR_INFO` read back through a byte view: winfsp-rs keeps
/// `DirInfo`'s fields private, and the `#[repr(C)]` layout is pinned by
/// the crate's own `ensure_layout!` assertions, so the leading `Size`
/// u16 (offset 0), the embedded `FileInfo` (offset 8: attributes at +0,
/// file size at +16) and the trailing name buffer (offset 104 =
/// `size_of::<DirInfo<0>>()`) are stable.
fn dir_info_parts(entry: &DirInfo<255>) -> (u16, u32, u64, Vec<u16>) {
    let raw: &[u8] = unsafe {
        std::slice::from_raw_parts(
            (entry as *const DirInfo<255>).cast::<u8>(),
            std::mem::size_of::<DirInfo<255>>(),
        )
    };
    let size = u16::from_le_bytes([raw[0], raw[1]]);
    let attributes = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    let file_size = u64::from_le_bytes(raw[24..32].try_into().expect("size field slice"));
    let name_offset = std::mem::size_of::<DirInfo<0>>();
    let mut name = Vec::new();
    let mut i = name_offset;
    while i + 1 < raw.len() {
        let ch = u16::from_le_bytes([raw[i], raw[i + 1]]);
        if ch == 0 {
            break;
        }
        name.push(ch);
        i += 2;
    }
    (size, attributes, file_size, name)
}

/// The batch's central proof: a full metadata walk (security -> open ->
/// stat -> enumerate -> volume -> close) serves real row data and leaves
/// the transport untouched.
#[test]
fn metadata_face_answers_from_rows_and_never_touches_the_transport() {
    let h = Harness::new();
    let docs_id = h.seed("/docs", true, 0, 1_700_000_000.0);
    let readme_id = h.seed("/docs/readme.txt", false, 1234, 1_700_000_000.0);
    h.seed("/hello.txt", false, 5, 1_699_000_000.0);

    // get_security_by_name: the implicit root, a directory, a file.
    let root = fsd_path("\\");
    let root_sec =
        h.fs.get_security_by_name(U16CStr::from_slice(&root).expect("root name"), None, |_| {
            None
        })
        .expect("root security");
    assert_eq!(root_sec.attributes, FILE_ATTRIBUTE_DIRECTORY);
    assert!(!root_sec.reparse);
    assert_eq!(root_sec.sz_security_descriptor, 0);

    let docs = fsd_path("\\docs");
    let docs_sec =
        h.fs.get_security_by_name(U16CStr::from_slice(&docs).expect("docs name"), None, |_| {
            None
        })
        .expect("docs security");
    assert_eq!(docs_sec.attributes, FILE_ATTRIBUTE_DIRECTORY);

    let readme = fsd_path("\\docs\\readme.txt");
    let readme_sec =
        h.fs.get_security_by_name(
            U16CStr::from_slice(&readme).expect("readme name"),
            None,
            |_| None,
        )
        .expect("readme security");
    assert_eq!(readme_sec.attributes, FILE_ATTRIBUTE_ARCHIVE);

    let missing = fsd_path("\\docs\\nope.txt");
    match h.fs.get_security_by_name(
        U16CStr::from_slice(&missing).expect("missing name"),
        None,
        |_| None,
    ) {
        Err(winfsp::FspError::NTSTATUS(status)) => assert_eq!(status as u32, 0xC000_0034),
        other => panic!("expected STATUS_OBJECT_NAME_NOT_FOUND, got {other:?}"),
    }

    // open: the create/open response must carry the row's stat data.
    let mut open_info: winfsp::filesystem::OpenFileInfo = unsafe { std::mem::zeroed() };
    let handle =
        h.fs.open(
            U16CStr::from_slice(&readme).expect("readme name"),
            0,
            0,
            &mut open_info,
        )
        .expect("open readme");
    assert!(!handle.is_dir());
    assert_eq!(handle.rel(), &rel("/docs/readme.txt"));
    assert_eq!(handle.meta().size, 1234);
    assert_eq!(handle.meta().index_number, readme_id as u64);
    let filled = open_info.as_ref();
    assert_eq!(filled.file_attributes, FILE_ATTRIBUTE_ARCHIVE);
    assert_eq!(filled.file_size, 1234);
    assert_eq!(filled.allocation_size, 1234);
    assert_eq!(filled.creation_time, T_1_700_000_000_FILETIME);
    assert_eq!(filled.last_write_time, T_1_700_000_000_FILETIME);
    assert_eq!(filled.change_time, T_1_700_000_000_FILETIME);

    // get_file_info answers off the handle (no db round-trip needed).
    let mut info = FileInfo::default();
    h.fs.get_file_info(&handle, &mut info).expect("file info");
    assert_eq!(info.file_size, 1234);
    assert_eq!(info.file_attributes, FILE_ATTRIBUTE_ARCHIVE);
    h.fs.close(handle);

    // A directory handle stats as a 0-byte directory.
    let docs_open =
        h.fs.open(
            U16CStr::from_slice(&docs).expect("docs name"),
            0,
            0,
            &mut open_info,
        )
        .expect("open docs");
    assert!(docs_open.is_dir());
    assert_eq!(docs_open.meta().size, 0);
    assert_eq!(docs_open.meta().index_number, docs_id as u64);
    assert_eq!(open_info.as_ref().file_attributes, FILE_ATTRIBUTE_DIRECTORY);
    h.fs.close(docs_open);

    // read_directory's DLL-free half: a fresh enumeration of `\docs`.
    let docs_entries =
        h.fs.prepare_enumeration(
            &h.fs.open_handle(&rel("/docs")).expect("open docs handle"),
            true,
        )
        .expect("prepare enumeration")
        .expect("fresh enumeration returns entries");
    assert_eq!(
        docs_entries
            .iter()
            .map(|e| (e.name.as_str(), e.meta.is_dir, e.meta.size))
            .collect::<Vec<_>>(),
        vec![("readme.txt", false, 1234)]
    );

    // get_volume_info: assembled from the snapshot, never from the wire.
    let mut volume: VolumeInfo = unsafe { std::mem::zeroed() };
    h.fs.get_volume_info(&mut volume).expect("volume info");
    assert_eq!(volume.free_size, VOLUME_HEADROOM);
    assert!(volume.total_size >= VOLUME_HEADROOM);

    h.assert_no_transport_calls("full metadata walk");
}

/// `read_directory` must refuse file handles (the DLL is never reached
/// on that branch).
#[test]
fn enumeration_refuses_file_handles() {
    let h = Harness::new();
    h.seed("/plain.txt", false, 7, 1_700_000_000.0);
    let handle =
        h.fs.open_handle(&rel("/plain.txt"))
            .expect("open plain file");
    match h.fs.prepare_enumeration(&handle, true) {
        Err(winfsp::FspError::NTSTATUS(status)) => assert_eq!(status as u32, 0xC000_0103),
        other => panic!("expected STATUS_NOT_A_DIRECTORY, got {other:?}"),
    }
    h.assert_no_transport_calls("file-handle enumeration");
}

/// Listings carry the db's order (directories first, then name-ascending)
/// and the full stat data per entry — K44's "readdir with stat".
#[test]
fn directory_listings_are_stat_carrying_and_ordered() {
    let h = Harness::new();
    h.seed("/docs", true, 0, 1_700_000_000.0);
    h.seed("/docs/zeta.txt", false, 10, 1_700_000_000.0);
    h.seed("/docs/sub", true, 0, 1_700_000_000.0);
    h.seed("/docs/alpha.txt", false, 20, 1_700_000_500.0);

    let entries = h.fs.dir_entries(&rel("/docs")).expect("list /docs");
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.name.as_str(), e.meta.is_dir, e.meta.size))
            .collect::<Vec<_>>(),
        vec![
            ("sub", true, 0),
            ("alpha.txt", false, 20),
            ("zeta.txt", false, 10),
        ]
    );
    assert_eq!(
        entries[1].meta.filetime(),
        unix_to_filetime(1_700_000_500.0),
        "entry stat must come from the row's mtime"
    );
    h.assert_no_transport_calls("directory listing");
}

/// The NUL trap, pinned at the byte level: the entry's `Size` covers the
/// name's wide chars only. `set_name` (which appends and counts a NUL)
/// would make this 2 bytes longer and send the FSD into a re-enumeration
/// loop.
#[test]
fn directory_entry_names_count_no_trailing_nul() {
    let entry = DirEntry {
        name: "docs".to_string(),
        meta: Meta {
            is_dir: true,
            size: 0,
            mtime: 0.0,
            index_number: 7,
        },
    };
    let mut info = DirInfo::<255>::new();
    fill_dir_info(&mut info, &entry).expect("fill dir entry");
    let (size, attributes, file_size, name) = dir_info_parts(&info);
    let base = std::mem::size_of::<DirInfo<0>>() as u16;
    assert_eq!(name, "docs".encode_utf16().collect::<Vec<u16>>());
    assert_eq!(
        size,
        base + 4 * 2,
        "Size must be base + name bytes (no NUL), got {size} (base {base})"
    );
    assert_eq!(attributes, FILE_ATTRIBUTE_DIRECTORY);
    assert_eq!(file_size, 0);

    let file = DirEntry {
        name: "readme.txt".to_string(),
        meta: Meta {
            is_dir: false,
            size: 1234,
            mtime: 1_700_000_000.0,
            index_number: 9,
        },
    };
    let mut info = DirInfo::<255>::new();
    fill_dir_info(&mut info, &file).expect("fill file entry");
    let (size, attributes, file_size, name) = dir_info_parts(&info);
    assert_eq!(name, "readme.txt".encode_utf16().collect::<Vec<u16>>());
    assert_eq!(size, base + 10 * 2);
    assert_eq!(attributes, FILE_ATTRIBUTE_ARCHIVE);
    assert_eq!(file_size, 1234);
}

/// FSD names -> the `/`-separated namespace, including the rejects.
#[test]
fn fsd_names_map_to_the_vfs_namespace() {
    let cases = [
        ("\\", "/"),
        ("\\docs", "/docs"),
        ("\\docs\\readme.txt", "/docs/readme.txt"),
        ("\\docs\\sub\\file.bin", "/docs/sub/file.bin"),
    ];
    for (input, expected) in cases {
        let wide = fsd_path(input);
        let got = rel_from_winfsp(U16CStr::from_slice(&wide).expect("name"))
            .unwrap_or_else(|_| panic!("{input} must convert"));
        assert_eq!(got.as_str(), expected, "for {input}");
    }

    for rejected in ["\\..\\etc", "\\docs\\..\\x", "\\.\\x", "\\docs\\\\x"] {
        let wide = fsd_path(rejected);
        match rel_from_winfsp(U16CStr::from_slice(&wide).expect("name")) {
            Err(winfsp::FspError::NTSTATUS(status)) => {
                assert_eq!(status as u32, 0xC000_0033, "for {rejected}")
            }
            other => panic!("{rejected} must be rejected, got {other:?}"),
        }
    }
}

/// FILETIME conversion: the epoch offset is what every Win32 consumer
/// (Explorer, the copy engine) reads the timestamps with.
#[test]
fn unix_seconds_convert_to_filetime() {
    assert_eq!(unix_to_filetime(0.0), 116_444_736_000_000_000);
    assert_eq!(unix_to_filetime(1_700_000_000.0), T_1_700_000_000_FILETIME);
    assert_eq!(
        unix_to_filetime(1_700_000_000.5),
        T_1_700_000_000_FILETIME + 5_000_000
    );
    assert_eq!(
        unix_to_filetime(-1.0),
        116_444_736_000_000_000,
        "pre-epoch values clamp to the Unix epoch rather than wrapping u64"
    );
}

/// The adapter carries the injected runtime (WF2's async bridge depends
/// on the identity, not on "some runtime").
#[test]
fn adapter_carries_the_injected_runtime_handle() {
    let h = Harness::new();
    assert_eq!(h.fs.bridge().handle().id(), h.rt.handle().id());
    assert_eq!(h.fs.volume().label, "cydrive-test");
    let _ = &h.vfs;
}
