//! # ck-baidu——百度网盘驱动（L2 驱动 crate，Phase 2 Batch B1+B2）
//!
//! B1（已交付）：crate 骨架、oauth 刷新状态机（K13）、HTTP client（spike
//! `examples/baidu_spike` 改造复用）、errno 映射表（mock 钉死）与
//! StorageDriver **元数据面**（list/stat/mkdir/delete/rename/quota）。
//!
//! B2（本批）：writer 接三步曲上传（precreate rtype=3 / superfile2 4MiB
//! 分片串行落定 / create + K7 差集续传会话表，`upload.rs`）、reader 接
//! 下载器（dlink 缓存 K8 + 4MiB 有界 Range 分片流 K9 + 403 两段
//! fallback，`download.rs`）、conformance 八断言全绿 + 能力位六位点亮。
//!
//! 卷身份：`baidu:<uid>`（uinfo 取 uid，K5）；句柄 = fs_id 十进制字符串
//! （跨 rename 稳定，PCFS api.go:170-171 先例）。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate（driver-onboarding
//! §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。
//!
//! 网络形态（K18）：直连（no_proxy + 强制 IPv4 dial）+ netdisk 族 UA——
//! 实例配置 proxy_url 对本驱动无效（client.rs 构造处声明）。

mod api;
mod client;
mod download;
mod driver;
mod oauth;
mod upload;

use std::path::PathBuf;
use std::sync::Arc;

use cloudkit_storage::StorageError;

pub use driver::BaiduDriver;
pub use oauth::TokenStore;

/// 生产 API base（pan.baidu.com；spike api.rs:12 同值）。
pub const DEFAULT_API_BASE: &str = "https://pan.baidu.com";
/// 生产 OAuth base（openapi.baidu.com；spike api.rs:13 同值）。
pub const DEFAULT_OAUTH_BASE: &str = "https://openapi.baidu.com";
/// 生产 PCS base——superfile2 分片上传端点所在域（spike api.rs:14 /
/// PCFS api.go:522 同值；独立于 pan.baidu.com，测试经 `pcs_base` 注入
/// mock 后端）。
pub const DEFAULT_PCS_BASE: &str = "https://d.pcs.baidu.com";
/// dlink 缓存 TTL 缺省（秒，K8：spike 附录 B 实测下界 ≥96min，取 60min
/// 保守值；403/31326 两段 fallback 兜底过期残余）。
pub const DEFAULT_DLINK_TTL_SECS: u64 = 3600;
/// 卷根缺省（K17：config 键 `baidu_root` 的默认值）。
pub const DEFAULT_ROOT: &str = "/apps/cloudfs";

/// 百度驱动参数（driver-onboarding §4：「配置 map → 参数结构体」纯函数
/// 形态；B3b 组合根把 config 键 + env 覆盖解析为本结构体）。
///
/// - `app_key`/`app_secret` **无代码默认值**（K14/R3：凭据不入代码——
///   B3b 的 config 键 `baidu_app_key`/`baidu_app_secret` + env
///   `CYDRIVE_BAIDU_APP_KEY/SECRET` 由组合根解析注入）；
/// - `api_base`/`oauth_base` 为实例字段（默认生产常量、测试注入 mock
///   后端 base URL）；
/// - `token_store`：刷新产物持久化回调（K13）；`None` = 不持久化（仅
///   测试/一次性用途——正式装配必须提供，B3b 接 CredentialStore）。
///
/// 不实现 `Debug`：`Arc<dyn TokenStore>` 不可调试，且派生展开有把凭据
/// 字段印进日志的风险（R3——错误诊断走脱敏路径，不 dump 参数结构体）。
#[derive(Clone)]
pub struct BaiduParams {
    pub app_key: String,
    pub app_secret: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// 卷根（`/` 开头的后端绝对路径；RelPath 语义的 String 形态）。
    pub root: String,
    pub api_base: String,
    pub oauth_base: String,
    pub token_store: Option<Arc<dyn TokenStore>>,
    /// K7 会话表根目录：上传会话（path/size/block_md5/uploadid/完成位图）
    /// 随分片完成即刻落盘于 `<sessions_dir>/baidu_state/sessions/<hash>.json`，
    /// 供中断/进程重启后差集续传（Batch B2 契约，`tests/upload_resume.rs`
    /// 钉死）。`None` = 纯内存会话——单进程内的 stager 丢弃→重建恢复仍
    /// 可用（conformance ⑦形态），跨进程恢复不可用。
    pub sessions_dir: Option<PathBuf>,
    /// dlink 缓存 TTL（秒）；`None` = 缺省 3600（K8）。测试注入短 TTL 钉
    /// 过期重取腿（`tests/dlink_cache.rs`）。
    pub dlink_ttl_secs: Option<u64>,
    /// superfile2 端点 base；`None` = 生产常量 `d.pcs.baidu.com`（PCS 域
    /// 独立于 api_base——mock 后端经此注入单一 base URL）。
    pub pcs_base: Option<String>,
}

impl Default for BaiduParams {
    /// 凭据空置、base/root 取生产常量、可选注入位全 `None`。
    ///
    /// 意图：B2 起新增字段优先 Option 形态 + Default 补齐，既有构造点用
    /// `..Default::default()` 吸收字段扩散（B2 测试基建约定）。
    fn default() -> Self {
        BaiduParams {
            app_key: String::new(),
            app_secret: String::new(),
            access_token: None,
            refresh_token: None,
            root: DEFAULT_ROOT.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            oauth_base: DEFAULT_OAUTH_BASE.to_string(),
            token_store: None,
            sessions_dir: None,
            dlink_ttl_secs: None,
            pcs_base: None,
        }
    }
}

impl BaiduParams {
    /// 以给定凭据构造参数，其余字段填生产默认。
    ///
    /// 凭据必须显式提供（K14）；`access_token`/`refresh_token` 留空由
    /// 调用方按需填入（[`BaiduDriver::connect`] 要求两者齐备）。
    pub fn new(app_key: impl Into<String>, app_secret: impl Into<String>) -> Self {
        BaiduParams {
            app_key: app_key.into(),
            app_secret: app_secret.into(),
            access_token: None,
            refresh_token: None,
            root: DEFAULT_ROOT.to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            oauth_base: DEFAULT_OAUTH_BASE.to_string(),
            token_store: None,
            sessions_dir: None,
            dlink_ttl_secs: None,
            pcs_base: None,
        }
    }
}

/// 装配工厂：connect → uinfo 取 uid → VolumeId `baidu:<uid>`（K5）。
///
/// 要求 `access_token`/`refresh_token` 齐备（B1 无 device-code 授权流，
/// 初始 token 由 B3b setup 分支/凭据链提供）；缺失 → `Invalid`。
pub async fn factory(params: &BaiduParams) -> Result<Arc<BaiduDriver>, StorageError> {
    Ok(Arc::new(BaiduDriver::connect(params).await?))
}
