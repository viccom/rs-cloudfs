//! # ck-pan123——123 云盘 web API 存储驱动（L1 驱动 crate，Phase 6）。
//!
//! 后端 = 一个 123 账号的可设置根（`pan123_root`，folder id，缺省
//! `"0"` = 网盘根，Phase 5 D3 同形态）。认证 = web API 路线（D1/K64：
//! QR 扫码或密码 sign_in → 90 天 token；**无 refresh 机制**——K76.4：
//! 失效即 `Unauthorized{recoverable:false}` + 重扫码指引）。协议实现
//! 自 123-0 spike 真机事实（`examples/pan123_spike` + 跟踪单 123-0
//! 批次日志）：envelope 双成功码（`code==0` 通用 / `code==200` 仅认证
//! 类）+ 顶层与字段双拼解析、dydomain 动态域名 + 粘性 fallback、web
//! 身份头（D5：无安卓头、无签名）。
//!
//! ## 批次边界（123-1…123-5 全批次完成，2026-09-20）
//!
//! - 认证/分类/域名面（123-1）：TDD 钉死（`tests/oauth_state_machine.rs`
//!   / `tests/errno_mapping.rs` / `tests/api_client.rs`）；123-2 扩充
//!   读写面 errno（5060/5113/5114/-1/400）+ 令牌桶限流 + HTTP 重试
//!   退避（`tests/limiter.rs` / `tests/api_retry.rs`）；
//! - 读路径（123-2）：list/stat/mkdir/delete/rename/reader/quota——
//!   `tests/read_path.rs` / `tests/download_hops.rs`；
//! - **写路径全接线（123-3）**：commit-on-close stager + 七步提交链
//!   （MD5 预计算 / Reuse 秒传 / 5060→duplicate=2 覆盖 / resume 差集 /
//!   size 校验）——[`upload`] 模块 + `tests/write_path.rs` 桩矩阵；
//!   `RapidUpload` 可选 trait **不接**（无驱动先例——秒传由服务端
//!   `Reuse` 自动完成，`WriteHint` 仅观察面；driver-onboarding §2.9
//!   可选项）；
//! - conformance + 12 装配点（123-4）：离线八断言全绿；真机矩阵
//!   （123-5）上传回读 / Range / 秒传 / 多分片 / 加密全栈 / WebDAV
//!   双模式全过——`cloudkit-cli` 的 dispatch 臂经
//!   [`Pan123Driver::connect`] 生产可达。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate
//! （driver-onboarding §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。

pub mod api;
pub mod download;
pub mod limiter;
pub mod models;
pub mod oauth;
pub mod pathcache;
pub mod transport_face;
pub mod upload;

use std::sync::Arc;

use async_trait::async_trait;
use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

pub use api::{Pan123Client, UA};
pub use oauth::TokenStore;
pub use transport_face::Pan123Transport;

/// 生产主域（dydomain 真机回值；`api::DEFAULT_PRIMARY_BASE` 的 lib 面
/// 再导出——ck-pan115 基常量同款布局）。
pub const DEFAULT_API_BASE: &str = api::DEFAULT_PRIMARY_BASE;
/// 备域（粘性 fallback；`api::DEFAULT_FALLBACK_BASE` 再导出）。
pub const DEFAULT_FALLBACK_BASE: &str = api::DEFAULT_FALLBACK_BASE;
/// 扫码登录域（QR 三端点专用；`oauth` 面的注入常量）。
pub const DEFAULT_LOGIN_BASE: &str = "https://login.123pan.com";
/// 卷根缺省（D3 沿用 Phase 5 拍板形态：folder id `"0"` = 网盘根；
/// 可设置）。
pub const DEFAULT_ROOT: &str = "0";

/// 123 驱动参数（driver-onboarding §4 的「配置 map → 参数结构体」形态；
/// 123-4 装配批把 config 键 + env 解析为本结构体注入）。
///
/// 不实现 `Debug`：结构体携带凭据（token），派生展开有把凭据印进
/// 日志的风险（R3——BaiduParams/Pan115Params 同款裁决）。
#[derive(Clone)]
pub struct Pan123Params {
    /// 登录 token（扫码/密码登录产出；90 天）。
    pub token: Option<String>,
    /// 卷根（folder id；缺省 [`DEFAULT_ROOT`]）。
    pub root: String,
    /// 主域 base（默认生产常量；测试注入 mock）。
    pub api_base: String,
    /// 备域 base（粘性 fallback；测试注入 mock）。
    pub fallback_base: String,
    /// 扫码登录域 base（QR 三端点；测试注入 mock）。
    pub login_base: String,
    /// 限流参数（`None` = 生产保守缺省 ~2rps；测试注入毫秒级节拍——
    /// RebuildTuning 式结构注入）。
    pub limiter: Option<limiter::LimiterConfig>,
    /// HTTP 重试/退避参数（`None` = §5.15 实测常量；测试注入毫秒级）。
    pub retry: Option<api::RetryConfig>,
    /// 会话/spool 目录（`None` = 系统临时目录且会话仅内存层；生产装配
    /// 给卷家目录——K21 锚形态，pan115 `sessions_dir` 同义）。
    pub sessions_dir: Option<std::path::PathBuf>,
}

