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
//! 5. bind the loopback control channel behind `cydrive stop` (an
//!    optional component — a bind failure only warns and the stack runs
//!    on without it);
//! 6. with `auto_mount_drive`, Windows maps the configured drive letter
//!    to the server (unit D) and Unix mounts the auto-mount target
//!    through the gio→davfs2 chain (status plan C5; either way a failed
//!    mount warns and the server lives on).
//!
//! The three shutdown sources — Ctrl+C, SIGTERM (unix) and the control
//! channel's STOP — funnel into one [`ShutdownWatch`] gate, and a
//! spawned stop task owns the graceful sequence so it runs exactly once
//! no matter which source (or how many) fired.
//!
//! [`RunHandle::shutdown`] mirrors the Python graceful exit with the
//! ordering flipped for safety: the WebDAV server stops first (in-flight
//! requests drain), then the upload queue drains to a terminal state,
//! the inbound worker joins, the control port file is removed, and
//! finally an auto-mounted drive is released (failure only warns —
//! it must not block the exit). The production entry point
//! (`src/main.rs`) injects a `GrammersTransport`; tests inject a
//! `MockTransport` through the same seam, [`run_with_transport`].

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ck_telegram::config::{
    TransportConfig, DEFAULT_API_HASH, DEFAULT_API_ID, DEFAULT_SESSION_STEM,
};
use ck_telegram::transport::GrammersTransport;
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::{Backend, CyDriveConfig, VolumeConfig};
use cloudkit_core::credentials::{
    CredentialStore, InMemoryStore, BOT_TOKEN, ENCRYPTION_PASSWORD, SERVICE,
};
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::inbound::spawn_inbound_worker;
use cloudkit_core::rebuild;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::sync::{
    namespace_key, namespace_key_for, sync_once, NamespaceIdentity, SyncOutcome,
};
use cloudkit_core::transport::Capabilities;
use cloudkit_core::transport::CloudTransport;
use cloudkit_core::vfs::{Vfs, VfsConfig, VfsError};
use cloudkit_storage::StorageDriver;
use cloudkit_web::WebUiServer;
use cloudkit_webdav::{CyDriveFs, WebDavServer};
use tokio::net::TcpStream;
use tokio::sync::Notify;

mod keyring_store;
mod signals;

pub mod control;
pub mod doctor;
pub mod setup;
pub mod sync_client;
pub mod volumes;

pub use keyring_store::KeyringStore;
pub use signals::sigterm;

/// Failure modes of [`connect_with_deadline`].
#[derive(Debug, thiserror::Error)]
pub enum ConnectGuardError<E> {
    /// The connect future did not finish within the budget.
    #[error("connect did not finish within {0:?}")]
    Deadline(std::time::Duration),
    /// The connect future finished with this error before the deadline.
    #[error(transparent)]
    Inner(E),
}

/// Bounds a (potentially blocked) connect future by a deadline so the CLI
/// can surface a human-readable diagnosis instead of hanging silently.
///
/// Real-machine regression (2026-09-02): a network-blocked Telegram
/// connect showed zero output for tens of seconds and a Ctrl+C in that
/// window hard-killed the process; every connect must therefore be
/// visibly bounded.
pub async fn connect_with_deadline<F, T, E>(
    fut: F,
    deadline: std::time::Duration,
) -> Result<T, ConnectGuardError<E>>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    match tokio::time::timeout(deadline, fut).await {
        Ok(result) => result.map_err(ConnectGuardError::Inner),
        Err(_elapsed) => Err(ConnectGuardError::Deadline(deadline)),
    }
}

/// Human-readable diagnosis printed when the Telegram connect fails:
/// the dominant real-world causes are an unreachable network (region
/// blocking; a SOCKS5 `proxy_url` usually solves it) and an invalid
/// token.
pub fn connect_failure_hint() -> String {
    "Cannot reach Telegram. Common causes:\n  \
     1) Telegram servers unreachable from this network (timeout / os error 10060) —\n\
     \x20    set a SOCKS5 proxy in config.toml (`proxy_url`, e.g. \"socks5://127.0.0.1:7890\")\n\
     \x20    or use a system-wide VPN/TUN;\n  \
     2) invalid bot token — re-run `cydrive setup`;\n  \
     3) no internet — check the connection and retry."
        .to_string()
}

/// The default connect budget shared by the `run` flow's connect guard
/// and [`connect_stack`]: 90s — long enough for a slow first sign-in,
/// short enough that a dead network surfaces [`connect_failure_hint`]
/// instead of a silent hang.
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(90);

/// Bytes per GB — cache capacity conversion (`cache_limit_gb`).
const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;

/// Bytes per MiB — the stats size math (`total_bytes / (1024 * 1024)`),
/// the same constant the bot `/stats` reply uses.
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// Human-readable cloud-storage size with the bot `/stats` semantics
/// (`bot.rs`): a true-division MB value, the GB branch opening at
/// `size_gb >= 1.0` (not `>= 1024` MB), two decimals. Public for the
/// `push` handler's terminal-state report (same formatting contract).
pub fn format_storage_size(total_bytes: i64) -> String {
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
    stats: &cloudkit_core::database::Stats,
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

/// The unified stop gate the run flow's shutdown sources funnel through
/// (service-lifecycle plan, contract C3): Ctrl+C and SIGTERM are selected
/// alongside it in `run`'s shutdown wait, the control channel's STOP
/// fires it directly, and [`RunHandle::shutdown`] fires it too — one
/// gate, however many sources.
///
/// Triggering is idempotent and latched: once fired, every current and
/// future [`ShutdownWatch::wait`] resolves immediately, so a second STOP
/// arriving while the graceful drain is already running is a harmless
/// no-op instead of a wedge.
pub struct ShutdownWatch {
    tx: tokio::sync::watch::Sender<bool>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl ShutdownWatch {
    /// A fresh, unfired gate.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self { tx, rx }
    }

    /// Fires the gate. Any number of triggers — from any source, in any
    /// order — leave it in the same fired state; there is no way back to
    /// blocking.
    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }

    /// Resolves once the gate has fired (immediately when it already
    /// has). Every waiter clones the shared value's view at call time,
    /// so wait-then-trigger and trigger-then-wait both resolve. A gate
    /// whose every [`ShutdownWatch`] has been dropped also releases its
    /// waiters — nobody is left to fire it.
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A running CyDrive stack: the WebDAV server, the web dashboard, the
/// VFS whose upload queue backs both, and the drive letter / mount
/// point auto-mounted at boot (if any). The graceful stop sequence
/// itself is owned by an internal stop task parked on the shared
/// [`ShutdownWatch`] gate, so it runs exactly once no matter which
/// shutdown source (or how many) fired. Dropping the handle without
/// [`RunHandle::shutdown`] leaves the workers to die with the runtime;
/// prefer an explicit shutdown.
pub struct RunHandle {
    webdav_addr: SocketAddr,
    web_ui_addr: Option<SocketAddr>,
    watch: Arc<ShutdownWatch>,
    /// The single runner of the graceful stop sequence (spawned inside
    /// [`run_with_transport`]): it waits on the stop gate and owns the
    /// shutdown ordering. [`RunHandle::shutdown`] fires the gate and
    /// joins this task; dropping the handle detaches it.
    stop_task: tokio::task::JoinHandle<()>,
    /// The drive letter the boot-time auto-mount actually claimed
    /// (`None` when unmounted: non-Windows, `auto_mount_drive` off, or a
    /// failed mount that only warned). The stop sequence releases
    /// exactly this letter.
    pub mounted_letter: Option<String>,
    /// The Unix mount point the boot-time auto-mount actually claimed
    /// (`None` when unmounted: non-unix, `auto_mount_drive` off, or a
    /// failed mount that only warned) — the Unix analog of
    /// [`RunHandle::mounted_letter`] (status plan C5). The stop sequence
    /// releases exactly this path.
    pub mounted_point: Option<PathBuf>,
    /// The periodic metadata-sync task (`None` when `sync_url` is unset,
    /// or the credentials to derive the namespace were missing at boot).
    /// Shutdown aborts it — see [`RunHandle::shutdown`] for why abort is
    /// the safe choice here.
    sync_task: Option<tokio::task::JoinHandle<()>>,
}

impl RunHandle {
    /// The WebDAV listener's actual bound address (a `:0` config port
    /// resolves to the real ephemeral port).
    pub fn local_addr(&self) -> SocketAddr {
        self.webdav_addr
    }

    /// The dashboard listener's actual bound address when
    /// `enable_web_ui` was on (`None` otherwise; a `:0` config port
    /// resolves to the real ephemeral port, same semantics as
    /// [`RunHandle::local_addr`]).
    pub fn web_ui_local_addr(&self) -> Option<SocketAddr> {
        self.web_ui_addr
    }

    /// One arm of the run flow's shutdown wait: resolves when the shared
    /// stop gate has fired — `run_with_transport` wires the control
    /// channel's STOP to it, and `run` selects it alongside Ctrl+C and
    /// SIGTERM, so any one of the three sources ends the wait.
    pub async fn wait_for_stop_request(&self) {
        self.watch.wait().await;
    }

    /// Graceful stop: the WebDAV server shuts down first (it stops
    /// accepting and in-flight requests drain), then the web dashboard
    /// stops the same way, then the upload queue drains — every job
    /// enqueued before this call reaches a terminal state before the
    /// future resolves — then the inbound worker joins, the control port
    /// file is removed, and finally an auto-mounted drive (Windows letter
    /// or Unix mount point) is released; an unmount failure only warns,
    /// it must never block the exit.
    ///
    /// The sequence itself lives in the internal stop task; this method
    /// fires the gate (a no-op when a source already did) and waits for
    /// that one run to finish, so the sequence never executes twice.
    pub async fn shutdown(self) {
        let Self {
            watch,
            stop_task,
            sync_task,
            ..
        } = self;
        watch.trigger();
        // Abort, not a cooperative drain: a first-full-push pass can run
        // for minutes, and an abort takes effect only at an await point —
        // rusqlite statements complete atomically and the sync engine is
        // idempotent for re-runs (own rows die at the pull idempotency
        // gate, a half-pushed batch simply re-pushes), so nothing is
        // corrupted and the exit stays prompt.
        if let Some(task) = sync_task {
            task.abort();
            if let Err(error) = task.await {
                if !error.is_cancelled() {
                    tracing::warn!(
                        %error,
                        "joining the periodic sync task failed after aborting it"
                    );
                }
            }
        }
        if let Err(error) = stop_task.await {
            tracing::warn!(
                %error,
                "joining the stop task failed after the shutdown sequence"
            );
        }
    }
}

/// Maps the application config onto the VFS knobs: chunk split from
/// `chunk_size_mb`, queue/hydrate tuning from the tier-1 keys
/// (`upload_workers` / `queue_capacity` / `hydrate_timeout_secs`,
/// contract C7), the default retry policy and the optional encryption
/// password.
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
        workers: cfg.upload_workers as usize,
        queue_capacity: cfg.queue_capacity as usize,
        retry: Default::default(),
        encryption_password: if cfg.enable_encryption {
            cfg.encryption_password.clone()
        } else {
            None
        },
        // Batch E / E-4: the container scheme for new encrypted uploads.
        // Unlike the password it rides along unconditionally — it only
        // takes effect on rows the password actually flags as encrypted,
        // and the read path keys on the per-row scheme, never this field.
        encryption_scheme: cfg.encryption_scheme,
        hydrate_timeout: Duration::from_secs(cfg.hydrate_timeout_secs),
    }
}

/// One-line transport capability declaration for the boot banner (R-5 /
/// interfaces §1 / logging §2: one-shot lifecycle info). Every bit shows,
/// on or off, in a fixed order — the consumer degrade warnings elsewhere
/// (inbound worker not started, bot commands dropped) point back at this
/// line for diagnosis.
///
/// Public for the pure-formatting tests (same frozen-API adjudication as
/// [`vfs_config`]).
pub fn transport_capabilities_line(caps: &Capabilities) -> String {
    format!(
        "range_read={}, resume={}, multipart={}, server_side_move={}, \
         rapid_upload={}, authoritative_index={}, change_feed={}, \
         inbound={}, chat={}, remote_delete={}",
        caps.range_read,
        caps.resume,
        caps.multipart,
        caps.server_side_move,
        caps.rapid_upload,
        caps.authoritative_index,
        caps.change_feed,
        caps.inbound,
        caps.chat,
        caps.remote_delete
    )
}

/// Boots the full stack with an injected transport (tests pass a
/// `MockTransport`; the `run` subcommand passes a `GrammersTransport`
/// for the telegram backend or the dispatched baidu/local transport).
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
    run_with_transport_options(cfg, transport, RunOptions::default()).await
}

