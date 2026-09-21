//! WebdavDriver——WebDAV 存储驱动（Phase 7 / WD1a 骨架）。
//!
//! 后端 = 一台 WebDAV 服务器上的一个（可含子路径的）基地址——
//! 「通用挂载协议」面：rclone serve / OpenList/Alist / Nextcloud /
//! 群晖 DSM / Apache mod_dav / 本仓 cloudkit-webdav 自举，全部成为
//! 可用后端（计划 §0 定位）。
//!
//! **批次边界（WD1a）**：能力位与卷身份本批钉死；九方法动词面占位
//! （读路径 WD2、写路径 WD3 接线——各方法 `TODO(wd2)`/`TODO(wd3)` 锚）；
//! 纯函数层（配置解析/URL 构造/mtime 三格式/challenge 解析/multistatus
//! 解析）单测在 lib.rs 钉死。
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

use async_trait::async_trait;

use cloudkit_storage::{
    ByteStream, Capabilities, Entry, EntryId, Listing, Page, Quota, Range, RelPath, StorageDriver,
    StorageError, UploadStager, VolumeId, WriteHint,
};

use crate::client::WebdavClient;
use crate::config::WebdavParams;

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
    client: WebdavClient,
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
        let client = WebdavClient::new(&params)?;
        Ok(WebdavDriver { volume, client })
    }

    /// 客户端句柄（transport 面共用同一连接/协商世界；WD2 接线）。
    #[allow(dead_code)] // WD1a 骨架：动词面 WD2 接线后进入使用
    pub(crate) fn client(&self) -> &WebdavClient {
        &self.client
    }
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

    /// 列目录（PROPFIND Depth 1 → 剔 self → href 解码 → 名字典序稳定
    /// 排序 → K67 过滤 → `Page` 切片；`Page` 在驱动内消化——PROPFIND
    /// 无分页原语，§4.7）。
    ///
    /// - 错误：目录不存在 → `NotFound`；他是文件 → `Invalid`（WD2）；
    /// - 并发：可并发；
    /// - 生命周期：无状态（每次全量拉取切片）。
    async fn list(&self, _dir: &RelPath, _page: Page) -> Result<Listing, StorageError> {
        // TODO(wd2): 读路径批接线（xml 增强面 + 过滤纪律）。
        Err(StorageError::Unsupported)
    }

    /// stat（PROPFIND Depth 0；404 → `NotFound`；`resourcetype/
    /// collection` 判目录；`getcontentlength` 缺失按 0——§4.7）。
    async fn stat(&self, _path: &RelPath) -> Result<Entry, StorageError> {
        // TODO(wd2): 读路径批接线。
        Err(StorageError::Unsupported)
    }

    /// mkdir（stat 预检——rclone 已存在 201 幂等陷阱，附录 C ⑥——
    /// baidu 同型先例；MKCOL 恒尾斜杠；409 父缺失 → 隐式建父重试
    /// 一次，§4.4）。已存在 → `Exists`（断言④）。
    async fn mkdir(&self, _path: &RelPath) -> Result<(), StorageError> {
        // TODO(wd2): 写路径批接线（WD3 stager 前置依赖）。
        Err(StorageError::Unsupported)
    }

    /// 按句柄删除；目录递归（DELETE 恒尾斜杠——附录 C ⑦）。声明幂等
    /// 形态：不存在 → `Ok(())`（WD2/WD3 裁决落文档）。
    async fn delete(&self, _id: &EntryId) -> Result<(), StorageError> {
        // TODO(wd2): 读路径批接线。
        Err(StorageError::Unsupported)
    }

    /// rename（MOVE = 服务端单侧移动，`server_side_move` 依据；恒显式
    /// `Overwrite` + 绝对 `Destination` + 目录腿全尾斜杠——附录 C ⑤/⑩；
    /// 412 → 重 stat 复核后 `Exists`，K75-1）。
    async fn rename(&self, _from: &RelPath, _to: &RelPath) -> Result<(), StorageError> {
        // TODO(wd2): 写路径批接线。
        Err(StorageError::Unsupported)
    }

    /// 流式读（stat 先行 → 8 MiB 串行窗口循环 → 206 校验/200 截断
    /// （§4.4/§4.5-6）；`start >= size` → 空流不开 GET——§4.7）。
    async fn reader(
        &self,
        _id: &EntryId,
        _range: Option<Range>,
    ) -> Result<ByteStream, StorageError> {
        // TODO(wd2): 窗口流批接线。
        Err(StorageError::Unsupported)
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
    /// `total = None` 降级（附录 C ⑪：双 fixture 均内层 404——
    /// `None` 是实证常态而非异常路径）。
    async fn quota(&self) -> Result<Quota, StorageError> {
        Ok(Quota {
            total: None,
            used: 0,
        })
    }
}
