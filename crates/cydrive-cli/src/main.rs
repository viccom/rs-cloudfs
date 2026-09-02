//! The `cydrive` binary: CLI parsing and the production `run` flow.
//!
//! `run` = discover config → validate / configured-ness gates (clear
//! guidance instead of a wizard) → tracing init (Pretty/INFO on stdout,
//! a parseable `RUST_LOG` wins) → connect the `GrammersTransport` → boot
//! the stack → wait for Ctrl+C → graceful shutdown → exit 0. The other
//! Python subcommands (mount/unmount/fix-reg/stats/setup) are future
//! units; clap rejects them as unknown today.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cydrive_cli::{discover_config, run_with_transport};
use cydrive_core::logging::LogConfig;
use cydrive_telegram::config::{
    TransportConfig, DEFAULT_API_HASH, DEFAULT_API_ID, DEFAULT_SESSION_STEM,
};
use cydrive_telegram::transport::GrammersTransport;

/// CyDrive — Telegram as an unlimited cloud drive, served over WebDAV.
#[derive(Debug, Parser)]
#[command(name = "cydrive", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the full stack: metadata DB, upload queue, WebDAV server.
    Run,
    /// Map a drive letter to the WebDAV server (`net use`).
    Mount {
        /// WebDAV URL (default: glued from the config's host/port).
        #[arg(long)]
        url: Option<String>,
        /// Drive letter (default: the config's `drive_letter`, e.g. "Y:").
        #[arg(long)]
        letter: Option<String>,
    },
    /// Remove a mapped drive letter.
    Unmount {
        /// Drive letter (default: the config's `drive_letter`).
        #[arg(long)]
        letter: Option<String>,
    },
    /// Tune the WebClient registry (4 GB limit + Basic auth) and restart
    /// the service. Needs an elevated shell.
    FixReg,
    /// Import a legacy Python installation: secrets into the OS
    /// credential store, a scrubbed canonical `config.toml`, and
    /// zero-copy adoption of any existing `cydrive_meta.db` /
    /// `Telegram_Cache` in the working directory.
    Migrate,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => run().await,
        Command::Mount { url, letter } => mount_cmd(url, letter).await,
        Command::Unmount { letter } => unmount_cmd(letter).await,
        Command::FixReg => fix_reg_cmd().await,
        Command::Migrate => migrate_cmd(),
    }
}

/// `cydrive mount`: resolve flags against the config, then map the best
/// available letter. Windows-only (the platform stub reports otherwise).
async fn mount_cmd(url: Option<String>, letter: Option<String>) -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    let (letter, url) = cydrive_cli::resolve_mount_params(&cfg, url, letter);
    let mounted = cydrive_platform::windows::mount_drive(&letter, &url)
        .with_context(|| format!("mounting {url} at {letter}"))?;
    println!("CyDrive mounted at {mounted} -> {url}");
    Ok(())
}

/// `cydrive unmount`: remove the mapping for the resolved letter.
async fn unmount_cmd(letter: Option<String>) -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    let letter = cydrive_cli::resolve_unmount_letter(&cfg, letter);
    cydrive_platform::windows::unmount_drive(&letter)
        .with_context(|| format!("unmounting {letter}"))?;
    println!("CyDrive unmounted from {letter}");
    Ok(())
}

/// `cydrive fix-reg`: write the WebClient tuning values and restart the
/// service (mirrors the Python `fix-reg` subcommand).
async fn fix_reg_cmd() -> Result<()> {
    cydrive_platform::windows::optimize_webdav_registry()
        .context("tuning the WebClient registry")?;
    println!("WebClient registry tuned (4 GB limit, Basic auth) and restarted");
    Ok(())
}

/// `cydrive migrate`: run the migration against the production OS
/// credential store. Unlike config discovery, an unusable store is a
/// hard error here — migrating secrets into a volatile in-memory
/// fallback would report success while losing them.
fn migrate_cmd() -> Result<()> {
    let store = cydrive_cli::KeyringStore::new().context(
        "the OS credential store is unavailable, so cydrive migrate cannot persist \
         your secrets; bring the platform keyring up (Windows Credential Manager / \
         macOS Keychain / Secret Service) and retry",
    )?;
    let report = cydrive_cli::run_migrate(&store).context("migration failed")?;
    print!("{report}");
    Ok(())
}

/// The production run flow; every step here is covered by the library
/// tests except the transport connect, which needs real Telegram
/// credentials and is compile-verified only.
async fn run() -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    cfg.validate().context("invalid configuration")?;
    if !cfg.is_configured() {
        anyhow::bail!(
            "CyDrive is not configured: set bot_token (a \"<id>:<secret>\" BotFather \
             token) and chat_id in config.toml (or a legacy config.json) in the \
             working directory, then run cydrive again"
        );
    }

    // Pretty/INFO on stdout; a parseable RUST_LOG overrides the level.
    cydrive_core::logging::init(&LogConfig::default()).context("initializing logging")?;

    let transport_config = TransportConfig {
        api_id: DEFAULT_API_ID,
        api_hash: DEFAULT_API_HASH.to_owned(),
        bot_token: cfg.bot_token.clone(),
        chat_id: cfg.chat_id,
        session_path: std::env::current_dir()
            .context("resolving the working directory")?
            .join(format!("{DEFAULT_SESSION_STEM}.session")),
    };
    let transport = GrammersTransport::connect(transport_config)
        .await
        .context("connecting the Telegram transport")?;

    let handle = run_with_transport(&cfg, Arc::new(transport)).await?;
    tracing::info!(
        webdav = %handle.local_addr(),
        "CyDrive is running; press Ctrl+C to stop"
    );

    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl+C")?;
    handle.shutdown().await;
    Ok(())
}
