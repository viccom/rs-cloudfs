//! ck-local conformance 套件接入（interfaces §6 / foundation D9）。
//!
//! Batch L 红阶段：被测对象是 Unsupported 占位骨架，预期
//! `conformance_suite_offline` 在断言①（mkdir/上传往返）红——红证据归档
//! 于红 commit；绿阶段（Batch L 实现批）①–⑥⑧ 转绿、⑦ RESUME 未声明
//! 由能力位门控自动跳过。
//!
//! local 后端形态声明：无远端、无后端错误码 → 断言⑤空表（interfaces
//! §6：仅 local 类允许）；`delete_missing`/`empty_range` 按 OS 文件系统
//! 语义声明（不存在 = NotFound、start>=size = 空读）。

use async_trait::async_trait;
use ck_local::{factory, LocalDriver, LocalParams};
use cloudkit_storage::conformance::ConformanceHarness;
use cloudkit_storage::StorageDriver;

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
/// 不断言 capabilities 具体值——绿阶段能力位逐位点开后该断言必漂移，
/// 能力声明形态断言归绿阶段测试。
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
