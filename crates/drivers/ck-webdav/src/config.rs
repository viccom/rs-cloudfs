//! WebDAV 驱动参数与配置解析（Phase 7 / WD1a）。
//!
//! driver-onboarding §4 的形态：「配置 map → 驱动参数结构体」纯函数
//! 组织——组合根（WD4 的 dispatch 装配）把 config.toml / 卷文件的
//! `webdav_*` 键收集为 map 传进 [`parse_from_map`]，卷文件与
//! config.toml 走同一解析面（与 cloudkit-core 的严格键校验前后相继：
//! core 的 KNOWN_TOML_KEYS 挡未知键，本函数在驱动侧再挡拼写错误的
//! webdav 键与非法值——早失败、错误可行动）。
//!
//! 与 sftp/pan123 的 `from_pairs` 双门形态的差异：本函数错误载荷是
//! `String`（人话文案带键名与出路），供装配面（core `validate()` 的
//! Sftp 分支同型第二道门）与测试直接断言文案——K31 可行动文案风格。
//!
//! 键集 = 计划 §4.2 的六键（[`KNOWN_WEBDAV_KEYS`] 单一来源）：
//!
//! | 键 | 必填 | 语义 |
//! |---|---|---|
//! | `webdav_url` | ✔ | http(s) 完整 URL，可含挂载子路径（子路径即卷根，无独立 root 键） |
//! | `webdav_username` | 与 password 成对 | 认证用户名 |
//! | `webdav_password` | 与 username 成对 | SECRET（env 链 `CYDRIVE_WEBDAV_PASSWORD`，WD4 接线） |
//! | `webdav_auth` | 缺省 `auto` | `auto`/`basic`/`digest`（D1 协商形态） |
//! | `webdav_vendor` | 缺省 `generic` | `generic`/`nextcloud`（只影响 mtime 写策略，D4） |
//! | `webdav_accept_invalid_certs` | 缺省 `false` | 自签 NAS 开洞（D3；true 时启动 warn） |

use std::collections::HashMap;

use url::Url;

/// 驱动接受的六个 `webdav_*` 配置键（单一来源；装配面与未知键拒绝
/// 文案共用）。
pub const KNOWN_WEBDAV_KEYS: &[&str] = &[
    "webdav_url",
    "webdav_username",
    "webdav_password",
    "webdav_auth",
    "webdav_vendor",
    "webdav_accept_invalid_certs",
];

/// 认证形态（计划 §8-D1：Basic 预发 + Digest 401 challenge 协商，
/// 恰一次重试；stale nonce 再协商恰一次）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// 缺省：Basic 预发 → 401 Digest challenge → 协商（D1 全链路）。
    Auto,
    /// 只发 Basic（服务器拒绝即失败，不协商）。
    Basic,
    /// 只走 Digest（首请求裸发吃 401 后协商）。
    Digest,
}

/// 服务端风味键（计划 §8-D4：只影响 mtime 写策略，v1 不嗅探）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    /// 缺省风味。D2 降级后 generic **不写 mtime**（WD0 实证两家服务器
    /// 均不能真写：rclone 内层 403 / apache 假成功——附录 C ①）。
    Generic,
    /// Nextcloud：PUT 携 `X-OC-Mtime` 搭车（fixture 无 NC 未实证、零
    /// 成本保留，附录 C ②）。
    Nextcloud,
}

/// WebDAV 驱动参数（计划 §4.2 六键的载体）。
///
/// 不实现 `Debug`：结构体携带凭据（`password`），派生展开有把凭据印
/// 进日志的风险（R3——SftpParams/BaiduParams/Pan123Params 同款裁决）。
#[derive(Clone)]
pub struct WebdavParams {
    /// 服务器基地址（已规范化：尾斜杠形态——集合操作的怪癖面见
    /// urls.rs `collection_url`；子路径即卷根）。
    pub url: Url,
    /// 认证用户名（与 `password` 成对；双缺 = 匿名访问）。
    pub username: Option<String>,
    /// 认证密码（SECRET；与 `username` 成对）。
    pub password: Option<String>,
    /// 认证形态（缺省 [`AuthMode::Auto`]）。
    pub auth: AuthMode,
    /// 服务端风味（缺省 [`Vendor::Generic`]）。
    pub vendor: Vendor,
    /// 接受自签 TLS 证书（缺省 false；D3 rustls 严格校验默认）。
    pub accept_invalid_certs: bool,
}

