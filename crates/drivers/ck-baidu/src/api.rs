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
//! | 110 | access_token 过期 | client 层刷新+重放一次；仍败 → `Unauthorized { recoverable: true }`（本函数的 110 臂是 client 自救后的终态兜底） | spike 报告 §1；PCFS client.go:196-203（刷新重放模式） |
//! | 111 | refresh_token 过期 | `Unauthorized { recoverable: false }`（重新授权指引，绝不死循环） | spike §1；PCFS lazy_multifs.go:19-21 |
//! | -6 | 鉴权失败 | `Unauthorized { recoverable: false }` | 同上 |
//! | 31034 | 访问频次超限 | `RateLimited { retry_after: None }` + client 单点重试一次（固定短退避，K15） | spike §2 / 附录 A（multicloud 计划 ：143） |
//! | -9 | 文件/目录不存在 | `NotFound` | spike cleanup 实证（dir_recheck errno=-9） |
//! | 12 | 参数错误 | `Invalid` | 附录 A errno 档 |
//! | 31326 | 下载鉴权失败（CDN 403） | `Unauthorized { recoverable: true }`（重取 dlink/追 token 可救——B2 两段 fallback 的前提） | spike §5 dl-try 矩阵 |
//! | 其他 | 未知 | `Unavailable`（载荷保留 `errno=<code>` 与后端原始消息，R2 可诊断约定） | cloudkit-storage error.rs 归置约定 |
//!
//! 另注（未入 B1 钉死表，mock 已按真实码建模）：-8 = 文件或目录已存在
//! （mkdir 目标已存在 → `Exists`，B2 conformance 断言④覆盖）。注源：
//! PCFS `drivers/baidu` 全目录 rg 无 -8 处理证据（2026-09-08 核实）——
//! 本码按「mock 建模 + B2 conformance 复核」入表。

use cloudkit_storage::StorageError;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::client::BaiduClient;

/// xpan 文件族端点（list/meta/quota/create/filemanager 共用）。
const XPAN_FILE: &str = "/rest/2.0/xpan/file";
/// xpan NAS 端点（uinfo 专用——在 /xpan/nas 不在 /xpan/file）。
const XPAN_NAS: &str = "/rest/2.0/xpan/nas";

/// 后端条目原始形态（list/meta 响应项；字段名 = 后端 JSON 原名，全部
/// `default`——后端对目录省略 size/md5 等字段是常态，spike RemoteEntry
/// 同款防御）。
#[allow(dead_code)]
// md5/server_filename B2 消费（秒传探测/文件名对账）；
// 其余字段 B1 元数据面已读取——结构体整体保留 allow 至 B2 点亮。
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RemoteEntry {
    #[serde(default)]
    pub(crate) fs_id: i64,
    #[serde(default)]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) server_filename: String,
    #[serde(default)]
    pub(crate) size: i64,
    #[serde(default)]
    pub(crate) isdir: i64,
    #[serde(default)]
    pub(crate) md5: String,
    #[serde(default)]
    pub(crate) server_mtime: i64,
}

/// errno → StorageError 归一（模块文档映射表；`payload` 为后端原始消息
/// （errmsg）或出错条目路径（filemanager info 项），未知码必须保留原码）。
pub(crate) fn map_errno(errno: i64, payload: &str) -> StorageError {
    match errno {
        // 终态兜底：client 层（client.rs dispatch）已刷新+重放过一次仍 110
        // 才走到这里；K13 → recoverable:true。
        110 => StorageError::Unauthorized { recoverable: true },
        111 | -6 => StorageError::Unauthorized { recoverable: false },
        -8 => StorageError::Exists, // mock 建模 + B2 conformance 复核（模块文档注源）
        -9 => StorageError::NotFound,
        12 => StorageError::Invalid,
        31034 => StorageError::RateLimited { retry_after: None },
        31326 => StorageError::Unauthorized { recoverable: true },
        _ => StorageError::Unavailable(format!("baidu errno={errno}: {payload}")),
    }
}

/// GET `/rest/2.0/xpan/nas?method=uinfo`——取 uid（VolumeId 构造，K5）。
///
/// 注：在 `/xpan/nas` 不在 `/xpan/file`（PCFS baiduauth/config.go:370-373）；
/// 响应 errno!=0 时按映射表归一（110 会在 client 层自救一次）。
pub(crate) async fn uinfo(client: &BaiduClient) -> Result<i64, StorageError> {
    let v = client.api_get(XPAN_NAS, &[("method", "uinfo")]).await?;
    v.get("uid")
        .and_then(Value::as_i64)
        .ok_or_else(|| StorageError::Unavailable("uinfo response missing uid".into()))
}

