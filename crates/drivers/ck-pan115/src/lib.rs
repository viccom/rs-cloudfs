//! # ck-pan115——115 网盘开放平台存储驱动（L1 驱动 crate，Phase 5 / 115-1）。
//!
//! 后端 = 一个 115 账号的可设置根（`pan115_root`，folder id，缺省
//! `"0"` = 网盘根，D3 拍板）。认证 = 官方开放平台 device-code PKCE
//! token 对（K65 路径丙 / K69.1：公共 client_id 自铸，运行期零
//! app_key；refresh 一次一换、驱动内自续并经 [`TokenStore`] 回写卷
//! 配置键）。协议实现自 115-0 spike（真机验证）port：envelope 双形态
//! 解析（`state:true` 布尔 / `state:1` 数字、HTTP 200 错误包）、PKCE
//! 三端点、文件面端点、OSS V1 自研签名层（K69.5——不引入 ali-oss-rs）。
//!
//! ## 批次边界（115-1…115-5 全批次完成，2026-09-16）
//!
//! - 认证/分类/限流面：TDD 钉死（`tests/oauth_state_machine.rs` /
//!   `tests/errno_mapping.rs` / `tests/limiter.rs`）；
//! - 读路径（115-2）：list/stat/mkdir/delete/rename/reader/quota 全接线；
//! - 写路径（115-3）：commit-on-close stager + 二次认证循环 + OSS
//!   分片 + resume 会话差集；
//! - conformance + 12 装配点（115-4）：离线八断言全绿，`cloudkit-cli`
//!   的 dispatch 臂经 [`Pan115Driver::connect`] 生产可达；
//! - 真机矩阵（115-5）：上传回读 / Range 窗口 / 秒传 / 多分片 /
//!   目录 rename / 进程重启后续传——全过（`tests/live_matrix.rs`）。
//!
//! 纯函数层（PKCE/签名输入构造/XML 工具/参数解析）单测在本文件。
//!
//! ## 限流（D4 落值，K69.3 实测）
//!
//! 全局令牌桶缺省 1 rps（burst 2）+ `770004` 账号级硬退避（300s
//! 起步指数 ×2、上限 1800s）——[`limiter`] 模块；api client 的
//! dispatch 状态机统一接线。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate
//! （driver-onboarding §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。

pub mod api;
pub mod download;
pub mod limiter;
pub mod oauth;
pub mod oss;
pub mod pathcache;
pub mod transport_face;
pub mod upload;

use std::sync::Arc;

use async_trait::async_trait;
use cloudkit_storage::{
    BackendHandle, ByteStream, Capabilities, Entry, EntryId, EntryKind, Listing, Page, PageCursor,
    Quota, Range, RelPath, StorageDriver, StorageError, UploadStager, VolumeId, WriteHint,
};

pub use api::{Pan115Client, UA};
pub use oauth::TokenStore;
pub use transport_face::Pan115Transport;

/// 生产 API base（proapi——文件/上传面；spike auth.rs 同值）。
pub const DEFAULT_API_BASE: &str = "https://proapi.115.com";
/// 生产认证 base（passportapi——authDeviceCode/deviceCodeToToken/
/// refreshToken；spike auth.rs 同值）。
pub const DEFAULT_PASSPORT_BASE: &str = "https://passportapi.115.com";
/// 生产扫码轮询 base（qrcodeapi——get.status；spike auth.rs 同值）。
pub const DEFAULT_QRCODE_BASE: &str = "https://qrcodeapi.115.com";
/// 扫码绑定的 app 身份缺省（K69.1：OpenList 官方托管的公共 client_id
/// `100197303`——**公开非机密值**，取自 api.oplist.org 登录端点的
/// authorize 跳转；K65 路径丙：PKCE 流全程无 secret）。
///
/// 注入位置 = 驱动参数构造处（[`Pan115Params`]），**非** config 解析处
/// ——baidu `DEFAULT_ROOT` 的先例形态：core 的 `pan115_client_id` 键保持
/// 缺省 `None`，覆盖它是「app 身份被连坐封禁后换身份重扫」的恢复路径。
pub const DEFAULT_CLIENT_ID: &str = "100197303";
/// 卷根缺省（D3：folder id `"0"` = 网盘根；可设置）。
pub const DEFAULT_ROOT: &str = "0";

