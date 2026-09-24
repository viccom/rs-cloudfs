//! WinFsp 面 × 宽面后端的 mutation 远端先行测试（2026-09-24 检查批）。
//!
//! 契约（BUG 2 / BUG 3 类缺陷的 FSD 面钉子——WebDAV 适配器面已修，
//! 本文件把同一契约钉到盘符挂载面）：
//! - `rename_entry` 的三个臂（普通 / 覆盖 / 大小写改名）在宽面 +
//!   `server_side_move` 后端上**远端先行**：`Vfs::rename_remote_for_row`
//!   先于 `db.rename_path`；覆盖臂的目标远端对象先删
//!   （`Vfs::delete_remote_for_row`）——失败则本地行原样保留，远端与
//!   本地绝不分裂；
//! - `prepare_create` 的目录腿在宽面上先 `Vfs::mkdir_remote_for_row`
//!   再写本地行——MKCOL 假成功（201 而远端无目录、行带 `is_uploaded:
//!   true` 假声明随后被 read-through 剪除）不再发生；
//! - 窄面（telegram/mock）的既有行为由 write.rs 的窄面套件钉住
//!   （远端先行在无宽面后端上是逐字 no-op）。
//!
//! 本文件全部在无 WinFsp 安装的环境运行：DLL 只经 mount host 触达。
//! 桩纪律：`WideTransport` 镜像生产 transport_face 的形态（`delete_remote`
//! = 句柄映射后 `driver.delete`，能力位镜像驱动声明 + `remote_delete`），
//! 不照实现抄。

#![cfg(all(windows, feature = "winfsp"))]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use cloudkit_core::cache::CacheManager;
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::{
    CloudTransport, RemoteHandle, StorageError, UploadJob, UploadReceipt,
};
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_storage::{
    BackendHandle, Entry, EntryId, MockStorageDriver, RelPath as VolRel, StorageDriver, VolumeId,
    WriteHint,
};
use cloudkit_winfsp::fs::{CloudFs, Disposition, Handle};
use winfsp::FspError;

/// 宽面 transport double：`as_driver` 暴露存储 mock，`delete_remote`
/// 镜像 ck-baidu transport_face 的句柄映射（数字句柄经 `first_msg_id`
/// 十进制串还原——mock 的句柄空间恰是该形态）。上传/读取面在这些
/// 测试里不可达（行全部来自读穿物化，无上传发生）。
struct WideTransport {
    driver: Arc<MockStorageDriver>,
}

#[async_trait]
impl CloudTransport for WideTransport {
    async fn connect(&self) -> Result<(), StorageError> {
        Ok(())
    }

    async fn upload(&self, _job: &UploadJob) -> Result<UploadReceipt, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn open(
        &self,
        _file: &RemoteHandle,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn open_range(
        &self,
        _file: &RemoteHandle,
        _off: u64,
        _len: u64,
    ) -> Result<cloudkit_core::transport::ByteStream, StorageError> {
        Err(StorageError::Unsupported)
    }

    async fn delete_remote(&self, handle: &RemoteHandle) -> Result<(), StorageError> {
        let id = EntryId::new(
            self.driver.volume().clone(),
            BackendHandle::new(handle.first_msg_id.to_string()),
        );
        self.driver.delete(&id).await
    }

    fn capabilities(&self) -> cloudkit_core::transport::Capabilities {
        let mut caps = StorageDriver::capabilities(self.driver.as_ref());
        caps.remote_delete = true;
        caps
    }

    fn as_driver(&self) -> Option<&dyn StorageDriver> {
        Some(self.driver.as_ref())
    }
}

/// 宽面 harness：真 SQLite + cache 树 + 存储 mock（宽面）+ Vfs +
/// 被测的 `CloudFs`。「远端」经 `driver` 种入；`files` 索引从空开始，
/// 行由读穿（`stat_fresh`）物化——挂载卷的真实形态。
struct WideHarness {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    vfs: Arc<Vfs>,
    driver: Arc<MockStorageDriver>,
    fs: CloudFs,
    rt: tokio::runtime::Runtime,
}

impl WideHarness {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open temp db"));
        let cache = CacheManager::new(dir.path().join("cache"), 1 << 30);
        let driver = Arc::new(MockStorageDriver::new(
            VolumeId::parse("baidu:123456789").expect("volume id"),
        ));
        let transport: Arc<dyn CloudTransport> = Arc::new(WideTransport {
            driver: Arc::clone(&driver),
        });
        // Vfs::new 会起上传队列：runtime 必须已在作用域内。
        let _guard = rt.enter();
        let vfs = Arc::new(Vfs::new(db.clone(), cache, transport, test_cfg()));
        let fs = CloudFs::new(vfs.clone(), rt.handle().clone(), "cydrive-test");
        Self {
            _dir: dir,
            db,
            vfs,
            driver,
            fs,
            rt,
        }
    }

