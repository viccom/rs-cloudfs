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
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
#[cfg(feature = "telegram")]
use ck_telegram::config::{
    TransportConfig, DEFAULT_API_HASH, DEFAULT_API_ID, DEFAULT_SESSION_STEM,
};
#[cfg(feature = "telegram")]
use ck_telegram::transport::GrammersTransport;
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::{Backend, CyDriveConfig, MountBackend, VolumeConfig};
use cloudkit_core::credentials::{
    CredentialStore, InMemoryStore, BOT_TOKEN, ENCRYPTION_PASSWORD, SERVICE,
};
use cloudkit_core::database::MetaDatabase;
use cloudkit_core::inbound::spawn_inbound_worker;
use cloudkit_core::materialize::CipherCtx;
use cloudkit_core::rebuild;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::sync::{namespace_key, sync_once, SyncOutcome};
use futures_util::FutureExt as _;
// The command-execution task's unwind guard (K58-FB) — the same
// containment pattern control.rs applies around its handler await.
use std::panic::AssertUnwindSafe;
// Local-arm-only sync vocab (FT3): the K12 local namespace derivation
// exists only with the `local` feature.
#[cfg(feature = "local")]
use cloudkit_core::sync::{namespace_key_for, NamespaceIdentity};
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

/// The actionable message every telegram surface carries when the binary
/// was built without the telegram driver (K31,
/// docs/plans/2026-09-09-driver-feature-gates.md): name the rebuild
/// command, then name the backend switch. Pinned by the off-feature test
/// in `tests/dispatch.rs`.
pub const TELEGRAM_DRIVER_REQUIRED: &str = "this binary was built without the telegram driver; \
     rebuild with `cargo build --features telegram`, or set `backend = \"local\"` / \
     `backend = \"baidu\"` in config.toml";

/// The actionable message every baidu surface carries when the binary
/// was built without the baidu driver (K31, FT2 — same shape as
/// [`TELEGRAM_DRIVER_REQUIRED`]): name the rebuild command, then name
/// the backend switch. Pinned by the off-feature test in
/// `tests/dispatch.rs`.
pub const BAIDU_DRIVER_REQUIRED: &str = "this binary was built without the baidu driver; \
     rebuild with `cargo build --features baidu`, or set `backend = \"telegram\"` / \
     `backend = \"local\"` in config.toml";

/// The actionable message every local surface carries when the binary
/// was built without the local driver (K31, FT3 — same shape as
/// [`TELEGRAM_DRIVER_REQUIRED`] / [`BAIDU_DRIVER_REQUIRED`]): name the
/// rebuild command, then name the backend switch. Pinned by the
/// off-feature test in `tests/dispatch.rs`.
pub const LOCAL_DRIVER_REQUIRED: &str = "this binary was built without the local driver; \
     rebuild with `cargo build --features local`, or set `backend = \"telegram\"` / \
     `backend = \"baidu\"` in config.toml";

/// The actionable message every sftp surface carries when the binary was
/// built without the sftp driver (K31 shape, Phase 4 / SF3 — same form as
/// [`TELEGRAM_DRIVER_REQUIRED`] / [`BAIDU_DRIVER_REQUIRED`] /
/// [`LOCAL_DRIVER_REQUIRED`]): name the rebuild command, then name the
/// backend switch. Pinned by the off-feature test in `tests/dispatch.rs`.
pub const SFTP_DRIVER_REQUIRED: &str = "this binary was built without the sftp driver; \
     rebuild with `cargo build --features sftp`, or set `backend = \"telegram\"` / \
     `backend = \"baidu\"` / `backend = \"local\"` in config.toml";

/// The actionable message every pan115 surface carries when the binary was
/// built without the pan115 driver (K31 shape, Phase 5 / 115-1 — same form
/// as the four driver constants above): name the rebuild command, then
/// name the backend switch. Consumed by the 115-4 dispatch arms; pinned by
/// the off-feature test when that wiring lands.
pub const PAN115_DRIVER_REQUIRED: &str = "this binary was built without the pan115 driver; \
     rebuild with `cargo build --features pan115`, or set `backend = \"telegram\"` / \
     `backend = \"baidu\"` / `backend = \"local\"` / `backend = \"sftp\"` in config.toml";

/// The actionable message every pan123 surface carries when the binary was
/// built without the pan123 driver (K31 shape, Phase 6 / 123-1 — same form
/// as the five driver constants above): name the rebuild command, then
/// name the backend switch. Consumed by the 123-4 dispatch arms; pinned by
/// the off-feature test when that wiring lands.
pub const PAN123_DRIVER_REQUIRED: &str = "this binary was built without the pan123 driver; \
     rebuild with `cargo build --features pan123`, or set `backend = \"telegram\"` / \
     `backend = \"baidu\"` / `backend = \"local\"` / `backend = \"sftp\"` / \
     `backend = \"pan115\"` in config.toml";

/// The actionable message every webdav surface carries when the binary was
/// built without the webdav driver (K31 shape, Phase 7 / WD1b — same form
/// as the six driver constants above): name the rebuild command, then
/// name the backend switch. Consumed by the WD1b dispatch arms; pinned by
/// the off-feature test in `tests/dispatch.rs`.
pub const WEBDAV_DRIVER_REQUIRED: &str = "this binary was built without the webdav driver; \
     rebuild with `cargo build --features webdav`, or set `backend = \"telegram\"` / \
     `backend = \"baidu\"` / `backend = \"local\"` / `backend = \"sftp\"` / \
     `backend = \"pan115\"` / `backend = \"pan123\"` in config.toml";

/// The driver list the binary was compiled with (K32,
/// docs/plans/2026-09-09-driver-feature-gates.md) — the `(drivers: ...)`
/// segment of the `--version` banner.
///
/// Extensible form (Phase 4 / SF1, `docs/plans/2026-09-14-sftp-driver.md`
/// §3): the fixed-order table maps one `cfg!` probe per driver, the list
/// is the filter-join of the enabled rows and the empty set reports
/// `none` — adding a driver is one appended row (the SF0-era 3-tuple
/// match would grow 2^n arms instead). Each row is still selected by the
/// feature set at compile time (`cfg!` expands to a literal), in the
/// fixed order telegram, baidu, local, sftp, pan115, pan123, webdav;
/// onboarding the next driver (driver-onboarding §1) appends its row
/// here. Pinned by `tests::compiled_drivers_lists_the_feature_set_in_fixed_order`
/// (cfg-gated arms — one assertion per build; the 3-driver combinations'
/// output is byte-identical to the pre-SF1 refactor, and the
/// sftp/pan115/pan123/webdav rows simply append to the fixed order).
pub fn compiled_drivers() -> String {
    const DRIVER_ROWS: &[(bool, &str)] = &[
        (cfg!(feature = "telegram"), "telegram"),
        (cfg!(feature = "baidu"), "baidu"),
        (cfg!(feature = "local"), "local"),
        (cfg!(feature = "sftp"), "sftp"),
        (cfg!(feature = "pan115"), "pan115"),
        (cfg!(feature = "pan123"), "pan123"),
        (cfg!(feature = "webdav"), "webdav"),
    ];
    let enabled: Vec<&str> = DRIVER_ROWS
        .iter()
        .filter(|(compiled, _)| *compiled)
        .map(|(_, name)| *name)
        .collect();
    if enabled.is_empty() {
        "none".to_string()
    } else {
        enabled.join(", ")
    }
}

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

    /// Whether the gate has already fired — the synchronous probe the
    /// volume-command surface checks at its entry and mid-sequence
    /// observation points (review H1); [`ShutdownWatch::wait`] is the
    /// async face. Latched like the gate itself.
    pub fn fired(&self) -> bool {
        *self.rx.borrow()
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
    // The double-start guard runs before any assembly: a second boot
    // over a live instance would clobber the control file and orphan
    // the first instance's stop handle.
    control::ensure_not_running(cfg).await?;
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
///
/// Clone is a cheap snapshot (Arc fields + the spec's strings): registry
/// lookups clone the entry out so the shared table's lock is never held
/// across the caller's work.
#[derive(Clone)]
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

/// The process's volume set (Phase 2.5 / MV1; a live shared handle
/// since Phase 3.6 / RV1, K51): the `Arc<RwLock<ordered volume table>>`
/// one [`VolumeRuntime`] per discovered volume, in discovery (file-name)
/// order. A thin registry by design — lifecycle lives in
/// [`MultiVolumeHandle`]'s stop task; this type only answers "what
/// volumes exist and how are they doing", and (RV2) lets the control
/// channel's ADD/REMOVE mutate that answer while the process runs.
///
/// Clone shares the table. Every read clones its result out and
/// releases the lock before the caller's work (banner printing, mount
/// passes), so the lock never spans an await; the same release-then-run
/// contract makes `status_list` a live query — the banner/doctor face
/// reads the table as it stands when asked, not as boot left it.
///
/// This is the master truth: the WebDAV dispatcher and the dashboard
/// each carry their own face table (crate-own types — a layering
/// constraint, R1) filled by the composition root from THIS table at
/// boot, and RV2's serialized ADD/REMOVE keeps all three coherent.
#[derive(Clone, Debug)]
pub struct RegistryHandle {
    volumes: Arc<std::sync::RwLock<Vec<VolumeRuntime>>>,
}

impl RegistryHandle {
    /// Builds the handle over the boot-assembled entries (in discovery
    /// order).
    pub fn new(volumes: Vec<VolumeRuntime>) -> Self {
        Self {
            volumes: Arc::new(std::sync::RwLock::new(volumes)),
        }
    }

    /// Registers one volume (appending to the table order) — RV2's ADD
    /// lands here at runtime, the boot loop hands its assembled Vec to
    /// [`RegistryHandle::new`]. A duplicate name is the caller's
    /// contract error (the control channel's ADD refuses duplicates
    /// before reaching here).
    pub fn insert(&self, runtime: VolumeRuntime) {
        self.write().push(runtime);
    }

    /// One volume's registry entry by name (`None` for an unknown name)
    /// — the lookup the mount pass resolves each claim's VFS with. A
    /// clone: the table lock is released before the caller's work.
    pub fn volume(&self, name: &str) -> Option<VolumeRuntime> {
        self.read()
            .iter()
            .find(|volume| volume.name() == name)
            .cloned()
    }

    /// The (name, status) list for banners and (MV3) `/api/volumes` —
    /// a live query over the current table (RV1).
    pub fn status_list(&self) -> Vec<(String, VolumeStatus)> {
        self.read()
            .iter()
            .map(|volume| (volume.name().to_string(), volume.status()))
            .collect()
    }

    /// A snapshot of the whole table, in order (mount-claim collection
    /// and the Debug faces).
    pub fn volumes(&self) -> Vec<VolumeRuntime> {
        self.read().clone()
    }

    /// `true` when not a single volume assembled (the boot's Err gate).
    pub fn all_failed(&self) -> bool {
        self.read()
            .iter()
            .all(|volume| matches!(volume, VolumeRuntime::Failed { .. }))
    }

    /// Removes the volume named `name`; `true` when it was registered
    /// (the K50 seam RV2's REMOVE acts through — the K51 liveness this
    /// pins is what makes the removal visible to the WebDAV dispatch,
    /// the dashboard and the banner).
    pub fn remove(&self, name: &str) -> bool {
        let mut volumes = self.write();
        match volumes.iter().position(|volume| volume.name() == name) {
            Some(position) => {
                volumes.remove(position);
                true
            }
            None => false,
        }
    }

    /// The table read lock, poisoned-lock recovery per the code-style
    /// norm (a panicked holder must not take the process's volume view
    /// down).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<VolumeRuntime>> {
        self.volumes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The table write lock (same recovery norm).
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<VolumeRuntime>> {
        self.volumes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The per-volume home directory (K21): `<volumes_dir>/<name>/` — the
/// resolution base for every volume-relative path (db/cache/session/
/// baidu state). The base dir is absolutised against the process cwd so
/// volume-relative `local_root`s resolve to the absolute paths
/// [`CyDriveConfig::validate`] demands.
pub fn volume_home(spec: &VolumeConfig) -> Result<PathBuf> {
    volume_home_under(&spec.base_dir, &spec.name)
}

/// `<base_dir>/<name>` with the base dir absolutised against the
/// process cwd — the K21 resolution [`volume_home`] runs, shared with
/// DESTROY's name-derived lookups (a schema-broken volume file has no
/// loadable spec, but its home is still `<volumes_dir>/<name>` — the
/// same directory a healthy spec would resolve to, derived straight
/// from the name so the preview and the purge leg cannot drift from
/// the assembly's own anchor).
fn volume_home_under(base_dir: &Path, name: &str) -> Result<PathBuf> {
    let base = if base_dir.is_absolute() {
        base_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving the working directory")?
            .join(base_dir)
    };
    Ok(base.join(name))
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
/// directory (K21) and creates the home directory when missing: the
/// **filesystem** keys (db_path, cache_path, local_root) rebase onto
/// `<volumes_dir>/<name>/` when the volume file leaves them relative —
/// the defaults keep their file names but land inside the volume home.
/// `baidu_root` is NOT one of them: it is a backend namespace path
/// (validate demands a leading `/`), and on Windows
/// `Path::is_absolute()` is false for "/apps/x", so rebasing would
/// mangle it into `<home>/apps/x`. Idempotent: already-absolute fs
/// paths pass through untouched, so the production dispatch may resolve
/// first and the assembly resolve again.
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
    // sftp_private_key_path is a filesystem path like the three above
    // (review fix): a relative value in a volume file would otherwise be
    // read against the process CWD — wrong key file (or none) depending on
    // where cydrive was launched from. sftp_root stays a backend namespace
    // path (leading '/', same exemption as baidu_root).
    if let Some(key_path) = settings.sftp_private_key_path.as_deref() {
        settings.sftp_private_key_path = Some(
            resolve_volume_path(&home, key_path)
                .to_string_lossy()
                .into_owned(),
        );
    }
    Ok(settings)
}

/// The pieces of one running volume the aggregated stop task owns: each
/// volume's VFS drain and inbound worker join, released in the same
/// per-volume order the single-volume stop sequence uses.
struct VolumeStopUnit {
    vfs: Arc<Vfs>,
    inbound: cloudkit_core::inbound::InboundWorkerHandle,
}

impl VolumeStopUnit {
    /// The per-volume stop segment the aggregated stop task, REMOVE's
    /// commit and ADD's rollback all share (RV2's 单卷停段): the VFS
    /// drain (queue workers join, every enqueued job reaches a terminal
    /// state) then the inbound worker join — the order the stop sequence
    /// has always used.
    async fn release(self) {
        self.vfs.shutdown().await;
        self.inbound.shutdown().await;
    }
}

// ------------------------------------------------- RV2: live volume plumbing ---

/// The boxed reply future of one [`DriveRelease::release`] call.
pub type ReleaseFuture<'a> =
    std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// The K50 mount-release step, abstracted over the mount backend so the
/// removal sequence (and its abort path) is the same code for every
/// backend — and testable without a WinFsp runtime (tests inject this
/// trait). The
/// default implementations are the WF4 pieces: [`WinFspRelease`] runs
/// the real `MountHandle::unmount` (its own disappearance poll, the 10s
/// K50 window) on the blocking pool; [`WebDavRelease`] deletes the
/// `net use` mapping. Tests inject any [`DriveRelease`] through
/// [`MultiVolumeHandle::live_table`] to pin the abort path.
pub trait DriveRelease: Send {
    /// Release the volume's drive mount and confirm the letter is gone.
    /// `Err` carries the actionable reason the removal must abort (K50:
    /// a failed unmount leaves the volume registered — never a
    /// half-removal). Idempotence is the implementation's business (the
    /// winfsp handle is take-once; a spent release is a no-op Ok).
    fn release(&mut self) -> ReleaseFuture<'_>;
}

/// The winfsp arm of [`DriveRelease`] (K40/K50): the native adapter's
/// own unmount — the dispatcher stop plus the disappearance poll — on
/// the blocking pool (synchronous WinFsp calls; see
/// `cloudkit_winfsp::mount`'s threading notes). The handle sits in an
/// `Option` because `unmount` is take-once: a second release is a
/// no-op `Ok` (the idempotence the K50 abort + retry flow relies on).
#[cfg(all(windows, feature = "winfsp"))]
struct WinFspRelease {
    handle: Option<cloudkit_winfsp::mount::MountHandle>,
}

#[cfg(all(windows, feature = "winfsp"))]
impl DriveRelease for WinFspRelease {
    fn release(&mut self) -> ReleaseFuture<'_> {
        let handle = self.handle.take();
        Box::pin(async move {
            let Some(mut handle) = handle else {
                return Ok(());
            };
            match tokio::task::spawn_blocking(move || handle.unmount()).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(format!("the unmount task failed to complete: {error}")),
            }
        })
    }
}

/// The `net use /delete` budget (review M1-3): a hung network provider
/// must not wedge the K50 removal sequence (or an executor thread — the
/// command runs on the blocking pool). 30s comfortably exceeds a healthy
/// mapping delete (sub-second) while staying well inside the client
/// exchange budget. The timeout branch is untestable offline (a real
/// `net use` cannot be made to hang on demand) — its cover is this
/// comment plus code review; the error path IS pinned
/// (`webdav_release_propagates_the_net_use_failure` and the K50 abort
/// integration pin).
const NET_USE_RELEASE_BUDGET: Duration = Duration::from_secs(30);

/// The WebDAV arm of [`DriveRelease`]: the `net use /delete` that
/// releases the cross-process mapping (the stop sequence's own release,
/// now per volume). A failure is the K50 abort reason verbatim.
/// Constructible (pub) so the tests inject the production release
/// object through the [`VolumeLiveTable::set_release`] seam.
pub struct WebDavRelease {
    /// The drive letter the mapping delete targets (`"Q:"`).
    pub letter: String,
}

impl DriveRelease for WebDavRelease {
    fn release(&mut self) -> ReleaseFuture<'_> {
        let letter = self.letter.clone();
        Box::pin(async move {
            // The mapping delete is a synchronous `std::process::Command`
            // — on the blocking pool (never the executor thread) and
            // inside the budget above: a hung provider aborts the
            // removal per K50 (卷保持注册不动) instead of wedging the
            // control loop.
            let deleted = tokio::time::timeout(
                NET_USE_RELEASE_BUDGET,
                tokio::task::spawn_blocking({
                    let letter = letter.clone();
                    move || cloudkit_platform::windows::unmount_drive(&letter)
                }),
            )
            .await;
            match deleted {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(error))) => Err(format!("`net use {letter} /delete` failed: {error}")),
                Ok(Err(join_error)) => Err(format!(
                    "the `net use {letter} /delete` task failed to complete: {join_error}"
                )),
                Err(_elapsed) => Err(format!(
                    "`net use {letter} /delete` did not finish within {}s — the network \
                     provider appears hung; the removal was aborted and the volume stays \
                     registered (K50)",
                    NET_USE_RELEASE_BUDGET.as_secs()
                )),
            }
        })
    }
}

/// One live volume's runtime plumbing (RV2): the stop segment's pieces,
/// the volume's own periodic sync task, and — when the volume claimed a
/// drive letter — the mount record plus the backend release step. ADD
/// creates entries, REMOVE consumes them, the stop task drains whatever
/// remains; all three go through [`VolumeLiveTable`].
struct LiveVolume {
    stop_unit: VolumeStopUnit,
    sync_task: Option<tokio::task::JoinHandle<()>>,
    mount: Option<MountedVolume>,
    release: Option<Box<dyn DriveRelease>>,
}

/// The live volumes' shared table (RV2): what the boot assembled and
/// ADD grew and REMOVE shrank — the single structure the stop sequence,
/// the control commands and the mount accessors read, replacing the
/// boot-time `Vec` snapshots. A short-critical-section std mutex (no
/// await runs under the lock); clones of the payload come out, never
/// references.
#[derive(Default)]
pub struct VolumeLiveTable {
    entries: std::sync::Mutex<Vec<(String, LiveVolume)>>,
}

impl VolumeLiveTable {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, LiveVolume)>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers one live volume (append order = registration order).
    fn insert(&self, name: &str, entry: LiveVolume) {
        self.lock().push((name.to_owned(), entry));
    }

    /// Takes the volume's entry out (REMOVE's first move); the caller
    /// either commits the removal or puts the entry back with
    /// [`VolumeLiveTable::insert`] when a K50 step aborts. The
    /// find-and-remove is atomic under ONE lock hold (review H1): the
    /// previous two-lock form let a concurrent [`VolumeLiveTable::
    /// take_all`] empty the vec between the position lookup and the
    /// remove, panicking out of bounds and killing the control loop.
    fn take(&self, name: &str) -> Option<LiveVolume> {
        let mut guard = self.lock();
        let position = guard
            .iter()
            .position(|(registered, _)| registered == name)?;
        Some(guard.remove(position).1)
    }

    /// Re-attaches the mount record + release step of a volume that
    /// mounted (`None` release = the backend needs none here).
    fn attach_mount(
        &self,
        name: &str,
        mount: MountedVolume,
        release: Option<Box<dyn DriveRelease>>,
    ) {
        if let Some((_, entry)) = self
            .lock()
            .iter_mut()
            .find(|(registered, _)| registered == name)
        {
            entry.mount = Some(mount);
            entry.release = release;
        }
    }

    /// Replaces the volume's release step — the injected-probe seam the
    /// K50 abort-path tests use (the winfsp unmount is the production
    /// implementation behind the same [`DriveRelease`] trait). `false`
    /// when no live volume carries that name.
    pub fn set_release(&self, name: &str, release: Box<dyn DriveRelease>) -> bool {
        match self
            .lock()
            .iter_mut()
            .find(|(registered, _)| registered == name)
        {
            Some((_, entry)) => {
                entry.release = Some(release);
                true
            }
            None => false,
        }
    }

    /// The mounts, in registration order (the banner and
    /// `mounted_volumes` face).
    fn mounted_volumes(&self) -> Vec<MountedVolume> {
        self.lock()
            .iter()
            .filter_map(|(_, entry)| entry.mount.clone())
            .collect()
    }

    /// The drive letters actually mounted (the `mounted_letters` face).
    fn mounted_letters(&self) -> Vec<String> {
        self.mounted_volumes()
            .into_iter()
            .map(|mount| mount.letter)
            .collect()
    }

    /// One volume's mount record (LIST's letter/backend columns).
    fn mount_of(&self, name: &str) -> Option<MountedVolume> {
        self.lock()
            .iter()
            .find(|(registered, _)| registered == name)
            .and_then(|(_, entry)| entry.mount.clone())
    }

    /// Snapshots the periodic sync tasks still live, emptying the
    /// entries' slots (JoinHandle is not Clone): shutdown's abort pass
    /// takes them exactly once; REMOVE-committed volumes no longer carry
    /// theirs. The same semantics the boot-time Vec had.
    fn take_sync_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let mut guard = self.lock();
        let tasks = guard
            .iter_mut()
            .filter_map(|(_, entry)| entry.sync_task.take())
            .collect();
        drop(guard);
        tasks
    }

    /// Drains the whole table (the stop task's teardown pass): every
    /// remaining entry comes out for its release + stop segment.
    fn take_all(&self) -> Vec<(String, LiveVolume)> {
        std::mem::take(&mut self.lock())
    }
}

impl std::fmt::Debug for VolumeLiveTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The entries are not Debug (task handles, trait objects); the
        // mount list is the diagnostic surface.
        f.debug_struct("VolumeLiveTable")
            .field("mounted", &self.mounted_volumes())
            .finish()
    }
}

/// The assembled product of [`run_multi_with_transports`]: the volume
/// registry (statuses for the banner and tests), the ONE stop gate every
/// volume and the process-level control channel funnel into, the single
/// multi-volume WebDAV listener (K20 — `None` when its bind degraded,
/// see [`MultiVolumeHandle::webdav_addr`]), the single multi-volume
/// dashboard (K24 / MV3 — `None` when off or degraded, see
/// [`MultiVolumeHandle::web_ui_addr`]), the per-volume live table (RV2 —
/// drive letters, mounts and sync tasks live there and follow runtime
/// ADD/REMOVE; K27) and the stop task that owns the aggregated graceful
/// sequence.
pub struct MultiVolumeHandle {
    registry: RegistryHandle,
    watch: Arc<ShutdownWatch>,
    stop_task: tokio::task::JoinHandle<()>,
    /// The live volumes (RV2): per-volume stop segments, sync tasks and
    /// mounts — grown and shrunk by the control channel's ADD/REMOVE,
    /// drained by the stop task. Replaces the boot-time Vec snapshots.
    live: Arc<VolumeLiveTable>,
    /// The single WebDAV listener's bound address (`None` = the bind
    /// degraded; the volumes' data planes keep running, K22).
    webdav_addr: Option<SocketAddr>,
    /// The single multi-volume dashboard's bound address (`None` =
    /// `enable_web_ui` off, or the bind degraded per the same K22
    /// policy).
    web_ui_addr: Option<SocketAddr>,
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
    /// The boot's registry handle (a shared clone — RV2's control
    /// channel mutates the SAME table through it; reads see the table
    /// as it stands when asked).
    pub fn registry(&self) -> RegistryHandle {
        self.registry.clone()
    }

    /// One volume's registry entry (name lookup). A snapshot clone —
    /// the shared table's lock is never held across the caller's work.
    pub fn volume(&self, name: &str) -> Option<VolumeRuntime> {
        self.registry.volume(name)
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
    /// mounts that only warned). A live query over the RV2 table: a
    /// runtime ADD/REMOVE is visible here immediately.
    pub fn mounted_letters(&self) -> Vec<String> {
        self.live.mounted_letters()
    }

    /// The mounts with the backend that carried each one (Phase 3 / WF4):
    /// the banner annotates every letter with it, so a degraded winfsp
    /// request is visible at a glance (`webdav (fallback: …)`). A live
    /// query over the RV2 table, in registration order.
    pub fn mounted_volumes(&self) -> Vec<MountedVolume> {
        self.live.mounted_volumes()
    }

    /// The live volumes' shared table (RV2): what the control channel's
    /// ADD/REMOVE mutate and the stop sequence drains. Published as the
    /// injection seam for the K50 abort-path tests (a stuck
    /// [`DriveRelease`] goes in through
    /// [`VolumeLiveTable::set_release`]); no CLI face reads it directly
    /// — the user-visible face is the control channel's `LIST`.
    pub fn live_table(&self) -> &Arc<VolumeLiveTable> {
        &self.live
    }

    /// One arm of the run flow's shutdown wait, the multi-volume analog
    /// of [`RunHandle::wait_for_stop_request`]: resolves when the shared
    /// stop gate fires (control STOP, `shutdown`, Ctrl+C in `run`).
    pub async fn wait_for_stop_request(&self) {
        self.watch.wait().await;
    }

    /// Fires the stop gate WITHOUT joining the stop task — the
    /// signal-arm semantic (`run`'s Ctrl+C/SIGTERM select cannot await;
    /// a real `shutdown` is trigger-then-join, this is the trigger
    /// alone). The aggregated stop sequence starts on its own task; a
    /// later [`MultiVolumeHandle::shutdown`] joins it (the trigger is
    /// idempotent). In-process callers — and the tests that need a
    /// gate fire mid-command, exactly where a signal would land — use
    /// this instead of racing the control channel's STOP (which queues
    /// behind an in-flight volume command).
    pub fn request_stop(&self) {
        self.watch.trigger();
    }

    /// Graceful stop for every volume: fires the ONE gate (a no-op when
    /// a source already did), aborts each live volume's periodic sync
    /// task (same abort-not-drain choice and rationale as
    /// [`RunHandle::shutdown`]; a non-cancelled join error there is a
    /// warn, not a stop-sequence failure), then joins the stop task,
    /// which drains every remaining live volume's upload queue, joins
    /// its inbound worker, releases its drive mount and finally removes
    /// the process-level control file. The stop task's own join error
    /// propagates — a panicked stop sequence is a failed shutdown, not a
    /// clean one.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            watch,
            stop_task,
            live,
            ..
        } = self;
        watch.trigger();
        for task in live.take_sync_tasks() {
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

/// The runtime transport dispatch for ADD (RV2 / K48): given a parsed
/// volume spec, connect its transport and produce the per-boot options —
/// the same two arms the production run dispatch runs, injected so the
/// library stays driver-facing only. `Ok(None)` = the connect was
/// interrupted; the ADD answers without registering anything.
pub type DispatchFuture<'a> = std::pin::Pin<
    Box<
        dyn Future<Output = anyhow::Result<Option<(RunOptions, Arc<dyn CloudTransport>)>>>
            + Send
            + 'a,
    >,
>;
pub type VolumeTransportDispatch =
    Arc<dyn for<'a> Fn(&'a VolumeConfig) -> DispatchFuture<'a> + Send + Sync + 'static>;

/// The boxed mount outcome of one [`RuntimeMount`] call (review H3's
/// seam — the same shape [`mount_volumes_if_configured`] returns).
pub type RuntimeMountFuture<'a> = std::pin::Pin<Box<dyn Future<Output = VolumeMounts> + Send + 'a>>;

/// The runtime ADD's mount step behind a seam (review H3): receives the
/// ADD's one claim (the volume's name and its configured letter) and
/// answers what the mount pass produced. Production (`None`) runs the
/// SAME [`mount_volumes_if_configured`] pass boot runs; the tests inject
/// stubs that block or fail the mount deterministically, so the
/// publish-after-mount ordering is pinnable without a real drive.
pub type RuntimeMount =
    Arc<dyn for<'a> Fn(&'a str, &'a str) -> RuntimeMountFuture<'a> + Send + Sync + 'static>;

/// K50's abort-timeout knobs (the removal sequence's two waits). The
/// defaults are the production budgets; [`RemoveTuning::fast`] shrinks
/// them to millisecond scale for the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveTuning {
    /// How long the drain step waits for the upload queue's in-flight
    /// jobs to reach a terminal state before the removal aborts (K50:
    /// 任一步超时 → 卷保持注册不动).
    pub drain_timeout: Duration,
    /// Cadence of the queue-idle poll inside the drain window.
    pub poll_interval: Duration,
}

impl Default for RemoveTuning {
    fn default() -> Self {
        Self {
            // Uploads are legitimately slow; a drain abort must be rare
            // (the LIST pending count is the visible aid meanwhile).
            drain_timeout: Duration::from_secs(60),
            poll_interval: Duration::from_millis(100),
        }
    }
}

impl RemoveTuning {
    /// Millisecond-scale windows for the tests (never production).
    pub fn fast() -> Self {
        Self {
            drain_timeout: Duration::from_millis(800),
            poll_interval: Duration::from_millis(20),
        }
    }
}