/// 115 驱动参数（driver-onboarding §4 的「配置 map → 参数结构体」形态；
/// 115-4 装配批把 config 键 + env 解析为本结构体注入）。
///
/// 不实现 `Debug`：结构体携带凭据（token 对），派生展开有把凭据印进
/// 日志的风险（R3——BaiduParams/SftpParams 同款裁决）。
#[derive(Clone)]
pub struct Pan115Params {
    /// 扫码绑定的 app 身份（非机密；缺省 [`DEFAULT_CLIENT_ID`]）。
    pub client_id: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// 卷根（folder id；缺省 [`DEFAULT_ROOT`]）。
    pub root: String,
    /// proapi 面 base（默认生产常量；测试注入 mock）。
    pub api_base: String,
    /// passportapi 面 base（refresh 端点；测试注入 mock）。
    pub passport_base: String,
    /// 刷新产物持久化回调（K13 形态；`None` = 不持久化——正式装配
    /// 必须提供，ConfigTokenStore 桥接在 115-4）。
    pub token_store: Option<Arc<dyn TokenStore>>,
    /// 限流参数（`None` = K69.3 生产缺省；测试注入毫秒级窗口）。
    pub limiter: Option<limiter::LimiterConfig>,
    /// 会话/spool 目录（`None` = 系统临时目录；生产装配给卷家目录——
    /// K21 锚形态，baidu `sessions_dir` 同义）。
    pub sessions_dir: Option<std::path::PathBuf>,
}

impl Default for Pan115Params {
    fn default() -> Self {
        Pan115Params {
            client_id: DEFAULT_CLIENT_ID.to_string(),
            access_token: None,
            refresh_token: None,
            root: DEFAULT_ROOT.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            passport_base: DEFAULT_PASSPORT_BASE.to_string(),
            token_store: None,
            limiter: None,
            sessions_dir: None,
        }
    }
}

impl Pan115Params {
    /// 从展平的配置键值对解析（纯函数：无 IO、无网络；SftpParams 同款
    /// 第二道门——core `KNOWN_TOML_KEYS` 挡未知键之后，这里再挡拼写
    /// 错误的 pan115 键与非法值）。
    ///
    /// - 键集 = 四个 `pan115_*` 键；未知键 → `Invalid`；
    /// - `pan115_root` 须为纯数字 folder id（与 core `validate()` 同规则）；
    /// - `pan115_client_id` 缺省注入 [`DEFAULT_CLIENT_ID`]（非机密，
    ///   baidu `DEFAULT_ROOT` 先例的注入位置）；
    /// - token 键空串视为未设置（empty-means-unset，baidu 键同语义）。
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Self, StorageError> {
        let mut client_id: Option<String> = None;
        let mut access_token: Option<String> = None;
        let mut refresh_token: Option<String> = None;
        let mut root: Option<String> = None;
        for (key, value) in pairs {
            let non_empty = || Some(value.clone()).filter(|v| !v.is_empty());
            match key.as_str() {
                "pan115_client_id" => client_id = non_empty(),
                "pan115_access_token" => access_token = non_empty(),
                "pan115_refresh_token" => refresh_token = non_empty(),
                "pan115_root" => root = non_empty(),
                other => {
                    tracing::warn!(
                        target: "ck_pan115::config",
                        "unknown pan115 key {other:?}: the accepted keys are pan115_client_id, \
                         pan115_access_token, pan115_refresh_token, pan115_root"
                    );
                    return Err(StorageError::Invalid);
                }
            }
        }
        let root = root.unwrap_or_else(|| DEFAULT_ROOT.to_string());
        if root.is_empty() || !root.bytes().all(|b| b.is_ascii_digit()) {
            tracing::warn!(
                target: "ck_pan115::config",
                "pan115_root must be a numeric 115 folder id (\"0\" is the netdisk root), \
                 got {root:?}"
            );
            return Err(StorageError::Invalid);
        }
        Ok(Pan115Params {
            client_id: client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string()),
            access_token,
            refresh_token,
            root,
            ..Pan115Params::default()
        })
    }
}

/// 115 存储驱动（115-5 起九方法全接线）。
///
/// - 语义：读路径 list/stat/mkdir/delete/rename/downurl 流读、写路径
///   init/get_token/OSS 分片/complete/秒传——全量实现（115-2/115-3）；
/// - 错误：后端错误经 [`api`] 分类表映射为 [`StorageError`]（K69.7）；
/// - 并发：全方法可并发调用（`&self`）；token 刷新单飞（client）；
/// - 生命周期：HTTP client 无连接态；限流器（D4）与 token 状态共享
///   于 [`Pan115Client`]。
pub struct Pan115Driver {
    volume: VolumeId,
    /// 共享 HTTP 面（CDN 流任务经 Arc 持有——ByteStream 生命周期独立于
    /// `&self`，baidu 同款）。
    client: Arc<Pan115Client>,
    params: Pan115Params,
    /// 路径 → folder id 解析缓存（115-2；list/stat 顺带喂）。
    paths: pathcache::PathCache,
    /// pick_code → CDN 直链 TTL 缓存（115-2；与流任务经 Arc 共享）。
    dlinks: Arc<download::DlinkCache>,
    /// 上传会话表（115-3；resume 差集资产，与 stager 经 Arc 共享）。
    sessions: Arc<upload::SessionStore>,
}

