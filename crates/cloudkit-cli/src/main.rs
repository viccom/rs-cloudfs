//! The `cydrive` binary: CLI parsing and the production `run` flow.
//!
//! `run` = discover config → validate / configured-ness gates (clear
//! guidance instead of a wizard) → tracing init (Pretty/INFO on stdout,
//! a parseable `RUST_LOG` wins) → connect the `GrammersTransport` → boot
//! the stack → wait for a stop source (Ctrl+C / SIGTERM / `cydrive stop`)
//! → graceful shutdown → exit 0. The
//! operational subcommands: stop (gracefully stop a background `run`
//! instance via its loopback control channel), status (probe the
//! instance, both listening ports and the current drive mapping),
//! push/pull (direct upload/download data channel, no WebDAV size
//! limits), cache (local disk cache stats / clear), sync (one manual
//! metadata-sync pass against the configured cydrive-sync server),
//! mount/unmount (drive mapping), fix-reg (WebClient tuning, elevated),
//! migrate (legacy Python import), stats (drive statistics table),
//! rebuild (bootstrap the metadata DB from the baidu/local backend's
//! authoritative index), doctor (offline diagnosis + platform checks)
//! and setup (interactive first-time wizard).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use ck_telegram::transport::GrammersTransport;
use clap::{Parser, Subcommand};
use cloudkit_cli::{
    discover_config, discover_config_with_volumes, BaiduEndpoints, ConfigTokenStore,
    DiscoveredConfig, VolumeStatus,
};
use cloudkit_core::config::{Backend, CyDriveConfig, VolumeConfig};
use cloudkit_core::logging::LogConfig;
use cloudkit_core::rel_path::RelPath;

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
    /// Gracefully stop a background `cydrive run` instance through its
    /// loopback control channel (must run in the same working directory
    /// as the instance).
    Stop,
    /// Show the current state: running instance (version, via the control
    /// channel's PING), WebDAV/dashboard ports, and the drive mapping.
    /// Same working-directory rule as `run`/`stop`.
    Status,
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
    /// Run one metadata sync pass against the configured sync server
    /// (`run` also syncs periodically on its own). The shared secret,
    /// when the server requires one, comes from the CYDRIVE_SYNC_SECRET
    /// environment variable or the config.toml sync_secret key (the
    /// variable wins).
    Sync,
    /// Mount the WebDAV server: a drive letter on Windows (`net use`),
    /// a directory on Linux (gio → davfs2).
    Mount {
        /// WebDAV URL (default: glued from the config's host/port).
        #[arg(long)]
        url: Option<String>,
        /// Drive letter (default: the config's `drive_letter`, e.g. "Y:")
        /// — Windows only.
        #[arg(long)]
        letter: Option<String>,
        /// Mount point directory (default: ~/CyDrive) — Unix only.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Unmount: release the drive letter (Windows) or the mount point
    /// directory (Unix).
    Unmount {
        /// Drive letter (default: the config's `drive_letter`) —
        /// Windows only.
        #[arg(long)]
        letter: Option<String>,
        /// Mount point directory (default: ~/CyDrive) — Unix only.
        #[arg(long)]
        path: Option<PathBuf>,
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
    /// Rebuild this instance's metadata DB from the backend's
    /// authoritative index (baidu / local backends; the instance db at
    /// the current working directory is rebuilt in place). Encrypted
    /// instances are refused — use `cydrive sync` for those.
    Rebuild,
    /// Diagnose the local installation: config, DB, cache, ports, and
    /// the Windows WebClient registry/service state.
    Doctor,
    /// Interactive first-time configuration wizard (bot token, chat ID,
    /// drive letter); secrets go to the OS credential store, or by
    /// explicit choice into config.toml when no credential store is
    /// available (headless).
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
        Command::Stop => stop_cmd().await,
        Command::Status => status_cmd().await,
        Command::Push { path, dest } => push_cmd(path, dest).await,
        Command::Pull { path, out } => pull_cmd(path, out).await,
        Command::Cache { action } => cache_cmd(action),
        Command::Sync => sync_cmd().await,
        Command::Mount { url, letter, path } => mount_cmd(url, letter, path).await,
        Command::Unmount { letter, path } => unmount_cmd(letter, path).await,
        Command::FixReg => fix_reg_cmd().await,
        Command::Migrate => migrate_cmd(),
        Command::Stats => stats_cmd(),
        Command::Rebuild => rebuild_cmd().await,
        Command::Doctor => doctor_cmd().await,
        Command::Setup => setup_cmd().await,
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

/// `cydrive stop`: discover the config (same cwd rule as `run` — the
/// port file resolves from the discovered `db_path`) and ask the running
/// instance to shut down gracefully via the loopback control channel.
async fn stop_cmd() -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    cloudkit_cli::control::stop_cmd(&cfg).await
}

/// `cydrive status`: discover the config (same cwd rule as `run`/`stop`
/// — the control file resolves from the discovered `db_path`, so an
/// instance is only detectable from its own working directory), collect
/// the probes and print the rendered report.
async fn status_cmd() -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    let report = cloudkit_cli::collect_status(&cfg).await;
    println!(
        "{}",
        cloudkit_cli::render_status(
            &report,
            &cloudkit_cli::default_mount_url(&cfg),
            cloudkit_cli::dashboard_url(&cfg),
        )
    );
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

    let stack = cloudkit_cli::connect_stack(&cfg).await?;
    let pushed = cloudkit_cli::push_file(&stack.vfs, &path, &dest).await?;
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
            cloudkit_cli::format_storage_size(row.size)
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

    let stack = cloudkit_cli::connect_stack(&cfg).await?;
    let pulled_to = cloudkit_cli::pull_file(&stack.vfs, &rel, &out).await?;
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
        CacheAction::Stats => cloudkit_cli::cache_stats(&cfg),
        CacheAction::Clear => cloudkit_cli::cache_clear_cmd(&cfg),
    }
}