/// GET `method=list&dir=<abs>`——depth-1 条目（无分页参数，见模块文档）。
pub(crate) async fn list(
    client: &BaiduClient,
    dir: &str,
) -> Result<Vec<RemoteEntry>, StorageError> {
    let v = client
        .api_get(XPAN_FILE, &[("method", "list"), ("dir", dir)])
        .await?;
    let items = v
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect())
}

/// GET `method=meta&path=<abs>`——单条目元数据（PCFS api.go:110-113）。
pub(crate) async fn meta_by_path(
    client: &BaiduClient,
    path: &str,
) -> Result<RemoteEntry, StorageError> {
    let v = client
        .api_get(XPAN_FILE, &[("method", "meta"), ("path", path)])
        .await?;
    single_entry(&v)
}

/// GET `method=meta&fs_ids=[<id>]`——按句柄解析（PCFS api.go:176-179）。
pub(crate) async fn meta_by_fs_id(
    client: &BaiduClient,
    fs_id: &str,
) -> Result<RemoteEntry, StorageError> {
    let v = client
        .api_get(
            XPAN_FILE,
            &[("method", "meta"), ("fs_ids", &format!("[{fs_id}]"))],
        )
        .await?;
    single_entry(&v)
}

/// meta 响应 `{"errno":0,"list":[…]}` 取首条（path 与 fs_ids 两形态共用）。
///
/// errno!=0 已在 client 层归一（-9 → `NotFound`）；这里只兜「errno=0 但
/// list 空/畸形」的防御形态 → `NotFound`/`Unavailable`。
fn single_entry(v: &Value) -> Result<RemoteEntry, StorageError> {
    let Some(item) = v
        .get("list")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    else {
        return Err(StorageError::NotFound);
    };
    serde_json::from_value(item.clone())
        .map_err(|e| StorageError::Unavailable(format!("meta entry parse: {e}")))
}

/// POST `method=create` + form `path=<abs>&isdir=1`（恰两字段，
/// PCFS api.go:708-735）。
pub(crate) async fn create_dir(client: &BaiduClient, path: &str) -> Result<(), StorageError> {
    client
        .api_post_form(
            XPAN_FILE,
            &[("method", "create")],
            &[("path", path.to_string()), ("isdir", "1".to_string())],
        )
        .await?;
    Ok(())
}

/// POST `method=filemanager&opera=delete` + form `filelist=[{"path":…}]`；
/// **检查 info[] 逐项 errno**（spike api.rs:468-517，比 PCFS 严）。
pub(crate) async fn filemanager_delete(
    client: &BaiduClient,
    path: &str,
) -> Result<(), StorageError> {
    let filelist = serde_json::to_string(&json!([{ "path": path }]))
        .map_err(|e| StorageError::Io(format!("filelist serialize: {e}")))?;
    let v = client
        .api_post_form(
            XPAN_FILE,
            &[("method", "filemanager"), ("opera", "delete")],
            &[("filelist", filelist)],
        )
        .await?;
    check_info(&v)
}

/// POST `method=filemanager&opera=move` + form `async=1` +
/// `filelist=[{"path","dest","newname","ondup":"overwrite"}]`；
/// **不轮询 taskid**（PCFS api.go:781-870，从不轮询——两源一致）。
pub(crate) async fn filemanager_move(
    client: &BaiduClient,
    from: &str,
    dest_dir: &str,
    new_name: &str,
) -> Result<(), StorageError> {
    let filelist = serde_json::to_string(&json!([{
        "path": from,
        "dest": dest_dir,
        "newname": new_name,
        "ondup": "overwrite",
    }]))
    .map_err(|e| StorageError::Io(format!("filelist serialize: {e}")))?;
    let v = client
        .api_post_form(
            XPAN_FILE,
            &[("method", "filemanager"), ("opera", "move")],
            &[("async", "1".to_string()), ("filelist", filelist)],
        )
        .await?;
    check_info(&v)
}

/// filemanager 响应 info[] 逐项 errno 检查（delete/move 共用；spike
/// api.rs:468-517——顶层 errno=0 不代表逐条成功，info 项的 -9 即目标缺失）。
fn check_info(v: &Value) -> Result<(), StorageError> {
    if let Some(items) = v.get("info").and_then(Value::as_array) {
        for item in items {
            let errno = item.get("errno").and_then(Value::as_i64).unwrap_or(0);
            if errno != 0 {
                let payload = item.get("path").and_then(Value::as_str).unwrap_or("");
                return Err(map_errno(errno, payload));
            }
        }
    }
    Ok(())
}

/// GET `method=quota`——响应顶层 `used`/`total`（PCFS api.go:914-933）。
pub(crate) async fn quota(client: &BaiduClient) -> Result<(i64, i64), StorageError> {
    let v = client.api_get(XPAN_FILE, &[("method", "quota")]).await?;
    let used = v.get("used").and_then(Value::as_i64).unwrap_or(0);
    let total = v.get("total").and_then(Value::as_i64).unwrap_or(0);
    Ok((used, total))
}