impl Default for Pan123Params {
    fn default() -> Self {
        Pan123Params {
            token: None,
            root: DEFAULT_ROOT.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            fallback_base: DEFAULT_FALLBACK_BASE.to_string(),
            login_base: DEFAULT_LOGIN_BASE.to_string(),
            limiter: None,
            retry: None,
            sessions_dir: None,
        }
    }
}

impl Pan123Params {
    /// 从展平的配置键值对解析（纯函数：无 IO、无网络；Pan115Params
    /// 同款第二道门——core `KNOWN_TOML_KEYS` 挡未知键之后，这里再挡
    /// 拼写错误的 pan123 键与非法值）。
    ///
    /// - 键集 = 两个 `pan123_*` 键；未知键 → `Invalid`；
    /// - `pan123_root` 须为纯数字 folder id（与 core `validate()` 同
    ///   规则）；
    /// - token 键空串视为未设置（empty-means-unset，baidu 键同语义）。
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Self, StorageError> {
        let mut token: Option<String> = None;
        let mut root: Option<String> = None;
        for (key, value) in pairs {
            let non_empty = || Some(value.clone()).filter(|v| !v.is_empty());
            match key.as_str() {
                "pan123_token" => token = non_empty(),
                "pan123_root" => root = non_empty(),
                other => {
                    tracing::warn!(
                        target: "ck_pan123::config",
                        "unknown pan123 key {other:?}: the accepted keys are pan123_token, \
                         pan123_root"
                    );
                    return Err(StorageError::Invalid);
                }
            }
        }
        let root = root.unwrap_or_else(|| DEFAULT_ROOT.to_string());
        if root.is_empty() || !root.bytes().all(|b| b.is_ascii_digit()) {
            tracing::warn!(
                target: "ck_pan123::config",
                "pan123_root must be a numeric 123pan folder id (\"0\" is the netdisk root), \
                 got {root:?}"
            );
            return Err(StorageError::Invalid);
        }
        Ok(Pan123Params {
            token,
            root,
            ..Pan123Params::default()
        })
    }
}

/// 123 存储驱动（123-3 写路径接线后读写两路全通）。
///
/// - 语义：读路径全接线（list 分页合并稳定排序 / stat list-walk 末级
///   新鲜 / mkdir 预检+隐式父级 / trash+回读校验 / rename 文件与目录
///   同端点（任务 0 实证）+ mod_pid 跨父 / reader 三跳流读）；写路径
///   commit-on-close（七步链 / Reuse 秒传 / duplicate=2 覆盖 / resume
///   差集——[`upload`] 模块）；
/// - 错误：协议面分类表见 [`api`]（认证面 + 读写面）；5060 在 writer
///   面内部以 duplicate=2 消化（对外 Exists 语义只在 mkdir 面）；
/// - 并发：全方法可并发调用（`&self`）；无刷新状态机（web API 无
///   refresh——K76.4）；
/// - 生命周期：HTTP client 无连接态；域名粘性状态、令牌桶与 token
///   共享于 [`Pan123Client`]；ByteStream 任务经 Arc 持有 client（流
///   生命周期独立于 `&self`，pan115/baidu 同款）。
pub struct Pan123Driver {
    volume: VolumeId,
    /// 共享 HTTP 面（CDN 流任务与上传 stager 经 Arc 持有）。
    client: Arc<Pan123Client>,
    params: Pan123Params,
    /// 路径 → file_id 解析缓存（list/stat 顺带喂；stager close 落库后
    /// 就近失效——经 Arc 与 stager 共享）。
    paths: Arc<pathcache::PathCache>,
    /// file_id → 最终应答 URL 的 TTL 缓存（与流任务经 Arc 共享）。
    dlinks: Arc<download::DlinkCache>,
    /// 上传会话表（resume 差集资产，与 stager 经 Arc 共享）。
    sessions: Arc<upload::SessionStore>,
}

impl Pan123Driver {
    /// 同步构造（不连网——dydomain 在首请求前惰性解析；ck-pan115
    /// 骨架同款形态）。
    ///
    /// token 必须齐备（初始值来自 setup 扫码/sign_in 或手填配置；
    /// 缺失 → `Invalid`——core `validate()` 的第一道门之外，这里是
    /// 驱动侧的第二道）。
    ///
    /// **VolumeId 离线形态**：`pan123:pending`——真值 `pan123:<uid>`
    /// （uid 取自 `/b/api/user/info` 的 `UID`）由 [`Pan123Driver::connect`]
    /// 确定（baidu factory connect 取 uid 先例）。生产装配恒走 `connect`，
    /// 占位身份只在离线测试与 conformance 桩中复用。
    pub fn new(params: Pan123Params) -> Result<Self, StorageError> {
        let Some(token) = &params.token else {
            return Err(StorageError::Invalid);
        };
        let limiter = params.limiter.unwrap_or_default();
        let retry = params.retry.unwrap_or_default();
        let sessions = Arc::new(upload::SessionStore::new(params.sessions_dir.clone()));
        let client = Arc::new(Pan123Client::with_tuning(
            token.clone(),
            params.api_base.clone(),
            params.fallback_base.clone(),
            None,
            limiter,
            retry,
        )?);
        Ok(Pan123Driver {
            volume: VolumeId::new("pan123", "pending")?,
            client,
            params,
            paths: Arc::new(pathcache::PathCache::new()),
            dlinks: Arc::new(download::DlinkCache::new()),
            sessions,
        })
    }

