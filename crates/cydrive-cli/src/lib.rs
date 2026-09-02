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

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use cydrive_core::cache::CacheManager;
use cydrive_core::config::CyDriveConfig;
use cydrive_core::database::MetaDatabase;
use cydrive_core::transport::CloudTransport;
use cydrive_core::vfs::{Vfs, VfsConfig};
use cydrive_webdav::{CyDriveFs, WebDavServer};

/// Bytes per GB — cache capacity conversion (`cache_limit_gb`).
const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;

/// A running CyDrive stack: the WebDAV server, the VFS whose upload
/// queue backs it, and the drive letter auto-mounted at boot (if any).
/// Dropping the handle without [`RunHandle::shutdown`] leaves the
/// workers to die with the runtime; prefer an explicit shutdown.
pub struct RunHandle {
    server: WebDavServer,
    vfs: Arc<Vfs>,
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

    /// Graceful stop: the WebDAV server shuts down first (it stops
    /// accepting and in-flight requests drain), then the upload queue
    /// drains — every job enqueued before this call reaches a terminal
    /// state before the future resolves — and finally an auto-mounted
    /// drive is released; an unmount failure only warns, it must never
    /// block the exit.
    pub async fn shutdown(self) {
        self.server.shutdown().await;
        self.vfs.shutdown().await;
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
fn vfs_config(cfg: &CyDriveConfig) -> VfsConfig {
    VfsConfig {
        chunk_size_bytes: cfg.chunk_size_mb * 1024 * 1024,
        workers: 2,
        queue_capacity: 256,
        retry: Default::default(),
        encryption_password: cfg.encryption_password.clone(),
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
    let mounted_letter = mount_if_configured(cfg);
    Ok(RunHandle {
        server,
        vfs,
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
/// files. `CYDRIVE_*` environment overrides apply on top of whichever
/// file won (precedence env > file > defaults).
pub fn discover_config() -> Result<CyDriveConfig> {
    let toml_path = Path::new("config.toml");
    if toml_path.exists() {
        let cfg = CyDriveConfig::load_toml(toml_path)
            .with_context(|| format!("loading {}", toml_path.display()))?;
        return Ok(cfg.with_env_overrides());
    }
    let legacy_path = Path::new("config.json");
    if legacy_path.exists() {
        let cfg = CyDriveConfig::load_legacy_json(legacy_path)
            .with_context(|| format!("loading legacy {}", legacy_path.display()))?;
        return Ok(cfg.with_env_overrides());
    }
    anyhow::bail!(
        "no config found in the current directory: write a config.toml (or a legacy \
         Python config.json) with bot_token and chat_id, then run cydrive again"
    )
}
