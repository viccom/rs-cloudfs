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
//! authoritative index), doctor (offline diagnosis + platform checks),
//! setup (interactive first-time wizard; `--multi` writes the
//! multi-volume skeleton) and volumes (the multi-volume manifest
//! listing).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
#[cfg(feature = "telegram")]
use ck_telegram::transport::GrammersTransport;
use clap::{Parser, Subcommand};
use cloudkit_cli::{discover_config, discover_config_with_volumes, DiscoveredConfig, VolumeStatus};
// Driver-only run-flow glue moved with `dispatch_unified_backend_volume`
// into the lib (115 review batch): BaiduEndpoints and the K13
// ConfigTokenStore are assembled behind that seam now.
use cloudkit_core::config::{Backend, CyDriveConfig, MountBackend, VolumeConfig};
use cloudkit_core::logging::LogConfig;
#[cfg(feature = "telegram")]
use cloudkit_core::rel_path::RelPath;

/// CyDrive — Telegram as an unlimited cloud drive, served over WebDAV.
#[derive(Debug, Parser)]
#[command(name = "cydrive", version = version_line(), about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The `--version` payload (K32): the crate version plus the
/// compiled-in driver list — e.g. `0.10.0 (drivers: telegram, baidu,
/// local)`; a build with every driver feature off reports
/// `(drivers: none)`. Serves both `-V` and `--version` through clap's
/// `version` attribute. The `&'static str` return keeps clap's
/// non-`string`-feature `From<&'static str>` path; the once-built line
/// lives in the static.
fn version_line() -> &'static str {
    static LINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LINE.get_or_init(|| {
        format!(
            "{} (drivers: {})",
            env!("CARGO_PKG_VERSION"),
            cloudkit_cli::compiled_drivers()
        )
    })
    .as_str()
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
    /// Mount the drive: a drive letter on Windows (`net use`, or an
    /// in-process WinFsp mount when `mount_backend = "winfsp"` — the
    /// latter stays in the foreground until Ctrl+C), a directory on Linux
    /// (gio → davfs2).
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
    /// directory (Unix). An in-process WinFsp mount cannot be released
    /// from another process — this command says so instead of pretending
    /// (stop that process instead).
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
    Setup {
        /// Write the multi-volume skeleton instead of the wizard: a
        /// process-scoped config.toml plus one example volume file
        /// (refuses to overwrite an existing config).
        #[arg(long)]
        multi: bool,
    },
    /// List the configured volumes (multi-volume mode) with their
    /// backends, drive letters and metadata DB paths — configuration
    /// facts only, no running instance is contacted.
    Volumes,
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
        Command::Setup { multi } => setup_cmd(multi).await,
        Command::Volumes => volumes_cmd(),
    }
}

