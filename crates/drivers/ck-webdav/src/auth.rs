//! RFC 7616 Digest 认证（Phase 7 / WD1a；自 WD0 spike `digest.rs`
//! 移植——引号感知 + 顺序无关设计保留，错误形态改 `String` 人话文案）。
//!
//! reqwest 无内建 Digest；Basic 头经 `RequestBuilder::basic_auth`
//! （WD2 接线）。密码不出本模块（只哈希进 HA1）；challenge/会话值
//! 是服务器签发物，日志安全。
//!
//! ## D1 协商语义（计划 §8-D1；完整接线 WD2）
//!
//! `auto` = **Basic 预发** → 401 带 Digest challenge → [`parse_challenge`]
//! → [`DigestSession`] → 重放恰一次；会话中 `stale=true`（nonce 过期）
//! → 换新 nonce 重算**恰一次**（附录 C ⑨：apache nonce 过期 401+
//! stale=true → 新 nonce 一次恢复——无 OpenList 的 12h cron 重建客户
//! 端兜底形态）。NTLM/Negotiate challenge 明确拒绝（可行动文案）。
//! nc 单调自守（客户端正确性——附录 C ⑨：apache 不查 nc 重放，纪律
//! 只在客户端）。
//!
//! 修订注记：算法只支持 MD5（fixture 实证面；SHA-256 挑战拒收 +
//! 文案），`MD5-sess` 同拒。

use md5::{Digest, Md5};
use rand::Rng;

/// 一条解析后的 `WWW-Authenticate: Digest ...` challenge。
///
/// 参数顺序无关（附录 C ⑨：stale=true 实测出现在 algorithm 之后）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // WD1a 骨架：WD2 的 401 重放路径构造
pub(crate) struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub algorithm: Option<String>,
    /// 原样保留服务器给的列表形态（如 `"auth,auth-int"`）；取首 token
    /// 消费在 [`authorization_header`]。
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub stale: bool,
}

/// 一个已协商 nonce 的客户端侧会话状态（nc = 用该 nonce 签名的请求
/// 计数——单调递增，调用方在每次构造头前递增）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DigestSession {
    pub realm: String,
    pub nonce: String,
    pub algorithm: Option<String>,
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub nc: u64,
}

impl DigestSession {
    /// 从 challenge 建会话（nc 归零起步）。
    #[allow(dead_code)] // WD1a 骨架：WD2 协商接线
    pub(crate) fn from_challenge(challenge: &Challenge) -> Self {
        DigestSession {
            realm: challenge.realm.clone(),
            nonce: challenge.nonce.clone(),
            algorithm: challenge.algorithm.clone(),
            qop: challenge.qop.clone(),
            opaque: challenge.opaque.clone(),
            nc: 0,
        }
    }
}

/// 驱动侧认证协商状态（D1 语义见模块文档；WD2 在 client.rs 的 401
/// 重放路径里驱动这个状态机）。
#[derive(Debug, Clone)]
#[allow(dead_code)] // WD1a 骨架：BasicReady/Digest 臂在 WD2 协商接线构造
pub(crate) enum AuthState {
    /// 未协商（首请求形态）。
    None,
    /// Basic 就绪（预发或显式 basic 形态）。
    BasicReady,
    /// Digest 会话已协商（nc 随每次签名递增）。
    Digest(DigestSession),
}