    /// 连接构造：`/b/api/user/info` 取 uid → VolumeId `pan123:<uid>`
    /// （K5；baidu/pan115 `connect` 同形——uid 是账号级身份）。
    ///
    /// 123-4 装配批的工厂走本入口；[`Pan123Driver::new`] 保持离线形态
    /// （占位 VolumeId）供纯本地测试与 conformance 桩复用。
    pub async fn connect(params: Pan123Params) -> Result<Self, StorageError> {
        let mut driver = Pan123Driver::new(params)?;
        let info = driver.client.user_info().await?;
        if info.uid <= 0 {
            return Err(StorageError::Unavailable(
                "user/info: UID missing".to_string(),
            ));
        }
        driver.volume = VolumeId::new("pan123", &info.uid.to_string())?;
        Ok(driver)
    }

    /// api client 的公开访问面（123-4 的 transport/doctor 面共用同一
    /// 域名/token/限流世界）。
    pub fn client(&self) -> &Pan123Client {
        &self.client
    }

    /// 驱动参数（路径面消费 root；只读）。
    pub fn params(&self) -> &Pan123Params {
        &self.params
    }

    // ---- upload.rs（写路径）的内部访问面 ------------------------------

    /// 共享 HTTP 面（upload.rs 的 stager 构造面）。
    pub(crate) fn client_arc(&self) -> &Arc<Pan123Client> {
        &self.client
    }

    /// 卷根 folder id（`pan123_root`；D3）。
    pub(crate) fn root_cid(&self) -> &str {
        &self.params.root
    }

    /// 路径解析缓存（upload.rs 的父目录解析与落库后就近失效）。
    pub(crate) fn paths_arc(&self) -> &Arc<pathcache::PathCache> {
        &self.paths
    }

    /// 上传会话表（upload.rs 的 resume 面）。
    pub(crate) fn sessions_arc(&self) -> &Arc<upload::SessionStore> {
        &self.sessions
    }

    /// 卷身份（upload.rs 的 Entry 产出面）。
    pub(crate) fn volume_ref(&self) -> &VolumeId {
        &self.volume
    }

    /// 分片 PUT 重试参数（§5.15——驱动构造时的注入源）。
    pub(crate) fn retry_config(&self) -> api::RetryConfig {
        self.params.retry.unwrap_or_default()
    }

    /// 路径 → 解析层（list/stat/mkdir/rename 共用）。
    async fn resolve(
        &self,
        path: &RelPath,
        want_dir: bool,
    ) -> Result<pathcache::Resolved, StorageError> {
        self.paths
            .resolve(&self.client, &self.params.root, path, want_dir)
            .await
    }

    /// 后端行 → [`Entry`]（list/stat 共用换算面）。
    ///
    /// - **目录 size 报 0**（123 的目录条目带累计聚合 Size——照 baidu
    ///   先例不透出为文件 size，任务 A）；
    /// - 不可寻址名（空名/含反斜杠/含 NUL）过滤 → None（「list 产出
    ///   即可寻址」纪律，K67.2 ck-local/ck-sftp/ck-pan115 同源硬化）；
    /// - 句柄 = 裸 file_id 字符串（任务 A 契约；delete 的父目录反查走
    ///   pathcache 反查索引——M-S1 的 123 形态解）。
    fn entry_from_row(&self, dir: &RelPath, row: &models::FileEntry) -> Option<Entry> {
        if !name_is_addressable(&row.file_name) {
            tracing::debug!(
                target: "ck_pan123::list",
                fid = row.file_id,
                "skipping unaddressable name (empty/backslash/NUL)"
            );
            return None;
        }
        let path = dir.join(&row.file_name).ok()?;
        let kind = if row.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let size = if row.is_dir() {
            0
        } else {
            row.size.max(0) as u64
        };
        Some(Entry {
            id: EntryId::new(
                self.volume.clone(),
                BackendHandle::new(row.file_id.to_string()),
            ),
            path,
            kind,
            size,
            mtime: row.update_at as f64,
        })
    }
}

/// 「list 产出即可寻址」：空名 / 含反斜杠 / 含 NUL 的名字不进 [`Entry`]
/// （跨平台卷的可寻址性纪律，K67.2 同源）。
fn name_is_addressable(name: &str) -> bool {
    !name.is_empty() && !name.contains('\\') && !name.contains('\u{0}')
}