impl Pan115Driver {
    /// 同步构造（不连网——首请求惰性建立；sftp D3 同款骨架形态）。
    ///
    /// token 对必须齐备（初始对来自 setup 扫码或手填配置；缺失 →
    /// `Invalid`——core `validate()` 的第一道门之外，这里是驱动侧的
    /// 第二道）。
    ///
    /// **VolumeId 离线形态**：`pan115:pending`——真值 `pan115:<uid>`
    /// （uid 取自 `user/info`）由 [`Pan115Driver::connect`] 装配面确定
    /// （baidu 在 factory 的 connect 阶段取 uid 先例；sftp 在 new 时以
    /// 已知 host:port 合成）。生产装配恒走 `connect`，占位身份只在离线
    /// 测试与 conformance 桩中复用。
    pub fn new(params: Pan115Params) -> Result<Self, StorageError> {
        let (Some(access), Some(refresh)) = (&params.access_token, &params.refresh_token) else {
            return Err(StorageError::Invalid);
        };
        let limiter_cfg = params.limiter.unwrap_or_default();
        let client = Arc::new(Pan115Client::new(
            access.clone(),
            refresh.clone(),
            params.passport_base.clone(),
            params.api_base.clone(),
            params.token_store.clone(),
            Arc::new(limiter::RateLimiter::new(limiter_cfg)),
        )?);
        let sessions = Arc::new(upload::SessionStore::new(params.sessions_dir.clone()));
        Ok(Pan115Driver {
            volume: VolumeId::new("pan115", "pending")?,
            client,
            params,
            paths: pathcache::PathCache::new(),
            dlinks: Arc::new(download::DlinkCache::new()),
            sessions,
        })
    }