/// R4's bounded-rebuild knobs (P2, `RebuildTuning`'s own row in
/// [`RuntimeVolumeCommands`]): the whole background rebuild runs under
/// `timeout` — an interrupted pass keeps its upserted rows (the rel_path
/// conflict key makes the merge idempotent) and, since Phase 8 / D8①,
/// its persisted cursor too, so a rerun continues where it stopped —
/// and the R5 checkpoint (instance identity + the shutdown gate + the
/// drained queue) is audited every `checkpoint_interval`. Deliberately
/// NOT config keys (可配性挂账): a struct injection keeps the surface at
/// zero while the tests reach millisecond scale through
/// [`RebuildTuning::fast`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildTuning {
    /// The whole background rebuild's budget (default 15 minutes).
    pub timeout: Duration,
    /// The R5 checkpoint cadence.
    pub checkpoint_interval: Duration,
    /// The per-pass entry cap (Phase 8 / D8②) fed to the core walk
    /// ([`rebuild::RebuildLimits::max_entries`]) — the graceful
    /// total-size bound: a capped pass stops between directories, keeps
    /// its persisted queue and reports the interrupted outcome ("rerun
    /// to continue"); a rerun resumes from the cursor.
    pub max_entries: usize,
}

impl Default for RebuildTuning {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15 * 60),
            checkpoint_interval: Duration::from_secs(1),
            max_entries: rebuild::RebuildLimits::default().max_entries,
        }
    }
}

impl RebuildTuning {
    /// Millisecond-scale windows for the tests (never production).
    pub fn fast() -> Self {
        Self {
            timeout: Duration::from_millis(300),
            checkpoint_interval: Duration::from_millis(50),
            max_entries: rebuild::RebuildLimits::default().max_entries,
        }
    }

    /// The core-walk bounds this tuning describes (D8②): the entry cap
    /// plus `time_budget` only when the caller has no supervisor of its
    /// own — the offline pass (the 15-minute budget's offline
    /// extension); the live executor passes `time_budget: None` because
    /// the R4 supervision aborts there.
    fn limits(&self, with_time_budget: bool) -> rebuild::RebuildLimits {
        rebuild::RebuildLimits {
            max_entries: self.max_entries,
            time_budget: with_time_budget.then_some(self.timeout),
        }
    }
}

/// The boxed outcome future of one [`RuntimeRebuild`] call (P2).
pub type RuntimeRebuildFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<rebuild::RebuildOutcome>> + Send + 'static>>;

/// The background rebuild executor behind a seam (P2, the mount seam's
/// twin): receives the volume's name and its RESOLVED settings, answers
/// the rebuild outcome. Production (`None` in
/// [`RuntimeVolumeCommands`]) runs the SAME `run_rebuild_command` the
/// offline `cydrive rebuild` uses (K11 gate + db open + the recursive
/// walk); the tests inject fakes that park on channels, fail, or count
/// calls — the R4/R5 supervision (timeout + checkpoint abort) lives
/// AROUND the executor, so a fake exercises it without a real backend.
pub type RuntimeRebuild =
    Arc<dyn Fn(&str, &CyDriveConfig) -> RuntimeRebuildFuture + Send + Sync + 'static>;

/// The RV2 boot extras: what the runtime-volume command surface needs
/// beyond the plain boot. [`Default`] (no dispatch, production tuning)
/// is exactly what [`run_multi_with_transports`] passes — LIST/REMOVE
/// work everywhere, only ADD needs a dispatch.
#[derive(Clone, Default)]
pub struct RuntimeVolumeCommands {
    /// The transport dispatch runtime ADD assembles volumes through
    /// (`None` = ADD refuses with the actionable rebuild-style message;
    /// the production `run` passes its real dispatch).
    pub dispatch: Option<VolumeTransportDispatch>,
    /// The K50 removal waits (tests shrink them).
    pub remove_tuning: RemoveTuning,
    /// The ADD mount step (review H3's seam): `None` (production) runs
    /// the real K27/K40 mount pass; the tests inject a stub that blocks
    /// or fails the mount to pin the publish-after-mount ordering.
    pub mount: Option<RuntimeMount>,
    /// The P2 background-rebuild waits (tests shrink them).
    pub rebuild_tuning: RebuildTuning,
    /// The background rebuild executor (P2): `None` (production) runs
    /// the real `run_rebuild_command`; the tests inject parking or
    /// failing fakes through the seam.
    pub rebuild: Option<RuntimeRebuild>,
}

/// The single-volume assembly result (RV2's 单卷装配): everything ONE
/// volume needs once its transport exists, regardless of who called —
/// the boot loop or a runtime ADD. The three face-table entries derive
/// from these pieces (the caller decides whether a failure registers a
/// `Failed` entry — boot — or refuses the ADD — the control channel).
struct AssembledVolume {
    /// The running registry entry (spec + transport + VFS).
    runtime: VolumeRuntime,
    /// The stop segment (VFS drain + inbound join).
    stop_unit: VolumeStopUnit,
    /// The volume's periodic sync task (K26), `None` when unconfigured.
    sync_task: Option<tokio::task::JoinHandle<()>>,
    /// The volume's FS adapter (the WebDAV face's per-volume handler).
    fs: CyDriveFs,
    /// The dashboard identity (K24) the web face registers.
    ui_config: cloudkit_web::WebUiConfig,
    /// The running VFS (the dashboard entry and LIST's pending count).
    vfs: Arc<Vfs>,
}

/// Assembles ONE volume (RV2's 单卷装配, the extraction of the boot
/// loop's per-volume body): resolves the settings against the volume
/// home (K21), builds the dashboard identity from the dispatched truth
/// (K24), declares the transport's capability line (R-5), builds the
/// volume core and spawns its periodic sync task when configured (K26),
/// and returns the pieces every face table consumes. On failure the
/// already-built dashboard identity comes back alongside the error —
/// the boot registers it as `Failed` (K22), a runtime ADD refuses.
async fn assemble_volume(
    process_cfg: &CyDriveConfig,
    spec: &VolumeConfig,
    options: &RunOptions,
    transport: Arc<dyn CloudTransport>,
    watch: &Arc<ShutdownWatch>,
) -> std::result::Result<AssembledVolume, (cloudkit_web::WebUiConfig, anyhow::Error)> {
    let name = spec.name.clone();
    // The dashboard identity pieces (K24), captured before the
    // transport moves into the core: the running arm reports the
    // dispatched truth (RunOptions' volume/quota payload plus the
    // transport's own capability bits — R4, never hardcoded).
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
    let (runtime, stop_unit, sync_task, fs) = build_volume_runtime(spec, options, transport, watch)
        .await
        .map_err(|error| (ui_config.clone(), error))?;
    let vfs = Arc::clone(runtime.vfs().expect("a running volume carries its vfs"));
    Ok(AssembledVolume {
        runtime,
        stop_unit,
        sync_task,
        fs,
        ui_config,
        vfs,
    })
}

/// Boots the multi-volume stack with the transports injected (Phase 2.5
/// / MV1's test seam; the production `run` dispatch builds the same
/// tuples per volume) — the plain face of
/// [`run_multi_with_transports_and_commands`], without a runtime ADD
/// dispatch (LIST/REMOVE still answer; ADD refuses actionably).
pub async fn run_multi_with_transports(
    process_cfg: &CyDriveConfig,
    volumes: Vec<(VolumeConfig, RunOptions, Arc<dyn CloudTransport>)>,
) -> Result<MultiVolumeHandle> {
    run_multi_with_transports_and_commands(
        process_cfg,
        volumes,
        RuntimeVolumeCommands::default(),
        false,
    )
    .await
}

