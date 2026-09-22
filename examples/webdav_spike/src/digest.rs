//! Hand-rolled RFC 7616 Digest auth (reqwest has no built-in Digest).
//! Quote-aware challenge parser (commas inside quoted values must not
//! split params), MD5 `response` computation with qop=auth, and the
//! Authorization header builder. The password never leaves this module
//! except hashed into HA1.

use anyhow::{anyhow, Result};
use md5::{Digest, Md5};
use rand::RngCore;

/// A parsed `WWW-Authenticate: Digest ...` challenge.
#[derive(Debug, Clone)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub algorithm: Option<String>,
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub stale: bool,
}

/// Client-side state for one negotiated nonce (nc counts requests signed
/// with this nonce; the caller increments before each header build).
#[derive(Debug, Clone)]
pub struct DigestSession {
    pub realm: String,
    pub nonce: String,
    pub algorithm: Option<String>,
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub nc: u64,
}

impl DigestSession {
    pub fn from_challenge(ch: &Challenge) -> Self {
        Self {
            realm: ch.realm.clone(),
            nonce: ch.nonce.clone(),
            algorithm: ch.algorithm.clone(),
            qop: ch.qop.clone(),
            opaque: ch.opaque.clone(),
            nc: 0,
        }
    }
}

/// Split challenge params on commas that are NOT inside double quotes
/// (a naive split would break on `qop="auth,auth-int"`).
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        if in_quotes && ch == '\\' {
            cur.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            cur.push(ch);
            continue;
        }
        if ch == ',' && !in_quotes {
            out.push(cur.trim().to_string());
            cur.clear();
            continue;
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Parse a `WWW-Authenticate` header value. Only the Digest scheme is
/// supported (that is what the fixture speaks).
pub fn parse_challenge(header: &str) -> Result<Challenge> {
    let header = header.trim();
    let (scheme, rest) = header
        .split_once(char::is_whitespace)
        .unwrap_or((header, ""));
    if !scheme.eq_ignore_ascii_case("digest") {
        return Err(anyhow!("not a Digest challenge: {header}"));
    }
    let mut ch = Challenge {
        realm: String::new(),
        nonce: String::new(),
        algorithm: None,
        qop: None,
        opaque: None,
        stale: false,
    };
    for param in split_params(rest) {
        let Some((key, val)) = param.split_once('=') else {
            continue;
        };
        let val = unquote(val);
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => ch.realm = val,
            "nonce" => ch.nonce = val,
            "algorithm" => ch.algorithm = Some(val),
            "qop" => ch.qop = Some(val),
            "opaque" => ch.opaque = Some(val),
            "stale" => ch.stale = val.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    if ch.realm.is_empty() || ch.nonce.is_empty() {
        return Err(anyhow!("challenge missing realm/nonce: {header}"));
    }
    if let Some(alg) = &ch.algorithm {
        if !alg.eq_ignore_ascii_case("md5") {
            return Err(anyhow!("unsupported digest algorithm {alg} (only MD5)"));
        }
    }
    Ok(ch)
}

fn md5_hex(s: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(s.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// RFC 7616 MD5 `response` (qop=auth when qop is offered).
pub fn response_md5(
    user: &str,
    pass: &str,
    realm: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    nc: u32,
    cnonce: &str,
    qop: Option<&str>,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    match qop {
        Some(q) => md5_hex(&format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:{q}:{ha2}")),
        None => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

/// Random 16-hex-char cnonce.
pub fn rand_cnonce() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the `Authorization` header from a session (uses `sess.nonce`
/// and signs with nc = `sess.nc`). If the challenge offered a qop list,
/// the first token is used.
pub fn authorization_header(
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    sess: &DigestSession,
    cnonce: &str,
) -> String {
    let qop = sess
        .qop
        .as_deref()
        .map(|q| q.split(',').next().unwrap_or(q).trim().to_string());
    let response = response_md5(
        user,
        pass,
        &sess.realm,
        method,
        uri,
        &sess.nonce,
        sess.nc as u32,
        cnonce,
        qop.as_deref(),
    );
    let mut h = format!(
        "Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
        sess.realm, sess.nonce
    );
    if let Some(q) = &qop {
        h.push_str(&format!(", qop={q}"));
        h.push_str(&format!(", nc={:08x}", sess.nc));
        h.push_str(&format!(", cnonce=\"{cnonce}\""));
    }
    if let Some(alg) = &sess.algorithm {
        h.push_str(&format!(", algorithm={alg}"));
    }
    if let Some(opaque) = &sess.opaque {
        h.push_str(&format!(", opaque=\"{opaque}\""));
    }
    h
}