/// `cydrive sync`: discover the config (same env > file > keyring chain
/// as `run`), then run exactly one sync pass and print the counters.
/// No tracing subscriber and no transport — a one-shot command prints
/// its own output (the stats/doctor convention). The shared secret, when
/// the server requires one, resolves exactly like the periodic task's:
/// `CYDRIVE_SYNC_SECRET` over the config.toml `sync_secret` key.
async fn sync_cmd() -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    let secret = cloudkit_cli::resolve_sync_secret(&cfg);
    let outcome = cloudkit_cli::run_sync_command(&cfg, secret.as_deref()).await?;
    println!("{}", cloudkit_cli::render_sync_summary(&outcome));
    Ok(())
}

/// `cydrive mount`: resolve flags against the config, then map the best
/// available letter (Windows) or mount the directory via the Linux
/// gio→davfs2 chain (other Unixes get the platform stub's Unsupported).
async fn mount_cmd(
    url: Option<String>,
    letter: Option<String>,
    path: Option<PathBuf>,
) -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;

    #[cfg(unix)]
    {
        let _ = letter; // Windows-only flag
        let url = url.unwrap_or_else(|| cloudkit_cli::default_mount_url(&cfg));
        let mount_point = unix_mount_point(path)?;
        let report = cloudkit_platform::linux::mount_drive(&mount_point, &url)
            .with_context(|| format!("mounting {url} at {}", mount_point.display()))?;
        println!("{report}");
        Ok(())
    }

    #[cfg(not(unix))]
    {
        if path.is_some() {
            anyhow::bail!("--path applies to Unix mounts only; Windows uses drive letters");
        }
        let (letter, url) = cloudkit_cli::resolve_mount_params(&cfg, url, letter);
        let mounted = cloudkit_platform::windows::mount_drive(&letter, &url)
            .with_context(|| format!("mounting {url} at {letter}"))?;
        println!("CyDrive mounted at {mounted} -> {url}");
        Ok(())
    }
}

/// `cydrive unmount`: release the mapping for the resolved letter
/// (Windows) or the mount point directory (Unix).
async fn unmount_cmd(letter: Option<String>, path: Option<PathBuf>) -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;

    #[cfg(unix)]
    {
        let _ = (&cfg, letter); // unmount takes no config defaults on Unix
        let mount_point = unix_mount_point(path)?;
        let report = cloudkit_platform::linux::unmount_drive(&mount_point)
            .with_context(|| format!("unmounting {}", mount_point.display()))?;
        println!("{report}");
        Ok(())
    }

    #[cfg(not(unix))]
    {
        if path.is_some() {
            anyhow::bail!("--path applies to Unix mounts only; Windows uses drive letters");
        }
        let letter = cloudkit_cli::resolve_unmount_letter(&cfg, letter);
        cloudkit_platform::windows::unmount_drive(&letter)
            .with_context(|| format!("unmounting {letter}"))?;
        println!("CyDrive unmounted from {letter}");
        Ok(())
    }
}