/// [`run_multi_with_transports`] with the runtime-volume command surface
/// (Phase 3.6 / RV2, K48): the process-level control channel gains
/// `ADD <name>` / `REMOVE <name>` / `LIST` — ADD assembles a volume at
/// runtime through `commands.dispatch` and registers it into all three
/// faces at once (master registry, WebDAV dispatch, dashboard — the cli
/// master table is the truth, the faces are projections), REMOVE runs
/// the K50 safe sequence, LIST reports the live set. Commands are
/// serialized behind the boot wiring's command gate (K58-FB — every
/// entrypoint's execution takes the same single permit), so they never
/// interleave, whichever face dispatched them.
///
/// Boot itself is unchanged: one volume core per spec (db + cache + VFS
/// with requeue and inbound worker, K21-resolved paths), one periodic sync
/// task per configured volume (K26), ONE process-level stop gate plus
/// control channel (K25), and the single multi-volume WebDAV listener
/// routing `/vol/<name>/` to every RUNNING volume (K20 / MV2 — failed
/// volumes stay out of the route table; their status is the registry's
/// to report). Volumes with an explicit `drive_letter` mount through
/// the process WebDAV port (K27).
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
/// `/api/volumes` listing and the K23 `?volume=` routing. The dashboard
/// receives the SAME volume-command handler the control channel runs
/// (web volume management §1.1): its management routes send commands
/// through the shared seam, so both trigger sources serialize behind
/// one queue — the handler's own single-permit gate (K58-FB), with
/// each execution detached from its caller's wait (a route budget that
/// elapses abandons the REPLY, never the command).
///
/// `first_run` (web first-run bootstrap FR1 / D3) marks the init mode a
/// `cydrive run` enters right after [`bootstrap_first_run_cwd`]
/// generated a fresh configuration: with it, gate B (the
/// no-enabled-volumes refusal) lets the EMPTY assembly through — the
/// dashboard binds the empty registry so the first volume can be added
/// through the web UI, the WebDAV listener stays unbound (the empty
/// face set is a structural `None`), and the control channel answers
/// `LIST` with zero volumes. On the bound dashboard a first run also
/// opens the system browser at the dashboard address (suppressed with
/// the `CYDRIVE_NO_OPEN_BROWSER` env var; a failed bind only warns —
/// edit config.toml and run again).
pub async fn run_multi_with_transports_and_commands(
    process_cfg: &CyDriveConfig,
    volumes: Vec<(VolumeConfig, RunOptions, Arc<dyn CloudTransport>)>,
    commands: RuntimeVolumeCommands,
    first_run: bool,
) -> Result<MultiVolumeHandle> {
    // The double-start guard (see the single-volume call site): refuse a
    // second boot over a live instance before touching any volume.
    control::ensure_not_running(process_cfg).await?;
    let watch = Arc::new(ShutdownWatch::new());
    let live: Arc<VolumeLiveTable> = Arc::new(VolumeLiveTable::default());
    let mut runtimes: Vec<VolumeRuntime> = Vec::new();
    let mut volume_fses: Vec<(String, CyDriveFs)> = Vec::new();
    let mut ui_entries: Vec<cloudkit_web::VolumeUiEntry> = Vec::new();

    for (spec, options, transport) in volumes {
        let name = spec.name.clone();
        match assemble_volume(process_cfg, &spec, &options, transport, &watch).await {
            Ok(assembled) => {
                tracing::info!(volume = %name, "volume is running");
                volume_fses.push((name.clone(), assembled.fs));
                ui_entries.push(cloudkit_web::VolumeUiEntry {
                    name: name.clone(),
                    status: cloudkit_web::VolumeUiStatus::Running,
                    config: assembled.ui_config,
                    vfs: Some(Arc::clone(&assembled.vfs)),
                });
                runtimes.push(assembled.runtime);
                live.insert(
                    &name,
                    LiveVolume {
                        stop_unit: assembled.stop_unit,
                        sync_task: assembled.sync_task,
                        mount: None,
                        release: None,
                    },
                );
            }
            Err((ui_config, error)) => {
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
            }
        }
    }

    if runtimes.is_empty() && !first_run {
        // Review M4: reachable since RV0 — every volume file disabled.
        // The assembly entry main.rs boots through IS the actionable
        // face (the skip itself is only an info line in the log), so
        // the bail rescan counts the volume files and names the two
        // ways out instead of the bare structural text.
        anyhow::bail!("{}", no_enabled_volumes_message(process_cfg));
    }
    let registry = RegistryHandle::new(runtimes);
    // The all-failed gate reads `all` — over an EMPTY registry that is
    // vacuously true, so it is only meaningful when something actually
    // assembled. The first-run boot reaches here with an empty registry
    // BY DESIGN (gate B above let it through); every other empty
    // assembly bailed one branch earlier.
    if !registry.volumes().is_empty() && registry.all_failed() {
        anyhow::bail!(
            "every volume failed to assemble ({}): {} — fix the reported volume \
             configurations and run again",
            registry.status_list().len(),
            registry
                .status_list()
                .iter()
                .map(|(name, status)| format!("{name}={}", status.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if first_run && registry.status_list().is_empty() {
        // The first-run init banner (D4): the configuration was just
        // generated and nothing serves yet — point at the dashboard the
        // boot is about to bind. The URL names the configured address
        // (the template pins 127.0.0.1:8486); the actual bound address
        // is announced by the boot's own web-UI line and the browser
        // opens there.
        tracing::info!(
            url = %format!("http://{}:{}", process_cfg.web_ui_host, process_cfg.web_ui_port),
            "first run: no volumes yet — add your first volume through the web \
             dashboard's volumes page (the ＋ Add Volume button)"
        );
    }

    // The ONE WebDAV listener (K20): `/vol/<name>/` routes to every
    // running volume — through the shared face table (RV1/K51) the
    // listener re-reads per request, so RV2's runtime ADD/REMOVE is
    // visible on the same port. A bind failure degrades (K22, the same
    // policy as the control channel but with the bigger blast radius
    // spelled out): the volumes' data planes — queues, sync, inbound —
    // keep running; the WebDAV face is simply absent. The boot keeps a
    // clone of the face handle: RV2's ADD/REMOVE mutate the same table.
    let webdav_face = cloudkit_webdav::RegistryHandle::new(volume_fses);
    // First run (FR1 fix): the empty registry still binds — the dashboard
    // is the boot's product, and a drive-letter volume added through it
    // mounts THROUGH this listener (the default `net use` backend's
    // endpoint); leaving it unbound would roll every runtime ADD with a
    // claimed letter back (the registry face is a per-request
    // projection, so the empty bind serves late-added volumes).
    let webdav_server = bind_multi_webdav(process_cfg, webdav_face.clone(), first_run).await;
    let webdav_addr = webdav_server.as_ref().map(WebDavServer::local_addr);

    // The ONE dashboard (K24 / MV3): a single process-level port serving
    // the volume registry (per-volume tabs, `/api/volumes` aggregate) —
    // through the dashboard's own shared face table (RV1/K51). Same K22
    // degrade policy as the WebDAV bind: a failure only logs an error —
    // the volumes keep running without the UI. The boot keeps a clone
    // for the same reason.
    let web_face = cloudkit_web::RegistryHandle::new(ui_entries);

    // The ONE volume-command handler (web volume management plan §1.1):
    // the same closure serves the control channel AND the dashboard's
    // command seam. K58-FB re-wired it so that BOTH claims about it are
    // now actually true:
    //
    // - SERIALIZATION (review H2): every invocation spawns a detached
    //   execution task that must first take `command_gate` — a
    //   single-permit mutex — so control-channel commands, web-route
    //   commands and web×web commands all queue behind the one
    //   execution (the K48 semantics the web seam used to bypass by
    //   running the handler inline on its own axum task).
    // - CANCELLATION SAFETY (review H1): the execution task owns the
    //   command; the future this closure returns only WAITS for the
    //   reply over a oneshot. A caller that stops waiting (the
    //   dashboard route's budget elapsing, a dropped web connection)
    //   abandons the WAIT, never the command — the same
    //   client-abandonment semantics the control channel's serialized
    //   loop always had. Pre-fix, the route budgets dropped the
    //   `surface.handle` future mid-K50, taking the volume's live
    //   entry with it and wedging the registry.
    //
    // Built before the dashboard bind so the bind can inject its `Arc`
    // clone (the web alias is structurally the same type; cloning
    // shares, it does not re-wrap).
    let in_flight = Arc::new(InFlightCommands::default());
    let command_gate = Arc::new(tokio::sync::Mutex::new(()));
    // The P2 rebuild executor resolution: the injected seam or the
    // production `run_rebuild_command` closure (the same body the
    // offline `cydrive rebuild` runs — K11 gate + db open + bounded
    // walk). D8②: the entry cap flows in from the tuning; the wall
    // clock stays the R4 supervision's business (this task's own
    // deadline), so the core walk gets `time_budget: None`.
    let rebuild_tuning = commands.rebuild_tuning;
    let rebuild_executor: RuntimeRebuild = commands.rebuild.clone().unwrap_or_else(move || {
        Arc::new(move |_name: &str, settings: &CyDriveConfig| {
            let settings = settings.clone();
            Box::pin(async move {
                run_rebuild_command_with_limits(&settings, rebuild_tuning.limits(false)).await
            })
        })
    });
    let command_surface = Arc::new(RuntimeVolumeControl {
        process_cfg: process_cfg.clone(),
        registry: registry.clone(),
        webdav: webdav_face.clone(),
        web: web_face.clone(),
        live: Arc::clone(&live),
        watch: Arc::clone(&watch),
        webdav_available: webdav_addr.is_some(),
        dispatch: commands.dispatch,
        mount: commands.mount,
        tuning: commands.remove_tuning,
        rebuilds: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        rebuild_tuning: commands.rebuild_tuning,
        rebuild: rebuild_executor,
    });
    let volume_command_handler: control::VolumeCommandHandler = {
        let surface = Arc::clone(&command_surface);
        let in_flight = Arc::clone(&in_flight);
        let command_gate = Arc::clone(&command_gate);
        Arc::new(move |line: &str| {
            let surface = Arc::clone(&surface);
            let gate = Arc::clone(&command_gate);
            // The idle barrier spans the TRUE execution: the guard
            // enters the moment the command is accepted from ANY face
            // and drops only when the execution task below reaches its
            // natural end — a caller abandoning its waiting future
            // cannot shorten it (the stop task's `wait_idle` therefore
            // still covers every mid-K50 entry hold, review H1's
            // original invariant).
            let guard = in_flight.enter();
            let line = line.to_owned();
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<String>();
            // The detached execution (K58-FB H1+H2). Deadlock audit —
            // why a plain single-permit Mutex is safe here:
            //
            // - the task acquires the gate FIRST and then awaits only
            //   inside `surface.handle` (dispatch, drain polls, mount);
            //   nothing on those paths routes back into this closure,
            //   so no execution ever re-acquires the gate it holds.
            //   The nested command callers (DISABLE/UPDATE/DESTROY)
            //   call the remove_volume/add_volume METHODS directly,
            //   never the command closure;
            // - the waiting side (the control accept loop, a web route
            //   task) holds NO lock while parked on `reply_rx`, so
            //   waiter → holder edges do not exist — the wait-for graph
            //   is a flat star around one mutex and cannot cycle;
            // - the stop task's idle barrier waits on the guard (not
            //   the gate), and a task queued behind a long command
            //   still finishes promptly once its turn comes: the gate
            //   refusal is the FIRST thing `handle` checks, so a
            //   post-shutdown command answers within one poll tick and
            //   releases its guard — `wait_idle` stays bounded.
            tokio::spawn(async move {
                let _in_flight = guard;
                let _serialized = gate.lock().await;
                // Panic isolation (M1's layering, relocated to where
                // the command actually runs): the task boundary
                // contains the unwind, the catch logs the payload and
                // converts it into the actionable reply — both faces
                // (control connection, web route) get an answer and
                // the serialization survives the panic. The payload is
                // our own command text and panic message — nothing
                // credential-shaped reaches the log.
                let reply = match AssertUnwindSafe(surface.handle(&line)).catch_unwind().await {
                    Ok(reply) => reply,
                    Err(panic) => {
                        let reason = panic
                            .downcast_ref::<&str>()
                            .map(|str| (*str).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "<non-string panic payload>".to_string());
                        tracing::error!(
                            command = %line,
                            %reason,
                            "a volume command execution panicked; the command surface stays up"
                        );
                        control::HANDLER_PANIC_REPLY.to_string()
                    }
                };
                // A caller that already gave up dropped the receiver:
                // the reply is simply discarded, the execution was
                // never affected (H1's whole point).
                let _ = reply_tx.send(reply);
            });
            Box::pin(async move {
                match reply_rx.await {
                    Ok(reply) => reply,
                    // The execution task ended without answering — only
                    // reachable when its very task was torn down (a
                    // runtime shutdown); answer the shared actionable
                    // text instead of parking the caller forever.
                    Err(_dropped_without_reply) => control::HANDLER_PANIC_REPLY.to_string(),
                }
            })
        })
    };
    let web_ui = bind_multi_web_ui(
        process_cfg,
        web_face.clone(),
        Some(Arc::clone(&volume_command_handler)),
    )
    .await;
    let web_ui_addr = web_ui.as_ref().map(WebUiServer::local_addr);
    // First-run browser hand-off (D4): on a freshly generated config the
    // dashboard IS the product — open it at the ACTUAL bound address once
    // the bind succeeded. The env kill-switch is the headless-server exit
    // (a server has no browser to open) and keeps automated boots (the
    // offline test suite) browser-free; the spawn is fire-and-forget and
    // deliberately carries NO behavioral assertion anywhere (a spawn side
    // effect — only its compilation on both platforms is gated). A
    // degraded bind warns instead: the init banner already named the URL,
    // but nothing answers there — editing config.toml and rerunning is
    // the way out.
    if first_run {
        match &web_ui {
            Some(server) if std::env::var_os("CYDRIVE_NO_OPEN_BROWSER").is_none() => {
                open_browser(&format!("http://{}", server.local_addr()));
            }
            Some(_) => {}
            None => tracing::warn!(
                "first run: the web dashboard did not bind — edit config.toml \
                 (web_ui_host / web_ui_port) and run `cydrive run` again; the \
                 volume-management UI lives there"
            ),
        }
    }

    // Per-volume mounts (K27 + K40): only volumes that EXPLICITLY set a
    // drive_letter claim a mount (the parsed default letter is a
    // placeholder, not a claim). The WebDAV backend needs the listener
    // (its mount target is the `/vol/<name>` URL); the in-process winfsp
    // backend does not — its claims are collected even when the bind
    // degraded, and a fallback that has no endpoint to land on says so.
    let mount_claims = match (
        webdav_addr.is_some(),
        process_cfg.mount_backend == MountBackend::Winfsp,
    ) {
        (true, _) | (false, true) => collect_mount_claims(&registry),
        (false, false) => Vec::new(),
    };
    let runtime_handle = tokio::runtime::Handle::current();
    let mut mounts = mount_volumes_if_configured(&MountPlan {
        process_cfg,
        claims: &mount_claims,
        registry: &registry,
        rt: &runtime_handle,
        webdav_available: webdav_addr.is_some(),
        winfsp: winfsp_capability(),
    })
    .await;
    // Each completed mount lands in its volume's live entry: the record
    // (letter + backend, the banner's payload) and the K50 release step
    // (the winfsp handle, or the net-use delete for the mapped ones).
    let mut winfsp_handles = mounts.winfsp.drain();
    for mount in mounts.mounted {
        let release = mount_release(&mount, &mut winfsp_handles);
        let name = mount.volume.clone();
        live.attach_mount(&name, mount, release);
    }

    // The process-level control channel (K25): same file name and
    // cwd-anchored location as the single-volume mode (the process
    // config's default db_path anchors the file in the working
    // directory), same optional-component degrade. RV2: the channel
    // serves the volume commands through the shared runtime state (the
    // handler built above — the dashboard got the same `Arc`).
    // The in-flight counter was created before the binds (a degraded
    // control bind keeps the stop task's idle barrier working — with no
    // handler installed it is trivially idle).
    let control_file = match control::ControlServer::bind(process_cfg).await {
        Ok(server) => {
            let path = control::control_file_path(process_cfg);
            tracing::info!(addr = %server.local_addr(), "control channel listening");
            let gate = Arc::clone(&watch);
            let handler = Arc::clone(&volume_command_handler);
            tokio::spawn(async move {
                if let Err(error) = server
                    .run_with_commands(move || gate.trigger(), Some(handler))
                    .await
                {
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
    // volume — drive mount release (winfsp unmount / net use delete)
    // BEFORE its VFS workers (the callbacks call into the Vfs, so the
    // host must be gone first; a failure only warns: a stuck letter
    // must never block the exit — rclone's "unmount has no force"
    // reality) — then the process-level control file. Exactly once for
    // any number of gate fires; the per-volume pass drains whatever the
    // runtime-ADD/REMOVE cycle left in the live table.
    let gate = Arc::clone(&watch);
    let stop_live = Arc::clone(&live);
    let stop_in_flight = Arc::clone(&in_flight);
    let stop_task = tokio::spawn(async move {
        watch.wait().await;
        if let Some(server) = &webdav_server {
            server.shutdown().await;
        }
        if let Some(web_ui) = &web_ui {
            web_ui.shutdown().await;
        }
        // The idle barrier (review H1): a volume command in flight when
        // the gate fired (Ctrl+C / SIGTERM race the serialized command
        // execution) may be holding an entry out of the live table
        // mid-K50 — draining NOW would orphan it. The in-flight command
        // observes the gate at its checkpoints and hands the entry back;
        // only then does this pass take everything (see
        // [`InFlightCommands`] for the invariant and the bounded-wait
        // argument — the guard spans the detached execution, so a web
        // caller that abandoned its wait cannot strand the entry).
        stop_in_flight.wait_idle().await;
        for (name, entry) in stop_live.take_all() {
            if let Some(mut release) = entry.release {
                match release.release().await {
                    Ok(()) => tracing::info!(volume = %name, "released the volume's drive mount"),
                    Err(error) => tracing::warn!(
                        volume = %name,
                        %error,
                        "releasing the volume's drive failed; continuing the shutdown"
                    ),
                }
            }
            entry.stop_unit.release().await;
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
    });
    Ok(MultiVolumeHandle {
        registry,
        watch: gate,
        stop_task,
        live,
        webdav_addr,
        web_ui_addr,
    })
}

/// Binds the single multi-volume dashboard (K24 / MV3) or skips it
/// (`enable_web_ui` off) or degrades visibly (K22): an address-parse
/// or bind failure logs an error and returns `None` — the volumes keep
/// running, only the UI face is gone. `commands` is the volume-command
/// seam (web volume management §1.1) injected into the dashboard's
/// management routes — the same `Arc` the control channel runs. The
/// §1.5 remote-administration ruling rides along: a non-loopback bind
/// keeps the management write family withheld unless the process key
/// `allow_remote_admin = true` opts in.
async fn bind_multi_web_ui(
    process_cfg: &CyDriveConfig,
    volumes: cloudkit_web::RegistryHandle,
    commands: Option<cloudkit_web::VolumeCommandClient>,
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
    match WebUiServer::serve_multi_with_remote_admin(
        volumes,
        bind,
        commands,
        process_cfg.allow_remote_admin,
    )
    .await
    {
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

/// The empty-assembly bail's message (review M4): a read-only rescan of
/// the volumes directory (cheap — a directory listing) separates the
/// RV0-reachable case (volume files exist, every one skipped as
/// `enabled = false`) from the structural one, and names the two ways
/// out. In the production flow an empty set reaching here IS the
/// all-disabled case: parse failures error out during discovery and
/// failed assemblies register `Failed` entries (kept alive by the
/// all-failed gate below).
fn no_enabled_volumes_message(process_cfg: &CyDriveConfig) -> String {
    let Some(dir) = process_cfg.volumes_dir.as_deref() else {
        return "multi-volume boot received no volumes to assemble".to_string();
    };
    match cloudkit_core::config::discover_volumes(Path::new(dir)) {
        Ok(files) if !files.is_empty() => format!(
            "every volume is disabled: all {} volume file(s) under `{}` set enabled = \
             false — flip the key back to true in the volume file(s) you want served, or \
             remove `volumes_dir` from config.toml to run single-volume",
            files.len(),
            dir
        ),
        _ => format!(
            "multi-volume boot received no enabled volumes to assemble — add one \
             `<name>.toml` per volume (enabled = true) under `{dir}`, or remove \
             `volumes_dir` from config.toml to run single-volume"
        ),
    }
}

/// Binds the single multi-volume WebDAV listener (K20) or degrades
/// visibly (K22): an address-parse or bind failure logs an error and
/// returns `None` — the volumes keep running, only the WebDAV face is
/// gone. An empty volume set cannot reach the bind on a configured
/// boot (the no-enabled-volumes and all-failed guards bail first); the
/// empty skip stays as a structural backstop for those — EXCEPT the
/// first-run boot, which arrives with an empty registry BY DESIGN and
/// binds anyway (`first_run`): the listener is the mount endpoint the
/// first dashboard-created drive-letter volume goes through, and the
/// registry face is a per-request projection that picks late-added
/// volumes up without a rebind (RV1).
async fn bind_multi_webdav(
    process_cfg: &CyDriveConfig,
    volumes: cloudkit_webdav::RegistryHandle,
    first_run: bool,
) -> Option<WebDavServer> {
    if volumes.is_empty() && !first_run {
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
fn collect_mount_claims(registry: &RegistryHandle) -> Vec<(String, String)> {
    registry
        .volumes()
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

// --------------------------------------- RV2: the volume command surface ---

/// The in-flight volume-command counter behind the stop task's idle
/// barrier (review H1): every handler invocation runs under an
/// [`InFlightGuard`] (+1 on entry, −1 with a notify on drop — a
/// cancelled future releases its slot too), and the stop task — after
/// the gate fires but BEFORE `VolumeLiveTable::take_all` — waits for
/// the count to reach zero.
///
/// Core invariant: once the gate has fired, no volume command mutates
/// the live table except to put an entry BACK (the entry refusal is
/// immediate; REMOVE's drain loop and commit point observe the gate
/// each tick and hand the entry back; ADD's checkpoint tears down a
/// volume that never served a request), so once the count is zero,
/// `take_all` is guaranteed to see every live entry — the orphaned-
/// entry race (REMOVE holding an entry out of the table while the stop
/// task drains it) cannot occur. The wait is bounded: every in-flight
/// command's tail after a gate fire is bounded (the K50 steps carry
/// their own deadlines, and a never-served volume's release joins idle
/// workers), so this barrier cannot deadlock the shutdown.
#[derive(Default)]
struct InFlightCommands {
    count: std::sync::atomic::AtomicUsize,
    idle: tokio::sync::Notify,
}

impl InFlightCommands {
    fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        InFlightGuard {
            tracker: Arc::clone(self),
        }
    }

    /// Resolves once no command is in flight. The double-check
    /// registers the notification BEFORE re-reading the counter, so a
    /// decrement landing between the check and the await cannot be
    /// missed.
    async fn wait_idle(&self) {
        loop {
            if self.count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return;
            }
            let notified = self.idle.notified();
            if self.count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// The +1/−1 guard around one volume-command invocation (see
/// [`InFlightCommands`]). Drop is the ONLY exit path — completion,
/// early return and cancellation all release the slot.
struct InFlightGuard {
    tracker: Arc<InFlightCommands>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if self
            .tracker
            .count
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            self.tracker.idle.notify_waiters();
        }
    }
}

/// `true` when the control line's first token is exactly `keyword` (the
/// connection layer trims the line, so the keyword starts at byte 0;
/// the match must end at whitespace so `CREATEX` never routes as
/// `CREATE`).
fn first_keyword_is(line: &str, keyword: &str) -> bool {
    let rest = line.strip_prefix(keyword);
    rest.is_some_and(|rest| rest.starts_with(char::is_whitespace) || rest.is_empty())
}

/// Splits `name <payload>` — the span after a CREATE/UPDATE keyword —
/// into the volume name and the payload REST OF THE LINE verbatim (the
/// payload is JSON whose strings may carry meaningful runs of whitespace;
/// the whitespace-token splitting of the other commands would mangle
/// them). `None` when there is no name token at all.
fn split_name_payload(rest: &str) -> Option<(&str, &str)> {
    let rest = rest.trim_start();
    let (name, tail) = rest.split_once(char::is_whitespace)?;
    Some((name, tail.trim_start()))
}

/// Converts one CREATE/UPDATE payload — a compact single-line JSON
/// object — into the toml::Table the controlled renderer consumes (web
/// volume management §1.2, P3). The key space is exactly
/// [`VOLUME_SCOPED_KEYS`] (a process-level key is refused with its
/// config.toml pointer, an unknown key with its own refusal); values go
/// through [`cloudkit_core::config::json_value_to_toml`], whose
/// null/empty-string → unset rule IS the write-only affordance (a
/// credential the form left empty overlays nothing) and whose type
/// refusals name the key, never the value (M3 — the payload may carry
/// credentials). Every refusal text is a complete `ERR: ...\n` reply.
fn volume_payload_table(payload: &str) -> Result<toml::Table, String> {
    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(error) => {
            return Err(format!(
                "ERR: the payload is not valid JSON: {error} — send one compact single-line \
                 JSON object of volume keys\n"
            ));
        }
    };
    let serde_json::Value::Object(fields) = value else {
        return Err(
            "ERR: the payload must be a JSON object of volume keys, not a bare value\n".to_string(),
        );
    };
    let mut table = toml::Table::new();
    for (key, value) in fields {
        if cloudkit_core::config::PROCESS_SCOPED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "ERR: key `{key}` is a process-level setting and belongs in config.toml, not \
                 in a volume — remove it from the payload\n"
            ));
        }
        if !cloudkit_core::config::VOLUME_SCOPED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "ERR: unknown key `{key}` — the payload accepts volume-scoped keys only \
                 (the backend selector, its credentials, the tuning keys, `enabled`)\n"
            ));
        }
        match cloudkit_core::config::json_value_to_toml(&key, &value) {
            Ok(Some(converted)) => {
                table.insert(key, converted);
            }
            // null / empty string: the write-only leave-alone rule — no
            // key in the table, so CREATE writes nothing and UPDATE
            // overlays nothing.
            Ok(None) => {}
            Err(error) => return Err(format!("ERR: {error}\n")),
        }
    }
    Ok(table)
}

/// Parses the volume-file text UPDATE already read as the raw explicit
/// table (the overlay base). K58-M5: the parse error routes through the
/// core redaction funnel ([`cloudkit_core::config::redact_credential_values`])
/// — this was the one toml parse surface that bare-concatenated the raw
/// error, whose embedded source line can carry a credential value (the
/// loader accepted the text a moment earlier, so a failure here means a
/// TOCTOU rewrite landed between the two reads — the fresher text's
/// values must not ride the ERR reply or the log).
fn parse_explicit_table_redacted(path: &Path, text: &str) -> Result<toml::Table, String> {
    toml::from_str(text).map_err(|error| {
        format!(
            "re-reading the volume file {} failed: {}",
            path.display(),
            cloudkit_core::config::redact_credential_values(&error.to_string())
        )
    })
}

/// The state the control channel's `ADD`/`REMOVE`/`LIST` commands act
/// through (RV2 / K48): the cli master registry (the truth), the two
/// face tables (the WebDAV dispatch and dashboard projections), the live
/// volumes and the pieces runtime ADD needs to assemble one. Commands
/// run serialized behind the command gate — the single-permit mutex the
/// boot wiring wraps every entrypoint's execution in (K58-FB: control
/// channel AND the dashboard's seam), so a plain-`&self` pass over
/// these interior-mutable tables cannot interleave no matter which face
/// dispatched the command.
struct RuntimeVolumeControl {
    process_cfg: CyDriveConfig,
    registry: RegistryHandle,
    webdav: cloudkit_webdav::RegistryHandle,
    web: cloudkit_web::RegistryHandle,
    live: Arc<VolumeLiveTable>,
    watch: Arc<ShutdownWatch>,
    /// Whether the single WebDAV listener bound at boot (the webdav
    /// arm's mount target — the same `webdav_available` the boot's own
    /// mount pass got).
    webdav_available: bool,
    dispatch: Option<VolumeTransportDispatch>,
    /// The ADD mount step (review H3's seam) — production runs the real
    /// pass (`None`), tests inject blockers/failures.
    mount: Option<RuntimeMount>,
    tuning: RemoveTuning,
    /// P2/R1: the per-volume rebuild state — a name present here has a
    /// background rebuild in flight; the value is its wall-clock start
    /// (the already-running refusal renders it `HH:MM:SS`). Guarded by
    /// a std Mutex touched from the serialized handler AND the
    /// background task's exit path; no await ever holds it.
    rebuilds: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::SystemTime>>>,
    /// The P2 rebuild waits (tests shrink them).
    rebuild_tuning: RebuildTuning,
    /// The resolved P2 executor (the injected seam or the production
    /// `run_rebuild_command` closure).
    rebuild: RuntimeRebuild,
}

impl RuntimeVolumeControl {
    /// One control line in, the verbatim reply out (K48's transport
    /// contract: every command answers, errors are actionable text, the
    /// connection never carries state across commands).
    async fn handle(&self, line: &str) -> String {
        // CREATE/UPDATE carry a JSON payload as their third span — the
        // payload is split off POSITIONALLY so its own whitespace
        // (significant inside JSON strings) survives verbatim; the
        // whitespace-token match below would mangle it.
        for keyword in ["CREATE", "UPDATE"] {
            if first_keyword_is(line, keyword) {
                let rest = line.split_once(char::is_whitespace).map_or("", |(_, r)| r);
                return match split_name_payload(rest) {
                    Some((name, payload)) if !payload.is_empty() => {
                        // The H1 gate entry point — the mutating family's
                        // shared refusal (see the match arm below for the
                        // rationale).
                        if self.watch.fired() {
                            return format!(
                                "ERR: {keyword} refused — the instance is shutting down; \
                                 volume changes are no longer accepted (`LIST` still \
                                 answers)\n"
                            );
                        }
                        match keyword {
                            "CREATE" => self.create_volume(name, payload).await,
                            _ => self.update_volume(name, payload).await,
                        }
                    }
                    _ => format!(
                        "ERR: usage: {keyword} <name> <json> — a volume name and one compact \
                         JSON object of volume keys (e.g. {{\"backend\": \"local\", \
                         \"local_root\": \"C:/data\"}}); volume names match \
                         ^[a-z][a-z0-9_-]{{0,31}}$\n"
                    ),
                };
            }
        }
        // DESTROY (P5) carries its two-leg protocol in literal second
        // and third words (`confirm`, then optionally `purge_local`) —
        // parsed positionally here, ahead of the three-token match
        // below (whose mutating arms demand a bare `<name>`).
        if first_keyword_is(line, "DESTROY") {
            let rest = line.split_once(char::is_whitespace).map_or("", |(_, r)| r);
            let mut parts = rest.split_whitespace();
            // (name, confirmed, purge_local)
            let invocation = match (parts.next(), parts.next(), parts.next()) {
                (Some(name), None, None) => Some((name, false, false)),
                (Some(name), Some("confirm"), None) => Some((name, true, false)),
                (Some(name), Some("confirm"), Some("purge_local")) => Some((name, true, true)),
                _ => None,
            };
            return match invocation {
                Some((name, confirmed, purge_local)) => {
                    // The gate's entry observation point (review H1):
                    // DESTROY joins the mutating family at keyword
                    // level (like CREATE/UPDATE's entry) — a
                    // shutting-down instance accepts no deletion (and
                    // no preview either: its paths go stale the moment
                    // the process exits).
                    if self.watch.fired() {
                        return "ERR: DESTROY refused — the instance is shutting down; volume \
                                changes are no longer accepted (`LIST` still answers)\n"
                            .to_string();
                    }
                    if confirmed {
                        self.destroy_volume(name, purge_local).await
                    } else {
                        self.destroy_preview(name)
                    }
                }
                None => "ERR: usage: DESTROY <name> — the preview (nothing executes); DESTROY \
                         <name> confirm — the destruction (unmount if running, delete the volume \
                         file); DESTROY <name> confirm purge_local — also delete the volume's \
                         local data directory (remote data is never touched either way)\n"
                    .to_string(),
            };
        }
        let mut tokens = line.split_whitespace();
        match (tokens.next(), tokens.next(), tokens.next()) {
            (Some("LIST"), None, None) => self.list(),
            // SHOW and CONFIGS are read-only like LIST (web volume
            // management §1.2): the shutdown gate refuses mutations,
            // never reads — the registry and the volume files stay valid
            // through it.
            (Some("SHOW"), Some(name), None) => self.show_volume(name),
            (Some("CONFIGS"), None, None) => self.configs(),
            (
                Some(cmd @ ("ADD" | "REMOVE" | "ENABLE" | "DISABLE" | "REBUILD")),
                Some(name),
                None,
            ) => {
                // The gate's entry observation point (review H1): once
                // the shutdown gate has fired, the mutating commands are
                // refused — the stop task's idle barrier is waiting for
                // in-flight commands to settle, and a fresh mutation
                // would race its `take_all`. LIST (and the other reads)
                // is read-only and the registry stays valid through the
                // shutdown, so it keeps answering. REBUILD joins the
                // mutating family (P2): a shutting-down instance accepts
                // no new background work. The refusal still goes back
                // over the reply channel (the connection task parks on
                // the one-shot).
                if self.watch.fired() {
                    return format!(
                        "ERR: {cmd} refused — the instance is shutting down; volume changes \
                         are no longer accepted (`LIST` still answers)\n"
                    );
                }
                match cmd {
                    // K58-M7: the ADD arm passes the reply through
                    // verbatim — the structured bool is CREATE/UPDATE's
                    // (and the unit tests') to consume.
                    "ADD" => self.add_volume(name).await.0,
                    "REMOVE" => self.remove_volume(name).await,
                    "DISABLE" => self.disable_volume(name).await,
                    "REBUILD" => self.rebuild_volume(name).await,
                    _ => self.enable_volume(name).await,
                }
            }
            (Some(cmd @ ("ADD" | "REMOVE" | "SHOW" | "ENABLE" | "DISABLE" | "REBUILD")), _, _) => {
                format!(
                    "ERR: usage: {cmd} <name> — exactly one volume name (volume names match \
                 ^[a-z][a-z0-9_-]{{0,31}}$)\n"
                )
            }
            (Some("CONFIGS"), _, _) => "ERR: usage: CONFIGS — takes no argument\n".to_string(),
            _ => "ERR: usage: ADD <name> | REMOVE <name> | ENABLE <name> | DISABLE <name> | \
                  SHOW <name> | REBUILD <name> | CREATE <name> <json> | UPDATE <name> <json> | \
                  DESTROY <name> [confirm [purge_local]] | LIST | CONFIGS\n"
                .to_string(),
        }
    }

    /// `LIST`: one line per registered volume —
    /// `name status letter backend pending=N` (K49's runtime read; the
    /// pending count is the §4 drain aid: the queue jobs REMOVE's drain
    /// step would wait for), with a trailing `rebuilding` marker while
    /// the volume has a background REBUILD in flight (P2/R1's progress
    /// face). The letter/backend columns name the actual mount; an
    /// unmounted volume carries `-` and its configured backend. A
    /// failed entry has no queue — `pending=-`.
    fn list(&self) -> String {
        let volumes = self.registry.volumes();
        let mut reply = format!("OK: {} volume(s)\n", volumes.len());
        for runtime in volumes {
            let name = runtime.name().to_owned();
            let status = runtime.status();
            let mount = self.live.mount_of(&name);
            let letter = mount
                .as_ref()
                .map(|mount| mount.letter.clone())
                .unwrap_or_else(|| "-".to_string());
            let backend = match &mount {
                Some(mount) => mount.backend.as_str().to_string(),
                None => runtime.spec().settings.backend.as_str().to_string(),
            };
            let pending = match runtime.vfs() {
                Some(vfs) => format!("pending={}", vfs.queue_stats().outstanding()),
                None => "pending=-".to_string(),
            };
            let rebuilding = self
                .rebuilds
                .lock()
                .map(|map| map.contains_key(&name))
                .unwrap_or(false);
            reply.push_str(&format!(
                "{name} {} {letter} {backend} {pending}{}\n",
                status.as_str(),
                if rebuilding { " rebuilding" } else { "" }
            ));
        }
        reply
    }

    /// Path-safety precheck before a volume name becomes a file name
    /// (shared by ADD and SHOW): the full name rules live in
    /// `load_volume_config`, but this stops `/`, `\`, `.`, `:` and
    /// whitespace from ever reaching a path join.
    fn name_is_path_safe(name: &str) -> bool {
        !name.is_empty()
            && !name
                .chars()
                .any(|c| matches!(c, '/' | '\\' | '.' | ':') || c.is_whitespace())
    }

    /// `SHOW <name>` (web volume management plan §1.2): the volume
    /// file's EXPLICIT configuration as one `OK: <single-line JSON>`
    /// reply — serialized by core's `volume_show_json`, so every
    /// credential-valued key collapses to `{"set": true/false}` and the
    /// VALUE never leaves the backend (write-only). Read-only: no gate
    /// refusal, no registry mutation — a volume file answers SHOW even
    /// while its volume is unregistered (REMOVE is runtime-only, K49).
    fn show_volume(&self, name: &str) -> String {
        if !Self::name_is_path_safe(name) {
            return format!(
                "ERR: `{name}` is not a volume name — SHOW takes the file stem of a \
                 volumes_dir entry (names match ^[a-z][a-z0-9_-]{{0,31}}$)\n"
            );
        }
        let Some(dir) = self.process_cfg.volumes_dir.as_deref() else {
            return "ERR: this instance runs single-volume mode (no volumes_dir) — \
                    SHOW serves multi-volume instances\n"
                .to_string();
        };
        let path = Path::new(dir).join(format!("{name}.toml"));
        if !path.is_file() {
            return format!(
                "ERR: no volume file for `{name}` at {} — SHOW reads the volumes_dir \
                 ({dir}); `LIST` shows the volumes this instance actually serves\n",
                path.display()
            );
        }
        match cloudkit_core::config::volume_show_json(&path) {
            Ok(json) => format!("OK: {json}\n"),
            Err(error) => format!(
                "ERR: reading the volume file {} failed: {error} — fix the file and \
                 retry\n",
                path.display()
            ),
        }
    }

    /// `CONFIGS` (web volume management P1): the configuration FULL set —
    /// every `*.toml` under the volumes_dir, one row per file, joined
    /// against the runtime registry. LIST cannot serve the dashboard's
    /// configuration page: it reads the registry, and a disabled volume
    /// is by definition absent from it. Row shapes (the web endpoint
    /// parses them back):
    ///
    /// - loadable file: `<name> <backend> enabled=<bool> running|absent`
    ///   (running = the registry has the name) with two trailing
    ///   sparse markers — `encrypted` while the file carries
    ///   `enable_encryption` (the web Refresh button's gating data,
    ///   P6) and `rebuilding` while a background REBUILD is in flight
    ///   (P2/R1's progress face);
    /// - schema-broken file: `<name> invalid (<reason>)` — one broken
    ///   file must not take the whole listing down (the K22 spirit).
    fn configs(&self) -> String {
        let Some(dir) = self.process_cfg.volumes_dir.as_deref() else {
            return "ERR: this instance runs single-volume mode (no volumes_dir) — \
                    CONFIGS serves multi-volume instances\n"
                .to_string();
        };
        let files = match cloudkit_core::config::discover_volumes(Path::new(dir)) {
            Ok(files) => files,
            Err(error) => return format!("ERR: {error}\n"),
        };
        let mut reply = format!("OK: {} volume file(s)\n", files.len());
        for path in files {
            let name = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            match cloudkit_core::config::load_volume_config(&path) {
                Ok(spec) => {
                    let running = if self.registry.volume(&spec.name).is_some() {
                        "running"
                    } else {
                        "absent"
                    };
                    // The sparse markers: emitted only when true, so a
                    // quiet row keeps its P1 shape byte-for-byte.
                    let mut markers = String::new();
                    if spec.settings.enable_encryption {
                        markers.push_str(" encrypted");
                    }
                    if self
                        .rebuilds
                        .lock()
                        .map(|map| map.contains_key(&spec.name))
                        .unwrap_or(false)
                    {
                        markers.push_str(" rebuilding");
                    }
                    reply.push_str(&format!(
                        "{} {} enabled={} {running}{markers}\n",
                        spec.name,
                        spec.settings.backend.as_str(),
                        spec.settings.enabled
                    ));
                }
                Err(error) => {
                    // The reason is already credential-scrubbed (the
                    // load funnel's M3 funnel); it may be multi-line
                    // (toml embeds the offending source) — the row
                    // protocol is line-oriented, so newlines fold.
                    let reason = error.to_string().replace('\n', " ");
                    reply.push_str(&format!("{name} invalid ({reason})\n"));
                }
            }
        }
        reply
    }

    /// The shared file-write segment of `ENABLE`/`DISABLE` (web volume
    /// management P1): validate through the full funnel, rewrite with
    /// the flag overlaid. `Err` carries the actionable refusal — the
    /// caller must not touch the runtime when it fires (the persistent
    /// file is the source of truth, K49).
    fn persist_enabled_flag(&self, name: &str, path: &Path, enabled: bool) -> Result<(), String> {
        match cloudkit_core::config::write_volume_enabled(path, enabled) {
            Ok(()) => Ok(()),
            Err(error) => Err(format!(
                "ERR: {} `{name}` failed writing the volume file {}: {error} — \
                 nothing was changed at runtime (the volume file is the persistent \
                 source of truth); fix the file and retry\n",
                if enabled { "ENABLE" } else { "DISABLE" },
                path.display()
            )),
        }
    }

    /// `DISABLE <name>` (web volume management P1): the narrow UPDATE —
    /// write `enabled = false` into the volume file FIRST, then, if the
    /// volume is running, take it down through the SAME K50 sequence
    /// `REMOVE` runs (直接调用 [`Self::remove_volume`] — the drain /
    /// release / unregister steps and every H1 observation point are
    /// reused, not duplicated). The write-first order is the K50
    /// philosophy: the persistent file is the source of truth, so a
    /// failed write leaves the runtime untouched; an unmount that fails
    /// after a successful write leaves a disabled file under a still
    /// running volume, which the idempotent retry (write is a no-op,
    /// unmount re-runs) resolves.
    async fn disable_volume(&self, name: &str) -> String {
        if !Self::name_is_path_safe(name) {
            return format!(
                "ERR: `{name}` is not a volume name — DISABLE takes the file stem of a \
                 volumes_dir entry (names match ^[a-z][a-z0-9_-]{{0,31}}$)\n"
            );
        }
        let Some(dir) = self.process_cfg.volumes_dir.as_deref() else {
            return "ERR: this instance runs single-volume mode (no volumes_dir) — \
                    DISABLE serves multi-volume instances\n"
                .to_string();
        };
        let path = Path::new(dir).join(format!("{name}.toml"));
        if !path.is_file() {
            return format!(
                "ERR: no volume file for `{name}` at {} — DISABLE reads the volumes_dir \
                 ({dir}); `CONFIGS` lists every volume file\n",
                path.display()
            );
        }
        if let Err(refusal) = self.persist_enabled_flag(name, &path, false) {
            return refusal;
        }
        if self.registry.volume(name).is_none() {
            return format!(
                "OK: disabled volume `{name}` (enabled = false written to {}; the \
                 volume was not running)\n",
                path.display()
            );
        }
        let removal = self.remove_volume(name).await;
        if removal.starts_with("OK:") {
            return format!(
                "OK: disabled volume `{name}` (enabled = false written to {}; volume \
                 unmounted and unregistered — the file keeps it disabled across \
                 restarts)\n",
                path.display()
            );
        }
        let reason = removal.trim().trim_start_matches("ERR: ");
        format!(
            "ERR: disabling `{name}` wrote the file (enabled = false at {}) but the \
             unmount failed: {reason} — retry `DISABLE {name}` once the cause is \
             resolved (the file write is idempotent)\n",
            path.display()
        )
    }

    /// `ENABLE <name>` (web volume management P1): write `enabled = true`
    /// into the volume file, then assemble through the SAME runtime ADD
    /// path ([`Self::add_volume`] verbatim reply — its refusals are
    /// already actionable: a duplicate registration, a missing file, a
    /// bad name; the structured bool is ENABLE's own verdict-free face,
    /// the reply text carries everything). The write-first order
    /// mirrors DISABLE's: a failed write never reaches the runtime.
    async fn enable_volume(&self, name: &str) -> String {
        if !Self::name_is_path_safe(name) {
            return format!(
                "ERR: `{name}` is not a volume name — ENABLE takes the file stem of a \
                 volumes_dir entry (names match ^[a-z][a-z0-9_-]{{0,31}}$)\n"
            );
        }
        let Some(dir) = self.process_cfg.volumes_dir.as_deref() else {
            return "ERR: this instance runs single-volume mode (no volumes_dir) — \
                    ENABLE serves multi-volume instances\n"
                .to_string();
        };
        let path = Path::new(dir).join(format!("{name}.toml"));
        if !path.is_file() {
            return format!(
                "ERR: no volume file for `{name}` at {} — ENABLE reads the volumes_dir \
                 ({dir}); `CONFIGS` lists every volume file\n",
                path.display()
            );
        }
        if let Err(refusal) = self.persist_enabled_flag(name, &path, true) {
            return refusal;
        }
        self.add_volume(name).await.0
    }

    /// `CREATE <name> <json>` (web volume management P3, plan §1.2):
    /// validate the name, refuse an existing file (UPDATE's territory),
    /// convert the payload through [`volume_payload_table`], render the
    /// controlled toml and run it through the FULL validation funnel —
    /// [`parse_volume_toml`] plus the cross-field `validate()` — BEFORE
    /// anything touches the disk (a refusal never creates a file). The
    /// write then lands through the "persistent file is the source of
    /// truth" order (K49's philosophy): `fs::write` FIRST, then the
    /// runtime assembly through the SAME [`Self::add_volume`] the
    /// control channel runs (mount claim, H3 publish-after-mount, every
    /// gate included) — so a failed ASSEMBLY leaves a saved file and a
    /// retryable state, exactly what the refusal then says.
    ///
    /// Credential discipline: the payload MAY carry credentials (the
    /// loopback channel is the same trust face as hand-editing the
    /// volume file), but no reply or log line carries a VALUE — the
    /// refusals above name keys (M3), and the tracing line logs the
    /// field-name list only.
    async fn create_volume(&self, name: &str, payload: &str) -> String {
        // The name precheck: the ONE rule the volume files enforce
        // (core's), before the name ever reaches a path join.
        if !cloudkit_core::config::is_valid_volume_name(name) {
            return format!(
                "ERR: invalid volume name `{name}`: volume names must match \
                 ^[a-z][a-z0-9_-]{{0,31}}$ (a lowercase letter first, then lowercase \
                 letters/digits/`_`/`-`, at most 32 characters) — no volume file was \
                 written\n"
            );
        }
        let Some(dir) = self.process_cfg.volumes_dir.as_deref() else {
            return "ERR: this instance runs single-volume mode (no volumes_dir) — CREATE \
                    serves multi-volume instances\n"
                .to_string();
        };
        let path = Path::new(dir).join(format!("{name}.toml"));
        if path.exists() {
            return format!(
                "ERR: a volume file for `{name}` already exists at {} — edit it instead \
                 (`UPDATE {name} <json>` from the dashboard, or hand-edit the file)\n",
                path.display()
            );
        }
        let table = match volume_payload_table(payload) {
            Ok(table) => table,
            Err(refusal) => return refusal,
        };
        // The pre-write validation funnel: the rendered file must parse
        // AND cross-field validate before `fs::write` (a refusal writes
        // nothing — pinned by tests).
        let text = cloudkit_core::config::render_volume_toml(&table, &toml::Table::new());
        let spec = match cloudkit_core::config::parse_volume_toml(&path, name, &text) {
            Ok(spec) => spec,
            Err(error) => {
                return format!(
                    "ERR: the generated volume file is invalid: {error} — nothing was \
                     written; fix the payload and retry\n"
                );
            }
        };
        if let Err(error) = spec.settings.validate() {
            return format!(
                "ERR: invalid volume settings: {error} — nothing was written; fix the \
                 payload and retry\n"
            );
        }
        // The field-name list only — no payload VALUE ever reaches a log
        // line (M3).
        let fields: Vec<&str> = table.keys().map(String::as_str).collect();
        tracing::info!(
            volume = name,
            fields = ?fields,
            "CREATE: writing a new volume file (field names only, by policy)"
        );
        // K58-H3: the atomic write — a torn CREATE (crash mid-write)
        // would leave a truncated file that boot refuses (or silently
        // drops trailing keys).
        if let Err(error) = cloudkit_core::config::write_config_atomically(&path, &text) {
            return format!(
                "ERR: writing the volume file {} failed: {error} — nothing was changed at \
                 runtime\n",
                path.display()
            );
        }
        println!(
            "Volume {name} created at {} (control CREATE).",
            path.display()
        );
        // A created-disabled volume never touches the runtime: the file
        // is the whole action (the ENABLE retry is the documented way
        // up).
        if !spec.settings.enabled {
            return format!(
                "OK: created volume `{name}` (disabled; file at {}) — `ENABLE {name}` \
                 starts it\n",
                path.display()
            );
        }
        // The runtime leg: the SAME add_volume the control channel's ADD
        // runs (assembly, mount claim, H3 semantics included). K58-M7:
        // the verdict is the structured bool — the reply text below is
        // display sugar only, so a future wording tweak can never flip
        // a real assembly into the failure branch.
        let (addition, added) = self.add_volume(name).await;
        if added {
            let state = addition
                .trim()
                .strip_prefix(&format!("OK: added volume `{name}` "))
                .map(|state| state.trim().to_owned())
                .unwrap_or_else(|| addition.trim().to_owned());
            format!(
                "OK: created volume `{name}` (file at {}; {})\n",
                path.display(),
                state
            )
        } else {
            format!(
                "ERR: the volume file was saved at {}, but assembling the volume failed: \
                 {} — fix the file (or send `UPDATE {name} <json>` with corrected fields) \
                 and retry with `ENABLE {name}` (or `ADD {name}`)\n",
                path.display(),
                addition.trim().trim_start_matches("ERR: ")
            )
        }
    }

    /// `UPDATE <name> <json>` (web volume management P4, plan §1.2): the
    /// payload names the fields to CHANGE; a credential key that is
    /// present and non-empty overwrites while a missing or empty one
    /// keeps the stored value (the write-only rule — the overlay works
    /// on the toml::Table level, so the stored credential values never
    /// pass through anything that logs). The order is
    /// file-first-K50-philosophy: validate the OLD file through the
    /// full funnel (a half-validated file is never "repaired" by a
    /// rewrite), overlay + render + validate the MERGED text before the
    /// write, `fs::write`, then the re-assembly — REMOVE (the same K50
    /// sequence) + ADD — for a volume that is registered and stays
    /// enabled; every mixed state the re-assembly can leave (file new /
    /// runtime old, file new / volume down) answers with its own
    /// actionable text and the comments-loss note (裁决①: the rewrite
    /// drops hand-written comments — surfaced, not preserved).
    async fn update_volume(&self, name: &str, payload: &str) -> String {
        if !Self::name_is_path_safe(name) {
            return format!(
                "ERR: `{name}` is not a volume name — UPDATE takes the file stem of a \
                 volumes_dir entry (names match ^[a-z][a-z0-9_-]{{0,31}}$)\n"
            );
        }
        let Some(dir) = self.process_cfg.volumes_dir.as_deref() else {
            return "ERR: this instance runs single-volume mode (no volumes_dir) — UPDATE \
                    serves multi-volume instances\n"
                .to_string();
        };
        let path = Path::new(dir).join(format!("{name}.toml"));
        if !path.is_file() {
            return format!(
                "ERR: no volume file for `{name}` at {} — UPDATE edits an existing volume \
                 (`CREATE {name} <json>` writes a new one)\n",
                path.display()
            );
        }
        // The full funnel on the OLD file first: a half-validated file is
        // never rewritten (the write_volume_enabled precedent).
        if let Err(error) = cloudkit_core::config::load_volume_config(&path) {
            return format!(
                "ERR: reading the volume file {} failed: {error} — UPDATE never rewrites \
                 a file the loader would refuse; fix it by hand and retry\n",
                path.display()
            );
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                return format!(
                    "ERR: reading the volume file {} failed: {error}\n",
                    path.display()
                );
            }
        };
        let explicit = match parse_explicit_table_redacted(&path, &text) {
            Ok(table) => table,
            Err(message) => return format!("ERR: {message}\n"),
        };
        let overlay = match volume_payload_table(payload) {
            Ok(table) => table,
            Err(refusal) => return refusal,
        };
        if overlay.is_empty() {
            return "ERR: the payload sets no fields to change — send at least one volume \
                    key (a credential left empty means keep, not clear)\n"
                .to_string();
        }
        let merged = cloudkit_core::config::render_volume_toml(&explicit, &overlay);
        let spec = match cloudkit_core::config::parse_volume_toml(&path, name, &merged) {
            Ok(spec) => spec,
            Err(error) => {
                return format!(
                    "ERR: the merged volume file is invalid: {error} — nothing was \
                     written; fix the payload and retry\n"
                );
            }
        };
        if let Err(error) = spec.settings.validate() {
            return format!(
                "ERR: invalid volume settings: {error} — nothing was written; fix the \
                 payload and retry\n"
            );
        }
        // The field-name list only — the overlay's (or the file's)
        // credential values never reach a log line (M3).
        let fields: Vec<&str> = overlay.keys().map(String::as_str).collect();
        tracing::info!(
            volume = name,
            fields = ?fields,
            "UPDATE: rewriting the volume file (field names only, by policy)"
        );
        // K58-H3: the atomic write — UPDATE rewriting a LIVE volume file
        // in place is exactly the torn window the primitive closes.
        if let Err(error) = cloudkit_core::config::write_config_atomically(&path, &merged) {
            return format!(
                "ERR: writing the volume file {} failed: {error} — the runtime was not \
                 touched (the volume keeps its current configuration)\n",
                path.display()
            );
        }
        println!(
            "Volume {name} updated at {} (control UPDATE).",
            path.display()
        );
        let comments = "note: the volume file was rewritten — hand-written comments are lost";
        // A file that ended up disabled: the DISABLE semantics (no
        // re-add), through the same remove_volume.
        if !spec.settings.enabled {
            if self.registry.volume(name).is_none() {
                return format!(
                    "OK: updated volume `{name}` (file at {}; the volume is not running \
                     and its file keeps enabled = false — `ENABLE {name}` starts it on \
                     the new settings); {comments}\n",
                    path.display()
                );
            }
            let removal = self.remove_volume(name).await;
            if removal.starts_with("OK:") {
                return format!(
                    "OK: updated volume `{name}` (enabled = false at {}; the volume was \
                     unmounted); {comments}\n",
                    path.display()
                );
            }
            let reason = removal.trim().trim_start_matches("ERR: ");
            return format!(
                "ERR: the file was updated (enabled = false at {}) but the unmount \
                 failed: {reason} — retry `DISABLE {name}` (the file write is \
                 idempotent); {comments}\n",
                path.display()
            );
        }
        // A volume that is not registered: the file write is the whole
        // action — assembling what ENABLE owns is not UPDATE's business.
        if self.registry.volume(name).is_none() {
            return format!(
                "OK: updated volume `{name}` (file at {}; the volume is not running — \
                 `ENABLE {name}` starts it on the new settings); {comments}\n",
                path.display()
            );
        }
        // The re-assembly: REMOVE (the same K50 sequence) then ADD. Each
        // mixed state the two legs can leave answers with its own text.
        let removal = self.remove_volume(name).await;
        if !removal.starts_with("OK:") {
            let reason = removal.trim().trim_start_matches("ERR: ");
            return format!(
                "ERR: the volume file was updated ({}) but the volume still runs its \
                 OLD configuration: {reason} — drain the pending uploads (`LIST` shows \
                 the count) and retry `REMOVE {name}` + `ENABLE {name}`; {comments}\n",
                path.display()
            );
        }
        let (addition, added) = self.add_volume(name).await;
        // K58-M7: the structured verdict classifies the re-assembly;
        // the strip is display sugar only (see CREATE's twin comment).
        if added {
            let state = addition
                .trim()
                .strip_prefix(&format!("OK: added volume `{name}` "))
                .map(|state| state.trim().to_owned())
                .unwrap_or_else(|| addition.trim().to_owned());
            format!(
                "OK: updated volume `{name}` (re-assembled: {}); {comments}\n",
                state
            )
        } else {
            format!(
                "ERR: the volume file was updated ({}) and the volume is now UNMOUNTED: \
                 {} — fix the file and `ENABLE {name}`; {comments}\n",
                path.display(),
                addition.trim().trim_start_matches("ERR: ")
            )
        }
    }

    /// `DESTROY <name>` (web volume management P5, plan §1.2/§2.2): the
    /// two-leg protocol's PREVIEW — executes nothing, answers the
    /// preview. An `OK:` (this is a confirmation request, not an
    /// error): what a confirm would do (the K50 unmount for a running
    /// volume, the volume file's deletion), what stays by default (the
    /// local data directory, with its path and the `purge_local` way
    /// out — 裁决②) and what is never touched (remote data — K49 分档's
    /// absolute), plus the exact confirmation command.
    fn destroy_preview(&self, name: &str) -> String {
        if let Some(refusal) = self.destroy_precheck(name) {
            return refusal;
        }
        let (path, home) = match self.destroy_paths(name) {
            Ok(pair) => pair,
            Err(error) => {
                return format!(
                    "ERR: resolving the paths of `{name}` failed: {error:#} — nothing was \
                     executed\n"
                );
            }
        };
        if !path.is_file() {
            return format!(
                "ERR: no volume file for `{name}` at {} — DESTROY reads the volumes_dir; \
                 `CONFIGS` lists every volume file (a running volume without a file unloads \
                 via `REMOVE {name}`)\n",
                path.display()
            );
        }
        let running = self.registry.volume(name).is_some();
        let unmount = if running {
            format!(
                "- unmount: `{name}` is running — the confirm leg unloads it through the \
                 REMOVE safety order (drain uploads, release the drive, unregister); a busy \
                 queue aborts the WHOLE destroy\n"
            )
        } else {
            format!(
                "- unmount: `{name}` is not running — nothing to unload, the confirm leg \
                 deletes the file directly\n"
            )
        };
        format!(
            "OK: DESTROY preview for `{name}` — nothing was executed:\n{unmount}\
             - delete: the volume file {}\n\
             - kept: the local data directory {} stays by default (`DESTROY {name} confirm \
             purge_local` deletes it too)\n\
             - never touched: the data on the volume's remote backend\n\
             confirm with: `DESTROY {name} confirm`\n",
            path.display(),
            home.display()
        )
    }

    /// The shared refusal ladder of DESTROY's two legs: the name rules
    /// (before any path join) and the multi-volume precondition. `Ok`
    /// means the caller may build the volume file / home paths.
    fn destroy_precheck(&self, name: &str) -> Option<String> {
        if !Self::name_is_path_safe(name) {
            return Some(format!(
                "ERR: `{name}` is not a volume name — DESTROY takes the file stem of a \
                 volumes_dir entry (names match ^[a-z][a-z0-9_-]{{0,31}}$)\n"
            ));
        }
        if self.process_cfg.volumes_dir.is_none() {
            return Some(
                "ERR: this instance runs single-volume mode (no volumes_dir) — DESTROY \
                 serves multi-volume instances\n"
                    .to_string(),
            );
        }
        None
    }

    /// The `(volume file, home directory)` pair both DESTROY legs
    /// speak of — the home resolved through the SAME K21 anchor the
    /// assembly uses ([`volume_home_under`]), derived from the name so
    /// even a schema-broken file previews and purges the directory a
    /// healthy spec would resolve to.
    fn destroy_paths(&self, name: &str) -> Result<(PathBuf, PathBuf)> {
        let dir = self
            .process_cfg
            .volumes_dir
            .as_deref()
            .expect("the precheck ruled out single-volume mode");
        let path = Path::new(dir).join(format!("{name}.toml"));
        let home = volume_home_under(Path::new(dir), name)?;
        Ok((path, home))
    }

    /// `DESTROY <name> confirm [/ purge_local]` (P5, plan §1.2/§2.2):
    /// the two-leg protocol's EXECUTION. The order is unmount-first,
    /// file-second — a drain or unmount refusal aborts the WHOLE
    /// destroy (the volume stays registered, the file survives — never
    /// a half-destroy, K50's semantics carried into the file
    /// deletion). The file deletion is idempotent (a hand-deleted file
    /// continues). The local data directory stays by default;
    /// `purge_local` deletes the whole home (db, cache, local root) —
    /// a purge failure does NOT roll back (the file is already gone,
    /// there is nothing to restore) but the reply says so and names
    /// the leftover path. Remote data is NEVER touched (K49 分档) —
    /// the OK says so explicitly.
    async fn destroy_volume(&self, name: &str, purge_local: bool) -> String {
        if let Some(refusal) = self.destroy_precheck(name) {
            return refusal;
        }
        let (path, home) = match self.destroy_paths(name) {
            Ok(pair) => pair,
            Err(error) => {
                return format!(
                    "ERR: resolving the paths of `{name}` failed: {error:#} — nothing was \
                     changed\n"
                );
            }
        };
        // Leg ①: a registered volume comes down through the SAME K50
        // sequence REMOVE runs (drain → release → unregister — 直接调
        // 用 [`Self::remove_volume`], every H1 observation point
        // included). Its refusal aborts the whole destroy: the file
        // must not survive an unmounted volume behind the operator's
        // back.
        let mut was_running = false;
        if self.registry.volume(name).is_some() {
            was_running = true;
            let removal = self.remove_volume(name).await;
            if !removal.starts_with("OK:") {
                let reason = removal.trim().trim_start_matches("ERR: ");
                return format!(
                    "ERR: destroying `{name}` aborted during the unmount: {reason} — the \
                     volume file {} was NOT deleted and the volume stays; resolve the cause \
                     and retry `DESTROY {name} confirm`\n",
                    path.display()
                );
            }
        }
        // Leg ②: delete the volume file. Idempotent on a missing file
        // (a hand deletion): the destroy still acknowledges, saying
        // the file was already gone.
        let mut file_state = if was_running {
            "unmounted and unregistered; volume file deleted"
        } else {
            "the volume was not running; volume file deleted"
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                println!(
                    "Volume {name} destroyed at {} (control DESTROY).",
                    path.display()
                );
                tracing::info!(
                    volume = name,
                    file = %path.display(),
                    "volume file deleted (control DESTROY)"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                file_state = if was_running {
                    "unmounted and unregistered; the volume file was already gone"
                } else {
                    "the volume was not running; the volume file was already gone"
                };
            }
            Err(error) => {
                // The unmount (if any) already happened — a mixed
                // state the reply must spell out with its idempotent
                // retry.
                return format!(
                    "ERR: deleting the volume file {} failed: {error} — {} — retry \
                     `DESTROY {name} confirm` (idempotent: the unmount, if any, already \
                     happened)\n",
                    path.display(),
                    if was_running {
                        "the volume IS unmounted but its file survived"
                    } else {
                        "the volume was never running and its file survived"
                    },
                );
            }
        }
        // The optional purge leg (裁决②): the whole home directory —
        // db, cache, local root. No rollback on failure (the file is
        // already deleted); the reply names the leftover.
        if purge_local {
            match std::fs::remove_dir_all(&home) {
                Ok(()) => {
                    println!(
                        "Volume {name} local data directory {} purged (control DESTROY).",
                        home.display()
                    );
                    tracing::info!(
                        volume = name,
                        home = %home.display(),
                        "volume local data directory purged (control DESTROY)"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return format!(
                        "ERR: volume `{name}` destroyed ({file_state}), but deleting its \
                         local data directory {} failed: {error} — the data remains at that \
                         path; close whatever holds it (Explorer windows, editors, a running \
                         db) and delete it by hand. Remote data was never touched.\n",
                        home.display()
                    );
                }
            }
            return format!(
                "OK: destroyed volume `{name}` ({file_state}; local data directory {} \
                 deleted; remote data was never touched)\n",
                home.display()
            );
        }
        format!(
            "OK: destroyed volume `{name}` ({file_state}; the local data directory is KEPT \
             at {} — delete it by hand or rerun with purge_local if you want it gone; remote \
             data was never touched)\n",
            home.display()
        )
    }

    /// `ADD <name>` (K48's runtime assembly): read `volumes/<name>.toml`
    /// (K49: `enabled = false` refuses — the file is the persistent
    /// source of truth), connect the transport through the injected
    /// dispatch, assemble through the SAME single-volume assembly the
    /// boot loop runs, mount the volume's explicit drive-letter claim
    /// through the same K27/K40 pass boot uses, and only THEN register
    /// into all three faces at once (review H3: a volume publishes only
    /// after its mount settles, so `/vol/<name>` never routes to a
    /// volume whose drive is not up — and a failed mount tears down a
    /// volume no client ever saw). Every failure answers with its
    /// reason and leaves the running set untouched (K22's runtime twin);
    /// a claim that cannot be honored fails the ADD with nothing
    /// registered (the assembled volume is quietly torn down — its
    /// workers never served a request).
    ///
    /// K58-M7: the outcome is STRUCTURED — the reply is the operator
    /// text, the bool is the assembly+mount verdict (true only when the
    /// volume ended up registered and serving). CREATE/UPDATE classify
    /// on the bool, never on re-parsing the reply's `OK:` prefix.
    async fn add_volume(&self, name: &str) -> (String, bool) {
        // Path-safety precheck (shared with SHOW): the full name rules
        // live in `load_volume_config`.
        if !Self::name_is_path_safe(name) {
            return (
                format!(
                    "ERR: `{name}` is not a volume name — ADD takes the file stem of a \
                     volumes_dir entry (names match ^[a-z][a-z0-9_-]{{0,31}}$)\n"
                ),
                false,
            );
        }
        if let Some(registered) = self.registry.volume(name) {
            return (
                format!(
                    "ERR: volume `{name}` is already registered (status: {}) — remove it \
                     first (`REMOVE {name}`) if you want to re-assemble it\n",
                    registered.status().as_str()
                ),
                false,
            );
        }
        let Some(dir) = self.process_cfg.volumes_dir.as_deref() else {
            return (
                "ERR: this instance runs single-volume mode (no volumes_dir) — \
                    runtime ADD serves multi-volume instances\n"
                    .to_string(),
                false,
            );
        };
        let path = Path::new(dir).join(format!("{name}.toml"));
        let spec = match cloudkit_core::config::load_volume_config(&path) {
            Ok(spec) => spec,
            Err(error) => {
                return (
                    format!(
                        "ERR: reading the volume file {} failed: {error} — fix the file \
                         and retry\n",
                        path.display()
                    ),
                    false,
                );
            }
        };
        if !spec.settings.enabled {
            return (
                format!(
                    "ERR: volume `{name}` is disabled (enabled = false in {}) — the volume \
                     file is the persistent source of truth (K49); flip the key to true \
                     and retry\n",
                    spec.file_path.display()
                ),
                false,
            );
        }
        let Some(dispatch) = &self.dispatch else {
            return (
                "ERR: this instance booted without a runtime transport dispatch — \
                    ADD cannot assemble new volumes here\n"
                    .to_string(),
                false,
            );
        };
        let (options, transport) = match dispatch(&spec).await {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                return (
                    format!("ERR: connecting volume `{name}` was interrupted; nothing changed\n"),
                    false,
                );
            }
            Err(error) => {
                return (
                    format!(
                        "ERR: connecting volume `{name}` failed: {error:#} — nothing was \
                         changed; fix the volume file and retry\n"
                    ),
                    false,
                );
            }
        };
        let assembled =
            match assemble_volume(&self.process_cfg, &spec, &options, transport, &self.watch).await
            {
                Ok(assembled) => assembled,
                Err((_, error)) => {
                    return (
                        format!(
                            "ERR: assembling volume `{name}` failed: {error:#} — nothing was \
                             changed; fix the volume file and retry\n"
                        ),
                        false,
                    );
                }
            };

        // The gate's mid-ADD observation point (review H1): the
        // dispatch/assembly awaits above can straddle a gate fire. The
        // check sits BEFORE the first mutation of the shared state (the
        // three faces and the live table — every registration below)
        // and before the mount pass, whose own awaits get a second
        // observation point after they settle. The teardown releases
        // the never-served volume's idle workers, so it is bounded and
        // cannot wedge the stop task's idle barrier.
        if self.watch.fired() {
            tear_down_unpublished(assembled).await;
            return (
                format!(
                    "ERR: volume `{name}` not added — the instance is shutting down; the \
                     assembled volume was torn down and nothing was registered\n"
                ),
                false,
            );
        }

        // Volumes that mount nothing (no explicit drive_letter claim, or
        // the auto-mount switch off) keep the assemble-and-publish
        // semantics — there is no mount window to hide behind (H3).
        if !spec.explicit_drive_letter {
            self.publish_added_volume(name, assembled, None, None);
            return (
                format!("OK: added volume `{name}` (running; no drive letter claimed)\n"),
                true,
            );
        }
        if !self.process_cfg.auto_mount_drive {
            tracing::info!(
                volume = name,
                "drive_letter claimed but auto_mount_drive is off; the volume runs \
                 without its drive (the same gate the boot mount pass applies)"
            );
            self.publish_added_volume(name, assembled, None, None);
            return (
                format!("OK: added volume `{name}` (running; no drive letter claimed)\n"),
                true,
            );
        }

        // The mount-first window (review H3): the ADD's drive mount runs
        // BEFORE any face publishes the volume. The old order inserted
        // into the three faces first, so `/vol/<name>` routed and the
        // dashboard tab appeared while the mount was still seconds away
        // — and a mount failure rolled the faces back under in-flight
        // requests (axum request tasks run concurrently with the
        // serialized command execution — even with every command behind
        // the gate, the DATA-plane requests the faces serve are not).
        // Publishing after the mount settles makes the window
        // unobservable: a volume is either not registered at all or
        // registered AND mounted. Accepted edge: the LIST/banner face
        // briefly lacks a volume whose mount is in flight (one ADD's
        // worth of seconds — and no client can observe the half-state
        // either way).
        let mut mounts = match &self.mount {
            Some(mount_step) => mount_step(name, &spec.settings.drive_letter).await,
            None => {
                // The pass's winfsp arm resolves each claim's VFS through
                // the registry it is handed, and the volume is
                // deliberately NOT in the shared one yet — so this
                // single-entry registry is the whole pass's view (the
                // claims list carries exactly this volume).
                let shadow_registry = RegistryHandle::new(vec![assembled.runtime.clone()]);
                mount_volumes_if_configured(&MountPlan {
                    process_cfg: &self.process_cfg,
                    claims: std::slice::from_ref(&(
                        name.to_string(),
                        spec.settings.drive_letter.clone(),
                    )),
                    registry: &shadow_registry,
                    rt: &tokio::runtime::Handle::current(),
                    webdav_available: self.webdav_available,
                    winfsp: winfsp_capability(),
                })
                .await
            }
        };
        let mut handles = mounts.winfsp.drain();
        let Some(mount) = mounts.mounted.into_iter().next() else {
            // A claimed letter that cannot be honored fails the ADD (the
            // ruling lists 盘符冲突 as an ADD failure): tear the
            // assembled volume down — nothing was ever registered, so
            // there are no faces to roll back and no in-flight client
            // request ever saw the volume (H3: the old rollback ran with
            // the faces already published).
            tear_down_unpublished(assembled).await;
            println!("Volume {name} rolled back: the drive mount failed (see the hints above).");
            tracing::warn!(
                volume = name,
                "runtime ADD rolled back: the drive mount failed"
            );
            return (
                format!(
                    "ERR: volume `{name}` assembled but its drive mount failed — the \
                     addition was rolled back; free the drive letter (see the hints \
                     above) and retry\n"
                ),
                false,
            );
        };
        let release = mount_release(&mount, &mut handles);
        let backend = mount.backend.label();
        let short = mount.backend.as_str();
        let letter = mount.letter.clone();

        // The gate's post-mount observation point (the H1 placement
        // semantic at the new registration point): the mount pass's
        // awaits can straddle a gate fire too. A shutdown landing here
        // takes over the cleanup — release the just-mounted drive FIRST
        // (its callbacks call into the Vfs, so the host must be gone
        // before the workers; a release failure only warns, the
        // stop-sequence policy), then tear the never-published volume
        // down. Nothing is registered on this path.
        if self.watch.fired() {
            if let Some(mut release) = release {
                if let Err(error) = release.release().await {
                    tracing::warn!(
                        volume = name,
                        %error,
                        "releasing the freshly mounted drive failed while aborting the \
                         ADD for shutdown; continuing the teardown"
                    );
                }
            }
            tear_down_unpublished(assembled).await;
            return (
                format!(
                    "ERR: volume `{name}` not added — the instance is shutting down; the \
                     freshly mounted drive was released, the assembled volume torn down, \
                     and nothing was registered\n"
                ),
                false,
            );
        }

        // The registration point: all three faces move together (the
        // master table is the truth, these are its projections), with
        // the settled mount and its release step attached — publication
        // IS the "registered AND mounted" state (H3).
        self.publish_added_volume(name, assembled, Some(mount), release);
        println!("Volume {name} mounted at {letter} ({backend}).");
        tracing::info!(volume = name, letter = %letter, backend = %backend, "runtime ADD mounted the volume's drive");
        (
            format!("OK: added volume `{name}` (running; mounted {letter} via {short})\n"),
            true,
        )
    }

    /// The ADD publication point (review H3): all three faces and the
    /// live table move together — the master registry is the truth, the
    /// WebDAV dispatch and the dashboard are its projections. Callers
    /// hold the publish until the volume's state is final (its mount
    /// settled, or it mounts nothing at all), so a published volume is
    /// never observable half-assembled.
    fn publish_added_volume(
        &self,
        name: &str,
        assembled: AssembledVolume,
        mount: Option<MountedVolume>,
        release: Option<Box<dyn DriveRelease>>,
    ) {
        self.registry.insert(assembled.runtime);
        self.webdav.insert(name, assembled.fs);
        self.web.insert(cloudkit_web::VolumeUiEntry {
            name: name.to_string(),
            status: cloudkit_web::VolumeUiStatus::Running,
            config: assembled.ui_config,
            vfs: Some(Arc::clone(&assembled.vfs)),
        });
        self.live.insert(
            name,
            LiveVolume {
                stop_unit: assembled.stop_unit,
                sync_task: assembled.sync_task,
                mount,
                release,
            },
        );
        println!("Volume {name} added at runtime (control ADD).");
        tracing::info!(volume = name, "volume added at runtime (control ADD)");
    }

    /// `REMOVE <name>` — the K50 safe sequence: drain the upload queue
    /// (bounded), release the drive mount (bounded by the backend's own
    /// disappearance poll), then commit (faces first, then the volume's
    /// workers). Any failed step aborts the removal with the volume
    /// fully registered — never a half-removal (K50). Runtime-only
    /// (K49): the volume file stays; a later boot brings the volume
    /// back.
    async fn remove_volume(&self, name: &str) -> String {
        let Some(runtime) = self.registry.volume(name) else {
            return format!(
                "ERR: no volume registered under `{name}` — `LIST` shows the current set\n"
            );
        };
        // A failed boot entry is runtime garbage: no queue, no mount —
        // the faces just drop it.
        let Some(vfs) = runtime.vfs().cloned() else {
            self.web.remove(name);
            self.registry.remove(name);
            println!("Volume {name} removed at runtime (failed entry).");
            tracing::info!(
                volume = name,
                "failed volume entry removed (control REMOVE)"
            );
            return format!("OK: removed volume `{name}`\n");
        };
        let Some(mut entry) = self.live.take(name) else {
            // Structural impossibility (Running entries always carry
            // live plumbing — both are written together); refuse rather
            // than half-remove.
            return format!("ERR: volume `{name}` has no live runtime state — nothing to remove\n");
        };

        // K50 step 1: drain — wait for the queue's in-flight jobs to
        // reach a terminal state (cumulative counters: enqueued minus
        // its terminal states), bounded by the drain budget.
        let deadline = Instant::now() + self.tuning.drain_timeout;
        // Review M2: the enqueued baseline for the still-arriving
        // check. The volume keeps serving during the drain (K50's
        // deliberate order), so an active writer keeps re-feeding the
        // queue — a drain budget can never cover that. The growth of
        // `enqueued` between polls is the observable.
        let mut last_enqueued: Option<u64> = None;
        loop {
            // The gate's drain observation point (review H1): a
            // shutdown that fires mid-drain takes over the cleanup —
            // hand the entry back (the stop task's idle barrier is
            // waiting on this command) and answer within one poll tick
            // instead of parking the whole drain budget. The gate
            // outranks the growth check below (a shutting-down
            // instance must not diagnose its writers).
            if self.watch.fired() {
                self.live.insert(name, entry);
                return removal_aborted_by_shutdown(name);
            }
            let stats = vfs.queue_stats();
            let outstanding = stats.outstanding();
            if outstanding == 0 {
                break;
            }
            if let Some(previous) = last_enqueued {
                if stats.enqueued > previous {
                    // New uploads arrived DURING the drain — a client
                    // is still writing the volume. Abort now (the
                    // budget would just run out and mis-advise) and
                    // put the entry back: the volume keeps running
                    // exactly as it was (K50's abort semantics).
                    self.live.insert(name, entry);
                    return format!(
                        "ERR: uploads are still arriving on volume `{name}` \
                         (pending={outstanding}, more enqueued during the removal) — a \
                         client is still writing the volume; the removal was aborted \
                         and the volume stays registered; close the programs using the \
                         volume (Explorer windows, copy tasks) and retry\n"
                    );
                }
            }
            last_enqueued = Some(stats.enqueued);
            if Instant::now() >= deadline {
                // Abort: put the entry back — the volume keeps running
                // exactly as it was (the queue workers were never
                // touched).
                self.live.insert(name, entry);
                return format!(
                    "ERR: volume `{name}` still has {outstanding} upload(s) in flight \
                     (pending={outstanding}) — the removal was aborted and the volume \
                     stays registered; if a program is still writing the volume, close \
                     it first, then retry once the queue drains (`LIST` shows the \
                     count)\n"
                );
            }
            tokio::time::sleep(self.tuning.poll_interval).await;
        }

        // K50 step 2: release the drive mount (the winfsp unmount's own
        // disappearance poll / the net-use delete). A failure aborts;
        // the release object goes back with the entry — a spent winfsp
        // handle is a no-op on retry, a failed net-use can be retried.
        if let Some(step) = entry.release.as_mut() {
            if let Err(error) = step.release().await {
                self.live.insert(name, entry);
                return format!(
                    "ERR: unmounting volume `{name}` failed: {error} — the removal was \
                     aborted and the volume stays registered and mounted; close whatever \
                     holds the drive and retry\n"
                );
            }
        }

        // The gate's commit observation point (review H1): a shutdown
        // that fired during the (bounded) release step also takes over
        // the cleanup — hand the entry back instead of committing the
        // face removals under the stop task's feet (a spent release is
        // a no-op when the shutdown's teardown pass runs it again).
        if self.watch.fired() {
            self.live.insert(name, entry);
            return removal_aborted_by_shutdown(name);
        }

        // K50 step 3: commit — the faces first (no new request routes to
        // the volume; an in-flight request drains on its cloned
        // handler), then the volume's own workers (the same per-volume
        // stop segment the shutdown runs).
        self.webdav.remove(name);
        self.web.remove(name);
        self.registry.remove(name);
        if let Some(task) = entry.sync_task {
            task.abort();
        }
        entry.stop_unit.release().await;
        println!("Volume {name} removed at runtime (control REMOVE).");
        tracing::info!(volume = name, "volume removed at runtime (control REMOVE)");
        format!("OK: removed volume `{name}`\n")
    }

    /// `REBUILD <name>` (web volume management P2, plan §1.3 R1–R6):
    /// the gates run SYNCHRONOUSLY — a volume must be registered and
    /// running (its live VFS answers the queue read), no rebuild may
    /// already be in flight (R1), a telegram volume is refused with
    /// the shadow-index refusal (R6 — actionable, never background
    /// work; encrypted volumes are accepted since Phase 8-B B5 — the
    /// walk carries the production cipher context), and the upload
    /// queue must be drained (R2 — the same
    /// `outstanding()` the REMOVE drain judges by). Everything past
    /// the gates is ACCEPTED, not executed: the marker goes up, the
    /// background task spawns (R3), and the reply is immediate — the
    /// command queue is never parked behind a walk. The background
    /// task owns the R4 budget and the R5 checkpoints; its result
    /// arrives as a log line, never on this reply. K58-FD: the task
    /// carries the ACCEPTED instance's identity (M4a — a re-assembled
    /// same-name volume is a different generation and aborts the walk)
    /// and re-audits the drained queue every checkpoint (M4b — uploads
    /// resuming mid-walk interrupt the pass recoverably).
    async fn rebuild_volume(&self, name: &str) -> String {
        if !Self::name_is_path_safe(name) {
            return format!(
                "ERR: `{name}` is not a volume name — REBUILD takes the file stem of a \
                 volumes_dir entry (names match ^[a-z][a-z0-9_-]{{0,31}}$)\n"
            );
        }
        let Some(runtime) = self.registry.volume(name) else {
            return format!(
                "ERR: no volume registered under `{name}` — `LIST` shows the current set\n"
            );
        };
        let Some(vfs) = runtime.vfs().cloned() else {
            return format!(
                "ERR: volume `{name}` is not running (its assembly failed) — fix the \
                 volume file, re-add it, and retry\n"
            );
        };
        // R1: the single-flight marker (the reply renders the start).
        if let Some(started) = self
            .rebuilds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .copied()
        {
            return format!(
                "ERR: rebuild already running on `{name}` (started {}) — `LIST` shows its \
                 progress\n",
                format_start_clock(started)
            );
        }
        // R6's synchronous gate: the telegram shadow-index refusal —
        // the same text the offline path prints, so the two surfaces
        // cannot drift. (The K11 plaintext gate is gone: Phase 8-B B5
        // accepts encrypted volumes — the walk behind the acceptance
        // seam carries the production cipher context.)
        let settings = &runtime.spec().settings;
        if settings.backend == Backend::Telegram {
            return format!("ERR: {TELEGRAM_REBUILD_REFUSAL}\n");
        }
        // R2: the drained-queue threshold (H2's `outstanding()`, the
        // same observable the REMOVE drain waits on).
        let outstanding = vfs.queue_stats().outstanding();
        if outstanding > 0 {
            return format!(
                "ERR: volume `{name}` has {outstanding} upload(s) in flight — rebuild \
                 needs a drained queue; wait or see `LIST` pending\n"
            );
        }
        // The volume's RESOLVED settings (K21 paths; the gates above
        // read the raw spec because they never touch a path).
        let resolved = match resolve_volume_settings(runtime.spec()) {
            Ok(resolved) => resolved,
            Err(error) => {
                return format!(
                    "ERR: resolving volume `{name}`'s paths failed: {error:#} — nothing was \
                     changed; fix the volume file and retry\n"
                );
            }
        };
        // R1 + R3: the marker goes up and the task detaches. Commands
        // serialize behind the command gate (K58-FB), so a second
        // REBUILD cannot race this insert; the background task's exit
        // path is the only other writer, and it removes only its own
        // name.
        //
        // K58-FD/M4a: the instance identity, captured at acceptance —
        // the accepted Vfs's heap allocation address (see
        // [`volume_identity`]). The task also keeps the Vfs clone
        // itself: the M4b checkpoint re-reads its queue, and the clone
        // PINNED here makes the identity exact for the task's whole
        // run (the accepted allocation can never be freed and its
        // address reused while the task holds it, so a fresh assembly
        // can never alias the identity).
        let identity = volume_identity(&runtime)
            .expect("the running gate above guaranteed a Vfs-backed entry");
        self.rebuilds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_string(), std::time::SystemTime::now());
        tokio::spawn(
            RebuildTask {
                name: name.to_string(),
                settings: resolved,
                registry: self.registry.clone(),
                watch: Arc::clone(&self.watch),
                rebuilds: Arc::clone(&self.rebuilds),
                tuning: self.rebuild_tuning,
                run: Arc::clone(&self.rebuild),
                vfs,
                identity,
            }
            .run(),
        );
        format!(
            "OK: rebuild of `{name}` started in background — `LIST` shows progress; the \
             result logs when done\n"
        )
    }
}

/// The wall-clock start of an in-flight rebuild as `HH:MM:SS` (local
/// time — the operator-facing reading of R1's already-running reply).
fn format_start_clock(started: std::time::SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = started.into();
    local.format("%H:%M:%S").to_string()
}

/// The instance identity behind a registry entry (K58-FD/M4a): the
/// entry's Vfs allocation address, read through the RUNNING variant's
/// `Arc<Vfs>` (`None` for a Failed entry — a broken assembly has no
/// identity, and a walk accepted on a running instance must not treat
/// it as its own).
///
/// Why the address is an EXACT identity for the rebuild task's run:
/// every registration path (boot discovery, runtime ADD, ENABLE —
/// which routes through `add_volume` verbatim — and UPDATE's internal
/// REMOVE+ADD) assembles a FRESH `Arc<Vfs>` through `assemble_volume`
/// (there is no same-Vfs reuse path anywhere: even DISABLE→ENABLE
/// re-assembles from scratch, so an in-flight walk from the
/// pre-DISABLE instance aborts — correct, its settings snapshot is the
/// old assembly's); and the RebuildTask holds its own clone of the
/// accepted Vfs for the whole run, so the accepted allocation stays
/// live (pinned) — a freed-and-reused address cannot alias the
/// identity. Same address therefore MEANS same instance.
fn volume_identity(runtime: &VolumeRuntime) -> Option<usize> {
    runtime.vfs().map(|vfs| Arc::as_ptr(vfs) as usize)
}

/// How one background rebuild ended (P2/R3–R5) — each exit has its own
/// one-line report; every one of them resets the R1 marker.
enum RebuildExit {
    /// The executor answered (either arm of its Result).
    Done(Result<rebuild::RebuildOutcome>),
    /// The R4 budget ran out (the idempotent-merge world: rows stay).
    TimedOut(Duration),
    /// The R5 checkpoint found the accepted instance gone from the
    /// registry — removed, or (K58-FD/M4a) replaced by a DIFFERENT
    /// generation under the same name (a re-assembly's fresh Vfs never
    /// matches the accepted identity).
    VolumeGone,
    /// The R5 checkpoint found the shutdown gate fired.
    ShuttingDown,
    /// The FD checkpoint (M4b) found uploads back in flight mid-walk:
    /// a recoverable interrupt — the rows stay, a rerun continues the
    /// merge once the queue drains.
    UploadsResumed,
    /// The executor's task panicked (contained, like a command panic).
    Panicked(String),
}

/// One detached background rebuild (P2/R3): owns the volume's resolved
/// settings and the executor, supervises it with the R4 budget and the
/// R5 checkpoints, and reports the outcome as one println + tracing
/// line (the acceptance reply already went out; this task's logs ARE
/// the result surface). On EVERY exit the R1 marker comes off, making
/// the volume re-acceptable — including the abort exits, because the
/// rebuild's upserts are idempotent (the rel_path conflict key: a
/// rerun re-merges exactly what the interrupted pass had written).
///
/// K58-FD: the checkpoints bind to the ACCEPTED instance, not the
/// name — `identity` is the accepted Vfs's address ([`volume_identity`])
/// and a same-name re-assembly (a new generation) aborts the walk
/// (M4a); `vfs` (the accepted instance's own queue) is re-audited
/// every checkpoint so uploads resuming mid-walk interrupt the pass
/// recoverably instead of letting stale list pages overwrite fresh
/// fs_ids (M4b).
struct RebuildTask {
    name: String,
    settings: CyDriveConfig,
    registry: RegistryHandle,
    watch: Arc<ShutdownWatch>,
    rebuilds: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::SystemTime>>>,
    tuning: RebuildTuning,
    run: RuntimeRebuild,
    /// The accepted instance's Vfs (M4b's queue read; also the pin
    /// that keeps the identity exact — see [`volume_identity`]).
    vfs: Arc<Vfs>,
    /// The accepted instance's identity (M4a's checkpoint verdict).
    identity: usize,
}

impl RebuildTask {
    /// The supervision loop: the executor runs on its own spawned task
    /// (so an abort can DROP it mid-await — the R5 semantics), with
    /// the checkpoint ticking between the shutdown gate, the accepted
    /// instance's identity and the drained queue, and the R4 deadline
    /// over the whole pass. The checkpoint cadence is the plan's
    /// "periodic checkpoint" stand-in
    /// for the per-page hook: `rebuild_from_backend` has no
    /// page-granularity seam, and a sub-second cadence bounds the
    /// abort latency as tightly without touching core's walk (the
    /// trade-off: a page walking slower than the cadence aborts on the
    /// NEXT tick, not mid-list — accepted, one tick's worth).
    async fn run(self) {
        let deadline = tokio::time::Instant::now() + self.tuning.timeout;
        let mut work = tokio::spawn((self.run)(&self.name, &self.settings));
        let mut checkpoints = tokio::time::interval(self.tuning.checkpoint_interval);
        checkpoints.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let exit = loop {
            tokio::select! {
                joined = &mut work => {
                    break match joined {
                        Ok(outcome) => RebuildExit::Done(outcome),
                        Err(panic) => RebuildExit::Panicked(
                            panic
                                .try_into_panic()
                                .map(|payload| {
                                    payload
                                        .downcast_ref::<&str>()
                                        .map(|str| (*str).to_string())
                                        .or_else(|| {
                                            payload.downcast_ref::<String>().cloned()
                                        })
                                        .unwrap_or_else(|| {
                                            "<non-string panic payload>".to_string()
                                        })
                                })
                                .unwrap_or_else(|cancelled| format!("aborted: {cancelled}")),
                        ),
                    };
                }
                _ = checkpoints.tick() => {
                    // R5's two observables, the H1 gate's own reading
                    // points: the shutdown gate and the registry entry
                    // — the latter as the ACCEPTED INSTANCE's identity
                    // (K58-FD/M4a): a same-name re-assembly (UPDATE's
                    // internal REMOVE+ADD, DISABLE→ENABLE) is a new
                    // generation and must not inherit the walk.
                    if self.watch.fired() {
                        work.abort();
                        let _ = work.await;
                        break RebuildExit::ShuttingDown;
                    }
                    let same_generation = self
                        .registry
                        .volume(&self.name)
                        .as_ref()
                        .and_then(volume_identity)
                        .is_some_and(|identity| identity == self.identity);
                    if !same_generation {
                        work.abort();
                        let _ = work.await;
                        break RebuildExit::VolumeGone;
                    }
                    // K58-FD/M4b: R2 re-audited mid-walk — uploads that
                    // resumed after acceptance would let an earlier
                    // list page overwrite a re-uploaded row's fs_id
                    // with the stale one (old bytes served until the
                    // next pass), so a non-empty queue interrupts the
                    // pass recoverably instead.
                    if self.vfs.queue_stats().outstanding() > 0 {
                        work.abort();
                        let _ = work.await;
                        break RebuildExit::UploadsResumed;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    work.abort();
                    let _ = work.await;
                    break RebuildExit::TimedOut(self.tuning.timeout);
                }
            }
        };
        // R1's reset on every exit path — the marker's whole lifetime
        // is this task's run (the handler only inserts what this
        // removes; the gate-serialized commands cannot interleave).
        self.rebuilds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.name);
        let name = &self.name;
        match exit {
            RebuildExit::Done(Ok(outcome)) if outcome.interrupted.is_some() => {
                // D8②: a gracefully capped pass — distinct from every
                // supervision abort (nothing was killed; the walk
                // stopped itself and left its cursor on disk).
                let reason = outcome.interrupted.unwrap();
                println!(
                    "Volume {name} rebuild interrupted: {} file row(s), {} directory row(s) \
                     this pass; {reason} — rerun REBUILD to continue (the rows already \
                     rebuilt are kept, the cursor is persisted).",
                    outcome.files, outcome.dirs
                );
                tracing::warn!(
                    volume = name,
                    files = outcome.files,
                    dirs = outcome.dirs,
                    %reason,
                    "background rebuild interrupted by its entry cap; the pass is resumable"
                );
            }
            RebuildExit::Done(Ok(outcome)) => {
                println!(
                    "Volume {name} rebuild finished: {} file row(s), {} directory row(s); \
                     {} stale row(s) pruned.",
                    outcome.files, outcome.dirs, outcome.pruned
                );
                tracing::info!(
                    volume = name,
                    files = outcome.files,
                    dirs = outcome.dirs,
                    pruned = outcome.pruned,
                    "background rebuild finished"
                );
            }
            RebuildExit::Done(Err(error)) => {
                println!("Volume {name} rebuild failed: {error:#}.");
                tracing::warn!(volume = name, %error, "background rebuild failed");
            }
            RebuildExit::TimedOut(budget) => {
                println!(
                    "Volume {name} rebuild interrupted; rerun REBUILD to continue (the \
                     {budget:?} budget ran out — the rows already rebuilt are kept, the \
                     merge is idempotent)."
                );
                tracing::warn!(
                    volume = name,
                    ?budget,
                    "background rebuild interrupted by its budget; the pass is resumable"
                );
            }
            RebuildExit::VolumeGone => {
                println!(
                    "Volume {name} rebuild aborted — the volume this rebuild started on is no \
                     longer registered under that name: it was removed, or re-assembled into a \
                     different generation, at runtime (nothing to resume; re-REBUILD the \
                     volume now registered if it should be rebuilt)."
                );
                tracing::info!(
                    volume = name,
                    "background rebuild aborted: the volume left the registry or its \
                     registered instance is a different generation (identity mismatch)"
                );
            }
            RebuildExit::UploadsResumed => {
                println!(
                    "Volume {name} rebuild interrupted — uploads resumed mid-walk; rerun \
                     REBUILD when the queue drains (the rows already rebuilt are kept, the \
                     merge is idempotent)."
                );
                tracing::warn!(
                    volume = name,
                    "background rebuild interrupted by resumed uploads; the pass is resumable"
                );
            }
            RebuildExit::ShuttingDown => {
                println!(
                    "Volume {name} rebuild interrupted; rerun REBUILD to continue (the \
                     instance is shutting down — the rows already rebuilt are kept)."
                );
                tracing::info!(
                    volume = name,
                    "background rebuild interrupted by the shutdown gate; the pass is \
                     resumable"
                );
            }
            RebuildExit::Panicked(reason) => {
                println!("Volume {name} rebuild task panicked: {reason}.");
                tracing::error!(
                    volume = name,
                    %reason,
                    "background rebuild task panicked; the marker was reset — a rerun is \
                     safe"
                );
            }
        }
    }
}

/// REMOVE's shutdown-gate reply — the drain-loop and commit-point
/// observation points (review H1) share it. The entry is back in the
/// live table by the time this is returned, so the stop task's idle
/// barrier plus `take_all` sees it and the volume's drain/release runs
/// in the shutdown sequence: this REMOVE neither half-removes the
/// volume nor orphans its entry.
fn removal_aborted_by_shutdown(name: &str) -> String {
    format!(
        "ERR: volume `{name}` removal aborted — the instance is shutting down; the \
         volume stays registered and its drain/release runs in the shutdown sequence\n"
    )
}

/// The teardown of an assembled-but-never-published volume (review H3):
/// abort its periodic sync task and release its stop segment (the VFS
/// drain + inbound join). Bounded by construction — the volume never
/// served a client request through the faces, so its workers are idle —
/// and nothing else needs undoing: no face was ever told about it.
async fn tear_down_unpublished(assembled: AssembledVolume) {
    if let Some(task) = assembled.sync_task {
        task.abort();
    }
    assembled.stop_unit.release().await;
}

/// Pairs a completed mount with its backend release step (shared by the
/// boot pass and runtime ADD): a winfsp mount carries its handle, every
/// net-use-carried letter (webdav or a winfsp fallback) carries the
/// mapping delete. The handle type exists only with the feature; the
/// twin build's pairs are empty placeholders.
#[cfg(all(windows, feature = "winfsp"))]
fn mount_release(
    mount: &MountedVolume,
    handles: &mut Vec<(String, cloudkit_winfsp::mount::MountHandle)>,
) -> Option<Box<dyn DriveRelease>> {
    if let Some(position) = handles.iter().position(|(name, _)| name == &mount.volume) {
        let (_, handle) = handles.remove(position);
        return Some(Box::new(WinFspRelease {
            handle: Some(handle),
        }));
    }
    match mount.backend {
        MountedBackend::WinFsp => {
            // Structural: a winfsp-carried mount without a parked handle
            // cannot occur (both are produced by the same pass). Refuse
            // loudly rather than leaving it unreleasable.
            tracing::error!(
                volume = mount.volume,
                "a winfsp mount has no parked handle; it cannot be released"
            );
            None
        }
        MountedBackend::WebDav | MountedBackend::WebDavFallback { .. } => {
            Some(Box::new(WebDavRelease {
                letter: mount.letter.clone(),
            }))
        }
    }
}

/// [`mount_release`] for every build that cannot mount through WinFsp:
/// no winfsp handle can exist, so every mount is a mapping delete.
#[cfg(not(all(windows, feature = "winfsp")))]
fn mount_release(
    mount: &MountedVolume,
    handles: &mut Vec<(String, ())>,
) -> Option<Box<dyn DriveRelease>> {
    let _ = handles;
    Some(Box::new(WebDavRelease {
        letter: mount.letter.clone(),
    }))
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
///
/// Exists only with the `telegram` feature: the config maps onto the
/// driver's `TransportConfig`, so a binary without the driver has no
/// use for it (its callers are all gated too).
#[cfg(feature = "telegram")]
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
#[cfg(feature = "telegram")]
pub async fn connect_stack(cfg: &CyDriveConfig) -> Result<Stack> {
    connect_stack_with_deadline(cfg, CONNECT_DEADLINE).await
}

/// The no-driver twin of [`connect_stack`] (K31): the one-shot data
/// channel is telegram-only, so a binary built without the driver
/// refuses at runtime with [`TELEGRAM_DRIVER_REQUIRED`] — the `push` /
/// `pull` entries stay compilable (and their Vfs-level helpers fully
/// tested) while the connect reports the actionable rebuild message.
#[cfg(not(feature = "telegram"))]
pub async fn connect_stack(cfg: &CyDriveConfig) -> Result<Stack> {
    let _ = cfg;
    anyhow::bail!("{TELEGRAM_DRIVER_REQUIRED}")
}

/// [`connect_stack`] with the connect deadline injected — the tests
/// shrink the budget to prove the guard wins; everything else about the
/// assembly is identical.
#[cfg(feature = "telegram")]
pub async fn connect_stack_with_deadline(cfg: &CyDriveConfig, deadline: Duration) -> Result<Stack> {
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    let transport_config = transport_config_from(cfg, &cwd);
    let transport: Arc<dyn CloudTransport> = Arc::new(
        connect_with_deadline(GrammersTransport::connect(transport_config), deadline)
            .await
            .context("connecting the Telegram transport")
            .context(connect_failure_hint())?,
    );
    build_stack(cfg, transport).await
}

/// The second half of the `connect_stack` family, over an
/// already-connected transport (Phase 3 / WF4 extraction): open the
/// metadata db, build the cache and the VFS, and bundle the three. Kept
/// public so the winfsp `mount` path — whose transport comes from the
/// unified backend dispatch rather than the telegram connect — assembles
/// exactly the same stack as `run`.
pub async fn build_stack(cfg: &CyDriveConfig, transport: Arc<dyn CloudTransport>) -> Result<Stack> {
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

/// The no-driver twin of the deadline-injected assembly: same refusal
/// contract as [`connect_stack`] (the deadline never comes into play —
/// there is no connect to bound without the driver).
#[cfg(not(feature = "telegram"))]
pub async fn connect_stack_with_deadline(cfg: &CyDriveConfig, deadline: Duration) -> Result<Stack> {
    let _ = (cfg, deadline);
    anyhow::bail!("{TELEGRAM_DRIVER_REQUIRED}")
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
        // Phase 4 / SF3: the sftp volume is authoritative-index (the
        // remote filesystem IS the truth — SF1 capability ruling); like
        // local, its sync participation is off — the remote side is the
        // source of truth, not a mirror target. `is_sync_supported`
        // reports the same and doctor shares the warning.
        Backend::Sftp => anyhow::bail!("{SFTP_SYNC_UNSUPPORTED}"),
        // Phase 5 / 115-4 ruling: pan115 joins the sync world like
        // baidu — its namespace key is the raw volume identity
        // (`pan115:<uid>`), and the driver self-refreshes tokens (the
        // K13 store rides the dispatch assembly).
        Backend::Pan115 => build_backend_transport(cfg)
            .await
            .context("connecting the pan115 backend to derive the sync namespace")?
            .sync_namespace_key(),
        // Phase 6 / 123-4 ruling: pan123 joins the sync world like
        // baidu/pan115 — the namespace key is the raw volume identity
        // (`pan123:<uid>`); no refresh to ride (K76.4), so a dead token
        // fails the connect with the re-authorize guidance.
        Backend::Pan123 => build_backend_transport(cfg)
            .await
            .context("connecting the pan123 backend to derive the sync namespace")?
            .sync_namespace_key(),
        // Phase 7 / WD1b ruling: webdav joins the sync world like the
        // pan115/pan123 cloud backends — the namespace key is the raw
        // volume identity (`webdav:<user>@<base-url>`, offline-derived —
        // D6: the transport constructs without connecting).
        Backend::Webdav => build_backend_transport(cfg)
            .await
            .context("connecting the webdav backend to derive the sync namespace")?
            .sync_namespace_key(),
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
/// discovery as every other subcommand). Since Phase 8 / D8② the walk
/// is bounded (entry cap + the 15-minute wall clock — the live path's
/// budget extended to the offline path), and an interrupted pass
/// reports `RebuildOutcome::interrupted` instead of failing.
pub async fn run_rebuild_command(cfg: &CyDriveConfig) -> Result<rebuild::RebuildOutcome> {
    run_rebuild_command_with_limits(cfg, RebuildTuning::default().limits(true)).await
}

/// [`run_rebuild_command`] with explicit per-pass bounds — the live
/// background executor's entry (D8②: the entry cap flows in from the
/// tuning; the wall clock stays the R4 supervision's business, so the
/// caller passes `time_budget: None` there).
async fn run_rebuild_command_with_limits(
    cfg: &CyDriveConfig,
    limits: rebuild::RebuildLimits,
) -> Result<rebuild::RebuildOutcome> {
    let driver = build_driver(cfg).await?;
    run_rebuild_with_driver_and_limits(cfg, driver.as_ref(), limits).await
}

/// [`run_rebuild_command`] for the multi-volume mode (Phase 2.5): one
/// bootstrap pass over every volume, each rebuilt from its OWN backend
/// into its OWN volume-home db (K21, via
/// [`resolve_volume_settings`]). Telegram volumes are skipped with the
/// [`TELEGRAM_REBUILD_REFUSAL`] as their error — the shadow index has
/// no backend walk, so "rebuild" is meaningless there; a fresh
/// telegram volume legitimately serves an empty listing until files
/// arrive through it or sync replicates the index.
///
/// Per-volume outcomes are reported, not fail-fast: one broken volume
/// must not stop the others' bootstrap (the K22 spirit, offline
/// edition). The caller prints the per-volume report.
pub async fn run_rebuild_multi(
    volumes: &[VolumeConfig],
) -> Result<Vec<(String, Result<rebuild::RebuildOutcome, String>)>> {
    let mut reports = Vec::new();
    for spec in volumes {
        let name = spec.name.clone();
        let result = async {
            let settings = resolve_volume_settings(spec)?;
            if settings.backend == Backend::Telegram {
                anyhow::bail!("{TELEGRAM_REBUILD_REFUSAL}");
            }
            run_rebuild_command(&settings).await
        }
        .await
        .map_err(|error| format!("{error:#}"));
        reports.push((name, result));
    }
    Ok(reports)
}

/// [`run_rebuild_command`] with the driver injected — the test seam
/// (tests seed a `MockStorageDriver`; production feeds the
/// backend-key assembly). Gates in order, all before any backend
/// traffic: config validity and the telegram shadow-index refusal;
/// then the db open and the bounded walk (D8②: entry cap + the
/// 15-minute budget — the offline path's extension of the live
/// rebuild's budget). The walk carries the production cipher context
/// (Phase 8-B B5): encrypted instances materialize cipher-truth rows
/// through the same `materialize_entry` mapping read-through uses.
pub async fn run_rebuild_with_driver(
    cfg: &CyDriveConfig,
    driver: &dyn StorageDriver,
) -> Result<rebuild::RebuildOutcome> {
    run_rebuild_with_driver_and_limits(cfg, driver, RebuildTuning::default().limits(true)).await
}

/// [`run_rebuild_with_driver`] with explicit per-pass bounds — the D8②
/// seam (core tests drive `rebuild_from_backend_with` directly; the
/// CLI-level tests keep the default-bounded seam).
async fn run_rebuild_with_driver_and_limits(
    cfg: &CyDriveConfig,
    driver: &dyn StorageDriver,
    limits: rebuild::RebuildLimits,
) -> Result<rebuild::RebuildOutcome> {
    cfg.validate().context("invalid configuration")?;
    if cfg.backend == Backend::Telegram {
        anyhow::bail!("{TELEGRAM_REBUILD_REFUSAL}");
    }
    // Phase 8-B B5: the walk carries the production cipher context —
    // built exactly as the Vfs thin shells build theirs (`vfs_config`
    // maps the CyDriveConfig, `from_cfg` reads password-present +
    // configured scheme), so rebuild and read-through share one truth
    // semantics: encrypted rows land cipher-correct (B1/B3) and an
    // existing `is_encrypted=1` row is never downgraded by a listing
    // (T4's invariant, now true for rebuild too).
    let cipher = Some(CipherCtx::from_cfg(&vfs_config(cfg)));
    let db = MetaDatabase::open(Path::new(&cfg.db_path))
        .with_context(|| format!("opening metadata db {:?}", cfg.db_path))?;
    rebuild::rebuild_from_backend_with_ctx(
        driver,
        &db,
        &cloudkit_storage::RelPath::root(),
        limits,
        cipher,
    )
    .await
    .context("rebuilding the index from the backend")
}

/// The P2 CLI forward: when a multi-volume instance is LIVE in this
/// working directory (control file present + PING answers — the status
/// forward's discovery pair), `cydrive rebuild` does not rebuild
/// offline against files the instance is actively serving — it
/// forwards one `REBUILD <name>` per non-telegram volume over the
/// control channel and reports the per-volume replies (the instance's
/// background task does the work; its reply is the acceptance). A
/// telegram volume keeps the shadow-index refusal line, the same
/// wording the offline pass prints.
///
/// `Ok(None)` = no live instance (no control file, or the address is
/// dead): the caller keeps the existing offline pass untouched —
/// zero drift for the not-running case. A live instance whose reply
/// fails to arrive mid-forward reports the transport error on that
/// volume's row without failing the others (the K22 spirit).
pub async fn rebuild_forward_live(
    process_cfg: &CyDriveConfig,
    volumes: &[VolumeConfig],
) -> Result<Option<Vec<(String, String)>>> {
    let addr = match control::read_control_addr(process_cfg) {
        Ok(addr) => addr,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "reading the control file {}",
                    control::control_file_path(process_cfg).display()
                )
            });
        }
    };
    if control::send_ping(addr).await.is_err() {
        // A stale port file: nothing is listening — the offline pass
        // (whose own boot guard removes the file) is the right answer.
        return Ok(None);
    }
    let mut rows = Vec::new();
    for spec in volumes {
        if spec.settings.backend == Backend::Telegram {
            rows.push((
                spec.name.clone(),
                format!("NOT rebuilt — {TELEGRAM_REBUILD_REFUSAL}"),
            ));
            continue;
        }
        match control::send_command(addr, &format!("REBUILD {}", spec.name)).await {
            Ok(reply) => rows.push((spec.name.clone(), reply.trim().to_owned())),
            Err(error) => rows.push((
                spec.name.clone(),
                format!("NOT rebuilt — forwarding the rebuild failed: {error}"),
            )),
        }
    }
    Ok(Some(rows))
}

// ------------------------------- backend dispatch (B3b 段二b unit 5) ---

/// The baidu endpoint set the dispatch assembles the driver params
/// with. Defaults are the production constants (empty inert strings in
/// a binary without the driver — [`Default::default`]); tests inject a
/// loopback mock (`build_backend_transport_with`).
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
    #[cfg(feature = "baidu")]
    fn default() -> Self {
        BaiduEndpoints {
            api_base: ck_baidu::DEFAULT_API_BASE.to_string(),
            oauth_base: ck_baidu::DEFAULT_OAUTH_BASE.to_string(),
            pcs_base: None,
        }
    }

    /// Inert placeholders (K31 / FT2): the struct stays because the
    /// dispatch and run signatures name it, but in a binary without the
    /// driver no baidu surface consumes these values — every consumer
    /// refuses with [`BAIDU_DRIVER_REQUIRED`] before any endpoint is
    /// read.
    #[cfg(not(feature = "baidu"))]
    fn default() -> Self {
        BaiduEndpoints {
            api_base: String::new(),
            oauth_base: String::new(),
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
///
/// Exists only with the `baidu` feature (FT2): every caller sits inside
/// a gated region, and the return type names the driver's params.
#[cfg(feature = "baidu")]
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
/// routes around it). With every driver gated out the enum is
/// uninhabited — the dispatch always refuses, so no value of this type
/// can ever exist.
pub enum BackendTransport {
    /// The baidu transport face over the factory-connected driver.
    /// Exists only with the `baidu` feature (FT2): a binary without the
    /// driver cannot assemble this arm (the dispatch refuses with
    /// [`BAIDU_DRIVER_REQUIRED`] instead).
    #[cfg(feature = "baidu")]
    Baidu(Arc<ck_baidu::BaiduTransport>),
    /// The local transport face over the factory-initialised driver.
    /// Exists only with the `local` feature (FT3): a binary without the
    /// driver cannot assemble this arm (the dispatch refuses with
    /// [`LOCAL_DRIVER_REQUIRED`] instead).
    #[cfg(feature = "local")]
    Local(Arc<ck_local::LocalTransport>),
    /// The sftp transport face over the factory-constructed driver
    /// (Phase 4 / SF3): requires the `sftp` feature — a binary without
    /// the driver refuses with [`SFTP_DRIVER_REQUIRED`] instead.
    #[cfg(feature = "sftp")]
    Sftp(Arc<ck_sftp::SftpTransport>),
    /// The pan115 transport face over the factory-connected driver
    /// (Phase 5 / 115-4): requires the `pan115` feature — a binary
    /// without the driver refuses with [`PAN115_DRIVER_REQUIRED`]
    /// instead.
    #[cfg(feature = "pan115")]
    Pan115(Arc<ck_pan115::Pan115Transport>),
    /// The pan123 transport face over the factory-connected driver
    /// (Phase 6 / 123-4): requires the `pan123` feature — a binary
    /// without the driver refuses with [`PAN123_DRIVER_REQUIRED`]
    /// instead.
    #[cfg(feature = "pan123")]
    Pan123(Arc<ck_pan123::Pan123Transport>),
    /// The webdav transport face over the factory-constructed driver
    /// (Phase 7 / WD1b): requires the `webdav` feature — a binary
    /// without the driver refuses with [`WEBDAV_DRIVER_REQUIRED`]
    /// instead.
    #[cfg(feature = "webdav")]
    Webdav(Arc<ck_webdav::WebdavTransport>),
}

impl BackendTransport {
    /// The assembled volume identity (`baidu:<uid>` / `local:<root>` /
    /// `sftp:<user>@<host>:<port>`).
    pub fn volume(&self) -> &str {
        match self {
            #[cfg(feature = "baidu")]
            BackendTransport::Baidu(t) => StorageDriver::volume(t.driver()).as_str(),
            #[cfg(feature = "local")]
            BackendTransport::Local(t) => StorageDriver::volume(t.driver()).as_str(),
            #[cfg(feature = "sftp")]
            BackendTransport::Sftp(t) => StorageDriver::volume(t.driver()).as_str(),
            #[cfg(feature = "pan115")]
            BackendTransport::Pan115(t) => StorageDriver::volume(t.driver()).as_str(),
            #[cfg(feature = "pan123")]
            BackendTransport::Pan123(t) => StorageDriver::volume(t.driver()).as_str(),
            #[cfg(feature = "webdav")]
            BackendTransport::Webdav(t) => StorageDriver::volume(t.driver()).as_str(),
            // Every driver gated out: the enum is uninhabited — no
            // value can exist. The empty match over the dereferenced
            // place is the never-taken arm a reference scrutinee needs
            // (a reference alone counts as inhabited, E0004).
            #[cfg(not(any(
                feature = "baidu",
                feature = "local",
                feature = "sftp",
                feature = "pan115",
                feature = "pan123",
                feature = "webdav"
            )))]
            _ => match *self {},
        }
    }

    /// The transport face's declared capabilities (the driver's bits
    /// plus the K4 `remote_delete` the faces declare).
    pub fn caps(&self) -> Capabilities {
        match self {
            #[cfg(feature = "baidu")]
            BackendTransport::Baidu(t) => CloudTransport::capabilities(t.as_ref()),
            #[cfg(feature = "local")]
            BackendTransport::Local(t) => CloudTransport::capabilities(t.as_ref()),
            #[cfg(feature = "sftp")]
            BackendTransport::Sftp(t) => CloudTransport::capabilities(t.as_ref()),
            #[cfg(feature = "pan115")]
            BackendTransport::Pan115(t) => CloudTransport::capabilities(t.as_ref()),
            #[cfg(feature = "pan123")]
            BackendTransport::Pan123(t) => CloudTransport::capabilities(t.as_ref()),
            #[cfg(feature = "webdav")]
            BackendTransport::Webdav(t) => CloudTransport::capabilities(t.as_ref()),
            #[cfg(not(any(
                feature = "baidu",
                feature = "local",
                feature = "sftp",
                feature = "pan115",
                feature = "pan123",
                feature = "webdav"
            )))]
            _ => match *self {},
        }
    }

    /// The K12 sync-namespace key this backend derives: baidu — the
    /// raw volume identity `baidu:<uid>` (a stable, non-secret account
    /// id); local — `local:<digest>` over the driver-normalized root
    /// (the raw path never ships to the server; local never starts the
    /// sync task anyway — [`is_sync_supported`]); sftp — the volume
    /// identity `sftp:<user>@<host>:<port>` (no path/credential parts;
    /// like local, sftp never starts the sync task today).
    pub fn sync_namespace_key(&self) -> String {
        match self {
            #[cfg(feature = "baidu")]
            BackendTransport::Baidu(_) => self.volume().to_string(),
            #[cfg(feature = "local")]
            BackendTransport::Local(t) => namespace_key_for(&NamespaceIdentity::Local {
                root: &t.driver().root_path().to_string_lossy(),
            }),
            #[cfg(feature = "sftp")]
            BackendTransport::Sftp(_) => self.volume().to_string(),
            // 115-4: baidu-style raw volume identity (`pan115:<uid>` —
            // the factory connects with the account uid).
            #[cfg(feature = "pan115")]
            BackendTransport::Pan115(_) => self.volume().to_string(),
            // 123-4: baidu-style raw volume identity (`pan123:<uid>` —
            // the factory connects with the account uid).
            #[cfg(feature = "pan123")]
            BackendTransport::Pan123(_) => self.volume().to_string(),
            // WD1b: baidu-style raw volume identity
            // (`webdav:<user>@<base-url>` — offline-constructed, D6; the
            // sync namespace refinement, if any, lands with WD4).
            #[cfg(feature = "webdav")]
            BackendTransport::Webdav(_) => self.volume().to_string(),
            #[cfg(not(any(
                feature = "baidu",
                feature = "local",
                feature = "sftp",
                feature = "pan115",
                feature = "pan123",
                feature = "webdav"
            )))]
            _ => match *self {},
        }
    }

    /// The same transport behind the run flow's `Arc<dyn
    /// CloudTransport>` seam (a clone of the shared Arc — the enum arm
    /// stays usable).
    pub fn clone_dyn(&self) -> Arc<dyn CloudTransport> {
        match self {
            #[cfg(feature = "baidu")]
            BackendTransport::Baidu(t) => t.clone() as Arc<dyn CloudTransport>,
            #[cfg(feature = "local")]
            BackendTransport::Local(t) => t.clone() as Arc<dyn CloudTransport>,
            #[cfg(feature = "sftp")]
            BackendTransport::Sftp(t) => t.clone() as Arc<dyn CloudTransport>,
            #[cfg(feature = "pan115")]
            BackendTransport::Pan115(t) => t.clone() as Arc<dyn CloudTransport>,
            #[cfg(feature = "pan123")]
            BackendTransport::Pan123(t) => t.clone() as Arc<dyn CloudTransport>,
            #[cfg(feature = "webdav")]
            BackendTransport::Webdav(t) => t.clone() as Arc<dyn CloudTransport>,
            #[cfg(not(any(
                feature = "baidu",
                feature = "local",
                feature = "sftp",
                feature = "pan115",
                feature = "pan123",
                feature = "webdav"
            )))]
            _ => match *self {},
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
            #[cfg(feature = "baidu")]
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
            #[cfg(feature = "local")]
            BackendTransport::Local(_) => None,
            #[cfg(feature = "sftp")]
            BackendTransport::Sftp(t) => match StorageDriver::quota(t.driver()).await {
                Ok(quota) => Some(cloudkit_web::QuotaSnapshot {
                    used: quota.used,
                    total: quota.total,
                }),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "sftp quota read failed; the dashboard storage card degrades to \
                         unlimited"
                    );
                    None
                }
            },
            #[cfg(feature = "pan115")]
            BackendTransport::Pan115(t) => match StorageDriver::quota(t.driver()).await {
                Ok(quota) => Some(cloudkit_web::QuotaSnapshot {
                    used: quota.used,
                    total: quota.total,
                }),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "pan115 quota read failed; the dashboard storage card degrades to \
                         unlimited"
                    );
                    None
                }
            },
            #[cfg(feature = "pan123")]
            BackendTransport::Pan123(t) => match StorageDriver::quota(t.driver()).await {
                Ok(quota) => Some(cloudkit_web::QuotaSnapshot {
                    used: quota.used,
                    total: quota.total,
                }),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "pan123 quota read failed; the dashboard storage card degrades to \
                         unlimited"
                    );
                    None
                }
            },
            // WD1b: the sftp shape — the WD1a driver's quota degrades to
            // `total = None` (the RFC 4331 best-effort, appendix C ⑪), so
            // the card renders unlimited by construction; a hard read
            // failure only downgrades, never blocks the boot.
            #[cfg(feature = "webdav")]
            BackendTransport::Webdav(t) => match StorageDriver::quota(t.driver()).await {
                Ok(quota) => Some(cloudkit_web::QuotaSnapshot {
                    used: quota.used,
                    total: quota.total,
                }),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "webdav quota read failed; the dashboard storage card degrades to \
                         unlimited"
                    );
                    None
                }
            },
            #[cfg(not(any(
                feature = "baidu",
                feature = "local",
                feature = "sftp",
                feature = "pan115",
                feature = "pan123",
                feature = "webdav"
            )))]
            _ => match *self {},
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
    // With the driver: production endpoints, the K13 config write-back
    // store and the single-volume cwd state base. Without it: the
    // reduced twin (no driver-typed parameters exist to pass).
    #[cfg(feature = "baidu")]
    let dispatched = build_backend_transport_with(
        cfg,
        &BaiduEndpoints::default(),
        Some(Arc::new(ConfigTokenStore::default())),
        Path::new("."),
    )
    .await?;
    #[cfg(not(feature = "baidu"))]
    let dispatched = build_backend_transport_with(cfg).await?;
    if let Some(warning) = proxy_ineffective_warning(cfg) {
        tracing::warn!("{warning}");
    }
    Ok(dispatched)
}