/// 基地址规范化（纯函数）：解析 + scheme/host 校验 + 尾斜杠归一。
///
/// - scheme 必须是 `http`/`https`（其他 scheme 的 DAV 服务器不存在）；
/// - host 非空；
/// - 拒 query/fragment（DAV 基地址不带这两者，静默丢弃比拒绝更糟）；
/// - path 补尾斜杠（`https://h/dav` → `https://h/dav/`——appendix C ⑧
///   集合语义的规范形态；`https://h` → `https://h/`）。
fn normalize_base_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|error| {
        format!(
            "webdav_url is not a valid URL: {value:?} ({error}); expected e.g. \
             https://nas.lan:5006/dav/"
        )
    })?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!(
            "webdav_url must use http or https, got scheme {:?}; set e.g. https://nas.lan:5006/dav/",
            url.scheme()
        ));
    }
    // 防御性分支：http(s) 的已知空 host 形态在解析层已报错；解析成功
    // 但 host 缺失/为空的极端形态在此兜底（host 是 DAV 服务器的必要件）。
    if url.host_str().map(str::is_empty).unwrap_or(true) {
        return Err(format!(
            "webdav_url has an empty host: {value:?}; set the server address, e.g. \
             https://nas.lan:5006/dav/"
        ));
    }
    // WD4 挂账①：userinfo 形态（`https://user:pass@host/`）会把凭据带
    // 进 SHOW 回显与 sync namespace（卷身份携带完整 base URL）——拒收
    // 并指路凭据键（core validate 是第一道漏斗，这里是第二道）。
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "webdav_url must not embed credentials as userinfo (user:pass@host), got \
             {value:?}: set webdav_username and webdav_password instead — the separate \
             keys keep the credentials out of the volume URL"
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "webdav_url must not carry a query or fragment: {value:?}; the share URL is a plain \
             http(s) path, e.g. https://nas.lan:5006/dav/"
        ));
    }
    let mut normalized = url;
    if !normalized.path().ends_with('/') {
        let slashed = format!("{}/", normalized.path());
        normalized.set_path(&slashed);
    }
    Ok(normalized)
}

/// 从配置 map 解析（纯函数：无 IO、无网络）。
///
/// - 键集 = [`KNOWN_WEBDAV_KEYS`] 六键；未知键 → `Err`（拼写错误在
///   装配期暴露，而不是静默变成缺省值后连不上）；
/// - `webdav_url` 必填，经 [`normalize_base_url`] 校验与尾斜杠归一；
/// - `webdav_username` / `webdav_password` 成对（只给其一 = 错，文案
///   说明；双缺 = 匿名）；
/// - `webdav_auth` ∈ {auto, basic, digest}（宽大小写/白空格）；
/// - `webdav_vendor` ∈ {generic, nextcloud}（同宽）；
/// - `webdav_accept_invalid_certs` ∈ {"true", "false"}（同宽）；
/// - 一切键空串视为未设置（empty-means-unset——UPDATE 的 write-only
///   叠加依赖这一条，baidu/sftp 键同语义）。
pub fn parse_from_map(map: &HashMap<String, String>) -> Result<WebdavParams, String> {
    let non_empty =
        |key: &str| -> Option<String> { map.get(key).filter(|v| !v.is_empty()).cloned() };

    for key in map.keys() {
        if !KNOWN_WEBDAV_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "unknown webdav key {key:?}: the accepted keys are {}",
                KNOWN_WEBDAV_KEYS.join(", ")
            ));
        }
    }

    let url = non_empty("webdav_url")
        .ok_or_else(|| {
            "backend = \"webdav\" requires webdav_url: set the full http(s) URL of the WebDAV \
             share (the sub-path becomes the volume root) in config.toml (or the volume file)"
                .to_string()
        })
        .and_then(|value| normalize_base_url(&value))?;

    let username = non_empty("webdav_username");
    let password = non_empty("webdav_password");
    match (&username, &password) {
        (Some(_), None) => {
            return Err(
                "webdav_username and webdav_password must be set as a pair: only \
                 webdav_username is set — add webdav_password, or remove both for anonymous \
                 access"
                    .to_string(),
            )
        }
        (None, Some(_)) => {
            return Err(
                "webdav_username and webdav_password must be set as a pair: only \
                 webdav_password is set — add webdav_username, or remove both for anonymous \
                 access"
                    .to_string(),
            )
        }
        _ => {}
    }

    let auth = match non_empty("webdav_auth").as_deref() {
        None => AuthMode::Auto,
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "auto" => AuthMode::Auto,
            "basic" => AuthMode::Basic,
            "digest" => AuthMode::Digest,
            _ => {
                return Err(format!(
                    "webdav_auth must be one of auto, basic, digest, got {value:?} \
                     (auto = Basic first, then Digest negotiation on 401)"
                ))
            }
        },
    };

    let vendor = match non_empty("webdav_vendor").as_deref() {
        None => Vendor::Generic,
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "generic" => Vendor::Generic,
            "nextcloud" => Vendor::Nextcloud,
            _ => {
                return Err(format!(
                    "webdav_vendor must be one of generic, nextcloud, got {value:?} \
                     (vendor only tunes the mtime write strategy)"
                ))
            }
        },
    };

    let accept_invalid_certs = match non_empty("webdav_accept_invalid_certs").as_deref() {
        None => false,
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" => true,
            "false" => false,
            _ => {
                return Err(format!(
                    "webdav_accept_invalid_certs must be \"true\" or \"false\", got {value:?} \
                     (true accepts self-signed TLS certificates)"
                ))
            }
        },
    };

    Ok(WebdavParams {
        url,
        username,
        password,
        auth,
        vendor,
        accept_invalid_certs,
    })
}
