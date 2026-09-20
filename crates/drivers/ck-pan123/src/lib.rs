//! # ck-pan123——123 云盘 web API 存储驱动（L1 驱动 crate，Phase 6 /
//! 123-1）。
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
//! ## 批次边界（123-1 = 骨架 + 认证层 + 配置接入 + 编译面接线）
//!
//! - 认证/分类/域名面：TDD 钉死（`tests/oauth_state_machine.rs` /
//!   `tests/errno_mapping.rs` / `tests/api_client.rs`）；
//! - **StorageDriver 九方法占位**（明确「未接线」形态的 `Unavailable`
//!   错误，见 [`Pan123Driver`]）：读路径 123-2、写路径 123-3、
//!   conformance + 12 装配点 123-4、真机矩阵 123-5——本批 dispatch
//!   臂同样占位（cloudkit-cli），没有任何装配路径能触达驱动的占位面；
//! - 纯函数层（envelope/时间双态/参数解析）单测在本文件与
//!   [`models`]。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate
//! （driver-onboarding §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。

pub mod api;
pub mod models;
pub mod oauth;

use std::sync::Arc;

use async_trait::async_trait;
use cloudkit_storage::{
    ByteStream, Capabilities, Entry, EntryId, Listing, Page, Quota, Range, RelPath, StorageDriver,
    StorageError, UploadStager, VolumeId, WriteHint,
};

pub use api::{Pan123Client, UA};
pub use oauth::TokenStore;

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
}