/// The local dispatch arm, shared by the `local`-feature states of
/// [`build_backend_transport_with`] (FT3): the factory assembly over
/// the config's root. Exists only with the `local` feature — a binary
/// without the driver refuses in the dispatch arm (K31) instead.
#[cfg(feature = "local")]
async fn build_local_transport(cfg: &CyDriveConfig) -> Result<BackendTransport> {
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

/// The sftp dispatch arm, shared by both [`build_backend_transport_with`]
/// twins and [`build_driver`] (Phase 4 / SF3): flatten the `sftp_*`
/// config keys into [`ck_sftp::SftpParams`] and hand them to the factory.
///
/// Credential resolution follows the R3 chain the other drivers use —
/// env var over file value, per key (`CYDRIVE_SFTP_PASSWORD` /
/// `CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE` are the credential-valued
/// keys; the non-secret keys stay file-only, matching the baidu
/// pattern where only the credential keys have env routes). The
/// factory itself only constructs — D3 lazy connect means this returns
/// without touching the network; the first operation (or the transport
/// `connect` probe) establishes the session.
#[cfg(feature = "sftp")]
fn sftp_params(cfg: &CyDriveConfig) -> Result<ck_sftp::SftpParams> {
    // 展平为 (key, value) 对——非空值才入列（空串 = 未设置，与驱动侧
    // empty-means-unset 语义对齐）。
    fn push(pairs: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            pairs.push((key.to_string(), v.to_string()));
        }
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    push(&mut pairs, "sftp_host", cfg.sftp_host.as_deref());
    if let Some(port) = cfg.sftp_port {
        pairs.push(("sftp_port".to_string(), port.to_string()));
    }
    push(&mut pairs, "sftp_username", cfg.sftp_username.as_deref());
    // Credential keys arrive already env-resolved: CYDRIVE_SFTP_PASSWORD /
    // CYDRIVE_SFTP_PRIVATE_KEY_PASSPHRASE ride `with_env_overrides` on the
    // single-volume load path (review fix, K28 — the multi-volume path
    // never applies env overrides, so volume files can't bleed credentials
    // across volumes; reading env here would bypass that guarantee).
    push(&mut pairs, "sftp_password", cfg.sftp_password.as_deref());
    push(
        &mut pairs,
        "sftp_private_key_path",
        cfg.sftp_private_key_path.as_deref(),
    );
    push(
        &mut pairs,
        "sftp_private_key_passphrase",
        cfg.sftp_private_key_passphrase.as_deref(),
    );
    push(
        &mut pairs,
        "sftp_host_fingerprint",
        cfg.sftp_host_fingerprint.as_deref(),
    );
    push(&mut pairs, "sftp_root", cfg.sftp_root.as_deref());
    ck_sftp::SftpParams::from_pairs(&pairs)
        .map_err(|error| anyhow::anyhow!("reading the sftp config keys: {error}"))
}

/// Test seam (the baidu `build_backend_transport_with` precedent): the
/// same pan115 assembly with the endpoint pair overridable, so the
/// dispatch tests can point the driver at a loopback mock instead of
/// proapi.115.com. Not used by production paths.
#[cfg(feature = "pan115")]
#[doc(hidden)]
pub async fn build_pan115_transport_with_endpoints(
    cfg: &CyDriveConfig,
    api_base: &str,
    passport_base: &str,
    token_store: Option<Arc<ConfigTokenStore>>,
    state_dir: Option<&Path>,
) -> Result<BackendTransport> {
    let mut params = pan115_params(cfg)?;
    params.api_base = api_base.to_string();
    params.passport_base = passport_base.to_string();
    params.token_store = token_store.map(|store| store as Arc<dyn ck_pan115::TokenStore>);
    if let Some(dir) = state_dir {
        params.sessions_dir = Some(dir.to_path_buf());
    }
    let driver = ck_pan115::factory(&params)
        .await
        .map_err(|error| anyhow::anyhow!("initialising the pan115 backend: {error}"))?;
    Ok(BackendTransport::Pan115(Arc::new(
        ck_pan115::Pan115Transport::new(driver),
    )))
}

/// Test seam for [`pan115_backend_probe`] (loopback mock endpoints).
#[cfg(feature = "pan115")]
#[doc(hidden)]
pub async fn pan115_backend_probe_with_endpoints(
    cfg: &CyDriveConfig,
    api_base: &str,
    passport_base: &str,
) -> ck_pan115::Pan115Probe {
    match pan115_params(cfg) {
        Ok(mut params) => {
            params.api_base = api_base.to_string();
            params.passport_base = passport_base.to_string();
            ck_pan115::probe(&params).await
        }
        Err(_) => ck_pan115::Pan115Probe::Unreachable {
            detail: "the pan115 config keys could not be read".to_string(),
        },
    }
}

/// Public seam for the multi-volume dispatcher (main.rs): same assembly
/// as the internal [`build_pan115_transport`] with the K13 store and the
/// volume home injected.
#[cfg(feature = "pan115")]
pub async fn build_pan115_transport_with(
    cfg: &CyDriveConfig,
    token_store: Option<Arc<ConfigTokenStore>>,
    state_dir: Option<&Path>,
) -> Result<BackendTransport> {
    build_pan115_transport(cfg, token_store, state_dir).await
}

/// The `pan123_*` config-key flattening shared by the dispatch twins and
/// [`build_driver`] (Phase 6 / 123-4). Credential resolution follows the
/// same R3 chain as the other drivers — the credential key rides
/// `with_env_overrides` on the single-volume load path (K28; reading env
/// here would bypass the multi-volume isolation guarantee).
#[cfg(feature = "pan123")]
fn pan123_params(cfg: &CyDriveConfig) -> Result<ck_pan123::Pan123Params> {
    fn push(pairs: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            pairs.push((key.to_string(), v.to_string()));
        }
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    push(&mut pairs, "pan123_token", cfg.pan123_token.as_deref());
    push(&mut pairs, "pan123_root", cfg.pan123_root.as_deref());
    ck_pan123::Pan123Params::from_pairs(&pairs)
        .map_err(|error| anyhow::anyhow!("reading the pan123 config keys: {error}"))
}

/// The pan123 factory assembly (composition-root R1 exemption: it may
/// name drivers). `factory` connects to read the account uid — the real
/// `pan123:<uid>` volume identity lands there (the placeholder
/// `pan123:pending` must never reach production). `state_dir` carries
/// the volume home when the dispatcher has one (K21: upload sessions
/// and the spool live with the volume).
///
/// No K13 write-back store: the 123 web API has **no refresh** (K76.4)
/// — the token is one-shot-per-90-days, so there is no rotation to
/// persist mid-operation (the baidu/pan115 bridges exist for exactly
/// that rotation; pan123's TokenStore seam only serves the setup
/// wizard's first save).
#[cfg(feature = "pan123")]
async fn build_pan123_transport(
    cfg: &CyDriveConfig,
    state_dir: Option<&Path>,
) -> Result<BackendTransport> {
    let mut params = pan123_params(cfg)?;
    if let Some(dir) = state_dir {
        params.sessions_dir = Some(dir.to_path_buf());
    }
    let driver = ck_pan123::factory(&params)
        .await
        .map_err(|error| anyhow::anyhow!("connecting the pan123 backend: {error}"))?;
    Ok(BackendTransport::Pan123(Arc::new(
        ck_pan123::Pan123Transport::new(driver),
    )))
}

/// Public seam for the multi-volume dispatcher (main.rs): same assembly
/// as [`build_pan123_transport`] with the volume home injected.
#[cfg(feature = "pan123")]
pub async fn build_pan123_transport_with(
    cfg: &CyDriveConfig,
    state_dir: Option<&Path>,
) -> Result<BackendTransport> {
    build_pan123_transport(cfg, state_dir).await
}

/// Test seam for [`build_pan123_transport`] (loopback mock endpoints —
/// the dispatch tests point the three bases at a local mock; same shape
/// as the pan115 endpoints twin).
#[cfg(feature = "pan123")]
pub async fn build_pan123_transport_with_endpoints(
    cfg: &CyDriveConfig,
    api_base: &str,
    fallback_base: &str,
    login_base: &str,
    state_dir: Option<&Path>,
) -> Result<BackendTransport> {
    let mut params = pan123_params(cfg)?;
    params.api_base = api_base.to_string();
    params.fallback_base = fallback_base.to_string();
    params.login_base = login_base.to_string();
    if let Some(dir) = state_dir {
        params.sessions_dir = Some(dir.to_path_buf());
    }
    let driver = ck_pan123::factory(&params)
        .await
        .map_err(|error| anyhow::anyhow!("connecting the pan123 backend: {error}"))?;
    Ok(BackendTransport::Pan123(Arc::new(
        ck_pan123::Pan123Transport::new(driver),
    )))
}

/// The `webdav_*` config-key flattening shared by the dispatch twins and
/// [`build_driver`] (Phase 7 / WD1b). Credential resolution follows the
/// same R3 chain as the other drivers — the credential key rides
/// `with_env_overrides` on the single-volume load path (K28 / B-M1);
/// reading env here would bypass the multi-volume isolation guarantee.
#[cfg(feature = "webdav")]
fn webdav_params(cfg: &CyDriveConfig) -> Result<ck_webdav::WebdavParams> {
    // 展平为 map——非空值才入列（空串 = 未设置，与驱动侧
    // empty-means-unset 语义对齐；WD1a 的 parse_from_map 吃
    // `&HashMap<String, String>`，未知键在驱动侧再挡一道）。
    fn push(map: &mut std::collections::HashMap<String, String>, key: &str, value: Option<&str>) {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            map.insert(key.to_string(), v.to_string());
        }
    }
    let mut map = std::collections::HashMap::new();
    push(&mut map, "webdav_url", cfg.webdav_url.as_deref());
    push(&mut map, "webdav_username", cfg.webdav_username.as_deref());
    // Credential keys arrive already env-resolved: CYDRIVE_WEBDAV_PASSWORD
    // rides `with_env_overrides` on the single-volume load path (B-M1 —
    // the multi-volume path never applies env overrides, so volume files
    // can't bleed credentials across volumes).
    push(&mut map, "webdav_password", cfg.webdav_password.as_deref());
    push(&mut map, "webdav_auth", cfg.webdav_auth.as_deref());
    push(&mut map, "webdav_vendor", cfg.webdav_vendor.as_deref());
    if let Some(flag) = cfg.webdav_accept_invalid_certs {
        map.insert("webdav_accept_invalid_certs".to_string(), flag.to_string());
    }
    ck_webdav::parse_from_map(&map)
        .map_err(|error| anyhow::anyhow!("reading the webdav config keys: {error}"))
}