/// Per-boot options the backend dispatch contributes (B3b 段二b): the
/// pre-derived sync namespace for backends whose identity is NOT the
/// telegram bot-token pair (baidu: the connected volume `baidu:<uid>`;
/// [`RunOptions::default`] keeps `None` = the legacy telegram
/// derivation, byte-identical for every existing caller), plus the
/// dashboard identity pieces that only exist on the dispatched enum
/// (web adapter).
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// The sync namespace key (`None` = derive from
    /// `bot_token`/`chat_id` — the frozen telegram path).
    pub sync_namespace: Option<String>,
    /// The dispatched volume identity for the dashboard
    /// (`baidu:<uid>` / `local:<hash>`; `None` = telegram — the
    /// CloudTransport face has no volume, none is invented).
    pub web_volume: Option<String>,
    /// The dashboard storage card's boot quota snapshot (`None` =
    /// telegram/local, no quota concept — or the informational read
    /// failed, which only degrades the card to "unlimited").
    pub web_quota: Option<cloudkit_web::QuotaSnapshot>,
}

/// The per-volume core every boot shares (single-volume and multi-volume
/// alike): the metadata db, the cache-backed VFS with its upload queue,
/// the boot-time pending requeue and the inbound indexing worker — the
/// first segments of the module's boot order, in exactly the
/// single-volume order. [`run_with_transport_options`] consumes this
/// verbatim; [`run_multi_with_transports`] builds one per volume.
struct VolumeCore {
    /// The metadata db (terminal-state reads; feeds periodic sync).
    db: Arc<MetaDatabase>,
    /// The VFS whose upload queue backs the volume.
    vfs: Arc<Vfs>,
    /// The cache root (the periodic sync task's own handle needs it).
    cache_root: PathBuf,
    /// The cache capacity in bytes (`cache_limit_gb` converted).
    cache_limit: u64,
    /// The inbound indexing worker handle (the stop sequence joins it).
    inbound: cloudkit_core::inbound::InboundWorkerHandle,
}

/// Assembles one volume's db + cache + VFS (+ queue workers) and runs the
/// boot-time pending requeue, then spawns the inbound indexing worker.
/// Extracted verbatim from [`run_with_transport_options`] (Phase 2.5 /
/// MV1) so the single-volume path and the per-volume multi-volume path
/// cannot drift.
async fn build_volume_core(
    cfg: &CyDriveConfig,
    transport: Arc<dyn CloudTransport>,
) -> Result<VolumeCore> {
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
    Ok(VolumeCore {
        db,
        vfs,
        cache_root,
        cache_limit,
        inbound,
    })
}

/// [`run_with_transport`] with the backend-derived sync namespace
/// injected: a baidu boot passes the dispatched transport's
/// [`BackendTransport::sync_namespace_key`] so the periodic task keys
/// on the account uid, not on (absent) telegram credentials.
pub async fn run_with_transport_options(
    cfg: &CyDriveConfig,
    transport: Arc<dyn CloudTransport>,
    options: RunOptions,
) -> Result<RunHandle> {
    let VolumeCore {
        db,
        vfs,
        cache_root,
        cache_limit,
        inbound,
    } = build_volume_core(cfg, Arc::clone(&transport)).await?;

    // A second cache handle over the same root backs the FS adapter's
    // path math and cache-copy housekeeping (same construction as the
    // webdav crate's own tests). `cache_root` stays alive for the
    // periodic sync task's own handle below.
    let fs = CyDriveFs::new(
        Arc::clone(&vfs),
        Arc::clone(&db),
        CacheManager::new(cache_root.clone(), cache_limit),
    );
    let host: IpAddr = cfg
        .webdav_host
        .parse()
        .with_context(|| format!("parsing webdav_host {:?}", cfg.webdav_host))?;
    // Capability banner next to the listening line (R-5): declare every
    // transport bit once at boot so the consumer degrade warnings (no
    // INBOUND → no inbound worker, no CHAT → commands dropped) have a
    // single diagnostic anchor. Ahead of the bind on purpose — a bind
    // failure still leaves the declaration in the log.
    tracing::info!(
        capabilities = %transport_capabilities_line(&transport.capabilities()),
        "transport capabilities declared"
    );
    let server = WebDavServer::serve(fs, SocketAddr::new(host, cfg.webdav_port))
        .await
        .context("starting the WebDAV server")?;
    tracing::info!(addr = %server.local_addr(), "WebDAV listening");

    // Dashboard (unit M4): same boot segment as the inbound worker —
    // up before the first client request can arrive (Python parity:
    // the aiohttp dashboard runs alongside WebDAV from boot). The
    // stats extras map straight off the config; `is_configured`
    // mirrors Python's bot-token check. The backend identity is the
    // web adapter's honesty contract: `backend` names what actually
    // booted (the config's stable spelling), `remote_delete` is the
    // transport face's OWN capability declaration — telegram false /
    // baidu, local true, never hardcoded here (R4) — and the volume /
    // quota snapshot ride in on the dispatch's RunOptions payload (the
    // erased CloudTransport face below has neither).
    let web_ui = if cfg.enable_web_ui {
        let host: IpAddr = cfg
            .web_ui_host
            .parse()
            .with_context(|| format!("parsing web_ui_host {:?}", cfg.web_ui_host))?;
        let ui_cfg = cloudkit_web::WebUiConfig {
            // Single-volume mode always reports the configured letter
            // (Some serializes identically to the pre-Option JSON).
            drive_letter: Some(cfg.drive_letter.clone()),
            webdav_url: default_mount_url(cfg),
            chat_id: cfg.chat_id,
            is_configured: cfg.is_configured(),
            backend: cfg.backend.as_str().to_string(),
            volume: options.web_volume.clone(),
            remote_delete: transport.capabilities().remote_delete,
            quota: options.web_quota.clone(),
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
    // The loopback control channel behind `cydrive stop` (contract C3):
    // an optional component — a bind failure only warns and the stack
    // runs on (Ctrl+C / SIGTERM still stop it); the STOP command fires
    // the unified gate below.
    let watch = Arc::new(ShutdownWatch::new());
    let control_file = match control::ControlServer::bind(cfg).await {
        Ok(server) => {
            let path = control::control_file_path(cfg);
            tracing::info!(addr = %server.local_addr(), "control channel listening");
            let gate = Arc::clone(&watch);
            tokio::spawn(async move {
                if let Err(error) = server.run(move || gate.trigger()).await {
                    tracing::warn!(%error, "the control channel accept loop ended");
                }
            });
            Some(path)
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "binding the control channel failed; `cydrive stop` cannot reach this \
                 instance (Ctrl+C / SIGTERM still work)"
            );
            None
        }
    };

    // Quasi-realtime metadata sync (sync-lite B4 + doorbell batch): with
    // sync_url set, a background task runs one pass immediately at boot,
    // then every sync_interval_secs (fallback) — and within seconds of
    // every local files-table change (the VFS doorbell) and every remote
    // push (the SSE doorbell). Any failure only warns — the served stack
    // is never affected; shutdown aborts the task (RunHandle::shutdown).
    let sync_task = spawn_periodic_sync(
        cfg,
        Arc::clone(&db),
        cache_root,
        cache_limit,
        Arc::clone(&watch),
        vfs.sync_notifier(),
        options.sync_namespace,
    );

    let (mounted_letter, mounted_point) = mount_if_configured(cfg);
    let webdav_addr = server.local_addr();
    let web_ui_addr = web_ui.as_ref().map(WebUiServer::local_addr);
    let unmount_letter = mounted_letter.clone();
    let unmount_point = mounted_point.clone();
    // The one runner of the graceful stop sequence: parked on the stop
    // gate, it executes the shutdown ordering exactly once for any
    // number of fires (control STOP, `RunHandle::shutdown`, or several
    // of them racing).
    let gate = Arc::clone(&watch);
    let stop_task = tokio::spawn(async move {
        watch.wait().await;
        server.shutdown().await;
        if let Some(web_ui) = &web_ui {
            web_ui.shutdown().await;
        }
        vfs.shutdown().await;
        inbound.shutdown().await;
        // The control port file is this instance's runtime artifact:
        // remove it just before the unmount so a `stop` racing the exit
        // never finds a file that will never answer again (a NotFound
        // means another path already cleaned up — silent).
        if let Some(path) = &control_file {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        "removing the control file failed; continuing the shutdown"
                    );
                }
            }
        }
        if let Some(letter) = unmount_letter {
            if let Err(e) = cloudkit_platform::windows::unmount_drive(&letter) {
                tracing::warn!(
                    letter = %letter,
                    error = %e,
                    "unmounting the auto-mounted drive failed; continuing the shutdown"
                );
            }
        }
        // The Unix auto-mount's release (status plan C5): same warn-only
        // semantics — a not-mounted or busy (EBUSY) mount point must never
        // block the exit. Both platform calls above/below compile
        // everywhere through the stubs, and each claim only ever exists
        // on its own platform, so exactly one arm can run.
        if let Some(point) = unmount_point {
            if let Err(e) = cloudkit_platform::linux::unmount_drive(&point) {
                tracing::warn!(
                    point = %point.display(),
                    error = %e,
                    "unmounting the auto-mounted directory failed; continuing the shutdown"
                );
            }
        }
    });
    Ok(RunHandle {
        webdav_addr,
        web_ui_addr,
        watch: gate,
        stop_task,
        mounted_letter,
        mounted_point,
        sync_task,
    })
}

// ------------------------------------------- Phase 2.5 / MV1: volume registry ---

/// The per-volume state machine (K22): a volume is `Starting` while the
/// assembly loop builds it, `Running` once its core is up, or `Failed`
/// with the assembly error's summary — never silently absent (the PCFS
/// anti-lesson: a failed volume must be visible, not swallowed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeStatus {
    /// The assembly loop is still building the volume.
    Starting,
    /// The volume's db/VFS/queue/inbound worker are up.
    Running,
    /// The volume failed to assemble; `reason` carries the error chain's
    /// top context for the banner and (MV3) `/api/volumes`.
    Failed {
        /// The assembly failure's top-level error message.
        reason: String,
    },
}

impl VolumeStatus {
    /// The stable lowercase spelling for banners and status lines.
    pub fn as_str(&self) -> &'static str {
        match self {
            VolumeStatus::Starting => "starting",
            VolumeStatus::Running => "running",
            VolumeStatus::Failed { .. } => "failed",
        }
    }
}

/// One volume's registry entry (Phase 2.5 / MV1): the parsed spec plus,
/// for the volumes that assembled, the running pieces. A `Failed` volume
/// carries its reason instead — the enum shape makes the
/// status-pieces pairing unrepresentable when inconsistent.
pub enum VolumeRuntime {
    /// The volume assembled: spec + injected transport + its own VFS.
    Running {
        /// The volume's discovered spec (name, file, base dir, settings).
        spec: VolumeConfig,
        /// The volume's transport (the injected / dispatched face).
        transport: Arc<dyn CloudTransport>,
        /// The volume's own VFS (db reachable through `vfs.db()`).
        vfs: Arc<Vfs>,
    },
    /// The volume failed to assemble (K22): the spec plus the reason.
    Failed {
        /// The volume's discovered spec.
        spec: VolumeConfig,
        /// The assembly failure's top-level error message.
        reason: String,
    },
}

impl VolumeRuntime {
    /// The volume's name (the file stem — URL segment, mount label and
    /// dashboard tab all reuse it, K29).
    pub fn name(&self) -> &str {
        match self {
            VolumeRuntime::Running { spec, .. } | VolumeRuntime::Failed { spec, .. } => &spec.name,
        }
    }

    /// The volume's discovered spec.
    pub fn spec(&self) -> &VolumeConfig {
        match self {
            VolumeRuntime::Running { spec, .. } | VolumeRuntime::Failed { spec, .. } => spec,
        }
    }

    /// The volume's status (a snapshot; `Failed` clones its reason).
    pub fn status(&self) -> VolumeStatus {
        match self {
            VolumeRuntime::Running { .. } => VolumeStatus::Running,
            VolumeRuntime::Failed { reason, .. } => VolumeStatus::Failed {
                reason: reason.clone(),
            },
        }
    }

    /// The volume's VFS (`None` for a failed volume).
    pub fn vfs(&self) -> Option<&Arc<Vfs>> {
        match self {
            VolumeRuntime::Running { vfs, .. } => Some(vfs),
            VolumeRuntime::Failed { .. } => None,
        }
    }
}

impl std::fmt::Debug for VolumeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transport face is not Debug; the name/status pair is the
        // diagnostic surface the banner and the registry report need.
        f.debug_struct("VolumeRuntime")
            .field("name", &self.name())
            .field("status", &self.status())
            .finish()
    }
}

/// The process's volume set (Phase 2.5 / MV1): one [`VolumeRuntime`] per
/// discovered volume, in discovery (file-name) order. A thin registry by
/// design — lifecycle lives in [`MultiVolumeHandle`]'s stop task; this
/// type only answers "what volumes exist and how are they doing".
#[derive(Debug)]
pub struct VolumeRegistry {
    /// The volumes in stable discovery order.
    pub volumes: Vec<VolumeRuntime>,
}

impl VolumeRegistry {
    /// The (name, status) list for banners and (MV3) `/api/volumes`.
    pub fn status_list(&self) -> Vec<(String, VolumeStatus)> {
        self.volumes
            .iter()
            .map(|volume| (volume.name().to_string(), volume.status()))
            .collect()
    }

    /// `true` when not a single volume assembled (the boot's Err gate).
    pub fn all_failed(&self) -> bool {
        self.volumes
            .iter()
            .all(|volume| matches!(volume, VolumeRuntime::Failed { .. }))
    }
}

