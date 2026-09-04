//! The `cydrive` binary: CLI parsing and the production `run` flow.
//!
//! `run` = discover config → validate / configured-ness gates (clear
//! guidance instead of a wizard) → tracing init (Pretty/INFO on stdout,
//! a parseable `RUST_LOG` wins) → connect the `GrammersTransport` → boot
//! the stack → wait for Ctrl+C → graceful shutdown → exit 0. The
//! operational subcommands: push/pull (direct upload/download data
//! channel, no WebDAV size limits), cache (local disk cache stats /
//! clear), mount/unmount (drive mapping), fix-reg (WebClient tuning,
//! elevated), migrate (legacy Python import), stats (drive statistics
//! table), doctor (offline diagnosis + platform checks) and setup
//! (interactive first-time wizard).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cydrive_cli::{discover_config, run_with_transport};
use cydrive_core::config::CyDriveConfig;
use cydrive_core::logging::LogConfig;
use cydrive_core::rel_path::RelPath;
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
    /// Upload a local file into the drive (bypasses the 4 GB WebClient and
    /// 1900 MB Web UI limits; uploads are chunked automatically).
    Push {
        /// Local file to upload.
        path: PathBuf,
        /// Destination path inside the drive (default: /<file name>).
        #[arg(long)]
        dest: Option<String>,
    },
    /// Download a drive file to a local path (hydrates from Telegram when
    /// not cached).
    Pull {
        /// Path inside the drive.
        path: String,
        /// Local destination (file path or existing directory).
        out: PathBuf,
    },
    /// Inspect or clear the local disk cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
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
    /// Print drive statistics from the metadata DB (files, folders,
    /// cloud storage, pending uploads) as a table.
    Stats,
    /// Diagnose the local installation: config, DB, cache, ports, and
    /// the Windows WebClient registry/service state.
    Doctor,
    /// Interactive first-time configuration wizard (bot token, chat ID,
    /// drive letter); secrets go to the OS credential store.
    Setup,
}

#[derive(Debug, Subcommand)]
enum CacheAction {
    /// Print cache root, used bytes and the configured limit.
    Stats,
    /// Delete cached copies of uploaded files; pending-upload staging
    /// copies are preserved.
    Clear,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => run().await,
        Command::Push { path, dest } => push_cmd(path, dest).await,
        Command::Pull { path, out } => pull_cmd(path, out).await,
        Command::Cache { action } => cache_cmd(action),
        Command::Mount { url, letter } => mount_cmd(url, letter).await,
        Command::Unmount { letter } => unmount_cmd(letter).await,
        Command::FixReg => fix_reg_cmd().await,
        Command::Migrate => migrate_cmd(),
        Command::Stats => stats_cmd(),
        Command::Doctor => doctor_cmd(),
        Command::Setup => setup_cmd(),
    }
}

/// The run() gates shared by the data channel: without a bot token +
/// chat id the Telegram connect cannot succeed, so fail with the same
/// actionable message instead of a transport error.
fn require_configured(cfg: &CyDriveConfig) -> Result<()> {
    cfg.validate().context("invalid configuration")?;
    if !cfg.is_configured() {
        anyhow::bail!(
            "CyDrive is not configured: set bot_token (a \"<id>:<secret>\" BotFather \
             token) and chat_id in config.toml (or a legacy config.json) in the \
             working directory, then run cydrive again"
        );
    }
    Ok(())
}

/// `cydrive push`: upload a local file straight through the data channel
/// (no WebDAV / Web UI size limits), drain the queue, then report the
/// row's terminal state. A degraded upload keeps its local copy and
/// retries on the next run.
async fn push_cmd(path: PathBuf, dest: Option<String>) -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    require_configured(&cfg)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("the source path {} carries no file name", path.display()))?;
    let dest = dest.unwrap_or_else(|| format!("/{file_name}"));
    let dest = RelPath::new(&dest)
        .with_context(|| format!("invalid drive path {dest:?} (drive paths start with \"/\")"))?;

    let stack = cydrive_cli::connect_stack(&cfg).await?;
    let pushed = cydrive_cli::push_file(&stack.vfs, &path, &dest).await?;
    println!("pushed {pushed} bytes; draining the upload queue ...");
    stack.shutdown().await;

    let terminal = stack
        .db
        .get_file(dest.as_str())
        .context("reading the pushed file's terminal state")?
        .filter(|row| row.is_uploaded);
    match terminal {
        Some(row) => println!(
            "uploaded: {} ({})",
            dest.as_str(),
            cydrive_cli::format_storage_size(row.size)
        ),
        None => println!(
            "queued but not uploaded yet (degraded or still pending); will retry on next run"
        ),
    }
    Ok(())
}

/// `cydrive pull`: hydrate a drive file (downloading from Telegram when
/// the local cache is cold) and copy it out to a local path.
async fn pull_cmd(path: String, out: PathBuf) -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    require_configured(&cfg)?;
    let rel = RelPath::new(&path).with_context(|| format!("invalid drive path {path:?}"))?;

    let stack = cydrive_cli::connect_stack(&cfg).await?;
    let pulled_to = cydrive_cli::pull_file(&stack.vfs, &rel, &out).await?;
    let bytes = std::fs::metadata(&pulled_to)
        .with_context(|| format!("reading the pulled file {}", pulled_to.display()))?
        .len();
    println!(
        "pulled: {} -> {} ({bytes} bytes)",
        rel.as_str(),
        pulled_to.display()
    );
    stack.shutdown().await;
    Ok(())
}

