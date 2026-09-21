//! WebdavDriver——WebDAV 存储驱动（Phase 7 / WD2b：读面全接线）。
//!
//! 后端 = 一台 WebDAV 服务器上的一个（可含子路径的）基地址——
//! 「通用挂载协议」面：rclone serve / OpenList/Alist / Nextcloud /
//! 群晖 DSM / Apache mod_dav / 本仓 cloudkit-webdav 自举，全部成为
//! 可用后端（计划 §0 定位）。
//!
//! **批次边界（WD2b）**：读路径（stat/list/reader/quota）+ transport
//! 读面全接线；写路径（mkdir/delete/rename/writer + client 的 MKCOL/
//! MOVE/DELETE/PROPPATCH 动词）留 WD3（各方法 `TODO(wd3)` 锚）。
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
//! ## commit-on-close（WD3 接线，[`crate::stager`]）
//!
//! spool → `PUT <final>.ckwd-<pid>-<seq>.part`（Content-Length 已知，
//! D5）→ `MOVE` 固化 → stat size 复核；覆盖写 stash 预案与断言①实测
//! 裁决在 WD3（计划 §4.6）。`.ckwd-` 暂存件对 `list` 恒过滤。
//!
//! ## 错误映射（R2，计划 §4.4 表——WD2/WD3 双桩回放钉死）
//!
//! 404→NotFound；401 协商后仍拒/403→Unauthorized{false}；405（MKCOL
//! 目标已存在）→Exists；409（MKCOL 父缺失）→隐式建父重试；412→重
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
use crate::config::WebdavParams;
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
    /// `await` 持有客户端句柄（流不持 `&self` 借用——K47 流式语义）。
    client: Arc<WebdavClient>,
}

impl WebdavDriver {
    /// 同步构造（不碰网络——reqwest 池惰性建连；pan123 `new` 同款
    /// 离线构造面）。卷身份 `webdav:<user>@<url>`（D6：同基地址同
    /// 账号同卷；user 缺省 `anonymous`）。
    pub fn new(params: WebdavParams) -> Result<Self, StorageError> {
        let user = params
            .username
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        let volume = VolumeId::new("webdav", &format!("{user}@{}", params.url))?;
        let client = Arc::new(WebdavClient::new(&params)?);
        Ok(WebdavDriver { volume, client })
    }

    /// 客户端句柄（transport 面共用同一连接/协商世界）。
    pub(crate) fn client(&self) -> &Arc<WebdavClient> {
        &self.client
    }

    /// 卷内相对路径 → 文件形态 URL（无尾斜杠；卷根 = 基地址尾斜杠形态
    /// ——stat 的 Depth 0 / reader 的 GET / WD3 写动词共用锚）。
    fn url_of(&self, rel: &RelPath) -> Result<url::Url, StorageError> {
        join_path(self.client.base(), rel.as_str()).map_err(|error| {
            // 不可达防御（词汇层已拒穿越/非法段；`Invalid` 无载荷——
            // 诊断进 debug 通道）。
            tracing::debug!(
                target: "ck_webdav::driver",
                path = %rel,
                error,
                "webdav URL construction rejected the path"
            );
            StorageError::Invalid
        })
    }

    /// stat 的驱动面（reader 的 stat 先行/416 复核共用）。
    async fn stat_entry(&self, rel: &RelPath) -> Result<Entry, StorageError> {
        let url = self.url_of(rel)?;
        let pe = self.client.stat(&url).await?;
        Ok(entry_from_propfind(&self.volume, rel, &pe))
    }

    /// Test seam（baidu `build_backend_transport_with_endpoints` 先例）：
    /// WD2b 重试白名单的**写侧**断言面——「PUT 永不自动重试」需要直测
    /// client 动词，而 driver 写面（stager）要到 WD3 才落地。不是生产
    /// 路径；WD3 writer 落地后由正式写面取代并随批移除。
    #[doc(hidden)]
    pub async fn test_put(&self, path: &RelPath, body: Bytes) -> Result<(), StorageError> {
        let url = self.url_of(path)?;
        self.client.put(&url, body).await
    }

    /// Test seam（同上）：越窗 GET 的协议半边——driver reader 的窗口恒
    /// 在 stat 尺寸内，416 只能由陈旧尺寸触发，复核腿的客户端侧（416 →
    /// `Ok(None)` passthrough）在此直测。
    #[doc(hidden)]
    pub async fn test_get_range(
        &self,
        path: &RelPath,
        window: Option<(u64, u64)>,
    ) -> Result<Option<Bytes>, StorageError> {
        let url = self.url_of(path)?;
        self.client.get_range(&url, window).await
    }
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

    /// mkdir（stat 预检——rclone 已存在 201 幂等陷阱，附录 C ⑥——
    /// baidu 同型先例；MKCOL 恒尾斜杠；409 父缺失 → 隐式建父重试
    /// 一次，§4.4）。已存在 → `Exists`（断言④）。
    async fn mkdir(&self, _path: &RelPath) -> Result<(), StorageError> {
        // TODO(wd3): 写路径批接线（stager 前置依赖）。
        Err(StorageError::Unsupported)
    }

    /// 按句柄删除；目录递归（DELETE 恒尾斜杠——附录 C ⑦）。声明幂等
    /// 形态：不存在 → `NotFound`（WD3 写面接线时随批落文档终稿）。
    async fn delete(&self, _id: &EntryId) -> Result<(), StorageError> {
        // TODO(wd3): 写路径批接线。
        Err(StorageError::Unsupported)
    }

    /// rename（MOVE = 服务端单侧移动，`server_side_move` 依据；恒显式
    /// `Overwrite` + 绝对 `Destination` + 目录腿全尾斜杠——附录 C ⑤/⑩；
    /// 412 → 重 stat 复核后 `Exists`，K75-1）。
    async fn rename(&self, _from: &RelPath, _to: &RelPath) -> Result<(), StorageError> {
        // TODO(wd3): 写路径批接线。
        Err(StorageError::Unsupported)
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
    async fn writer(
        &self,
        _path: &RelPath,
        _hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        // TODO(wd3): 写路径批接线（spool → PUT .part → MOVE → size 复核）。
        Err(StorageError::Unsupported)
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
