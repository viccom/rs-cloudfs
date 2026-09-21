//! Paths, persisted state (atomic writes), masking and small helpers.
//! Everything sensitive lives under the gitignored spike test dir — no
//! secrets ever enter the repo (pan115_spike style).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_TEST_DIR: &str = r"E:\GitHub\rs-CyDrive\test";

/// Spike working dir: env `PAN123_SPIKE_TEST_DIR`, default the rs-CyDrive
/// test dir (exists, gitignored, outside this repo).
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let dir = std::env::var("PAN123_SPIKE_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_TEST_DIR));
        Paths { dir }
    }

    pub fn account(&self) -> PathBuf {
        self.dir.join("pan123-test-account.json")
    }
    pub fn tokens(&self) -> PathBuf {
        self.dir.join("pan123-tokens.json")
    }
    pub fn payload(&self) -> PathBuf {
        self.dir.join("pan123-spike-file.bin")
    }
}

/// Test-account credentials — the password is a secret: never printed.
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub passport: String,
    pub password: String,
    #[serde(default)]
    pub note: String,
}

/// Persisted login token (web identity: one token serves both the
/// `Authorization: Bearer` header and the `Cookie: sso-token=` header).
/// `login_uuid` is kept stable across runs (pan123-rs md5(uuid) form).
#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub token: String,
    pub login_uuid: String,
    pub obtained_unix: i64,
    #[serde(default)]
    pub user_info: serde_json::Value,
}

pub fn load_account(paths: &Paths) -> Result<Account> {
    read_json(&paths.account())
}

pub fn load_tokens(paths: &Paths) -> Result<Tokens> {
    read_json(&paths.tokens())
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// tmp-file + rename write, so a crash never leaves a half-written file.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// `abcdef...wxyz` masked to first 6 + last 4 chars (baidu_spike style).
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

/// Random lowercase-hex string of `bytes` bytes (2 chars per byte).
pub fn rand_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
