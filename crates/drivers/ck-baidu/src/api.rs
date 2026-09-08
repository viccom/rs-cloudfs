//! 百度网盘协议包装层（B1 元数据面）——错误归一到 [`StorageError`]（R2）。
//!
//! ## 端点与表单黄金参照
//!
//! 分歧时以 spike 实抓为准（`examples/baidu_spike/src/api.rs` +
//! `docs/reports/2026-09-07-baidu-spike.md`），PCFS 源码
//! （`E:\Go_codes\PrivateCloudFS\drivers\baidu`）为交叉权威——逐操作注源：
//!
//! | 操作 | 端点 | 参数形态 | 源 |
//! |---|---|---|---|
//! | list | GET `/rest/2.0/xpan/file` | query 恰 `method=list&dir=<abs>&access_token`（**无分页参数**——driver 内 offset 游标切 Page，ck-local 先例） | spike api.rs:131-148；PCFS api.go:49-52 |
//! | stat | GET 同上 | query 恰 `method=meta&path=<abs>&access_token`（**path 参数**，非 filelist） | PCFS api.go:110-113 |
//! | stat（按句柄） | GET 同上 | query `method=meta&fs_ids=[<id>]&access_token`（fs_id → path 解析，delete 流程用；数组形态） | PCFS api.go:176-179 |
//! | mkdir | POST 同上 | query `method=create&access_token`；form 恰 `path=<abs>&isdir=1` 两字段（application/x-www-form-urlencoded） | PCFS api.go:708-735 |
//! | delete | POST 同上 | query `method=filemanager&opera=delete&access_token`；form 恰 `filelist` 字段 = `[{"path":<abs>}]`；**检查 info[] 逐项 errno** | spike api.rs:468-517（比 PCFS 严，按 spike） |
//! | move（rename） | POST 同上 | query `method=filemanager&opera=move&access_token`；form `async=1` + `filelist=[{"path":…,"dest":…,"newname":…,"ondup":"overwrite"}]`；**不轮询 taskid** | PCFS api.go:781-870（829-845 形态） |
//! | quota | GET 同上 | query 恰 `method=quota&access_token`（拼在 xpan/file 上）；响应顶层 `used`/`total`（i64） | PCFS api.go:914-933 |
//! | uinfo | GET `/rest/2.0/xpan/nas` | query 恰 `method=uinfo&access_token`（**在 /xpan/nas 不在 /xpan/file**）；响应顶层含 `uid` | PCFS internal/baiduauth/config.go:370-373 |
//! | oauth refresh | GET `/oauth/2.0/token` | query 恰 `grant_type=refresh_token&refresh_token=…&client_id=…&client_secret=…`；成功顶层 `access_token/refresh_token/expires_in`，失败顶层 `error/error_description` | spike api.rs:22-81 |
//!
//! 响应条目字段：`fs_id`/`path`/`server_filename`/`size`/`isdir`/`md5`/
//! `server_mtime`——**mtime 读 `server_mtime`**（PCFS api.go:85-87/162-164；
//! 两源均无 local_mtime）。
//!
//! B2 占位（本批不实现）：precreate / superfile2 / create（文件）/ dlink
//! ——形态见计划 §4 与上表 spike 源码行号。
//!
//! ## errno → StorageError 映射表（R2 义务；mock 钉死）
//!
//! 回放测试：`tests/errno_mapping.rs`（31034/-9/12/31326/未知码） +
//! `tests/oauth_state_machine.rs`（110/111/-6）。逐码注源：
//!
//! | errno | 语义 | 映射 | 源 |
//! |---|---|---|---|
//! | 110 | access_token 过期 | client 层刷新+重放一次；仍败 → `Unauthorized { recoverable: true }` | spike 报告 §1；PCFS client.go:196-203（刷新重放模式） |
//! | 111 | refresh_token 过期 | `Unauthorized { recoverable: false }`（重新授权指引，绝不死循环） | spike §1；PCFS lazy_multifs.go:19-21 |
//! | -6 | 鉴权失败 | `Unauthorized { recoverable: false }` | 同上 |
//! | 31034 | 访问频次超限 | `RateLimited { retry_after: None }` + client 单点重试一次（指数退避，K15） | spike §2 / 附录 A（multicloud 计划 ：143） |
//! | -9 | 文件/目录不存在 | `NotFound` | spike cleanup 实证（dir_recheck errno=-9） |
//! | 12 | 参数错误 | `Invalid` | 附录 A errno 档 |
//! | 31326 | 下载鉴权失败（CDN 403） | `Unauthorized { recoverable: true }`（重取 dlink/追 token 可救——B2 两段 fallback 的前提） | spike §5 dl-try 矩阵 |
//! | 其他 | 未知 | `Unavailable`（载荷保留 `errno=<code>` 与后端原始消息，R2 可诊断约定） | cloudkit-storage error.rs 归置约定 |
//!
//! 另注（未入 B1 钉死表，mock 已按真实码建模）：-8 = 文件或目录已存在
//! （mkdir 目标已存在 → `Exists`，B2 conformance 断言④覆盖）。