/// `cydrive cache stats|clear`: local-disk cache inspection and cleanup
/// — no Telegram connection involved.
fn cache_cmd(action: CacheAction) -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    match action {
        CacheAction::Stats => cydrive_cli::cache_stats(&cfg),
        CacheAction::Clear => cydrive_cli::cache_clear_cmd(&cfg),
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

/// `cydrive stats`: discover the config (keyring backfill included),
/// open the metadata DB and print the report table.
fn stats_cmd() -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    let db = cydrive_core::database::MetaDatabase::open(std::path::Path::new(&cfg.db_path))
        .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?;
    let stats = db.get_stats().context("reading drive statistics")?;
    println!(
        "{}",
        cydrive_cli::format_stats_report(
            &stats,
            &cfg.drive_letter,
            &cydrive_cli::default_mount_url(&cfg)
        )
    );
    Ok(())
}

/// `cydrive doctor`: offline checks from the discovered config (a config
/// that will not load still gets diagnosed — `config_present: false`
/// with the default ports), then the platform/remote checks, all merged
/// into one report. Never fails on an unhealthy installation; the report
/// is the answer.
fn doctor_cmd() -> Result<()> {
    let discovered = discover_config();
    let config_present = discovered.is_ok();
    let cfg = discovered.unwrap_or_default();
    let ctx = cydrive_cli::doctor::DoctorContext {
        config_present,
        db_path: config_present.then(|| std::path::PathBuf::from(&cfg.db_path)),
        cache_path: config_present.then(|| std::path::PathBuf::from(&cfg.cache_path)),
        webdav_port: cfg.webdav_port,
        web_ui_port: cfg.web_ui_port,
    };
    let mut results = cydrive_cli::doctor::run_doctor(&ctx);
    results.extend(cydrive_cli::doctor::platform_checks());
    print!("{}", cydrive_cli::doctor::render_report(&results));
    Ok(())
}

/// `cydrive setup`: the interactive wizard against the production OS
/// credential store. Unlike `migrate`, an unusable store degrades to the
/// in-memory fallback with a warning (same as config discovery): the
/// scrubbed `config.toml` still lands on disk, and the warning tells the
/// user the token did not persist beyond this process.
fn setup_cmd() -> Result<()> {
    let store: Box<dyn cydrive_core::credentials::CredentialStore> =
        match cydrive_cli::KeyringStore::new() {
            Ok(store) => Box::new(store),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "OS credential store unavailable; the wizard's scrubbed config.toml is \
                     still written, but the bot token will not survive this process — bring \
                     the platform keyring up and re-run cydrive setup"
                );
                Box::new(cydrive_core::credentials::InMemoryStore::new())
            }
        };
    cydrive_cli::setup::run_setup_interactive(store.as_ref())
}

/// The production run flow; every step here is covered by the library
/// tests except the transport connect, which needs real Telegram
/// credentials and is compile-verified only.
///
/// UX contract (real-machine regression 2026-09-02): every long phase
/// prints visible progress BEFORE blocking, the Telegram connect is
/// deadline-bounded (90s) with a human-readable diagnosis on failure,
/// and Ctrl+C during the connect phase exits cleanly instead of
/// hard-killing the process with no output.
async fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    println!(
        "cydrive {} starting in {}",
        env!("CARGO_PKG_VERSION"),
        cwd.display()
    );
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

    let transport_config = cydrive_cli::transport_config_from(&cfg, &cwd);
    println!(
        "Connecting to Telegram (session: {}) ...",
        transport_config.session_path.display()
    );
    let connect = cydrive_cli::connect_with_deadline(
        GrammersTransport::connect(transport_config),
        cydrive_cli::CONNECT_DEADLINE,
    );
    let transport = tokio::select! {
        result = connect => match result {
            Ok(transport) => transport,
            Err(error) => {
                eprintln!("Error: connecting the Telegram transport");
                match &error {
                    cydrive_cli::ConnectGuardError::Deadline(_) => {}
                    cydrive_cli::ConnectGuardError::Inner(source) => {
                        eprintln!("Caused by:\n    {source}");
                    }
                }
                eprintln!("{}", cydrive_cli::connect_failure_hint());
                std::process::exit(1);
            }
        },
        _ = tokio::signal::ctrl_c() => {
            println!("Interrupted while connecting to Telegram; exiting.");
            return Ok(());
        }
    };

    let handle = run_with_transport(&cfg, Arc::new(transport)).await?;
    println!(
        "CyDrive is running: WebDAV at http://{}  |  dashboard at http://127.0.0.1:{}  |  press Ctrl+C to stop",
        handle.local_addr(),
        cfg.web_ui_port
    );
    if handle.mounted_letter.is_none() && cfg.auto_mount_drive && cfg!(windows) {
        println!(
            "Note: no drive letter was mapped (see the log above); `cydrive fix-reg` in an \
             elevated shell and a running WebClient are prerequisites for Explorer mapping."
        );
    }

    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl+C")?;
    println!("Shutting down (draining uploads, unmounting) ...");
    handle.shutdown().await;
    Ok(())
}