/// The webdav factory assembly shared by the dispatch twins and
/// [`build_driver`] (composition-root R1 exemption: it may name drivers).
/// `factory` only constructs — the reqwest pool connects lazily (D6), so
/// this returns without touching the network; the first operation (or
/// the transport `connect` probe, WD4) establishes the connection and
/// the D1 auth negotiation. No K13 write-back store (no token rotation —
/// the credentials are static) and no sessions dir (the WD1a stager
/// spools through tempfile's own temp dir).
#[cfg(feature = "webdav")]
async fn build_webdav_transport(cfg: &CyDriveConfig) -> Result<BackendTransport> {
    let params = webdav_params(cfg)?;
    let driver = ck_webdav::factory(&params)
        .await
        .map_err(|error| anyhow::anyhow!("initialising the webdav backend: {error}"))?;
    Ok(BackendTransport::Webdav(Arc::new(
        ck_webdav::WebdavTransport::new(driver),
    )))
}

/// The unified multi-volume backend dispatch (RV2 extraction shared by
/// the main.rs boot loop and the runtime-volume dispatch): K13/K21 —
/// token rotations write back into the volume's own file, upload
/// sessions live in the volume home. The dispatched RunOptions fields
/// (sync namespace, dashboard identity, quota snapshot) land in
/// `run_options`.
pub async fn dispatch_unified_backend_volume(
    spec: &VolumeConfig,
    settings: &CyDriveConfig,
    home: &std::path::Path,
    run_options: &mut RunOptions,
) -> Result<Arc<dyn CloudTransport>> {
    // M-I2 收口（123-4）：单匹配对所有 feature 组合通用；在没有任何
    // 「按卷注入」驱动（baidu/pan115/pan123 全关）的裁剪构建里，spec/
    // home 无人消费——此处显式标记，保持裁剪组合零 unused 告警（K30
    // 纪律：告警即构建噪音，CI 的 -D warnings 腿才不会漂）。
    let _ = (spec, home);
    // Per-backend assembly (K13/K21: token rotations write back into the
    // volume's own file; upload sessions live in the volume home).
    //
    // The baidu arm routes through its endpoint-injecting twin; pan115
    // has its own assembly (the driver connects to read the uid and
    // takes the same write-back store plus the volume home as its
    // session base); pan123 takes the volume home the same way (no
    // write-back store — no refresh protocol); every other backend —
    // including the no-driver refusals (K31) — routes through
    // `build_backend_transport`, whose twins carry the right message
    // per feature. One match for ALL feature combos (review M-I2: the
    // former `not(baidu)+pan115` special block assembled ANY backend
    // through the pan115 path).
    let dispatched = match settings.backend {
        #[cfg(feature = "baidu")]
        Backend::Baidu => {
            let token_store = ConfigTokenStore::new(spec.file_path.clone());
            build_backend_transport_with(
                settings,
                &BaiduEndpoints::default(),
                Some(Arc::new(token_store)),
                home,
            )
            .await?
        }
        #[cfg(feature = "pan115")]
        Backend::Pan115 => {
            let token_store = ConfigTokenStore::new(spec.file_path.clone());
            build_pan115_transport_with(settings, Some(Arc::new(token_store)), Some(home)).await?
        }
        #[cfg(feature = "pan123")]
        Backend::Pan123 => build_pan123_transport_with(settings, Some(home)).await?,
        // WD1b: the webdav assembly needs no per-volume injection (no
        // token store, no sessions dir — the driver constructs offline,
        // D6), so it rides the same single-volume helper both here and
        // through `build_backend_transport`'s default arm.
        #[cfg(feature = "webdav")]
        Backend::Webdav => build_webdav_transport(settings).await?,
        _ => build_backend_transport(settings).await?,
    };
    run_options.sync_namespace = Some(dispatched.sync_namespace_key());
    run_options.web_volume = Some(dispatched.volume().to_string());
    run_options.web_quota = dispatched.web_quota_snapshot().await;
    Ok(dispatched.clone_dyn())
}

