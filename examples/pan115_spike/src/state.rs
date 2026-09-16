//! Paths, persisted state (atomic writes), masking and QR PNG rendering.
//! Everything lives under the gitignored spike test dir — no secrets in repo.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_TEST_DIR: &str = r"E:\GitHub\rs-CyDrive\test";

/// Spike working dir: env `PAN115_SPIKE_TEST_DIR`, default the rs-CyDrive
/// test dir (exists, gitignored, outside this repo).
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let dir = std::env::var("PAN115_SPIKE_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_TEST_DIR));
        Paths { dir }
    }

    pub fn auth_state(&self) -> PathBuf {
        self.dir.join("pan115-auth-state.json")
    }
    pub fn tokens(&self) -> PathBuf {
        self.dir.join("pan115-tokens.json")
    }
    pub fn qr_png(&self) -> PathBuf {
        self.dir.join("pan115-qr.png")
    }
}

/// In-flight device-code PKCE state (auth-qr -> auth-poll handoff).
/// `code_verifier`/`sign` are auth secrets: never printed.
#[derive(Clone, Serialize, Deserialize)]
pub struct AuthState {
    pub client_id: String,
    pub code_verifier: String,
    pub uid: String,
    pub time: i64,
    pub sign: String,
    pub qrcode_url: String,
    pub created_unix: i64,
}

/// Persisted token pair. Tokens are secrets: printed masked only.
#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub obtained_unix: i64,
    /// Last confirmed user/info `data` object (no credentials inside).
    pub user_info: serde_json::Value,
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

/// Render `url` as a QR PNG of at least `min_px` per side (error-correction
/// level M; the crate renderer adds the spec 4-module quiet zone) and return
/// the actual side length in px.
pub fn render_qr_png(url: &str, min_px: u32, out: &Path) -> Result<u32> {
    let code = qrcode::QrCode::with_error_correction_level(url.as_bytes(), qrcode::EcLevel::M)
        .context("encode QR")?;
    let img = code
        .render::<image::Luma<u8>>()
        .min_dimensions(min_px, min_px)
        .build();
    let side = img.width();
    img.save(out)
        .with_context(|| format!("save QR png {}", out.display()))?;
    Ok(side)
}