    /// 在存储 mock 的远端种一个文件（writer + write + close）。
    fn seed_wide_file(&self, path: &str, data: &[u8]) {
        self.rt.block_on(async {
            let rel = VolRel::new(path).expect("seed path");
            let hint = WriteHint {
                size: Some(data.len() as u64),
                ..Default::default()
            };
            let mut stager = self.driver.writer(&rel, &hint).await.expect("seed writer");
            stager.write(data).await.expect("seed write");
            stager.close().await.expect("seed close");
        });
    }

    /// 经读穿把远端对象物化成本地行（挂载卷上 Explorer 看到行的方式）。
    fn materialize(&self, rel: &str) {
        self.rt
            .block_on(self.vfs.stat_fresh(&path(rel)))
            .expect("materialize the row off the remote");
    }

    /// 驱动自己的 stat（远端真态），返回 Entry。
    fn remote_stat(&self, path: &str) -> Result<Entry, StorageError> {
        self.rt
            .block_on(self.driver.stat(&VolRel::new(path).expect("stat path")))
    }

    /// `files` 行。
    fn row(&self, rel: &str) -> Option<cloudkit_core::database::FileRecord> {
        self.db.get_file(rel).expect("db read")
    }
}

fn path(rel: &str) -> RelPath {
    RelPath::new(rel).expect("valid rel path")
}

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

/// 普通改名（BUG 2 类，winfsp 面）：宽面 + `server_side_move` 后端上，
/// F2 改名必须把远端对象一起搬走——修复前只有本地行走、远端留在旧
/// 路径（Explorer 报成功，rebuild 后名字回退）。
#[test]
fn rename_on_a_wide_face_moves_the_remote_object() {
    let h = WideHarness::new();
    h.seed_wide_file("old.bin", b"abcdefg");
    h.materialize("/old.bin");

    let handle = h.fs.open_handle(&path("/old.bin")).expect("open the row");
    h.fs.rename_entry(&handle, &path("/old.bin"), &path("/renamed.bin"), false)
        .expect("rename");

    assert!(
        matches!(h.remote_stat("old.bin"), Err(StorageError::NotFound)),
        "the remote object must be gone from the old path after rename"
    );
    let entry = h
        .remote_stat("renamed.bin")
        .expect("the remote object must exist at the new path after rename");
    assert_eq!(entry.size, 7, "the moved object keeps its bytes");
    assert!(h.row("/renamed.bin").is_some(), "the local row moved too");
}