/// Resolves the Unix mount point for `mount`/`unmount`: an explicit
/// `--path` wins, otherwise the Python baseline's default `~/CyDrive`
/// (derived from `$HOME`).
#[cfg(unix)]
fn unix_mount_point(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(point) => Ok(point),
        None => {
            let home = std::env::var("HOME")
                .context("no --path given and $HOME is unset; pass --path <dir>")?;
            Ok(cloudkit_platform::default_mount_point(
                std::path::Path::new(&home),
            ))
        }
    }
}

/// `cydrive fix-reg`: write the WebClient tuning values and restart the
/// service (mirrors the Python `fix-reg` subcommand).
async fn fix_reg_cmd() -> Result<()> {
    cloudkit_platform::windows::optimize_webdav_registry()
        .context("tuning the WebClient registry")?;
    println!("WebClient registry tuned (4 GB limit, Basic auth) and restarted");
    Ok(())
}

/// `cydrive migrate`: run the migration against the production OS
/// credential store. Unlike config discovery, an unusable store is a
/// hard error here — migrating secrets into a volatile in-memory
/// fallback would report success while losing them.
fn migrate_cmd() -> Result<()> {
    let store = cloudkit_cli::KeyringStore::new().context(
        "the OS credential store is unavailable, so cydrive migrate cannot persist \
         your secrets; bring the platform keyring up (Windows Credential Manager / \
         macOS Keychain / Secret Service) and retry",
    )?;
    let report = cloudkit_cli::run_migrate(&store).context("migration failed")?;
    print!("{report}");
    Ok(())
}

/// `cydrive stats`: discover the config (keyring backfill included),
/// open the metadata DB and print the report table.
fn stats_cmd() -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    let db = cloudkit_core::database::MetaDatabase::open(std::path::Path::new(&cfg.db_path))
        .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?;
    let stats = db.get_stats().context("reading drive statistics")?;
    println!(
        "{}",
        cloudkit_cli::format_stats_report(
            &stats,
            &cfg.drive_letter,
            &cloudkit_cli::default_mount_url(&cfg)
        )
    );
    Ok(())
}

/// `cydrive rebuild`: discover the config (same cwd rule as `run` —
/// the instance db resolves from the discovered `db_path`), assemble
/// the driver from the `backend` key and bootstrap the index. One-shot
/// command: no tracing subscriber, its own output (the stats/doctor
/// convention).
async fn rebuild_cmd() -> Result<()> {
    let cfg = discover_config().context("config discovery failed")?;
    let outcome = cloudkit_cli::run_rebuild_command(&cfg).await?;
    println!(
        "rebuild complete: {} file row(s), {} directory row(s) rebuilt from the {} backend",
        outcome.files,
        outcome.dirs,
        cfg.backend.as_str()
    );
    Ok(())
}

/// `cydrive doctor`: offline checks from the discovered config (a config
/// that will not load still gets diagnosed — `config_present: false`
/// with the default ports), then the platform/remote checks, all merged
/// into one report. Never fails on an unhealthy installation; the report
/// is the answer.
///
/// Backend legs (Phase 2 / B3b): telegram keeps the fixed manual-run
/// advisory; baidu adds the offline K18 proxy notice plus the LIVE
/// token probe (uinfo + quota + a root list, read-only); local adds the
/// root exists-and-writable check and the K12 sync-unsupported warning.
/// The Windows WebClient leg runs for every backend (the WebDAV mount
/// surface is backend-independent).
async fn doctor_cmd() -> Result<()> {
    let discovered = discover_config();
    let config_present = discovered.is_ok();
    let cfg = discovered.unwrap_or_default();
    // The keyring availability probe doubles as the credential store the
    // doctor checks read: a failed constructor becomes an
    // UnavailableKeyring so the credentials check sees the headless
    // condition (C6) instead of a healthy-looking in-memory fallback.
    let credential_store: Arc<dyn cloudkit_core::credentials::CredentialStore> =
        match cloudkit_cli::KeyringStore::new() {
            Ok(store) => Arc::new(store),
            Err(error) => Arc::new(cloudkit_cli::doctor::UnavailableKeyring::new(
                error.to_string(),
            )),
        };
    let ctx = cloudkit_cli::doctor::DoctorContext {
        config_present,
        db_path: config_present.then(|| std::path::PathBuf::from(&cfg.db_path)),
        cache_path: config_present.then(|| std::path::PathBuf::from(&cfg.cache_path)),
        webdav_port: cfg.webdav_port,
        web_ui_port: cfg.web_ui_port,
        bot_token: cfg.bot_token.clone(),
        credential_store,
    };
    let mut results = cloudkit_cli::doctor::run_doctor(&ctx);
    results.extend(cloudkit_cli::doctor::webclient_checks());
    match cfg.backend {
        cloudkit_core::config::Backend::Telegram => {
            results.push(cloudkit_cli::doctor::telegram_connectivity_check());
        }
        cloudkit_core::config::Backend::Baidu => {
            results.extend(cloudkit_cli::doctor::backend_checks(&cfg));
            results.push(cloudkit_cli::doctor::baidu_connectivity_check(
                &cloudkit_cli::baidu_backend_probe(&cfg).await,
            ));
        }
        cloudkit_core::config::Backend::Local => {
            results.extend(cloudkit_cli::doctor::backend_checks(&cfg));
        }
    }
    print!("{}", cloudkit_cli::doctor::render_report(&results));
    Ok(())
}