/// cid（目录句柄字符串）→ i64（P3/K79 防御性加固）。
///
/// **可达性如实声明**：生产路径下不可达——本卷 cid 全部由
/// `params.root`（[`Pan123Params::from_pairs`] 的纯数字校验门）或
/// `file_id.to_string()`（i64）铸出，类型系统 + 配置门保证解析成功。
/// 但 [`Pan123Params`] 是 pub 结构体：绕过 `from_pairs` 直接构造（或
/// 未来新增装配面漏检）即得非数字 cid——旧形态 `unwrap_or(0)` 会把
/// 破损**静默归到网盘根（0）**：错向账号根做 mkdir/move/upload 是数据
/// 破坏面。显式报错（`Invalid`）+ error 通道保留原值（R2 双通道先例
/// ——`Invalid` 无载荷，诊断文案走日志）。
pub(crate) fn parse_cid(cid: &str, stage: &'static str) -> Result<i64, StorageError> {
    cid.parse::<i64>().map_err(|_| {
        tracing::error!(
            target: "ck_pan123::cid",
            stage,
            cid = %cid,
            "pan123 internal cid invariant broken: a directory id is not numeric — refusing \
             to fall back to the netdisk root (0)"
        );
        StorageError::Invalid
    })
}

/// 跨父 rename 改名腿失败后的错误文案面（M11）：`file/mod_pid` 已
/// 生效——文件已移至目标父目录但**保留旧名**（旧路径 resolve
/// NotFound、新名不可见的「消失」假象实为移位）。追加部分变更事实
/// 与可行动出路（在目标目录下用旧名重试 rename）。
///
/// 错误大类纪律：只动**载荷型**变体（`Io`/`Unavailable`——同形拼接、
/// 保留原始错误文本）；契约类变体（`Exists`/`NotFound`/`Invalid`/
/// `Unauthorized` 等）无载荷且语义不可动——原样透传，部分变更事实经
/// warn 通道补观（R2 双通道先例）。
fn partial_move_error(e: StorageError, old_name: &str, dst_parent: &RelPath) -> StorageError {
    let hint = format!(
        "partial move: file/mod_pid already moved the file to '{}' but the rename leg \
         failed; it keeps its old name '{}' there — retry rename from that location",
        dst_parent.as_str(),
        old_name
    );
    match e {
        StorageError::Io(msg) => StorageError::Io(format!("{msg}; {hint}")),
        StorageError::Unavailable(msg) => StorageError::Unavailable(format!("{msg}; {hint}")),
        other => {
            tracing::warn!(
                target: "ck_pan123::rename",
                %hint,
                error = %other,
                "cross-parent rename failed after mod_pid (contract-class error kept as-is)"
            );
            other
        }
    }
}

