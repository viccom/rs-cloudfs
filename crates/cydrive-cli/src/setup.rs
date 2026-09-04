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

use std::path::Path;

use anyhow::{Context, Result};
use cydrive_core::config::CyDriveConfig;
use cydrive_core::credentials::{CredentialStore, BOT_TOKEN, ENCRYPTION_PASSWORD};

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
/// Prompts:
/// bot token (validated in a red-on-error loop), chat ID (numeric loop —
/// dialoguer re-asks on a parse failure), drive letter (Windows only,
/// defaulting to the platform's best pick; off Windows the question is
/// skipped and the config default `"Y:"` applies, per the frozen spec).
/// Then apply + persist + the run guidance.
pub fn run_setup_interactive(store: Option<&dyn CredentialStore>) -> Result<()> {
    use dialoguer::Input;

    println!("CyDrive first-time setup");
    match store {
        Some(_) => println!("Secrets are stored in the OS credential manager, never in the config file."),
        None => println!("No OS credential store is available (headless); secrets will be written into config.toml."),
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
        let default_letter = cydrive_platform::pick_drive_letter(
            "Y:",
            &cydrive_platform::windows::used_drive_letters(),
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
