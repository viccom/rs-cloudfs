//! WebdavDriver——WebDAV 存储驱动（Phase 7 / WD3：读面 + 写面全接线）。
//!
//! 后端 = 一台 WebDAV 服务器上的一个（可含子路径的）基地址——
//! 「通用挂载协议」面：rclone serve / OpenList/Alist / Nextcloud /
//! 群晖 DSM / Apache mod_dav / 本仓 cloudkit-webdav 自举，全部成为
//! 可用后端（计划 §0 定位）。
//!
//! ## 读路径形态（计划 §4.7）
//!
//! - `stat` = PROPFIND Depth 0（文件路径不带尾斜杠；apache SlashStrict
//!   的集合 301 由 client 的尾斜杠重试腿吸收——附录 C ⑧）；
//! - `list` = PROPFIND Depth 1 **恒带尾斜杠**（apache 集合 no-slash 301
//!   规避）→ 剔 self（容忍尾斜杠差异）→ K67 不可寻址名过滤 +
//!   `.ckwd-` 暂存件过滤 → 名字典序稳定排序 → `Page` 驱动内全量切片
//!   （PROPFIND 无分页原语——`off:N` 不透明令牌，sftp/local 同款）；
//! - `reader` = stat 先行（start ≥ size → 空流不开 GET）→ **8 MiB 串行
//!   窗口循环** → GET Range（206 校验/200 截断在 client.get_range——
//!   §4.5-6）→ futures ByteStream 流式 yield。**窗口间不跨窗自动重试**
//!   ——单窗内 client 白名单自愈即止（M-S3 思想的白名单内简化版：跨窗
//!   重试要重排窗口状态机，收益集中于极罕见的「每窗都断」形态，交给
//!   调用方重开 reader 更简单也更诚实）；416 → stat 复核（offset 仍
//!   越界 = 并发收缩 → EOF 空收尾；否则 `Unavailable`）。
//!
//! ## 写路径形态（WD3，计划 §4.4/§4.6）
//!
//! - `mkdir`：**stat 预检先行**（存在→`Exists`——rclone MKCOL-201 幂等
//!   陷阱，附录 C ⑥）→ MKCOL 恒尾斜杠 → 409 → 隐式建父重试恰一次
//!   （仍败 → `NotFound`（父））；
//! - `delete(EntryId)`：**幂等形态声明 = 不存在恒 `NotFound`**（sftp
//!   先例同款并恒定）——stat 预检（目录腿尾斜杠判定 + 形态归一）后
//!   DELETE；stat 与 DELETE 之间被并发删除的 404 同归 `NotFound`；
//! - `rename`：stat from 先行 → MOVE `Overwrite: F`（trait 契约与 sftp
//!   先例：目标存在 → `Exists`，不覆盖），恒显式头、绝对 Destination、
//!   目录腿双侧尾斜杠（附录 C ⑤/⑩）→ 412 → **重 stat 复核后
//!   `Exists`**（K75-1：只认显式 precondition，传输/服务端类绝不映射
//!   Exists）→ 403/409/500（缺父嫌疑三态）→ stat 目标父核实：缺则
//!   隐式建父重试恰一次（仍败 → `NotFound`（父）），在则按 §4.4
//!   通用表归一；
//! - `writer` → [`crate::stager`]（commit-on-close，模块文档）。
//!
//! ## 错误映射（R2，计划 §4.4 表——WD2/WD3 双桩回放钉死）
//!
//! 404→NotFound；401 协商后仍拒/403→Unauthorized{false}；405（MKCOL
//! 目标已存在）→Exists；409（MKCOL/MOVE 父缺失）→隐式建父重试；412→重
//! stat 复核后 Exists（K75-1：传输类绝不映射 Exists）；416→EOF 语义；
//! 200 应答 Range→截断回退；429/5xx→Unavailable；连接类→Unavailable。
//!
//! ## 并发与一致性
//!
//! 全方法可并发调用（`&self`）；单 reqwest Client 池内并发（D6），
//! 8 MiB 串行窗口（§4.7）；一致性来源 = 服务器文件系统语义
//! （authoritative_index 位）。

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;

use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::client::{WebdavClient, ALLPROP_BODY, QUOTA_BODY};
use crate::config::{Vendor, WebdavParams};
use crate::mtime::parse_http_time;
use crate::urls::{collection_url, join_path, percent_decode_lossy};
use crate::xml::{is_addressable_name, PropfindEntry};

