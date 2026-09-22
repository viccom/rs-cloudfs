//! # ck-webdav——WebDAV 存储驱动（L1 驱动 crate，Phase 7 / WD1a）。
//!
//! 后端 = 一台 WebDAV 服务器上的一个（可含子路径的）基地址
//! （`webdav_url`——子路径即卷根，无独立 root 键）。协议层为自铸薄
//! 异步客户端（reqwest + quick-xml，计划 §0/K80）：骨架移植 rs-f4ss
//! 已验证资产（连接池调优/重试白名单/URL 穿越防御），以两轮前置研究
//! （OpenList gowebdav fork 深读 + rs-f4ss 深度审计）的缺陷清单为负
//! 面清单正面修法（计划 §4.5），怪癖矩阵以 WD0 双服务器真机实证为
//! 设计输入（附录 C）。
//!
//! 驱动形态：StorageDriver 九方法（能力位四真六假，逐位依据见
//! driver.rs `capabilities()`）+ [`WebdavTransport`] CloudTransport
//! 薄壳（K2 路径寻址 / K6 0 占位，照 ck-local/ck-sftp 双面结构）。
//! D1–D6 拍板（计划 §8）：认证 = Basic 预发 + Digest 401 challenge
//! 协商恰一次（stale 再协商恰一次）；TLS = rustls 严格默认 +
//! `webdav_accept_invalid_certs` 开洞；vendor 键只影响 mtime 写策略
//! （D2 降级：generic 只读 mtime）；上传 = spool → PUT(.part 带
//! Content-Length) → MOVE 固化；连接 = 单 reqwest Client 池内并发。
//!
//! **批次边界（WD1a）**：网络动词面（PROPFIND/GET/PUT/...）以编译 +
//! clippy 为门，行为测试在 WD2 手搓桩批补；纯函数层（配置解析 /
//! URL 构造与穿越防御 / mtime 三格式 / challenge 解析 / multistatus
//! 解析 / 能力位）本批 TDD 红→绿钉死。装配接线（feature 三件套 +
//! dispatch 臂）在 WD1b 批。
//!
//! 层位置：只依赖 cloudkit-storage（L2）与外部 crate
//! （driver-onboarding §1）；禁依赖 cloudkit-core 及任何 L3+ crate（R1）。

mod auth;
mod client;
mod config;
mod driver;
mod mtime;
mod stager;
mod transport_face;
mod urls;
mod xml;

use std::sync::Arc;

use cloudkit_storage::StorageError;

pub use config::{parse_from_map, AuthMode, Vendor, WebdavParams, KNOWN_WEBDAV_KEYS};
pub use driver::WebdavDriver;
pub use mtime::parse_http_time;
pub use stager::WebdavStager;
pub use transport_face::WebdavTransport;
pub use urls::{collection_url, destination_uri, join_path};
pub use xml::is_addressable_name;

/// 装配工厂：构造驱动（**不连接**——reqwest 池惰性建连，D6；连接与
/// 认证协商发生在首次操作或 transport 面 `connect` 探活）。
///
/// 阻塞面：无（`WebdavDriver::new` 纯构造）。
pub async fn factory(cfg: &WebdavParams) -> Result<Arc<WebdavDriver>, StorageError> {
    Ok(Arc::new(WebdavDriver::new(cfg.clone())?))
}

// ------------------------------------------------------- doctor 探活 ---

/// doctor 探活的五态判定（WD4；`cydrive doctor` 的 webdav 连通腿与
/// 渲染器 [`crate`] 之外的 cloudkit-cli `doctor::webdav_connectivity_check`
/// 共用此枚举——渲染是纯函数，网络腿在这里）。
///
/// 与 sftp `SftpProbe` / pan115 `Pan115Probe` 同形态：结构化判据住驱
/// 动（CLI 侧无 HTTP 面），渲染器把每态映成一条可行动检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebdavProbe {
    /// ①可达且认证通过（`dav_class` = DAV 头摘要、`allow` = Allow 头
    /// 摘要——RFC 都不强制，缺席为 `None`，渲染照常 Ok）。
    Alive {
        dav_class: Option<String>,
        allow: Option<String>,
    },
    /// ②可达但凭据被拒（401 协商已尽——`detail` 携带与 warn 通道同
    /// 源的可行动文案：缺凭据 / Basic 拒 / NTLM / 协商后拒）。
    CredentialsRejected { detail: String },
    /// ③可达且无需认证（匿名可用——提示可配置凭据）。
    ReachableNoAuth,
    /// ④不可达（连接拒绝/超时/DNS——`detail` 保留传输错误原文）。
    Unreachable { detail: String },
    /// ⑤TLS 证书不可信（自签场景——网络可达、仅证书校验不过）。
    TlsUntrusted { detail: String },
}

