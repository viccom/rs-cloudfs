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
//! ## 批次边界（115-1 = 骨架 + 认证层 + 配置接入 + 编译面接线）
//!
//! - 认证/分类/限流面：TDD 钉死（`tests/oauth_state_machine.rs` /
//!   `tests/errno_mapping.rs` / `tests/limiter.rs`）；
//! - **StorageDriver 九方法占位**（明确「未接线」形态的 `Unavailable`
//!   错误，见 [`Pan115Driver`]）：读路径 115-2、写路径 115-3、
//!   conformance + 12 装配点 115-4、真机矩阵 115-5——本批 dispatch
//!   臂同样占位（cloudkit-cli），没有任何装配路径能触达驱动的占位面；
//! - 纯函数层（PKCE/签名输入构造/XML 工具/参数解析）单测在本文件。
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
pub mod limiter;
pub mod oauth;
pub mod oss;

use std::sync::Arc;

use async_trait::async_trait;
use cloudkit_storage::{
    ByteStream, Capabilities, Entry, EntryId, Listing, Page, Quota, Range, RelPath, StorageDriver,
    StorageError, UploadStager, VolumeId, WriteHint,
};

pub use api::{Pan115Client, UA};
pub use oauth::TokenStore;

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

/// 115 存储驱动（115-1 骨架——九方法占位）。
///
/// - 语义：读路径 115-2（list/stat/mkdir/delete/rename/downurl 流读）、
///   写路径 115-3（init/get_token/OSS 分片/complete/秒传）；
/// - 错误：占位面统一返回 `Unavailable`（载荷指明落地批次）；协议
///   错误分类表见 [`api`]（K69.7）；
/// - 并发：全方法可并发调用（`&self`）；token 刷新单飞（client）；
/// - 生命周期：HTTP client 无连接态；限流器（D4）与 token 状态共享
///   于 [`Pan115Client`]。
pub struct Pan115Driver {
    volume: VolumeId,
    client: Pan115Client,
    params: Pan115Params,
}