    /// 连接构造（115-2）：`user/info` 取 uid → VolumeId `pan115:<uid>`
    /// （K5；baidu `connect` 同形——uid 是账号级身份，ID 从第一天带卷）。
    ///
    /// 115-4 装配批的工厂走本入口；[`Pan115Driver::new`] 保持离线形态
    /// （占位 VolumeId `pan115:pending`）供纯本地测试与 conformance 桩
    /// 复用。
    pub async fn connect(params: Pan115Params) -> Result<Self, StorageError> {
        let mut driver = Pan115Driver::new(params)?;
        let info = driver.client.user_info().await?;
        let uid = info
            .get("user_id")
            .map(|v| match v {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty() && s != "null")
            .ok_or_else(|| StorageError::Unavailable("user/info: user_id missing".to_string()))?;
        driver.volume = VolumeId::new("pan115", &uid)?;
        // 复审推广（2026-09-25，负责人裁定「带病挂载」清剿）：装配根
        // 探测——root 是账号内分类 cid，配错即「死卷」（路径↔cid 永远
        // 解析失败，且 id 非路径形态无法自愈建出）；装配期一次
        // get_info 以可行动错误拒绝之（sftp/webdav connect 门同型）。
        // "0" = 网盘根（虚拟容器，恒在）不探。
        let root_cid = driver.root_cid().to_string();
        if root_cid != "0" {
            if let Err(error) = driver.client.get_info(&root_cid).await {
                return Err(StorageError::Unavailable(format!(
                    "the pan115 root category {root_cid} is not accessible: {error} — set \
                     pan115_root to an existing category id (0 = the drive root)"
                )));
            }
        }
        Ok(driver)
    }

    /// 卷根的 folder id（`pan115_root`；D3）。
    fn root_cid(&self) -> &str {
        &self.params.root
    }

    /// 共享 HTTP 面（upload.rs 的 stager 构造面）。
    pub(crate) fn client_arc(&self) -> &Arc<Pan115Client> {
        &self.client
    }

    /// 路径解析缓存（upload.rs 的父目录解析面）。
    pub(crate) fn paths(&self) -> &pathcache::PathCache {
        &self.paths
    }

    /// 上传会话表（upload.rs 的面）。
    pub(crate) fn sessions_arc(&self) -> &Arc<upload::SessionStore> {
        &self.sessions
    }

    /// 驱动参数只读面（upload.rs 的 spool 目录解析）。
    pub(crate) fn params_ref(&self) -> &Pan115Params {
        &self.params
    }

    /// 卷身份（upload.rs 的 Entry 产出面；`StorageDriver::volume` 需要
    /// trait 导入，内部面直接给出）。
    pub(crate) fn volume_ref(&self) -> &VolumeId {
        &self.volume
    }

    /// 路径 → 解析层（list/stat/mkdir/rename 共用）。
    async fn resolve(
        &self,
        path: &RelPath,
        want_dir: bool,
    ) -> Result<pathcache::Resolved, StorageError> {
        self.paths
            .resolve(&self.client, self.root_cid(), path, want_dir)
            .await
    }

    /// api client 的公开访问面（115-2 起驱动方法经它调用端点；115-4
    /// 的 transport/doctor 面共用同一限流与 token 世界）。
    pub fn client(&self) -> &Pan115Client {
        &self.client
    }

    /// 驱动参数（115-2 的路径面消费 root；只读）。
    pub fn params(&self) -> &Pan115Params {
        &self.params
    }
}

#[async_trait]
impl StorageDriver for Pan115Driver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    /// 能力位声明（计划 §4.1）。
    ///
    /// R4 依据：每一「真」位都有已验证的落地证据——
    /// - 后端原语存在性：115-0 真机（K69.4 Range 206 + etag /
    ///   K69.8 OSS 分片与 resume 差集 / move·delete·秒传命中实证）；
    /// - `resume` 位：conformance 断言⑦（`tests/conformance.rs` 的
    ///   `conformance_suite_offline`，能力位门控下实跑）+ 分片级差集
    ///   强证明（`pan115_part_level_diff_resume`）+ 真机进程重启续传
    ///   （`tests/live_matrix.rs::resume_reuses_the_session_after_a_process_death`）。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // downurl + CDN Range 206 实证（K69.4：HEAD 探测可用，
            // Content-Range 起始吻合校验；CDN etag = MD5）。
            range_read: true,
            // /open/upload/resume + OSS ListParts 差集补片：断言⑦ +
            // 分片级差集 + 真机重启续传三重证据（见本函数文档）。
            resume: true,
            // OSS 分片上传（min 5MiB、10000 片上限）。
            multipart: true,
            // /open/ufile/move = 服务端单侧移动。
            server_side_move: true,
            // SHA1+pre_sha1+preid 秒传（K69.6 命中实证；密文永不命中
            // 无害）。
            rapid_upload: true,
            // 后端 list 即真相（D4；rebuild 可用）。
            authoritative_index: true,
            // 无变更推送通道。
            change_feed: false,
            // 非 bot 后端。
            inbound: false,
            chat: false,
            // /open/ufile/delete（D2：进回收站语义）。
            remote_delete: true,
        }
    }

    /// 列目录（depth-1）：解析目录 cid → 全量翻页 → 字典序稳定排序 →
    /// 内部 offset 游标切 [`Page`]（baidu 同款形态——后端分页参数在
    /// 驱动内消化，游标是对外不透明令牌）。
    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let resolved = self.resolve(dir, true).await?;
        let rows = pathcache::list_all(&self.client, &resolved.cid).await?;
        // 解析层缓存顺带更新（list 产出即喂——同目录下一次零网络）。
        self.paths.put_dir(&resolved.cid, &rows).await;
        let mut entries: Vec<Entry> = rows
            .iter()
            .filter_map(|row| self.entry_from_row(&resolved.cid, dir, row))
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
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
            entries: entries.drain(offset..end).collect(),
            next,
        })
    }

    /// 取条目元数据：解析层精确匹配（未命中走 list 下行）；根 → 卷根本身。
    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        if path.is_root() {
            return Ok(Entry {
                id: EntryId::new(self.volume.clone(), BackendHandle::new(self.root_cid())),
                path: RelPath::root(),
                kind: EntryKind::Dir,
                size: 0,
                mtime: 0.0,
            });
        }
        // stat = 新鲜度查询：末级现列（绕过缓存——后端错误必须能被
        // stat 观察到，conformance ⑤ 的硬要求）。
        let resolved = self
            .paths
            .resolve_with(&self.client, self.root_cid(), path, false, true)
            .await?;
        let row = resolved.row.as_ref().ok_or(StorageError::NotFound)?;
        let parent = path.parent().unwrap_or_else(RelPath::root);
        self.entry_from_row(&resolved.parent_cid, &parent, row)
            .ok_or(StorageError::NotFound)
    }

    /// 建目录（逐级隐式建父；末段已存在 → `Exists`——baidu 真网教训
    /// 同源纪律：**先 list 预检再 create**，不产重名副本垃圾）。
    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        if path.is_root() {
            return Err(StorageError::Exists); // 卷根本就存在（ck-local/baidu 同款）
        }
        let comps: Vec<String> = path.components().map(str::to_string).collect();
        let last = comps.len() - 1;
        let mut cid = self.root_cid().to_string();
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
                if row.fc != "0" {
                    return Err(StorageError::NotFound); // 中途遇文件
                }
                cid = row.fid;
                continue;
            }
            // 不存在 → create；I/O 类失败走一次复查（mkdir 竞态臂归一，
            // K67.3 ck-sftp 同源纪律）。
            let fid = match self.client.mkdir(&cid, comp).await {
                Ok(fid) => fid,
                Err(StorageError::Io(msg)) => {
                    let rows = pathcache::list_all(&self.client, &cid).await?;
                    self.paths.put_dir(&cid, &rows).await;
                    match self.paths.get_child(&cid, comp).await {
                        Some(_) if i == last => return Err(StorageError::Exists),
                        Some(row) => row.fid,
                        None => return Err(StorageError::Io(msg)),
                    }
                }
                Err(e) => return Err(e),
            };
            self.paths.invalidate(&cid).await; // 结构变更：父级缓存失效
            cid = fid;
        }
        Ok(())
    }

    /// 删除（`ufile/delete`，D2：进回收站语义）。幂等声明 = 删除不
    /// 存在句柄 → `NotFound`（trait 二选一；本驱动选查询式，baidu 同款）。
    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄（trait 契约）
        }
        let (fid, _pc, parent) = parse_handle(id.handle.as_str())?;
        self.client.delete(&fid, &parent).await?;
        self.paths.invalidate(&parent).await; // 结构变更：父级缓存失效
        Ok(())
    }

    /// 移动/重命名（`server_side_move`）。
    ///
    /// - 同父改名：文件走 `ufile/update`、目录走 `move` + `update`
    ///   两步（`update` 端点对目录的行为 115-0 spike 未覆盖，115-5 真机
    ///   复核；远端拒绝原样上抛不编造）；
    /// - 跨父移动：`ufile/move`（源/目标 cid 都解析）；
    /// - 预检目标存在 → `Exists`；目标在源之下 → `Invalid`（契约）。
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
        // 目录时**现列预检**（mkdir 同款纪律，M-S4——cache-only 会让冷
        // 目录的跨父 rename 穿透到 move/update，契约的 Exists 丢失）。
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
            // 跨父：move 到目标父，再按需改名（move 被拒 = 竞态占位）。
            self.client.move_entries(&src_row.fid, &dst.cid).await?;
        }
        // 改名腿（同父改名或跨父后改名；名字已同则跳过）。
        if src_row.fname != to_name {
            self.client.update(&src_row.fid, &to_name).await?;
        }
        // 结构变更：两个父级都失效（同父时同一个）。
        self.paths.invalidate(&src.parent_cid).await;
        if !same_parent {
            self.paths.invalidate(&dst.cid).await;
        }
        Ok(())
    }

    /// 打开读取流。句柄编码 `fid:pick_code:parent_cid`（[`parse_handle`]）。
    /// range 钳制在这里（end 越界 → EOF；start≥size → 空流——baidu
    /// 同款 conformance ② 形态）。
    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        if id.volume != self.volume {
            return Err(StorageError::NotFound); // 他卷句柄
        }
        let (fid, pc, _parent) = parse_handle(id.handle.as_str())?;
        // 形态判定：句柄不含 kind——get_info 复核（目录 → Invalid）。
        let info = self.client.get_info(&fid).await?;
        if info.file_category != "1" {
            return Err(StorageError::Invalid); // 目录不可读（trait 契约）
        }
        let size = info.size_byte.max(0) as u64;
        let pick_code = if !pc.is_empty() {
            pc
        } else if !info.pick_code.is_empty() {
            info.pick_code
        } else {
            return Err(StorageError::Unavailable(
                "reader: pick_code unavailable for this entry".to_string(),
            ));
        };
        let (start, end) = match range {
            None => (0u64, size),
            Some(r) => (r.start, r.end.unwrap_or(size).min(size)),
        };
        download::open_range(&self.client, &self.dlinks, &pick_code, size, start, end).await
    }

    /// 打开写暂存器（commit-on-close；115-3 全链见 [`upload`] 模块）。
    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        upload::writer(self, path, hint).await
    }

    /// 配额：`user/info` 的 `rt_space_info`（K69 采样：`all_total.size`
    /// 总空间 / `all_use.size` 已用；`size` 数值/字符串两形态容错）。
    async fn quota(&self) -> Result<Quota, StorageError> {
        let info = self.client.user_info().await?;
        let space = info.get("rt_space_info");
        let num = |v: Option<&serde_json::Value>| -> Option<u64> {
            v.and_then(|v| match v {
                serde_json::Value::Number(n) => n.as_u64(),
                serde_json::Value::String(s) => s.parse().ok(),
                _ => None,
            })
        };
        let total = num(space
            .and_then(|s| s.get("all_total"))
            .and_then(|t| t.get("size")));
        let used = num(space
            .and_then(|s| s.get("all_use"))
            .and_then(|t| t.get("size")))
        .unwrap_or(0);
        Ok(Quota { total, used })
    }
}