/// The per-volume home directory (K21): `<volumes_dir>/<name>/` — the
/// resolution base for every volume-relative path (db/cache/session/
/// baidu state). The base dir is absolutised against the process cwd so
/// volume-relative `local_root`s resolve to the absolute paths
/// [`CyDriveConfig::validate`] demands.
pub fn volume_home(spec: &VolumeConfig) -> Result<PathBuf> {
    let base = if spec.base_dir.is_absolute() {
        spec.base_dir.clone()
    } else {
        std::env::current_dir()
            .context("resolving the working directory")?
            .join(&spec.base_dir)
    };
    Ok(base.join(&spec.name))
}

/// Resolves one volume-relative path against the volume home (K21):
/// absolute paths pass through untouched; relative paths (including the
/// `./`-prefixed defaults) land inside the volume home. A leading `./`
/// is stripped so joined paths stay lexically clean.
fn resolve_volume_path(home: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let cleaned = raw.strip_prefix("./").unwrap_or(raw);
    home.join(cleaned)
}

/// Resolves a volume spec's path-carrying settings against its home
/// directory (K21) and creates the home directory when missing: db_path,
/// cache_path, local_root and baidu_root all rebase onto
/// `<volumes_dir>/<name>/` when the volume file leaves them relative —
/// the defaults keep their file names but land inside the volume home.
/// Idempotent: already-absolute paths pass through untouched, so the
/// production dispatch may resolve first and the assembly resolve again.
pub fn resolve_volume_settings(spec: &VolumeConfig) -> Result<CyDriveConfig> {
    let home = volume_home(spec)?;
    std::fs::create_dir_all(&home)
        .with_context(|| format!("creating the volume home directory {}", home.display()))?;
    let mut settings = spec.settings.clone();
    settings.db_path = resolve_volume_path(&home, &settings.db_path)
        .to_string_lossy()
        .into_owned();
    settings.cache_path = resolve_volume_path(&home, &settings.cache_path)
        .to_string_lossy()
        .into_owned();
    if let Some(root) = settings.local_root.as_deref() {
        settings.local_root = Some(
            resolve_volume_path(&home, root)
                .to_string_lossy()
                .into_owned(),
        );
    }
    settings.baidu_root = resolve_volume_path(&home, &settings.baidu_root)
        .to_string_lossy()
        .into_owned();
    Ok(settings)
}

/// The pieces of one running volume the aggregated stop task owns: each
/// volume's VFS drain and inbound worker join, released in the same
/// per-volume order the single-volume stop sequence uses.
struct VolumeStopUnit {
    vfs: Arc<Vfs>,
    inbound: cloudkit_core::inbound::InboundWorkerHandle,
}

/// The assembled product of [`run_multi_with_transports`]: the volume
/// registry (statuses for the banner and tests), the ONE stop gate every
/// volume and the process-level control channel funnel into, the single
/// multi-volume WebDAV listener (K20 — `None` when its bind degraded,
/// see [`MultiVolumeHandle::webdav_addr`]), the single multi-volume
/// dashboard (K24 / MV3 — `None` when off or degraded, see
/// [`MultiVolumeHandle::web_ui_addr`]), the drive letters actually
/// mounted (K27), the per-volume periodic sync tasks (aborted at
/// shutdown, same semantics as [`RunHandle::shutdown`]) and the stop
/// task that owns the aggregated graceful sequence.
pub struct MultiVolumeHandle {
    registry: VolumeRegistry,
    watch: Arc<ShutdownWatch>,
    stop_task: tokio::task::JoinHandle<()>,
    /// The per-volume periodic sync tasks (`None` entries = volumes
    /// without sync configured / failed volumes).
    sync_tasks: Vec<Option<tokio::task::JoinHandle<()>>>,
    /// The single WebDAV listener's bound address (`None` = the bind
    /// degraded; the volumes' data planes keep running, K22).
    webdav_addr: Option<SocketAddr>,
    /// The single multi-volume dashboard's bound address (`None` =
    /// `enable_web_ui` off, or the bind degraded per the same K22
    /// policy).
    web_ui_addr: Option<SocketAddr>,
    /// The drive letters the per-volume mounts actually claimed
    /// (possibly fewer than configured — a failed mount only warns).
    mounted_letters: Vec<String>,
}

impl std::fmt::Debug for MultiVolumeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The gate and the raw task handles are not Debug; the volume
        // list is the diagnostic surface `expect_err` contexts need.
        f.debug_struct("MultiVolumeHandle")
            .field("volumes", &self.volumes())
            .finish()
    }
}

impl MultiVolumeHandle {
    /// One volume's registry entry (name lookup).
    pub fn volume(&self, name: &str) -> Option<&VolumeRuntime> {
        self.registry.volumes.iter().find(|v| v.name() == name)
    }

    /// The (name, status) list — the K29 banner payload.
    pub fn volumes(&self) -> Vec<(String, VolumeStatus)> {
        self.registry.status_list()
    }

    /// The single multi-volume WebDAV listener's bound address (K20):
    /// `None` means the bind degraded (K22 — the volumes' data planes
    /// kept running; the reason is in the log).
    pub fn webdav_addr(&self) -> Option<SocketAddr> {
        self.webdav_addr
    }

    /// The single multi-volume dashboard's bound address (K24 / MV3):
    /// `None` means `enable_web_ui` was off or the bind degraded (K22 —
    /// the volumes keep running without the UI; the reason is in the
    /// log). A `:0` config port resolves to the real ephemeral one.
    pub fn web_ui_addr(&self) -> Option<SocketAddr> {
        self.web_ui_addr
    }

    /// The drive letters the per-volume mounts actually claimed (K27;
    /// empty when none did — volumes without an explicit
    /// `drive_letter`, `auto_mount_drive` off, non-Windows, or failed
    /// mounts that only warned).
    pub fn mounted_letters(&self) -> &[String] {
        &self.mounted_letters
    }

    /// One arm of the run flow's shutdown wait, the multi-volume analog
    /// of [`RunHandle::wait_for_stop_request`]: resolves when the shared
    /// stop gate fires (control STOP, `shutdown`, Ctrl+C in `run`).
    pub async fn wait_for_stop_request(&self) {
        self.watch.wait().await;
    }

    /// Graceful stop for every volume: fires the ONE gate (a no-op when
    /// a source already did), aborts each volume's periodic sync task
    /// (same abort-not-drain choice and rationale as
    /// [`RunHandle::shutdown`]; a non-cancelled join error there is a
    /// warn, not a stop-sequence failure), then joins the stop task,
    /// which drains every running volume's upload queue, joins its
    /// inbound worker and finally removes the process-level control
    /// file. The stop task's own join error propagates — a panicked
    /// stop sequence is a failed shutdown, not a clean one.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            watch,
            stop_task,
            sync_tasks,
            ..
        } = self;
        watch.trigger();
        for task in sync_tasks.into_iter().flatten() {
            task.abort();
            if let Err(error) = task.await {
                if !error.is_cancelled() {
                    tracing::warn!(
                        %error,
                        "joining a periodic sync task failed after aborting it"
                    );
                }
            }
        }
        stop_task
            .await
            .context("joining the aggregated stop sequence")
    }
}

/// Boots the multi-volume stack with the transports injected (Phase 2.5
/// / MV1's test seam; the production `run` dispatch builds the same
/// tuples per volume). One volume core per spec (db + cache + VFS +
/// requeue + inbound worker, K21-resolved paths), one periodic sync task
/// per configured volume (K26), ONE process-level stop gate plus control
/// channel (K25), and the single multi-volume WebDAV listener routing
/// `/vol/<name>/` to every RUNNING volume (K20 / MV2 — failed volumes
/// stay out of the route table; their status is the registry's to
/// report). Volumes with an explicit `drive_letter` mount through the
/// process WebDAV port (K27).
///
/// K22 failure policy: a volume that fails to assemble becomes
/// `Failed{reason}` in the registry (with a `tracing::error`) and the
/// loop continues — a broken volume must not take healthy siblings down
/// and must never be silent. Every volume failing returns `Err` (the
/// process-exits-nonzero semantics). A WebDAV or web-UI bind failure is
/// NOT a volume failure: it degrades with a `tracing::error` while the
/// volumes' data planes keep running.
///
/// MV3: with `enable_web_ui` the boot also binds the ONE process-level
/// dashboard (K24) serving the volume registry — per-volume tabs, the
/// `/api/volumes` listing and the K23 `?volume=` routing.
pub async fn run_multi_with_transports(
    process_cfg: &CyDriveConfig,
    volumes: Vec<(VolumeConfig, RunOptions, Arc<dyn CloudTransport>)>,
) -> Result<MultiVolumeHandle> {
    let watch = Arc::new(ShutdownWatch::new());
    let mut runtimes: Vec<VolumeRuntime> = Vec::new();
    let mut stop_units: Vec<VolumeStopUnit> = Vec::new();
    let mut sync_tasks: Vec<Option<tokio::task::JoinHandle<()>>> = Vec::new();
    let mut volume_fses: Vec<(String, CyDriveFs)> = Vec::new();
    let mut ui_entries: Vec<cloudkit_web::VolumeUiEntry> = Vec::new();

    for (spec, options, transport) in volumes {
        let name = spec.name.clone();
        // The dashboard identity pieces (K24), captured before the
        // transport moves into the assembly: the running arm reports
        // the dispatched truth (RunOptions' volume/quota payload plus
        // the transport's own capability bits — R4, never hardcoded);
        // the failed arm degrades to what the volume file declares.
        let capabilities = transport.capabilities();
        let ui_config = cloudkit_web::WebUiConfig {
            // Only an explicit drive_letter claim reaches the dashboard;
            // an unclaimed volume mounts nothing and must not report the
            // config-default letter as a phantom claim (the parsed
            // default "Y:" is a placeholder, not a mount).
            drive_letter: spec
                .explicit_drive_letter
                .then(|| spec.settings.drive_letter.clone()),
            webdav_url: volume_mount_url(process_cfg, &name),
            chat_id: spec.settings.chat_id,
            is_configured: spec.settings.is_configured(),
            backend: spec.settings.backend.as_str().to_string(),
            volume: options.web_volume.clone(),
            remote_delete: capabilities.remote_delete,
            quota: options.web_quota.clone(),
        };
        match build_volume_runtime(&spec, &options, transport, &watch).await {
            Ok((runtime, stop_unit, sync_task, fs)) => {
                tracing::info!(volume = %name, "volume is running");
                volume_fses.push((name.clone(), fs));
                ui_entries.push(cloudkit_web::VolumeUiEntry {
                    name: name.clone(),
                    status: cloudkit_web::VolumeUiStatus::Running,
                    config: ui_config,
                    vfs: Some(Arc::clone(
                        runtime.vfs().expect("a running volume carries its vfs"),
                    )),
                });
                runtimes.push(runtime);
                stop_units.push(stop_unit);
                sync_tasks.push(sync_task);
            }
            Err(error) => {
                tracing::error!(
                    volume = %name,
                    %error,
                    "assembling the volume failed; continuing with the remaining volumes (K22)"
                );
                ui_entries.push(cloudkit_web::VolumeUiEntry {
                    name: name.clone(),
                    status: cloudkit_web::VolumeUiStatus::Failed {
                        reason: error.to_string(),
                    },
                    config: ui_config,
                    vfs: None,
                });
                runtimes.push(VolumeRuntime::Failed {
                    spec,
                    reason: error.to_string(),
                });
                sync_tasks.push(None);
            }
        }
    }

    if runtimes.is_empty() {
        anyhow::bail!("multi-volume boot received no volumes to assemble");
    }
    let registry = VolumeRegistry { volumes: runtimes };
    if registry.all_failed() {
        anyhow::bail!(
            "every volume failed to assemble ({}): {} — fix the reported volume \
             configurations and run again",
            registry.volumes.len(),
            registry
                .status_list()
                .iter()
                .map(|(name, status)| format!("{name}={}", status.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // The ONE WebDAV listener (K20): `/vol/<name>/` routes to every
    // running volume. A bind failure degrades (K22, the same policy as
    // the control channel but with the bigger blast radius spelled
    // out): the volumes' data planes — queues, sync, inbound — keep
    // running; the WebDAV face is simply absent.
    let webdav_server = bind_multi_webdav(process_cfg, volume_fses).await;
    let webdav_addr = webdav_server.as_ref().map(WebDavServer::local_addr);

    // The ONE dashboard (K24 / MV3): a single process-level port serving
    // the volume registry (per-volume tabs, `/api/volumes` aggregate).
    // Same K22 degrade policy as the WebDAV bind: a failure only logs
    // an error — the volumes keep running without the UI.
    let web_ui = bind_multi_web_ui(process_cfg, ui_entries).await;
    let web_ui_addr = web_ui.as_ref().map(WebUiServer::local_addr);

    // Per-volume mounts (K27): only volumes that EXPLICITLY set a
    // drive_letter claim a mount (the parsed default letter is a
    // placeholder, not a claim); nothing mounts without the listener.
    let mount_claims = webdav_addr
        .map(|_| collect_mount_claims(&registry))
        .unwrap_or_default();
    let mounted_letters = mount_volumes_if_configured(process_cfg, &mount_claims);

    // The process-level control channel (K25): same file name and
    // cwd-anchored location as the single-volume mode (the process
    // config's default db_path anchors the file in the working
    // directory), same optional-component degrade.
    let control_file = match control::ControlServer::bind(process_cfg).await {
        Ok(server) => {
            let path = control::control_file_path(process_cfg);
            tracing::info!(addr = %server.local_addr(), "control channel listening");
            let gate = Arc::clone(&watch);
            tokio::spawn(async move {
                if let Err(error) = server.run(move || gate.trigger()).await {
                    tracing::warn!(%error, "the control channel accept loop ended");
                }
            });
            Some(path)
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "binding the control channel failed; `cydrive stop` cannot reach this \
                 instance (Ctrl+C / SIGTERM still work)"
            );
            None
        }
    };

    // The one runner of the aggregated graceful stop sequence: the
    // WebDAV listener stops first (in-flight requests drain), then the
    // dashboard drains the same way (the single-volume order), then per
    // volume (queue drain then inbound join), then the process-level
    // control file, then the mounted letters release — exactly once for
    // any number of gate fires.
    let gate = Arc::clone(&watch);
    let unmount_letters = mounted_letters.clone();
    let stop_task = tokio::spawn(async move {
        watch.wait().await;
        if let Some(server) = &webdav_server {
            server.shutdown().await;
        }
        if let Some(web_ui) = &web_ui {
            web_ui.shutdown().await;
        }
        for unit in stop_units {
            unit.vfs.shutdown().await;
            unit.inbound.shutdown().await;
        }
        if let Some(path) = &control_file {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        "removing the control file failed; continuing the shutdown"
                    );
                }
            }
        }
        for letter in &unmount_letters {
            if let Err(e) = cloudkit_platform::windows::unmount_drive(letter) {
                tracing::warn!(
                    letter = %letter,
                    error = %e,
                    "unmounting the volume's drive failed; continuing the shutdown"
                );
            }
        }
    });
    Ok(MultiVolumeHandle {
        registry,
        watch: gate,
        stop_task,
        sync_tasks,
        webdav_addr,
        web_ui_addr,
        mounted_letters,
    })
}

