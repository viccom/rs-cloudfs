//! # ck-local——本地文件系统驱动（L2 驱动 crate，Phase 2 Batch L）。
//!
//! 后端 = 一个根目录下的普通文件系统。卷身份 `local:<规范化绝对根路径>`
//!（D6：同根多实例共享卷，换根 = 换卷；根路径规范化归本驱动，L2 只存
//! opaque key——Windows 盘符与 `\\?\` 前缀形态对 L2 合法）。
//!
//! 驱动形态：StorageDriver 九方法全实现（conformance 断言①–⑥⑧ 绿；
//! ⑦ RESUME 未声明自动跳过）。能力位四真六假（B3b 起 +remote_delete），
//! 逐位注码见 driver.rs `capabilities()`：range_read / server_side_move /
//! authoritative_index / remote_delete（transport 面）。上传走
//! commit-on-close：暂存文件在卷根 `.cklocal-staging/`（保留名，
//! list 不可见），close = 同卷原子 rename——详见 driver.rs / stager.rs
//! 模块文档（含 io::Error 映射表与保留名规则）。
//!
//! 双面驱动（B3b）：[`LocalTransport`] 是 CloudTransport 面（K2 path 寻址
//! / K6 msg_id=0 占位）——与 StorageDriver 面共享同一驱动实现，薄壳互调
//! （`transport_face.rs`）。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate（driver-onboarding
//! §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。

mod driver;
mod stager;
mod transport_face;

use std::path::PathBuf;
use std::sync::Arc;

use cloudkit_storage::StorageError;

pub use driver::LocalDriver;
pub use stager::LocalStager;
pub use transport_face::LocalTransport;

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
