//! Minimal run orchestration for the `cydrive` binary (unit C).
//!
//! Boot order — the Python `cydrive/cli.py` `run` baseline adapted to the
//! single-tokio-runtime Rust design (the thread topology is deliberately
//! not replicated; the observable ordering is):
//!
//! 1. open the metadata DB and the cache manager;
//! 2. assemble the VFS (upload queue workers spawn inside);
//! 3. re-enqueue pending uploads **before** WebDAV accepts traffic, so
//!    crash-staged uploads resume ahead of new client writes;
//! 4. serve WebDAV on `(webdav_host, webdav_port)`;
//! 5. on Windows with `auto_mount_drive`, map the configured drive letter
//!    to the server (unit D; a failed mount warns and the server lives on).
//!
//! [`RunHandle::shutdown`] mirrors the Python graceful exit with the
//! ordering flipped for safety: the WebDAV server stops first (in-flight
//! requests drain), then the upload queue drains to a terminal state,
//! and finally an auto-mounted drive is released (failure only warns —
//! it must not block the exit). The production entry point
//! (`src/main.rs`) injects a `GrammersTransport`; tests inject a
//! `MockTransport` through the same seam, [`run_with_transport`].

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cydrive_core::cache::CacheManager;
use cydrive_core::config::CyDriveConfig;
use cydrive_core::credentials::{
    CredentialStore, InMemoryStore, BOT_TOKEN, ENCRYPTION_PASSWORD, SERVICE,
};
use cydrive_core::database::MetaDatabase;
use cydrive_core::inbound::{spawn_inbound_worker, InboundWorkerHandle};
use cydrive_core::transport::CloudTransport;
use cydrive_core::vfs::{Vfs, VfsConfig};
use cydrive_web::WebUiServer;
use cydrive_webdav::{CyDriveFs, WebDavServer};

mod keyring_store;

pub mod doctor;
pub mod setup;

pub use keyring_store::KeyringStore;

/// Bytes per GB — cache capacity conversion (`cache_limit_gb`).
const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;

/// Bytes per MiB — the stats size math (`total_bytes / (1024 * 1024)`),
/// the same constant the bot `/stats` reply uses.
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// Human-readable cloud-storage size with the bot `/stats` semantics
/// (`bot.rs`): a true-division MB value, the GB branch opening at
/// `size_gb >= 1.0` (not `>= 1024` MB), two decimals.
fn format_storage_size(total_bytes: i64) -> String {
    let size_mb = total_bytes as f64 / BYTES_PER_MB;
    let size_gb = size_mb / 1024.0;
    if size_gb >= 1.0 {
        format!("{size_gb:.2} GB")
    } else {
        format!("{size_mb:.2} MB")
    }
}

/// Renders the `stats` subcommand's report: a comfy-table carrying the
/// Python `/stats` rows (Total Files / Total Folders / Total Cloud
/// Storage / Synced Files / Pending Uploads) plus the Drive and URL
/// extras the web dashboard's `/api/stats` also exposes. Pure — the CLI
/// feeds it `MetaDatabase::get_stats` output and prints verbatim.
pub fn format_stats_report(
    stats: &cydrive_core::database::Stats,
    drive_letter: &str,
    webdav_url: &str,
) -> String {
    use comfy_table::{Cell, Table};

    let mut table = Table::new();
    table.set_header(vec![Cell::new("Metric"), Cell::new("Value")]);
    table.add_row(vec![Cell::new("Total Files"), Cell::new(stats.total_files)]);
    table.add_row(vec![
        Cell::new("Total Folders"),
        Cell::new(stats.total_dirs),
    ]);
    table.add_row(vec![
        Cell::new("Total Cloud Storage"),
        Cell::new(format_storage_size(stats.total_bytes)),
    ]);
    table.add_row(vec![
        Cell::new("Synced Files"),
        Cell::new(stats.uploaded_files),
    ]);
    table.add_row(vec![
        Cell::new("Pending Uploads"),
        Cell::new(stats.pending_uploads),
    ]);
    table.add_row(vec![Cell::new("Drive"), Cell::new(drive_letter)]);
    table.add_row(vec![Cell::new("URL"), Cell::new(webdav_url)]);
    table.to_string()
}