/// The `pan115_*` config-key flattening shared by the dispatch twins and
/// [`build_driver`] (Phase 5 / 115-4). Credential resolution follows the
/// same R3 chain as the other drivers — the credential keys ride
/// `with_env_overrides` on the single-volume load path (K28); reading
/// env here would bypass the multi-volume isolation guarantee.
#[cfg(feature = "pan115")]
fn pan115_params(cfg: &CyDriveConfig) -> Result<ck_pan115::Pan115Params> {
    fn push(pairs: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            pairs.push((key.to_string(), v.to_string()));
        }
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    push(
        &mut pairs,
        "pan115_client_id",
        cfg.pan115_client_id.as_deref(),
    );
    push(
        &mut pairs,
        "pan115_access_token",
        cfg.pan115_access_token.as_deref(),
    );
    push(
        &mut pairs,
        "pan115_refresh_token",
        cfg.pan115_refresh_token.as_deref(),
    );
    push(&mut pairs, "pan115_root", cfg.pan115_root.as_deref());
    ck_pan115::Pan115Params::from_pairs(&pairs)
        .map_err(|error| anyhow::anyhow!("reading the pan115 config keys: {error}"))
}

/// The pan115 factory assembly (composition-root R1 exemption: it may
/// name drivers). `factory` connects to read the account uid — the real
/// `pan115:<uid>` volume identity lands there (the placeholder
/// `pan115:pending` must never reach production). `state_dir` carries
/// the volume home when the dispatcher has one (K21: upload sessions
/// and the spool live with the volume).
#[cfg(feature = "pan115")]
async fn build_pan115_transport(
    cfg: &CyDriveConfig,
    token_store: Option<Arc<ConfigTokenStore>>,
    state_dir: Option<&Path>,
) -> Result<BackendTransport> {
    let mut params = pan115_params(cfg)?;
    params.token_store = token_store.map(|store| store as Arc<dyn ck_pan115::TokenStore>);
    if let Some(dir) = state_dir {
        params.sessions_dir = Some(dir.to_path_buf());
    }
    let driver = ck_pan115::factory(&params)
        .await
        .map_err(|error| anyhow::anyhow!("initialising the pan115 backend: {error}"))?;
    Ok(BackendTransport::Pan115(Arc::new(
        ck_pan115::Pan115Transport::new(driver),
    )))
}

/// The sftp factory assembly shared by the dispatch and the rebuild
/// driver builder (composition-root R1 exemption: it may name drivers).
#[cfg(feature = "sftp")]
async fn build_sftp_transport(cfg: &CyDriveConfig) -> Result<BackendTransport> {
    let params = sftp_params(cfg)?;
    let driver = ck_sftp::factory(&params)
        .await
        .map_err(|error| anyhow::anyhow!("initialising the sftp backend: {error}"))?;
    let transport = ck_sftp::SftpTransport::new(driver);
    // 复审修复（2026-09-25 负责人真机报障裁定「不存在就别带病挂载」）：
    // 装配期 connect 门 = 卷根校验——根不存在/不是目录在这里拒绝装配
    //（不挂载、不进上传队列带病重试），connect 的错误自带可行动文案
    //（指名 sftp_root 与路径）。D3 的「factory 不连接」语义不变；装配
    // 后的首个网络动作就是这道门（服务器不可达同样在此拒绝）。
    transport
        .connect()
        .await
        .map_err(|error| anyhow::anyhow!("the sftp volume is not usable: {error}"))?;
    Ok(BackendTransport::Sftp(Arc::new(transport)))
}

/// [`build_backend_transport`] with the endpoint set, the K13
/// `TokenStore` and the instance state directory injected — the test
/// seam (loopback mock backends; a capturing store). `state_dir` is the
/// baidu upload-session base: single-volume callers pass the cwd (`"."`,
/// the frozen behaviour), the multi-volume dispatch passes the volume's
/// home directory (K21). Exists only with the `baidu` feature (FT2 —
/// the injected types live in the driver).
#[cfg(feature = "baidu")]
pub async fn build_backend_transport_with(
    cfg: &CyDriveConfig,
    endpoints: &BaiduEndpoints,
    token_store: Option<Arc<dyn ck_baidu::TokenStore>>,
    state_dir: &Path,
) -> Result<BackendTransport> {
    match cfg.backend {
        Backend::Telegram => {
            // With the driver: the legacy arm's guidance — the run flow
            // owns the connect (byte-for-byte since pre-Phase-2).
            // Without the driver: the run flow cannot connect it either,
            // so the arm carries the actionable rebuild message (K31).
            #[cfg(feature = "telegram")]
            {
                anyhow::bail!(
                    "the telegram backend does not assemble through the backend dispatch: the run \
                     flow connects it through its own deadline-bounded GrammersTransport path \
                     (unchanged since pre-Phase-2); a config without the backend key is telegram"
                )
            }
            #[cfg(not(feature = "telegram"))]
            {
                anyhow::bail!("{TELEGRAM_DRIVER_REQUIRED}")
            }
        }
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
            // With the driver: the factory assembly sharing
            // [`build_local_transport`] (validate() guarantees Some +
            // absolute when backend=local). Without it: the actionable
            // rebuild message (K31).
            #[cfg(feature = "local")]
            {
                build_local_transport(cfg).await
            }
            #[cfg(not(feature = "local"))]
            {
                anyhow::bail!("{LOCAL_DRIVER_REQUIRED}")
            }
        }
        // Phase 4 / SF3: the sftp dispatch arm — with the driver the
        // factory assembly ([`build_sftp_transport`]); without it the
        // actionable rebuild message (K31).
        Backend::Sftp => {
            #[cfg(feature = "sftp")]
            {
                build_sftp_transport(cfg).await
            }
            #[cfg(not(feature = "sftp"))]
            {
                anyhow::bail!("{SFTP_DRIVER_REQUIRED}")
            }
        }
        // Phase 5 / 115-4: the pan115 factory assembly — connect with
        // the K13 write-back store and the volume home as the session
        // base (K21; the single-volume caller below passes the cwd).
        Backend::Pan115 => {
            #[cfg(feature = "pan115")]
            {
                build_pan115_transport(
                    cfg,
                    Some(Arc::new(ConfigTokenStore::default())),
                    Some(Path::new(".")),
                )
                .await
            }
            #[cfg(not(feature = "pan115"))]
            {
                anyhow::bail!("{PAN115_DRIVER_REQUIRED}")
            }
        }
        // Phase 6 / 123-4: the pan123 factory assembly — connect reads
        // the account uid (`pan123:<uid>`), the volume home (here the
        // cwd — the single-volume form) anchors the upload sessions
        // (K21). No K13 store: web API has no refresh (K76.4).
        Backend::Pan123 => {
            #[cfg(feature = "pan123")]
            {
                build_pan123_transport(cfg, Some(Path::new("."))).await
            }
            #[cfg(not(feature = "pan123"))]
            {
                anyhow::bail!("{PAN123_DRIVER_REQUIRED}")
            }
        }
        // Phase 7 / WD1b: the webdav factory assembly — construction
        // only (D6 lazy connect), no store and no session dir to inject.
        // Without the driver: the actionable rebuild message (K31).
        Backend::Webdav => {
            #[cfg(feature = "webdav")]
            {
                build_webdav_transport(cfg).await
            }
            #[cfg(not(feature = "webdav"))]
            {
                anyhow::bail!("{WEBDAV_DRIVER_REQUIRED}")
            }
        }
    }
}