use cloudkit_storage::StorageError;

use crate::client::BaiduClient;

/// 后端条目原始形态（list/meta 响应项；字段名 = 后端 JSON 原名，
/// serde rename 由实现侧落位）。
#[allow(dead_code)] // 红骨架：绿阶段响应解析使用
#[derive(Debug, Clone, Default)]
pub(crate) struct RemoteEntry {
    pub(crate) fs_id: i64,
    pub(crate) path: String,
    pub(crate) server_filename: String,
    pub(crate) size: i64,
    pub(crate) isdir: i64,
    pub(crate) md5: String,
    pub(crate) server_mtime: i64,
}

/// errno → StorageError 归一（模块文档映射表；`payload` 为后端原始消息，
/// 未知码必须保留原码）。
#[allow(dead_code)] // 红骨架：绿阶段 client.rs 错误路径接线
pub(crate) fn map_errno(errno: i64, payload: &str) -> StorageError {
    let _ = (errno, payload);
    StorageError::Unsupported
}

/// GET `/rest/2.0/xpan/nas?method=uinfo`——取 uid（VolumeId 构造，K5）。
///
/// 注：在 `/xpan/nas` 不在 `/xpan/file`（PCFS baiduauth/config.go:370-373）；
/// 响应 errno!=0 时按映射表归一。
pub(crate) async fn uinfo(client: &BaiduClient) -> Result<i64, StorageError> {
    let _ = client;
    Err(StorageError::Unsupported)
}

/// GET `method=list&dir=<abs>`——depth-1 条目（无分页参数，见模块文档）。
#[allow(dead_code)] // 红骨架：绿阶段 driver.rs list 接线
pub(crate) async fn list(
    client: &BaiduClient,
    dir: &str,
) -> Result<Vec<RemoteEntry>, StorageError> {
    let _ = (client, dir);
    Err(StorageError::Unsupported)
}

/// GET `method=meta&path=<abs>`——单条目元数据（PCFS api.go:110-113）。
#[allow(dead_code)] // 红骨架：绿阶段 driver.rs stat 接线
pub(crate) async fn meta_by_path(
    client: &BaiduClient,
    path: &str,
) -> Result<RemoteEntry, StorageError> {
    let _ = (client, path);
    Err(StorageError::Unsupported)
}

/// GET `method=meta&fs_ids=[<id>]`——按句柄解析（PCFS api.go:176-179）。
#[allow(dead_code)] // 红骨架：绿阶段 driver.rs delete 解析路径接线
pub(crate) async fn meta_by_fs_id(
    client: &BaiduClient,
    fs_id: &str,
) -> Result<RemoteEntry, StorageError> {
    let _ = (client, fs_id);
    Err(StorageError::Unsupported)
}

/// POST `method=create` + form `path=<abs>&isdir=1`（恰两字段，
/// PCFS api.go:708-735）。
#[allow(dead_code)] // 红骨架：绿阶段 driver.rs mkdir 接线
pub(crate) async fn create_dir(client: &BaiduClient, path: &str) -> Result<(), StorageError> {
    let _ = (client, path);
    Err(StorageError::Unsupported)
}

/// POST `method=filemanager&opera=delete` + form `filelist=[{"path":…}]`；
/// **检查 info[] 逐项 errno**（spike api.rs:468-517，比 PCFS 严）。
#[allow(dead_code)] // 红骨架：绿阶段 driver.rs delete 接线
pub(crate) async fn filemanager_delete(
    client: &BaiduClient,
    path: &str,
) -> Result<(), StorageError> {
    let _ = (client, path);
    Err(StorageError::Unsupported)
}

/// POST `method=filemanager&opera=move` + form `async=1` +
/// `filelist=[{"path","dest","newname","ondup":"overwrite"}]`；
/// **不轮询 taskid**（PCFS api.go:781-870，从不轮询——两源一致）。
#[allow(dead_code)] // 红骨架：绿阶段 driver.rs rename 接线
pub(crate) async fn filemanager_move(
    client: &BaiduClient,
    from: &str,
    dest_dir: &str,
    new_name: &str,
) -> Result<(), StorageError> {
    let _ = (client, from, dest_dir, new_name);
    Err(StorageError::Unsupported)
}

/// GET `method=quota`——响应顶层 `used`/`total`（PCFS api.go:914-933）。
#[allow(dead_code)] // 红骨架：绿阶段 driver.rs quota 接线
pub(crate) async fn quota(client: &BaiduClient) -> Result<(i64, i64), StorageError> {
    let _ = client;
    Err(StorageError::Unsupported)
}
