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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => run().await,
    }
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
