//! ck-local conformance 套件接入（interfaces §6 / foundation D9）。
//!
//! Batch L：被测对象是全量实现驱动，`conformance_suite_offline` 跑
//! 断言①–⑥⑧（绿 commit 归档输出尾部）；⑦ RESUME 未声明由能力位门控
//! 自动跳过。
//!
//! local 后端形态声明：无远端、无后端错误码 → 断言⑤空表（interfaces
//! §6：仅 local 类允许）；`delete_missing`/`empty_range` 按 OS 文件系统
//! 语义声明（不存在 = NotFound、start>=size = 空读）。

use async_trait::async_trait;
use ck_local::{factory, LocalDriver, LocalParams};
use cloudkit_storage::conformance::ConformanceHarness;
use cloudkit_storage::{
    BackendHandle, Capabilities, EntryId, StorageDriver, StorageError, VolumeId,
};

/// 无分块驱动：chunk 边界报 1（ConformanceHarness::chunk_size 契约）。
const CHUNK: u64 = 1;

struct LocalHarness {
    driver: LocalDriver,
    /// 临时根目录 guard：活到 harness 生命结束，Drop 时清理整棵测试卷。
    _root: tempfile::TempDir,
}

impl LocalHarness {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("创建临时根目录失败");
        let driver = LocalDriver::new(root.path().to_path_buf()).expect("LocalDriver 构造失败");
        LocalHarness {
            driver,
            _root: root,
        }
    }
}

#[async_trait]
impl ConformanceHarness for LocalHarness {
    fn driver(&self) -> &dyn StorageDriver {
        &self.driver
    }

    fn chunk_size(&self) -> u64 {
        CHUNK
    }

    fn delete_missing_yields_not_found(&self) -> bool {
        true // OS 文件系统语义：删除不存在路径报 NotFound（声明形态恒定）
    }

    fn empty_range_yields_empty_stream(&self) -> bool {
        true // OS 文件系统语义：start>=size 是空读而非错误
    }

    // error_table 默认空表：local 无后端错误码，断言⑤空跑（interfaces §6
    // 明示 local 类允许）；inject_backend_error / backend_bytes_received
    // 保持默认——local 无注入面、未声明 RESUME（Capabilities::none()）。
}

cloudkit_storage::conformance_suite!(LocalHarness::new());

/// 工厂冒烟：async 装配入口可用，卷形态 `local:<规范化根路径>`。
/// 不断言 capabilities 具体值——能力声明形态断言归下方独立测试。
#[tokio::test]
async fn factory_bootstraps_local_volume() {
    let root = tempfile::tempdir().expect("创建临时根目录失败");
    let driver = factory(&LocalParams {
        root: root.path().to_path_buf(),
    })
    .await
    .expect("factory 构造失败");
    assert_eq!(driver.volume().scheme(), "local");
    assert!(!driver.volume().key().is_empty());
}

/// L2 trait 契约钉死：他卷句柄 delete → `NotFound`（driver.rs 契约行
/// 「他卷句柄 → NotFound」+ mock 先例；本卷视角下他卷对象即不存在）。
#[tokio::test]
async fn delete_foreign_volume_handle_yields_not_found() {
    let root = tempfile::tempdir().expect("创建临时根目录失败");
    let driver = LocalDriver::new(root.path().to_path_buf()).expect("LocalDriver 构造失败");
    let foreign = EntryId::new(
        VolumeId::new("local", "Z:/foreign-root").expect("他卷 VolumeId 构造失败"),
        BackendHandle::new("a.txt"),
    );
    assert_eq!(driver.delete(&foreign).await, Err(StorageError::NotFound));
}

/// 能力声明静态锁（R4 诚实性）：九位精确值钉死，防未来漂移（先例：
/// cloudkit-storage tests/conformance_mock.rs
/// `mock_capabilities_are_the_declared_set`）。动态验证由
/// `conformance_suite_offline` 全套跑通承担。
#[test]
fn capabilities_are_the_declared_set() {
    let root = tempfile::tempdir().expect("创建临时根目录失败");
    let driver = LocalDriver::new(root.path().to_path_buf()).expect("LocalDriver 构造失败");
    assert_eq!(
        driver.capabilities(),
        Capabilities {
            range_read: true,          // 断言②全套绿（半开/钳制/空窗口/start>=size 空流）
            resume: false,             // 暂存不跨 stager 生命周期复活，无差集续传
            multipart: false,          // 本地 FS 无远端分片概念
            server_side_move: true,    // fs::rename 同卷原子移动（断言⑥）
            rapid_upload: false,       // 无内容寻址去重后端
            authoritative_index: true, // list 即本地 FS 真相（断言③；local 类真机豁免）
            change_feed: false,        // 非云后端，无推送
            inbound: false,            // 无入站通道
            chat: false,               // 无对话通道
        }
    );
}
