//! The `setup` subcommand (M5-2): the first-time interactive wizard,
//! ported from Python `cydrive/config.py:85-135` as a pure core plus a
//! thin dialoguer layer.
//!
//! Pure core (fully tested offline):
//!
//! * [`validate_token`] — the Python wizard's two token complaints,
//!   verbatim texts (empty / no `':'`);
//! * [`apply_wizard`] — fill `bot_token`/`chat_id`/`drive_letter`
//!   (normalised like the Python wizard: trim, uppercase, ensure the
//!   trailing colon) onto a config, leaving every other field as passed
//!   in (callers start from [`CyDriveConfig::default`], mirroring the
//!   Python wizard always building a fresh dataclass);
//! * [`persist_setup`] — `save_toml_scrubbed` + the token into the
//!   [`CredentialStore`] (there is deliberately **no** wizard entry for
//!   an encryption password — the Python wizard never asked either, and
//!   the flag stays default-off; the branch below only exists so a
//!   caller-supplied already-encrypted config cannot lose its secret to
//!   the scrubbed write).
//!
//! Interactive layer ([`run_setup_interactive`]): dialoguer prompts with
//! built-in validation loops (a failed validation prints in red and
//! re-asks — the Rust stand-in for the Python `while` loops). Divergence
//! from Python, frozen by the M5-2 spec: hosts are never asked (they
//! stay at their defaults — Python's non-Windows VPS question is gone)
//! and a token without `':'` is re-asked outright instead of offering
//! Python's keep-anyway confirmation. Compile-verified only; driving a
//! TTY prompt from tests is not an offline concern.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::credentials::{CredentialStore, BOT_TOKEN, ENCRYPTION_PASSWORD};

/// The wizard's collected answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WizardAnswers {
    /// BotFather token, `"<id>:<secret>"`.
    pub bot_token: String,
    /// Telegram user/chat ID.
    pub chat_id: i64,
    /// Windows drive letter in any of the shapes the wizard accepts
    /// (`"y"`, `"y:"`, `" Y: "` — [`apply_wizard`] normalises).
    pub drive_letter: String,
}

/// Token validation mirroring the Python wizard's complaints: empty →
/// "Token cannot be empty"; no `':'` → the format warning text. Whitespace
/// is trimmed first, exactly like Python's `.strip()`.
pub fn validate_token(token: &str) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() {
        Err("Token cannot be empty".to_string())
    } else if !token.contains(':') {
        Err("Bot tokens usually follow format '123456789:ABCdef...'".to_string())
    } else {
        Ok(())
    }
}

/// Normalises a drive letter the Python wizard's way: trim, uppercase,
/// ensure the trailing colon (an empty input stays empty — the
/// interactive layer always supplies a default, so the pure function
/// just stays faithful).
fn normalize_wizard_letter(input: &str) -> String {
    let trimmed = input.trim().to_uppercase();
    if trimmed.is_empty() || trimmed.ends_with(':') {
        trimmed
    } else {
        format!("{trimmed}:")
    }
}

/// The baidu wizard branch's collected answers (Phase 2 / B3b dispatch
/// unit): the three pasted credentials. The access token is NOT asked
/// for — the branch refreshes with the pasted refresh_token and stores
/// the verified pair ([`apply_baidu_wizard`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaiduWizardAnswers {
    /// Baidu app key (K14; no code default exists).
    pub app_key: String,
    /// Baidu app secret (K14).
    pub app_secret: String,
    /// A live refresh token (paste from the authorization flow).
    pub refresh_token: String,
}

/// Validates the baidu answers: every credential non-empty after
/// trimming (the complaint names the offending field, the token-shape
/// loop's baidu counterpart).
pub fn validate_baidu_credentials(answers: &BaiduWizardAnswers) -> Result<(), String> {
    for (name, value) in [
        ("app_key", &answers.app_key),
        ("app_secret", &answers.app_secret),
        ("refresh_token", &answers.refresh_token),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{name} cannot be empty"));
        }
    }
    Ok(())
}