/// Binds the single multi-volume dashboard (K24 / MV3) or skips it
/// (`enable_web_ui` off) or degrades visibly (K22): an address-parse
/// or bind failure logs an error and returns `None` — the volumes keep
/// running, only the UI face is gone.
async fn bind_multi_web_ui(
    process_cfg: &CyDriveConfig,
    volumes: Vec<cloudkit_web::VolumeUiEntry>,
) -> Option<WebUiServer> {
    if !process_cfg.enable_web_ui {
        tracing::info!("web UI disabled (enable_web_ui = false)");
        return None;
    }
    let bind = match process_cfg.web_ui_host.parse::<IpAddr>() {
        Ok(host) => SocketAddr::new(host, process_cfg.web_ui_port),
        Err(error) => {
            tracing::error!(
                %error,
                host = %process_cfg.web_ui_host,
                "parsing web_ui_host failed; running the volumes without the web UI (K22 degrade)"
            );
            return None;
        }
    };
    match WebUiServer::serve_multi(volumes, bind).await {
        Ok(server) => {
            tracing::info!(addr = %server.local_addr(), "multi-volume web UI listening");
            Some(server)
        }
        Err(error) => {
            tracing::error!(
                %error,
                "binding the multi-volume web UI failed; the volumes keep running without \
                 the dashboard (K22 degrade)"
            );
            None
        }
    }
}

/// Binds the single multi-volume WebDAV listener (K20) or degrades
/// visibly (K22): an address-parse or bind failure logs an error and
/// returns `None` — the volumes keep running, only the WebDAV face is
/// gone. An empty volume set (theoretically unreachable behind the
/// all-failed gate) also skips the bind.
async fn bind_multi_webdav(
    process_cfg: &CyDriveConfig,
    volumes: Vec<(String, CyDriveFs)>,
) -> Option<WebDavServer> {
    if volumes.is_empty() {
        return None;
    }
    let bind = match process_cfg.webdav_host.parse::<IpAddr>() {
        Ok(host) => SocketAddr::new(host, process_cfg.webdav_port),
        Err(error) => {
            tracing::error!(
                %error,
                host = %process_cfg.webdav_host,
                "parsing webdav_host failed; running the volumes without the WebDAV \
                 server (K22 degrade)"
            );
            return None;
        }
    };
    match WebDavServer::serve_volumes(volumes, bind).await {
        Ok(server) => {
            tracing::info!(addr = %server.local_addr(), "multi-volume WebDAV listening");
            Some(server)
        }
        Err(error) => {
            tracing::error!(
                %error,
                "binding the multi-volume WebDAV server failed; the volumes' data planes \
                 keep running without WebDAV (K22 degrade)"
            );
            None
        }
    }
}