#[async_trait]
impl StorageDriver for Pan123Driver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    /// 能力位声明（计划 §4.1 的目标位）。
    ///
    /// R4 注记：这些位的**后端原语存在性**已由 123-0 真机验证（下载
    /// Range 206 逐字节 MATCH / 分片 presign PUT / mod_pid 服务端移动 /
    /// MD5 etag 秒传命中 / resume 会话保留差集补传哈希 MATCH）；驱动面
    /// 的 conformance 八断言于 123-4 全绿——**resume 位已点亮**（断言⑦
    /// 差集上界驱动层可观测验证过：tests/conformance.rs 的
    /// `conformance_suite_offline` + 分片级特化
    /// `pan123_partial_parts_resume_only_missing`——第一轮中断后只补
    /// 缺片，「到齐即传 + 本地五元组记录优先（123-5 真机修正）」双证）。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // download_info → CDN Range 206 实证（123-0 ④：跨窗口
            // 逐字节 MATCH；dlink 会话内可复用）。
            range_read: true,
            // 已点亮（123-4 断言⑦过）：本地五元组记录优先 +
            // list_parts 保留分片 + 差集补传哈希 MATCH（123-0 ⑤ 真机 +
            // conformance ⑦/特化用例离线双证；同参重发的会话复用只在
            // 零片会话上成立——123-5 真机三轮实验）。
            resume: true,
            // 分片 presigned PUT（repare 批量恒 multipart 会话）。
            multipart: true,
            // mod_pid = 服务端单侧移动。
            server_side_move: true,
            // MD5 etag 秒传（Reuse=true 瞬时命中实证；密文永不命中
            // 无害——etag 必填所以必须算真值）。
            rapid_upload: true,
            // 后端 list 即真相（D4；rebuild 可用）。
            authoritative_index: true,
            // 无变更推送通道。
            change_feed: false,
            // 非 bot 后端。
            inbound: false,
            chat: false,
            // trash `intoRecycle`（D2：进回收站语义）。
            remote_delete: true,
        }
    }

    /// 列目录（depth-1）：解析目录 file_id → 全页拉齐合并 → **驱动内
    /// 稳定排序**（服务端排序仅 file_id asc/desc——按名字典序，同名字
    /// 回退 file_id；conformance ③「分页稳定有序」靠这个）→ 内部
    /// offset 游标切 [`Page`]（pan115/baidu 同款形态——后端分页参数在
    /// 驱动内消化，游标是对外不透明令牌）。
    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let resolved = self.resolve(dir, true).await?;
        let mut rows = pathcache::list_all(&self.client, &resolved.cid).await?;
        // 解析层缓存顺带更新（list 产出即喂——同目录下一次零网络）。
        self.paths.put_dir(&resolved.cid, &rows).await;
        // 跨页合并后的稳定排序：名字典序，同名字回退 file_id（任务 A）。
        rows.sort_by(|a, b| {
            a.file_name
                .cmp(&b.file_name)
                .then_with(|| a.file_id.cmp(&b.file_id))
        });
        let entries: Vec<Entry> = rows
            .iter()
            .filter_map(|row| self.entry_from_row(dir, row))
            .collect();
        let total = entries.len();
        let offset = match page.cursor {
            PageCursor::Start => 0usize,
            PageCursor::Next(tok) => tok
                .strip_prefix("off:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        };
        let offset = offset.min(total); // 伪超大 offset 钳制
        let end = offset.saturating_add(page.limit).min(total);
        let next = (end < total).then(|| PageCursor::Next(format!("off:{end}")));
        Ok(Listing {
            entries: entries.into_iter().skip(offset).take(page.limit).collect(),
            next,
        })
    }

    /// 取条目元数据：list-walk 解析（**末级新鲜查询**——绕过缓存现列
    /// 父目录；后端错误必须能被 stat 观察到，M-S1）；根 → 卷根本身。
    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        if path.is_root() {
            return Ok(Entry {
                id: EntryId::new(
                    self.volume.clone(),
                    BackendHandle::new(self.params.root.clone()),
                ),
                path: RelPath::root(),
                kind: EntryKind::Dir,
                size: 0,
                mtime: 0.0,
            });
        }
        let resolved = self
            .paths
            .resolve_with(&self.client, &self.params.root, path, false, true)
            .await?;
        let row = resolved.row.ok_or(StorageError::NotFound)?;
        let parent = path.parent().unwrap_or_else(RelPath::root);
        self.entry_from_row(&parent, &row)
            .ok_or(StorageError::NotFound)
    }

    /// 建目录（逐级隐式建父；末段已存在 → `Exists`——百度真网教训
    /// 同源纪律：**先 list 预检再 create**，不产重名副本垃圾；123 的
    /// 目录 create 撞已存在形态不明确，预检是硬要求）。
    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        if path.is_root() {
            return Err(StorageError::Exists); // 卷根本就存在（ck-local/baidu 同款）
        }
        let comps: Vec<String> = path.components().map(str::to_string).collect();
        let last = comps.len() - 1;
        let mut cid = self.params.root.clone();
        for (i, comp) in comps.iter().enumerate() {
            let existing = match self.paths.get_child(&cid, comp).await {
                Some(row) => Some(row),
                None => {
                    let rows = pathcache::list_all(&self.client, &cid).await?;
                    self.paths.put_dir(&cid, &rows).await;
                    self.paths.get_child(&cid, comp).await
                }
            };
            if let Some(row) = existing {
                if i == last {
                    return Err(StorageError::Exists);
                }
                if !row.is_dir() {
                    return Err(StorageError::NotFound); // 中途遇文件
                }
                cid = row.file_id.to_string();
                continue;
            }
            // 不存在 → create（5060 同名竞争窗口 → Exists 由 errno 表归一）。
            // cid 解析：防御性显式报错（P3/K79——不变式见 parse_cid）。
            let parent = parse_cid(&cid, "mkdir")?;
            let fid = self.client.mkdir(parent, comp).await?;
            self.paths.invalidate(&cid).await; // 结构变更：父级缓存失效
            cid = fid.to_string();
        }
        Ok(())
    }

    /// 删除（`file/trash`，D2：进回收站语义）+ **回读校验**（§5.11 数据
    /// 完整性纪律：code=0 但父目录还在 → `Io` 不吞——trash 静默失败
    /// 陷阱的纵深防御）。
    ///
    /// 幂等声明：trash 成功且回读已不在（或 info 显示已入回收站/查无）
    /// → `Ok`；句柄查无实体的冷删除同样 `Ok`（幂等面）。
    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄（trait 契约）
        }
        // 非数字句柄：本卷实体域全是数字 file_id——**查无实体的极端
        // 形态**，归入幂等 Ok（conformance ④ 的声明形态必须恒定：
        // delete 不存在 → Ok；123-4 红灯修复——原 Invalid 破坏恒定）。
        let Ok(fid) = id.handle.as_str().parse::<i64>() else {
            return Ok(());
        };
        self.client.trash(fid).await?;
        // 回读校验：优先父目录 list（句柄由 list/stat 铸出时父几乎必在
        // 缓存——反查索引）；冷句柄走 info（Trashed 标志/查无）。
        match self.paths.invalidate_owner(fid).await {
            Some(parent_cid) => {
                let rows = pathcache::list_all(&self.client, &parent_cid).await?;
                if rows.iter().any(|r| r.file_id == fid) {
                    return Err(StorageError::Io(format!(
                        "file/trash acknowledged (code=0) but entry {fid} still listed: \
                         the payload shape was likely rejected silently"
                    )));
                }
                self.paths.put_dir(&parent_cid, &rows).await;
            }
            None => {
                if let Some(row) = self.client.file_info(fid).await? {
                    if !row.trashed {
                        return Err(StorageError::Io(format!(
                            "file/trash acknowledged (code=0) but entry {fid} is still live"
                        )));
                    }
                }
                // info 查无 → 已删（幂等面）。
            }
        }
        self.dlinks.invalidate(fid).await; // 直链缓存同失效
        Ok(())
    }

    /// 移动/重命名（`server_side_move`）。
    ///
    /// - 同父改名：`file/rename`（**文件与目录同端点**——任务 0 真机
    ///   实证目录可用：同 FileId、Type 保持、list 回读新名）；
    /// - 跨父移动：`file/mod_pid`（pan123-rs wire 形态）+ 按需改名；
    ///   改名腿失败 = 部分变更（文件已在目标父、保留旧名）——文案
    ///   明示 + 不回滚（[`partial_move_error`]，M11）；
    /// - 预检目标存在 → `Exists`（冷目录现列预检——M-S4 同源纪律）；
    ///   目标在源之下 → `Invalid`（契约）。
    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        if from.is_root() || to.is_root() {
            return Err(StorageError::Invalid);
        }
        // 后代检查（纯字符串面：`to` 以 `from/` 起头即源之后代）。
        if to.as_str().starts_with(&format!("{}/", from.as_str())) {
            return Err(StorageError::Invalid);
        }
        let src = self.resolve(from, false).await?;
        let src_row = src.row.clone().ok_or(StorageError::NotFound)?;
        let to_name = to
            .components()
            .last()
            .ok_or(StorageError::Invalid)?
            .to_string();
        let dst_parent = to.parent().unwrap_or_else(RelPath::root);
        let dst = self.resolve(&dst_parent, true).await?;
        // 目标已存在 → Exists（占位检查先于任何变更）。缓存未列过目标
        // 目录时**现列预检**（mkdir 同款纪律，M-S4）。
        let occupied = match self.paths.get_child(&dst.cid, &to_name).await {
            Some(_) => true,
            None => {
                let rows = pathcache::list_all(&self.client, &dst.cid).await?;
                self.paths.put_dir(&dst.cid, &rows).await;
                self.paths.get_child(&dst.cid, &to_name).await.is_some()
            }
        };
        if occupied {
            return Err(StorageError::Exists);
        }
        let same_parent = src.parent_cid == dst.cid;
        if !same_parent {
            // 跨父：mod_pid 到目标父，再按需改名（move 被拒 = 竞态占位）。
            // 目标父 cid 解析：防御性显式报错（P3/K79——绝不静默归 0
            // 把文件移进网盘根）。
            let target_parent = parse_cid(&dst.cid, "rename/mod_pid")?;
            self.client.mod_pid(src_row.file_id, target_parent).await?;
        }
        // 改名腿（同父改名或跨父后改名；名字已同则跳过）。
        if src_row.file_name != to_name {
            if let Err(e) = self.client.rename(src_row.file_id, &to_name).await {
                // 跨父时 mod_pid 已生效：源父缓存已无此行、目标父缓存
                // 少了旧名行——两父级与成功路径同一失效面，失败也不例外
                //（否则过期缓存把新位置藏掉，重试 rename/stat 全瞎）。
                self.paths.invalidate(&src.parent_cid).await;
                if !same_parent {
                    self.paths.invalidate(&dst.cid).await;
                    // M11：两步非原子的部分变更面——文件已移至目标父
                    // 目录但保留旧名（旧路径 NotFound、新名不可见的
                    // 「消失」假象）。**不回滚**（回滚自身也可能失败，
                    // 把状态搞得更糟）；文案明示实情 + 可行动出路。
                    return Err(partial_move_error(e, &src_row.file_name, &dst_parent));
                }
                return Err(e);
            }
        }
        // 结构变更：两个父级都失效（同父时同一个）。
        self.paths.invalidate(&src.parent_cid).await;
        if !same_parent {
            self.paths.invalidate(&dst.cid).await;
        }
        Ok(())
    }

    /// 打开读取流（三跳解析 + 有界窗口流，[`download`] 模块）。
    ///
    /// 句柄 = 裸 file_id——info 复核形态（目录 → `Invalid`）并取 size。
    /// range 钳制在这里（end 越界 → EOF；start≥size → 空流——baidu
    /// 同款 conformance ② 形态）。
    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄
        }
        let fid: i64 = id
            .handle
            .as_str()
            .parse()
            .map_err(|_| StorageError::Invalid)?;
        // 形态与元数据复核：info 端点（活——spike 实证）。
        let row = self
            .client
            .file_info(fid)
            .await?
            .ok_or(StorageError::NotFound)?;
        if row.is_dir() {
            return Err(StorageError::Invalid); // 目录不可读（trait 契约）
        }
        let size = row.size.max(0) as u64;
        let (start, end) = match range {
            None => (0u64, size),
            Some(r) => (r.start, r.end.unwrap_or(size).min(size)),
        };
        download::open_range(&self.client, &self.dlinks, &row, start, end).await
    }

    /// 打开写暂存器（commit-on-close；[`upload`] 模块——123-3 七步链：
    /// MD5 预计算 → upload_request（Reuse/5060→duplicate=2）→ list 对账
    /// → repare 预签名 → 逐分片裸 PUT → 确认 → complete → /v2 全量
    /// body + size 校验）。根路径 → `Invalid`；目标父目录缺失则逐级隐式
    /// 创建。
    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        upload::writer(self, path, hint).await
    }

    /// 配额：`user/info` 的 `SpacePermanent`（总）/`SpaceUsed`（已用）
    /// ——spike 实证空间字段真身在此端点（`report/info` 只有会员档位）。
    async fn quota(&self) -> Result<Quota, StorageError> {
        let info = self.client.user_info().await?;
        Ok(Quota {
            total: Some(info.space_permanent.max(0) as u64),
            used: info.space_used.max(0) as u64,
        })
    }
}