/// Applies the baidu wizard's outcome onto `cfg` (pure): sets the four
/// K14 keys and flips `backend = "baidu"`.
///
/// **Strict-validate ruling (B3b 段二b)**: call this ONLY with a
/// refresh-VERIFIED pair (`ck_baidu::refresh_tokens` succeeded) — the
/// backend key flips LAST, when all four keys are in hand, so no
/// half-configured baidu instance can be written and a later boot
/// fails in validate with a naming-what's-missing message instead of
/// deep inside the connect. (The alternative — writing
/// `backend = "baidu"` first and back-filling tokens — was rejected:
/// validate's four-key rule would refuse the half-written config with
/// a MORE actionable message than a mid-connect Invalid, so strictness
/// costs nothing and prevents the foot-gun.)
pub fn apply_baidu_wizard(
    mut cfg: CyDriveConfig,
    answers: &BaiduWizardAnswers,
    access_token: &str,
    refresh_token: &str,
) -> CyDriveConfig {
    cfg.backend = cloudkit_core::config::Backend::Baidu;
    cfg.baidu_app_key = Some(answers.app_key.trim().to_string());
    cfg.baidu_app_secret = Some(answers.app_secret.trim().to_string());
    cfg.baidu_access_token = Some(access_token.to_string());
    cfg.baidu_refresh_token = Some(refresh_token.to_string());
    cfg
}

/// The local wizard branch's collected answers: the drive's root
/// directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalWizardAnswers {
    /// The absolute path serving as the drive root (K6: the local
    /// backend's identity IS its root).
    pub local_root: String,
}

/// Validates a local root: non-empty after trimming and absolute
/// (validate's rule surfaced at prompt time instead of boot time —
/// both Windows `C:\...` and Unix `/...` shapes pass).
pub fn validate_local_root(root: &str) -> Result<(), String> {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return Err("local_root cannot be empty".to_string());
    }
    if !std::path::Path::new(trimmed).is_absolute() {
        return Err(format!(
            "local_root must be an absolute path (e.g. \"C:\\data\\cloudfs\" or \
             \"/srv/cloudfs\"), got {trimmed:?}"
        ));
    }
    Ok(())
}

/// Applies the local wizard's outcome onto `cfg` (pure): sets
/// `local_root` and flips `backend = "local"` (no secrets involved).
pub fn apply_local_wizard(mut cfg: CyDriveConfig, answers: &LocalWizardAnswers) -> CyDriveConfig {
    cfg.backend = cloudkit_core::config::Backend::Local;
    cfg.local_root = Some(answers.local_root.trim().to_string());
    cfg
}

/// Applies the answers onto `cfg`: fills the three wizard fields (the
/// letter normalised) and returns the config; nothing is written to
/// disk and every other field passes through untouched.
pub fn apply_wizard(mut cfg: CyDriveConfig, answers: &WizardAnswers) -> CyDriveConfig {
    cfg.bot_token = answers.bot_token.trim().to_string();
    cfg.chat_id = answers.chat_id;
    cfg.drive_letter = normalize_wizard_letter(&answers.drive_letter);
    cfg
}