/// The mount claims (K27): every RUNNING volume whose volume file
/// explicitly set `drive_letter` (presence semantics — the parsed
/// default is a placeholder that claims nothing).
fn collect_mount_claims(registry: &VolumeRegistry) -> Vec<(String, String)> {
    registry
        .volumes
        .iter()
        .filter_map(|volume| match volume {
            VolumeRuntime::Running { spec, .. } if spec.explicit_drive_letter => {
                Some((spec.name.clone(), spec.settings.drive_letter.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Assembles one volume: resolves its settings against its home dir
/// (K21), declares the transport's capability line (R-5, the same
/// declaration the single-volume banner makes), builds the volume core
/// and spawns its periodic sync task when configured (K26 — volumes
/// without `sync_url` or without a sync-capable backend skip it, the
/// existing gating). Also returns the volume's [`CyDriveFs`] for the
/// single multi-volume WebDAV listener (same second-cache-handle
/// construction as the single-volume boot).
async fn build_volume_runtime(
    spec: &VolumeConfig,
    options: &RunOptions,
    transport: Arc<dyn CloudTransport>,
    watch: &Arc<ShutdownWatch>,
) -> Result<(
    VolumeRuntime,
    VolumeStopUnit,
    Option<tokio::task::JoinHandle<()>>,
    CyDriveFs,
)> {
    let settings = resolve_volume_settings(spec)?;
    // The capability banner per volume (R-5): same one-line declaration
    // as the single-volume boot, keyed by volume name.
    tracing::info!(
        volume = %spec.name,
        capabilities = %transport_capabilities_line(&transport.capabilities()),
        "transport capabilities declared"
    );
    let core = build_volume_core(&settings, Arc::clone(&transport)).await?;
    let sync_task = spawn_periodic_sync(
        &settings,
        Arc::clone(&core.db),
        core.cache_root.clone(),
        core.cache_limit,
        Arc::clone(watch),
        core.vfs.sync_notifier(),
        options.sync_namespace.clone(),
    );
    // The FS adapter's cache handle is path-math only (same root, same
    // construction as the single-volume boot).
    let fs = CyDriveFs::new(
        Arc::clone(&core.vfs),
        Arc::clone(&core.db),
        CacheManager::new(core.cache_root.clone(), core.cache_limit),
    );
    Ok((
        VolumeRuntime::Running {
            spec: spec.clone(),
            transport,
            vfs: Arc::clone(&core.vfs),
        },
        VolumeStopUnit {
            vfs: core.vfs,
            inbound: core.inbound,
        },
        sync_task,
        fs,
    ))
}

/// A one-shot CLI stack (the `push` / `pull` data channel, contract
/// C11): a connected transport + the db + cache + VFS — and nothing
/// else. Unlike [`run_with_transport`] this never starts the WebDAV
/// server, the dashboard, the inbound worker or the boot-time pending
/// requeue; the subcommand owns the whole lifecycle, and
/// [`Stack::shutdown`] drains the upload queue.
pub struct Stack {
    /// The metadata db (terminal-state reads after the drain).
    pub db: Arc<MetaDatabase>,
    /// The VFS whose upload queue backs the data channel.
    pub vfs: Arc<Vfs>,
    /// The connected Telegram transport behind the [`CloudTransport`]
    /// seam.
    pub transport: Arc<dyn CloudTransport>,
}

impl Stack {
    /// Drains the upload queue: every job enqueued before this call
    /// reaches a terminal state before the future resolves (`&self`, so
    /// the caller can still read `db` afterwards for a terminal-state
    /// report). The data channel runs no servers, so the queue is the
    /// entire shutdown.
    pub async fn shutdown(&self) {
        self.vfs.shutdown().await;
    }
}

/// Assembles the [`TransportConfig`] from the application config: the
/// built-in API constants (compat contract 1), the session file
/// `{DEFAULT_SESSION_STEM}.session` under the given base directory, and
/// the bot / chat / proxy credentials (review M2 DRY). Shared by the
/// `run` flow and the `push` / `pull` data channel so the two assembly
/// sites cannot drift. `cwd` is the session-state base directory
/// (Phase 2.5 / K21): single-volume callers pass the process cwd (the
/// frozen behaviour), the multi-volume dispatch passes the volume's
/// home directory so each volume owns its session file.
pub fn transport_config_from(cfg: &CyDriveConfig, cwd: &Path) -> TransportConfig {
    TransportConfig {
        api_id: DEFAULT_API_ID,
        api_hash: DEFAULT_API_HASH.to_owned(),
        bot_token: cfg.bot_token.clone(),
        chat_id: cfg.chat_id,
        session_path: cwd.join(format!("{DEFAULT_SESSION_STEM}.session")),
        proxy_url: cfg.proxy_url.clone(),
    }
}

/// Boots the [`Stack`] for the `push` / `pull` subcommands: mirror of
/// the `run` assembly (connect → db → cache → Vfs) minus everything a
/// one-shot transfer does not need (WebDAV, dashboard, inbound worker,
/// mount, pending requeue). The transport connect needs real Telegram
/// credentials, so tests exercise the library bodies
/// ([`push_file`] / [`pull_file`]) against a hand-assembled Vfs instead.
///
/// The connect segment is deadline-bounded by [`CONNECT_DEADLINE`]
/// (review H1): a silent proxy surfaces "connect did not finish
/// within ..." plus the [`connect_failure_hint`] diagnosis instead of
/// hanging the subcommand without output.
pub async fn connect_stack(cfg: &CyDriveConfig) -> Result<Stack> {
    connect_stack_with_deadline(cfg, CONNECT_DEADLINE).await
}

/// [`connect_stack`] with the connect deadline injected — the tests
/// shrink the budget to prove the guard wins; everything else about the
/// assembly is identical.
pub async fn connect_stack_with_deadline(cfg: &CyDriveConfig, deadline: Duration) -> Result<Stack> {
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    let transport_config = transport_config_from(cfg, &cwd);
    let transport: Arc<dyn CloudTransport> = Arc::new(
        connect_with_deadline(GrammersTransport::connect(transport_config), deadline)
            .await
            .context("connecting the Telegram transport")
            .context(connect_failure_hint())?,
    );

    let db = Arc::new(
        MetaDatabase::open(Path::new(&cfg.db_path))
            .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?,
    );
    let cache_limit = cfg.cache_limit_gb * BYTES_PER_GB;
    let vfs = Arc::new(Vfs::new(
        Arc::clone(&db),
        CacheManager::new(Path::new(&cfg.cache_path).to_path_buf(), cache_limit),
        Arc::clone(&transport),
        vfs_config(cfg),
    ));
    Ok(Stack { db, vfs, transport })
}

/// The `push` body (contract C11): stream `local` into the drive at
/// `dest`, creating the missing ancestor directory rows on the way (an
/// ancestor that already exists — file or directory — is simply kept).
/// Returns the pushed byte count; the caller owns the queue drain
/// ([`Stack::shutdown`]) and the terminal-state report.
pub async fn push_file(vfs: &Vfs, local: &Path, dest: &RelPath) -> Result<u64> {
    if dest.is_root() {
        anyhow::bail!("the push destination may not be the drive root");
    }
    // Gate on the source before any db write so a missing file leaves no
    // directory rows behind; the mtime keeps the source's timestamp (the
    // same UNIX-epoch conversion the WebDAV adapter's staged metadata
    // uses).
    let source_meta = tokio::fs::metadata(local)
        .await
        .with_context(|| format!("reading the source file {}", local.display()))?;
    // Gate directories before any db write: push has no recursive mode,
    // and without this gate the copy would fail late (bare io error)
    // leaving ancestor directory rows behind (review L).
    if source_meta.is_dir() {
        anyhow::bail!(
            "the source is a directory; push uploads single files: {}",
            local.display()
        );
    }
    let mtime = source_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());

    // Ancestors collected deepest-first, then created shallow-first so
    // every parent row exists before its children (`create_dir`'s
    // ParentMissing gate); an `Exists` answer means the row is already
    // there and is swallowed.
    let mut ancestors = Vec::new();
    let mut current = dest.parent();
    while let Some(dir) = current {
        if dir.is_root() {
            break;
        }
        current = dir.parent();
        ancestors.push(dir);
    }
    for dir in ancestors.iter().rev() {
        match vfs.create_dir(dir) {
            Ok(()) | Err(VfsError::Exists(_)) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating the ancestor directory {dir}"));
            }
        }
    }

    vfs.ingest_file(dest, local, mtime)
        .await
        .with_context(|| format!("staging {dest} into the upload queue"))
}

/// The `pull` body (contract C11): hydrate `rel` (downloading from
/// Telegram when the local cache is cold), then copy the hydrated file
/// out to `out` — an existing directory receives the rel path's
/// basename, any other `out` is the target file itself (overwritten
/// when present). Returns the path written.
pub async fn pull_file(vfs: &Vfs, rel: &RelPath, out: &Path) -> Result<PathBuf> {
    let hydrated = vfs
        .hydrate(rel)
        .await
        .with_context(|| format!("hydrating {rel}"))?;
    let target = if out.is_dir() {
        out.join(rel.name())
    } else {
        out.to_path_buf()
    };
    tokio::fs::copy(&hydrated, &target)
        .await
        .with_context(|| format!("copying {rel} to {}", target.display()))?;
    Ok(target)
}

/// The `cache stats` body (contract C11): db + cache manager over the
/// config paths alone, no transport. The db open doubles as the same
/// early gate as `stats` — an unreadable metadata db fails the command
/// instead of printing cache numbers against a broken drive. Prints the
/// cache root, used bytes and the configured limit.
pub fn cache_stats(cfg: &CyDriveConfig) -> Result<()> {
    MetaDatabase::open(Path::new(&cfg.db_path))
        .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?;
    let cache_limit = cfg.cache_limit_gb * BYTES_PER_GB;
    let cache = CacheManager::new(Path::new(&cfg.cache_path).to_path_buf(), cache_limit);
    println!("cache root: {}", cfg.cache_path);
    println!(
        "used:      {}",
        format_storage_size(cache.total_size() as i64)
    );
    println!("limit:     {}", format_storage_size(cache_limit as i64));
    Ok(())
}

/// The `cache clear` body (contract C11 + plan revision A1): db + cache
/// manager over the config paths alone, no transport. Deletes the
/// cached copies of **uploaded** files and clears their `is_cached`
/// flags via the shared pending-preserving path
/// (`cloudkit_core::vfs::clear_cache_preserving_pending`, the same one
/// behind `Vfs::cache_clear`): pending uploads (`is_uploaded = 0`) keep
/// both their local copy (for them it is the only copy of the bytes)
/// and their flag. Prints the freed bytes and the number of cleared
/// flags.
pub fn cache_clear_cmd(cfg: &CyDriveConfig) -> Result<()> {
    let db = MetaDatabase::open(Path::new(&cfg.db_path))
        .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?;
    let cache_limit = cfg.cache_limit_gb * BYTES_PER_GB;
    let cache = CacheManager::new(Path::new(&cfg.cache_path).to_path_buf(), cache_limit);

    // Plan revision A1: pending uploads survive the clear — the shared
    // core helper owns that semantics (review M1 single source).
    let before = cache.total_size();
    let cleared = cloudkit_core::vfs::clear_cache_preserving_pending(&db, &cache)
        .context("clearing the local cache")?;
    let freed = before - cache.total_size();
    println!(
        "cache cleared: freed {}, cleared {} is_cached flag(s)",
        format_storage_size(freed as i64),
        cleared
    );
    Ok(())
}

// ------------------------------------------------- sync (sync-lite B4) ---

/// Parses a candidate sync secret: unset, empty and whitespace-only all
/// mean "send none" (a value that trims to empty is no secret); a
/// non-empty value travels **verbatim** — a secret is byte-exact, never
/// trimmed or logged.
pub fn parse_sync_secret(value: Option<String>) -> Option<String> {
    value.filter(|secret| !secret.trim().is_empty())
}

/// Reads the optional family-level shared secret from the
/// [`sync_client::SYNC_SECRET_ENV`] variable (`None` when unset/empty).
pub fn sync_secret_from_env() -> Option<String> {
    parse_sync_secret(std::env::var(sync_client::SYNC_SECRET_ENV).ok())
}

/// Resolves the sync shared secret for **both** entry points (`cydrive
/// sync` and the `run` periodic task): env `CYDRIVE_SYNC_SECRET` >
/// config.toml `sync_secret` > `None`. A set-but-empty (or
/// whitespace-only) env value explicitly clears the config value — the
/// `CYDRIVE_SYNC_URL` precedent — and an empty/whitespace-only config
/// value reads as unset. Non-empty values pass through verbatim (a
/// secret is byte-exact, never trimmed) and are never logged; when a
/// secret is in play a `debug!` line names its *source* only.
pub fn resolve_sync_secret(cfg: &CyDriveConfig) -> Option<String> {
    // A set variable (even a clearing empty one) wins outright; only an
    // unset/unreadable variable falls through to the file value — the
    // same shape `with_env_overrides` gives every CYDRIVE_* key.
    if let Ok(value) = std::env::var(sync_client::SYNC_SECRET_ENV) {
        let secret = parse_sync_secret(Some(value));
        if secret.is_some() {
            tracing::debug!(
                "using the sync secret from the CYDRIVE_SYNC_SECRET environment variable"
            );
        }
        return secret;
    }
    let secret = parse_sync_secret(cfg.sync_secret.clone());
    if secret.is_some() {
        tracing::debug!("using the sync secret from config.toml (sync_secret key)");
    }
    secret
}

/// Renders one pass's counters for the `cydrive sync` output — the
/// labels are pinned by the CLI tests. `skipped_invalid` covers rows
/// dropped for an undecodable payload or an invalid row key (counted
/// and logged, never fatal).
pub fn render_sync_summary(outcome: &SyncOutcome) -> String {
    format!(
        "sync done: pulled {}, applied {}, pushed {}, tombstoned {}, skipped_ghost {}, \
         skipped_idempotent {}, skipped_invalid {}, pushed_tombstones {}",
        outcome.pulled,
        outcome.applied,
        outcome.pushed,
        outcome.tombstoned,
        outcome.skipped_ghost,
        outcome.skipped_idempotent,
        outcome.skipped_invalid,
        outcome.pushed_tombstones
    )
}

/// The `cydrive sync` body (sync-lite Batch B4): one manual pass against
/// the configured sync server. No transport stack — sync touches only
/// the metadata db and HTTP.
///
/// Gates (actionable errors, exit-non-zero via the anyhow error leaving
/// `main`): a missing `sync_url` names the config.toml key; missing
/// `bot_token`/`chat_id` name both halves of the namespace identity
/// (the token arrives through the existing discovery chain — env >
/// file > OS credential store — so `cfg.bot_token` is already resolved).
/// The db/cache assembly mirrors the `run` flow's opening segment
/// verbatim (same paths, same capacity math).
///
/// `secret` is the caller-resolved optional shared secret — production
/// (`cydrive sync` and the periodic task alike) reads
/// [`resolve_sync_secret`], the single env > config.toml chain; tests
/// inject.
pub async fn run_sync_command(cfg: &CyDriveConfig, secret: Option<&str>) -> Result<SyncOutcome> {
    let Some(sync_url) = cfg.sync_url.clone() else {
        anyhow::bail!(
            "sync is not configured: set the sync_url key in config.toml (or export \
             CYDRIVE_SYNC_URL) to your cydrive-sync-server address, e.g. \
             \"http://192.168.1.10:8290\""
        );
    };
    // K12：命名空间按后端推导——telegram 沿 bot_token/chat_id（历史形
    // 态逐字节不变）；baidu 连接驱动取卷身份 `baidu:<uid>`（与 run 装配
    // 的周期任务同一推导，main.rs 的 run_options.sync_namespace 先例）；
    // local 不参与 sync。
    let key = match cfg.backend {
        Backend::Telegram => {
            if cfg.bot_token.is_empty() || cfg.chat_id == 0 {
                anyhow::bail!(
                    "cydrive sync needs bot_token and chat_id to derive the sync namespace: set \
                     them in config.toml (bot_token may come from the OS credential store) and \
                     retry"
                );
            }
            namespace_key(&cfg.bot_token, &cfg.chat_id.to_string())
        }
        Backend::Baidu => build_backend_transport(cfg)
            .await
            .context("connecting the baidu backend to derive the sync namespace")?
            .sync_namespace_key(),
        Backend::Local => anyhow::bail!("{LOCAL_SYNC_UNSUPPORTED}"),
    };

    let db = MetaDatabase::open(Path::new(&cfg.db_path))
        .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?;
    let cache_limit = cfg.cache_limit_gb * BYTES_PER_GB;
    let cache = CacheManager::new(Path::new(&cfg.cache_path).to_path_buf(), cache_limit);

    let client_id = db
        .sync_client_id()
        .context("reading this instance's sync client id")?;
    let client = sync_client::HttpSyncClient::new(&sync_url, client_id);
    sync_once(&db, &cache, &client, &key, secret)
        .await
        .with_context(|| format!("sync pass against {sync_url} failed"))
}

// ------------------------------------------- rebuild subcommand (K11) ---

/// The canonical telegram rebuild refusal (Phase 2 / K11): telegram's
/// remote store is message-shaped — the [`CloudTransport`] face has no
/// list/stat, so there is no authoritative backend index to walk; this
/// db IS the index (the shadow index). Shared by the seam gate
/// ([`run_rebuild_with_driver`]) and the driver assembly
/// ([`build_driver`]) so the two cannot drift.
pub const TELEGRAM_REBUILD_REFUSAL: &str =
    "rebuild is not supported for the telegram backend: its remote store is message-shaped \
     (no list/index face to walk) — this db IS the authoritative index (the shadow index); \
     use `cydrive sync` to replicate it to another instance instead";

/// The `cydrive rebuild` body (Phase 2 / K11): bootstrap the instance
/// metadata db from the backend's authoritative index. Assembles the
/// driver from the config's `backend` key ([`build_driver`]), then runs
/// the shared gate + walk against the instance db (`db_path`, same
/// discovery as every other subcommand).
pub async fn run_rebuild_command(cfg: &CyDriveConfig) -> Result<rebuild::RebuildOutcome> {
    let driver = build_driver(cfg).await?;
    run_rebuild_with_driver(cfg, driver.as_ref()).await
}

/// [`run_rebuild_command`] with the driver injected — the test seam
/// (tests seed a `MockStorageDriver`; production feeds the
/// backend-key assembly). Gates in order, all before any backend
/// traffic: config validity, the telegram shadow-index refusal, the
/// K11 plaintext-only gate; then the db open and the recursive walk.
pub async fn run_rebuild_with_driver(
    cfg: &CyDriveConfig,
    driver: &dyn StorageDriver,
) -> Result<rebuild::RebuildOutcome> {
    cfg.validate().context("invalid configuration")?;
    if cfg.backend == Backend::Telegram {
        anyhow::bail!("{TELEGRAM_REBUILD_REFUSAL}");
    }
    // No context wrapper: RebuildError::EncryptedInstance's Display IS
    // the actionable message (sync guidance) — a context would bury it.
    rebuild::ensure_plaintext_instance(cfg)?;
    let db = MetaDatabase::open(Path::new(&cfg.db_path))
        .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?;
    rebuild::rebuild_from_backend(driver, &db, &cloudkit_storage::RelPath::root())
        .await
        .context("rebuilding the index from the backend")
}

// ------------------------------- backend dispatch (B3b 段二b unit 5) ---

/// The baidu endpoint set the dispatch assembles [`ck_baidu::BaiduParams`]
/// with. Defaults are the production constants; tests inject a loopback
/// mock (`build_backend_transport_with`).
#[derive(Debug, Clone)]
pub struct BaiduEndpoints {
    /// xpan API base (`pan.baidu.com`).
    pub api_base: String,
    /// OAuth base (`openapi.baidu.com`).
    pub oauth_base: String,
    /// superfile2 PCS base (`None` = the production `d.pcs.baidu.com`).
    pub pcs_base: Option<String>,
}

impl Default for BaiduEndpoints {
    fn default() -> Self {
        BaiduEndpoints {
            api_base: ck_baidu::DEFAULT_API_BASE.to_string(),
            oauth_base: ck_baidu::DEFAULT_OAUTH_BASE.to_string(),
            pcs_base: None,
        }
    }
}

/// Assembles the [`BaiduParams`] for a config (the single mapping the
/// transport dispatch and the rebuild driver assembly share — the two
/// cannot drift): the four K14 credential keys, the root, the endpoint
/// set, the K13 persistence callback and the instance state directory.
/// `state_dir` is the baidu upload-session base (Phase 2.5 / K21): the
/// single-volume boot passes the cwd (`"."`), the multi-volume dispatch
/// passes the volume's home directory — public so the per-volume
/// path-resolution stays pinned by tests.
pub fn baidu_params(
    cfg: &CyDriveConfig,
    endpoints: &BaiduEndpoints,
    token_store: Option<Arc<dyn ck_baidu::TokenStore>>,
    state_dir: &Path,
) -> ck_baidu::BaiduParams {
    ck_baidu::BaiduParams {
        app_key: cfg.baidu_app_key.clone().unwrap_or_default(),
        app_secret: cfg.baidu_app_secret.clone().unwrap_or_default(),
        access_token: cfg.baidu_access_token.clone(),
        refresh_token: cfg.baidu_refresh_token.clone(),
        root: cfg.baidu_root.clone(),
        api_base: endpoints.api_base.clone(),
        oauth_base: endpoints.oauth_base.clone(),
        token_store,
        pcs_base: endpoints.pcs_base.clone(),
        // K7：上传会话表落实例状态目录（跨进程差集续传；runtime 产物
        // 不入库——R7，与 .session 文件同惯例；testkit 实例目录即 cwd；
        // 多卷模式下即卷主目录，K21）。装配只传目录本身——驱动内契约
        // 自行拼 `<sessions_dir>/baidu_state/sessions/`，此处再带
        // baidu_state 会嵌套（E2E 观察项①）。
        sessions_dir: Some(state_dir.to_path_buf()),
        ..Default::default()
    }
}

/// The dispatch product of [`build_backend_transport`]: one concrete arm
/// per non-telegram backend (the enum IS the type assertion surface —
/// tests pin that `backend = "baidu"` lands the `Baidu` arm). Telegram
/// is deliberately NOT an arm: its assembly is the run flow's dedicated
/// deadline-bounded Grammers connect (zero change — the dispatch only
/// routes around it).
pub enum BackendTransport {
    /// The baidu transport face over the factory-connected driver.
    Baidu(Arc<ck_baidu::BaiduTransport>),
    /// The local transport face over the factory-initialised driver.
    Local(Arc<ck_local::LocalTransport>),
}

impl BackendTransport {
    /// The assembled volume identity (`baidu:<uid>` / `local:<root>`).
    pub fn volume(&self) -> &str {
        match self {
            BackendTransport::Baidu(t) => StorageDriver::volume(t.driver()).as_str(),
            BackendTransport::Local(t) => StorageDriver::volume(t.driver()).as_str(),
        }
    }

    /// The transport face's declared capabilities (the driver's bits
    /// plus the K4 `remote_delete` the faces declare).
    pub fn caps(&self) -> Capabilities {
        match self {
            BackendTransport::Baidu(t) => CloudTransport::capabilities(t.as_ref()),
            BackendTransport::Local(t) => CloudTransport::capabilities(t.as_ref()),
        }
    }

    /// The K12 sync-namespace key this backend derives: baidu — the
    /// raw volume identity `baidu:<uid>` (a stable, non-secret account
    /// id); local — `local:<digest>` over the driver-normalized root
    /// (the raw path never ships to the server; local never starts the
    /// sync task anyway — [`is_sync_supported`]).
    pub fn sync_namespace_key(&self) -> String {
        match self {
            BackendTransport::Baidu(_) => self.volume().to_string(),
            BackendTransport::Local(t) => namespace_key_for(&NamespaceIdentity::Local {
                root: &t.driver().root_path().to_string_lossy(),
            }),
        }
    }

    /// The same transport behind the run flow's `Arc<dyn
    /// CloudTransport>` seam (a clone of the shared Arc — the enum arm
    /// stays usable).
    pub fn clone_dyn(&self) -> Arc<dyn CloudTransport> {
        match self {
            BackendTransport::Baidu(t) => t.clone() as Arc<dyn CloudTransport>,
            BackendTransport::Local(t) => t.clone() as Arc<dyn CloudTransport>,
        }
    }

    /// The dashboard storage card's boot quota snapshot (web adapter):
    /// baidu reads the driver quota once at dispatch — informational,
    /// never a live meter. Local has no quota concept (the card shows
    /// the indexed bytes on an unbounded disk), and telegram never
    /// comes through the dispatch. A failed read only downgrades to
    /// `None` (the card degrades to "unlimited") — it must not block
    /// the boot.
    pub async fn web_quota_snapshot(&self) -> Option<cloudkit_web::QuotaSnapshot> {
        match self {
            BackendTransport::Baidu(t) => match StorageDriver::quota(t.driver()).await {
                Ok(quota) => Some(cloudkit_web::QuotaSnapshot {
                    used: quota.used,
                    total: quota.total,
                }),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "baidu quota read failed; the dashboard storage card degrades to \
                         unlimited"
                    );
                    None
                }
            },
            BackendTransport::Local(_) => None,
        }
    }
}

/// The unified backend-key transport dispatch (B3b 段二b): production
/// endpoints, the K13 config write-back store, and the K18 proxy
/// declaration (a `proxy_url` on a baidu instance is logged
/// ineffective — the driver always connects directly).
///
/// Telegram is refused with guidance: the legacy arm's assembly is the
/// run flow's own deadline-bounded Grammers connect, kept byte-for-
/// byte (the absent `backend` key IS telegram — pre-Phase-2 configs
/// never reach this function's error).
pub async fn build_backend_transport(cfg: &CyDriveConfig) -> Result<BackendTransport> {
    let dispatched = build_backend_transport_with(
        cfg,
        &BaiduEndpoints::default(),
        Some(Arc::new(ConfigTokenStore::default())),
        Path::new("."),
    )
    .await?;
    if let Some(warning) = proxy_ineffective_warning(cfg) {
        tracing::warn!("{warning}");
    }
    Ok(dispatched)
}

/// [`build_backend_transport`] with the endpoint set, the K13
/// `TokenStore` and the instance state directory injected — the test
/// seam (loopback mock backends; a capturing store). `state_dir` is the
/// baidu upload-session base: single-volume callers pass the cwd (`"."`,
/// the frozen behaviour), the multi-volume dispatch passes the volume's
/// home directory (K21).
pub async fn build_backend_transport_with(
    cfg: &CyDriveConfig,
    endpoints: &BaiduEndpoints,
    token_store: Option<Arc<dyn ck_baidu::TokenStore>>,
    state_dir: &Path,
) -> Result<BackendTransport> {
    match cfg.backend {
        Backend::Telegram => anyhow::bail!(
            "the telegram backend does not assemble through the backend dispatch: the run \
             flow connects it through its own deadline-bounded GrammersTransport path \
             (unchanged since pre-Phase-2); a config without the backend key is telegram"
        ),
        Backend::Baidu => {
            let params = baidu_params(cfg, endpoints, token_store, state_dir);
            let driver = ck_baidu::factory(&params)
                .await
                .map_err(|error| anyhow::anyhow!("connecting the baidu backend: {error}"))?;
            Ok(BackendTransport::Baidu(Arc::new(
                ck_baidu::BaiduTransport::new(driver),
            )))
        }
        Backend::Local => {
            // validate() guarantees Some + absolute when backend=local.
            let root = cfg.local_root.clone().unwrap_or_default();
            let driver = ck_local::factory(&ck_local::LocalParams {
                root: PathBuf::from(root),
            })
            .await
            .map_err(|error| anyhow::anyhow!("initialising the local backend: {error}"))?;
            Ok(BackendTransport::Local(Arc::new(
                ck_local::LocalTransport::new(driver),
            )))
        }
    }
}

/// The K13 persistence adapter bridging ck-baidu's [`TokenStore`] to
/// the instance's config.toml: a rotation (refresh_token is one-use —
/// the new pair is the ONLY live value) writes the two token keys back
/// into the target file. K14's ruling allows plaintext token keys in
/// config (the sync_secret precedent); there is deliberately no
/// keyring leg — the core credential schema carries no baidu keys, and
/// inventing them is out of the dispatch unit's scope.
///
/// Failure policy: WARN, never propagate — the refresh itself already
/// succeeded and the request path must not crash on a file problem; a
/// missing/unparsable file means the credentials arrived via env
/// (`CYDRIVE_BAIDU_*` outrank the file anyway — the operator owns that
/// rotation), which the warning names.
pub struct ConfigTokenStore {
    path: PathBuf,
}

impl ConfigTokenStore {
    /// Targets `path` (production: `config.toml` in the cwd —
    /// [`ConfigTokenStore::default`]; tests inject a temp file).
    pub fn new(path: PathBuf) -> Self {
        ConfigTokenStore { path }
    }
}

impl Default for ConfigTokenStore {
    fn default() -> Self {
        ConfigTokenStore {
            path: PathBuf::from("config.toml"),
        }
    }
}

impl ck_baidu::TokenStore for ConfigTokenStore {
    fn save_tokens(&self, access_token: &str, refresh_token: &str) {
        // Light single-file I/O on the refresh path (a few KiB) — the
        // same weight class as the config writes setup/migrate already
        // do inline.
        let report = |message: String| {
            tracing::warn!(
                path = %self.path.display(),
                "{message}"
            );
        };
        let Ok(mut cfg) = CyDriveConfig::load_toml(&self.path) else {
            report(
                "rotated baidu tokens were NOT persisted: no readable config.toml — the \
                 credentials likely came from CYDRIVE_BAIDU_* env (update them there); \
                 the new refresh_token is now the only live value"
                    .to_string(),
            );
            return;
        };
        cfg.baidu_access_token = Some(access_token.to_string());
        cfg.baidu_refresh_token = Some(refresh_token.to_string());
        if let Err(error) = cfg.save_toml(&self.path) {
            report(format!(
                "rotated baidu tokens were NOT persisted (write failed: {error}); the \
                 new refresh_token is now the only live value"
            ));
        }
    }
}

// -------------------------------------------- K12 / K18 warning helpers ---

/// K18: `proxy_url` has no effect on the baidu/local backends (their
/// drivers always connect directly — no_proxy + forced IPv4); the
/// assembly logs this warning and doctor repeats it. `None` on
/// telegram (the proxy is a live setting there) or when no proxy is
/// configured.
pub fn proxy_ineffective_warning(cfg: &CyDriveConfig) -> Option<&'static str> {
    let effective =
        cfg.proxy_url.as_deref().is_some_and(|p| !p.is_empty()) && cfg.backend != Backend::Telegram;
    effective.then_some(PROXY_DIRECT_BACKEND_NOTICE)
}

/// The K18 declaration text shared by the assembly log and doctor.
pub const PROXY_DIRECT_BACKEND_NOTICE: &str =
    "proxy_url is set but has no effect on this backend: baidu/local always connect \
     directly (no_proxy + forced IPv4); the proxy only serves the telegram transport";

/// K12: a local instance cannot run the metadata-sync task (the local
/// root IS the source of truth); a `sync_url` on such an instance is a
/// misconfiguration surfaced as this warning (the sync task's start
/// gate and doctor share the text). `None` for every other shape.
pub fn local_sync_unsupported_warning(cfg: &CyDriveConfig) -> Option<&'static str> {
    (cfg.sync_url.is_some() && !cloudkit_core::sync::is_sync_supported(&cfg.backend))
        .then_some(LOCAL_SYNC_UNSUPPORTED)
}