/// 装配工厂：构造驱动并**取真身份**（`user/info` 的 uid → VolumeId
/// `pan123:<uid>`；baidu/pan115 factory 同形）。
///
/// 与 [`Pan123Driver::new`] 的分工：`new` 是离线构造面（占位
/// VolumeId——纯本地测试与 conformance 桩复用）；本入口是生产装配面。
/// **占位身份绝不进生产**。
pub async fn factory(params: &Pan123Params) -> Result<Arc<Pan123Driver>, StorageError> {
    Ok(Arc::new(Pan123Driver::connect(params.clone()).await?))
}

/// 探连接的结构化结果（doctor 腿；baidu `BackendProbe` / pan115
/// `Pan115Probe` 同款形态）。
///
/// 探活 = `user/info`（token 活力 + uid + 空间 + vip）+ **尽力而为的
/// traffic/check**（诊断面：失败降级 `None`，不让余量查询失败误报
/// 连接不可达——D5 的余量透出面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pan123Probe {
    /// token 活力 + 空间/流量查询全过。
    Alive {
        /// 账号 uid（VolumeId 的 key）。
        uid: String,
        /// 可用空间（字节）。
        free: u64,
        /// 总空间（`None` = 未知）。
        total: Option<u64>,
        /// 每日下载流量余量（字节；`Some(0)` = 已超额；`None` = 查询
        /// 失败/未知——doctor 降级显示）。
        traffic_remain: Option<u64>,
        /// 会员标志（VIP 消解流量上限——doctor 的会员指引面）。
        vip: bool,
    },
    /// 需要重新授权（20101/401）：token 失效——web API 无 refresh
    /// （K76.4），重扫码/重登是唯一出路。
    NeedsReauth,
    /// 网络/协议层不可达（超时、DNS、TLS、未映射码等）。
    Unreachable {
        /// 不含凭据的诊断文案。
        detail: String,
    },
}

