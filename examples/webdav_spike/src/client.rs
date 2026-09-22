//! reqwest wrapper for the spike: `.no_proxy()` (the local http_proxy env
//! var would hijack the direct VM-IP connection), 15s connect / 60s total
//! timeouts, no redirect following (the apache 301-not-executed MOVE
//! contrast must be observed, not chased), and per-server auth:
//! rclone = Basic, apache = hand-rolled RFC 7616 Digest with automatic
//! 401 -> challenge -> re-negotiate -> retry-once.

use crate::digest::{self, DigestSession};
use anyhow::{anyhow, Context, Result};
use std::time::Duration;

/// Allprop PROPFIND body used for listing probes. (The XML declaration
/// must be exactly well-formed: apache's expat rejects a malformed decl
/// with 400 "XML declaration not well-formed", while rclone's parser
/// silently tolerates it — pinned live during WD0.)
pub const ALLPROP_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8" ?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#
);

/// Environment configuration (nothing is committed; missing vars fail
/// with an actionable message).
pub struct Config {
    pub rclone_url: String,
    pub apache_url: String,
    pub apache_stale_url: String,
    pub user: String,
    pub pass: String,
}

fn env_nonempty(key: &'static str, missing: &mut Vec<&'static str>) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => {
            missing.push(key);
            None
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let mut missing: Vec<&'static str> = Vec::new();
        let rclone_url = env_nonempty("WEBDAV_SPIKE_RCLONE_URL", &mut missing);
        let apache_url = env_nonempty("WEBDAV_SPIKE_APACHE_URL", &mut missing);
        let apache_stale_url = env_nonempty("WEBDAV_SPIKE_APACHE_STALE_URL", &mut missing);
        let user = env_nonempty("WEBDAV_SPIKE_USER", &mut missing);
        let pass = env_nonempty("WEBDAV_SPIKE_PASS", &mut missing);
        if !missing.is_empty() {
            return Err(anyhow!(
                "missing required env var(s): {}\n\
                 set them before running (one-off local fixture creds; the password is never printed):\n\
                 \x20 export WEBDAV_SPIKE_RCLONE_URL=http://<VM_IP>:8080/\n\
                 \x20 export WEBDAV_SPIKE_APACHE_URL=http://<VM_IP>:8081/dav/\n\
                 \x20 export WEBDAV_SPIKE_APACHE_STALE_URL=http://<VM_IP>:8081/dav-stale/\n\
                 \x20 export WEBDAV_SPIKE_USER=<fixture user>\n\
                 \x20 export WEBDAV_SPIKE_PASS=<fixture password>\n\
                 \x20 (VM_IP: wsl.exe -d Ubuntu-24.04 -- bash -c \"hostname -I\")",
                missing.join(", ")
            ));
        }
        Ok(Config {
            rclone_url: rclone_url.expect("checked non-empty above"),
            apache_url: apache_url.expect("checked non-empty above"),
            apache_stale_url: apache_stale_url.expect("checked non-empty above"),
            user: user.expect("checked non-empty above"),
            pass: pass.expect("checked non-empty above"),
        })
    }
}

/// Bare client with the spike transport policy (shared by the matrix
/// DavServer and the manual digest chain).
pub fn bare_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .context("build reqwest client")?)
}

/// Request body kind: bytes are replayable across the digest retry,
/// a stream is not (establish the digest session BEFORE streaming).
pub enum BodyKind {
    None,
    Bytes(Vec<u8>),
    Stream(reqwest::Body),
}

/// One DAV server fixture with its auth mode.
pub struct DavServer {
    pub name: String,
    pub basic: bool,
    base: reqwest::Url,
    user: String,
    pass: String,
    http: reqwest::Client,
    session: Option<DigestSession>,
}

/// The request-URI (path + query) exactly as sent on the wire — this is
/// what RFC 7616 HA2 must hash.
pub fn request_uri(url: &reqwest::Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    }
}