/// 按不在双引号内的逗号切分 challenge 参数（引号内逗号不分裂——
/// `qop="auth,auth-int"` 的天真 split 会碎成 `qop="auth` + `auth-int"`；
/// 反斜杠转义感知）。
// WD1a 骨架：经 parse_challenge 仅供测试直达——WD2 全链路接线
#[allow(dead_code)]
fn split_params(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if in_quotes && ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch == ',' && !in_quotes {
            out.push(current.trim().to_string());
            current.clear();
            continue;
        }
        current.push(ch);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// 去包裹引号（`"value"` → `value`；无引号原样）。
// WD1a 骨架：同上（split_params 的配套）
#[allow(dead_code)]
fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// 解析 `WWW-Authenticate` 头值：只认 Digest scheme（`Basic` 头不是
/// challenge；NTLM/Negotiate 拒收给文案）。
///
/// - realm/nonce 缺一不可；
/// - algorithm 非 MD5（含 SHA-256、MD5-sess）→ 拒（只支持 MD5）。
#[allow(dead_code)] // WD1a 骨架：WD2 的 401 处理消费
pub(crate) fn parse_challenge(header: &str) -> Result<Challenge, String> {
    let header = header.trim();
    let (scheme, rest) = header
        .split_once(char::is_whitespace)
        .unwrap_or((header, ""));
    if !scheme.eq_ignore_ascii_case("digest") {
        return Err(format!(
            "not a Digest challenge (only Digest is supported; NTLM/Negotiate are rejected): \
             {header}"
        ));
    }
    let mut challenge = Challenge {
        realm: String::new(),
        nonce: String::new(),
        algorithm: None,
        qop: None,
        opaque: None,
        stale: false,
    };
    for param in split_params(rest) {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        let value = unquote(value);
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => challenge.realm = value,
            "nonce" => challenge.nonce = value,
            "algorithm" => challenge.algorithm = Some(value),
            "qop" => challenge.qop = Some(value),
            "opaque" => challenge.opaque = Some(value),
            "stale" => challenge.stale = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    if challenge.realm.is_empty() || challenge.nonce.is_empty() {
        return Err(format!(
            "Digest challenge is missing realm/nonce (both are required): {header}"
        ));
    }
    if let Some(algorithm) = &challenge.algorithm {
        if !algorithm.eq_ignore_ascii_case("md5") {
            return Err(format!(
                "unsupported Digest algorithm {algorithm:?} (only MD5 is supported; SHA-256 \
                 and MD5-sess are rejected)"
            ));
        }
    }
    Ok(challenge)
}

// WD1a 骨架：经 response_md5 仅供测试直达——WD2 接线
#[allow(dead_code)]
fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// RFC 7616 MD5 `response`（qop 提供时走 qop=auth 形态，否则 RFC 2069
/// 兼容形态）。
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // WD1a 骨架：WD2 的 Authorization 构造消费
pub(crate) fn response_md5(
    user: &str,
    pass: &str,
    realm: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    nc: u64,
    cnonce: &str,
    qop: Option<&str>,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    match qop {
        Some(qop) => md5_hex(&format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:{qop}:{ha2}")),
        None => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

/// 随机 16 hex 字符 cnonce。
#[allow(dead_code)] // WD1a 骨架：WD2 的签名路径消费
pub(crate) fn rand_cnonce() -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 从会话构造 `Authorization` 头值（以 `session.nc` 签名——调用方先
/// 递增；challenge 给了 qop 列表时取首 token）。
#[allow(dead_code)] // WD1a 骨架：WD2 的签名路径消费
pub(crate) fn authorization_header(
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    session: &DigestSession,
    cnonce: &str,
) -> String {
    let qop = session
        .qop
        .as_deref()
        .map(|list| list.split(',').next().unwrap_or(list).trim().to_string());
    let response = response_md5(
        user,
        pass,
        &session.realm,
        method,
        uri,
        &session.nonce,
        session.nc,
        cnonce,
        qop.as_deref(),
    );
    let mut header = format!(
        "Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", \
         response=\"{response}\"",
        session.realm, session.nonce
    );
    if let Some(qop) = &qop {
        header.push_str(&format!(", qop={qop}"));
        header.push_str(&format!(", nc={:08x}", session.nc));
        header.push_str(&format!(", cnonce=\"{cnonce}\""));
    }
    if let Some(algorithm) = &session.algorithm {
        header.push_str(&format!(", algorithm={algorithm}"));
    }
    if let Some(opaque) = &session.opaque {
        header.push_str(&format!(", opaque=\"{opaque}\""));
    }
    header
}