/// The run() gates shared by the data channel: without a bot token +
/// chat id the Telegram connect cannot succeed, so fail with the same
/// actionable message instead of a transport error. Telegram-only (the
/// data channel has no other backend), hence gated with the driver.
#[cfg(feature = "telegram")]
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
/// the probes and print the rendered report. Multi-volume mode (Phase
/// 2.5 / MV4) appends the per-volume db stats section — the reads are
/// exists-guarded and read-only, so the numbers exist whether or not a
/// process is running.
async fn status_cmd() -> Result<()> {
    match discover_config_with_volumes().context("config discovery failed")? {
        DiscoveredConfig::Single(cfg) => {
            let report = cloudkit_cli::collect_status(&cfg).await;
            println!(
                "{}",
                cloudkit_cli::render_status(
                    &report,
                    &cloudkit_cli::default_mount_url(&cfg),
                    cloudkit_cli::dashboard_url(&cfg),
                )
            );
        }
        DiscoveredConfig::Multi { process, volumes } => {
            // Process-level probes first (the same renderer over the
            // process config — the control file and ports are
            // process-level in multi-volume mode too, K25/K20).
            let report = cloudkit_cli::collect_status(&process).await;
            println!(
                "{}",
                cloudkit_cli::render_status(
                    &report,
                    &cloudkit_cli::default_mount_url(&process),
                    cloudkit_cli::dashboard_url(&process),
                )
            );
            println!();
            let rows = cloudkit_cli::volumes::collect_volume_stats(&volumes)?;
            println!("{}", cloudkit_cli::volumes::render_volume_stats(&rows));
            // RV2 (K48): the live instance's runtime volume table,
            // forwarded as a LIST over the control channel. An instance
            // that is not running keeps the config-only face above — the
            // existing fallback behavior is untouched (no control file =
            // nothing live to ask).
            if report.instance.is_some() {
                if let Ok(addr) = cloudkit_cli::control::read_control_addr(&process) {
                    match cloudkit_cli::control::send_command(addr, "LIST").await {
                        Ok(reply) => {
                            if let Some(section) =
                                cloudkit_cli::volumes::format_runtime_volumes_section(&reply)
                            {
                                println!();
                                print!("{section}");
                            }
                        }
                        Err(error) => {
                            println!();
                            println!(
                                "runtime volumes: the control channel did not answer ({error})"
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// `cydrive push`: upload a local file straight through the data channel
/// (no WebDAV / Web UI size limits), drain the queue, then report the
/// row's terminal state. A degraded upload keeps its local copy and
/// retries on the next run.
async fn push_cmd(path: PathBuf, dest: Option<String>) -> Result<()> {
    // The data channel is telegram-only: a binary without the driver
    // refuses up front (K31) instead of surfacing telegram-credential
    // gates first (FT1).
    #[cfg(not(feature = "telegram"))]
    {
        let _ = (&path, &dest);
        anyhow::bail!(
            "`cydrive push` needs the telegram driver: {}",
            cloudkit_cli::TELEGRAM_DRIVER_REQUIRED
        )
    }
    #[cfg(feature = "telegram")]
    {
        let cfg = discover_config().context("config discovery failed")?;
        require_configured(&cfg)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("the source path {} carries no file name", path.display()))?;
        let dest = dest.unwrap_or_else(|| format!("/{file_name}"));
        let dest = RelPath::new(&dest).with_context(|| {
            format!("invalid drive path {dest:?} (drive paths start with \"/\")")
        })?;

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
}

/// `cydrive pull`: hydrate a drive file (downloading from Telegram when
/// the local cache is cold) and copy it out to a local path.
async fn pull_cmd(path: String, out: PathBuf) -> Result<()> {
    // Telegram-only refusal up front — see `push_cmd` (K31, FT1).
    #[cfg(not(feature = "telegram"))]
    {
        let _ = (&path, &out);
        anyhow::bail!(
            "`cydrive pull` needs the telegram driver: {}",
            cloudkit_cli::TELEGRAM_DRIVER_REQUIRED
        )
    }
    #[cfg(feature = "telegram")]
    {
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
        // K40's decision, with the same shape as the boot's: a winfsp
        // request that cannot be honoured degrades visibly to the WebDAV
        // mapping (never a refusal), and the reason is printed first.
        let decision = cloudkit_cli::choose_mount_backend(
            cfg.mount_backend,
            &cloudkit_cli::winfsp_capability(),
        );
        match &decision {
            cloudkit_cli::MountBackendDecision::WebDav => {}
            cloudkit_cli::MountBackendDecision::WebDavFallback(reason) => {
                tracing::error!(backend = "winfsp", %reason, "the winfsp mount backend is \
                     unavailable for `cydrive mount`; falling back to the WebDAV drive \
                     mapping (K40)");
                println!("{}", cloudkit_cli::winfsp_fallback_notice(reason));
            }
            cloudkit_cli::MountBackendDecision::WinFsp => {
                return mount_cmd_winfsp(cfg, url, letter).await;
            }
        }
        let (letter, url) = cloudkit_cli::resolve_mount_params(&cfg, url, letter);
        let mounted = cloudkit_platform::windows::mount_drive(&letter, &url)
            .with_context(|| format!("mounting {url} at {letter}"))?;
        println!("CyDrive mounted at {mounted} -> {url}");
        Ok(())
    }
}

/// `cydrive mount` with the in-process WinFsp backend (Phase 3 / WF4):
/// build the configured volume's stack, mount it on its drive letter and
/// stay in the foreground until Ctrl+C, then unmount — rclone-mount
/// shaped, because an in-process mount lives exactly as long as the
/// process that owns it (there is no cross-process unmount to offer).
///
/// `--url` is refused: it names a WebDAV endpoint, and this path never
/// speaks WebDAV (the mount is a direct VFS adapter).
#[cfg(all(windows, feature = "winfsp"))]
async fn mount_cmd_winfsp(
    cfg: CyDriveConfig,
    url: Option<String>,
    letter: Option<String>,
) -> Result<()> {
    if url.is_some() {
        anyhow::bail!(
            "--url applies to the WebDAV drive mapping; `mount_backend = \"winfsp\"` mounts \
             the configured volume in-process (set mount_backend = \"webdav\" to map a URL)"
        );
    }
    // The run flow's double-start guard applies here too (RB3 / cli-H2):
    // this command builds a full stack against the instance's metadata db
    // — a boot over a LIVE instance would contend for the db lock and
    // double-write its caches, so it is refused the same actionable way.
    cloudkit_cli::control::ensure_not_running(&cfg).await?;
    let letter = letter.unwrap_or_else(|| cfg.drive_letter.clone());
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    let mut dispatch_options = cloudkit_cli::RunOptions::default();
    let Some(transport) =
        connect_single_volume_transport(&cfg, &cwd, &mut dispatch_options).await?
    else {
        return Ok(()); // Ctrl+C during the connect
    };
    let stack = cloudkit_cli::build_stack(&cfg, transport).await?;
    // Review L1 (RB4): the volume label is the NAME Explorer shows, not
    // the mount point — labeling the volume with its own drive letter
    // ("V:") told the operator nothing. Single-volume mode has no volume
    // name (that is a multi-volume `volumes_dir` concept, K21), so the
    // stable product name is the honest label; the letter stays visible
    // as the mount point in the banner.
    let label = "CyDrive".to_string();
    let vfs = Arc::clone(&stack.vfs);
    let rt = tokio::runtime::Handle::current();
    let mount_point = letter.clone();
    let mounted = tokio::task::spawn_blocking(move || {
        cloudkit_winfsp::mount::mount(vfs, rt, &mount_point, &label)
    })
    .await
    .context("the winfsp mount task failed to complete")?
    .with_context(|| format!("mounting the configured volume at {letter} through winfsp"))?;
    if let Err(error) = &mounted.readiness {
        println!("Warning: the drive did not appear yet ({error}); it may still come up.");
    }
    println!(
        "CyDrive is mounted at {} (winfsp, in-process). Press Ctrl+C to unmount and exit.",
        mounted.handle.mount_point()
    );
    let mut handle = mounted.handle;
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl+C")?;
    println!("Ctrl+C received; unmounting ...");
    let unmounted = tokio::task::spawn_blocking(move || handle.unmount())
        .await
        .context("the winfsp unmount task failed to complete")?;
    if let Err(error) = unmounted {
        println!("Unmount reported a problem: {error}");
    } else {
        println!("Unmounted.");
    }
    stack.shutdown().await;
    Ok(())
}

/// The no-feature twin: unreachable in practice (the decision above only
/// answers [`cloudkit_cli::MountBackendDecision::WinFsp`] when the feature
/// is compiled in) — kept as an actionable refusal so a future refactor
/// cannot silently ignore a winfsp request.
#[cfg(not(all(windows, feature = "winfsp")))]
async fn mount_cmd_winfsp(
    _cfg: CyDriveConfig,
    _url: Option<String>,
    _letter: Option<String>,
) -> Result<()> {
    anyhow::bail!("{}", cloudkit_cli::WINFSP_FEATURE_REQUIRED)
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
        // K40 / WF4 + RB3 (cli-H1): probe before answering. A *degraded*
        // winfsp boot created a real `net use` mapping (K40's visible
        // fallback) — that mapping is cross-process and ours, so unmount
        // releases it at the probe-verified letter. Only a winfsp
        // instance with nothing mapped on its drive URL gets the
        // in-process note; the WebDAV backend keeps the direct release
        // below. The probe is the status path's read-only `net use` scan
        // (`cloudkit_platform::current_mount_for`) — never a mutation.
        let probed = if cfg.mount_backend == MountBackend::Winfsp {
            cloudkit_platform::current_mount_for(&cloudkit_cli::default_mount_url(&cfg))
        } else {
            None
        };
        match cloudkit_cli::winfsp_unmount_step(cfg.mount_backend, probed) {
            Some(cloudkit_cli::WinfspUnmountStep::ReleaseDegradedMapping { letter }) => {
                cloudkit_platform::windows::unmount_drive(&letter)
                    .with_context(|| format!("unmounting {letter}"))?;
                println!(
                    "CyDrive unmounted from {letter} — the degraded winfsp boot's WebDAV \
                     mapping (mount_backend = \"winfsp\" could not be honoured, so the boot \
                     mapped the drive with `net use`; this release is that mapping's \
                     `net use /delete`)"
                );
                return Ok(());
            }
            Some(cloudkit_cli::WinfspUnmountStep::ExplainInProcess) => {
                let note = cloudkit_cli::winfsp_unmount_note(&cfg)
                    .expect("the winfsp backend always carries the in-process note");
                println!("{note}");
                return Ok(());
            }
            None => {} // the WebDAV backend: its cross-process mapping is released below
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
/// convention). Multi-volume mode rebuilds every rebuildable volume
/// from its own backend (telegram volumes skip — shadow index) —
/// UNLESS a multi-volume instance is live in this working directory
/// (P2): then one `REBUILD <name>` per non-telegram volume rides the
/// control channel and the live instance's background tasks do the
/// work against the very dbs it is serving (the offline pass would
/// race its writers for nothing).
async fn rebuild_cmd() -> Result<()> {
    match discover_config_with_volumes().context("config discovery failed")? {
        DiscoveredConfig::Single(cfg) => {
            let outcome = cloudkit_cli::run_rebuild_command(&cfg).await?;
            println!(
                "rebuild complete: {} file row(s), {} directory row(s) rebuilt from the {} backend",
                outcome.files,
                outcome.dirs,
                cfg.backend.as_str()
            );
        }
        DiscoveredConfig::Multi { process, volumes } => {
            // The P2 forward: a live instance owns these dbs — the
            // per-volume replies (acceptances and refusals alike) ride
            // the report; no instance keeps the offline pass untouched.
            if let Some(rows) = cloudkit_cli::rebuild_forward_live(&process, &volumes).await? {
                for (name, line) in rows {
                    println!("volume {name}: {line}");
                }
                println!(
                    "rebuild pass forwarded to the running instance — the results log there \
                     when each background rebuild finishes"
                );
                return Ok(());
            }
            let reports = cloudkit_cli::run_rebuild_multi(&volumes).await?;
            for (name, result) in reports {
                match result {
                    Ok(outcome) => println!(
                        "volume {name}: rebuilt {} file row(s), {} directory row(s)",
                        outcome.files, outcome.dirs
                    ),
                    Err(message) => println!("volume {name}: NOT rebuilt — {message}"),
                }
            }
            println!("multi-volume rebuild pass complete");
        }
    }
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
///
/// Multi-volume mode (Phase 2.5 / MV4): discovery routes the two shapes
/// — a `volumes_dir` config runs the multi-volume doctor (process-level
/// config + ports, then one check group per volume), anything else runs
/// the frozen single-volume body below unchanged.
async fn doctor_cmd() -> Result<()> {
    let discovered = discover_config_with_volumes();
    if let Ok(DiscoveredConfig::Multi { process, volumes }) = &discovered {
        let mut results = cloudkit_cli::doctor::run_doctor_multi(process, volumes);
        results.extend(cloudkit_cli::doctor::webclient_checks());
        // Phase 3 / WF4: the winfsp install probe rides the platform leg
        // (Warn when absent — the webdav default needs nothing).
        results.extend(cloudkit_cli::doctor::winfsp_checks());
        print!("{}", cloudkit_cli::doctor::render_report(&results));
        return Ok(());
    }
    let config_present = discovered.is_ok();
    let cfg = match discovered {
        Ok(DiscoveredConfig::Single(cfg)) => cfg,
        Ok(DiscoveredConfig::Multi { .. }) => unreachable!("handled above"),
        Err(_) => CyDriveConfig::default(),
    };
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
    // Phase 3 / WF4: the winfsp install probe (Warn when absent — never a
    // Fail: the webdav default needs nothing installed).
    results.extend(cloudkit_cli::doctor::winfsp_checks());
    match cfg.backend {
        cloudkit_core::config::Backend::Telegram => {
            results.push(cloudkit_cli::doctor::telegram_connectivity_check());
        }
        cloudkit_core::config::Backend::Baidu => {
            results.extend(cloudkit_cli::doctor::backend_checks(&cfg));
            // The live token probe needs the driver (FT2 / K31): a
            // binary without it skips the liveness leg instead of
            // reporting a fake Unreachable.
            #[cfg(feature = "baidu")]
            results.push(cloudkit_cli::doctor::baidu_connectivity_check(
                &cloudkit_cli::baidu_backend_probe(&cfg).await,
            ));
        }
        cloudkit_core::config::Backend::Local => {
            results.extend(cloudkit_cli::doctor::backend_checks(&cfg));
        }
        // Phase 4 / SF3: the sftp leg — the offline checks plus the live
        // connectivity probe (host-key fingerprint gate, auth, network).
        // A binary without the driver skips the dial-out leg (K31 shape,
        // the baidu rule): no fake Unreachable.
        cloudkit_core::config::Backend::Sftp => {
            results.extend(cloudkit_cli::doctor::backend_checks(&cfg));
            #[cfg(feature = "sftp")]
            results.push(cloudkit_cli::doctor::sftp_connectivity_check(
                &cloudkit_cli::sftp_backend_probe(&cfg).await,
            ));
        }
        // Phase 5 / 115-4: the pan115 leg — the offline checks plus the
        // live probe (user/info token liveness + quota, with the
        // re-scan / manual-token guidance on auth failures). A binary
        // without the driver skips the dial-out leg (K31 shape, the
        // baidu rule): no fake Unreachable.
        cloudkit_core::config::Backend::Pan115 => {
            results.extend(cloudkit_cli::doctor::backend_checks(&cfg));
            #[cfg(feature = "pan115")]
            results.push(cloudkit_cli::doctor::pan115_connectivity_check(
                &cloudkit_cli::pan115_backend_probe(&cfg).await,
            ));
        }
        // Phase 6 / 123-1 placeholder: the pan123 live token probe (the
        // user/info liveness leg + the QR-scan setup guidance) is a
        // 123-4 item; the offline checks run now (115-1's pan115 arm
        // was the same shape until 115-4 grew the dial-out leg).
        cloudkit_core::config::Backend::Pan123 => {
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
///
/// `--multi` (Phase 2.5 / MV4) skips the wizard entirely and writes the
/// multi-volume skeleton (no prompts — the multi-volume layout is files
/// the user edits directly).
async fn setup_cmd(multi: bool) -> Result<()> {
    if multi {
        print!(
            "{}",
            cloudkit_cli::setup::run_setup_multi().context("writing the multi-volume skeleton")?
        );
        return Ok(());
    }
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

/// `cydrive volumes` (Phase 2.5 / MV4): the read-only volume listing —
/// discover (the same face `run` uses), then either the manifest table
/// or the single-volume migration hint. Configuration facts only: the
/// command contacts no running instance, so it never guesses runtime
/// state (live status is `cydrive status` / the dashboard).
fn volumes_cmd() -> Result<()> {
    let discovered = discover_config_with_volumes().context("config discovery failed")?;
    println!(
        "{}",
        cloudkit_cli::volumes::volumes_report(&discovered).context("listing the volumes")?
    );
    Ok(())
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
    // Pretty/INFO on stdout; a parseable RUST_LOG overrides the level.
    // Installed BEFORE config discovery: discovery-time events (the RV0
    // disabled-volume skip note among them) must reach the log, not die
    // against a not-yet-installed subscriber.
    cloudkit_core::logging::init(&LogConfig::default()).context("initializing logging")?;
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
    // (Installed in `run` before discovery — see the note there.)
    let mut run_options = cloudkit_cli::RunOptions::default();
    let Some(transport) = connect_single_volume_transport(&cfg, &cwd, &mut run_options).await?
    else {
        return Ok(()); // Ctrl+C during the connect
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
/// deadline-bounded connect and the unified non-telegram dispatch
/// (baidu/local/sftp) — keyed
/// on each volume's settings with K21 volume-home state directories),
/// then the Volume Registry assembly and ONE process-level stop gate.
async fn run_multi_volume(
    process: CyDriveConfig,
    volumes: Vec<VolumeConfig>,
    _cwd: std::path::PathBuf,
) -> Result<()> {
    // (Logging is installed in `run` before discovery — see the note there.)
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
                // RV2 extraction: the arm below is shared verbatim with
                // the runtime ADD's dispatch (`dispatch_runtime_volume`)
                // so the two dispatch sites cannot drift.
                cloudkit_cli::dispatch_unified_backend_volume(
                    &spec,
                    &settings,
                    &home,
                    &mut run_options,
                )
                .await?
            }
        };
        injections.push((spec, run_options, transport));
    }

    // RV2 (K48): the runtime-volume command surface — the runtime ADD
    // assembles volumes through the same prelude and unified-backend arm
    // this boot just ran; its telegram arm is the runtime twin of
    // `connect_telegram_volume` (an answerable Err instead of the boot's
    // process exit — a control command must never kill the instance).
    let dispatch: cloudkit_cli::VolumeTransportDispatch = Arc::new(|spec: &VolumeConfig| {
        Box::pin(async move { dispatch_runtime_volume(spec).await })
    });
    let handle = cloudkit_cli::run_multi_with_transports_and_commands(
        &process,
        injections,
        cloudkit_cli::RuntimeVolumeCommands {
            dispatch: Some(dispatch),
            ..cloudkit_cli::RuntimeVolumeCommands::default()
        },
    )
    .await?;
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
    // MV3 / K24: the single dashboard port (the same line the
    // single-volume banner prints; absent when off or degraded).
    match handle.web_ui_addr() {
        Some(addr) => banner.push_str(&format!("  |  dashboard at http://{addr}")),
        None => {
            banner.push_str("  |  dashboard unavailable (disabled or bind failed; see the log)")
        }
    }
    // K40 / WF4: every mount names its backend — `V: winfsp`,
    // `Y: webdav`, `Z: webdav (fallback: …)` — so a degraded winfsp
    // request is visible in the door line, not just in the log.
    let mounts = handle.mounted_volumes();
    if !mounts.is_empty() {
        let listing = mounts
            .iter()
            .map(|mount| {
                format!(
                    "{} {} ({})",
                    mount.letter,
                    mount.volume,
                    mount.backend.label()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        banner.push_str(&format!("  |  mounted: {listing}"));
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

/// The single-volume transport dispatch shared by `run` and the in-process
/// winfsp `mount` path (Phase 3 / WF4 extraction; the body is the
/// pre-WF4 `run_single_volume` arm verbatim): telegram's deadline-bounded
/// connect raced against Ctrl+C, or the unified non-telegram dispatch
/// (baidu/local/sftp) which
/// also fills the dashboard identity into `options`. `Ok(None)` means
/// Ctrl+C won the connect race (the caller exits cleanly).
async fn connect_single_volume_transport(
    cfg: &CyDriveConfig,
    cwd: &std::path::Path,
    options: &mut cloudkit_cli::RunOptions,
) -> Result<Option<Arc<dyn cloudkit_core::transport::CloudTransport>>> {
    match cfg.backend {
        Backend::Telegram => {
            // The legacy arm, byte-for-byte: session glue → visible
            // progress line → deadline-bounded connect raced against
            // Ctrl+C → the failure hint on error.
            let transport = connect_telegram_volume(cfg, cwd).await?;
            Ok(transport)
        }
        backend => {
            println!(
                "Connecting to the {backend} backend ...",
                backend = backend.as_str()
            );
            let dispatched = cloudkit_cli::build_backend_transport(cfg).await?;
            // K12: the periodic sync task keys on the backend's own
            // identity (baidu's account uid), not on telegram creds.
            options.sync_namespace = Some(dispatched.sync_namespace_key());
            // Dashboard identity (web adapter): the volume label and the
            // boot quota snapshot exist only on the dispatched enum —
            // past this point the run flow sees the erased
            // CloudTransport face, which carries neither.
            options.web_volume = Some(dispatched.volume().to_string());
            options.web_quota = dispatched.web_quota_snapshot().await;
            Ok(Some(dispatched.clone_dyn()))
        }
    }
}

/// The telegram connect arm shared by both run flows (Phase 2.5 / MV1
/// extracted verbatim from the single-volume path): session glue →
/// visible progress line → deadline-bounded connect raced against Ctrl+C
/// → the failure hint on error. `state_base` is the session-state
/// directory — the process cwd for a single-volume boot, the volume's
/// home directory in multi-volume mode (K21). Returns `None` when Ctrl+C
/// won the race (the caller exits cleanly).
#[cfg(feature = "telegram")]
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

/// The no-driver twin (K31): the signature is identical so both run
/// flows' telegram arms stay compilable unchanged; a boot that reaches
/// the telegram connect in a binary built without the driver gets the
/// actionable rebuild message instead of a connect attempt.
#[cfg(not(feature = "telegram"))]
async fn connect_telegram_volume(
    cfg: &CyDriveConfig,
    state_base: &std::path::Path,
) -> Result<Option<Arc<dyn cloudkit_core::transport::CloudTransport>>> {
    let _ = (cfg, state_base);
    anyhow::bail!("{}", cloudkit_cli::TELEGRAM_DRIVER_REQUIRED)
}

// ------------------------------------------------ RV2: runtime ADD dispatch ---

/// The runtime ADD's transport dispatch (RV2 / K48): the same prelude
/// (K21 resolution + validate) and the same unified-backend arm the boot
/// loop runs; the telegram arm is the runtime twin of
/// [`connect_telegram_volume`] — the same deadline-bounded connect with
/// the failure hint, but an ANSWERABLE Err instead of the boot's
/// `process::exit(1)`, and no Ctrl+C race (a control command must never
/// kill the instance; the run flow's own Ctrl+C handling still owns
/// shutdown). `Ok(None)` cannot occur on this path (nothing interrupts
/// the connect but its own deadline).
async fn dispatch_runtime_volume(
    spec: &VolumeConfig,
) -> Result<
    Option<(
        cloudkit_cli::RunOptions,
        Arc<dyn cloudkit_core::transport::CloudTransport>,
    )>,
> {
    let home = cloudkit_cli::volume_home(spec)?;
    let settings = cloudkit_cli::resolve_volume_settings(spec)?;
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
            connect_telegram_volume_for_add(&settings, &home).await?
        }
        _ => {
            println!(
                "Connecting volume {name} to the {backend} backend ...",
                name = spec.name,
                backend = settings.backend.as_str()
            );
            cloudkit_cli::dispatch_unified_backend_volume(spec, &settings, &home, &mut run_options)
                .await?
        }
    };
    Ok(Some((run_options, transport)))
}

/// The runtime twin of [`connect_telegram_volume`]'s connect core (RV2):
/// deadline-bounded connect, the failure hint on error — but the error
/// RETURNS (the control command answers `ERR`) instead of exiting the
/// process, and no Ctrl+C arm (the run flow owns shutdown signals).
#[cfg(feature = "telegram")]
async fn connect_telegram_volume_for_add(
    settings: &CyDriveConfig,
    home: &std::path::Path,
) -> Result<Arc<dyn cloudkit_core::transport::CloudTransport>> {
    let transport_config = cloudkit_cli::transport_config_from(settings, home);
    let transport = cloudkit_cli::connect_with_deadline(
        GrammersTransport::connect(transport_config),
        cloudkit_cli::CONNECT_DEADLINE,
    )
    .await
    .context("connecting the Telegram transport")
    .context(cloudkit_cli::connect_failure_hint())?;
    Ok(Arc::new(transport))
}

/// The no-driver twin (K31 shape): identical signature, the actionable
/// rebuild message.
#[cfg(not(feature = "telegram"))]
async fn connect_telegram_volume_for_add(
    _settings: &CyDriveConfig,
    _home: &std::path::Path,
) -> Result<Arc<dyn cloudkit_core::transport::CloudTransport>> {
    anyhow::bail!("{}", cloudkit_cli::TELEGRAM_DRIVER_REQUIRED)
}