/// The K12 warning text shared by the sync-start gate and doctor.
pub const LOCAL_SYNC_UNSUPPORTED: &str =
    "sync is not supported for the local backend: the periodic sync task stays off and \
     the sync_url key has no effect";

// -------------------------------------------- doctor: baidu probe (B3b) ---

/// The doctor baidu probe's structured outcome ([`baidu_backend_probe`]):
/// the verdict mapper ([`doctor::baidu_connectivity_check`]) turns each
/// shape into one check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendProbe {
    /// uinfo (factory) + quota + a root list all answered — the token
    /// is alive and the backend reachable.
    Alive,
    /// Re-authorization is needed (invalid/expired refresh token or
    /// incomplete credentials) — re-run `cydrive setup`.
    NeedsReauth(String),
    /// Transient (network / rate limit / backend 5xx) — retry later.
    Unreachable(String),
}

/// The doctor baidu leg (production endpoints): assemble the driver
/// (uinfo = the token liveness probe — a 110 refreshes once mid-probe,
/// exactly the runtime behavior) → quota (the lightest xpan call) →
/// one root list. Read-only by design: root WRITABILITY is not probed
/// (no probe artifacts in the user's drive); it is discovered on the
/// first upload after setup. Bounded by 45s so a dead network cannot
/// hang an interactive doctor run.
pub async fn baidu_backend_probe(cfg: &CyDriveConfig) -> BackendProbe {
    baidu_backend_probe_with(cfg, &BaiduEndpoints::default()).await
}

/// [`baidu_backend_probe`] with the endpoint set injected (tests point
/// it at a loopback mock).
pub async fn baidu_backend_probe_with(
    cfg: &CyDriveConfig,
    endpoints: &BaiduEndpoints,
) -> BackendProbe {
    // Incomplete credentials never reach the network: a clear
    // misconfiguration verdict beats a confusing connect failure.
    if let Err(error) = cfg.validate() {
        return BackendProbe::NeedsReauth(format!(
            "the baidu configuration is incomplete ({error}); re-run `cydrive setup` or set \
             the four baidu_* keys (config.toml or CYDRIVE_BAIDU_* env)"
        ));
    }
    let probe = async {
        let params = baidu_params(cfg, endpoints, None, Path::new("."));
        let driver = ck_baidu::factory(&params).await?;
        StorageDriver::quota(driver.as_ref()).await?;
        StorageDriver::list(
            driver.as_ref(),
            &cloudkit_storage::RelPath::root(),
            cloudkit_storage::Page::default(),
        )
        .await?;
        Ok::<(), cloudkit_storage::StorageError>(())
    };
    match tokio::time::timeout(Duration::from_secs(45), probe).await {
        Ok(Ok(())) => BackendProbe::Alive,
        Ok(Err(cloudkit_storage::StorageError::Unauthorized { recoverable: false })) => {
            BackendProbe::NeedsReauth(
                "the baidu refresh token is no longer valid; re-run `cydrive setup` and \
                 paste a fresh refresh_token"
                    .to_string(),
            )
        }
        Ok(Err(cloudkit_storage::StorageError::Invalid)) => BackendProbe::NeedsReauth(
            "the baidu credentials are incomplete (access/refresh token missing); re-run \
             `cydrive setup`"
                .to_string(),
        ),
        Ok(Err(error)) => BackendProbe::Unreachable(error.to_string()),
        Err(_elapsed) => BackendProbe::Unreachable(
            "the probe did not finish within 45s (network path to pan.baidu.com?)".to_string(),
        ),
    }
}