/// The no-driver twin (K31 / FT2): the baidu arm carries the actionable
/// rebuild message — the binary cannot assemble a baidu transport —
/// while the telegram arm keeps its [`build_backend_transport_with`]
/// guidance pair and the local arm follows the `local` feature (FT3:
/// the factory assembly on, the K31 rebuild message off).
#[cfg(not(feature = "baidu"))]
pub async fn build_backend_transport_with(cfg: &CyDriveConfig) -> Result<BackendTransport> {
    match cfg.backend {
        Backend::Telegram => {
            #[cfg(feature = "telegram")]
            {
                anyhow::bail!(
                    "the telegram backend does not assemble through the backend dispatch: the run \
                     flow connects it through its own deadline-bounded GrammersTransport path \
                     (unchanged since pre-Phase-2); a config without the backend key is telegram"
                )
            }
            #[cfg(not(feature = "telegram"))]
            {
                anyhow::bail!("{TELEGRAM_DRIVER_REQUIRED}")
            }
        }
        Backend::Baidu => anyhow::bail!("{BAIDU_DRIVER_REQUIRED}"),
        Backend::Local => {
            #[cfg(feature = "local")]
            {
                build_local_transport(cfg).await
            }
            #[cfg(not(feature = "local"))]
            {
                anyhow::bail!("{LOCAL_DRIVER_REQUIRED}")
            }
        }
        // Phase 4 / SF3 — the sftp arm follows its feature like the
        // local arm above.
        Backend::Sftp => {
            #[cfg(feature = "sftp")]
            {
                build_sftp_transport(cfg).await
            }
            #[cfg(not(feature = "sftp"))]
            {
                anyhow::bail!("{SFTP_DRIVER_REQUIRED}")
            }
        }
        // Phase 5 / 115-4: same assembly as the baidu-feature twin above
        // (this no-baidu twin still carries the pan115 arm when the
        // `pan115` feature is on) — including the K13 write-back store
        // and the cwd session base (review M-I1: passing None, None lost
        // every rotation — a one-use refresh_token pair that is never
        // written back is a dead credential after restart).
        Backend::Pan115 => {
            #[cfg(feature = "pan115")]
            {
                build_pan115_transport(
                    cfg,
                    Some(Arc::new(ConfigTokenStore::default())),
                    Some(Path::new(".")),
                )
                .await
            }
            #[cfg(not(feature = "pan115"))]
            {
                anyhow::bail!("{PAN115_DRIVER_REQUIRED}")
            }
        }
        // Phase 6 / 123-4: same assembly as the baidu-feature twin above
        // (this no-baidu twin still carries the pan123 arm when the
        // `pan123` feature is on) — cwd session base, no K13 store (no
        // refresh protocol, K76.4).
        Backend::Pan123 => {
            #[cfg(feature = "pan123")]
            {
                build_pan123_transport(cfg, Some(Path::new("."))).await
            }
            #[cfg(not(feature = "pan123"))]
            {
                anyhow::bail!("{PAN123_DRIVER_REQUIRED}")
            }
        }
        // Phase 7 / WD1b: same assembly as the baidu-feature twin above
        // (this no-baidu twin still carries the webdav arm when the
        // `webdav` feature is on) — construction only, nothing injected.
        Backend::Webdav => {
            #[cfg(feature = "webdav")]
            {
                build_webdav_transport(cfg).await
            }
            #[cfg(not(feature = "webdav"))]
            {
                anyhow::bail!("{WEBDAV_DRIVER_REQUIRED}")
            }
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
///
/// Exists only with the `baidu` feature (FT2): its behavior IS the
/// driver's [`ck_baidu::TokenStore`] seam — a binary without the driver
/// has no token rotation to persist.
#[cfg(any(feature = "baidu", feature = "pan115"))]
pub struct ConfigTokenStore {
    path: PathBuf,
}

#[cfg(any(feature = "baidu", feature = "pan115"))]
impl ConfigTokenStore {
    /// Targets `path` (production: `config.toml` in the cwd —
    /// [`ConfigTokenStore::default`]; tests inject a temp file).
    pub fn new(path: PathBuf) -> Self {
        ConfigTokenStore { path }
    }
}

#[cfg(any(feature = "baidu", feature = "pan115"))]
impl Default for ConfigTokenStore {
    fn default() -> Self {
        ConfigTokenStore {
            path: PathBuf::from("config.toml"),
        }
    }
}

#[cfg(feature = "baidu")]
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

/// The pan115 write-back leg of the same store (Phase 5 / 115-4): a
/// refresh rotation mid-operation must persist, or the next boot reads a
/// dead refresh_token — 115 rotates the pair on every refresh (K69.1).
#[cfg(feature = "pan115")]
impl ck_pan115::TokenStore for ConfigTokenStore {
    fn save_tokens(&self, access_token: &str, refresh_token: &str) {
        let report = |message: String| {
            tracing::warn!(
                path = %self.path.display(),
                "{message}"
            );
        };
        let Ok(mut cfg) = CyDriveConfig::load_toml(&self.path) else {
            report(
                "rotated pan115 tokens were NOT persisted: no readable config.toml — the \
                 credentials likely came from CYDRIVE_PAN115_* env (update them there); \
                 the new refresh_token is now the only live value"
                    .to_string(),
            );
            return;
        };
        cfg.pan115_access_token = Some(access_token.to_string());
        cfg.pan115_refresh_token = Some(refresh_token.to_string());
        if let Err(error) = cfg.save_toml(&self.path) {
            report(format!(
                "rotated pan115 tokens were NOT persisted (write failed: {error}); the \
                 new refresh_token is now the only live value"
            ));
        }
    }
}

// -------------------------------------------- K12 / K18 warning helpers ---

/// K18: `proxy_url` has no effect on the baidu/local/sftp backends (their
/// drivers always connect directly — no_proxy + forced IPv4; the sftp
/// transport has no proxy support); the
/// assembly logs this warning and doctor repeats it. `None` on
/// telegram (the proxy is a live setting there) or when no proxy is
/// configured.
pub fn proxy_ineffective_warning(cfg: &CyDriveConfig) -> Option<&'static str> {
    if cfg.backend == Backend::Telegram {
        return None;
    }
    if cfg.proxy_url.as_deref().is_some_and(|p| !p.is_empty()) {
        // webdav 刻意尊重系统代理 env（自备服务器 = 用户自己的网络路径），
        // 「恒直连」声明对它不成立——专属文案避免误导排查方向
        //（Phase 7 审查 M14；pan123 落地时的文案联动先例同族）。
        if cfg.backend == Backend::Webdav {
            return Some(WEBDAV_PROXY_NOTICE);
        }
        return Some(PROXY_DIRECT_BACKEND_NOTICE);
    }
    None
}

/// The K18 declaration text shared by the assembly log and doctor.
pub const PROXY_DIRECT_BACKEND_NOTICE: &str =
    "proxy_url is set but has no effect on this backend: baidu/local/sftp/pan115/pan123 always \
     connect directly (no_proxy + forced IPv4; the sftp transport has no proxy support); the \
     proxy only serves the telegram transport";

/// The webdav variant (Phase 7 审查 M14): the config key is not read by the
/// driver, but its client deliberately honors the standard proxy environment
/// variables — the honest diagnosis names both halves.
pub const WEBDAV_PROXY_NOTICE: &str =
    "proxy_url is set but the webdav driver does not read this config key; it connects with the \
     standard http_proxy/https_proxy environment variables instead (deliberate: user-provided \
     servers ride the user's own network path)";

/// K12: a local instance cannot run the metadata-sync task (the local
/// root IS the source of truth); a `sync_url` on such an instance is a
/// misconfiguration surfaced as this warning (the sync task's start
/// gate and doctor share the text). `None` for every other shape.
/// Phase 4 / SF3: the sftp backend got the same ruling (the remote
/// filesystem is the source of truth) — the warning text follows the
/// backend ([`SFTP_SYNC_UNSUPPORTED`] for sftp).
pub fn local_sync_unsupported_warning(cfg: &CyDriveConfig) -> Option<&'static str> {
    (cfg.sync_url.is_some() && !cloudkit_core::sync::is_sync_supported(&cfg.backend)).then_some({
        match cfg.backend {
            Backend::Sftp => SFTP_SYNC_UNSUPPORTED,
            _ => LOCAL_SYNC_UNSUPPORTED,
        }
    })
}

/// The K12 warning text shared by the sync-start gate and doctor.
pub const LOCAL_SYNC_UNSUPPORTED: &str =
    "sync is not supported for the local backend: the periodic sync task stays off and \
     the sync_url key has no effect";

/// The K12 warning text for the sftp backend (Phase 4 / SF3): the remote
/// filesystem is the source of truth — same ruling as local.
/// The pan115 doctor probe leg (Phase 5 / 115-4): assembles the driver
/// params from config and runs [`ck_pan115::probe`] — `user/info` for
/// token liveness plus a quota read. Requires the `pan115` feature (the
/// probe types live in the driver); a binary without it skips the
/// dial-out leg (K31 shape, the baidu rule): no fake Unreachable.
#[cfg(feature = "pan115")]
pub async fn pan115_backend_probe(cfg: &CyDriveConfig) -> ck_pan115::Pan115Probe {
    match pan115_params(cfg) {
        Ok(params) => ck_pan115::probe(&params).await,
        Err(_) => ck_pan115::Pan115Probe::Unreachable {
            detail: "the pan115 config keys could not be read (see doctor's validate pass)"
                .to_string(),
        },
    }
}

/// The pan123 doctor probe leg (Phase 6 / 123-4): assembles the driver
/// params from config and runs [`ck_pan123::probe`] — `user/info` for
/// token liveness plus space/vip fields, with the best-effort
/// `traffic/check` remain read (the D5 display face). Requires the
/// `pan123` feature (the probe types live in the driver); a binary
/// without it skips the dial-out leg (K31 shape, the baidu rule): no
/// fake Unreachable.
#[cfg(feature = "pan123")]
pub async fn pan123_backend_probe(cfg: &CyDriveConfig) -> ck_pan123::Pan123Probe {
    match pan123_params(cfg) {
        Ok(params) => ck_pan123::probe(&params).await,
        Err(_) => ck_pan123::Pan123Probe::Unreachable {
            detail: "the pan123 config keys could not be read (see doctor's validate pass)"
                .to_string(),
        },
    }
}

/// The K12 warning text for the sftp backend (Phase 4 / SF3): the remote
/// filesystem is the source of truth — same ruling as local.
pub const SFTP_SYNC_UNSUPPORTED: &str =
    "sync is not supported for the sftp backend: the remote filesystem is the source of \
     truth, so the periodic sync task stays off and the sync_url key has no effect";

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
///
/// Exists only with the `baidu` feature (FT2 / K31): a binary without
/// the driver has no token to probe — doctor skips the baidu liveness
/// leg entirely instead of reporting a fake Unreachable.
#[cfg(feature = "baidu")]
pub async fn baidu_backend_probe(cfg: &CyDriveConfig) -> BackendProbe {
    // 审查修复 M1（2026-09-25）：探针必须携带写回 store——探针期的 110
    // 轮换一次一换，不落盘 = 烧毁唯一活 refresh_token（doctor 恰在 token
    // 疑似过期时被运行）。与 run 单卷路径同源：cwd 的 config.toml。
    let store: Arc<dyn ck_baidu::TokenStore> = Arc::new(ConfigTokenStore::default());
    baidu_backend_probe_with(cfg, &BaiduEndpoints::default(), store).await
}

/// The sftp backend probe (Phase 4 / SF3 doctor leg): connects once
/// (TCP/KEX/host-key/auth/subsystem) and returns the driver's structured
/// verdict — including the D2 host-key three-state detail the driver's
/// `Unauthorized` cannot carry (L2's frozen no-payload contract). The
/// driver gate mirrors the baidu probe's K31 rule: a binary without the
/// `sftp` feature cannot dial, so the CLI skips this leg entirely.
#[cfg(feature = "sftp")]
pub async fn sftp_backend_probe(cfg: &CyDriveConfig) -> ck_sftp::SftpProbe {
    // Incomplete config never reaches the network: the validate pass
    // gives the actionable verdict first (the sftp arm of validate()).
    let params = match sftp_params(cfg) {
        Ok(params) => params,
        Err(error) => {
            return ck_sftp::SftpProbe::Unreachable(format!(
                "the sftp configuration is incomplete ({error}); set the sftp_* keys in \
                 config.toml and retry"
            ));
        }
    };
    ck_sftp::probe(&params).await
}

/// The webdav doctor probe leg (Phase 7 / WD4): assembles the driver
/// params from config and runs [`ck_webdav::probe`] — one OPTIONS round
/// with the D1 auth negotiation, classified into the five-state
/// [`ck_webdav::WebdavProbe`] (the renderer is
/// [`doctor::webdav_connectivity_check`]). Requires the `webdav` feature
/// (the probe types live in the driver); a binary without it skips the
/// dial-out leg (K31 shape, the baidu rule): no fake Unreachable.
///
/// Incomplete config never reaches the network (the sftp probe's rule);
/// the whole probe is bounded by a 45s outer deadline (the baidu probe's
/// wall) so a blackholed host cannot hang an interactive doctor run
/// (the driver's own retry whitelist + 15s connect timeout bound one
/// leg; the deadline bounds their sum).
#[cfg(feature = "webdav")]
pub async fn webdav_backend_probe(cfg: &CyDriveConfig) -> ck_webdav::WebdavProbe {
    let params = match webdav_params(cfg) {
        Ok(params) => params,
        Err(error) => {
            return ck_webdav::WebdavProbe::Unreachable {
                detail: format!(
                    "the webdav configuration is incomplete ({error}); set the webdav_* keys \
                     in config.toml and retry"
                ),
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(45), ck_webdav::probe(&params)).await {
        Ok(probe) => probe,
        Err(_elapsed) => ck_webdav::WebdavProbe::Unreachable {
            detail: "the probe did not finish within 45s (network path to the server?)".to_string(),
        },
    }
}

/// [`baidu_backend_probe`] with the endpoint set injected (tests point
/// it at a loopback mock).
#[cfg(feature = "baidu")]
pub async fn baidu_backend_probe_with(
    cfg: &CyDriveConfig,
    endpoints: &BaiduEndpoints,
    token_store: Arc<dyn ck_baidu::TokenStore>,
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
        let params = baidu_params(cfg, endpoints, Some(token_store), Path::new("."));
        let driver = ck_baidu::factory(&params).await?;
        // quota API 在本 appkey 下恒 error_code=3 "Unsupported open api"
        // （2026-09-25 真机探针实证）——探针只承重 token 活性，list 足证。
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
        // With the driver: the factory assembly sharing [`baidu_params`]
        // with the transport dispatch. Without it: the actionable
        // rebuild message (K31) — `rebuild` cannot walk a backend the
        // binary cannot talk to.
        #[cfg(feature = "baidu")]
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
        #[cfg(not(feature = "baidu"))]
        Backend::Baidu => anyhow::bail!("{BAIDU_DRIVER_REQUIRED}"),
        // With the driver: the factory assembly (validate() guarantees
        // Some + absolute when backend=local). Without it: the
        // actionable rebuild message (K31) — `rebuild` cannot walk a
        // volume the binary cannot index.
        #[cfg(feature = "local")]
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
        #[cfg(not(feature = "local"))]
        Backend::Local => anyhow::bail!("{LOCAL_DRIVER_REQUIRED}"),
        // With the driver: the factory assembly ([`sftp_params`] +
        // ck-sftp factory, D3 lazy connect). Without it: the actionable
        // rebuild message (K31) — `rebuild` cannot walk a volume the
        // binary cannot index.
        #[cfg(feature = "sftp")]
        Backend::Sftp => {
            let params = sftp_params(cfg)?;
            let driver = ck_sftp::factory(&params)
                .await
                .map_err(|error| anyhow::anyhow!("initialising the sftp backend: {error}"))?;
            Ok(driver)
        }
        #[cfg(not(feature = "sftp"))]
        Backend::Sftp => anyhow::bail!("{SFTP_DRIVER_REQUIRED}"),
        // Phase 5 / 115-1 placeholder: the pan115 factory assembly (the
        // `pan115_params` mapping + ConfigTokenStore bridge) is a 115-4
        // item; the driver itself compiles with placeholder methods until
        // 115-2/3. Both arms refuse loudly until then.
        // With the driver: the factory assembly (connect reads the uid)
        // plus the K13 write-back store — a rebuild walk must persist a
        // rotation or the next boot reads a dead refresh_token (the
        // sftp arm has no rotation; baidu's is the shape mirrored here).
        #[cfg(feature = "pan115")]
        Backend::Pan115 => {
            let mut params = pan115_params(cfg)?;
            params.token_store = Some(Arc::new(ConfigTokenStore::default()));
            let driver = ck_pan115::factory(&params)
                .await
                .map_err(|error| anyhow::anyhow!("initialising the pan115 backend: {error}"))?;
            Ok(driver)
        }
        #[cfg(not(feature = "pan115"))]
        Backend::Pan115 => anyhow::bail!("{PAN115_DRIVER_REQUIRED}"),
        // Phase 6 / 123-4: the pan123 factory assembly (connect reads
        // the uid). No K13 write-back store — no refresh protocol
        // (K76.4). The rebuild walk rides the driver's conservative
        // token-bucket limiter (K62.3 形态——LimiterConfig 缺省即保守
        // 节拍，与驱动全生命周期共用同一只桶).
        #[cfg(feature = "pan123")]
        Backend::Pan123 => {
            let params = pan123_params(cfg)?;
            let driver = ck_pan123::factory(&params)
                .await
                .map_err(|error| anyhow::anyhow!("connecting the pan123 backend: {error}"))?;
            Ok(driver)
        }
        #[cfg(not(feature = "pan123"))]
        Backend::Pan123 => anyhow::bail!("{PAN123_DRIVER_REQUIRED}"),
        // Phase 7 / WD1b: the webdav factory assembly — the rebuild walk
        // rides the same offline construction (D6; the walk's first list
        // opens the pool). Without it: the actionable rebuild message
        // (K31) — `rebuild` cannot walk a volume the binary cannot
        // talk to.
        #[cfg(feature = "webdav")]
        Backend::Webdav => {
            let params = webdav_params(cfg)?;
            let driver = ck_webdav::factory(&params)
                .await
                .map_err(|error| anyhow::anyhow!("initialising the webdav backend: {error}"))?;
            Ok(driver)
        }
        #[cfg(not(feature = "webdav"))]
        Backend::Webdav => anyhow::bail!("{WEBDAV_DRIVER_REQUIRED}"),
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

// ------------------------------------------- mount backends (Phase 3 / WF4) ---
//
// K87/K89: `mount_backend = "winfsp" | "webdav"` selects how a volume
// becomes a Windows drive letter; the default is `winfsp` (in-process
// native mount, and the feature is compiled into the default binary).
// `webdav` is the `net use` mapping onto the process WebDAV endpoint —
// byte-identical to the Python baseline — and needs one admin
// `cydrive fix-reg` (explicit opt-in). The winfsp arm needs one thing
// the config cannot guarantee: an installed WinFsp runtime. When the
// runtime is missing (or a mount fails) there is **no net use
// fallback**: the volume keeps no drive letter, stays reachable through
// the WebDAV endpoint/dashboard, and the reason is logged and printed
// visibly.
//
// The decision is a pure function of the config and the capability, which
// is what makes it testable in both build legs (`tests/mount_backend.rs`).

/// The rebuild/switch hint a winfsp request gets from a binary built
/// without the feature (the K31 driver-message precedent: name the rebuild
/// command, then name the way out). Pinned by `tests/mount_backend.rs`
/// in the default leg, where it is what the user actually sees.
pub const WINFSP_FEATURE_REQUIRED: &str = "this binary was built without the winfsp mount \
     backend; rebuild with `cargo build -p cloudkit-cli --features winfsp`, or set \
     `mount_backend = \"webdav\"` in config.toml";

/// Whether this build *and* this machine can mount through WinFsp — K40's
/// decision input, assembled once per boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinFspCapability {
    /// Compiled with the feature and the runtime is ready (the registry
    /// found an install, its DLL loaded, `winfsp_init` answered Ok).
    Ready,
    /// Built without `--features winfsp`: no code path can mount (K38).
    NotCompiled,
    /// Compiled in, but the machine cannot serve it; the string is the
    /// runtime probe's actionable reason.
    Unavailable(String),
}

/// What the mount flow will actually do (K40's pure decision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountBackendDecision {
    /// Mount in-process through WinFsp.
    WinFsp,
    /// Map the WebDAV endpoint with `net use` (the default, and what an
    /// inherited config without the key gets).
    WebDav,
    /// `mount_backend = "winfsp"` was asked for and cannot be honoured;
    /// the reason is user-facing and the WebDAV mapping runs instead.
    WinfspUnavailable(String),
}

impl MountBackendDecision {
    /// `true` for [`MountBackendDecision::WinFsp`].
    pub fn is_winfsp(&self) -> bool {
        matches!(self, MountBackendDecision::WinFsp)
    }

    /// The banner/status spelling: the backend that runs, plus the reason
    /// when it is a fallback (never a silent downgrade).
    pub fn label(&self) -> String {
        match self {
            MountBackendDecision::WinFsp => "winfsp".to_string(),
            MountBackendDecision::WebDav => "webdav".to_string(),
            MountBackendDecision::WinfspUnavailable(reason) => {
                format!("winfsp (unavailable: {reason})")
            }
        }
    }
}

/// How a volume actually ended up mounted — the per-volume banner payload
/// (`MultiVolumeHandle::mounted_volumes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountedBackend {
    /// In-process WinFsp mount.
    WinFsp,
    /// `net use` mapping (the requested backend, or the default).
    WebDav,
    /// A winfsp request that landed on WebDAV; the reason says why
    /// (runtime unavailable, feature missing, or this volume's own mount
    /// failure).
    WebDavFallback {
        /// User-facing degradation reason.
        reason: String,
    },
}

impl MountedBackend {
    /// The stable lowercase backend name (`"winfsp"` / `"webdav"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            MountedBackend::WinFsp => "winfsp",
            MountedBackend::WebDav | MountedBackend::WebDavFallback { .. } => "webdav",
        }
    }

    /// The banner spelling: the backend, plus `(fallback: …)` whenever the
    /// config asked for something else.
    pub fn label(&self) -> String {
        match self {
            MountedBackend::WinFsp => "winfsp".to_string(),
            MountedBackend::WebDav => "webdav".to_string(),
            MountedBackend::WebDavFallback { reason } => {
                format!("webdav (fallback: {reason})")
            }
        }
    }
}

/// The pure decision: the config's request against what this
/// build/machine can do. `webdav` is never upgraded; a `winfsp` request
/// that cannot run does NOT fall back to webdav (负责人 2026-09-24 裁决) —
/// it answers [`MountBackendDecision::WinfspUnavailable`] with the reason
/// and the volume stays reachable without a drive letter.
pub fn choose_mount_backend(
    backend: MountBackend,
    capability: &WinFspCapability,
) -> MountBackendDecision {
    match backend {
        MountBackend::Webdav => MountBackendDecision::WebDav,
        MountBackend::Winfsp => match capability {
            WinFspCapability::Ready => MountBackendDecision::WinFsp,
            WinFspCapability::NotCompiled => {
                MountBackendDecision::WinfspUnavailable(WINFSP_FEATURE_REQUIRED.to_string())
            }
            WinFspCapability::Unavailable(reason) => {
                MountBackendDecision::WinfspUnavailable(reason.clone())
            }
        },
    }
}

/// This build's + machine's WinFsp capability. With the feature it probes
/// the real runtime ([`cloudkit_winfsp::mount::winfsp_status`], whose
/// verdict is cached for the process); without it — or off Windows, where
/// the crate compiles to an empty library — the answer is
/// [`WinFspCapability::NotCompiled`], which is what makes the degradation
/// path the *default* outcome of asking for winfsp.
#[cfg(all(windows, feature = "winfsp"))]
pub fn winfsp_capability() -> WinFspCapability {
    match cloudkit_winfsp::mount::winfsp_status() {
        cloudkit_winfsp::mount::WinFspStatus::Ready { .. } => WinFspCapability::Ready,
        cloudkit_winfsp::mount::WinFspStatus::Unavailable(reason) => {
            WinFspCapability::Unavailable(reason)
        }
    }
}

/// [`winfsp_capability`] for every build that cannot mount through WinFsp
/// (no feature, or not Windows — the mount is Windows-only).
#[cfg(not(all(windows, feature = "winfsp")))]
pub fn winfsp_capability() -> WinFspCapability {
    WinFspCapability::NotCompiled
}

/// The one-line notice an unavailable-winfsp boot prints (and logs at error
/// level): the backend, the reason, the fact that drive letters are simply
/// not mounted, and the two ways up.
pub fn winfsp_unavailable_notice(reason: &str) -> String {
    format!(
        "winfsp mount backend unavailable ({reason}); drive letters are not mounted (no \
         mapping (net use) - install the WinFsp runtime, or set mount_backend =
         \"webdav\" in config.toml to opt into it. The volumes stay reachable"
    )
}

/// Single-volume mode's scope note (WF4 ruling, records in the tracker):
/// the in-process backend is wired into the multi-volume flow, where every
/// volume carries its own explicit `drive_letter` claim (K27). A
/// single-volume config that asked for `winfsp` gets the WebDAV mapping
/// and this note — degradation stays visible (K40), never silent.
pub fn single_volume_winfsp_note(cfg: &CyDriveConfig) -> Option<&'static str> {
    (cfg.mount_backend == MountBackend::Winfsp).then_some(
        "mount_backend = \"winfsp\" applies to the multi-volume flow (one in-process mount per \
         volume claim); this single-volume instance keeps the WebDAV drive mapping — split the \
         instance into `volumes_dir` volumes for the winfsp backend, or set mount_backend = \
         \"webdav\"",
    )
}

/// The in-process note text itself — [`winfsp_unmount_note`]'s payload
/// and the [`WinfspUnmountStep::ExplainInProcess`] arm's content, kept in
/// one place so the two spellings cannot drift.
pub const WINFSP_UNMOUNT_IN_PROCESS_NOTE: &str =
    "nothing to unmount: `mount_backend = \"winfsp\"` mounts the volume in-process, and an \
     in-process mount is released when its process exits — WinFsp has no cross-process \
     unmount. Stop that process instead (Ctrl+C in its console, or `cydrive stop`). No \
     `net use` mapping is created by the winfsp backend; if the letter carries one you \
     mapped yourself, remove it with `net use <letter> /delete`.";

/// `cydrive unmount`'s answer for a winfsp-backed instance (Phase 3 /
/// WF4): an in-process mount lives exactly as long as the process that
/// created it, and WinFsp has no cross-process unmount — so there is
/// nothing this command can release, and it must not pretend otherwise (a
/// `net use /delete` on the configured letter would target a mapping that
/// is not ours). `None` for the WebDAV backend, whose mapping *is*
/// cross-process.
pub fn winfsp_unmount_note(cfg: &CyDriveConfig) -> Option<&'static str> {
    (cfg.mount_backend == MountBackend::Winfsp).then_some(WINFSP_UNMOUNT_IN_PROCESS_NOTE)
}

/// What `cydrive unmount` (Windows) does for a winfsp-configured
/// instance, as a pure function of the backend and the *probed* mapping
/// (RB3 / cli-H1): a degraded winfsp boot created a real `net use`
/// mapping (K40's visible fallback) and that mapping is cross-process and
/// ours — unmount releases it, at the letter the probe actually found,
/// instead of refusing. Only a winfsp instance with no mapping for its
/// drive URL gets the in-process note; the WebDAV backend keeps its
/// resolve-and-release flow (`None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinfspUnmountStep {
    /// The instance's drive URL is mapped at this actual letter — a
    /// degraded boot's WebDAV fallback. Release it, saying that is what
    /// happened.
    ReleaseDegradedMapping {
        /// The probe-verified letter (`net use`'s own answer, which an
        /// explicit `--letter` on the original mount may override).
        letter: String,
    },
    /// No mapping for the drive URL: print the in-process note and touch
    /// nothing ([`WINFSP_UNMOUNT_IN_PROCESS_NOTE`]).
    ExplainInProcess,
}

/// The decision behind [`WinfspUnmountStep`]. The probe input is the
/// status path's [`cloudkit_platform::current_mount_for`] against the
/// glued drive URL — the same read-only `net use` scan the degraded
/// boot's mapping answers; never a mutation.
pub fn winfsp_unmount_step(
    backend: MountBackend,
    probed: Option<String>,
) -> Option<WinfspUnmountStep> {
    match backend {
        MountBackend::Webdav => None,
        MountBackend::Winfsp => Some(match probed {
            Some(letter) => WinfspUnmountStep::ReleaseDegradedMapping { letter },
            None => WinfspUnmountStep::ExplainInProcess,
        }),
    }
}

/// One volume's completed mount (K27 + K40): the name, the drive letter
/// it claimed and the backend that actually carried it — the banner
/// annotates every volume with the last two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedVolume {
    /// The volume name (the file stem, K19).
    pub volume: String,
    /// The drive letter actually mounted (`"Q:"`).
    pub letter: String,
    /// Which backend carried it (and, when degraded, why).
    pub backend: MountedBackend,
}

/// The live in-process WinFsp mounts a mount pass produced, as this
/// build can hold them: `(volume name, handle)` pairs so each handle
/// lands in its own volume's live entry (RV2 — the stop sequence
/// releases through the per-volume [`DriveRelease`]s, runtime REMOVE
/// through the same).
///
/// With the feature it wraps
/// [`cloudkit_winfsp::mount::MountHandle`]s; without it the set is
/// necessarily empty (the arm degrades to WebDAV before a handle could
/// exist) — the type itself stays the same so the boot and stop code paths
/// are written once.
#[cfg(all(windows, feature = "winfsp"))]
#[derive(Default)]
pub struct WinFspHandles(Vec<(String, cloudkit_winfsp::mount::MountHandle)>);

/// The no-feature twin: nothing to hold, ever (see the feature'd type).
#[cfg(not(all(windows, feature = "winfsp")))]
#[derive(Default)]
pub struct WinFspHandles;

#[cfg(all(windows, feature = "winfsp"))]
impl WinFspHandles {
    /// Parks one live mount (with its volume's name) for the per-volume
    /// live entries.
    fn push(&mut self, volume: String, handle: cloudkit_winfsp::mount::MountHandle) {
        self.0.push((volume, handle));
    }

    /// `true` when no mount was parked.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Hands every parked mount out (name + handle), emptying the set —
    /// the mount pass's caller parks each handle into its volume's live
    /// entry (RV2's per-volume release plumbing).
    pub fn drain(&mut self) -> Vec<(String, cloudkit_winfsp::mount::MountHandle)> {
        std::mem::take(&mut self.0)
    }
}

#[cfg(not(all(windows, feature = "winfsp")))]
impl WinFspHandles {
    /// `true` when no mount was parked — always, in this build.
    pub fn is_empty(&self) -> bool {
        true
    }

    /// Nothing to hand out: this build cannot mount through WinFsp (the
    /// arm degrades to WebDAV), so the set is empty by construction.
    pub fn drain(&mut self) -> Vec<(String, ())> {
        Vec::new()
    }
}

/// What one boot's mount pass produced: the per-volume report (banner,
/// `mounted_letters`) plus the live in-process mounts the stop sequence
/// must release before the volumes' VFS workers go down.
#[derive(Default)]
pub struct VolumeMounts {
    /// The volumes that mounted, in claim order.
    pub mounted: Vec<MountedVolume>,
    /// The in-process WinFsp mounts still live (empty otherwise).
    pub winfsp: WinFspHandles,
}

/// Everything the per-volume mount dispatch reads (K27's claims plus K40's
/// decision inputs), bundled so the signature stays readable.
struct MountPlan<'a> {
    /// The process-level config (the `mount_backend` policy and the
    /// `auto_mount_drive` gate).
    process_cfg: &'a CyDriveConfig,
    /// The `(volume, letter)` claims: volumes that explicitly set a
    /// `drive_letter` (K27).
    claims: &'a [(String, String)],
    /// The registry, for each claim's own VFS (the winfsp arm mounts it).
    ///
    /// Only the winfsp arm reads this pair: without the feature that arm
    /// is a cfg'd no-op, so the fields are legitimately unread there — the
    /// `allow` keeps a `-D warnings` build honest about the *other* legs.
    #[cfg_attr(not(all(windows, feature = "winfsp")), allow(dead_code))]
    registry: &'a RegistryHandle,
    /// The process runtime handle the adapters bridge their async core on.
    #[cfg_attr(not(all(windows, feature = "winfsp")), allow(dead_code))]
    rt: &'a tokio::runtime::Handle,
    /// Whether the single WebDAV listener bound — the webdav arm's target
    /// and the fallback's only possible landing spot.
    webdav_available: bool,
    /// This build's + machine's WinFsp capability.
    winfsp: WinFspCapability,
}

/// The per-volume mount pass (K27 / MV2 + K40): `webdav` maps every claim
/// through `net use`, `winfsp` mounts every claim in-process, and every
/// way the second can fail lands on the first with the reason printed —
/// a failed mount only warns, exactly like the pre-WF4 behaviour.
async fn mount_volumes_if_configured(plan: &MountPlan<'_>) -> VolumeMounts {
    #[cfg(unix)]
    {
        let _ = plan;
        tracing::info!(
            "multi-volume mode mounts no directories on unix (per-volume drive-letter \
             mounts are the K27 scope); mount manually: `cydrive mount --path <dir>`"
        );
        VolumeMounts::default()
    }

    #[cfg(not(unix))]
    {
        if plan.claims.is_empty() {
            // Review L2 (RB4): the claims list is pre-filtered upstream —
            // with the webdav backend and a degraded listener it is
            // emptied wholesale (`run_multi_volume`'s claims match), so
            // "no volume claimed" would be a lie there. Name the actual
            // reason instead; with the winfsp backend (or a bound
            // listener) an empty list really is no claims.
            if plan.process_cfg.mount_backend == MountBackend::Winfsp {
                tracing::info!("no volume claimed a drive_letter; skipping the drive mounts");
            } else if plan.webdav_available {
                tracing::info!("no volume claimed a drive_letter; skipping the drive mappings");
            } else {
                tracing::info!(
                    "the WebDAV listener is not bound (the bind degraded), so the drive \
                     mappings have no endpoint to mount; skipping"
                );
            }
            return VolumeMounts::default();
        }
        if !plan.process_cfg.auto_mount_drive {
            tracing::info!("auto_mount_drive is off; skipping the drive mappings");
            return VolumeMounts::default();
        }
        if !cfg!(windows) {
            tracing::info!("drive mapping is Windows-only; skipping");
            return VolumeMounts::default();
        }
        match choose_mount_backend(plan.process_cfg.mount_backend, &plan.winfsp) {
            MountBackendDecision::WebDav => VolumeMounts {
                mounted: mount_claims_via_webdav(
                    plan.process_cfg,
                    plan.claims,
                    plan.webdav_available,
                    |_| MountedBackend::WebDav,
                ),
                ..VolumeMounts::default()
            },
            // No net use fallback (负责人 2026-09-24 裁决): an unavailable
            // winfsp backend means the drive letters simply do not mount —
            // the volumes keep running and stay reachable through the
            // dashboard / WebDAV endpoints, and the notice names the two
            // ways up (install WinFsp, or opt into webdav explicitly).
            MountBackendDecision::WinfspUnavailable(reason) => {
                tracing::error!(
                    backend = "winfsp",
                    %reason,
                    "the winfsp mount backend is unavailable; drive letters are not \
                     mounted (no net use fallback) — the volumes stay reachable"
                );
                println!("{}", winfsp_unavailable_notice(&reason));
                VolumeMounts::default()
            }
            MountBackendDecision::WinFsp => mount_claims_via_winfsp(plan).await,
        }
    }
}

/// The WebDAV arm: every claim is mapped with `net use` onto the volume's
/// `/vol/<name>` endpoint (K27). A failed mount only warns — the volume
/// stays reachable at its URL. `backend` labels what the caller will
/// record (the plain backend, or the degradation that landed here).
#[cfg(not(unix))]
fn mount_claims_via_webdav(
    process_cfg: &CyDriveConfig,
    claims: &[(String, String)],
    webdav_available: bool,
    backend: impl Fn(&str) -> MountedBackend,
) -> Vec<MountedVolume> {
    if !webdav_available {
        tracing::warn!(
            "the WebDAV listener is not bound; the drive mappings have no endpoint to \
             mount (the volumes keep running)"
        );
        return Vec::new();
    }
    let mut mounted = Vec::new();
    for (name, letter) in claims {
        let url = volume_mount_url(process_cfg, name);
        println!("Mounting volume {name} as drive {letter} -> {url} ...");
        match cloudkit_platform::windows::mount_drive(letter, &url) {
            Ok(actual) => {
                println!("Drive mounted: {actual} -> {url} (volume {name})");
                mounted.push(MountedVolume {
                    volume: name.clone(),
                    letter: actual,
                    backend: backend(name),
                });
            }
            Err(error) => {
                println!(
                    "Auto-mount FAILED for volume {name} ({error}); the volume stays \
                     reachable at {url}."
                );
                println!(
                    "  Hints: run `cydrive fix-reg` in an elevated shell, ensure the \
                     WebClient                  service can start, and check that the letter \
                     is free (`cydrive doctor`)."
                );
            }
        }
    }
    mounted
}