/// 探连接（doctor 腿的直连探针）：connect（user/info 取 uid）→ 再取
/// user_info 的空间/vip 面 + traffic/check 余量（尽力而为），把失败归一
/// 为 [`Pan123Probe`]。
pub async fn probe(params: &Pan123Params) -> Pan123Probe {
    let driver = match Pan123Driver::connect(params.clone()).await {
        Ok(driver) => driver,
        Err(error) => return classify_probe_error(error),
    };
    // 空间/vip 面（user_info 是这些字段的真源——quota 的地基）。
    let info = match driver.client().user_info().await {
        Ok(info) => info,
        Err(error) => return classify_probe_error(error),
    };
    // 流量余量：诊断面尽力而为（fid = 卷根 folder id——诊断无特定
    // 文件；失败/超额形态归一为 None/Some(0)）。
    let root_fid: i64 = params.root.parse().unwrap_or(0);
    let traffic_remain = match driver.client().traffic_check(&[root_fid]).await {
        Ok(status) if status.is_traffic_exceeded => Some(0),
        Ok(status) => Some(status.original_remain_traffic.max(0) as u64),
        Err(error) => {
            tracing::warn!(
                target: "ck_pan123::probe",
                %error,
                "traffic/check failed during the probe: the remain display degrades to unknown"
            );
            None
        }
    };
    let total = info.space_permanent.max(0) as u64;
    Pan123Probe::Alive {
        uid: driver.volume().key().to_string(),
        free: total.saturating_sub(info.space_used.max(0) as u64),
        total: (total > 0).then_some(total),
        traffic_remain,
        vip: info.vip,
    }
}