impl Pan115Driver {
    /// 同步构造（不连网——首请求惰性建立；sftp D3 同款骨架形态）。
    ///
    /// token 对必须齐备（初始对来自 setup 扫码或手填配置；缺失 →
    /// `Invalid`——core `validate()` 的第一道门之外，这里是驱动侧的
    /// 第二道）。
    ///
    /// **VolumeId 骨架占位**：`pan115:pending`——真值 `pan115:<uid>`
    /// （uid 取自 `user/info`）在 115-2 装配批确定（baidu 在 factory 的
    /// connect 阶段取 uid 先例；sftp 在 new 时以已知 host:port 合成）。
    /// 115-1 无任何装配路径触达本驱动（dispatch 占位臂），占位身份
    /// 不进生产。
    pub fn new(params: Pan115Params) -> Result<Self, StorageError> {
        let (Some(access), Some(refresh)) = (&params.access_token, &params.refresh_token) else {
            return Err(StorageError::Invalid);
        };
        let limiter_cfg = params.limiter.unwrap_or_default();
        let client = Pan115Client::new(
            access.clone(),
            refresh.clone(),
            params.passport_base.clone(),
            params.api_base.clone(),
            params.token_store.clone(),
            Arc::new(limiter::RateLimiter::new(limiter_cfg)),
        )?;
        Ok(Pan115Driver {
            volume: VolumeId::new("pan115", "pending")?,
            client,
            params,
        })
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

/// 占位面的统一「未接线」形态：载荷指明落地批次（可行动文案——消费
/// 方/开发者一眼定位；SF1 的 CLI 占位臂同款文案纪律）。
fn not_wired(face: &str, batch: &str) -> StorageError {
    StorageError::Unavailable(format!(
        "pan115 {face} lands in Phase 5 / {batch} (the 115-1 skeleton ships the auth layer, \
         config keys and the compile surface only)"
    ))
}

#[async_trait]
impl StorageDriver for Pan115Driver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    /// 能力位声明（计划 §4.1 的目标位）。
    ///
    /// R4 注记：这些位的**后端原语存在性**已由 115-0 真机验证（K69.4
    /// Range 206 + etag / K69.8 OSS 分片与 resume 差集 / move·delete·
    /// 秒传命中实证）；驱动面的 conformance 八断言在 115-4 验收——
    /// **resume 位在断言⑦（差集续传驱动层可观测）通过后复核**，
    /// 未过则降为 false（当前 115-4 未跑，标注为「计划位」）。
    /// 115-1 无装配路径触达本驱动（dispatch 占位臂），位不被生产消费。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // downurl + CDN Range 206 实证（K69.4：HEAD 探测可用，
            // Content-Range 起始吻合校验；CDN etag = MD5）。
            range_read: true,
            // 计划位（断言⑦后复核）：/open/upload/resume + OSS
            // ListParts 差集补片真机已过（K69.8）。
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

    async fn list(&self, _dir: &RelPath, _page: Page) -> Result<Listing, StorageError> {
        Err(not_wired("list", "115-2"))
    }

    async fn stat(&self, _path: &RelPath) -> Result<Entry, StorageError> {
        Err(not_wired("stat", "115-2"))
    }

    async fn mkdir(&self, _path: &RelPath) -> Result<(), StorageError> {
        Err(not_wired("mkdir", "115-2"))
    }

    async fn delete(&self, _id: &EntryId) -> Result<(), StorageError> {
        Err(not_wired("delete", "115-2"))
    }

    async fn rename(&self, _from: &RelPath, _to: &RelPath) -> Result<(), StorageError> {
        Err(not_wired("rename", "115-2"))
    }

    async fn reader(
        &self,
        _id: &EntryId,
        _range: Option<Range>,
    ) -> Result<ByteStream, StorageError> {
        Err(not_wired("reader", "115-2"))
    }

    async fn writer(
        &self,
        _path: &RelPath,
        _hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        Err(not_wired("writer", "115-3"))
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        Err(not_wired("quota", "115-2"))
    }
}

/// 装配工厂：构造驱动（**不连网**——首请求惰性建立；token 对齐备性
/// 在构造期校验）。
pub async fn factory(params: &Pan115Params) -> Result<Arc<Pan115Driver>, StorageError> {
    Ok(Arc::new(Pan115Driver::new(params.clone())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudkit_storage::{Page, RelPath, StorageDriver, WriteHint};

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
        // 计划 §4.1 逐位（resume 的「断言⑦后复核」注记见实现）。
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
    async fn nine_methods_hold_the_not_wired_shape() {
        // 占位形态稳定性：全部九方法拒绝且载荷指明落地批次（115-2 读
        // 面 / 115-3 写面）——115-4 装配前的任何触达都得到可行动错误。
        let driver = Pan115Driver::new(skeleton_params()).expect("driver");
        let root = RelPath::root();
        let check = |face: &'static str, err: StorageError, batch: &str| match &err {
            StorageError::Unavailable(detail) => {
                assert!(detail.contains(face), "{face} names itself: {detail}");
                assert!(detail.contains(batch), "{face} names {batch}: {detail}");
            }
            other => panic!("{face} must hold the Unavailable shape, got {other:?}"),
        };
        check(
            "list",
            driver.list(&root, Page::all()).await.expect_err("list"),
            "115-2",
        );
        check("stat", driver.stat(&root).await.expect_err("stat"), "115-2");
        check(
            "mkdir",
            driver.mkdir(&root).await.expect_err("mkdir"),
            "115-2",
        );
        let id = cloudkit_storage::EntryId::new(
            driver.volume().clone(),
            cloudkit_storage::BackendHandle::new("f.txt"),
        );
        check(
            "delete",
            driver.delete(&id).await.expect_err("delete"),
            "115-2",
        );
        check(
            "rename",
            driver
                .rename(&root, &RelPath::new("x").expect("rel"))
                .await
                .expect_err("rename"),
            "115-2",
        );
        check(
            "reader",
            driver.reader(&id, None).await.err().expect("reader"),
            "115-2",
        );
        check("quota", driver.quota().await.expect_err("quota"), "115-2");
        check(
            "writer",
            driver
                .writer(&root, &WriteHint::default())
                .await
                .err()
                .expect("writer"),
            "115-3",
        );
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
