//! RED-phase tests for the `proxy_url` extension of `cydrive_core::config`.
//!
//! Contract under test: `CyDriveConfig` gains an optional SOCKS5 proxy URL
//! (`None` by default, backward-compatible — an existing `config.toml`
//! without the key loads unchanged). Precedence follows the established
//! chain **env > file > defaults**: `CYDRIVE_PROXY_URL` overrides the file
//! value, an empty env value explicitly clears it, and `save_toml`
//! round-trips the field like every other one.

use std::fs;
use std::sync::{Mutex, MutexGuard};

use cydrive_core::config::CyDriveConfig;

// ------------------------------------------------------------- helpers ---

/// Default config with `bot_token`/`chat_id` replaced — the minimal
/// "configured" shape (mirrors the helper in `tests/config.rs`).
fn default_with(bot_token: &str, chat_id: i64) -> CyDriveConfig {
    CyDriveConfig {
        bot_token: bot_token.to_string(),
        chat_id,
        ..CyDriveConfig::default()
    }
}

/// Every environment key [`CyDriveConfig::with_env_overrides`] recognises
/// (the full set, so the guard also isolates against ambient env state).
const ENV_KEYS: &[&str] = &[
    "CYDRIVE_BOT_TOKEN",
    "CYDRIVE_CHAT_ID",
    "CYDRIVE_WEBDAV_PORT",
    "CYDRIVE_WEB_UI_PORT",
    "CYDRIVE_DRIVE_LETTER",
    "CYDRIVE_CHUNK_SIZE_MB",
    "CYDRIVE_ENABLE_ENCRYPTION",
    "CYDRIVE_PROXY_URL",
];

/// Serialises every test that touches process-global environment state
/// (tests in one binary share one process; `set_var`/`remove_var` race) —
/// same precedent as `tests/config.rs`.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Holds [`ENV_MUTEX`] and clears all override keys on drop — including on
/// panic — so no env state leaks between tests.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
    }
}

/// Locks [`ENV_MUTEX`] and starts from a clean env (all override keys unset).
fn env_guard() -> EnvGuard {
    let _lock = ENV_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for key in ENV_KEYS {
        std::env::remove_var(key);
    }
    EnvGuard { _lock }
}

// -------------------------------------------------------------- tests ---

#[test]
fn default_has_no_proxy() {
    let cfg = CyDriveConfig::default();
    assert_eq!(cfg.proxy_url, None, "default config must have no proxy");
}

#[test]
fn load_toml_reads_proxy_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        concat!(
            "bot_token = \"123456:ABC-DEF\"\n",
            "chat_id = 123456789\n",
            "proxy_url = \"socks5://127.0.0.1:7897\"\n",
        ),
    )
    .expect("write config.toml");

    let cfg = CyDriveConfig::load_toml(&path).expect("config with proxy_url loads");
    assert_eq!(
        cfg.proxy_url,
        Some("socks5://127.0.0.1:7897".to_string()),
        "load_toml must pick up the proxy_url key"
    );
}

#[test]
fn env_proxy_url_overrides_file_and_empty_clears_it() {
    let _env = env_guard();

    // env > file: a set variable wins over the value from the config file.
    std::env::set_var("CYDRIVE_PROXY_URL", "socks5://127.0.0.1:7897");
    let cfg = default_with("1:a", 1).with_env_overrides();
    assert_eq!(
        cfg.proxy_url,
        Some("socks5://127.0.0.1:7897".to_string()),
        "CYDRIVE_PROXY_URL must override the file value"
    );

    // An empty env value explicitly clears the proxy (back to None).
    std::env::set_var("CYDRIVE_PROXY_URL", "");
    let cfg = default_with("1:a", 1).with_env_overrides();
    assert_eq!(
        cfg.proxy_url, None,
        "empty CYDRIVE_PROXY_URL must clear the file value"
    );
}

#[test]
fn toml_roundtrip_preserves_proxy_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");

    let cfg = CyDriveConfig {
        proxy_url: Some("socks5://127.0.0.1:7897".to_string()),
        ..default_with("10:AA", 10)
    };

    cfg.save_toml(&path).expect("save_toml");
    let loaded = CyDriveConfig::load_toml(&path).expect("load_toml");
    assert_eq!(loaded, cfg, "proxy_url must survive the TOML round-trip");
}