/// 覆盖改名（BUG 2 类覆盖臂）：目标的远端对象必须先删——修复前本地
/// 目标行被删而远端目标留存，rebuild 会把它复活成幽灵并顶掉改名结果。
#[test]
fn rename_overwrite_on_a_wide_face_replaces_the_remote_destination() {
    let h = WideHarness::new();
    h.seed_wide_file("src.bin", b"source!");
    h.seed_wide_file("dst.bin", b"dst");
    h.materialize("/src.bin");
    h.materialize("/dst.bin");

    let handle = h.fs.open_handle(&path("/src.bin")).expect("open the row");
    h.fs.rename_entry(&handle, &path("/src.bin"), &path("/dst.bin"), true)
        .expect("rename over");

    assert!(
        matches!(h.remote_stat("src.bin"), Err(StorageError::NotFound)),
        "the source must be gone from the remote"
    );
    let entry = h
        .remote_stat("dst.bin")
        .expect("the destination holds the moved object");
    assert_eq!(
        entry.size, 7,
        "the destination was replaced by the source's bytes, not the old 3-byte object"
    );
}

/// 大小写改名（BUG 2 类大小写臂）：Windows 的大小写翻转在远端同样
/// 要落地——修复前只有本地行换了拼写，rebuild 后回退旧拼写。
#[test]
fn case_only_rename_on_a_wide_face_flips_the_remote_spelling() {
    let h = WideHarness::new();
    h.seed_wide_file("Mixed.TXT", b"payload");
    h.materialize("/Mixed.TXT");

    let handle = h.fs.open_handle(&path("/Mixed.TXT")).expect("open the row");
    h.fs.rename_entry(&handle, &path("/Mixed.TXT"), &path("/mixed.txt"), false)
        .expect("case rename");

    assert!(
        matches!(h.remote_stat("Mixed.TXT"), Err(StorageError::NotFound)),
        "the old spelling must be gone from the remote"
    );
    assert!(
        h.remote_stat("mixed.txt").is_ok(),
        "the new spelling must exist on the remote"
    );
}

/// Explorer 新建文件夹（BUG 3 类，winfsp 面）：宽面后端上目录必须
/// 建到远端——修复前只写本地行（`is_uploaded: true` 假声明），远端
/// 无此目录，read-through reconcile 随后把「消失的文件夹」剪除。
#[test]
fn explorer_new_folder_on_a_wide_face_creates_the_remote_directory() {
    let h = WideHarness::new();

    // FILE_CREATE + FILE_DIRECTORY_FILE + FILE_ATTRIBUTE_DIRECTORY
    // （FSD 内核侧形态，同 write.rs 的 create_dir_entry）。
    let options = (Disposition::Create as u32) << 24 | 0x01;
    let _handle: Handle =
        h.fs.prepare_create(&path("/made-by-explorer"), options, 0x0010)
            .expect("create dir");

    let entry = h
        .remote_stat("made-by-explorer")
        .expect("the remote directory must exist after the create");
    assert_eq!(
        entry.kind,
        cloudkit_storage::EntryKind::Dir,
        "got {entry:?}"
    );
    assert!(h.row("/made-by-explorer").is_some(), "the local row exists");
}

/// 远端已有时新建文件夹必须报碰撞（STATUS_OBJECT_NAME_COLLISION），
/// 而不是把假声明行写进去——Explorer 看到「已存在」而非凭空多出
/// 一个刷新即消失的文件夹。
#[test]
fn explorer_new_folder_collides_when_the_remote_already_has_it() {
    let h = WideHarness::new();
    h.rt.block_on(async {
        h.driver
            .mkdir(&VolRel::new("remote-made").expect("dir"))
            .await
            .expect("seed remote dir");
    });

    let options = (Disposition::Create as u32) << 24 | 0x01;
    assert_eq!(
        status_of(h.fs.prepare_create(&path("/remote-made"), options, 0x0010)),
        0xC000_0035,
        "STATUS_OBJECT_NAME_COLLISION when the remote already has the directory"
    );
    assert!(
        h.row("/remote-made").is_none(),
        "no local row was written for the refused create"
    );
}

/// write.rs 同款：FspError → NTSTATUS 数值。
fn status_of<T>(result: winfsp::Result<T>) -> u32 {
    match result {
        Ok(_) => panic!("expected an error status, got success"),
        Err(FspError::NTSTATUS(status)) => status as u32,
        Err(other) => panic!("expected an NTSTATUS carrier, got {other:?}"),
    }
}