/// reader 窗口大小（§4.7：8 MiB 常量——真机吞吐腿校验后可调，不做配置
/// 键；窗口内存有界 ⇒ 120s 读超时安全）。
const READ_WINDOW: u64 = 8 * 1024 * 1024;

/// WebDAV 存储驱动。
///
/// - 语义：远端 WebDAV 树即「云」；写入路径缺失父目录隐式创建
///   （trait 契约）；目录删除递归（WD3 接线）；
/// - 错误：HTTP 形态按计划 §4.4 映射表归一（R2——WD2/WD3 桩回放
///   钉死）；XML/编码解析失败 → `Io` 保留片段；
/// - 并发：全方法可并发调用（`&self` + reqwest 池内并发）；同一
///   stager 串行使用（trait 契约）；
/// - 生命周期：连接/nonce 会话/协商状态归客户端自理（D1/D6）；
///   调用方只负责 stager 的 close/abort。
pub struct WebdavDriver {
    volume: VolumeId,
    /// `Arc` 形态：reader 的窗口流（`stream::unfold` 状态机）需要跨
    /// `await` 持有客户端句柄（流不持 `&self` 借用——K47 流式语义）；
    /// stager 同源共用（WD3）。
    client: Arc<WebdavClient>,
    /// 服务端风味（D4：只影响写面的 mtime 策略——nextcloud 的 stager
    /// PUT 搭车 `X-OC-Mtime`；generic 不写 mtime，D2 降级）。
    vendor: Vendor,
}

impl WebdavDriver {
    /// 同步构造（不碰网络——reqwest 池惰性建连；pan123 `new` 同款
    /// 离线构造面）。卷身份 `webdav:<user>@<url>`（D6：同基地址同
    /// 账号同卷；user 缺省 `anonymous`）。
    ///
    /// D3 开洞 warn 在装配面一次性打出（WD4 从 `WebdavClient::new`
    /// 挪来——doctor 探活的宽校验诊断客户端也建 client，warn 跟着
    /// 用户配置走才不误导）。
    pub fn new(params: WebdavParams) -> Result<Self, StorageError> {
        if params.accept_invalid_certs {
            tracing::warn!(
                target: "ck_webdav::driver",
                url = %params.url,
                "webdav_accept_invalid_certs is enabled: TLS certificates are NOT verified for \
                 this volume (self-signed NAS escape hatch) — do not enable this on untrusted \
                 networks"
            );
        }
        let user = params
            .username
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        let volume = VolumeId::new("webdav", &format!("{user}@{}", params.url))?;
        let client = Arc::new(WebdavClient::new(&params)?);
        Ok(WebdavDriver {
            volume,
            client,
            vendor: params.vendor,
        })
    }

    /// 客户端句柄（transport 面共用同一连接/协商世界）。
    pub(crate) fn client(&self) -> &Arc<WebdavClient> {
        &self.client
    }

    /// 卷内相对路径 → 文件形态 URL（无尾斜杠；卷根 = 基地址尾斜杠形态
    /// ——stat 的 Depth 0 / reader 的 GET / 写动词共用锚；自由函数面
    /// [`url_for`] 的方法壳）。
    fn url_of(&self, rel: &RelPath) -> Result<url::Url, StorageError> {
        url_for(&self.client, rel)
    }

    /// stat 的驱动面（reader 的 stat 先行/416 复核共用）。
    async fn stat_entry(&self, rel: &RelPath) -> Result<Entry, StorageError> {
        let url = self.url_of(rel)?;
        let pe = self.client.stat(&url).await?;
        Ok(entry_from_propfind(&self.volume, rel, &pe))
    }

    /// MKCOL 的驱动半边（恒尾斜杠——附录 C ⑥/⑧；mkdir 与
    /// [`ensure_parents`] 共用）。
    async fn mkcol_rel(&self, rel: &RelPath) -> Result<crate::client::MkcolOutcome, StorageError> {
        let url = collection_url(&self.url_of(rel)?);
        self.client.mkcol(&url).await
    }

    /// MOVE 的驱动半边（rename 与 stager 的 stash/固化腿共用形态：
    /// 恒显式 Overwrite + 绝对 Destination + 目录腿双侧尾斜杠——
    /// 附录 C ⑤/⑩）。
    async fn move_rel(
        &self,
        from: &RelPath,
        to: &RelPath,
        dir_leg: bool,
        overwrite: bool,
    ) -> Result<crate::client::MoveOutcome, StorageError> {
        let mut from_url = self.url_of(from)?;
        let mut to_url = self.url_of(to)?;
        if dir_leg {
            from_url = collection_url(&from_url);
            to_url = collection_url(&to_url);
        }
        self.client.move_(&from_url, &to_url, overwrite).await
    }
}

