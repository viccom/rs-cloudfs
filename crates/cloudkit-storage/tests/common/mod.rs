//! 测试共享 fixture（integration tests 间的公共模块，非独立测试）。

use cloudkit_storage::StorageError;

/// 百度 errno → StorageError 映射表（**test-only fixture**）。
///
/// R1 红线：L2 运行时代码不认识任何后端错误码——真实映射表归 ck-baidu
/// （Phase 2 Batch B1）在驱动内实现并用 mock 端点回放钉死。此 fixture
/// 的作用是把「分类学可以表达这三档」从第一天钉死（interfaces §3 /
/// multicloud 附录 A / foundation D2）：
///
/// - `110`：access token 过期——驱动内自动刷新 + 重放一次仍失败 →
///   `Unauthorized { recoverable: true }`（再刷新有望恢复）；
/// - `111`：refresh token 失效——需人工重新授权 →
///   `Unauthorized { recoverable: false }`；
/// - `-6`：鉴权失败（身份/签名无效）→
///   `Unauthorized { recoverable: false }`。
pub fn map_baidu_errno(errno: i32) -> StorageError {
    match errno {
        110 => StorageError::Unauthorized { recoverable: true },
        111 => StorageError::Unauthorized { recoverable: false },
        -6 => StorageError::Unauthorized { recoverable: false },
        // 未知错误 → Unavailable + 原始码保留（interfaces §3：可诊断不丢信息）
        other => StorageError::Unavailable(format!("baidu errno {other}")),
    }
}

/// 三档断言用的码表。
pub const BAIDU_AUTH_TIERS: [i32; 3] = [110, 111, -6];