/// `cydrive setup`: the interactive wizard. With a working OS credential
/// store the secrets go to the vault and the config stays scrubbed (M5).
/// Without one (WSL / servers without Secret Service) the user chooses:
/// write the secrets into `config.toml` (headless mode) or abort — the
/// pre-2026-09-04 behavior of silently storing them in a volatile
/// in-memory fallback reported success while losing the token.
async fn setup_cmd() -> Result<()> {
    let store: Option<cloudkit_cli::KeyringStore> = match cloudkit_cli::KeyringStore::new() {
        Ok(store) => Some(store),
        Err(error) => {
            println!(
                "Warning: the OS credential store is unavailable ({error}).\n\
                 On a headless system the secrets can be written into config.toml instead."
            );
            let proceed = dialoguer::Confirm::new()
                .with_prompt("Write the bot token into config.toml?")
                .default(false)
                .interact()
                .context("asking about headless secret storage")?;
            if !proceed {
                anyhow::bail!(
                    "setup aborted without persisting secrets: bring the platform keyring \
                     up (Windows Credential Manager / macOS Keychain / Secret Service) and \
                     re-run `cydrive setup`, or write bot_token into config.toml by hand"
                );
            }
            None
        }
    };
    cloudkit_cli::setup::run_setup_interactive(
        store
            .as_ref()
            .map(|s| s as &dyn cloudkit_core::credentials::CredentialStore),
    )
    .await
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
///
/// Backend dispatch (Phase 2 / B3b): telegram keeps its dedicated
/// deadline-bounded Grammers connect below VERBATIM (the absent
/// `backend` key IS telegram — pre-Phase-2 behavior); baidu/local
/// assemble through the unified [`cloudkit_cli::build_backend_transport`]
/// and boot with their backend-derived sync namespace (K12).
///
/// Phase 2.5 / MV1: discovery routes the two shapes — a plain
/// single-volume config runs the frozen path below, a `volumes_dir`
/// config runs [`run_multi_volume`] (per-volume dispatch + the Volume
/// Registry assembly).
async fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    println!(
        "cydrive {} starting in {}",
        env!("CARGO_PKG_VERSION"),
        cwd.display()
    );
    match discover_config_with_volumes().context("config discovery failed")? {
        DiscoveredConfig::Single(cfg) => run_single_volume(cfg, cwd).await,
        DiscoveredConfig::Multi { process, volumes } => {
            run_multi_volume(process, volumes, cwd).await
        }
    }
}