// ----------------------------------------------- 写面共享助手（stager 共用）---

/// 卷内相对路径 → 文件形态 URL（stager 的 PUT/MOVE/stat 腿共用锚——
/// 错误归一与 [`WebdavDriver::url_of`] 同源）。
pub(crate) fn url_for(client: &WebdavClient, rel: &RelPath) -> Result<url::Url, StorageError> {
    join_path(client.base(), rel.as_str()).map_err(|error| {
        // 不可达防御（词汇层已拒穿越/非法段；`Invalid` 无载荷——诊断进
        // debug 通道）。
        tracing::debug!(
            target: "ck_webdav::driver",
            path = %rel,
            error,
            "webdav URL construction rejected the path"
        );
        StorageError::Invalid
    })
}

/// stat 的自由函数面（stager 的 size 复核/对账腿与 [`ensure_parents`]
/// 的逐级探测共用——PROPFIND 原始条目形态，投影归调用方）。
pub(crate) async fn stat_pe(
    client: &WebdavClient,
    rel: &RelPath,
) -> Result<PropfindEntry, StorageError> {
    client.stat(&url_for(client, rel)?).await
}

/// 隐式建父（trait 契约：写入/建目录路径缺失的父级隐式创建；mkdir 的
/// 409 重试腿与 rename 的缺父重试腿与 stager close 共用）。
///
/// 自浅向深逐级 stat：存在的目录即屏障（更浅层必然存在）；被文件占住
/// 的祖先 → `Exists`（占位冲突）；缺失的祖先自浅向深 MKCOL（此时父级
/// 已核实/已建，MKCOL 的 409 只剩竞态窗——按 `NotFound` 归一（父），
/// §4.4 终局形态）。
pub(crate) async fn ensure_parents(
    client: &WebdavClient,
    rel: &RelPath,
) -> Result<(), StorageError> {
    let Some(deepest) = rel.parent() else {
        return Ok(()); // 根自身：无父可建
    };
    if deepest.is_root() {
        // 卷根 = 挂载基地址——恒按存在处理（其缺失由紧随的动词自己
        // 暴露，省一次 PROPFIND 往返）。
        return Ok(());
    }
    // 自深向浅收集缺失链，直到命中存在的目录（或链尽 = 卷根）。
    let mut missing: Vec<RelPath> = Vec::new();
    let mut cursor = Some(deepest);
    while let Some(dir) = cursor {
        match stat_pe(client, &dir).await {
            Ok(pe) if pe.is_collection => break,
            Ok(_) => return Err(StorageError::Exists), // 文件占住祖先路径
            Err(StorageError::NotFound) => missing.push(dir.clone()),
            Err(other) => return Err(other),
        }
        cursor = dir.parent();
    }
    // 自浅向深补建（missing 是自深向浅收集的 → rev 即自浅向深）。
    for dir in missing.iter().rev() {
        let url = collection_url(&url_for(client, dir)?);
        match client.mkcol(&url).await? {
            crate::client::MkcolOutcome::Created => {}
            crate::client::MkcolOutcome::ParentMissing => {
                // 父级已核实仍在（或刚建）——409 只剩竞态窗：按 §4.4
                // 的「仍败 → NotFound（父）」归一。
                return Err(StorageError::NotFound);
            }
        }
    }
    Ok(())
}

/// [`crate::client::map_status`] 的 u16 包装（`MoveOutcome::ParentSuspect`
/// 携带数字码；403/409/500 在构造面恒可转，防御臂保留原文）。
fn map_status_of(status: u16, verb: &str, diagnostic: &str) -> StorageError {
    match reqwest::StatusCode::from_u16(status) {
        Ok(code) => crate::client::map_status(code, verb, diagnostic),
        Err(_) => StorageError::Unavailable(format!(
            "webdav {verb} failed (HTTP {status}): {diagnostic}"
        )),
    }
}