impl Default for Pan123Params {
    fn default() -> Self {
        Pan123Params {
            token: None,
            root: DEFAULT_ROOT.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            fallback_base: DEFAULT_FALLBACK_BASE.to_string(),
            login_base: DEFAULT_LOGIN_BASE.to_string(),
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

/// 123 存储驱动（123-1 骨架——九方法占位）。
///
/// - 语义：读路径 123-2（list/stat/mkdir/trash/rename/download_info
///   流读）、写路径 123-3（upload_request/分片 presign/complete）；
/// - 错误：占位面统一返回 `Unavailable`（载荷指明落地批次）；认证面
///   错误分类表见 [`api`]（本批钉死 20101/401 → 终态）；
/// - 并发：全方法可并发调用（`&self`）；无刷新状态机（web API 无
///   refresh——K76.4）；
/// - 生命周期：HTTP client 无连接态；域名粘性状态与 token 共享于
///   [`Pan123Client`]。
pub struct Pan123Driver {
    volume: VolumeId,
    client: Pan123Client,
    params: Pan123Params,
}

impl Pan123Driver {
    /// 同步构造（不连网——dydomain 在首请求前惰性解析；ck-pan115
    /// 骨架同款形态）。
    ///
    /// token 必须齐备（初始值来自 setup 扫码/sign_in 或手填配置；
    /// 缺失 → `Invalid`——core `validate()` 的第一道门之外，这里是
    /// 驱动侧的第二道）。
    ///
    /// **VolumeId 骨架占位**：`pan123:pending`——真值 `pan123:<uid>`
    /// （uid 取自 `/b/api/user/info` 的 `UID`）由 [`Pan123Driver::connect`]
    /// 确定（baidu factory connect 取 uid 先例）。123-1 无任何装配路径
    /// 触达本驱动（dispatch 占位臂），占位身份不进生产。
    pub fn new(params: Pan123Params) -> Result<Self, StorageError> {
        let Some(token) = &params.token else {
            return Err(StorageError::Invalid);
        };
        let client = Pan123Client::new(
            token.clone(),
            params.api_base.clone(),
            params.fallback_base.clone(),
            None,
        )?;
        Ok(Pan123Driver {
            volume: VolumeId::new("pan123", "pending")?,
            client,
            params,
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

    /// api client 的公开访问面（123-2 起驱动方法经它调用端点；123-4
    /// 的 transport/doctor 面共用同一域名/token 世界）。
    pub fn client(&self) -> &Pan123Client {
        &self.client
    }

    /// 驱动参数（123-2 的路径面消费 root；只读）。
    pub fn params(&self) -> &Pan123Params {
        &self.params
    }
}

/// 占位面的统一「未接线」形态：载荷指明落地批次（可行动文案——消费
/// 方/开发者一眼定位；ck-pan115 115-1 同款文案纪律）。
fn not_wired(face: &str, batch: &str) -> StorageError {
    StorageError::Unavailable(format!(
        "pan123 {face} lands in Phase 6 / {batch} (the 123-1 skeleton ships the auth layer, \
         config keys and the compile surface only)"
    ))
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
    /// 的 conformance 八断言在 123-4 验收——**resume 位在断言⑦（差集
    /// 续传驱动层可观测）通过后复核**，未过则降为 false（标注为
    /// 「计划位」）。123-1 无装配路径触达本驱动（dispatch 占位臂），
    /// 位不被生产消费。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // download_info → CDN Range 206 实证（123-0 ④：跨窗口
            // 逐字节 MATCH；dlink 会话内可复用）。
            range_read: true,
            // 计划位（断言⑦后复核）：同参重发返回同一 UploadId +
            // list_parts 保留分片 + 差集补传哈希 MATCH（123-0 ⑤）。
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

    async fn list(&self, _dir: &RelPath, _page: Page) -> Result<Listing, StorageError> {
        Err(not_wired("list", "123-2"))
    }

    async fn stat(&self, _path: &RelPath) -> Result<Entry, StorageError> {
        Err(not_wired("stat", "123-2"))
    }

    async fn mkdir(&self, _path: &RelPath) -> Result<(), StorageError> {
        Err(not_wired("mkdir", "123-2"))
    }

    async fn delete(&self, _id: &EntryId) -> Result<(), StorageError> {
        Err(not_wired("delete", "123-2"))
    }

    async fn rename(&self, _from: &RelPath, _to: &RelPath) -> Result<(), StorageError> {
        Err(not_wired("rename", "123-2"))
    }

    async fn reader(
        &self,
        _id: &EntryId,
        _range: Option<Range>,
    ) -> Result<ByteStream, StorageError> {
        Err(not_wired("reader", "123-2"))
    }

    async fn writer(
        &self,
        _path: &RelPath,
        _hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        Err(not_wired("writer", "123-3"))
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        Err(not_wired("quota", "123-2"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use cloudkit_storage::{Page, PageCursor, RelPath, StorageDriver, WriteHint};

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
    async fn nine_methods_are_explicit_not_wired_placeholders() {
        // 占位契约：九方法全部 `Unavailable` 且载荷指明落地批次——
        // 123-1 没有任何装配路径触达这里（cli dispatch 占位臂），钉住
        // 「占位必须明确」的形态本身。
        let driver = Pan123Driver::new(skeleton_params()).expect("driver");
        let root = RelPath::root();
        let id = EntryId::new(
            VolumeId::new("pan123", "pending").unwrap(),
            cloudkit_storage::BackendHandle::new("1"),
        );
        let not_wired_batch = |err: &StorageError, batch: &str| match err {
            StorageError::Unavailable(detail) => {
                detail.contains("Phase 6") && detail.contains(batch)
            }
            other => panic!("expected Unavailable, got {other:?}"),
        };
        let expect_unavailable = |result: Result<(), StorageError>, batch: &str| match result {
            Err(err) => assert!(not_wired_batch(&err, batch), "{err:?}"),
            Ok(()) => panic!("the placeholder must refuse, not succeed"),
        };
        expect_unavailable(
            driver
                .list(
                    &root,
                    Page {
                        limit: 10,
                        cursor: PageCursor::Start,
                    },
                )
                .await
                .map(|_| ()),
            "123-2",
        );
        expect_unavailable(driver.stat(&root).await.map(|_| ()), "123-2");
        expect_unavailable(driver.mkdir(&root).await, "123-2");
        expect_unavailable(driver.delete(&id).await, "123-2");
        expect_unavailable(driver.rename(&root, &root).await, "123-2");
        expect_unavailable(driver.reader(&id, None).await.map(|_| ()), "123-2");
        expect_unavailable(
            driver
                .writer(&root, &WriteHint::default())
                .await
                .map(|_| ()),
            "123-3",
        );
        expect_unavailable(driver.quota().await.map(|_| ()), "123-2");
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