/// A running CyDrive stack: the WebDAV server, the web dashboard, the
/// VFS whose upload queue backs both, and the drive letter
/// auto-mounted at boot (if any). Dropping the handle without
/// [`RunHandle::shutdown`] leaves the workers to die with the runtime;
/// prefer an explicit shutdown.
pub struct RunHandle {
    server: WebDavServer,
    /// The web dashboard (unit M4), served when `enable_web_ui` is on.
    /// [`RunHandle::shutdown`] stops it after the WebDAV server (its
    /// in-flight uploads finish before the queue drains).
    web_ui: Option<cydrive_web::WebUiServer>,
    vfs: Arc<Vfs>,
    /// The inbound indexing worker consuming `transport.incoming()`
    /// (files sent to the bot land in the VFS as metadata-only rows).
    /// [`RunHandle::shutdown`] joins it last, after the queue has
    /// drained, so no index write races the shutdown.
    inbound: InboundWorkerHandle,
    /// The drive letter the boot-time auto-mount actually claimed
    /// (`None` when unmounted: non-Windows, `auto_mount_drive` off, or a
    /// failed mount that only warned). [`RunHandle::shutdown`] releases
    /// exactly this letter.
    pub mounted_letter: Option<String>,
}

impl RunHandle {
    /// The WebDAV listener's actual bound address (a `:0` config port
    /// resolves to the real ephemeral port).
    pub fn local_addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    /// The dashboard listener's actual bound address when
    /// `enable_web_ui` was on (`None` otherwise; a `:0` config port
    /// resolves to the real ephemeral port, same semantics as
    /// [`RunHandle::local_addr`]).
    pub fn web_ui_local_addr(&self) -> Option<SocketAddr> {
        self.web_ui.as_ref().map(WebUiServer::local_addr)
    }

    /// Graceful stop: the WebDAV server shuts down first (it stops
    /// accepting and in-flight requests drain), then the web dashboard
    /// stops the same way, then the upload queue drains — every job
    /// enqueued before this call reaches a terminal state before the
    /// future resolves — and finally an auto-mounted drive is released;
    /// an unmount failure only warns, it must never block the exit.
    pub async fn shutdown(self) {
        self.server.shutdown().await;
        if let Some(web_ui) = &self.web_ui {
            web_ui.shutdown().await;
        }
        self.vfs.shutdown().await;
        self.inbound.shutdown().await;
        if let Some(letter) = self.mounted_letter {
            if let Err(e) = cydrive_platform::windows::unmount_drive(&letter) {
                tracing::warn!(
                    letter = %letter,
                    error = %e,
                    "unmounting the auto-mounted drive failed; continuing the shutdown"
                );
            }
        }
    }
}

/// Maps the application config onto the VFS knobs: chunk split from
/// `chunk_size_mb`, two queue workers, a 256-slot queue, the default
/// retry policy and the optional encryption password.
///
/// The password reaches the VFS only while `enable_encryption` is on —
/// the Python AND semantics (`telegram_client.py:167`:
/// `enable_encryption and encryption_password`): a stale password in the
/// config with the flag off must not silently flip uploads to encrypted.
///
/// Public for the pure-mapping tests (frozen API addition, trait-evolution
/// adjudication 2026-09-02); the mapping itself is part of the upload
/// encryption contract.
pub fn vfs_config(cfg: &CyDriveConfig) -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: cfg.chunk_size_mb * 1024 * 1024,
        workers: 2,
        queue_capacity: 256,
        retry: Default::default(),
        encryption_password: if cfg.enable_encryption {
            cfg.encryption_password.clone()
        } else {
            None
        },
        hydrate_timeout: Duration::from_secs(180),
    }
}