/// The backend-key driver assembly for `rebuild` (B3b 段二a minimal
/// form, consolidated by the dispatch unit): telegram → the
/// shadow-index refusal; baidu/local → the drivers' `factory`s behind
/// `Arc<dyn StorageDriver>` (the rebuild walk needs the StorageDriver
/// face — list/stat; the CloudTransport face carries no listing).
///
/// The baidu arm shares [`baidu_params`] with the transport dispatch
/// (single mapping, no drift) and carries the K13 write-back store —
/// a refresh triggered mid-walk must persist its rotated pair or the
/// next boot reads a dead refresh_token.
async fn build_driver(cfg: &CyDriveConfig) -> Result<Arc<dyn StorageDriver>> {
    match cfg.backend {
        Backend::Telegram => anyhow::bail!("{TELEGRAM_REBUILD_REFUSAL}"),
        Backend::Baidu => {
            let params = baidu_params(
                cfg,
                &BaiduEndpoints::default(),
                Some(Arc::new(ConfigTokenStore::default())),
                Path::new("."),
            );
            let driver = ck_baidu::factory(&params)
                .await
                .map_err(|error| anyhow::anyhow!("connecting the baidu backend: {error}"))?;
            Ok(driver)
        }
        Backend::Local => {
            // validate() guarantees Some + absolute when backend=local.
            let root = cfg.local_root.clone().unwrap_or_default();
            let driver = ck_local::factory(&ck_local::LocalParams {
                root: PathBuf::from(root),
            })
            .await
            .map_err(|error| anyhow::anyhow!("initialising the local backend: {error}"))?;
            Ok(driver)
        }
    }
}

/// Tuning of [`spawn_sync_doorbell`]'s reconnect loop (quasi-realtime
/// batch). The default is [`DOORBELL_BACKOFF`]: 1s doubling to a 60s cap
/// — a family-scale server outage heals within a minute of its end,
/// while the fallback interval keeps freshness meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoorbellBackoff {
    /// First delay after a disconnect (doubles on every further one).
    pub initial: Duration,
    /// Exponential cap of the delays.
    pub max: Duration,
}

/// Production doorbell backoff: 1s doubling, capped at 60s.
pub const DOORBELL_BACKOFF: DoorbellBackoff = DoorbellBackoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(60),
};

/// The SSE doorbell task of the `run` flow (quasi-realtime batch): one
/// long-lived subscription whose foreign-origin events ring `wake` — the
/// SAME [`Notify`] the VFS's local-change hooks ring — so the periodic
/// sync task runs a pass within seconds of a remote change.
///
/// Reconnect policy: any disconnect (stream end, read failure, rejected
/// or timed-out handshake) warns and retries after an exponential
/// `backoff_initial` → `backoff_max` delay; a successful (re)connection
/// resets the backoff and rings `wake` once immediately — covering the
/// doorbells that rang while this side was offline (pull is idempotent,
/// so one catch-up pass is all the missed rings deserve). The whole task
/// is best-effort: a dead or gated server only warns, and the periodic
/// fallback keeps the drive converging.
///
/// Every wait is selected against the shared [`ShutdownWatch`], so the
/// task exits promptly on shutdown (the reader task inside
/// [`HttpSyncClient::subscribe_stream`] ends on its own once the
/// receiver drops or the next send fails — the server's keepalive
/// cadence bounds that lag).
///
/// Public as the unit-test seam of the reconnect policy (the e2e wiring
/// is covered through [`run_with_transport`]).
pub fn spawn_sync_doorbell(
    client: Arc<sync_client::HttpSyncClient>,
    key: String,
    secret: Option<String>,
    client_id: String,
    wake: Arc<Notify>,
    watch: Arc<ShutdownWatch>,
    backoff: DoorbellBackoff,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut delay = backoff.initial;
        loop {
            match client.subscribe_stream(&key, secret.as_deref()).await {
                Ok(mut events) => {
                    tracing::info!(
                        url = client.url(),
                        "sync doorbell connected; remote changes now trigger immediate passes"
                    );
                    // A fresh connection covers everything that rang
                    // while we were offline — one catch-up pass now.
                    wake.notify_one();
                    delay = backoff.initial;
                    loop {
                        tokio::select! {
                            event = events.recv() => match event {
                                Some(event) => {
                                    // Never ring our own bell: the
                                    // server skips our pushes too, but a
                                    // proxy forwarding frames (or an
                                    // anonymous fallback id) must not
                                    // echo us into a pass loop.
                                    if event.origin.as_deref() != Some(client_id.as_str()) {
                                        tracing::debug!(
                                            max_version = event.max_version,
                                            origin = ?event.origin,
                                            "sync doorbell rang; running a pass"
                                        );
                                        wake.notify_one();
                                    }
                                }
                                None => break, // stream ended: reconnect
                            },
                            _ = watch.wait() => return,
                        }
                    }
                    tracing::info!("sync doorbell stream ended; reconnecting");
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "subscribing the sync doorbell failed; retrying with backoff \
                         (the periodic fallback sync stays active)"
                    );
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = watch.wait() => return,
            }
            delay = delay.saturating_mul(2).min(backoff.max);
        }
    })
}

/// Spawns the `run` flow's periodic sync task (sync-lite Batch B4 +
/// quasi-realtime batch): one [`sync_once`] pass immediately at boot,
/// then one every `sync_interval_secs` — PLUS a pass within seconds of
/// every local files-table change (the VFS doorbell `wake`, rung by the
/// put/remove/create/inbound hooks and the upload queue's success
/// persist) and of every remote change (the SSE doorbell task). Returns
/// `None` when `sync_url` is unset (the feature is off), the backend
/// cannot sync at all (K12: local — a warn, never a boot failure), or
/// the namespace identity is missing (a warn, never a boot failure).
/// Every pass failure only warns — the served stack is unaffected. The
/// task exits on its own when the shared stop gate fires;
/// [`RunHandle::shutdown`] additionally aborts it so a pass in flight
/// cannot delay the exit.
///
/// `namespace` (B3b 段二b): the backend-dispatched namespace key
/// (baidu's `baidu:<uid>`); `None` keeps the frozen telegram derivation
/// and its `is_configured` gate.
fn spawn_periodic_sync(
    cfg: &CyDriveConfig,
    db: Arc<MetaDatabase>,
    cache_root: PathBuf,
    cache_limit: u64,
    watch: Arc<ShutdownWatch>,
    wake: Arc<Notify>,
    namespace: Option<String>,
) -> Option<tokio::task::JoinHandle<()>> {
    let url = cfg.sync_url.clone()?;
    // K12 gate: the local backend never runs the sync task — the local
    // root IS the source of truth; doctor repeats this warning offline.
    if let Some(warning) = local_sync_unsupported_warning(cfg) {
        tracing::warn!("{warning}");
        return None;
    }
    let key = match namespace {
        // A backend-derived key (baidu volume) bypasses the telegram
        // credential gate — the identity is the account, not a bot.
        Some(key) => key,
        None => {
            if !cfg.is_configured() {
                tracing::warn!(
                    "sync_url is set but bot_token/chat_id are missing; periodic sync stays off"
                );
                return None;
            }
            namespace_key(&cfg.bot_token, &cfg.chat_id.to_string())
        }
    };
    let secret = resolve_sync_secret(cfg);
    // The per-database stable identity: the doorbell's origin-skip (and
    // the access log's client tag) key on it. A read failure only warns
    // and falls back to the empty string — which serializes away (no
    // client_id on the wire), so a broken identity degrades to the
    // anonymous pre-doorbell behavior instead of disabling sync.
    let client_id = match db.sync_client_id() {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(
                %error,
                "reading this instance's sync client id failed; pushing anonymously"
            );
            String::new()
        }
    };
    let client = Arc::new(sync_client::HttpSyncClient::new(&url, client_id.clone()));
    let cache = CacheManager::new(cache_root, cache_limit);
    let period = Duration::from_secs(cfg.sync_interval_secs);
    tracing::info!(
        url = %url,
        interval_secs = cfg.sync_interval_secs,
        "quasi-realtime metadata sync enabled (doorbell + fallback interval)"
    );
    Some(tokio::spawn(async move {
        // The SSE doorbell task starts BEFORE the first pass: ordering
        // is not load-bearing (a ring during the boot pass merely
        // coalesces into one permit → one follow-up pass), but starting
        // it first minimizes the window in which an early remote change
        // would wait for the fallback tick.
        let _doorbell = spawn_sync_doorbell(
            Arc::clone(&client),
            key.clone(),
            secret.clone(),
            client_id,
            Arc::clone(&wake),
            Arc::clone(&watch),
            DOORBELL_BACKOFF,
        );
        // Anti-lost-wakeup pattern for select! over a Notify (tokio's
        // documented `enable` + reset dance): the long-lived Notified is
        // enabled at the top of every iteration, so a wake stored while
        // another branch fired stays assigned to THIS future instead of
        // being dropped — a local change made mid-pass still triggers
        // the follow-up pass.
        let wakeup = wake.notified();
        tokio::pin!(wakeup);
        // The interval's first tick completes immediately, which is
        // exactly the desired boot-time first pass.
        let mut ticker = tokio::time::interval(period);
        loop {
            wakeup.as_mut().enable();
            tokio::select! {
                _ = ticker.tick() => {}
                _ = wakeup.as_mut() => {
                    // consumed: re-arm for the next wake
                    wakeup.set(wake.notified());
                }
                _ = watch.wait() => break,
            }
            match sync_once(&db, &cache, client.as_ref(), &key, secret.as_deref()).await {
                Ok(outcome) => tracing::info!(
                    applied = outcome.applied,
                    pushed = outcome.pushed,
                    tombstoned = outcome.tombstoned,
                    skipped_ghost = outcome.skipped_ghost,
                    "metadata sync pass complete"
                ),
                Err(error) => tracing::warn!(
                    %error,
                    "periodic metadata sync failed; the service is unaffected"
                ),
            }
        }
        // The doorbell child exits through the same watch; aborting here
        // merely shortens its final sleep for the cooperative exit path
        // (RunHandle::shutdown aborts THIS task, and the child still
        // exits on its own watch arm).
        _doorbell.abort();
    }))
}

/// Auto-mount step (unit D; the Unix leg is status plan C5): with
/// `auto_mount_drive`, Windows maps the best available drive letter to
/// the config's WebDAV URL, while Unix mounts the auto-mount target
/// (`mount_point` key or `$HOME/CyDrive`) through the gio→davfs2 chain,
/// after releasing any stale davfs mounts still pointing at the URL. A
/// disabled config (or unsupported platform) skips with an info line; a
/// failed mount only warns — the server stays reachable at its URL
/// either way.
///
/// Returns the stop sequence's claim pair: the Windows drive letter and
/// the Unix mount point. Exactly one of them can ever be `Some` (each
/// platform's branch fills only its own slot), and both platform calls
/// the stop task makes compile everywhere through the platform stubs.
fn mount_if_configured(cfg: &CyDriveConfig) -> (Option<String>, Option<PathBuf>) {
    #[cfg(unix)]
    {
        if !cfg.auto_mount_drive {
            tracing::info!("auto_mount_drive is off; skipping the drive mapping");
            return (None, None);
        }
        // auto_mount_target consults home only in its default arm, so a
        // missing $HOME still lets an explicit mount_point key carry the
        // auto-mount.
        let target = match std::env::var("HOME") {
            Ok(home) => cloudkit_platform::auto_mount_target(cfg, Path::new(&home)),
            Err(_no_home) => cfg.mount_point.as_deref().map(PathBuf::from),
        };
        let Some(target) = target else {
            tracing::warn!(
                "auto_mount_drive is on but no mount target resolved (no mount_point \
                 key and $HOME is unset); skipping"
            );
            return (None, None);
        };
        let url = default_mount_url(cfg);
        println!("Mounting {} -> {} ...", target.display(), url);
        // Stale cleanup first: a leftover davfs mount from a previous run
        // (or a crashed session) must not keep serving a dead server
        // under the fresh one.
        cloudkit_platform::linux::unmount_stale_for(&url);
        match cloudkit_platform::linux::mount_drive(&target, &url) {
            Ok(report) => {
                println!("{report}");
                (None, Some(target))
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    point = %target.display(),
                    "unix auto-mount failed; continuing without it"
                );
                println!("Auto-mount FAILED ({error}); WebDAV stays reachable at {url}.");
                println!(
                    "  Hints: install davfs2 (e.g. `apt install davfs2`), mount with \
                     permission (root or the davfs2 group), or mount manually: `cydrive \
                     mount --path {}`.",
                    target.display()
                );
                (None, None)
            }
        }
    }

    #[cfg(not(unix))]
    {
        if !cfg.auto_mount_drive {
            tracing::info!("auto_mount_drive is off; skipping the drive mapping");
            return (None, None);
        }
        if !cfg!(windows) {
            tracing::info!("drive mapping is Windows-only; skipping");
            return (None, None);
        }
        let url = default_mount_url(cfg);
        println!("Mounting drive letter {} -> {} ...", cfg.drive_letter, url);
        match cloudkit_platform::windows::mount_drive(&cfg.drive_letter, &url) {
            Ok(letter) => {
                println!("Drive mounted: {letter} -> {url}");
                (Some(letter), None)
            }
            Err(error) => {
                println!("Auto-mount FAILED ({error}); WebDAV stays reachable at {url}.");
                println!(
                    "  Hints: run `cydrive fix-reg` in an elevated shell, ensure the WebClient                  service can start, and check that the letter is free (`cydrive doctor`)."
                );
                (None, None)
            }
        }
    }
}