/// The single-volume run flow — the pre-Phase-2.5 behaviour, byte for
/// byte (only extracted from `run` for the MV1 dispatch).
async fn run_single_volume(cfg: CyDriveConfig, cwd: std::path::PathBuf) -> Result<()> {
    cfg.validate().context("invalid configuration")?;
    if cfg.backend == Backend::Telegram && !cfg.is_configured() {
        anyhow::bail!(
            "CyDrive is not configured: set bot_token (a \"<id>:<secret>\" BotFather \
             token) and chat_id in config.toml (or a legacy config.json) in the \
             working directory, then run cydrive again"
        );
    }

    // Pretty/INFO on stdout; a parseable RUST_LOG overrides the level.
    cloudkit_core::logging::init(&LogConfig::default()).context("initializing logging")?;

    let mut run_options = cloudkit_cli::RunOptions::default();
    let transport: Arc<dyn cloudkit_core::transport::CloudTransport> = match cfg.backend {
        Backend::Telegram => {
            // The legacy arm, byte-for-byte: session glue → visible
            // progress line → deadline-bounded connect raced against
            // Ctrl+C → the failure hint on error.
            match connect_telegram_volume(&cfg, &cwd).await? {
                Some(transport) => transport,
                None => return Ok(()), // Ctrl+C during the connect
            }
        }
        backend => {
            println!(
                "Connecting to the {backend} backend ...",
                backend = backend.as_str()
            );
            let dispatched = cloudkit_cli::build_backend_transport(&cfg).await?;
            // K12: the periodic sync task keys on the backend's own
            // identity (baidu's account uid), not on telegram creds.
            run_options.sync_namespace = Some(dispatched.sync_namespace_key());
            // Dashboard identity (web adapter): the volume label and the
            // boot quota snapshot exist only on the dispatched enum —
            // past this point the run flow sees the erased
            // CloudTransport face, which carries neither.
            run_options.web_volume = Some(dispatched.volume().to_string());
            run_options.web_quota = dispatched.web_quota_snapshot().await;
            dispatched.clone_dyn()
        }
    };

    let handle = cloudkit_cli::run_with_transport_options(&cfg, transport, run_options).await?;
    println!(
        "CyDrive is running: WebDAV at http://{}  |  dashboard at http://127.0.0.1:{}  |  press Ctrl+C to stop  |  or `cydrive stop`",
        handle.local_addr(),
        cfg.web_ui_port
    );
    if handle.mounted_letter.is_none() && cfg.auto_mount_drive && cfg!(windows) {
        println!(
            "Note: no drive letter was mapped (see the log above); `cydrive fix-reg` in an \
             elevated shell and a running WebClient are prerequisites for Explorer mapping."
        );
    }

    // The three shutdown sources race: Ctrl+C, SIGTERM (unix) and the
    // control channel's STOP (a `cydrive stop` against this instance).
    // Whichever wins, the same graceful drain follows.
    let stop_source = tokio::select! {
        _ = tokio::signal::ctrl_c() => "Ctrl+C",
        _ = cloudkit_cli::sigterm() => "SIGTERM",
        _ = handle.wait_for_stop_request() => "stop command",
    };
    println!("{stop_source} received.");
    println!("Shutting down (draining uploads, unmounting) ...");
    handle.shutdown().await;
    Ok(())
}