/// 进程内暂存件序号（`<pid>-<seq>` = 进程内唯一名；跨进程由 pid 区分。
/// 崩溃残留的 `.part`/`.old` 是孤儿暂存件——list 恒过滤，同 pid 序号
/// 复用时 Overwrite:T 清扫）。sftp `staging_names` 同款形态。
fn next_staging_seq() -> u64 {
    static STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// 暂存件命名（§4.6）：`<final>.ckwd-<pid>-<seq><suffix>`
///（suffix ∈ `.part`/`.old`——`is_staging_artifact` 的双条件过滤面）。
pub(crate) fn staging_rel(
    final_rel: &RelPath,
    seq: u64,
    suffix: &str,
) -> Result<RelPath, StorageError> {
    RelPath::new(&format!(
        "{}.ckwd-{}-{}{suffix}",
        final_rel.as_str(),
        std::process::id(),
        seq
    ))
}

/// 句柄 → RelPath（K6 往返：句柄字符串 = rel_path 形态，sftp/pan123
/// 路径句柄先例——**禁 etag 前缀**：etag 非普遍稳定（§4.3 rapid_upload
/// 位依据的同源事实），路径是 WebDAV 的稳定寻址面）。
fn rel_from_handle(id: &EntryId) -> Option<RelPath> {
    RelPath::new(id.handle.as_str()).ok()
}

/// PROPFIND 条目投影 → Entry（stat/list 共用）。
///
/// mtime：`getlastmodified` 经 [`parse_http_time`] 三格式解析；失败 →
/// `debug!` 留痕 + 值落 0.0（**绝不静默 epoch**——OpenList 单格式静默
/// 回 0 的正面修法，§4.5-5：日志就是「不静默」的那一半）。目录的
/// `getcontentlength` 在内层 404 propstat（矩阵⑧）已被 xml 投影过滤
/// → `None` → size 0（毒值 999 不外漏）。
pub(crate) fn entry_from_propfind(volume: &VolumeId, rel: &RelPath, pe: &PropfindEntry) -> Entry {
    let mtime = match pe.last_modified.as_deref() {
        Some(text) => match parse_http_time(text) {
            Some(secs) => secs as f64,
            None => {
                tracing::debug!(
                    target: "ck_webdav::driver",
                    path = %rel,
                    raw = %text,
                    "unparseable getlastmodified; mtime falls back to 0"
                );
                0.0
            }
        },
        None => 0.0,
    };
    Entry {
        id: EntryId::new(volume.clone(), BackendHandle::new(rel.as_str())),
        path: rel.clone(),
        kind: if pe.is_collection {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: if pe.is_collection {
            0
        } else {
            pe.content_length.unwrap_or(0)
        },
        mtime,
    }
}

/// `.ckwd-` 暂存件判定（§4.6/断言③：集合完整性——stager 的 `.part`/
/// stash `.old` 是驱动实现细节，不是卷内容）。**双条件**（sftp
/// `.cksftp-` 同款纪律）：含 `.ckwd-` **且**以 `.part`/`.old` 结尾
/// ——单条件会误伤用户的合法文件名（`notes.ckwd-diary.txt`）。
pub(crate) fn is_staging_artifact(name: &str) -> bool {
    name.contains(".ckwd-") && (name.ends_with(".part") || name.ends_with(".old"))
}

/// 一条 PROPFIND 条目相对「列目录请求」的分类。
enum HrefClass {
    /// 自条目（携带其 collection 形态——文件目标的 `list` → `Invalid`
    /// 判据）。
    Target(bool),
    /// 直接子条目（单组件名——已剔除前导/尾随斜杠）。
    Child(String),
    /// 跳过：他人 href（不在卷基之下）/孙代（Depth 1 不产出，防御）/
    /// 空余量。
    Skip,
}

/// href（已解码）相对「基地址 + 请求目录」的分类（§4.7 剔 self——
/// **容忍尾斜杠差异**：wire href 目录带尾斜杠、请求路径不带）。
///
/// `base_path` 是**解码后**的基地址路径（百分号编码形态与解码 href
/// 不可比——`%20` 与空格互不匹配）。
fn classify_href(base_path: &str, dir: &RelPath, entry: &PropfindEntry) -> HrefClass {
    // 尾斜杠容忍的归一：剥尾斜杠、空串归一为根（"/" 剥完是 ""——
    // 卷根的 href 与请求路径必须仍然命中）。
    let normalize = |path: &str| {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let base = base_path.trim_end_matches('/');
    let target_path = if dir.is_root() {
        if base.is_empty() {
            "/".to_string()
        } else {
            base.to_string()
        }
    } else {
        format!("{base}/{}", dir.as_str())
    };
    let href = normalize(&entry.href);
    if href == target_path {
        return HrefClass::Target(entry.is_collection);
    }
    let prefix = if target_path == "/" {
        "/".to_string()
    } else {
        format!("{target_path}/")
    };
    let Some(rest) = href.strip_prefix(&prefix) else {
        return HrefClass::Skip;
    };
    let name = rest.trim_matches('/');
    if name.is_empty() || name.contains('/') {
        return HrefClass::Skip;
    }
    HrefClass::Child(name.to_string())
}

#[async_trait]
impl StorageDriver for WebdavDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    /// 能力位（R4 逐位依据——计划 §4.3 表行）。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // HTTP Range 原生（计划 §4.3：206 校验 + 200 截断回退，
            // §4.5-6——附录 C ④ 双服务器 206/钳制/416 实证）——支撑
            // K47 流式解密播放。WD2 窗口流接线后 conformance ② 复核。
            range_read: true,
            // 无远端分片会话（计划 §4.3：PUT 整体原子）；上层
            // `.enc.tmp` 承担断点续传。
            resume: false,
            // 无分块上传原语（计划 §4.3：OC-Chunked 明确不做，§7）。
            multipart: false,
            // MOVE 原生 = 服务端单侧 O(1) 移动（计划 §4.3；附录 C ⑤
            // 双服务器实证）。
            server_side_move: true,
            // etag 非普遍稳定，不做内容寻址去重（计划 §4.3/§7）。
            rapid_upload: false,
            // 远端即真相（计划 §4.3：同 local/baidu/sftp/pan115）→
            // rebuild 可用（rebuild.rs 零 backend 分支同构继承）。
            authoritative_index: true,
            // 无变更推送通道（计划 §4.3；WebDAV Sync RFC 6578 不做，
            // §7）。
            change_feed: false,
            // 非 bot 后端：无入站通道。
            inbound: false,
            // 无对话通道。
            chat: false,
            // K4：DELETE 经 transport 面真删（WebDAV DELETE 即终删——
            // 协议无回收站；Nextcloud trashbin 是服务端行为不可依赖，
            // 计划 §4.3 末行）。
            remote_delete: true,
        }
    }

    /// depth-1 列目录（字典序稳定排序 + `off:N` 不透明分页令牌——
    /// PROPFIND 无分页原语，`Page` 在驱动内全量拉取后切片，sftp/local
    /// 同款形态；doc 注明：超大目录的内存驻留 = 全量条目元数据）。
    ///
    /// - 错误：目录不存在 → `NotFound`（外层 404）；目标非集合 →
    ///   `Invalid`（自条目的 resourcetype 判定——trait 契约）；
    /// - 并发：可并发（PROPFIND 无会话态）；
    /// - 生命周期：无状态（每次全量拉取切片）。
    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let url = collection_url(&self.url_of(dir)?);
        let outcome = self
            .client
            .propfind(&url, crate::client::Depth::One, ALLPROP_BODY)
            .await?;
        // href 比对用解码形态（wire 百分号编码与解码 href 不可比）。
        let base_decoded = percent_decode_lossy(self.client.base().path());
        let mut target_kind: Option<bool> = None;
        let mut entries: Vec<Entry> = Vec::new();
        for pe in &outcome.entries {
            match classify_href(&base_decoded, dir, pe) {
                HrefClass::Target(is_collection) => target_kind = Some(is_collection),
                HrefClass::Skip => {}
                HrefClass::Child(name) => {
                    // K67「list 产出即可寻址」：`\`/`\0`/lossy 形态不可见
                    //（xml.rs 同源纪律）。
                    if !is_addressable_name(&name) {
                        continue;
                    }
                    // 驱动暂存件是实现细节，不是卷内容（断言③）。
                    if is_staging_artifact(&name) {
                        continue;
                    }
                    // 词汇层不可表示的残余形态（防御——Depth 1 的子名
                    // 已无 `/`，此处仅挡极端编码残片）。
                    let Ok(child) = dir.join(&name) else {
                        continue;
                    };
                    entries.push(entry_from_propfind(&self.volume, &child, pe));
                }
            }
        }
        match target_kind {
            // 自条目缺席 = 无法验证目标形态（RFC 4918 Depth 1 恒含自
            // 条目；缺席视作目标不存在）。
            Some(true) => {}
            Some(false) => return Err(StorageError::Invalid), // 文件不是目录
            None => return Err(StorageError::NotFound),
        }
        // RelPath 字典序稳定排序（trait 契约；PROPFIND 返回序是实现细节）。
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let total = entries.len();
        let offset = match page.cursor {
            PageCursor::Start => 0usize,
            // 伪令牌回退 0 重放（local/sftp 同款容错）。
            PageCursor::Next(token) => token
                .strip_prefix("off:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        };
        let offset = offset.min(total);
        let end = offset.saturating_add(page.limit).min(total);
        let next = (end < total).then(|| PageCursor::Next(format!("off:{end}")));
        Ok(Listing {
            entries: entries.drain(offset..end).collect(),
            next,
        })
    }

    /// stat（PROPFIND Depth 0；404 → `NotFound`；`resourcetype/
    /// collection` 判目录；`getcontentlength` 缺失按 0——§4.7；apache
    /// SlashStrict 的集合 301 由 client 尾斜杠重试腿吸收）。
    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        self.stat_entry(path).await
    }

    /// mkdir（stat 预检先行——rclone 已存在 201 幂等陷阱，附录 C ⑥——
    /// baidu 同型先例；MKCOL 恒尾斜杠；409 父缺失 → 隐式建父重试
    /// 一次，§4.4；重试仍败 → `NotFound`（父））。已存在（目录或文件
    /// 占位）→ `Exists`（断言④）。
    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        match self.stat_entry(path).await {
            // 预检：已存在（无论形态）→ Exists——不赌服务器的「已存在」
            // 回应形态（rclone 201 / apache 405 两真形）。
            Ok(_) => return Err(StorageError::Exists),
            Err(StorageError::NotFound) => {}
            Err(other) => return Err(other),
        }
        match self.mkcol_rel(path).await? {
            crate::client::MkcolOutcome::Created => Ok(()),
            crate::client::MkcolOutcome::ParentMissing => {
                // 409：父集合缺失——隐式建父后重试恰一次（§4.4）。
                ensure_parents(&self.client, path).await?;
                match self.mkcol_rel(path).await? {
                    crate::client::MkcolOutcome::Created => Ok(()),
                    // 仍 409 = 建父与重试之间的竞态/父仍不可建——§4.4
                    // 终局形态：NotFound（父）。Err 通道（含竞态 405→
                    // Exists）按动词映射原样上浮。
                    crate::client::MkcolOutcome::ParentMissing => Err(StorageError::NotFound),
                }
            }
        }
    }

    /// 按句柄删除；目录递归（DELETE 恒尾斜杠——附录 C ⑦）。**幂等形态
    /// 声明：不存在 → 恒 `NotFound`**（stat 预检天然给出，sftp 先例同
    /// 款并恒定；stat 与 DELETE 之间被并发删除的 404 亦同归 NotFound）。
    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄（trait 契约）
        }
        let Some(rel) = rel_from_handle(id) else {
            return Err(StorageError::Invalid);
        };
        if rel.is_root() {
            return Err(StorageError::Invalid); // 卷根不可删
        }
        // stat 预检：①目录腿需要尾斜杠判定（apache no-slash 集合 301
        // 不执行——附录 C ⑦）；②不存在的幂等形态在此归一。
        let entry = self.stat_entry(&rel).await?;
        let url = if entry.kind == EntryKind::Dir {
            collection_url(&self.url_of(&rel)?)
        } else {
            self.url_of(&rel)?
        };
        self.client.delete(&url).await
    }

    /// rename（MOVE = 服务端单侧移动，`server_side_move` 依据）。
    ///
    /// - **Overwrite 选型 = `F`**（trait 契约与 sftp 先例：目标存在 →
    ///   `Exists`，rename 不做覆盖语义——目标是调用方的显式错误）；
    /// - 恒显式 `Overwrite` 头 + 绝对 `Destination` + 目录腿（stat from
    ///   判 is_dir）双侧尾斜杠（附录 C ⑤/⑩）；
    /// - 412 → 重 stat 复核后 `Exists`（K75-1：只认显式 precondition，
    ///   传输/服务端类绝不映射 Exists）；
    /// - 403/409/500（缺父嫌疑三态：rclone403 / apache500 / RFC 409）→
    ///   stat 目标父核实：缺 → 隐式建父重试恰一次（仍败 → `NotFound`
    ///   （父），§4.4 行）；在 → 按 §4.4 通用表归一（403→Unauthorized /
    ///   409→Invalid / 500→Unavailable）；
    /// - `to` 是 `from` 的后代 → `Invalid`（自搬进自身，sftp 先例）；
    ///   任一端点是卷根 → `Invalid`。
    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        if from.is_root() || to.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为 rename 端点
        }
        let from_prefix = format!("{}/", from.as_str());
        if to.as_str().starts_with(&from_prefix) {
            return Err(StorageError::Invalid); // to 在 from 之内
        }
        // 源先行（语义早失败——NotFound 归 stat）+ 目录腿判定。
        let from_entry = self.stat_entry(from).await?;
        let dir_leg = from_entry.kind == EntryKind::Dir;
        match self.move_rel(from, to, dir_leg, false).await {
            Ok(crate::client::MoveOutcome::Done) => Ok(()),
            Ok(crate::client::MoveOutcome::PreconditionFailed { diagnostic }) => {
                // 412 = Overwrite:F 撞既有目标：重 stat 复核后 Exists——
                // 目标在 = 真撞；不在 = 状态异常（保留 412 原文）。
                match self.stat_entry(to).await {
                    Ok(_) => Err(StorageError::Exists),
                    Err(StorageError::NotFound) => Err(StorageError::Unavailable(format!(
                        "webdav MOVE failed with 412 Precondition Failed but the destination \
                         is absent: {diagnostic}"
                    ))),
                    Err(other) => Err(StorageError::Unavailable(format!(
                        "webdav MOVE failed with 412 Precondition Failed and the destination \
                         re-check errored ({other}): {diagnostic}"
                    ))),
                }
            }
            Ok(crate::client::MoveOutcome::ParentSuspect { status, diagnostic }) => {
                // 缺父嫌疑核实：父在 → 非缺父形态（403 的真拒绝 / 500 的
                // 服务端故障），按通用表归一——绝不折叠 Exists（K75-1）。
                let parent = to.parent().ok_or(StorageError::Invalid)?;
                match self.stat_entry(&parent).await {
                    Err(StorageError::NotFound) => {}
                    Err(other) => return Err(other),
                    Ok(parent_entry) if parent_entry.kind == EntryKind::Dir => {
                        return Err(map_status_of(status, "MOVE", &diagnostic))
                    }
                    // 父路径被文件占住：隐式建父不可行——按「父不可用」归
                    // Exists（占位冲突，与 mkdir 的同型判定一致）。
                    Ok(_) => return Err(StorageError::Exists),
                }
                ensure_parents(&self.client, to).await?;
                // 重试恰一次；仍败 → NotFound（父）（§4.4 终局形态——
                // 竞态窗内的新失败不逐类细分，调用方按「路径当前不可
                // 达」处置）。
                match self.move_rel(from, to, dir_leg, false).await {
                    Ok(crate::client::MoveOutcome::Done) => Ok(()),
                    _ => Err(StorageError::NotFound),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 流式读（stat 先行 → 8 MiB 串行窗口循环 → 206 校验/200 截断
    /// （client.get_range，§4.4/§4.5-6）；`start >= size` → 空流不开
    /// GET——§4.7；窗口间不跨窗重试（模块文档注记）；416 → stat 复核
    /// 的 EOF/`Unavailable` 分叉）。
    ///
    /// - 错误：句柄不存在 → `NotFound`；他卷句柄 → `NotFound`；句柄
    ///   指向目录 → `Invalid`（trait 契约）；传输错误在流中途浮现
    ///   （`Err` 帧，ByteStream 契约）；
    /// - 并发：可并发；
    /// - 生命周期：流消费到底/中途 Drop 均无远端残留（窗口 GET 无句柄
    ///   态——HTTP 语义天然如此，sftp「三硬仗①」的 awaited-close 在
    ///   HTTP 面无对应物）。
    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄（trait 契约）
        }
        let rel = rel_from_handle(id).ok_or(StorageError::Invalid)?;
        let entry = self.stat_entry(&rel).await?;
        if entry.kind == EntryKind::Dir {
            return Err(StorageError::Invalid); // 句柄指向目录（trait 契约）
        }
        let size = entry.size;
        // 窗口：None 全量；Some 按半开 + 越界钳制（sftp/local 同款）。
        let (start, len) = match range {
            None => (0, size),
            Some(r) => (r.start, r.clamped_len(size)),
        };
        if len == 0 {
            // 空窗口/空文件/start >= size：空流不开 GET（§4.7——最好
            // 的「不开」就是不发）。
            let empty: Vec<Result<Bytes, StorageError>> = Vec::new();
            return Ok(Box::pin(stream::iter(empty)));
        }
        let url = self.url_of(&rel)?;
        let client = self.client.clone();
        let frames = stream::unfold(
            Some((client, url, start, start + len)),
            |state| async move {
                let (client, url, pos, end) = state?;
                if pos >= end {
                    return None;
                }
                let win_end = (pos + READ_WINDOW).min(end);
                match client.get_range(&url, Some((pos, win_end))).await {
                    Ok(Some(bytes)) => {
                        let consumed = bytes.len() as u64;
                        Some((Ok(bytes), Some((client, url, pos + consumed, end))))
                    }
                    Ok(None) => {
                        // 416 复核（§4.4）：stat 新鲜查询——offset 仍越过 =
                        // 并发收缩，EOF 空收尾；否则形态异常（`Unavailable`
                        // 保留原文，K75-1 纪律：传输类绝不映射别的）。
                        match client.stat(&url).await {
                            Ok(fresh) if fresh.content_length.unwrap_or(0) <= pos => None,
                            Ok(_) => Some((
                                Err(StorageError::Unavailable(format!(
                                "webdav window GET at offset {pos} returned 416 while the file \
                                 still extends past it (concurrent mutation?)"
                            ))),
                                None,
                            )),
                            Err(error) => Some((Err(error), None)),
                        }
                    }
                    // 窗口间不跨窗重试：单窗内 client 白名单已尽，错误如实
                    // 浮现为流内 `Err` 帧（模块文档注记——调用方重开 reader
                    // 即恢复）。
                    Err(error) => Some((Err(error), None)),
                }
            },
        );
        Ok(Box::pin(frames))
    }

    /// 打开上传暂存器（commit-on-close——[`crate::stager`]；hint.size
    /// 超承诺 → `Invalid`，sftp 同款契约）。
    ///
    /// **stash 裁决在打开时**（断言①覆盖写腿，sftp `.old` 判例）：目标
    /// 已存在（文件）→ 先 `MOVE final → .ckwd-*.old`——staging 窗口内
    /// 旧对象不可见，close 成功删 stash、abort 恢复；目标是目录 →
    /// `Invalid`（目录不可被文件覆盖写）。
    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        if path.is_root() {
            return Err(StorageError::Invalid); // 卷根不可作为上传目标
        }
        let seq = next_staging_seq();
        let stash_rel = match self.stat_entry(path).await {
            Err(StorageError::NotFound) => None,
            Ok(entry) if entry.kind == EntryKind::Dir => {
                return Err(StorageError::Invalid);
            }
            Ok(_) => {
                // 覆盖写：旧对象先 stash 成 .old（Overwrite:T——清扫上次
                // 崩溃残留的同名孤儿 stash；staging 窗口内目标不可见。
                // 断言①实测裁决（WD3，dav-server 参照桩）：无 stash 形态
                // 红证在案——close 前旧对象可见；stash 上车后绿）。
                let stash = staging_rel(path, seq, ".old")?;
                match self.move_rel(path, &stash, false, true).await {
                    Ok(crate::client::MoveOutcome::Done) => Some(stash),
                    Ok(other) => return Err(other.into_storage_error()),
                    Err(error) => return Err(error),
                }
            }
            Err(other) => return Err(other),
        };
        Ok(Box::new(crate::stager::WebdavStager::new(
            self.client.clone(),
            self.volume.clone(),
            self.vendor,
            path.clone(),
            stash_rel,
            seq,
            hint.size,
        )?))
    }

    /// 卷配额：RFC 4331 `quota-*-bytes` PROPFIND best-effort，失败
    /// `total = None` 降级（附录 C ⑪：双 fixture 均内层 404——`None`
    /// 是实证常态而非异常路径；`debug!` 留痕不留障）。
    async fn quota(&self) -> Result<Quota, StorageError> {
        let url = collection_url(&self.url_of(&RelPath::root())?);
        let degraded = |detail: &str| {
            tracing::debug!(
                target: "ck_webdav::driver",
                detail,
                "webdav quota probe unavailable; degrading to total=unknown"
            );
            Quota {
                total: None,
                used: 0,
            }
        };
        let outcome = match self
            .client
            .propfind(&url, crate::client::Depth::Zero, QUOTA_BODY)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => return Ok(degraded(&error.to_string())),
        };
        let Some(entry) = outcome.entries.first() else {
            return Ok(degraded("multistatus carried no entries"));
        };
        match (entry.quota_used_bytes, entry.quota_available_bytes) {
            (Some(used), Some(available)) => Ok(Quota {
                total: Some(used + available),
                used,
            }),
            // 内层 404（矩阵⑪）或单边缺 prop——任一缺失即整体降级
            //（半份配额比没有更误导）。
            _ => Ok(degraded("quota props absent or partially reported")),
        }
    }
}