/// Persists a wizard-produced config.
///
/// `Some(store)` is the credential-vault flow (M5): the token goes into
/// `store` first, then the scrubbed `config.toml` lands in the cwd
/// (secrets never touch the file; they return at startup via
/// [`CyDriveConfig::with_credential_backfill`]).
///
/// `None` is the headless flow (WSL / servers without Secret Service,
/// 2026-09-04 fix): the secrets are written INTO `config.toml` via
/// [`CyDriveConfig::save_toml`] — the previous behavior stored them in a
/// volatile in-memory fallback that evaporated on exit while the wizard
/// claimed success, silently losing the entered token.
pub fn persist_setup(cfg: &CyDriveConfig, store: Option<&dyn CredentialStore>) -> Result<()> {
    match store {
        Some(store) => {
            store
                .set(BOT_TOKEN, &cfg.bot_token)
                .context("storing bot_token in the OS credential store")?;
            if cfg.enable_encryption {
                if let Some(password) = cfg.encryption_password.as_deref() {
                    store
                        .set(ENCRYPTION_PASSWORD, password)
                        .context("storing encryption_password in the OS credential store")?;
                }
            }
            cfg.save_toml_scrubbed(Path::new("config.toml"))
                .context("writing the scrubbed config.toml")?;
        }
        None => {
            cfg.save_toml(Path::new("config.toml"))
                .context("writing config.toml with the secrets in-file (headless mode)")?;
        }
    }
    Ok(())
}

/// The interactive wizard against the injected credential store.
/// `Some(store)` = credential-vault mode; `None` = headless mode (the
/// secrets land in `config.toml`, banner and final message say so).
///
/// First question is the storage backend (Phase 2 / B3b dispatch unit;
/// default telegram — the frozen legacy flow, byte-identical):
///
/// - **telegram** — the frozen prompts: bot token (validated in a
///   red-on-error loop), chat ID (numeric loop), drive letter
///   (Windows only, defaulting to the platform's best pick);
/// - **baidu** — paste app key / secret / refresh token; the refresh
///   runs against the OAuth endpoint (verification — an invalid
///   refresh_token refuses the wizard instead of writing a dead
///   config), and the VERIFIED pair plus the pasted credentials land
///   in `config.toml` (K14 allows plaintext token keys there — the
///   vault flow scrubs only the telegram token/password/secret; the
///   baidu keys have no credential-schema keyring entries);
/// - **local** — one absolute-path prompt (validated), no secrets.
///
/// Then apply + persist + the run guidance. (Async: the baidu branch's
/// refresh-verification dials the OAuth endpoint.)
pub async fn run_setup_interactive(store: Option<&dyn CredentialStore>) -> Result<()> {
    use dialoguer::{Input, Select};

    println!("CyDrive first-time setup");
    match store {
        Some(_) => println!("Secrets are stored in the OS credential manager, never in the config file."),
        None => println!("No OS credential store is available (headless); secrets will be written into config.toml."),
    }

    let backends = ["telegram", "baidu", "local (a directory on this machine)"];
    let backend = Select::new()
        .with_prompt("Storage backend")
        .items(backends)
        .default(0)
        .interact()
        .context("asking for the storage backend")?;
    match backend {
        1 => return run_setup_baidu(store).await,
        2 => return run_setup_local(),
        _ => {}
    }

    let bot_token: String = Input::new()
        .with_prompt("Telegram Bot Token (from @BotFather)")
        .validate_with(|input: &String| validate_token(input))
        .interact_text()
        .context("reading the bot token")?
        .trim()
        .to_string();

    let chat_id: i64 = Input::new()
        .with_prompt("Telegram User/Chat ID (from @userinfobot)")
        .interact_text()
        .context("reading the chat id")?;

    let drive_letter = if cfg!(windows) {
        let default_letter = cloudkit_platform::pick_drive_letter(
            "Y:",
            &cloudkit_platform::windows::used_drive_letters(),
        );
        Input::new()
            .with_prompt("Windows drive letter")
            .default(default_letter)
            .interact_text()
            .context("reading the drive letter")?
    } else {
        // Python does not ask for a letter off Windows either; the spec
        // freezes "don't ask" as "keep the config default".
        "Y:".to_string()
    };

    let answers = WizardAnswers {
        bot_token,
        chat_id,
        drive_letter,
    };
    let cfg = apply_wizard(CyDriveConfig::default(), &answers);
    persist_setup(&cfg, store)?;

    match store {
        Some(_) => println!("Configuration saved to ./config.toml (secrets live in the credential store)."),
        None => println!("Configuration saved to ./config.toml (secrets are IN the file — headless mode; keep it private)."),
    }
    println!("Run `cydrive run` to start CyDrive, or `cydrive doctor` to verify the setup.");
    Ok(())
}