/// The multi-volume run flow (Phase 2.5 / MV1): per-volume transport
/// dispatch (the same two arms as the single-volume flow — telegram's
/// deadline-bounded connect and the unified baidu/local dispatch — keyed
/// on each volume's settings with K21 volume-home state directories),
/// then the Volume Registry assembly and ONE process-level stop gate.
async fn run_multi_volume(
    process: CyDriveConfig,
    volumes: Vec<VolumeConfig>,
    _cwd: std::path::PathBuf,
) -> Result<()> {
    // Pretty/INFO on stdout; a parseable RUST_LOG overrides the level.
    cloudkit_core::logging::init(&LogConfig::default()).context("initializing logging")?;
    println!("Assembling {} volume(s) ...", volumes.len());

    let mut injections = Vec::new();
    for spec in volumes {
        // K21: the volume's home directory anchors its session, baidu
        // state and (in the assembly) its db/cache paths.
        let home = cloudkit_cli::volume_home(&spec)?;
        let settings = cloudkit_cli::resolve_volume_settings(&spec)?;
        settings
            .validate()
            .with_context(|| format!("invalid configuration for volume `{}`", spec.name))?;
        let mut run_options = cloudkit_cli::RunOptions::default();
        let transport: Arc<dyn cloudkit_core::transport::CloudTransport> = match settings.backend {
            Backend::Telegram => {
                if !settings.is_configured() {
                    anyhow::bail!(
                        "volume `{}` is not configured: set bot_token (a \"<id>:<secret>\" \
                         BotFather token) and chat_id in the volume file {}",
                        spec.name,
                        spec.file_path.display()
                    );
                }
                println!("Connecting volume {} to Telegram ...", spec.name);
                match connect_telegram_volume(&settings, &home).await? {
                    Some(transport) => transport,
                    None => return Ok(()), // Ctrl+C during the connect
                }
            }
            backend => {
                println!(
                    "Connecting volume {name} to the {backend} backend ...",
                    name = spec.name,
                    backend = backend.as_str()
                );
                // K13/K21: token rotations write back into the volume's
                // own file; upload sessions live in the volume home.
                let token_store = ConfigTokenStore::new(spec.file_path.clone());
                let dispatched = cloudkit_cli::build_backend_transport_with(
                    &settings,
                    &BaiduEndpoints::default(),
                    Some(Arc::new(token_store)),
                    &home,
                )
                .await?;
                run_options.sync_namespace = Some(dispatched.sync_namespace_key());
                run_options.web_volume = Some(dispatched.volume().to_string());
                run_options.web_quota = dispatched.web_quota_snapshot().await;
                dispatched.clone_dyn()
            }
        };
        injections.push((spec, run_options, transport));
    }

    let handle = cloudkit_cli::run_multi_with_transports(&process, injections).await?;
    // K29 process-level banner: the volume list with per-volume status;
    // each volume's capability line rides in the boot log (same R-5
    // declaration as the single-volume banner). MV2 adds the single
    // WebDAV endpoint (per-volume path `/vol/<name>`) and the mounted
    // drive letters (K27) to the same line.
    let listing = handle
        .volumes()
        .iter()
        .map(|(name, status)| format!("{name}:{}", status.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut banner = format!("CyDrive multi-volume is running: {listing}");
    match handle.webdav_addr() {
        Some(addr) => banner.push_str(&format!(
            "  |  WebDAV at http://{addr} (volumes at /vol/<name>)"
        )),
        None => banner.push_str("  |  WebDAV unavailable (bind failed; see the log)"),
    }
    let letters = handle.mounted_letters();
    if !letters.is_empty() {
        banner.push_str(&format!("  |  mounted: {}", letters.join(", ")));
    }
    banner.push_str("  |  press Ctrl+C to stop  |  or `cydrive stop`");
    println!("{banner}");
    let volume_states = handle.volumes();
    let failed: Vec<&(String, VolumeStatus)> = volume_states
        .iter()
        .filter(|(_, status)| matches!(status, VolumeStatus::Failed { .. }))
        .collect();
    if !failed.is_empty() {
        let names = failed
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!("Warning: volume(s) failed to assemble and are NOT running: {names} (see the log for the reasons)");
    }

    // The three shutdown sources race (same as single-volume): Ctrl+C,
    // SIGTERM and the ONE control channel's STOP — whichever wins, every
    // volume drains through the aggregated stop sequence.
    let stop_source = tokio::select! {
        _ = tokio::signal::ctrl_c() => "Ctrl+C",
        _ = cloudkit_cli::sigterm() => "SIGTERM",
        _ = handle.wait_for_stop_request() => "stop command",
    };
    println!("{stop_source} received.");
    println!("Shutting down (draining every volume's uploads) ...");
    handle.shutdown().await?;
    Ok(())
}

/// The telegram connect arm shared by both run flows (Phase 2.5 / MV1
/// extracted verbatim from the single-volume path): session glue →
/// visible progress line → deadline-bounded connect raced against Ctrl+C
/// → the failure hint on error. `state_base` is the session-state
/// directory — the process cwd for a single-volume boot, the volume's
/// home directory in multi-volume mode (K21). Returns `None` when Ctrl+C
/// won the race (the caller exits cleanly).
async fn connect_telegram_volume(
    cfg: &CyDriveConfig,
    state_base: &std::path::Path,
) -> Result<Option<Arc<dyn cloudkit_core::transport::CloudTransport>>> {
    let transport_config = cloudkit_cli::transport_config_from(cfg, state_base);
    println!(
        "Connecting to Telegram (session: {}) ...",
        transport_config.session_path.display()
    );
    let connect = cloudkit_cli::connect_with_deadline(
        GrammersTransport::connect(transport_config),
        cloudkit_cli::CONNECT_DEADLINE,
    );
    let transport = tokio::select! {
        result = connect => match result {
            Ok(transport) => transport,
            Err(error) => {
                eprintln!("Error: connecting the Telegram transport");
                match &error {
                    cloudkit_cli::ConnectGuardError::Deadline(_) => {}
                    cloudkit_cli::ConnectGuardError::Inner(source) => {
                        eprintln!("Caused by:\n    {source}");
                    }
                }
                eprintln!("{}", cloudkit_cli::connect_failure_hint());
                std::process::exit(1);
            }
        },
        _ = tokio::signal::ctrl_c() => {
            println!("Interrupted while connecting to Telegram; exiting.");
            return Ok(None);
        }
    };
    Ok(Some(Arc::new(transport)))
}