/// Boots the full stack with an injected transport (tests pass a
/// `MockTransport`; the `run` subcommand passes a `GrammersTransport`).
///
/// Start order: DB + cache → Vfs → pending requeue → WebDAV serve. The
/// returned handle answers [`RunHandle::local_addr`] with the bound
/// address (production binds `127.0.0.1:8080` via the config; a `:0`
/// port binds an ephemeral one — validation of non-zero ports is the
/// caller's concern, which is why tests may pass `0` here).
pub async fn run_with_transport(
    cfg: &CyDriveConfig,
    transport: Arc<dyn CloudTransport>,
) -> Result<RunHandle> {
    let db = Arc::new(
        MetaDatabase::open(Path::new(&cfg.db_path))
            .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?,
    );
    let cache_root = Path::new(&cfg.cache_path).to_path_buf();
    let cache_limit = cfg.cache_limit_gb * BYTES_PER_GB;

    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(cache_root.clone(), cache_limit),
        Arc::clone(&transport),
        vfs_config(cfg),
    ));

    // Boot-time recovery before serving: crash-staged uploads re-enter
    // the queue ahead of any client traffic.
    let requeued = vfs
        .requeue_pending()
        .await
        .context("re-enqueueing pending uploads")?;
    tracing::info!(pending = requeued, "requeued pending uploads");

    // Same boot segment as the requeue, before the server accepts
    // traffic: inbound remote files (media sent to the bot) begin
    // indexing into the VFS before the first client request can arrive
    // (Python parity: the Telegram handlers run alongside the WebDAV
    // server from boot).
    let inbound = spawn_inbound_worker(
        Arc::clone(&vfs),
        Arc::clone(&transport),
        cfg.drive_letter.clone(),
    );
    tracing::info!("inbound indexing worker spawned");

    // A second cache handle over the same root backs the FS adapter's
    // path math and cache-copy housekeeping (same construction as the
    // webdav crate's own tests).
    let fs = CyDriveFs::new(
        Arc::clone(&vfs),
        Arc::clone(&db),
        CacheManager::new(cache_root, cache_limit),
    );
    let host: IpAddr = cfg
        .webdav_host
        .parse()
        .with_context(|| format!("parsing webdav_host {:?}", cfg.webdav_host))?;
    let server = WebDavServer::serve(fs, SocketAddr::new(host, cfg.webdav_port))
        .await
        .context("starting the WebDAV server")?;
    tracing::info!(addr = %server.local_addr(), "WebDAV listening");

    // Dashboard (unit M4): same boot segment as the inbound worker —
    // up before the first client request can arrive (Python parity:
    // the aiohttp dashboard runs alongside WebDAV from boot). The
    // stats extras map straight off the config; `is_configured`
    // mirrors Python's bot-token check.
    let web_ui = if cfg.enable_web_ui {
        let host: IpAddr = cfg
            .web_ui_host
            .parse()
            .with_context(|| format!("parsing web_ui_host {:?}", cfg.web_ui_host))?;
        let ui_cfg = cydrive_web::WebUiConfig {
            drive_letter: cfg.drive_letter.clone(),
            webdav_url: default_mount_url(cfg),
            chat_id: cfg.chat_id,
            is_configured: cfg.is_configured(),
        };
        let web_ui = WebUiServer::serve(
            Arc::clone(&vfs),
            ui_cfg,
            SocketAddr::new(host, cfg.web_ui_port),
        )
        .await
        .context("starting the web dashboard")?;
        tracing::info!(addr = %web_ui.local_addr(), "web UI listening");
        Some(web_ui)
    } else {
        tracing::info!("web UI disabled (enable_web_ui = false)");
        None
    };
    let mounted_letter = mount_if_configured(cfg);
    Ok(RunHandle {
        server,
        web_ui,
        vfs,
        inbound,
        mounted_letter,
    })
}

/// Auto-mount step (unit D): on Windows with `auto_mount_drive`, map the
/// best available drive letter to the config's WebDAV URL. Non-Windows
/// platforms and disabled configs skip with an info line; a failed mount
/// only warns — the server stays reachable at its URL either way.
fn mount_if_configured(cfg: &CyDriveConfig) -> Option<String> {
    if !cfg.auto_mount_drive {
        tracing::info!("auto_mount_drive is off; skipping the drive mapping");
        return None;
    }
    if !cfg!(windows) {
        tracing::info!("drive mapping is Windows-only; skipping");
        return None;
    }
    let url = default_mount_url(cfg);
    match cydrive_platform::windows::mount_drive(&cfg.drive_letter, &url) {
        Ok(letter) => {
            tracing::info!(letter = %letter, url = %url, "drive mapped");
            Some(letter)
        }
        Err(error) => {
            tracing::warn!(%error, url = %url, "auto-mount failed; WebDAV stays at its URL");
            None
        }
    }
}

/// Glues the canonical mount URL from the config's WebDAV host/port.
pub fn default_mount_url(cfg: &CyDriveConfig) -> String {
    format!("http://{}:{}", cfg.webdav_host, cfg.webdav_port)
}

