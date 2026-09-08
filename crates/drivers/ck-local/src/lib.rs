//! # ck-local——本地文件系统驱动（L2 驱动 crate，Phase 2 Batch L）。
//!
//! 后端 = 一个根目录下的普通文件系统。卷身份 `local:<规范化绝对根路径>`
//! （D6：同根多实例共享卷，换根 = 换卷；根路径规范化归本驱动，L2 只存
//! opaque key——Windows 盘符与 `\\?\` 前缀形态对 L2 合法）。
//!
//! **当前状态：TDD 红阶段骨架**——[`LocalDriver`] 九方法与
//! [`LocalStager`] 全部返回 `StorageError::Unsupported`，能力位恒
//! `Capabilities::none()`；conformance 套件（tests/conformance.rs）预期
//! 在断言①红。绿阶段（Batch L 实现批）逐方法点亮并声明真实能力位。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate（driver-onboarding
//! §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。

mod driver;
mod stager;

use std::path::PathBuf;
use std::sync::Arc;

use cloudkit_storage::StorageError;

pub use driver::LocalDriver;
pub use stager::LocalStager;

/// local 驱动参数（driver-onboarding §4：「配置 map → 参数结构体」纯函数
/// 形态；Phase 2.5 多卷时每实例一份）。
#[derive(Debug, Clone)]
pub struct LocalParams {
    /// 卷根目录（工厂负责创建与规范化，见 [`factory`]）。
    pub root: PathBuf,
}

/// 装配工厂：创建根目录（幂等）→ 规范化为绝对路径 → 构造驱动。
///
/// 阻塞面（create_dir_all/canonicalize）经 `spawn_blocking` 隔离
/// （code-style §4：async 上下文禁阻塞调用）。
pub async fn factory(cfg: &LocalParams) -> Result<Arc<LocalDriver>, StorageError> {
    let root = cfg.root.clone();
    let driver = tokio::task::spawn_blocking(move || LocalDriver::new(root))
        .await
        .map_err(|e| StorageError::Io(format!("local driver init join error: {e}")))?;
    Ok(Arc::new(driver?))
}