/// The WinFsp arm: every claim is mounted in-process through the native
/// adapter. Each mount runs on the blocking pool (the bring-up and the
/// readiness poll are synchronous WinFsp calls), and every failure
/// degrades per volume per K87: log, print, no drive letter — the
/// volume stays reachable through WebDAV/仪表盘 without any net use
/// fallback (an occupied letter is reported and skipped, never taken
/// over).
#[cfg(all(windows, feature = "winfsp"))]
async fn mount_claims_via_winfsp(plan: &MountPlan<'_>) -> VolumeMounts {
    let mut mounted = Vec::new();
    let mut handles = WinFspHandles::default();
    for (name, letter) in plan.claims {
        let Some(vfs) = plan
            .registry
            .volume(name)
            .and_then(|runtime| runtime.vfs().cloned())
        else {
            tracing::error!(
                volume = %name,
                "the volume has no running VFS; skipping its winfsp mount"
            );
            println!("WinFsp mount skipped for volume {name}: the volume is not running.");
            continue;
        };
        let rt = plan.rt.clone();
        let label = name.clone();
        let requested = letter.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            cloudkit_winfsp::mount::mount(vfs, rt, &requested, &label)
        })
        .await;
        match outcome {
            Ok(Ok(landed)) => {
                let letter = landed.handle.mount_point().to_string();
                if let Err(error) = &landed.readiness {
                    // The mount is up but the letter did not answer: kept
                    // (see the adapter's rationale), reported here.
                    tracing::error!(
                        volume = %name,
                        %error,
                        "the winfsp mount point did not appear; the mount is kept"
                    );
                    println!(
                        "Warning: volume {name} is mounted through winfsp at {letter}, but \
                         the drive did not appear yet ({error})."
                    );
                }
                println!("Volume {name} mounted in-process at {letter} (winfsp).");
                handles.push(name.clone(), landed.handle);
                mounted.push(MountedVolume {
                    volume: name.clone(),
                    letter,
                    backend: MountedBackend::WinFsp,
                });
            }
            Ok(Err(error)) => {
                tracing::error!(
                    volume = %name,
                    %error,
                    "winfsp mounting failed; falling back to the WebDAV drive mapping (K40)"
                );
                println!("winfsp mounting FAILED for volume {name} ({error}).");
                // No net use fallback: it would also hide an occupied letter
                // (the webdav mapping clears the letter first, taking over
                // whatever holds it). The volume stays reachable without the
                // letter; webdav is an explicit config opt-in.
                println!(
                    "The volume stays reachable at {} without a drive letter - fix the                      cause (letter in use? WinFsp runtime?) or set mount_backend =                      \"webdav\" in config.toml to opt into the net use mapping.",
                    volume_mount_url(plan.process_cfg, name)
                );
            }
            Err(error) => {
                tracing::error!(
                    volume = %name,
                    %error,
                    "the winfsp mount task failed to complete; no net use fallback - the \
                     volume stays reachable without a drive letter"
                );
                println!("winfsp mount task FAILED for volume {name} ({error}).");
                // A join failure means the mount task itself panicked: no
                // typed verdict about the letter exists, so a read-only
                // occupancy probe decides the WARNING's wording - a letter
                // that currently answers on anything (foreign mapping,
                // someone else's volume, an orphan of the panicking mount)
                // is called out by name and never touched.
                let letter_taken = cloudkit_platform::windows::used_drive_letters()
                    .iter()
                    .any(|used| used.eq_ignore_ascii_case(letter));
                if letter_taken {
                    println!(
                        "The drive letter {letter} is already in use and the failed mount \
                         task left no typed verdict about it - the letter is left alone."
                    );
                }
                println!(
                    "The volume stays reachable at {} without a drive letter - fix the \
                     cause or set mount_backend = \"webdav\" in config.toml to opt into \
                     the net use mapping.",
                    volume_mount_url(plan.process_cfg, name)
                );
            }
        }
    }
    VolumeMounts {
        mounted,
        winfsp: handles,
    }
}

/// The WinFsp arm of a build without the feature: `choose_mount_backend`
/// cannot answer [`MountBackendDecision::WinFsp`] here (the capability is
/// always `NotCompiled`), so this arm is unreachable — kept as a loud
/// no-op rather than an `unreachable!()` so a future refactor that breaks
/// that invariant cannot mount nothing in silence.
#[cfg(not(all(windows, feature = "winfsp")))]
async fn mount_claims_via_winfsp(_plan: &MountPlan<'_>) -> VolumeMounts {
    tracing::error!(
        "the winfsp mount backend is not compiled into this binary: {}",
        WINFSP_FEATURE_REQUIRED
    );
    VolumeMounts::default()
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
        // WF4 scope ruling: the in-process backend is wired into the
        // multi-volume flow (one mount per K27 claim). A single-volume
        // config asking for it gets the note and the WebDAV mapping —
        // visible degradation, never a silent ignore.
        if let Some(note) = single_volume_winfsp_note(cfg) {
            tracing::warn!(backend = "winfsp", "{note}");
            println!("{note}");
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

// --------------------------- web first-run bootstrap (FR1, D1–D4) ---

/// The first-run process `config.toml` (web first-run plan FR1 / D2):
/// hand-written like the setup skeleton ([`crate::setup`]'s
/// `MULTI_PROCESS_TOML`) — `save_toml` would pave every defaulted
/// volume-scoped key and fail its own K19 mixing guard. Ports are the
/// program defaults (8485 WebDAV / 8486 web UI, 负责人 2026-09-23
/// 裁决); `auto_mount_drive` is process-scoped so the guard accepts it.
/// `mount_backend` is deliberately ABSENT: the global default is winfsp
/// (负责人 2026-09-24 裁决) and an absent key reads as that default.
/// The file text is pinned byte for byte by
/// `tests/multivolume_config.rs::first_run_template_pin_keys_ports_and_header`.
const FIRST_RUN_PROCESS_TOML: &str = "\
# cydrive process config — generated on first run: no config.toml or
# config.json was found in this directory, so this minimal configuration
# was written and the instance started so a first volume can be added
# through the web dashboard. Process-level keys only — each volume's own
# settings live in volumes/<name>.toml.
volumes_dir = \"volumes\"

webdav_host = \"127.0.0.1\"
webdav_port = 8485
enable_web_ui = true
web_ui_host = \"127.0.0.1\"
web_ui_port = 8486
auto_mount_drive = true
";

/// The `run` bootstrap probe (web first-run plan FR1 / D1+D2): in a
/// working directory with NEITHER `config.toml` NOR a legacy
/// `config.json`, generate the minimal init configuration — the
/// [`FIRST_RUN_PROCESS_TOML`] template through the one atomic-write
/// primitive ([`cloudkit_core::config::write_config_atomically`], K58-H3)
/// plus the empty `volumes/` directory it points at — and return `true`
/// so the caller enters first-run mode. An existing configuration of
/// EITHER shape disables the bootstrap entirely (`Ok(false)`): the probe
/// runs before anything is written, so there is no overwrite path —
/// a configured boot is byte-for-byte untouched.
///
/// Errors are the write/mkdir failures only; a *parse* failure of a
/// pre-existing config is the ordinary discovery's business.
pub fn bootstrap_first_run_cwd() -> Result<bool> {
    if Path::new("config.toml").exists() || Path::new("config.json").exists() {
        return Ok(false);
    }
    cloudkit_core::config::write_config_atomically(
        Path::new("config.toml"),
        FIRST_RUN_PROCESS_TOML,
    )
    .context("writing the first-run config.toml")?;
    std::fs::create_dir_all("volumes").context("creating the volumes directory")?;
    tracing::info!(
        "first run: no configuration found — generated a minimal config.toml; \
         add your first volume through the web dashboard"
    );
    Ok(true)
}

/// The first-run discovery (web first-run plan FR1 / D3): load the
/// config.toml [`bootstrap_first_run_cwd`] just wrote through the SAME
/// funnel the ordinary multi-volume discovery uses — parse with raw-key
/// capture, the K19 mixing guard, validate — then return
/// [`DiscoveredConfig::Multi`] with an EMPTY volume manifest. The empty
/// manifest is the point: the ordinary discovery's volume load (core's
/// `discover_volumes`) refuses an empty volumes directory (gate A), and
/// the first-run boot must boot INTO the empty state the web UI exists
/// to fill. This variant changes nothing in core and nothing in the
/// ordinary functions beside it — it is only ever called on the file
/// the bootstrap just generated (which always carries `volumes_dir`).
pub fn discover_first_run_config() -> Result<DiscoveredConfig> {
    let toml_path = Path::new("config.toml");
    let (cfg, raw_keys) = CyDriveConfig::load_toml_with_keys(toml_path)
        .with_context(|| format!("loading {}", toml_path.display()))?;
    cloudkit_core::config::ensure_no_volume_keys_in_process(&raw_keys)
        .context("the generated config.toml mixes process- and volume-scoped keys")?;
    cfg.validate()
        .context("invalid process-level configuration")?;
    Ok(DiscoveredConfig::Multi {
        process: cfg,
        volumes: Vec::new(),
    })
}

/// Opens the system browser at `url` (web first-run plan FR1 / D4) —
/// fire-and-forget: the child is spawned detached from the boot flow and
/// never waited on, so a spawn failure is only a `tracing::warn` (a
/// headless machine must not fail its boot over a missing browser; the
/// init banner already printed the URL). The function body carries NO
/// behavioral test anywhere — asserting a spawn side effect would need a
/// real browser on every test machine; both cfg arms are instead kept
/// compilable on the other platform by the workspace gates (the R5
/// cross-platform red line), and the production caller is suppressed in
/// tests via `CYDRIVE_NO_OPEN_BROWSER`.
fn open_browser(url: &str) {
    #[cfg(unix)]
    {
        match std::process::Command::new("xdg-open").arg(url).spawn() {
            Ok(_child) => tracing::info!(%url, "opening the dashboard in the browser"),
            Err(error) => {
                tracing::warn!(%url, %error, "could not open a browser (xdg-open)")
            }
        }
    }

    #[cfg(not(unix))]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the `start` shim must not flash a console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // `start`'s first quoted argument is the new window's TITLE —
        // the empty string keeps the URL itself from being parsed as one.
        let result = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        match result {
            Ok(_child) => tracing::info!(%url, "opening the dashboard in the browser"),
            Err(error) => tracing::warn!(%url, %error, "could not open a browser (start)"),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// K58-M5: the UPDATE re-read parse (the overlay base) routes its
    /// error through the core redaction funnel — it was the one toml
    /// parse surface that bypassed it, bare-concatenating the raw error
    /// into the ERR reply. The refusal keeps the path and the key name,
    /// never the value; the happy arm returns the parsed table.
    #[test]
    fn parse_explicit_table_redacted_masks_credential_values() {
        let broken =
            "backend = \"telegram\"\nproxy_url = \"socks5://user:secretpass@127.0.0.1:7897\n";
        let error = parse_explicit_table_redacted(Path::new("volumes/media.toml"), broken)
            .expect_err("the broken-quote line must fail to parse");
        assert!(
            error.starts_with("re-reading the volume file volumes/media.toml failed"),
            "the refusal keeps the path wording: {error}"
        );
        assert!(
            !error.contains("secretpass"),
            "the URL credential value is masked: {error}"
        );
        assert!(
            error.contains("proxy_url"),
            "the key name stays for diagnosis: {error}"
        );

        let table =
            parse_explicit_table_redacted(Path::new("volumes/media.toml"), "backend = \"local\"\n")
                .expect("valid text parses");
        assert_eq!(
            table.get("backend").and_then(toml::Value::as_str),
            Some("local"),
            "the happy arm returns the explicit table"
        );
    }

    #[test]
    fn compiled_drivers_lists_the_feature_set_in_fixed_order() {
        // K32: the `--version` banner's `(drivers: ...)` segment. Every
        // arm is a cfg-gated literal — no runtime feature probing — so
        // exactly one assertion is compiled per build and it pins the
        // expected list for that feature combination: fixed order
        // telegram, baidu, local, sftp, pan115, pan123, webdav; the
        // all-off build reports `none`.
        //
        // SF3 structure: the sftp row (appended after local, never
        // interleaved) is factored out of the literal pins — the
        // 3-driver expectations below stay byte-identical, and the two
        // sftp assertions pin the suffix rule and the 4-driver list.
        // 115-1 mirror: the pan115 row strips the same way before the
        // sftp strip. 123-1 mirror: the pan123 row strips first of all —
        // every pre-existing assertion is untouched. WD1b mirror: the
        // webdav row strips first of all — the pan123 strip rewires to
        // the webdav-stripped view and the pre-existing assertion values
        // stay untouched (their cfg guards gain the mechanical
        // `not(webdav)` the pan123 landing added to pan115's).
        let drivers = compiled_drivers();
        let without_webdav: String = if cfg!(feature = "webdav") {
            match drivers.strip_suffix(", webdav") {
                Some(base) => base.to_string(),
                None => "none".to_string(), // webdav is the only driver enabled
            }
        } else {
            drivers.clone()
        };
        let without_pan123: String = if cfg!(feature = "pan123") {
            match without_webdav.strip_suffix(", pan123") {
                Some(base) => base.to_string(),
                None => "none".to_string(), // pan123 is the only driver enabled
            }
        } else {
            without_webdav.clone()
        };
        let without_pan115: String = if cfg!(feature = "pan115") {
            match without_pan123.strip_suffix(", pan115") {
                Some(base) => base.to_string(),
                None => "none".to_string(), // pan115 is the only driver enabled
            }
        } else {
            without_pan123.clone()
        };
        let base: String = if cfg!(feature = "sftp") {
            match without_pan115.strip_suffix(", sftp") {
                Some(base) => base.to_string(),
                None => "none".to_string(), // sftp is the only driver enabled
            }
        } else {
            without_pan115.clone()
        };

        #[cfg(all(feature = "telegram", feature = "baidu", feature = "local"))]
        assert_eq!(base, "telegram, baidu, local");
        #[cfg(all(feature = "telegram", feature = "baidu", not(feature = "local")))]
        assert_eq!(base, "telegram, baidu");
        #[cfg(all(feature = "telegram", not(feature = "baidu"), feature = "local"))]
        assert_eq!(base, "telegram, local");
        #[cfg(all(not(feature = "telegram"), feature = "baidu", feature = "local"))]
        assert_eq!(base, "baidu, local");
        #[cfg(all(feature = "telegram", not(feature = "baidu"), not(feature = "local")))]
        assert_eq!(base, "telegram");
        #[cfg(all(not(feature = "telegram"), feature = "baidu", not(feature = "local")))]
        assert_eq!(base, "baidu");
        #[cfg(all(not(feature = "telegram"), not(feature = "baidu"), feature = "local"))]
        assert_eq!(base, "local");
        #[cfg(not(any(feature = "telegram", feature = "baidu", feature = "local")))]
        assert_eq!(base, "none");

        // SF3: the sftp row is a pure append in the fixed order — the
        // list ends with it whenever the feature is on, the all-four
        // build reads in the documented order, and the sftp-only build
        // reports just the row.
        #[cfg(feature = "sftp")]
        assert!(
            without_pan115.ends_with("sftp"),
            "sftp must precede pan115/pan123 when several are on: {drivers}"
        );
        #[cfg(all(
            feature = "telegram",
            feature = "baidu",
            feature = "local",
            feature = "sftp",
            not(feature = "pan115"),
            not(feature = "pan123"),
            not(feature = "webdav")
        ))]
        assert_eq!(drivers, "telegram, baidu, local, sftp");
        #[cfg(all(
            not(feature = "telegram"),
            not(feature = "baidu"),
            not(feature = "local"),
            feature = "sftp",
            not(feature = "pan115"),
            not(feature = "pan123"),
            not(feature = "webdav")
        ))]
        assert_eq!(drivers, "sftp");

        // 115-1: the pan115 row is the same pure-append rule — last
        // among the pre-pan123 rows whenever on, the all-five build
        // reads in the documented order, and the pan115-only build
        // reports just the row.
        #[cfg(feature = "pan115")]
        assert!(
            without_pan123.ends_with("pan115"),
            "pan115 must precede pan123 when both are on: {drivers}"
        );
        #[cfg(all(
            feature = "telegram",
            feature = "baidu",
            feature = "local",
            feature = "sftp",
            feature = "pan115",
            not(feature = "pan123"),
            not(feature = "webdav")
        ))]
        assert_eq!(drivers, "telegram, baidu, local, sftp, pan115");
        #[cfg(all(
            not(feature = "telegram"),
            not(feature = "baidu"),
            not(feature = "local"),
            not(feature = "sftp"),
            feature = "pan115",
            not(feature = "pan123"),
            not(feature = "webdav")
        ))]
        assert_eq!(drivers, "pan115");

        // 123-1: the pan123 row is the same pure-append rule — last
        // whenever on (under a webdav row, if that is on too), the
        // all-six build reads in the documented order, and the
        // pan123-only build reports just the row.
        #[cfg(feature = "pan123")]
        assert!(
            without_webdav.ends_with("pan123"),
            "pan123 must be the last row under webdav: {drivers}"
        );
        #[cfg(all(
            feature = "telegram",
            feature = "baidu",
            feature = "local",
            feature = "sftp",
            feature = "pan115",
            feature = "pan123",
            not(feature = "webdav")
        ))]
        assert_eq!(drivers, "telegram, baidu, local, sftp, pan115, pan123");
        #[cfg(all(
            not(feature = "telegram"),
            not(feature = "baidu"),
            not(feature = "local"),
            not(feature = "sftp"),
            not(feature = "pan115"),
            feature = "pan123",
            not(feature = "webdav")
        ))]
        assert_eq!(drivers, "pan123");

        // WD1b: the webdav row is the same pure-append rule — last
        // whenever on, the all-seven build reads in the documented
        // order, and the webdav-only build reports just the row.
        #[cfg(feature = "webdav")]
        assert!(
            drivers.ends_with("webdav"),
            "webdav must be the last row: {drivers}"
        );
        #[cfg(all(
            feature = "telegram",
            feature = "baidu",
            feature = "local",
            feature = "sftp",
            feature = "pan115",
            feature = "pan123",
            feature = "webdav"
        ))]
        assert_eq!(
            drivers,
            "telegram, baidu, local, sftp, pan115, pan123, webdav"
        );
        #[cfg(all(
            not(feature = "telegram"),
            not(feature = "baidu"),
            not(feature = "local"),
            not(feature = "sftp"),
            not(feature = "pan115"),
            not(feature = "pan123"),
            feature = "webdav"
        ))]
        assert_eq!(drivers, "webdav");
    }

    /// H1 (review fix): `take` used to find the position under one lock
    /// and remove it under a second — a concurrent `take_all` landing in
    /// that seam emptied the vec and the `remove(position)` panicked out
    /// of bounds (killing the control loop task). This pins the table's
    /// concurrency invariant instead of racing for the narrow seam:
    /// hammered from three threads, the table never panics and every
    /// inserted entry is accounted for — each is either taken by name
    /// or drained by `take_all`; none is lost to the seam.
    #[tokio::test]
    async fn live_table_concurrent_take_take_all_insert_is_lossless() {
        // One shared VFS backs every hammer entry's stop unit (the table
        // structure is under test, not the stop semantics); each entry
        // carries its identity in the mount record's letter.
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open db"));
        let cache = CacheManager::new(dir.path().join("cache"), u64::MAX);
        let transport: Arc<dyn CloudTransport> =
            Arc::new(cloudkit_core::transport::mock::MockTransport::new());
        let vfs = Arc::new(Vfs::new(
            db,
            cache,
            Arc::clone(&transport),
            Default::default(),
        ));

        let table = Arc::new(VolumeLiveTable::default());
        let inserted: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let taken: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let drained: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        const ROUNDS: usize = 2_000;

        // Pre-build the hammer entries in the async context (each stop
        // unit's inbound handle needs a reactor to spawn); the threads
        // below only move them through the table.
        let mut to_insert: Vec<(String, LiveVolume)> = (0..ROUNDS)
            .map(|k| {
                let letter = format!("i{k}");
                let entry = LiveVolume {
                    stop_unit: VolumeStopUnit {
                        vfs: Arc::clone(&vfs),
                        inbound: spawn_inbound_worker(
                            Arc::clone(&vfs),
                            Arc::clone(&transport),
                            "X:".to_string(),
                        ),
                    },
                    sync_task: None,
                    mount: Some(MountedVolume {
                        volume: "v".to_string(),
                        letter: letter.clone(),
                        backend: MountedBackend::WebDav,
                    }),
                    release: None,
                };
                (letter, entry)
            })
            .collect();

        // The methods are synchronous (a std mutex inside), so plain
        // threads hammer them concurrently.
        std::thread::scope(|scope| {
            let inserter_table = Arc::clone(&table);
            let inserted = Arc::clone(&inserted);
            scope.spawn(move || {
                while let Some((letter, entry)) = to_insert.pop() {
                    inserter_table.insert("v", entry);
                    inserted.lock().expect("inserted log").push(letter);
                }
            });
            let taker_table = Arc::clone(&table);
            let taken = Arc::clone(&taken);
            scope.spawn(move || {
                for _ in 0..ROUNDS {
                    if let Some(entry) = taker_table.take("v") {
                        let letter = entry.mount.expect("every entry carries its id").letter;
                        taken.lock().expect("taken log").push(letter);
                    }
                }
            });
            let drainer_table = Arc::clone(&table);
            let drained = Arc::clone(&drained);
            scope.spawn(move || {
                for _ in 0..ROUNDS {
                    for (_, entry) in drainer_table.take_all() {
                        let letter = entry.mount.expect("every entry carries its id").letter;
                        drained.lock().expect("drained log").push(letter);
                    }
                }
            });
        });

        let mut expected = inserted.lock().expect("inserted log").clone();
        expected.sort();
        let mut accounted: Vec<String> = taken
            .lock()
            .expect("taken log")
            .drain(..)
            .chain(drained.lock().expect("drained log").drain(..))
            .collect();
        accounted.sort();
        // Entries move out of the table exactly once, so the multiset
        // equality is losslessness: every inserted id was handed to
        // exactly one consumer (a take or the take_all drain).
        assert_eq!(
            accounted, expected,
            "every inserted entry is either taken by name or drained by take_all — none lost"
        );
    }

    /// K58-M7: `add_volume` reports its outcome STRUCTURALLY — the
    /// reply stays the operator text, the bool is the assembly+mount
    /// verdict. The pre-fix CREATE/UPDATE classification re-parsed the
    /// reply's `OK:` prefix, so any wording drift would misreport a
    /// real assembly as a failure (with the wrong recovery guidance).
    /// Characterization-style unit test — the review's sanctioned
    /// fallback form (the reply wording is not injectable, so the
    /// drift-immunity itself cannot be pinned by rewriting it in a
    /// test): the three enumerated endings each pin the bool directly
    /// against a minimal surface.
    #[tokio::test]
    async fn add_volume_reports_structured_success_for_its_three_endings() {
        // The minimal surface under test: empty faces, fast tuning, no
        // webdav listener, and the dispatch/mount seams injectable per
        // ending. The volumes_dir is ABSOLUTE (no chdir dance).
        fn unit_surface(
            cfg: CyDriveConfig,
            dispatch: VolumeTransportDispatch,
            mount: Option<RuntimeMount>,
        ) -> RuntimeVolumeControl {
            RuntimeVolumeControl {
                process_cfg: cfg,
                registry: RegistryHandle::new(Vec::new()),
                webdav: cloudkit_webdav::RegistryHandle::new(Vec::new()),
                web: cloudkit_web::RegistryHandle::new(Vec::new()),
                live: Arc::new(VolumeLiveTable::default()),
                watch: Arc::new(ShutdownWatch::new()),
                webdav_available: false,
                dispatch: Some(dispatch),
                mount,
                tuning: RemoveTuning::fast(),
                rebuilds: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                rebuild_tuning: RebuildTuning::fast(),
                rebuild: Arc::new(|_name: &str, _settings: &CyDriveConfig| {
                    Box::pin(async {
                        Ok(rebuild::RebuildOutcome {
                            files: 0,
                            dirs: 0,
                            ..Default::default()
                        })
                    })
                }),
            }
        }
        fn unit_volume_toml() -> String {
            "backend = \"telegram\"\nbot_token = \"1:UNIT\"\nchat_id = 1\n".to_string()
        }
        let mock_dispatch: VolumeTransportDispatch = Arc::new(move |_spec: &VolumeConfig| {
            Box::pin(async {
                let mock = Arc::new(cloudkit_core::transport::mock::MockTransport::new());
                mock.connect().await.expect("unit mock connects");
                Ok(Some((
                    RunOptions::default(),
                    mock as Arc<dyn CloudTransport>,
                )))
            })
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let volumes = dir.path().join("volumes");
        std::fs::create_dir_all(&volumes).expect("create volumes dir");
        let plain_cfg = CyDriveConfig {
            volumes_dir: Some(volumes.to_string_lossy().into_owned()),
            ..CyDriveConfig::default()
        };

        // Ending 1 — success: the file assembles and publishes (no
        // drive letter claimed — the no-mount publication arm).
        std::fs::write(volumes.join("ok.toml"), unit_volume_toml()).expect("write ok.toml");
        let (reply, added) = unit_surface(plain_cfg.clone(), mock_dispatch.clone(), None)
            .add_volume("ok")
            .await;
        assert!(added, "a clean assembly reports success: {reply}");
        assert!(
            reply.starts_with("OK: added volume `ok`"),
            "the success reply keeps its operator text: {reply}"
        );

        // Ending 2 — dispatch (connect/credential) failure: nothing
        // changes, the verdict is failure.
        std::fs::write(volumes.join("bad.toml"), unit_volume_toml()).expect("write bad.toml");
        let failing_dispatch: VolumeTransportDispatch = Arc::new(move |_spec: &VolumeConfig| {
            Box::pin(async { anyhow::bail!("connect refused (unit test)") })
        });
        let (reply, added) = unit_surface(plain_cfg.clone(), failing_dispatch, None)
            .add_volume("bad")
            .await;
        assert!(!added, "a dispatch failure reports failure: {reply}");
        assert!(
            reply.starts_with("ERR:") && reply.contains("connecting volume `bad` failed"),
            "the refusal keeps its actionable text: {reply}"
        );

        // Ending 3 — mount-failure rollback: the assembled volume is
        // torn down unpublished, the verdict is failure.
        std::fs::write(
            volumes.join("mnt.toml"),
            format!("{}drive_letter = \"Q\"\n", unit_volume_toml()),
        )
        .expect("write mnt.toml");
        let empty_mount_stub: RuntimeMount = Arc::new(move |_name: &str, _letter: &str| {
            Box::pin(async {
                VolumeMounts {
                    mounted: Vec::new(),
                    winfsp: Default::default(),
                }
            })
        });
        let mount_cfg = CyDriveConfig {
            auto_mount_drive: true,
            ..plain_cfg.clone()
        };
        let (reply, added) = unit_surface(mount_cfg, mock_dispatch, Some(empty_mount_stub))
            .add_volume("mnt")
            .await;
        assert!(!added, "a mount-failure rollback reports failure: {reply}");
        assert!(
            reply.starts_with("ERR:") && reply.contains("rolled back"),
            "the rollback refusal keeps its actionable text: {reply}"
        );
    }

    /// M1-3 (review fix): the net-use release's error path through the
    /// spawn_blocking + budget wrapper — deleting a mapping that does
    /// not exist must come back Err (the verbatim net-use failure, with
    /// the command named), the exact input the K50 abort consumes; the
    /// wrapper must neither swallow nor alter it. The budget branch (a
    /// hung provider) is untestable offline — see
    /// NET_USE_RELEASE_BUDGET's comment.
    #[tokio::test]
    async fn webdav_release_propagates_the_net_use_failure() {
        let used = cloudkit_platform::windows::used_drive_letters();
        let letter = (b'A'..=b'Z')
            .map(|byte| format!("{}:", char::from(byte)))
            .find(|letter| {
                used.iter()
                    .all(|mounted| !mounted.eq_ignore_ascii_case(letter))
            })
            .expect("at least one unmapped drive letter exists");
        let mut release = WebDavRelease { letter };
        let error = release
            .release()
            .await
            .expect_err("deleting a mapping that does not exist must fail");
        assert!(
            error.contains("net use"),
            "the failure names the command it ran: {error}"
        );
        assert!(
            error.contains("failed"),
            "the failure reads as a failure: {error}"
        );
    }

    /// K58-FD/M4a: the checkpoint's identity observable — the registry
    /// entry's Vfs allocation address. The unit pins the extraction's
    /// three semantics directly (the integration test
    /// `a_reassembled_volume_is_a_new_generation_...` pins the walk's
    /// abort): the identity is STABLE while the same instance stays
    /// registered, a REMOVE→re-ADD swap (UPDATE's internals, ENABLE's
    /// path — every registration goes through a fresh assembly) reads
    /// a DIFFERENT identity, and a Failed entry carries none.
    #[tokio::test]
    async fn volume_identity_is_stable_per_instance_and_changes_across_reassembly() {
        fn unit_spec(name: &str, dir: &Path) -> VolumeConfig {
            VolumeConfig {
                name: name.to_string(),
                file_path: dir.join(format!("{name}.toml")),
                base_dir: dir.to_path_buf(),
                settings: CyDriveConfig::default(),
                explicit_drive_letter: false,
            }
        }
        // Two independent assemblies (fresh db + cache + Vfs each —
        // `assemble_volume`'s shape in miniature); the tempdirs stay
        // alive for the entries' lifetime.
        let assemble = |stem: &str| {
            let dir = tempfile::tempdir().expect("tempdir");
            let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open db"));
            let cache = CacheManager::new(dir.path().join("cache"), u64::MAX);
            let transport: Arc<dyn CloudTransport> =
                Arc::new(cloudkit_core::transport::mock::MockTransport::new());
            let vfs = Arc::new(Vfs::new(
                db,
                cache,
                Arc::clone(&transport),
                Default::default(),
            ));
            let runtime = VolumeRuntime::Running {
                spec: unit_spec(stem, dir.path()),
                transport,
                vfs,
            };
            (runtime, dir)
        };

        let (first, _keep_first) = assemble("a");
        let accepted = volume_identity(&first).expect("a Running entry carries its Vfs identity");

        // Stable for the live instance: the registry clone reads the
        // same identity the acceptance captured.
        let registry = RegistryHandle::new(vec![first]);
        let live = registry.volume("a").expect("registered");
        assert_eq!(
            volume_identity(&live),
            Some(accepted),
            "the same registered instance keeps its identity"
        );

        // The REMOVE→re-ADD swap: the fresh assembly is a new
        // generation — the identity the checkpoint compares against
        // MUST change, or a re-assembled volume would inherit the old
        // walk.
        let (second, _keep_second) = assemble("a");
        assert!(registry.remove("a"), "the REMOVE leg drops the entry");
        registry.insert(second);
        let swapped = registry
            .volume("a")
            .expect("the re-ADD registers the new generation");
        let swapped_vfs = swapped.vfs().expect("the re-assembled entry runs");
        assert_ne!(
            Arc::as_ptr(swapped_vfs) as usize,
            accepted,
            "a re-assembled volume is a different generation (identity changed)"
        );

        // A Failed entry (a broken re-assembly) has no identity at all
        // — never the accepted one.
        let failed = VolumeRuntime::Failed {
            spec: unit_spec("a", Path::new(".")),
            reason: "assembly failed".to_string(),
        };
        assert_eq!(
            volume_identity(&failed),
            None,
            "a Failed entry carries no identity"
        );
    }
}