/// Resolves the `mount` subcommand's arguments: an explicit `--url` /
/// `--letter` wins, otherwise the config's `drive_letter` and the glued
/// default URL apply. Letters pass through verbatim; normalization is
/// [`cydrive_platform::windows::mount_drive`]'s concern.
pub fn resolve_mount_params(
    cfg: &CyDriveConfig,
    url: Option<String>,
    letter: Option<String>,
) -> (String, String) {
    (
        letter.unwrap_or_else(|| cfg.drive_letter.clone()),
        url.unwrap_or_else(|| default_mount_url(cfg)),
    )
}

/// Resolves the `unmount` subcommand's `--letter`: the flag first, the
/// config's `drive_letter` second.
pub fn resolve_unmount_letter(cfg: &CyDriveConfig, letter: Option<String>) -> String {
    letter.unwrap_or_else(|| cfg.drive_letter.clone())
}

/// Discovers the configuration in the current working directory.
///
/// Discovery order: `./config.toml` (canonical) → `./config.json`
/// (legacy Python format) → [`anyhow::Error`] with guidance naming both
/// files. The M5 precedence chain applies on top of whichever file won:
/// **`CYDRIVE_*` env > file > OS credential store** — the store
/// backfills only secret fields the file left empty. The production OS
/// store is [`KeyringStore`]; when it is unavailable this degrades to an
/// empty [`InMemoryStore`] with a warning instead of refusing to start
/// (the config file alone still carries everything needed).
pub fn discover_config() -> Result<CyDriveConfig> {
    let store: Box<dyn CredentialStore> = match KeyringStore::new() {
        Ok(store) => Box::new(store),
        Err(error) => {
            tracing::warn!(
                %error,
                "OS credential store unavailable; continuing without keyring backfill"
            );
            Box::new(InMemoryStore::new())
        }
    };
    discover_config_with_store(store.as_ref())
}

/// [`discover_config`] with the credential store injected (tests pass an
/// [`InMemoryStore`]; production passes a [`KeyringStore`]). The store
/// leg only ever fills empty file values, then `CYDRIVE_*` environment
/// overrides apply last, keeping env on top of everything.
pub fn discover_config_with_store(store: &dyn CredentialStore) -> Result<CyDriveConfig> {
    let toml_path = Path::new("config.toml");
    if toml_path.exists() {
        let cfg = CyDriveConfig::load_toml(toml_path)
            .with_context(|| format!("loading {}", toml_path.display()))?;
        return Ok(cfg.with_credential_backfill(store).with_env_overrides());
    }
    let legacy_path = Path::new("config.json");
    if legacy_path.exists() {
        let cfg = CyDriveConfig::load_legacy_json(legacy_path)
            .with_context(|| format!("loading legacy {}", legacy_path.display()))?;
        return Ok(cfg.with_credential_backfill(store).with_env_overrides());
    }
    anyhow::bail!(
        "no config found in the current directory: write a config.toml (or a legacy \
         Python config.json) with bot_token and chat_id, then run cydrive again"
    )
}