/// The baidu branch of the wizard (Phase 2 / B3b): paste the three
/// credentials → refresh-verify (a live network call; a failure
/// refuses the wizard with the actionable message instead of writing
/// a dead config) → persist the verified four keys + `backend=baidu`.
///
/// Token persistence is `config.toml` in BOTH store modes (K14: the
/// token keys are config-domain; the scrubbed write keeps them — only
/// the telegram token/password/sync-secret are scrubbed).
async fn run_setup_baidu(store: Option<&dyn CredentialStore>) -> Result<()> {
    use dialoguer::Input;

    println!("Baidu netdisk backend: paste the app credentials and a refresh token.");
    println!("  (The refresh token rotates on every refresh — this wizard verifies it once and stores the NEW pair.)");

    let answers = loop {
        let app_key: String = Input::new()
            .with_prompt("Baidu App Key")
            .interact_text()
            .context("reading the app key")?;
        let app_secret: String = Input::new()
            .with_prompt("Baidu App Secret")
            .interact_text()
            .context("reading the app secret")?;
        let refresh_token: String = Input::new()
            .with_prompt("Baidu Refresh Token")
            .interact_text()
            .context("reading the refresh token")?;
        let answers = BaiduWizardAnswers {
            app_key,
            app_secret,
            refresh_token,
        };
        match validate_baidu_credentials(&answers) {
            Ok(()) => break answers,
            Err(complaint) => println!("{complaint}"),
        }
    };

    // Refresh-verify (production OAuth endpoint; K18 direct client —
    // no proxy). One-time-use semantics: the response's pair is the
    // only live value from here on.
    let params = ck_baidu::BaiduParams {
        app_key: answers.app_key.trim().to_string(),
        app_secret: answers.app_secret.trim().to_string(),
        refresh_token: Some(answers.refresh_token.trim().to_string()),
        ..Default::default()
    };
    println!("Verifying the refresh token against Baidu OAuth ...");
    let (access, refresh) = match ck_baidu::refresh_tokens(&params).await {
        Ok(pair) => pair,
        Err(cloudkit_storage::StorageError::Unauthorized { recoverable: false }) => {
            anyhow::bail!(
                "the refresh token was refused (invalid or expired) — nothing was written; \
                 re-authorize the app to obtain a fresh refresh token and re-run \
                 `cydrive setup`"
            );
        }
        Err(error) => {
            anyhow::bail!(
                "verifying the refresh token failed ({error}) — nothing was written; check \
                 the network (baidu connects directly, no proxy) and re-run `cydrive setup`"
            );
        }
    };

    let cfg = apply_baidu_wizard(CyDriveConfig::default(), &answers, &access, &refresh);
    persist_setup(&cfg, store)?;
    println!(
        "Configuration saved to ./config.toml (backend = baidu; the four baidu_* keys are in \
         the file — K14 allows plaintext token keys there, keep it private)."
    );
    println!("Run `cydrive doctor` to probe the token, or `cydrive run` to start.");
    Ok(())
}

/// The local branch of the wizard (Phase 2 / B3b): one absolute-path
/// prompt (validated; the directory is created on first run by the
/// driver factory — doctor probes writability on demand). No secrets.
fn run_setup_local() -> Result<()> {
    use dialoguer::Input;

    println!("Local backend: the drive is one directory on this machine.");

    let answers = loop {
        let local_root: String = Input::new()
            .with_prompt("Local drive root (absolute path)")
            .interact_text()
            .context("reading the local root")?;
        let answers = LocalWizardAnswers { local_root };
        match validate_local_root(&answers.local_root) {
            Ok(()) => break answers,
            Err(complaint) => println!("{complaint}"),
        }
    };

    let cfg = apply_local_wizard(CyDriveConfig::default(), &answers);
    persist_setup(&cfg, None)?;
    println!(
        "Configuration saved to ./config.toml (backend = local; no secrets involved). The \
         root directory is created on the first `cydrive run`."
    );
    println!("Run `cydrive doctor` to check the root, or `cydrive run` to start.");
    Ok(())
}

