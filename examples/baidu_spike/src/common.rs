//! Shared config/secrets/clients for the Baidu spike.
//!
//! Credential rules (Batch S): values never enter the repo; everything is
//! read at runtime and every printed string goes through [`scrub`].

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// UA used for every Baidu call (PCS endpoints are known to behave
/// differently without a `netdisk`-family UA; recorded in the report).
pub const UA: &str = "netdisk;P2SP;2.2.91.136;android-android";

const DEFAULT_INSTANCE: &str = r"E:\GitHub\rs-CyDrive\test\instances\baidu1.json";
const DEFAULT_PCFS_GO: &str = r"E:\Go_codes\PrivateCloudFS\drivers\baidu\client.go";

// ---------------------------------------------------------------------------
// secret registry + scrubbing
// ---------------------------------------------------------------------------

static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Register live secret values (tokens, appkey, secret) for [`scrub`].
/// May be called repeatedly (e.g. again after a token refresh).
pub fn register_secrets(values: Vec<String>) {
    if let Ok(mut v) = SECRETS.lock() {
        v.extend(values.into_iter().filter(|s| !s.is_empty()));
    }
}

/// `abcdef...wxyz` -> `abcdef...wxyz` masked to first 6 + last 4 chars.
pub fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n >= 12 {
        let head: String = s.chars().take(6).collect();
        let tail: String = s.chars().skip(n - 4).collect();
        format!("{head}...{tail}")
    } else {
        "***".to_string()
    }
}

/// Replace every registered secret occurrence with its masked form.
/// Apply at every print/error boundary; cheap enough for spike output.
pub fn scrub(s: &str) -> String {
    let mut out = s.to_string();
    if let Ok(secrets) = SECRETS.lock() {
        for t in secrets.iter() {
            if !t.is_empty() && out.contains(t.as_str()) {
                out = out.replace(t.as_str(), &mask(t));
            }
        }
    }
    out
}

/// Structured summary line (greppable evidence for the report).
pub fn summ(line: impl AsRef<str>) {
    println!("[SUMMARY] {}", scrub(line.as_ref()));
}

// ---------------------------------------------------------------------------
// config + secrets loading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Cfg {
    pub instance_path: PathBuf,
    pub pcfs_go_path: PathBuf,
    pub tmp_dir: PathBuf,
}

impl Cfg {
    pub fn from_env() -> Self {
        let tmp_dir = std::env::var("BAIDU_SPIKE_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("baidu_spike"));
        Cfg {
            instance_path: std::env::var("BAIDU_SPIKE_INSTANCE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(DEFAULT_INSTANCE)),
            pcfs_go_path: std::env::var("BAIDU_SPIKE_PCFS_GO")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(DEFAULT_PCFS_GO)),
            tmp_dir,
        }
    }

    pub fn token_path(&self) -> PathBuf {
        self.tmp_dir.join("token.json")
    }
    pub fn state_path(&self) -> PathBuf {
        self.tmp_dir.join("state.json")
    }
    pub fn resume_state_path(&self) -> PathBuf {
        self.tmp_dir.join("resume_state.json")
    }
}

/// App credentials + refresh token assembled at runtime (env > file parse).
#[derive(Debug, Clone)]
pub struct Secrets {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    /// Which sources were actually used (for the report; no values).
    pub provenance: String,
}

/// Extract the first `"..."` literal following `key` in a Go source string.
fn go_string_literal(src: &str, key: &str) -> Option<String> {
    let i = src.find(key)?;
    let rest = &src[i + key.len()..];
    let a = rest.find('"')?;
    let b = rest[a + 1..].find('"')?;
    Some(rest[a + 1..a + 1 + b].to_string())
}

pub fn load_secrets(cfg: &Cfg) -> Result<Secrets> {
    // instance JSON: { config: { access_token, refresh_token, ... } }
    let raw = std::fs::read_to_string(&cfg.instance_path)
        .with_context(|| format!("read instance json {}", cfg.instance_path.display()))?;
    let inst: serde_json::Value = serde_json::from_str(&raw).context("parse instance json")?;
    let conf = inst
        .get("config")
        .ok_or_else(|| anyhow!("instance json has no config object"))?;
    let refresh_token = conf
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("instance json config.refresh_token missing"))?
        .to_string();

    // appkey/secret: env override, else parse the PrivateCloudFS sources.
    let env_id = std::env::var("BAIDU_SPIKE_CLIENT_ID").ok();
    let env_secret = std::env::var("BAIDU_SPIKE_CLIENT_SECRET").ok();
    let (client_id, client_secret, provenance) =
        if let (Some(id), Some(sec)) = (&env_id, &env_secret) {
            (
                id.clone(),
                sec.clone(),
                "client_id/secret from BAIDU_SPIKE_CLIENT_ID/SECRET env".to_string(),
            )
        } else {
            let go = std::fs::read_to_string(&cfg.pcfs_go_path)
                .with_context(|| format!("read pcfs source {}", cfg.pcfs_go_path.display()))?;
            let id = go_string_literal(&go, "clientID:")
                .ok_or_else(|| anyhow!("clientID literal not found in pcfs source"))?;
            let sec = go_string_literal(&go, "clientSecret:")
                .ok_or_else(|| anyhow!("clientSecret literal not found in pcfs source"))?;
            (
                id,
                sec,
                format!(
                    "client_id/secret parsed at runtime from {} (values never committed)",
                    cfg.pcfs_go_path.display()
                ),
            )
        };