/// Glues the canonical mount URL from the config's WebDAV host/port.
pub fn default_mount_url(cfg: &CyDriveConfig) -> String {
    format!("http://{}:{}", cfg.webdav_host, cfg.webdav_port)
}

/// Glues one volume's mount URL (K27): the process WebDAV endpoint plus
/// the `/vol/<name>` segment the single multi-volume listener routes
/// (K20). Pure URL construction — the Windows MiniRedir compatibility
/// of the sub-path mount is the real-machine probe's question, not
/// this function's.
pub fn volume_mount_url(process_cfg: &CyDriveConfig, volume_name: &str) -> String {
    format!("{}/vol/{volume_name}", default_mount_url(process_cfg))
}

/// Per-volume auto-mount (K27 / MV2, the multi-volume analog of
/// [`mount_if_configured`]): every claim is a RUNNING volume whose file
/// explicitly set `drive_letter`, and the mount target is the volume's
/// `/vol/<name>` path on the process WebDAV port. The process-level
/// `auto_mount_drive` switch stays the gate (off = mount nothing, the
/// single-volume semantics). A failed mount only warns — the volume
/// stays reachable at its WebDAV URL. Returns the letters actually
/// mounted (the stop sequence releases exactly these).
///
/// Unix: K27 adjudicated the Windows drive-letter mounts only; the
/// single-volume Unix gio→davfs2 chain is a process-level
/// `mount_point` concept with no per-volume counterpart yet, so
/// multi-volume mode mounts nothing there (info line, never silent).
fn mount_volumes_if_configured(cfg: &CyDriveConfig, claims: &[(String, String)]) -> Vec<String> {
    #[cfg(unix)]
    {
        let _ = (cfg, claims);
        tracing::info!(
            "multi-volume mode mounts no directories on unix (per-volume drive-letter \
             mounts are the K27 scope); mount manually: `cydrive mount --path <dir>`"
        );
        Vec::new()
    }

    #[cfg(not(unix))]
    {
        if claims.is_empty() {
            tracing::info!("no volume claimed a drive_letter; skipping the drive mappings");
            return Vec::new();
        }
        if !cfg.auto_mount_drive {
            tracing::info!("auto_mount_drive is off; skipping the drive mappings");
            return Vec::new();
        }
        if !cfg!(windows) {
            tracing::info!("drive mapping is Windows-only; skipping");
            return Vec::new();
        }
        let mut mounted = Vec::new();
        for (name, letter) in claims {
            let url = volume_mount_url(cfg, name);
            println!("Mounting volume {name} as drive {letter} -> {url} ...");
            match cloudkit_platform::windows::mount_drive(letter, &url) {
                Ok(actual) => {
                    println!("Drive mounted: {actual} -> {url} (volume {name})");
                    mounted.push(actual);
                }
                Err(error) => {
                    println!("Auto-mount FAILED for volume {name} ({error}); the volume stays reachable at {url}.");
                    println!(
                        "  Hints: run `cydrive fix-reg` in an elevated shell, ensure the WebClient                  service can start, and check that the letter is free (`cydrive doctor`)."
                    );
                }
            }
        }
        mounted
    }
}

// ------------------------------------------------- status subcommand (C3) ---

/// The dashboard's canonical URL when `enable_web_ui` is on, `None` when
/// the UI is off — shared by [`collect_status`]'s probe decision and the
/// renderer's `disabled` line, so the two cannot drift.
pub fn dashboard_url(cfg: &CyDriveConfig) -> Option<String> {
    cfg.enable_web_ui
        .then(|| format!("http://{}:{}", cfg.web_ui_host, cfg.web_ui_port))
}

/// The `cydrive status` data model (status plan C3): every field carries
/// only its healthy value — `instance` is a live instance's PING reply
/// (the `OK: cydrive <version>` line), `control` the control file's
/// address when one exists (rendering marks it stale when the PING
/// failed), `webdav`/`dashboard` the glued URL when the port answered
/// the 1s probe, `mount` the current machine's mapping for the drive
/// URL. `None` everywhere means "down/absent", never "unknown".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// A running instance's PING reply (`Some` = running).
    pub instance: Option<String>,
    /// The control file's address (`None` = no control file at all).
    pub control: Option<String>,
    /// The WebDAV URL when its port answered (`Some` = listening).
    pub webdav: Option<String>,
    /// The dashboard URL when its port answered (`Some` = listening).
    pub dashboard: Option<String>,
    /// The current mount for the drive URL (`"Y:"` / `"/root/CyDrive"`).
    pub mount: Option<String>,
}

/// The `cydrive status` collection body (status plan C3), run against
/// the same config discovery as `run`/`stop` — the control file resolves
/// relative to the discovered `db_path`, so `status` shares their
/// working-directory rule. Never fails: every dead probe lands as a
/// `None` field for [`render_status`] to phrase.
///
/// - control file + [`control::send_ping`]: a reply puts the version
///   line on `instance`; a file whose address stays silent keeps
///   `instance` empty and `control` carries the address (stale);
/// - WebDAV / dashboard ports: 1s connect probes (the dashboard only
///   when `enable_web_ui`);
/// - mount: [`cloudkit_platform::current_mount_for`] against the glued
///   drive URL.
pub async fn collect_status(cfg: &CyDriveConfig) -> StatusReport {
    let (instance, control) = match control::read_control_addr(cfg) {
        Ok(addr) => {
            let control = Some(addr.to_string());
            match control::send_ping(addr).await {
                Ok(reply) => (Some(reply), control),
                Err(_dead_address) => (None, control),
            }
        }
        Err(_no_file) => (None, None),
    };

    let webdav_url = default_mount_url(cfg);
    let webdav = probe_listening(&cfg.webdav_host, cfg.webdav_port, &webdav_url).await;
    let dashboard = match dashboard_url(cfg) {
        Some(url) => probe_listening(&cfg.web_ui_host, cfg.web_ui_port, &url).await,
        None => None,
    };
    let mount = cloudkit_platform::current_mount_for(&webdav_url);
    StatusReport {
        instance,
        control,
        webdav,
        dashboard,
        mount,
    }
}

/// The 1s connect probe behind the status port lines: `Some(url)` exactly
/// when the address answered — a refused or silent port (or an
/// unparseable host) reads as down. The successful connection is dropped
/// on the spot; connectability is all `status` asks of the port.
async fn probe_listening(host: &str, port: u16, url: &str) -> Option<String> {
    let addr = SocketAddr::new(host.parse::<IpAddr>().ok()?, port);
    match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(addr)).await {
        Ok(connected) => connected.is_ok().then(|| url.to_string()),
        Err(_elapsed) => None,
    }
}

/// The `cydrive status` renderer (status plan C3; format pinned verbatim
/// by the status tests): labels left-aligned to column 12, the instance
/// parenthetical is the PING reply verbatim, the control line is omitted
/// when no control file exists and gains a ` (stale)` marker when its
/// address no longer answers, and a disabled dashboard prints
/// `disabled`. Lines are `'\n'`-joined without a trailing newline — the
/// CLI `println!`s the whole report.
pub fn render_status(r: &StatusReport, webdav_url: &str, dash_url: Option<String>) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(5);
    match &r.instance {
        Some(reply) => lines.push(format!("instance:   running ({reply})")),
        None => lines.push("instance:   not running".to_string()),
    }
    if let Some(addr) = &r.control {
        // instance down + file present = the file points at a dead run.
        let stale = if r.instance.is_some() { "" } else { " (stale)" };
        lines.push(format!("control:    {addr}{stale}"));
    }
    lines.push(if r.webdav.is_some() {
        format!("webdav:     {webdav_url} listening")
    } else {
        format!("webdav:     {webdav_url} not reachable")
    });
    match dash_url {
        Some(url) if r.dashboard.is_some() => {
            lines.push(format!("dashboard:  {url} listening"));
        }
        Some(url) => lines.push(format!("dashboard:  {url} not reachable")),
        None => lines.push("dashboard:  disabled".to_string()),
    }
    match &r.mount {
        Some(mapping) => lines.push(format!("mount:      {mapping} -> {webdav_url}")),
        None => lines.push("mount:      not mounted".to_string()),
    }
    lines.join("\n")
}

/// Resolves the `mount` subcommand's arguments: an explicit `--url` /
/// `--letter` wins, otherwise the config's `drive_letter` and the glued
/// default URL apply. Letters pass through verbatim; normalization is
/// [`cloudkit_platform::windows::mount_drive`]'s concern.
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

/// The [`discover_config_with_volumes`] outcome (Phase 2.5 / MV0):
/// single-volume mode keeps returning a plain [`CyDriveConfig`]
/// (byte-identical chain), multi-volume mode returns the process
/// config plus the discovered volume manifest.
#[derive(Debug)]
pub enum DiscoveredConfig {
    /// `volumes_dir` absent — the one-config-is-one-drive status quo.
    Single(CyDriveConfig),
    /// `volumes_dir` set — `process` holds the process-scoped config,
    /// `volumes` the per-volume files in stable name order.
    Multi {
        /// The process-level `config.toml` (process-scoped keys only).
        process: CyDriveConfig,
        /// The volume manifest, in file-name order ([`VolumeConfig`]).
        volumes: Vec<VolumeConfig>,
    },
}

/// Volume-aware sibling of [`discover_config`] (Phase 2.5 / MV0): when
/// the process `config.toml` sets `volumes_dir`, loads and validates the
/// whole volume set (mixing guard, volumes directory, per-volume parse,
/// drive-letter conflicts) and returns [`DiscoveredConfig::Multi`];
/// otherwise the existing single-volume chain applies unchanged.
pub fn discover_config_with_volumes() -> Result<DiscoveredConfig> {
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
    discover_config_with_volumes_and_store(store.as_ref())
}

/// [`discover_config_with_volumes`] with the credential store injected
/// (tests pass an [`InMemoryStore`]). In multi-volume mode the store
/// backfill and the `CYDRIVE_*` overrides are **not** applied — the
/// process config carries no credentials, and global env overrides
/// would cross-wire volume settings across volumes, so they are ignored
/// with a tracing note (K28). The per-volume credential resolution
/// chain (env > file > keyring at driver-resolve time) is unchanged.
pub fn discover_config_with_volumes_and_store(
    store: &dyn CredentialStore,
) -> Result<DiscoveredConfig> {
    let toml_path = Path::new("config.toml");
    if toml_path.exists() {
        let (cfg, raw_keys) = CyDriveConfig::load_toml_with_keys(toml_path)
            .with_context(|| format!("loading {}", toml_path.display()))?;
        if let Some(volumes_dir) = cfg.volumes_dir.clone() {
            tracing::info!(
                volumes_dir = %volumes_dir,
                "multi-volume mode: CYDRIVE_* environment overrides are ignored (K28); \
                 per-volume settings come from the volume files"
            );
            cloudkit_core::config::ensure_no_volume_keys_in_process(&raw_keys)
                .context("multi-volume config.toml mixes process- and volume-scoped keys")?;
            cfg.validate()
                .context("invalid process-level configuration")?;
            let volumes = cloudkit_core::config::load_volumes(Path::new(&volumes_dir))
                .with_context(|| format!("loading volumes from {volumes_dir:?}"))?;
            return Ok(DiscoveredConfig::Multi {
                process: cfg,
                volumes,
            });
        }
        return Ok(DiscoveredConfig::Single(
            cfg.with_credential_backfill(store).with_env_overrides(),
        ));
    }
    let legacy_path = Path::new("config.json");
    if legacy_path.exists() {
        let cfg = CyDriveConfig::load_legacy_json(legacy_path)
            .with_context(|| format!("loading legacy {}", legacy_path.display()))?;
        return Ok(DiscoveredConfig::Single(
            cfg.with_credential_backfill(store).with_env_overrides(),
        ));
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