impl Pan115Driver {
    /// 后端行 → [`Entry`]（list/stat 共用换算面）。不可寻址名（空名 /
    /// 含反斜杠 / 含 NUL）过滤 → None（「list 产出即可寻址」纪律，
    /// K67.2 ck-local/ck-sftp 同源硬化）。`parent_cid` 编进句柄第三段
    /// ——delete 的缓存失效与 API `parent_id` 都消费它（M-S1：恒空段
    /// 曾使 invalidate("") 成为 no-op，删除后 ghost 行驻留缓存）。
    fn entry_from_row(&self, parent_cid: &str, dir: &RelPath, row: &api::ListRow) -> Option<Entry> {
        if !name_is_addressable(&row.fname) {
            tracing::debug!(
                target: "ck_pan115::list",
                fid = %row.fid,
                "skipping unaddressable name (empty/backslash/NUL)"
            );
            return None;
        }
        let path = dir.join(&row.fname).ok()?;
        let kind = if row.fc == "0" {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        Some(Entry {
            id: EntryId::new(
                self.volume.clone(),
                BackendHandle::new(encode_handle(&row.fid, &row.pc, parent_cid)),
            ),
            path,
            kind,
            size: row.fs.max(0) as u64,
            mtime: row.upt as f64,
        })
    }
}

/// 句柄编码 `fid:pc:parent`（三段；pc/parent 允许空——空段保留位置）。
fn encode_handle(fid: &str, pc: &str, parent: &str) -> String {
    format!("{fid}:{pc}:{parent}")
}

/// 句柄解码（[`encode_handle`] 的逆；fid 非空校验）。
fn parse_handle(handle: &str) -> Result<(String, String, String), StorageError> {
    let mut parts = handle.splitn(3, ':');
    let fid = parts.next().unwrap_or_default();
    let pc = parts.next().unwrap_or_default();
    let parent = parts.next().unwrap_or_default();
    if fid.is_empty() {
        return Err(StorageError::Invalid);
    }
    Ok((fid.to_string(), pc.to_string(), parent.to_string()))
}

/// 「list 产出即可寻址」：空名 / 含反斜杠 / 含 NUL 的名字不进 [`Entry`]
/// （跨平台卷的可寻址性纪律，K67.2 同源）。
fn name_is_addressable(name: &str) -> bool {
    !name.is_empty() && !name.contains('\\') && !name.contains('\u{0}')
}

/// 装配工厂：构造驱动并**取真身份**（`user/info` 的 uid → VolumeId
/// `pan115:<uid>`）。
///
/// 与 [`Pan115Driver::new`] 的分工：`new` 是离线构造面（占位 VolumeId
/// `pan115:pending`——纯本地测试与 conformance 桩复用）；本入口是生产
/// 装配面（baidu `factory` → `connect` 同形）。**占位身份绝不进生产**
/// ——它会同时污染 VolumeId/web_volume/sync_namespace_key 三面。
pub async fn factory(params: &Pan115Params) -> Result<Arc<Pan115Driver>, StorageError> {
    Ok(Arc::new(Pan115Driver::connect(params.clone()).await?))
}

/// 探连接的结构化结果（doctor 腿；baidu `BackendProbe` / sftp
/// `SftpProbe` 同款形态）。
///
/// 探活 = `user/info`（token 活力）+ `quota`（顺带取证空间数字）；失败
/// 按驱动既有映射归一为下列变体（R3：载荷不含凭据值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pan115Probe {
    /// token 活力 + 空间查询全过。
    Alive {
        /// 账号 uid（VolumeId 的 key）。
        uid: String,
        /// 可用空间（字节）。
        free: u64,
        /// 总空间（`None` = 未知）。
        total: Option<u64>,
    },
    /// 需要重新授权（401* 刷新失败族）：token 对失效——重扫或手填。
    NeedsReauth,
    /// 账号级访问上限（770004）：驱动已进入硬退避窗。
    RateLimited {
        /// 剩余封锁时长。
        window: std::time::Duration,
    },
    /// 网络/协议层不可达（超时、DNS、TLS 等）。
    Unreachable {
        /// 不含凭据的诊断文案。
        detail: String,
    },
}