impl DavServer {
    pub fn new(name: &str, base_url: &str, user: &str, pass: &str, basic: bool) -> Result<Self> {
        let http = bare_client()?;
        let mut base =
            reqwest::Url::parse(base_url).with_context(|| format!("parse base url {base_url}"))?;
        // Normalize to a trailing slash so Url::join appends under it.
        if !base.as_str().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            name: name.to_string(),
            basic,
            base,
            user: user.to_string(),
            pass: pass.to_string(),
            http,
            session: None,
        })
    }

    /// Absolute URI for a repo-relative path (MOVE Destination header).
    pub fn url_abs(&self, path: &str) -> Result<String> {
        Ok(self
            .base
            .join(path)
            .with_context(|| format!("join {path} onto {}", self.base))?
            .to_string())
    }

    /// Repo-relative path from a multistatus href (percent-safe for the
    /// ASCII names this spike uses).
    pub fn path_from_href(&self, href: &str) -> Option<String> {
        let base_path = self.base.path().trim_end_matches('/');
        let rest = href.trim_end_matches('/').strip_prefix(base_path)?;
        Some(rest.trim_start_matches('/').to_string())
    }

    /// Send a request; attaches Basic auth, or preemptive Digest auth
    /// once a nonce is known, re-negotiating exactly once on a 401.
    pub async fn send(
        &mut self,
        method: reqwest::Method,
        path: &str,
        headers: &[(String, String)],
        mut body: BodyKind,
    ) -> Result<reqwest::Response> {
        let url = self
            .base
            .join(path)
            .with_context(|| format!("join {path} onto {}", self.base))?;
        let uri = request_uri(&url);
        let mut attempt = 0u32;
        loop {
            let mut rb = self.http.request(method.clone(), url.clone());
            for (k, v) in headers {
                rb = rb.header(k, v);
            }
            match body {
                BodyKind::None => {}
                BodyKind::Bytes(ref b) => {
                    rb = rb.body(b.clone());
                }
                BodyKind::Stream(_) if attempt > 0 => {
                    return Err(anyhow!(
                        "streaming body got 401 — cannot replay a stream; \
                         establish the digest session before streaming"
                    ));
                }
                BodyKind::Stream(_) => {
                    // Streams are consumed exactly once (attempt 0 only).
                    if let BodyKind::Stream(s) =
                        std::mem::replace(&mut body, BodyKind::None)
                    {
                        rb = rb.body(s);
                    }
                }
            }
            if self.basic {
                rb = rb.basic_auth(&self.user, Some(&self.pass));
            } else if let Some(sess) = self.session.as_mut() {
                sess.nc += 1;
                let authz = digest::authorization_header(
                    &self.user,
                    &self.pass,
                    method.as_str(),
                    &uri,
                    sess,
                    &digest::rand_cnonce(),
                );
                rb = rb.header("authorization", authz);
            }
            let resp = rb
                .send()
                .await
                .with_context(|| format!("{} {url}", method.as_str()))?;
            if resp.status().as_u16() != 401 || attempt >= 1 || self.basic {
                return Ok(resp);
            }
            // Digest (re)negotiation from the 401 challenge.
            let www = resp
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            match digest::parse_challenge(www) {
                Ok(ch) => {
                    self.session = Some(DigestSession::from_challenge(&ch));
                }
                Err(_) => return Ok(resp),
            }
            attempt += 1;
        }
    }

    pub async fn options(&mut self) -> Result<reqwest::Response> {
        self.send(reqwest::Method::OPTIONS, "", &[], BodyKind::None)
            .await
    }

    pub async fn propfind(
        &mut self,
        path: &str,
        depth: &str,
        body: &str,
    ) -> Result<reqwest::Response> {
        self.send(
            reqwest::Method::from_bytes(b"PROPFIND")?,
            path,
            &[
                ("depth".into(), depth.into()),
                ("content-type".into(), "text/xml; charset=utf-8".into()),
            ],
            BodyKind::Bytes(body.as_bytes().to_vec()),
        )
        .await
    }

    pub async fn proppatch(&mut self, path: &str, body: &str) -> Result<reqwest::Response> {
        self.send(
            reqwest::Method::from_bytes(b"PROPPATCH")?,
            path,
            &[("content-type".into(), "text/xml; charset=utf-8".into())],
            BodyKind::Bytes(body.as_bytes().to_vec()),
        )
        .await
    }

    pub async fn put_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<reqwest::Response> {
        self.send(
            reqwest::Method::PUT,
            path,
            &[],
            BodyKind::Bytes(bytes.to_vec()),
        )
        .await
    }

    pub async fn put_bytes_headers(
        &mut self,
        path: &str,
        bytes: &[u8],
        extra: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let headers: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        self.send(reqwest::Method::PUT, path, &headers, BodyKind::Bytes(bytes.to_vec()))
            .await
    }

    pub async fn put_stream(
        &mut self,
        path: &str,
        body: reqwest::Body,
    ) -> Result<reqwest::Response> {
        self.send(reqwest::Method::PUT, path, &[], BodyKind::Stream(body))
            .await
    }

    pub async fn mkcol(&mut self, path: &str) -> Result<reqwest::Response> {
        self.send(
            reqwest::Method::from_bytes(b"MKCOL")?,
            path,
            &[],
            BodyKind::None,
        )
        .await
    }

    pub async fn delete(&mut self, path: &str) -> Result<reqwest::Response> {
        self.send(reqwest::Method::DELETE, path, &[], BodyKind::None)
            .await
    }

    pub async fn move_(
        &mut self,
        path: &str,
        dest_uri: &str,
        overwrite: Option<bool>,
    ) -> Result<reqwest::Response> {
        let mut headers: Vec<(String, String)> =
            vec![("destination".into(), dest_uri.into())];
        if let Some(ow) = overwrite {
            headers.push(("overwrite".into(), if ow { "T".into() } else { "F".into() }));
        }
        self.send(
            reqwest::Method::from_bytes(b"MOVE")?,
            path,
            &headers,
            BodyKind::None,
        )
        .await
    }

    pub async fn get(&mut self, path: &str) -> Result<reqwest::Response> {
        self.send(reqwest::Method::GET, path, &[], BodyKind::None)
            .await
    }

    pub async fn get_range(&mut self, path: &str, range: &str) -> Result<reqwest::Response> {
        self.send(
            reqwest::Method::GET,
            path,
            &[("range".into(), range.into())],
            BodyKind::None,
        )
        .await
    }
}