// ------------------------------------------------ multi-volume (MV4) ---

/// The process-level `config.toml` of the multi-volume skeleton: the
/// `volumes_dir` key plus the process-scoped keys — nothing else (the
/// K19 mixing guard rejects any volume-scoped key here, so the skeleton
/// is hand-written rather than round-tripped through `save_toml`, which
/// would serialize every defaulted volume key and fail its own load).
const MULTI_PROCESS_TOML: &str = "\
# cydrive multi-volume process config (generated by `cydrive setup --multi`).
# Process-level keys only — each volume's own settings live in volumes/<name>.toml.
volumes_dir = \"volumes\"

webdav_host = \"127.0.0.1\"
webdav_port = 8080
enable_web_ui = true
web_ui_host = \"127.0.0.1\"
web_ui_port = 8088
";

/// The example volume file of the multi-volume skeleton: the local
/// backend with a volume-relative root (relative paths resolve under the
/// volume's own home directory `volumes/local/`, K21) and a
/// commented-out `drive_letter` claiming nothing by default.
const EXAMPLE_VOLUME_TOML: &str = "\
# One volume per file: volume-scoped keys only (backend, credentials,
# db_path, drive_letter, ...). Relative paths resolve under this volume's
# own directory volumes/local/ (K21).
backend = \"local\"
local_root = \"root\"
# Uncomment to auto-mount this volume as a drive letter (the process-level
# auto_mount_drive switch gates every mount):
# drive_letter = \"V\"
";

/// The non-interactive multi-volume skeleton behind `cydrive setup
/// --multi` (Phase 2.5 / MV4, minimal by design — no wizard prompts, the
/// multi-volume layout is files the user edits directly): writes the
/// process-scoped `config.toml` plus one example volume file
/// `volumes/local.toml`. Refuses to touch anything when a `config.toml`
/// or the example volume file already exists — setup never overwrites an
/// existing configuration. Returns the printed report.
pub fn run_setup_multi() -> Result<String> {
    let config_path = Path::new("config.toml");
    if config_path.exists() {
        anyhow::bail!(
            "config.toml already exists in the current directory — `cydrive setup --multi` \
             only writes a fresh skeleton; add volumes by creating more `<name>.toml` files \
             under the volumes_dir instead"
        );
    }
    let volume_path = Path::new("volumes").join("local.toml");
    if volume_path.exists() {
        anyhow::bail!(
            "{} already exists — refusing to overwrite it; edit the volume file by hand \
             instead",
            volume_path.display()
        );
    }
    std::fs::create_dir_all(Path::new("volumes")).context("creating the volumes directory")?;
    std::fs::write(config_path, MULTI_PROCESS_TOML)
        .context("writing the multi-volume config.toml")?;
    std::fs::write(&volume_path, EXAMPLE_VOLUME_TOML).context("writing the example volume file")?;

    let mut report = String::new();
    let _ = writeln!(
        report,
        "CyDrive multi-volume skeleton written (nothing is running yet):"
    );
    let _ = writeln!(
        report,
        "- config.toml — process-level keys only (volumes_dir = \"volumes\", the WebDAV and \
         dashboard endpoints)"
    );
    let _ = writeln!(
        report,
        "- volumes/local.toml — a local-backend example volume (root under the volume's own \
         directory; a commented-out drive_letter to uncomment per volume)"
    );
    let _ = writeln!(
        report,
        "Add more volumes as volumes/<name>.toml (one storage volume per file, name = the \
         file stem), then `cydrive volumes` lists them, `cydrive doctor` checks each one, \
         and `cydrive run` boots them all behind one WebDAV port at /vol/<name>."
    );
    Ok(report)
}