/// 探针错误分类（driven by StorageError 变体）。
fn classify_probe_error(error: StorageError) -> Pan123Probe {
    match error {
        StorageError::Unauthorized { .. } => Pan123Probe::NeedsReauth,
        other => Pan123Probe::Unreachable {
            detail: format!("{other}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudkit_storage::{RelPath, StorageDriver, WriteHint};

    /// 文档与实现一致性守卫（R4 的可读面）：驱动的对外说明不得残留
    /// 「未接线/占位」类完成态声明——123-5 起九方法全接线、装配面
    /// 经 [`Pan123Driver::connect`] 生产可达。
    ///
    /// 钉的是「审查者读到的注记与代码事实一致」（pan115 同款守卫，
    /// 该处曾因残留注记导致误判 R4）。
    #[test]
    fn docs_do_not_claim_placeholder_status() {
        const SOURCE: &str = include_str!("lib.rs");
        // 只扫 #[cfg(test)] 之前的部分：断言消息自身逐字含这些串。
        let body = SOURCE.split("#[cfg(test)]").next().unwrap_or_default();
        for stale in ["仍占位", "没有装配路径触达", "占位臂"] {
            assert!(
                !body.contains(stale),
                "crate 文档残留陈旧完成态声明 {stale:?}——代码已全接线，须改成既成事实"
            );
        }
    }

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_string(), value.to_string())
    }

    fn base_pairs() -> Vec<(String, String)> {
        vec![pair("pan123_token", "mock-token-0")]
    }

    fn skeleton_params() -> Pan123Params {
        Pan123Params::from_pairs(&base_pairs()).expect("params")
    }

    // ------------------------------------------------------ config 面 ---

    #[test]
    fn from_pairs_resolves_both_keys() {
        let pairs = vec![
            pair("pan123_token", "mock-token-0"),
            pair("pan123_root", "1234567890"),
        ];
        let params = Pan123Params::from_pairs(&pairs).expect("complete pairs parse");
        assert_eq!(params.token.as_deref(), Some("mock-token-0"));
        assert_eq!(params.root, "1234567890");
        assert_eq!(params.api_base, DEFAULT_API_BASE);
        assert_eq!(params.fallback_base, DEFAULT_FALLBACK_BASE);
        assert_eq!(params.login_base, DEFAULT_LOGIN_BASE);
    }

    #[test]
    fn from_pairs_injects_the_default_root() {
        // D3：root 缺省 "0"（网盘根）。
        let params = Pan123Params::from_pairs(&base_pairs()).expect("minimal pairs parse");
        assert_eq!(params.root, DEFAULT_ROOT);
    }

    #[test]
    fn from_pairs_rejects_non_numeric_root_and_unknown_keys() {
        let mut bad_root = base_pairs();
        bad_root.push(pair("pan123_root", "12x45"));
        assert!(
            matches!(
                Pan123Params::from_pairs(&bad_root),
                Err(StorageError::Invalid)
            ),
            "non-numeric root must be rejected"
        );
        let mut typo = base_pairs();
        typo.push(pair("pan123_tokne", "x"));
        assert!(
            matches!(Pan123Params::from_pairs(&typo), Err(StorageError::Invalid)),
            "a typoed key must be rejected, not silently dropped"
        );
        // 空串 token = 未设置（empty-means-unset）
        let mut empty = base_pairs();
        empty[0].1 = String::new();
        let params = Pan123Params::from_pairs(&empty).expect("empty clears, not errors");
        assert_eq!(params.token, None);
    }

    // ------------------------------------------------------ driver 面 ---

    #[test]
    fn driver_volume_is_the_pending_placeholder() {
        let driver = Pan123Driver::new(skeleton_params()).expect("constructs");
        let volume = VolumeId::new("pan123", "pending").expect("volume");
        assert_eq!(*driver.volume(), volume);
    }

    #[test]
    fn driver_construction_requires_the_token() {
        let mut params = skeleton_params();
        params.token = None;
        assert!(
            matches!(Pan123Driver::new(params), Err(StorageError::Invalid)),
            "a missing token must be rejected at construction"
        );
    }

    #[test]
    fn capabilities_declare_the_planned_bits() {
        // 计划 §4.1 逐位（resume 的「断言⑦后复核」注记见实现）。
        let driver = Pan123Driver::new(skeleton_params()).expect("driver");
        let caps = driver.capabilities();
        assert!(caps.range_read);
        assert!(caps.resume);
        assert!(caps.multipart);
        assert!(caps.server_side_move);
        assert!(caps.rapid_upload);
        assert!(caps.authoritative_index);
        assert!(caps.remote_delete);
        assert!(!caps.change_feed);
        assert!(!caps.inbound);
        assert!(!caps.chat);
    }

    #[tokio::test]
    async fn writer_refuses_the_root_path() {
        // 写路径已全接线（123-3——协议面行为由 write_path.rs 的桩矩阵
        // 覆盖）；本单测只钉契约面：根路径不可写入（离线断言，无网络）。
        let driver = Pan123Driver::new(skeleton_params()).expect("driver");
        let err = driver
            .writer(&RelPath::root(), &WriteHint::default())
            .await
            .err()
            .expect("root is not writable");
        assert!(matches!(err, StorageError::Invalid), "{err:?}");
    }

    // -------------------------------------------------------- api 面 ---

    #[test]
    fn login_uuid_is_the_md5_of_a_uuid_hex() {
        // pan123-rs 形态：32 位小写 hex（md5 输出）；两次生成不同
        // （uuid v4 随机源）。
        let a = api::new_login_uuid();
        let b = api::new_login_uuid();
        assert_eq!(a.len(), 32, "md5 hex digest");
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex: {a}"
        );
        assert_ne!(a, b, "fresh uuid per session");
    }
}