/// 探连接（doctor 腿的直连探针）：构造驱动 + `user/info` + `quota`，
/// 把失败归一为 [`Pan115Probe`]。
pub async fn probe(params: &Pan115Params) -> Pan115Probe {
    let driver = match Pan115Driver::connect(params.clone()).await {
        Ok(driver) => driver,
        Err(error) => return classify_probe_error(error),
    };
    match driver.quota().await {
        Ok(quota) => {
            let uid = driver.volume().key().to_string();
            Pan115Probe::Alive {
                uid,
                free: quota
                    .total
                    .map(|t| t.saturating_sub(quota.used))
                    .unwrap_or(0),
                total: quota.total,
            }
        }
        Err(error) => classify_probe_error(error),
    }
}

/// 探针错误分类（driven by StorageError 变体）。
fn classify_probe_error(error: StorageError) -> Pan115Probe {
    match error {
        StorageError::Unauthorized { .. } => Pan115Probe::NeedsReauth,
        StorageError::RateLimited { retry_after } => Pan115Probe::RateLimited {
            window: retry_after.unwrap_or_default(),
        },
        other => Pan115Probe::Unreachable {
            detail: format!("{other}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudkit_storage::{RelPath, StorageDriver, WriteHint};

    /// 文档与实现一致性守卫（R4 的可读面）：驱动的对外说明不得残留
    /// 「未接线/占位/未跑」类完成态声明——115-5 起九方法全接线、装配
    /// 面可达、conformance 八断言（含⑦）在离线套件中实跑。
    ///
    /// 钉的是「审查者读到的注记与代码事实一致」：一轮审查曾因残留的
    /// 「115-4 未跑，标注为计划位」注记误判 resume 位不诚实，而该位
    /// 实际有断言⑦ + 分片级差集 + 真机重启续传三重证据。
    #[test]
    fn docs_do_not_claim_placeholder_status() {
        const SOURCE: &str = include_str!("lib.rs");
        // 只扫 #[cfg(test)] 之前的部分：断言消息自身逐字含这些串。
        let body = SOURCE.split("#[cfg(test)]").next().unwrap_or_default();
        for stale in ["九方法占位", "占位臂", "115-4 未跑", "计划位"] {
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
        vec![
            pair("pan115_access_token", "mock-access-0"),
            pair("pan115_refresh_token", "mock-refresh-0"),
        ]
    }

    fn skeleton_params() -> Pan115Params {
        Pan115Params::from_pairs(&base_pairs()).expect("params")
    }

    // ------------------------------------------------------ config 面 ---

    #[test]
    fn from_pairs_resolves_all_four_keys() {
        let pairs = vec![
            pair("pan115_client_id", "100197303"),
            pair("pan115_access_token", "mock-access-0"),
            pair("pan115_refresh_token", "mock-refresh-0"),
            pair("pan115_root", "1234567890"),
        ];
        let params = Pan115Params::from_pairs(&pairs).expect("complete pairs parse");
        assert_eq!(params.client_id, "100197303");
        assert_eq!(params.access_token.as_deref(), Some("mock-access-0"));
        assert_eq!(params.refresh_token.as_deref(), Some("mock-refresh-0"));
        assert_eq!(params.root, "1234567890");
        assert_eq!(params.api_base, DEFAULT_API_BASE);
        assert_eq!(params.passport_base, DEFAULT_PASSPORT_BASE);
    }

    #[test]
    fn from_pairs_injects_the_default_client_id_and_root() {
        // K69.1：client_id 缺省 = 公共身份 100197303（构造处注入而非
        // config 解析处——覆盖键是换身份恢复路径）；D3：root 缺省 "0"。
        let params = Pan115Params::from_pairs(&base_pairs()).expect("minimal pairs parse");
        assert_eq!(params.client_id, DEFAULT_CLIENT_ID);
        assert_eq!(params.root, DEFAULT_ROOT);
    }

    #[test]
    fn from_pairs_rejects_non_numeric_root_and_unknown_keys() {
        let mut bad_root = base_pairs();
        bad_root.push(pair("pan115_root", "12x45"));
        assert!(
            matches!(
                Pan115Params::from_pairs(&bad_root),
                Err(StorageError::Invalid)
            ),
            "non-numeric root must be rejected"
        );
        let mut typo = base_pairs();
        typo.push(pair("pan115_acces_token", "x"));
        assert!(
            matches!(Pan115Params::from_pairs(&typo), Err(StorageError::Invalid)),
            "a typoed key must be rejected, not silently dropped"
        );
        // 空串 token = 未设置（empty-means-unset）
        let mut empty = base_pairs();
        empty[0].1 = String::new();
        let params = Pan115Params::from_pairs(&empty).expect("empty clears, not errors");
        assert_eq!(params.access_token, None);
    }

    // ------------------------------------------------------ driver 面 ---

    #[test]
    fn driver_volume_is_the_pending_placeholder() {
        let driver = Pan115Driver::new(skeleton_params()).expect("constructs");
        let volume = VolumeId::new("pan115", "pending").expect("volume");
        assert_eq!(*driver.volume(), volume);
    }

    #[test]
    fn driver_construction_requires_the_token_pair() {
        let mut params = skeleton_params();
        params.refresh_token = None;
        assert!(
            matches!(Pan115Driver::new(params), Err(StorageError::Invalid)),
            "a missing token pair must be rejected at construction"
        );
    }

    #[test]
    fn capabilities_declare_the_planned_bits() {
        // 计划 §4.1 逐位（每位证据见 capabilities 的方法文档）。
        let driver = Pan115Driver::new(skeleton_params()).expect("driver");
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
        // 115-3 后九方法全数接线（协议面行为由 read_path.rs /
        // upload_path.rs 的桩回放矩阵覆盖）；本单测只钉驱动的纯输入
        // 守卫：卷根不可作为写入目标（trait 语义——根是目录）。
        let driver = Pan115Driver::new(skeleton_params()).expect("driver");
        let err = driver
            .writer(&RelPath::root(), &WriteHint::default())
            .await
            .err()
            .expect("root is not writable");
        assert!(matches!(err, StorageError::Invalid), "got {err:?}");
    }

    // ------------------------------------------------------ oauth 面 ---

    #[test]
    fn pkce_shapes_follow_the_validated_form() {
        let verifier = oauth::gen_code_verifier();
        assert_eq!(verifier.len(), 64, "spec range 43..=128, spike took 64");
        assert!(
            verifier
                .bytes()
                .all(|b| oauth::VERIFIER_CHARSET.contains(&b)),
            "charset = RFC 7636 unreserved + marks"
        );
        // challenge = STANDARD base64（带填充）的 SHA-256 ——32 字节摘要
        // 编码为 44 字符、恰一个 '='；确定性（无盐：同 verifier 同
        // challenge）
        let challenge = oauth::pkce_challenge("fixed-verifier-for-the-vector");
        assert_eq!(challenge.len(), 44);
        assert!(challenge.ends_with('='), "STANDARD padding, not urlsafe");
        assert_eq!(
            challenge,
            oauth::pkce_challenge("fixed-verifier-for-the-vector")
        );
    }

    // -------------------------------------------------------- oss 面 ---

    #[test]
    fn pct_encode_matches_go_query_escape() {
        assert_eq!(crate::oss::tests::pct("a b"), "a%20b", "space is %20 not +");
        assert_eq!(crate::oss::tests::pct("a/b"), "a%2Fb");
        assert_eq!(
            crate::oss::tests::pct("A-z_9.~"),
            "A-z_9.~",
            "unreserved pass through"
        );
        assert_eq!(
            crate::oss::tests::pct("中文"),
            "%E4%B8%AD%E6%96%87",
            "UTF-8 percent triples"
        );
    }

    #[test]
    fn xml_helpers_extract_known_shapes() {
        let body = "<InitiateMultipartUploadResult>\
                    <Bucket>b</Bucket><UploadId>id-1</UploadId></InitiateMultipartUploadResult>";
        assert_eq!(crate::oss::xml_tag(body, "UploadId"), Some("id-1"));
        assert_eq!(crate::oss::xml_tag(body, "Missing"), None);
        let parts = "<ListPartsResult><Part><PartNumber>1</PartNumber></Part>\
                     <Part><PartNumber>2</PartNumber></Part></ListPartsResult>";
        let blocks = crate::oss::tests::blocks(parts, "Part");
        assert_eq!(blocks.len(), 2);
        assert_eq!(crate::oss::xml_tag(blocks[0], "PartNumber"), Some("1"));
    }

    #[test]
    fn string_to_sign_pins_the_two_canonicalization_traps() {
        // 陷阱一：canonical headers 小写升序 + 每行尾 \n（最后一个也带）
        // 陷阱二：resource 原样拼接（RAW 值——URL 侧才 encode）
        let sts = crate::oss::tests::sts(
            "PUT",
            "application/octet-stream",
            "Thu, 01 Jan 2026 00:00:00 GMT",
            vec![
                ("x-oss-security-token".to_string(), "STS.TOK".to_string()),
                ("x-oss-callback".to_string(), "Y2I=".to_string()),
            ],
            "/bucket/object?partNumber=1&uploadId=uid",
        );
        assert_eq!(
            sts,
            "PUT\n\napplication/octet-stream\nThu, 01 Jan 2026 00:00:00 GMT\n\
             x-oss-callback:Y2I=\n\
             x-oss-security-token:STS.TOK\n\
             /bucket/object?partNumber=1&uploadId=uid",
            "sorted headers, trailing newline on the last one, raw resource"
        );
    }

    // -------------------------------------------------------- api 面 ---

    #[test]
    fn mask_keeps_head_and_tail_only() {
        assert_eq!(api::mask("abcdefghijkl"), "abcdef...ijkl");
        assert_eq!(api::mask("short"), "***");
    }
}