/// The `migrate` subcommand body (M5): import a legacy Python
/// installation into the Rust layout. Steps:
///
/// 1. pick the source — `./config.json` (legacy Python) wins over an
///    existing `./config.toml`; neither → actionable error;
/// 2. move the secrets into `store` (bot_token when non-empty;
///    encryption_password when `enable_encryption` carries one) — a
///    store failure aborts the migration, because silently keeping the
///    secrets in plaintext would defeat the whole point;
/// 3. adopt Python-default data artifacts found in the cwd
///    (`cydrive_meta.db`, `Telegram_Cache`) by pointing the config at
///    them — zero-copy, nothing is moved or rewritten;
/// 4. write a scrubbed `config.toml`
///    ([`CyDriveConfig::save_toml_scrubbed`]): secrets stay in the
///    store, every other field round-trips unchanged;
/// 5. verify the metadata DB opens and answers `get_stats` — a failure
///    becomes a warning line in the report, never an abort.
///
/// Idempotent: re-running overwrites the store entries and rewrites the
/// toml without complaining. The legacy `config.json` is **never**
/// deleted (irreversible; the report tells the user to remove it
/// manually), and Telegram sessions cannot be carried over — the first
/// `run` asks for a one-time sign-in.
///
/// Returns the human-readable report (the CLI prints it verbatim; tests
/// assert on it).
pub fn run_migrate(store: &dyn CredentialStore) -> Result<String> {
    let legacy_path = Path::new("config.json");
    let toml_path = Path::new("config.toml");
    let (mut cfg, used_legacy) = if legacy_path.exists() {
        let cfg = CyDriveConfig::load_legacy_json(legacy_path)
            .with_context(|| format!("loading legacy {}", legacy_path.display()))?;
        (cfg, true)
    } else if toml_path.exists() {
        let cfg = CyDriveConfig::load_toml(toml_path)
            .with_context(|| format!("loading {}", toml_path.display()))?;
        (cfg, false)
    } else {
        anyhow::bail!(
            "nothing to migrate: neither ./config.json (legacy Python) nor ./config.toml \
             exists in the current directory; run cydrive migrate in the directory that \
             holds your Python CyDrive installation (or write a config.toml first)"
        )
    };

    // 2. Secrets into the store, before the scrubbed write forgets them.
    let mut moved: Vec<&str> = Vec::new();
    if !cfg.bot_token.is_empty() {
        store
            .set(BOT_TOKEN, &cfg.bot_token)
            .context("storing bot_token in the OS credential store")?;
        moved.push(BOT_TOKEN);
    }
    if cfg.enable_encryption && cfg.encryption_password.is_some() {
        if let Some(password) = cfg.encryption_password.as_deref() {
            store
                .set(ENCRYPTION_PASSWORD, password)
                .context("storing encryption_password in the OS credential store")?;
        }
        moved.push(ENCRYPTION_PASSWORD);
    }

    // 3. Zero-copy adoption of Python-default data artifacts in the cwd.
    let adopted_db = Path::new("cydrive_meta.db").exists();
    if adopted_db {
        cfg.db_path = "./cydrive_meta.db".to_string();
    }
    let adopted_cache = Path::new("Telegram_Cache").exists();
    if adopted_cache {
        cfg.cache_path = "./Telegram_Cache".to_string();
    }

    // 4. Scrubbed canonical write.
    cfg.save_toml_scrubbed(toml_path)
        .context("writing the scrubbed config.toml")?;

    // 5. Verify the DB when its file exists; failures only warn.
    let mut db_line = if adopted_db {
        format!(
            "- metadata database: adopted {} from the Python installation",
            cfg.db_path
        )
    } else {
        format!(
            "- metadata database: {} (kept from the source config)",
            cfg.db_path
        )
    };
    if Path::new(&cfg.db_path).exists() {
        match MetaDatabase::open(Path::new(&cfg.db_path)).and_then(|db| db.get_stats()) {
            Ok(stats) => db_line.push_str(&format!(
                "; verified readable ({} files, {} bytes, {} pending uploads)",
                stats.total_files, stats.total_bytes, stats.pending_uploads
            )),
            Err(error) => {
                db_line.push_str(&format!("; WARNING: could not verify readability: {error}"))
            }
        }
    } else {
        db_line.push_str(" (no existing file; it will be created on first run)");
    }

    let mut report = String::new();
    let _ = writeln!(report, "CyDrive migration report:");
    if used_legacy {
        let _ = writeln!(
            report,
            "- source: legacy config.json (kept in place — delete it yourself once the \
             migration checks out)"
        );
    } else {
        let _ = writeln!(
            report,
            "- source: config.toml (already canonical; re-scrubbed)"
        );
    }
    if moved.is_empty() {
        let _ = writeln!(
            report,
            "- credentials: none found in the source config; the OS store was left untouched"
        );
    } else {
        let _ = writeln!(
            report,
            "- credentials moved to the OS credential store (service \"{SERVICE}\"): {}",
            moved.join(", ")
        );
    }
    let _ = writeln!(report, "{db_line}");
    if adopted_cache {
        let _ = writeln!(
            report,
            "- local cache: adopted ./Telegram_Cache from the Python installation"
        );
    }
    let _ = writeln!(
        report,
        "- wrote config.toml (secrets scrubbed; they return automatically via the \
         credential store at startup)"
    );
    let _ = writeln!(
        report,
        "- Telegram session files cannot be migrated; the first `cydrive run` will ask \
         for a one-time sign-in"
    );
    Ok(report)
}