/// doctor 探活（WD4）：一轮 OPTIONS 的五态判定。
///
/// 编排（两腿——`webdav_accept_invalid_certs=false` 时先宽后严）：
///
/// 1. **宽校验腿**（证书不校验的诊断客户端）：失败即定论——
///    `Unauthorized` → ②凭据被拒（分类细节读自协商路径的实际记录，
///    [`client::RejectionReason`]）；其余传输失败 → ④不可达（网络
///    都不通，无 TLS 话题）。
/// 2. **严格腿**（用户配置的真实校验姿态）：`accept_invalid_certs =
///    true` 时与宽腿同一客户端，单轮定 ①/③/②；否则宽腿已证网络可
///    达，严格腿的连接类失败只能是证书校验 → ⑤（服务器在两腿之间
///    抖动的竞态窗接受为诊断误差——doctor 重跑即分辨）。
///
/// ①/③ 的分界只在配置：有凭据 → ①（认证通过）；无凭据 → ③（匿名
/// 可用）。
///
/// 无外层 deadline——重试白名单 + connect 15s 界定单腿上限，调用方
/// （CLI probe）按 baidu 先例加 45s 外墙。
pub async fn probe(params: &WebdavParams) -> WebdavProbe {
    use client::RejectionReason;
    use cloudkit_storage::StorageError;

    /// 2xx 投影 → ①/③（有凭据 = 认证通过）。
    fn alive_or_anonymous(
        params: &WebdavParams,
        projected: (Option<String>, Option<String>),
    ) -> WebdavProbe {
        if params.username.is_some() {
            WebdavProbe::Alive {
                dav_class: projected.0,
                allow: projected.1,
            }
        } else {
            WebdavProbe::ReachableNoAuth
        }
    }

    /// 401 终局 → ②（detail = 与 warn 通道同源的单一来源文案）。
    fn rejection_detail(client: &client::WebdavClient) -> WebdavProbe {
        let detail = match client.last_rejection() {
            Some(RejectionReason::MissingCredentials) => client::message_missing_credentials(),
            Some(RejectionReason::BasicModeRefused { offered }) => {
                client::message_basic_mode_refused(&offered)
            }
            Some(RejectionReason::UnsupportedScheme(scheme)) => {
                client::message_unsupported_scheme(&scheme)
            }
            Some(RejectionReason::RejectedAfterNegotiation) | None => {
                client::message_credentials_rejected()
            }
        };
        WebdavProbe::CredentialsRejected { detail }
    }

    let strict = match client::WebdavClient::new(params) {
        Ok(client) => client,
        Err(error) => {
            return WebdavProbe::Unreachable {
                detail: error.to_string(),
            }
        }
    };

    if params.accept_invalid_certs {
        // 用户配置已开洞：单轮 OPTIONS 即真实姿态。
        return match strict.options_probe().await {
            Ok(projected) => alive_or_anonymous(params, projected),
            Err(StorageError::Unauthorized { .. }) => rejection_detail(&strict),
            Err(error) => WebdavProbe::Unreachable {
                detail: error.to_string(),
            },
        };
    }

    // 宽校验腿：证书不校验——失败即定论（网络不通没有 TLS 话题）。
    let mut lenient_params = params.clone();
    lenient_params.accept_invalid_certs = true;
    let lenient = match client::WebdavClient::new(&lenient_params) {
        Ok(client) => client,
        Err(error) => {
            return WebdavProbe::Unreachable {
                detail: error.to_string(),
            }
        }
    };
    match lenient.options_probe().await {
        Err(StorageError::Unauthorized { .. }) => return rejection_detail(&lenient),
        Err(error) => {
            return WebdavProbe::Unreachable {
                detail: error.to_string(),
            }
        }
        Ok(_) => {}
    }

    // 严格腿：网络已证可达——连接类失败 = 证书校验（⑤）。
    match strict.options_probe().await {
        Ok(projected) => alive_or_anonymous(params, projected),
        Err(StorageError::Unauthorized { .. }) => rejection_detail(&strict),
        Err(error) => WebdavProbe::TlsUntrusted {
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
// WebdavParams 故意不实现 Debug（R3——凭据不入日志），`expect_err` 在
// Ok 侧无 Debug 的调用点上不可用；错误路径断言统一走 `.err().expect`。
#[allow(clippy::err_expect)]
mod tests {
    use super::*;
    use cloudkit_storage::{RelPath, StorageDriver, VolumeId};
    use std::collections::HashMap;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn base_url() -> url::Url {
        url::Url::parse("https://nas.lan:5006/dav/").expect("constant URL")
    }

    // ------------------------------------------------------ config 面 ---

    #[test]
    fn config_resolves_all_six_keys() {
        let params = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan:5006/dav"),
            ("webdav_username", "spike"),
            ("webdav_password", "pw"),
            ("webdav_auth", "digest"),
            ("webdav_vendor", "nextcloud"),
            ("webdav_accept_invalid_certs", "true"),
        ]))
        .expect("complete map parses");
        assert_eq!(params.url.as_str(), "https://nas.lan:5006/dav/");
        assert_eq!(params.username.as_deref(), Some("spike"));
        assert_eq!(params.password.as_deref(), Some("pw"));
        assert_eq!(params.auth, AuthMode::Digest);
        assert_eq!(params.vendor, Vendor::Nextcloud);
        assert!(params.accept_invalid_certs);
    }

    #[test]
    fn config_applies_the_three_defaults_and_anonymous_access() {
        let params =
            parse_from_map(&map(&[("webdav_url", "https://nas.lan:5006/dav/")])).expect("parses");
        assert_eq!(params.auth, AuthMode::Auto);
        assert_eq!(params.vendor, Vendor::Generic);
        assert!(!params.accept_invalid_certs);
        assert_eq!(
            params.username.as_deref(),
            None,
            "anonymous when both absent"
        );
        assert_eq!(params.password.as_deref(), None);
    }

    #[test]
    fn config_requires_the_url_with_an_actionable_message() {
        let error = parse_from_map(&map(&[])).err().expect("missing webdav_url");
        assert!(
            error.contains("webdav_url") && error.contains("requires"),
            "message must name the key and the problem: {error}"
        );
        // 空串 = 未设置（empty-means-unset）
        let empty = parse_from_map(&map(&[("webdav_url", "")]))
            .err()
            .expect("empty url");
        assert!(empty.contains("webdav_url"), "{empty}");
    }

    #[test]
    fn config_rejects_non_http_schemes() {
        let error = parse_from_map(&map(&[("webdav_url", "ftp://nas.lan/dav/")]))
            .err()
            .expect("ftp");
        assert!(
            error.contains("webdav_url") && error.contains("http"),
            "{error}"
        );
        let error = parse_from_map(&map(&[("webdav_url", "not a url")]))
            .err()
            .expect("garbage");
        assert!(error.contains("webdav_url"), "{error}");
    }

    #[test]
    fn config_rejects_empty_hosts() {
        // 注：`https:///dav/` 不是空 host 形态——url crate 跳过多余斜杠
        // 后把 `dav` 当 host（技术上是合法 URL）；真空 host 是显式
        // `://:port/` 形态，在解析层即报 empty host。
        let error = parse_from_map(&map(&[("webdav_url", "https://:5006/dav/")]))
            .err()
            .expect("empty host");
        assert!(error.contains("webdav_url"), "{error}");
    }

    #[test]
    fn config_rejects_query_and_fragment_on_the_base() {
        for bad in ["https://nas.lan/dav/?x=1", "https://nas.lan/dav/#f"] {
            let error = parse_from_map(&map(&[("webdav_url", bad)]))
                .err()
                .expect(bad);
            assert!(
                error.contains("webdav_url") && error.contains("query"),
                "{error}"
            );
        }
    }

    #[test]
    fn config_rejects_userinfo_embedded_credentials() {
        // WD4 挂账①裁决：`https://user:pass@host/` 的 userinfo 形态会把
        // 凭据带进 SHOW 回显与 sync namespace（卷身份携带完整 base
        // URL——SECRET 清单只盖 webdav_password 键）。双漏斗拒收之一。
        let error = parse_from_map(&map(&[(
            "webdav_url",
            "https://spike:pw@nas.lan:5006/dav/",
        )]))
        .err()
        .expect("user:pass userinfo must refuse");
        assert!(
            error.contains("webdav_username") && error.contains("webdav_password"),
            "the refusal routes credentials to their keys: {error}"
        );
        // username-only userinfo（password 可缺省形态）同拒。
        let error = parse_from_map(&map(&[("webdav_url", "https://spike@nas.lan:5006/dav/")]))
            .err()
            .expect("username-only userinfo must refuse too");
        assert!(
            error.contains("webdav_username"),
            "the refusal names the credential key: {error}"
        );
    }

    #[test]
    fn config_rejects_split_credentials() {
        let error = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_username", "spike"),
        ]))
        .err()
        .expect("username only");
        assert!(
            error.contains("webdav_username") && error.contains("webdav_password"),
            "message must name both keys: {error}"
        );
        let error = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_password", "pw"),
        ]))
        .err()
        .expect("password only");
        assert!(
            error.contains("webdav_username") && error.contains("webdav_password"),
            "{error}"
        );
    }

    #[test]
    fn config_rejects_bad_auth_values_by_listing_the_legal_ones() {
        let error = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_auth", "ntlm"),
        ]))
        .err()
        .expect("ntlm");
        assert!(
            error.contains("webdav_auth") && error.contains("auto, basic, digest"),
            "{error}"
        );
        // 宽大小写 + 白空格
        let lenient = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_auth", " Digest "),
        ]))
        .expect("lenient casing/whitespace");
        assert_eq!(lenient.auth, AuthMode::Digest);
    }

    #[test]
    fn config_rejects_bad_vendor_values_by_listing_the_legal_ones() {
        let error = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_vendor", "owncloud"),
        ]))
        .err()
        .expect("owncloud");
        assert!(
            error.contains("webdav_vendor") && error.contains("generic, nextcloud"),
            "{error}"
        );
    }

    #[test]
    fn config_parses_accept_invalid_certs_in_three_states() {
        let yes = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_accept_invalid_certs", "true"),
        ]))
        .expect("true");
        assert!(yes.accept_invalid_certs);
        let lenient = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_accept_invalid_certs", " FALSE "),
        ]))
        .expect("lenient false");
        assert!(!lenient.accept_invalid_certs);
        let error = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_accept_invalid_certs", "maybe"),
        ]))
        .err()
        .expect("maybe");
        assert!(
            error.contains("webdav_accept_invalid_certs") && error.contains("true"),
            "{error}"
        );
    }

    #[test]
    fn config_normalizes_the_trailing_slash() {
        let sub = parse_from_map(&map(&[("webdav_url", "https://nas.lan:5006/dav")]))
            .expect("no trailing slash");
        assert_eq!(sub.url.as_str(), "https://nas.lan:5006/dav/");
        let bare = parse_from_map(&map(&[("webdav_url", "https://nas.lan")])).expect("bare host");
        assert_eq!(bare.url.as_str(), "https://nas.lan/");
    }

    #[test]
    fn config_rejects_unknown_keys_by_listing_the_accepted_set() {
        let error = parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan/dav/"),
            ("webdav_pasword", "x"),
        ]))
        .err()
        .expect("typoed key");
        assert!(error.contains("unknown webdav key"), "{error}");
        for key in KNOWN_WEBDAV_KEYS {
            assert!(error.contains(key), "message lists {key}: {error}");
        }
    }

    #[test]
    fn known_keys_constant_is_exactly_the_six_config_keys() {
        assert_eq!(KNOWN_WEBDAV_KEYS.len(), 6);
        for key in [
            "webdav_url",
            "webdav_username",
            "webdav_password",
            "webdav_auth",
            "webdav_vendor",
            "webdav_accept_invalid_certs",
        ] {
            assert!(KNOWN_WEBDAV_KEYS.contains(&key), "missing {key}");
        }
    }

    // -------------------------------------------------------- urls 面 ---

    #[test]
    fn urls_join_appends_segments_under_a_slashed_base() {
        let joined = join_path(&base_url(), "a/b.txt").expect("joins");
        assert_eq!(joined.as_str(), "https://nas.lan:5006/dav/a/b.txt");
        // 无尾斜杠 base 同样折叠为单斜杠
        let unslashed = url::Url::parse("https://nas.lan:5006/dav").expect("url");
        assert_eq!(
            join_path(&unslashed, "a/b.txt").expect("joins").as_str(),
            "https://nas.lan:5006/dav/a/b.txt"
        );
    }

    #[test]
    fn urls_join_rejects_traversal_segments() {
        for bad in ["../x", "a/../b", "./x", "a/./b"] {
            assert!(
                join_path(&base_url(), bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn urls_join_rejects_percent_encoded_traversal() {
        for bad in ["%2e%2e", "%2E%2e", "a/%2e%2E/b", "%2E"] {
            assert!(
                join_path(&base_url(), bad).is_err(),
                "encoded traversal {bad:?} must be rejected after percent-decoding"
            );
        }
    }

    #[test]
    fn urls_join_folds_double_slashes_and_tolerates_edges() {
        assert_eq!(
            join_path(&base_url(), "a//b").expect("folds").as_str(),
            "https://nas.lan:5006/dav/a/b"
        );
        assert_eq!(
            join_path(&base_url(), "/a/").expect("edges").as_str(),
            "https://nas.lan:5006/dav/a"
        );
    }

    #[test]
    fn urls_join_percent_encodes_spaces_unicode_and_literal_percent() {
        assert_eq!(
            join_path(&base_url(), "my file.txt")
                .expect("space")
                .as_str(),
            "https://nas.lan:5006/dav/my%20file.txt"
        );
        assert_eq!(
            join_path(&base_url(), "ü.txt").expect("unicode").as_str(),
            "https://nas.lan:5006/dav/%C3%BC.txt"
        );
        assert_eq!(
            join_path(&base_url(), "a%b.bin")
                .expect("literal percent")
                .as_str(),
            "https://nas.lan:5006/dav/a%25b.bin"
        );
        assert_eq!(
            join_path(&base_url(), "dir one/my file.txt")
                .expect("nested")
                .as_str(),
            "https://nas.lan:5006/dav/dir%20one/my%20file.txt"
        );
    }

    #[test]
    fn urls_join_rejects_nul_bytes() {
        assert!(join_path(&base_url(), "a\0b").is_err());
    }

    #[test]
    fn urls_join_root_path_is_the_trailing_slash_form() {
        assert_eq!(
            join_path(&base_url(), "").expect("root").as_str(),
            "https://nas.lan:5006/dav/"
        );
        let unslashed = url::Url::parse("https://nas.lan:5006/dav").expect("url");
        assert_eq!(
            join_path(&unslashed, "").expect("root").as_str(),
            "https://nas.lan:5006/dav/"
        );
    }

    #[test]
    fn urls_collection_url_appends_the_trailing_slash() {
        let slashed =
            collection_url(&url::Url::parse("https://nas.lan:5006/dav/sub").expect("url"));
        assert_eq!(slashed.as_str(), "https://nas.lan:5006/dav/sub/");
        let already =
            collection_url(&url::Url::parse("https://nas.lan:5006/dav/sub/").expect("url"));
        assert_eq!(already.as_str(), "https://nas.lan:5006/dav/sub/");
    }

    #[test]
    fn urls_destination_uri_is_an_absolute_encoded_uri() {
        // 附录 C ⑩：apache 拒相对 Destination（400）——恒绝对 URI。
        let destination = destination_uri(&base_url(), "a b.txt").expect("absolute URI");
        assert_eq!(destination, "https://nas.lan:5006/dav/a%20b.txt");
        assert!(destination.starts_with("https://"), "absolute, not a path");
    }

    // ------------------------------------------------------- mtime 面 ---

    #[test]
    fn mtime_parses_imf_fixdate_with_the_wd0_sample() {
        // WD0 真机样本（附录 C ⑧：双服务器 mtime 形态）。
        assert_eq!(
            parse_http_time("Mon, 21 Sep 2026 10:51:05 GMT"),
            Some(1_789_987_865)
        );
        assert_eq!(
            parse_http_time("  Mon, 21 Sep 2026 10:51:05 GMT "),
            Some(1_789_987_865)
        );
    }

    #[test]
    fn mtime_parses_rfc850_with_two_digit_years() {
        assert_eq!(
            parse_http_time("Monday, 21-Sep-26 10:51:05 GMT"),
            Some(1_789_987_865)
        );
        assert_eq!(
            parse_http_time("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
    }

    #[test]
    fn mtime_parses_asctime_including_space_padded_days() {
        assert_eq!(
            parse_http_time("Mon Sep 21 10:51:05 2026"),
            Some(1_789_987_865)
        );
        assert_eq!(
            parse_http_time("Sun Nov  6 08:49:37 1994"),
            Some(784_111_777)
        );
    }

    #[test]
    fn mtime_two_digit_year_boundary_follows_rfc9110() {
        // RFC 9110 §5.6.7：0-49 → 2000s，50-99 → 1900s。
        assert_eq!(
            parse_http_time("Friday, 01-Jan-49 00:00:00 GMT"),
            Some(2_493_072_000)
        );
        assert_eq!(
            parse_http_time("Sunday, 01-Jan-50 00:00:00 GMT"),
            Some(-631_152_000)
        );
        assert_eq!(
            parse_http_time("Friday, 01-Jan-99 00:00:00 GMT"),
            Some(915_148_800)
        );
    }

    #[test]
    fn mtime_garbage_is_none_never_a_silent_epoch_zero() {
        for bad in [
            "",
            "not a date",
            "Mon, 21 Sep 2026 10:51:05",
            "Mon, 21 Xyz 2026 10:51:05 GMT",
            "Monday, 30-Feb-26 10:00:00 GMT",
            "Mon Sep 21 25:61:61 2026",
            "Monday, 21-Sep-26 10:00:00",
            "Wed Feb 30 10:00:00 2026",
        ] {
            assert_eq!(parse_http_time(bad), None, "{bad:?} must not parse");
        }
    }

    // -------------------------------------------------------- auth 面 ---

    /// WD0 真形（附录 C ⑨）：apache challenge，stale=true 位置不定
    /// （实测在 algorithm 之后）。
    const APACHE_CHALLENGE: &str =
        "Digest realm=\"webdavtest\", nonce=\"abc+/=\", algorithm=MD5, stale=true, qop=\"auth\"";

    #[test]
    fn auth_quoted_comma_list_stays_intact() {
        let challenge =
            crate::auth::parse_challenge("Digest realm=\"r\", nonce=\"n\", qop=\"auth,auth-int\"")
                .expect("parses");
        assert_eq!(challenge.qop.as_deref(), Some("auth,auth-int"));
        assert_eq!(challenge.realm, "r");
        assert_eq!(challenge.nonce, "n");
    }

    #[test]
    fn auth_challenge_parsing_is_order_independent_and_reads_late_stale() {
        let challenge = crate::auth::parse_challenge(APACHE_CHALLENGE).expect("parses");
        assert_eq!(challenge.realm, "webdavtest");
        assert_eq!(challenge.nonce, "abc+/=");
        assert_eq!(challenge.algorithm.as_deref(), Some("MD5"));
        assert_eq!(challenge.qop.as_deref(), Some("auth"));
        assert!(challenge.stale, "stale=true after algorithm must be read");
        assert_eq!(challenge.opaque, None);
    }

    #[test]
    fn auth_rejects_non_digest_schemes() {
        let error = crate::auth::parse_challenge("Basic realm=\"rclone\"")
            .err()
            .expect("basic");
        assert!(error.contains("Digest"), "{error}");
    }

    #[test]
    fn auth_requires_realm_and_nonce() {
        assert!(crate::auth::parse_challenge("Digest nonce=\"n\"").is_err());
        assert!(crate::auth::parse_challenge("Digest realm=\"r\"").is_err());
    }

    #[test]
    fn auth_rejects_non_md5_algorithms() {
        for challenge in [
            "Digest realm=\"r\", nonce=\"n\", algorithm=SHA-256",
            "Digest realm=\"r\", nonce=\"n\", algorithm=MD5-sess",
        ] {
            let error = crate::auth::parse_challenge(challenge)
                .err()
                .expect(challenge);
            assert!(error.contains("MD5"), "{error}");
        }
    }

    #[test]
    fn auth_response_md5_pins_the_standard_vector() {
        // RFC 2617 §3.5 标准向量（RFC 7616 承接的 MD5 基线例）：
        // Mufasa / "Circle Of Life" @ testrealm@host.com。
        let response = crate::auth::response_md5(
            "Mufasa",
            "Circle Of Life",
            "testrealm@host.com",
            "GET",
            "/dir/index.html",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            1,
            "0a4f113b",
            Some("auth"),
        );
        assert_eq!(response, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn auth_response_md5_without_qop_uses_the_legacy_form() {
        // 同向量去掉 qop（RFC 2069 兼容形态；期望值经独立工具链计算）。
        let response = crate::auth::response_md5(
            "Mufasa",
            "Circle Of Life",
            "testrealm@host.com",
            "GET",
            "/dir/index.html",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            1,
            "0a4f113b",
            None,
        );
        assert_eq!(response, "670fd8c2df070c60b045671b8b24ff02");
    }

    #[test]
    fn auth_rand_cnonce_is_16_lowercase_hex_and_fresh() {
        let a = crate::auth::rand_cnonce();
        let b = crate::auth::rand_cnonce();
        assert_eq!(a.len(), 16, "16 hex chars: {a}");
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "lowercase hex: {a}"
        );
        assert_ne!(a, b, "fresh randomness per draw");
    }

    #[test]
    fn auth_authorization_header_shape_and_vector_crosscheck() {
        let challenge = crate::auth::parse_challenge(
            "Digest realm=\"testrealm@host.com\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", \
             algorithm=MD5, qop=\"auth,auth-int\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"",
        )
        .expect("parses");
        let mut session = crate::auth::DigestSession::from_challenge(&challenge);
        assert_eq!(session.nc, 0, "nc starts at zero");
        session.nc = 1; // 首个签名请求
        let header = crate::auth::authorization_header(
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            &session,
            "0a4f113b",
        );
        assert!(header.starts_with("Digest "), "{header}");
        for fragment in [
            "username=\"Mufasa\"",
            "realm=\"testrealm@host.com\"",
            "nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\"",
            "uri=\"/dir/index.html\"",
            "response=\"6629fae49393a05397450978507c4ef1\"",
            "qop=auth",
            "nc=00000001",
            "cnonce=\"0a4f113b\"",
            "algorithm=MD5",
            "opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"",
        ] {
            assert!(header.contains(fragment), "missing {fragment} in {header}");
        }
    }

    // -------------------------------------------------------- xml 面 ---

    /// apache 真形（附录 C ⑧）：同一文档 `D:`/`ns0:`/`lp1:`/`g0:` 多前缀
    /// 并存（皆绑 `DAV:`）+ `lp2:` 绑 apache 私有 props 命名空间 + href
    /// 实体/百分号混合编码 + 目录的 getcontentlength 在内层 404
    /// propstat（404 块置于 200 块**之前**——非 2xx 行不得覆盖 2xx 真值）。
    const APACHE_MULTISTATUS: &str = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:" xmlns:lp1="DAV:" "#,
        r#"xmlns:g0="DAV:" xmlns:lp2="http://apache.org/dav/props/">"#,
        r#"<D:response><D:href>/dav/docs/</D:href>"#,
        r#"<D:propstat><D:prop><g0:getcontentlength>999</g0:getcontentlength></D:prop>"#,
        r#"<D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"#,
        r#"<D:propstat><D:prop><lp1:resourcetype><D:collection/></lp1:resourcetype>"#,
        r#"<lp1:getlastmodified>Mon, 21 Sep 2026 10:51:05 GMT</lp1:getlastmodified>"#,
        r#"<lp2:executable F/>"#,
        r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
        r#"</D:response>"#,
        r#"<D:response><D:href>/dav/docs/a%20b&amp;c%C3%BC.txt</D:href>"#,
        r#"<D:propstat><D:prop><ns0:resourcetype/>"#,
        r#"<lp1:getcontentlength>1234</lp1:getcontentlength>"#,
        r#"<lp1:getlastmodified>Mon, 21 Sep 2026 10:51:06 GMT</lp1:getlastmodified>"#,
        r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
        r#"</D:response>"#,
        r#"</D:multistatus>"#,
    );

    #[test]
    fn xml_apache_multi_prefix_local_name_parsing() {
        let rows = crate::xml::parse_multistatus(APACHE_MULTISTATUS).expect("parses");
        assert!(!rows.is_empty());
        assert!(
            rows.iter().any(|row| row
                .propstat_status
                .as_deref()
                .is_some_and(|status| status.contains("404"))),
            "the inner 404 propstat must be observable on rows"
        );
        let entries = crate::xml::entries(&rows);
        assert_eq!(entries.len(), 2, "one entry per response href");
        let dir = &entries[0];
        assert_eq!(dir.href, "/dav/docs/");
        assert!(dir.is_collection);
        assert_eq!(
            dir.content_length, None,
            "the poisoned 404-block value must not clobber the 2xx projection"
        );
        assert_eq!(
            dir.last_modified.as_deref(),
            Some("Mon, 21 Sep 2026 10:51:05 GMT")
        );
        let file = &entries[1];
        assert!(!file.is_collection);
        assert_eq!(file.content_length, Some(1234));
    }

    #[test]
    fn xml_href_entity_and_percent_decoding_plus_stays_literal() {
        let rows = crate::xml::parse_multistatus(APACHE_MULTISTATUS).expect("parses");
        let entries = crate::xml::entries(&rows);
        // `&amp;` → &、`%20` → 空格、`%C3%BC` → ü（大写十六进制容忍）。
        assert_eq!(entries[1].href, "/dav/docs/a b&cü.txt");

        // WD0 实证：href 里的 `+` 是字面量（只有 query 串才解释为空格）。
        let plus = concat!(
            r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/a+b.txt</D:href>"#,
            r#"<D:propstat><D:prop><D:resourcetype/><D:getcontentlength>1</D:getcontentlength>"#,
            r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
            r#"</D:response></D:multistatus>"#,
        );
        let entries = crate::xml::entries(&crate::xml::parse_multistatus(plus).expect("parses"));
        assert_eq!(entries[0].href, "/a+b.txt");
        assert_eq!(entries[0].content_length, Some(1));
    }

    #[test]
    fn xml_rclone_form_with_empty_props_and_zero_length() {
        // rclone 真形（附录 C ⑧）：`D:` 前缀 + 空 prop（Empty 事件）+
        // 零字节文件 getcontentlength=0（Some(0) ≠ None——0 是真值）。
        let body = concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<D:multistatus xmlns:D="DAV:">"#,
            r#"<D:response><D:href>/</D:href>"#,
            r#"<D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype>"#,
            r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"#,
            r#"<D:response><D:href>/empty.bin</D:href>"#,
            r#"<D:propstat><D:prop><D:resourcetype/>"#,
            r#"<D:getcontentlength>0</D:getcontentlength>"#,
            r#"<D:getlastmodified>Mon, 21 Sep 2026 10:51:05 GMT</D:getlastmodified>"#,
            r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"#,
            r#"</D:multistatus>"#,
        );
        let entries = crate::xml::entries(&crate::xml::parse_multistatus(body).expect("parses"));
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_collection);
        assert!(!entries[1].is_collection);
        assert_eq!(entries[1].content_length, Some(0));
        assert_eq!(
            entries[1].last_modified.as_deref(),
            Some("Mon, 21 Sep 2026 10:51:05 GMT")
        );
    }

    #[test]
    fn xml_non_multistatus_body_yields_no_rows() {
        let rows = crate::xml::parse_multistatus("<html><body>404</body></html>").expect("lenient");
        assert!(rows.is_empty());
    }

    #[test]
    fn xml_addressable_name_filter() {
        // K67：`\`/`\0`/lossy 形态不可见（「list 产出即可寻址」）。
        assert!(is_addressable_name("normal.txt"));
        assert!(is_addressable_name("中文文件.bin"));
        assert!(is_addressable_name("a+b.txt"));
        assert!(!is_addressable_name("back\\slash.txt"));
        assert!(!is_addressable_name("nul\0byte"));
        assert!(!is_addressable_name("mojibake\u{FFFD}.txt"));
    }

    // ------------------------------------------------------ driver 面 ---

    fn params() -> WebdavParams {
        parse_from_map(&map(&[
            ("webdav_url", "https://nas.lan:5006/dav/"),
            ("webdav_username", "spike"),
            ("webdav_password", "pw"),
        ]))
        .expect("params")
    }

    #[test]
    fn driver_volume_identity_and_offline_construction() {
        // 构造不碰网络（reqwest 池惰性建连）——本测试能跑过就是证明。
        let driver = WebdavDriver::new(params()).expect("constructs offline");
        let volume = VolumeId::new("webdav", "spike@https://nas.lan:5006/dav/").expect("volume");
        assert_eq!(*driver.volume(), volume);

        let anonymous = parse_from_map(&map(&[("webdav_url", "https://nas.lan:5006/dav/")]))
            .expect("anonymous");
        let driver = WebdavDriver::new(anonymous).expect("constructs");
        let volume =
            VolumeId::new("webdav", "anonymous@https://nas.lan:5006/dav/").expect("volume");
        assert_eq!(*driver.volume(), volume);
    }

    #[test]
    fn driver_capabilities_declare_the_planned_bits() {
        // 计划 §4.3 逐位：range_read / server_side_move /
        // authoritative_index / remote_delete 真；其余假。
        let driver = WebdavDriver::new(params()).expect("driver");
        let caps = driver.capabilities();
        assert!(caps.range_read);
        assert!(caps.server_side_move);
        assert!(caps.authoritative_index);
        assert!(caps.remote_delete);
        assert!(!caps.resume);
        assert!(!caps.multipart);
        assert!(!caps.rapid_upload);
        assert!(!caps.change_feed);
        assert!(!caps.inbound);
        assert!(!caps.chat);
    }

    #[tokio::test]
    async fn driver_offline_construction_survives_wd2b_wiring() {
        // WD1a 的「动词面占位恒 Unsupported」断言随 WD2b 读面接线移除
        //（其文档注明的生命周期）——行为面由 tests/read_path.rs 的桩
        // 套件接管；本测试只钉「构造仍不碰网络」。
        let driver = WebdavDriver::new(params()).expect("driver");
        assert_eq!(
            driver.volume().as_str(),
            "webdav:spike@https://nas.lan:5006/dav/"
        );
    }

    // ---------------------------------------------------- client 面 ---
    // WD2b 纯函数层（重试白名单/退避档/片段截断/Content-Range 解析/
    // 可行动文案）——桩面行为由 tests/connect_auth.rs 钉死，这里钉数学
    // 与字面。

    #[test]
    fn client_retry_whitelist_covers_only_idempotent_verbs() {
        // §4.5-10：PUT/MOVE/MKCOL/DELETE/PROPPATCH 非幂等，永不自动重试。
        for verb in ["GET", "HEAD", "PROPFIND", "OPTIONS"] {
            assert!(
                crate::client::verb_is_retryable(verb),
                "{verb} is idempotent and retryable"
            );
        }
        for verb in ["PUT", "MOVE", "MKCOL", "DELETE", "PROPPATCH"] {
            assert!(
                !crate::client::verb_is_retryable(verb),
                "{verb} is NOT auto-retryable (non-idempotent)"
            );
        }
    }

    #[test]
    fn client_backoff_is_exponential_and_capped() {
        use std::time::Duration;
        // 500ms → 1s → 2s → 4s … 封顶 30s。
        assert_eq!(
            crate::client::backoff_delay(1, None),
            Duration::from_millis(500)
        );
        assert_eq!(
            crate::client::backoff_delay(2, None),
            Duration::from_secs(1)
        );
        assert_eq!(
            crate::client::backoff_delay(3, None),
            Duration::from_secs(2)
        );
        assert_eq!(
            crate::client::backoff_delay(9, None),
            Duration::from_secs(30),
            "capped at 30s"
        );
    }

    #[test]
    fn client_retry_after_takes_priority_and_clamps_one_to_sixty() {
        use std::time::Duration;
        // K79.3：服务端明示的节奏优先；0 → 1s（防空转）、999 → 60s
        //（防勒索性长眠）。
        assert_eq!(
            crate::client::backoff_delay(1, Some(0)),
            Duration::from_secs(1)
        );
        assert_eq!(
            crate::client::backoff_delay(1, Some(7)),
            Duration::from_secs(7)
        );
        assert_eq!(
            crate::client::backoff_delay(1, Some(999)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn client_snippet_truncates_at_two_hundred_bytes_on_char_boundaries() {
        let tiny = "short body";
        assert_eq!(crate::client::snippet(tiny), tiny);
        let long = "x".repeat(500);
        let cut = crate::client::snippet(&long);
        let body = cut.strip_suffix('…').expect("truncation marker");
        assert_eq!(body.len(), 200, "body capped at 200 bytes");
        // 多字节字符不劈开（char boundary 回退：ü = 2 字节 → 100 字符）。
        let unicode = "ü".repeat(150); // 300 字节
        let cut = crate::client::snippet(&unicode);
        let body = cut.strip_suffix('…').expect("truncation marker");
        assert_eq!(body.chars().count(), 100);
        assert!(body.chars().all(|c| c == 'ü'));
    }

    #[test]
    fn client_content_range_parsing_covers_the_real_shapes() {
        // 矩阵④真形：`bytes 10-19/100`、钳制 `bytes 90-99/100`。
        assert_eq!(
            crate::client::parse_content_range("bytes 10-19/100"),
            Some((10, 19, Some(100)))
        );
        assert_eq!(
            crate::client::parse_content_range(" bytes 90-99/100 "),
            Some((90, 99, Some(100)))
        );
        assert_eq!(
            crate::client::parse_content_range("bytes 0-0/*"),
            Some((0, 0, None))
        );
        // 416 形态 `bytes */100` 与垃圾输入不解析（None = mismatch 路径）。
        assert_eq!(crate::client::parse_content_range("bytes */100"), None);
        assert_eq!(crate::client::parse_content_range("garbage"), None);
    }

    #[test]
    fn client_unauthorized_messages_are_actionable() {
        // R3 双通道：`Unauthorized{false}` 无载荷——文案住 warn 通道，
        // 字面在此钉死（键名/方案名/出路必须齐全）。
        let missing = crate::client::message_missing_credentials();
        assert!(
            missing.contains("webdav_username") && missing.contains("webdav_password"),
            "{missing}"
        );
        let ntlm = crate::client::message_unsupported_scheme("ntlm");
        assert!(
            ntlm.contains("ntlm") && ntlm.contains("Digest"),
            "scheme named + a way out: {ntlm}"
        );
        let rejected = crate::client::message_credentials_rejected();
        assert!(
            rejected.contains("webdav_username") && rejected.contains("verify"),
            "{rejected}"
        );
        let basic = crate::client::message_basic_mode_refused(&[
            r#"Digest realm="webdavtest", nonce="abc+=""#.to_string(),
        ]);
        assert!(
            basic.contains("webdav_auth=basic") && basic.contains("digest"),
            "names the mode and the way out: {basic}"
        );
    }

    // ------------------------------------------------------ xml 面（WD2b）---

    #[test]
    fn xml_truncated_multistatus_is_an_error_not_a_silent_empty_set() {
        // 桩 malformed_multistatus 旋钮的形态：207 + 无闭合截断——纯解析
        // 不吞（quick-xml 在 Eof 温和收尾，截断检测在这里补位）。
        let truncated = concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/trunc"#
        );
        let error = crate::xml::parse_multistatus(truncated)
            .err()
            .expect("truncation must fail");
        assert!(error.contains("truncated"), "{error}");
        // 良构的非 multistatus 体维持「空集宽收」（WD1a 契约零漂移）。
        assert!(
            crate::xml::parse_multistatus("<html><body>404</body></html>")
                .expect("lenient")
                .is_empty()
        );
    }

    #[test]
    fn xml_quota_props_project_only_from_2xx_propstats() {
        // RFC 4331：2xx 块给出真值。
        let body = concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<D:multistatus xmlns:D="DAV:">"#,
            r#"<D:response><D:href>/</D:href>"#,
            r#"<D:propstat><D:prop><D:quota-used-bytes>4096</D:quota-used-bytes>"#,
            r#"<D:quota-available-bytes>1048576</D:quota-available-bytes></D:prop>"#,
            r#"<D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
            r#"</D:response></D:multistatus>"#,
        );
        let entries = crate::xml::entries(&crate::xml::parse_multistatus(body).expect("parses"));
        assert_eq!(entries[0].quota_used_bytes, Some(4096));
        assert_eq!(entries[0].quota_available_bytes, Some(1_048_576));

        // 真机/桩形态（矩阵⑪）：主体 props 在 200 块、quota props 在内层
        // 404 块——条目在（200 块产出），quota 字段 None（404 行不进投
        // 影）——驱动 quota 面的降级信号。
        let inner404 = concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<D:multistatus xmlns:D="DAV:">"#,
            r#"<D:response><D:href>/</D:href>"#,
            r#"<D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype>"#,
            r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
            r#"<D:propstat><D:prop><D:quota-used-bytes/><D:quota-available-bytes/></D:prop>"#,
            r#"<D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"#,
            r#"</D:response></D:multistatus>"#,
        );
        let entries =
            crate::xml::entries(&crate::xml::parse_multistatus(inner404).expect("parses"));
        assert_eq!(entries.len(), 1, "the 200 block still yields the entry");
        assert!(entries[0].is_collection);
        assert_eq!(entries[0].quota_used_bytes, None);
        assert_eq!(entries[0].quota_available_bytes, None);

        // 全 404 块（无任何 2xx 行）：不产出条目——「成员按成员映射」的
        // 缺席形态（调用方视作降级/NotFound 的判据）。
        let all404 = concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<D:multistatus xmlns:D="DAV:">"#,
            r#"<D:response><D:href>/</D:href>"#,
            r#"<D:propstat><D:prop><D:quota-used-bytes/></D:prop>"#,
            r#"<D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"#,
            r#"</D:response></D:multistatus>"#,
        );
        let entries = crate::xml::entries(&crate::xml::parse_multistatus(all404).expect("parses"));
        assert!(
            entries.is_empty(),
            "a member with only non-2xx blocks vanishes"
        );
    }

    // ----------------------------------------------------- driver 面 ---

    #[test]
    fn driver_staging_artifact_double_condition() {
        // §4.6/断言③：`.ckwd-` 且 .part/.old 结尾才是驱动残件（单条件
        // 误伤用户合法名）。
        assert!(crate::driver::is_staging_artifact(
            "report.bin.ckwd-4242-7.part"
        ));
        assert!(crate::driver::is_staging_artifact(
            "report.bin.ckwd-4242-7.old"
        ));
        assert!(!crate::driver::is_staging_artifact("notes.ckwd-diary.txt"));
        assert!(!crate::driver::is_staging_artifact("keep.part"));
        assert!(!crate::driver::is_staging_artifact("keep.old"));
    }

    #[test]
    fn driver_entry_projection_defaults_bad_mtime_to_zero() {
        // §4.5-5：坏 mtime → debug 留痕 + 0.0（绝不静默 epoch——「不静
        // 默」的一半是日志，另一半是这里钉住的可控行为）。桩无法注入
        // 坏 mtime（恒发 IMF-fixdate 真形），纯函数面钉死。
        let volume = VolumeId::new("webdav", "t").expect("volume");
        let rel = RelPath::new("f.txt").expect("rel");
        let pe = crate::xml::PropfindEntry {
            href: "/f.txt".to_string(),
            is_collection: false,
            content_length: Some(11),
            last_modified: Some("not a date".to_string()),
            quota_used_bytes: None,
            quota_available_bytes: None,
        };
        let entry = crate::driver::entry_from_propfind(&volume, &rel, &pe);
        assert_eq!(entry.size, 11);
        assert_eq!(entry.mtime, 0.0, "garbage mtime degrades to 0, not a lie");
        // 好形态照常回真值。
        let pe = crate::xml::PropfindEntry {
            last_modified: Some("Mon, 21 Sep 2026 10:51:05 GMT".to_string()),
            ..pe
        };
        let entry = crate::driver::entry_from_propfind(&volume, &rel, &pe);
        assert_eq!(entry.mtime, 1_789_987_865.0);
    }
}