    let instance_file = cfg
        .instance_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let provenance = format!(
        "{provenance}; refresh_token from {instance_file} (filename only, value never printed)"
    );

    Ok(Secrets {
        client_id,
        client_secret,
        refresh_token,
        provenance,
    })
}

// ---------------------------------------------------------------------------
// token cache (OS temp dir only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    pub obtained_at_unix: i64,
    pub expires_in: i64,
}

pub fn save_token(cfg: &Cfg, t: &Token) -> Result<()> {
    std::fs::create_dir_all(&cfg.tmp_dir).ok();
    let path = cfg.token_path();
    std::fs::write(&path, serde_json::to_vec_pretty(t)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn load_token(cfg: &Cfg) -> Result<Token> {
    let path = cfg.token_path();
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no cached token at {} (run `refresh` first)",
            path.display()
        )
    })?;
    Ok(serde_json::from_str(&raw)?)
}

// ---------------------------------------------------------------------------
// remote-dir state (resolved once, shared by every subcommand)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub remote_dir: Option<String>,
    pub remote_dir_note: Option<String>,
}

pub fn save_state(cfg: &Cfg, s: &State) -> Result<()> {
    std::fs::create_dir_all(&cfg.tmp_dir).ok();
    std::fs::write(cfg.state_path(), serde_json::to_vec_pretty(s)?)?;
    Ok(())
}

pub fn load_state(cfg: &Cfg) -> State {
    std::fs::read_to_string(cfg.state_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// HTTP clients: DIRECT (no proxy) + IPv4-only, per PCFS intel
// ---------------------------------------------------------------------------

fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy() // Baidu endpoints must bypass the local proxy
        .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)) // force IPv4 dial
        .user_agent(UA)
}

/// API client: pan.baidu.com / openapi — no redirects (download 302 must be
/// observed, not followed), 60s per-request timeout.
pub fn api_client() -> Result<reqwest::Client> {
    base_builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .context("build api client")
}

/// Streaming client: CDN uploads/downloads, no total timeout.
pub fn stream_client() -> Result<reqwest::Client> {
    base_builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .build()
        .context("build stream client")
}

// ---------------------------------------------------------------------------
// context handed to every subcommand
// ---------------------------------------------------------------------------

pub struct Ctx {
    pub cfg: Cfg,
    pub secrets: Secrets,
    pub token: tokio::sync::RwLock<Token>,
    pub api: reqwest::Client,
    pub stream: reqwest::Client,
    pub state: std::sync::Mutex<State>,
}

impl Ctx {
    /// Load secrets + cached token and register secrets for scrubbing.
    pub fn init() -> Result<Self> {
        let cfg = Cfg::from_env();
        let secrets = load_secrets(&cfg)?;
        register_secrets(vec![
            secrets.client_id.clone(),
            secrets.client_secret.clone(),
            secrets.refresh_token.clone(),
        ]);
        let token = load_token(&cfg)?;
        register_secrets(vec![
            token.access_token.clone(),
            token.refresh_token.clone(),
        ]);
        let state = load_state(&cfg);
        Ok(Ctx {
            cfg,
            secrets,
            token: tokio::sync::RwLock::new(token),
            api: api_client()?,
            stream: stream_client()?,
            state: std::sync::Mutex::new(state),
        })
    }

    #[allow(dead_code)]
    pub fn remote_dir(&self) -> Result<String> {
        self.state
            .lock()
            .unwrap()
            .remote_dir
            .clone()
            .ok_or_else(|| anyhow!("remote_dir not resolved yet (run a remote subcommand)"))
    }

    #[allow(dead_code)]
    pub fn note(&self, line: impl AsRef<str>) {
        println!("[note] {}", scrub(line.as_ref()));
    }
}

/// Error helper for transport errors that may embed URLs with tokens.
pub fn xe(stage: &str, e: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(scrub(&format!("{stage}: {e}")))
}

pub fn ensure_not_token_error(errno: i64) -> Result<()> {
    match errno {
        111 => bail!(
            "refresh token expired (errno 111): re-authorization needed — \
             stop and report; do NOT retry-loop (§7a)"
        ),
        -6 => bail!(
            "authentication failed (errno -6): token/appkey mismatch — \
             stop and report (§7a)"
        ),
        _ => Ok(()),
    }
}
