//! Web dashboard for CyDrive (unit M4): the six-route contract of the
//! Python `cydrive/web_ui/app.py` aiohttp dashboard, served on axum.
//!
//! Shape parity is the contract (compat contract 1): the frontend
//! `static/js/app.js` + `templates/index.html` are embedded verbatim
//! (via `rust-embed`) and consume exactly the JSON the Python handlers
//! produced, so the Rust responses mirror those key by key:
//!
//! - `GET /` — `templates/index.html` raw. The template has no
//!   server-side variables (the Python handler reads and returns the
//!   file without any rendering), so embedding it statically is the
//!   exact behavior, not a simplification.
//! - `GET /static/*` — the copied `static/{css,js,img}` tree.
//! - `GET /api/files` — `list_all_files()` rows serialized like
//!   sqlite's `SELECT *` (all 16 columns, `is_*` flags as 0/1 ints).
//! - `GET /api/stats` — `get_stats()` plus the six handler-added
//!   dashboard fields, plus the five backend-identity keys the
//!   multi-backend adapter reports from its config (`backend`,
//!   `volume`, `remote_delete`, `quota_used`, `quota_total` — absent
//!   identity serializes as `null`; the frozen eleven keys are
//!   untouched).
//! - `POST /api/upload` — multipart field `file` staged through
//!   [`Vfs::put`] (accepted ≠ uploaded; the queue uploads async).
//! - `POST /api/delete` — `{"filename": ...}`, a single delete call
//!   through the VFS (fixing the Python double-call defect per the
//!   design doc; review High-2 routes files through
//!   [`Vfs::remove_file`]).
//! - `GET /api/download/{filename}` — stream off the remote (SR2 / K36:
//!   a plaintext row on a `range_read` transport answers from bounded
//!   `open_range` windows, never a hydrated local copy) or hydrate
//!   through the VFS for the rows that cannot (R-5), answering with
//!   Python's inline disposition; unknown files get Python's verbatim
//!   404 body.
//!
//! Three incremental routes (no Python baseline; frozen by the Rust
//! design doc, all additive — the six routes above are untouched):
//!
//! - `GET /api/list?path=/docs` — one directory's direct children as a
//!   flat `/api/files`-shaped array (the frontend assembles the tree).
//!   The query value is normalized (leading `/` guaranteed, `\` folded
//!   onto `/`); segment pollution 400s, a directory with neither a row
//!   nor children 404s, an existing-but-empty one lists 200.
//! - `GET /api/download/{filename}` with `Range` — standard HTTP
//!   single-range semantics (the aiohttp `FileResponse` baseline): a
//!   satisfiable range answers 206 with an exact slice and
//!   `Content-Range`; an unsatisfiable start answers 416 with
//!   `Content-Range: bytes */size`; malformed or multi-range headers
//!   are ignored and the full body is served with 200. Since SR2 both
//!   the streaming and the hydrate faces serve these semantics (the
//!   streaming one decides off the authoritative row size, K35, with
//!   no remote call for the 416).
//! - `GET /api/queue` — the four upload-queue counters plus the DB
//!   pending-uploads tally.
//!
//! Multi-volume mode (Phase 2.5 / MV3, K23/K24): [`WebUiServer::serve_multi`]
//! serves ONE dashboard over a volume registry instead of one VFS. Every
//! volume-scoped route above then takes an optional `?volume=<name>`
//! query parameter — no parameter answers 400 with the addressable
//! volume list (there is no default volume — the PCFS "fall back to the
//! first driver" anti-lesson), an unknown name answers 404, a
//! known-but-failed volume answers 409 with its reason, and a running
//! volume answers exactly the frozen per-route shapes above (the 16-key
//! `/api/stats` contract included, with the identity keys read from that
//! volume's own registry entry). Two additive routes exist only in this
//! mode:
//!
//! - `GET /api/volumes` — the registry listing (name / backend /
//!   volume_id / drive_letter / webdav_url / status / status_reason /
//!   quota / total_files / total_bytes / pending), the numbers read
//!   from each volume's db metadata — never a backend scan (the PCFS
//!   Stats anti-lesson); `pending` is the queue's outstanding upload
//!   count (`null` on a failed volume — no queue exists).
//! - `GET /api/stats/summary` — the cross-volume aggregate (Σ files /
//!   bytes / dirs / uploaded / pending / quota over the RUNNING
//!   volumes) for the dashboard's summary card. A distinct shape on
//!   a purpose: the frozen 16-key `/api/stats` contract stays a
//!   per-volume answer.
//! - `GET /api/volumes/{name}/config` — the named volume's FILE
//!   configuration through the injected [`VolumeCommandClient`] seam
//!   (web volume management §1.1): a `SHOW <name>` whose JSON reply
//!   passes through verbatim, credentials already collapsed to
//!   `{"set": bool}` markers by the core serializer (write-only — a
//!   credential VALUE never reaches this layer). The registry gates the
//!   lookup (a REMOVE'd volume 404s though its file stays), no seam
//!   answers 503.
//! - `GET /api/volumes/configs` (P1) — the configuration FULL set (the
//!   management page's data source: the registry listing cannot see
//!   disabled volumes): one `CONFIGS` through the seam, the row lines
//!   parsed into `{name, backend, enabled, running}` / `{name,
//!   invalid, reason}` JSON, with the P2 sparse markers `rebuilding`/
//!   `encrypted` riding as `true` when the row carries them.
//! - `POST /api/volumes/{name}/remove|disable|enable` (P1) — the
//!   management write family: one command through the same seam (the
//!   SAME serialized execution the control channel funnels into), the
//!   reply mapped to `{"ok": true, "reply": "..."}` or a 409 carrying
//!   the actionable `ERR:` text. The web-side budget is the control
//!   client's 120s (a REMOVE legitimately drains uploads first). No
//!   registry gate: a disabled volume is by definition absent from the
//!   registry — the command's own reply is the authority.
//! - `POST /api/volumes/{name}/rebuild` (P6) — the Refresh button's
//!   member of the same write family: one `REBUILD <name>` whose reply
//!   is the instance's ACCEPTANCE (the walk is the instance's
//!   background task, so the 120s budget covers the serialized queue,
//!   never the walk); the volume's `rebuilding` marker (the configs
//!   rows above) is the button's live state.
//! - `POST /api/volumes` (P3) — the Add Volume form's member: the body's
//!   `name` keys the command and the remaining object rides as the
//!   compact single-line JSON payload of one `CREATE <name> <json>`; the
//!   command is the authority on the payload (key space, validation
//!   before any write, the controlled toml, the ADD leg), the route on
//!   the body's shape (a non-JSON body or a missing `name` answers 400).
//!   The 180s assembly budget covers the ADD leg's connect + mount
//!   windows behind the serialized queue.
//!
//! The volume-management family (`/api/volumes`, the two config
//! routes, and the write routes) runs behind a same-origin guard (§1.5
//! 裁决③): an Origin/Referer naming another site 403s; headerless
//! requests (non-browser clients) pass. The write family additionally
//! carries the §1.5 non-loopback ruling: bound to a non-loopback
//! address without the process key `allow_remote_admin = true`, the
//! write routes 403 naming that key (the management plane degrades to
//! read-only; the reads and pages stay open). `GET /volumes` (both
//! mode tables — multi serves the management page, single-volume the
//! explanation page, 裁决④) stays open like every page/read route.
//!
//! Single-volume mode (the `serve` constructor) is untouched: the same
//! nine-route table, the same bodies, and a stray `?volume=` parameter
//! is simply ignored (there is exactly one volume — no registry to
//! route through).
//!
//! Production binds `127.0.0.1:8088` (the caller's concern); tests bind
//! `127.0.0.1:0`.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use bytes::Bytes;
use futures_core::Stream;
use rust_embed::RustEmbed;
use tokio::net::TcpListener;
use tokio::sync::watch;

use cloudkit_core::database::FileRecord;
use cloudkit_core::rel_path::RelPath;
use cloudkit_core::transport::{ByteStream, CloudTransport, RemoteHandle, StorageError};
use cloudkit_core::vfs::{StreamSource, Vfs, VfsError};

/// A boot-time quota snapshot for the dashboard's storage card, read
/// once at assembly by the caller (informational — never a live meter;
/// the baidu leg snapshots `StorageDriver::quota` at dispatch). The
/// web layer only reports it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSnapshot {
    /// Bytes used at snapshot time.
    pub used: u64,
    /// Bytes of the quota ceiling; `None` = unknown/unlimited.
    pub total: Option<u64>,
}

/// Configuration knobs for the dashboard: the source of the
/// `/api/stats` extra fields plus nothing else — binding goes through
/// [`WebUiServer::serve`]. Clone: a plain value the cli assembly
/// snapshots per volume (MV3 registry construction).
#[derive(Debug, Clone)]
pub struct WebUiConfig {
    /// Windows drive letter reported by `/api/stats` (`"Y:"`). `None`
    /// in multi-volume mode for a volume that did not claim a letter
    /// (an unclaimed volume mounts nothing — reporting the config
    /// default would be a phantom mount claim); single-volume mode
    /// always carries `Some`.
    pub drive_letter: Option<String>,
    /// WebDAV URL reported by `/api/stats` (`"http://127.0.0.1:8080"`).
    /// The handler glue also derives the `webdav_host` / `webdav_port`
    /// keys from this single URL (the frozen config carries no separate
    /// host/port fields).
    pub webdav_url: String,
    /// Telegram chat id reported by `/api/stats`.
    pub chat_id: i64,
    /// Whether the Telegram side is configured (drives the stats flag;
    /// Python: `bool(bot_token and bot_token != "NOT_CONFIGURED")`).
    pub is_configured: bool,
    /// The active backend's stable config spelling (`"telegram" |
    /// "baidu" | "local"` — `Backend::as_str`); the dashboard derives
    /// its labels and copy from this. Sourced from the config at
    /// assembly, so it names the backend that actually booted.
    pub backend: String,
    /// The dispatched volume identity (`baidu:<uid>` / `local:<hash>`);
    /// `None` = the backend has no volume on the CloudTransport face
    /// (telegram — the face simply has no volume, none is invented).
    pub volume: Option<String>,
    /// The K4 gate the delete UX echoes: `true` = a dashboard delete
    /// really deletes the cloud object (baidu/local); `false` = only
    /// the local row goes and the remote copy stays (telegram, Python
    /// parity). Sourced from the transport's own `capabilities()` —
    /// never hardcoded (R4 honesty).
    pub remote_delete: bool,
    /// Boot quota snapshot for the storage card (`None` = no quota
    /// concept on this backend, or the informational read failed).
    pub quota: Option<QuotaSnapshot>,
}

/// One registry entry's assembly status (Phase 2.5 / K22, the web face
/// of the cli's `VolumeStatus`): a volume either runs, or carries the
/// reason its assembly failed — never silently absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeUiStatus {
    /// The volume's db/VFS/queue are up and serve traffic.
    Running,
    /// The volume failed to assemble; `reason` is the error chain's
    /// summary, surfaced verbatim by `/api/volumes` and the 409 refusals.
    Failed {
        /// The assembly failure's top-level error message.
        reason: String,
    },
}

impl VolumeUiStatus {
    /// The stable lowercase spelling for `/api/volumes`.
    pub fn as_str(&self) -> &'static str {
        match self {
            VolumeUiStatus::Running => "running",
            VolumeUiStatus::Failed { .. } => "failed",
        }
    }
}

/// One volume's dashboard identity plus its pieces (Phase 2.5 / K24):
/// the per-volume [`WebUiConfig`] face (drive letter, the volume's own
/// `/vol/<name>` WebDAV URL, backend identity, boot quota snapshot —
/// assembled by the cli composition root, the web layer reports it
/// verbatim and invents nothing) with the registry name and status. A
/// `Failed` entry carries no VFS: its `/api/volumes` row reports the
/// reason instead of numbers, and volume-scoped routes refuse it.
#[derive(Clone)]
pub struct VolumeUiEntry {
    /// The registry name (the URL segment, mount label and dashboard tab
    /// all reuse it — K29).
    pub name: String,
    /// The volume's assembly status (K22).
    pub status: VolumeUiStatus,
    /// The volume's dashboard knobs — the same [`WebUiConfig`] face the
    /// single-volume boot assembles, per volume.
    pub config: WebUiConfig,
    /// The volume's own VFS (`None` for a failed volume).
    pub vfs: Option<Arc<Vfs>>,
}

/// The volume-command callback seam (web volume management plan §1.1):
/// the dashboard's route into the cli composition root's serialized
/// volume-command execution — the raw command line in, the verbatim
/// reply text out (`OK: ...` / `ERR: ...`), the same contract the
/// loopback control channel's handler serves. Defined here, structurally
/// identical to cli's `VolumeCommandHandler`, because web and cli are
/// both L5 (a web→cli import would be a Cargo cycle); the cli assembly
/// injects its installed handler as one more `Arc` clone, so web-sent
/// commands and control-channel commands funnel into the SAME
/// serialized queue (K48's serialization semantics inherit unchanged).
pub type VolumeCommandClient = Arc<
    dyn for<'a> Fn(&'a str) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>
        + Send
        + Sync
        + 'static,
>;

/// Errors from assembling the dashboard.
#[derive(Debug, thiserror::Error)]
pub enum WebUiError {
    /// The listener could not bind the requested address.
    #[error("bind {addr}: {source}")]
    Bind {
        /// The requested bind address.
        addr: SocketAddr,
        /// The underlying io error.
        source: std::io::Error,
    },
}

/// The dynamic volume table behind the multi-volume dashboard (Phase 3.6
/// / RV1, K51): the `Arc<RwLock<ordered volume table>>` shared handle —
/// one [`VolumeUiEntry`] per volume, in insertion order, so the caller
/// can insert/remove volumes while the dashboard keeps serving; every
/// request re-reads the table, so a removal drops the volume's tab,
/// `/api/volumes` row and `?volume=` routes at once and an insertion
/// makes them addressable at once (no listener restart, no route
/// rebuild). The cli composition root fills the table at boot and (RV2)
/// mutates it through the same handle.
///
/// Clone shares the table; every read clones entries out and releases
/// the lock before the (sync) handler bodies run, so the lock never
/// spans an await. Poison recovery follows the code-style norm — a
/// panicked holder must not take the dashboard down.
#[derive(Clone)]
pub struct RegistryHandle {
    volumes: Arc<std::sync::RwLock<Vec<VolumeUiEntry>>>,
}

impl RegistryHandle {
    /// Builds the table with the given starting volumes (boot order).
    pub fn new(volumes: Vec<VolumeUiEntry>) -> Self {
        Self {
            volumes: Arc::new(std::sync::RwLock::new(volumes)),
        }
    }

    /// Registers a volume (appending to the table order); a duplicate
    /// name is the caller's contract error (RV2's ADD refuses it before
    /// reaching here).
    pub fn insert(&self, entry: VolumeUiEntry) {
        self.write().push(entry);
    }

    /// Removes the volume named `name`; `true` when it was registered.
    pub fn remove(&self, name: &str) -> bool {
        let mut volumes = self.write();
        match volumes.iter().position(|entry| entry.name == name) {
            Some(position) => {
                volumes.remove(position);
                true
            }
            None => false,
        }
    }

    /// The registered names, in table order (the K23 error payloads'
    /// addressable-volume list).
    pub fn names(&self) -> Vec<String> {
        self.read().iter().map(|entry| entry.name.clone()).collect()
    }

    /// One volume's entry by name (a clone — the lock is released
    /// before the handler body runs).
    pub fn find(&self, name: &str) -> Option<VolumeUiEntry> {
        self.read().iter().find(|entry| entry.name == name).cloned()
    }

    /// The whole table, in order (a snapshot — same release-then-run
    /// contract as [`RegistryHandle::find`]).
    pub fn snapshot(&self) -> Vec<VolumeUiEntry> {
        self.read().clone()
    }

    /// The table read lock, poisoned-lock recovery per the code-style
    /// norm (a panicked holder must not take the dashboard down).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<VolumeUiEntry>> {
        self.volumes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The table write lock (same recovery norm).
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<VolumeUiEntry>> {
        self.volumes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The embedded `static/` tree (copied verbatim from the Python
/// dashboard; the multi-backend adapter edits only its copy — the
/// served bytes are whatever this folder holds at compile time).
#[derive(RustEmbed)]
#[folder = "static/"]
struct StaticAssets;

/// The embedded `templates/` tree (`index.html`).
#[derive(RustEmbed)]
#[folder = "templates/"]
struct Templates;

/// Upload body cap: the Telegram chunk threshold (1900 MB) is the
/// largest single-message payload, so the dashboard must accept at
/// least that — axum's 2 MB `DefaultBodyLimit` default would 413 every
/// real upload.
const MAX_UPLOAD_BYTES: usize = 1900 * 1024 * 1024;

/// Python's verbatim 404 body for unknown downloads.
const DOWNLOAD_NOT_FOUND: &str = "File not found in CyDrive cloud";

/// The running dashboard: an axum listener dispatching the six contract
/// routes (plus `/static/*`) over a [`Vfs`].
///
/// Clone-free handle; `shutdown` is idempotent and drains gracefully
/// (the listener stops accepting and in-flight requests finish).
pub struct WebUiServer {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl WebUiServer {
    /// Binds `addr` and serves the dashboard over `vfs` — the frozen
    /// single-volume constructor (nine routes; a stray `?volume=` query
    /// parameter is ignored).
    pub async fn serve(
        vfs: Arc<Vfs>,
        cfg: WebUiConfig,
        addr: SocketAddr,
    ) -> Result<Self, WebUiError> {
        let app = router(vfs, cfg);
        serve_router(app, addr).await
    }

    /// Binds `addr` and serves the multi-volume dashboard (Phase 2.5 /
    /// MV3; dynamic since RV1 / K51) over the shared volume table: the
    /// same nine routes (now taking the K23 `?volume=<name>` parameter)
    /// plus the two registry routes `/api/volumes` and
    /// `/api/stats/summary` — every request re-reads the table, so
    /// later insertions/removals are visible at once. Without the
    /// command seam (the volume-management routes answer 503).
    pub async fn serve_multi(
        volumes: RegistryHandle,
        addr: SocketAddr,
    ) -> Result<Self, WebUiError> {
        Self::serve_multi_with_commands(volumes, addr, None).await
    }

    /// [`WebUiServer::serve_multi`] with the volume-command seam
    /// installed (web volume management plan §1.1): the dashboard's
    /// management routes (`GET /api/volumes/{name}/config`, the P1+
    /// write family) send their commands through `commands` — the SAME
    /// handler the cli composition root installed on the control
    /// channel, so both trigger sources serialize behind one queue.
    /// `None` keeps the read-only dashboard (the seam routes answer
    /// their actionable 503). Equivalent to
    /// [`WebUiServer::serve_multi_with_remote_admin`] without the
    /// opt-in: a non-loopback bind keeps the write family withheld.
    pub async fn serve_multi_with_commands(
        volumes: RegistryHandle,
        addr: SocketAddr,
        commands: Option<VolumeCommandClient>,
    ) -> Result<Self, WebUiError> {
        Self::serve_multi_with_remote_admin(volumes, addr, commands, false).await
    }

    /// [`WebUiServer::serve_multi_with_commands`] plus the §1.5
    /// remote-administration ruling: the management WRITE family
    /// (`POST /api/volumes/{name}/{remove,enable,disable}`) serves only
    /// when the bind address is loopback OR the operator passed
    /// `allow_remote_admin = true` (the process key of the same name,
    /// threaded in by the cli assembly). A non-loopback bind without the
    /// opt-in degrades the management plane to read-only — the write
    /// routes 403 naming the key, the reads (listing, configs, config)
    /// stay open — with a startup warning.
    pub async fn serve_multi_with_remote_admin(
        volumes: RegistryHandle,
        addr: SocketAddr,
        commands: Option<VolumeCommandClient>,
        allow_remote_admin: bool,
    ) -> Result<Self, WebUiError> {
        let writes_allowed = addr.ip().is_loopback() || allow_remote_admin;
        if !writes_allowed {
            // The crate's log face is eprintln (the accept-loop error
            // precedent); the cli assembly's tracing carries the same
            // fact on its own.
            eprintln!(
                "[web-ui] bound to a non-loopback address ({addr}): the \
                 volume-management write routes are disabled (403) — set \
                 allow_remote_admin = true in config.toml to manage volumes remotely"
            );
        }
        let app = multi_router(volumes, commands, writes_allowed);
        serve_router(app, addr).await
    }

    /// The actually bound address (`:0` resolves to the real port).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Graceful stop; idempotent. Signals the server, then waits for
    /// the accept task to finish (in-flight requests drain first).
    pub async fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        let task = self.task.lock().expect("server task lock").take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

/// The shared bind + accept-loop segment behind both constructors.
async fn serve_router(app: axum::Router, addr: SocketAddr) -> Result<WebUiServer, WebUiError> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| WebUiError::Bind { addr, source })?;
    let addr = listener
        .local_addr()
        .map_err(|source| WebUiError::Bind { addr, source })?;

    let (shutdown, rx) = watch::channel(false);
    let task = tokio::task::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async move {
            let mut rx = rx;
            let _ = rx.changed().await;
        });
        if let Err(error) = server.await {
            // The accept loop only errors on I/O failure after a
            // successful bind; there is no caller to surface it to,
            // so it lands in the logs (mirrors the WebDAV crate).
            eprintln!("[web-ui] server error: {error}");
        }
    });

    Ok(WebUiServer {
        addr,
        shutdown,
        task: Mutex::new(Some(task)),
    })
}

/// The nine contract routes (shared by both modes); the caller appends
/// any mode-specific routes and applies `.with_state` last.
fn contract_routes(router: axum::Router<AppState>) -> axum::Router<AppState> {
    router
        .route("/", get(index))
        // The volume-management page (web volume management §1.4) is on
        // BOTH mode tables by design: the multi-volume mode serves the
        // management skeleton, the single-volume mode the explanation
        // page (裁决④) — one handler, state-picked body.
        .route("/volumes", get(page_volumes))
        .route("/api/files", get(api_files))
        .route("/api/stats", get(api_stats))
        .route("/api/upload", post(api_upload))
        .route("/api/delete", post(api_delete))
        // `{*filename}` spans slashes: a sub-path download resolves as
        // its virtual RelPath (the Python `{filename}` route only ever
        // matched one segment because the frontend encodes bare names;
        // the wildcard keeps those requests identical while making
        // nested paths work).
        .route("/api/download/{*filename}", get(api_download))
        .route("/api/list", get(api_list))
        .route("/api/queue", get(api_queue))
        .route("/static/{*path}", get(static_asset))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
}

/// Assembles the single-volume route table over the shared state — the
/// frozen nine routes, byte-identical to the pre-MV3 table.
fn router(vfs: Arc<Vfs>, cfg: WebUiConfig) -> axum::Router {
    let state = AppState::Single {
        vfs,
        cfg: Arc::new(cfg),
    };
    contract_routes(axum::Router::new()).with_state(state)
}

/// Assembles the multi-volume route table: the nine contract routes
/// (K23 volume-parameter flavour) plus the two registry routes and the
/// volume-management family — `/api/volumes`, the config endpoint, the
/// configs listing and the P1 write routes share the same-origin guard
/// (§1.5 裁决③), the family deliberately NOT spanning the read routes
/// (downloads stay linkable from anywhere). `writes_allowed` is the
/// §1.5 verdict (loopback bind or the `allow_remote_admin` opt-in);
/// the write handlers 403 when it is `false`.
fn multi_router(
    volumes: RegistryHandle,
    commands: Option<VolumeCommandClient>,
    writes_allowed: bool,
) -> axum::Router {
    let state = AppState::Multi {
        volumes,
        commands,
        writes_allowed,
    };
    contract_routes(axum::Router::new())
        .route(
            "/api/volumes",
            get(api_volumes)
                .post(api_volume_create)
                .layer(axum::middleware::from_fn(same_origin_guard)),
        )
        .route(
            "/api/volumes/configs",
            get(api_volume_configs).layer(axum::middleware::from_fn(same_origin_guard)),
        )
        .route(
            "/api/volumes/{name}/config",
            get(api_volume_config).layer(axum::middleware::from_fn(same_origin_guard)),
        )
        .route(
            "/api/volumes/{name}/remove",
            post(api_volume_remove).layer(axum::middleware::from_fn(same_origin_guard)),
        )
        .route(
            "/api/volumes/{name}/disable",
            post(api_volume_disable).layer(axum::middleware::from_fn(same_origin_guard)),
        )
        .route(
            "/api/volumes/{name}/enable",
            post(api_volume_enable).layer(axum::middleware::from_fn(same_origin_guard)),
        )
        .route(
            "/api/volumes/{name}/rebuild",
            post(api_volume_rebuild).layer(axum::middleware::from_fn(same_origin_guard)),
        )
        .route("/api/stats/summary", get(api_stats_summary))
        .with_state(state)
}

/// Shared handler state: exactly one volume (the frozen single-volume
/// face) or the volume registry (MV3; a live shared handle since RV1).
#[derive(Clone)]
enum AppState {
    /// The single-volume boot: one VFS, one config — handlers resolve
    /// every request to this pair unchanged (a stray `?volume=` query
    /// parameter is ignored; there is no registry to route through).
    Single {
        vfs: Arc<Vfs>,
        cfg: Arc<WebUiConfig>,
    },
    /// The multi-volume boot (K23/K24): the shared table the handlers
    /// re-read per request (RV1) to resolve the `?volume=<name>`
    /// parameter, plus the volume-command seam (web volume management
    /// §1.1) the management routes send through — `None` on a dashboard
    /// booted without one (those routes answer 503) — and the §1.5
    /// write verdict (loopback bind or the `allow_remote_admin`
    /// opt-in; the write routes 403 when `false`).
    Multi {
        volumes: RegistryHandle,
        commands: Option<VolumeCommandClient>,
        writes_allowed: bool,
    },
}

/// One request's volume resolution: the `(vfs, config)` pair the frozen
/// handler bodies run against. Owned values — cloned out of the table
/// lock before the handler body runs.
struct ResolvedVolume {
    vfs: Arc<Vfs>,
    cfg: WebUiConfig,
}

/// The K23 routing refusals' uniform JSON body: the error message plus
/// the addressable volume list, so a wrong or missing parameter is
/// actionable on the spot.
fn volume_routing_error(
    status: StatusCode,
    message: impl Into<String>,
    volumes: &[String],
) -> Response {
    let body = serde_json::json!({ "error": message.into(), "volumes": volumes });
    (status, Json(body)).into_response()
}

impl AppState {
    /// Resolves one request's `?volume=` parameter (K23). Single-volume
    /// mode ignores the parameter outright (zero drift — there is
    /// exactly one volume). Multi-volume mode requires it: absent or
    /// empty answers 400 with the volume list, an unknown name answers
    /// 404 with the same list, a known-but-failed volume answers 409
    /// with its reason, and a running volume resolves to its own
    /// `(vfs, config)` pair.
    // axum's Response IS the handler currency here (every refusal goes
    // straight back as the handler's return value); boxing it would
    // trade a per-request indirection for a lint's byte count.
    #[allow(clippy::result_large_err)]
    fn resolve(&self, query: Option<&str>) -> Result<ResolvedVolume, Response> {
        match self {
            AppState::Single { vfs, cfg } => Ok(ResolvedVolume {
                vfs: Arc::clone(vfs),
                cfg: (**cfg).clone(),
            }),
            AppState::Multi { volumes, .. } => {
                let names = volumes.names();
                let requested = query
                    .and_then(|q| query_param(q, "volume"))
                    .filter(|name| !name.is_empty());
                let Some(name) = requested else {
                    return Err(volume_routing_error(
                        StatusCode::BAD_REQUEST,
                        "the volume parameter is required in multi-volume mode: add \
                         ?volume=<name> to the request",
                        &names,
                    ));
                };
                let Some(entry) = volumes.find(&name) else {
                    return Err(volume_routing_error(
                        StatusCode::NOT_FOUND,
                        format!("unknown volume '{name}'"),
                        &names,
                    ));
                };
                match (&entry.status, &entry.vfs) {
                    (VolumeUiStatus::Failed { reason }, _) => Err(volume_routing_error(
                        StatusCode::CONFLICT,
                        format!("volume '{name}' is failed and serves no traffic: {reason}"),
                        &names,
                    )),
                    (_, Some(vfs)) => Ok(ResolvedVolume {
                        vfs: Arc::clone(vfs),
                        cfg: entry.config.clone(),
                    }),
                    // A running entry without a VFS is an inconsistent
                    // assembly (unrepresentable through the cli boot);
                    // refusing with the same conflict shape is the
                    // honest answer, never a panic.
                    (_, None) => Err(volume_routing_error(
                        StatusCode::CONFLICT,
                        format!("volume '{name}' exposes no vfs"),
                        &names,
                    )),
                }
            }
        }
    }
}

/// A full-body response with one content type.
fn content_response(content_type: &str, bytes: Vec<u8>) -> Response {
    (
        [(header::CONTENT_TYPE, content_type.to_string())],
        Body::from(bytes),
    )
        .into_response()
}

/// A JSON error body with a status (the handlers' uniform error shape,
/// mirroring Python's `{"error": "..."}` responses).
fn error_json(status: StatusCode, message: impl Into<String>) -> Response {
    let body = serde_json::json!({ "error": message.into() });
    (status, Json(body)).into_response()
}

/// Serves `templates/index.html` verbatim: the skeleton is fully static
/// (app.js fills every dynamic field), exactly like the Python handler
/// that reads and returns the file without template substitution.
async fn index() -> Response {
    match Templates::get("index.html") {
        Some(file) => content_response("text/html; charset=utf-8", file.data.into_owned()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serves an embedded static asset with a guessed content type
/// (aiohttp's `add_static` equivalent).
async fn static_asset(Path(path): Path<String>) -> Response {
    match StaticAssets::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            content_response(mime.as_ref(), file.data.into_owned())
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /volumes` (web volume management §1.4): the configuration-state
/// page, one handler for both modes — multi-volume serves the
/// management skeleton (`volumes.html`, filled by `volumes.js` off
/// `/api/volumes`); a single-volume instance gets the explanation page
/// (裁决④: its one volume is defined by `config.toml`, there is no
/// volume registry to manage).
async fn page_volumes(State(state): State<AppState>) -> Response {
    let template = match &state {
        AppState::Multi { .. } => "volumes.html",
        AppState::Single { .. } => "volumes-single.html",
    };
    match Templates::get(template) {
        Some(file) => content_response("text/html; charset=utf-8", file.data.into_owned()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ------------------------------- the same-origin guard (web volume §1.5) ---

/// The volume-management family's CSRF-shaped defense (裁决③: Origin
/// checking, not tokens): a state-changing browser request always
/// carries an Origin (or a Referer) naming the site that issued it, so
/// a management API call whose Origin/Referer names another site is a
/// cross-site forgery and answers 403. Requests carrying neither header
/// (non-browser clients, address-bar navigation) pass — the dashboard
/// has no session for them to forge. Deliberately scoped to the
/// `/api/volumes*` family only: the read routes (downloads, stats) and
/// the pages stay open to foreign links.
async fn same_origin_guard(request: Request, next: Next) -> Response {
    match cross_origin_refusal(&request) {
        Some(message) => error_json(StatusCode::FORBIDDEN, message),
        None => next.run(request).await,
    }
}

/// The refusal verdict for one request: `Some(message)` when an Origin
/// or Referer header is present and does not name this server's own
/// authority (compared case-insensitively against the Host header).
/// `Origin: null` (sandboxed frames) counts as foreign. A missing Host
/// passes — browsers always send one, so a headerless pair is a
/// non-browser client, not an attack.
fn cross_origin_refusal(request: &Request) -> Option<String> {
    let headers = request.headers();
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let (name, value) = if let Some(origin) = headers.get(header::ORIGIN) {
        ("Origin", origin.to_str().ok()?)
    } else {
        let referer = headers.get(header::REFERER)?;
        ("Referer", referer.to_str().ok()?)
    };
    let authority = origin_authority(value);
    (!authority.eq_ignore_ascii_case(host)).then(|| {
        format!(
            "cross-origin request refused: the volume management API only serves this \
             dashboard's own origin ({name} {value} does not match Host {host})"
        )
    })
}

/// The authority span of an Origin/Referer value — `scheme://authority`
/// up to the first `/` (the path tail of a Referer never participates).
/// A value with no parseable scheme degrades to its text up to the
/// first `/`, which cannot equal a `host:port` pair unless it is one.
fn origin_authority(value: &str) -> &str {
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    rest.split('/').next().unwrap_or(rest)
}

// ------------------------- the volume-command seam routes (web volume §1.1) ---

/// The web-side budget for one seam command (§4's risk mitigation):
/// SHOW is a fast file read, but the seam serializes behind whatever
/// the control channel has in flight (a REMOVE draining a large
/// upload) — the dashboard refuses to park behind it. Deliberately NOT
/// the control client's 120s budget: that exists for a REMOVE's own
/// drain windows; this is an in-process callback answering reads.
const VOLUME_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// `GET /api/volumes/{name}/config` (multi-volume mode only; web
/// volume management §1.1/§1.2): the named volume's FILE configuration
/// through the command seam — a `SHOW <name>` whose reply JSON passes
/// through verbatim (core's serializer already collapsed every
/// credential key to its `{"set": bool}` marker; the value never
/// reaches this layer, let alone the browser). The registry gates the
/// lookup — a REMOVE'd volume 404s even though its file stays (K49:
/// removal is runtime-only). A missing seam answers 503 (a dashboard
/// booted without volume management), a command that outlives
/// [`VOLUME_COMMAND_TIMEOUT`] 503s actionably, and the seam's `ERR:`
/// replies carry their actionable text as 404s.
async fn api_volume_config(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let AppState::Multi {
        volumes, commands, ..
    } = &state
    else {
        return error_json(
            StatusCode::NOT_FOUND,
            "no volume registry: this dashboard serves a single volume",
        );
    };
    let names = volumes.names();
    if volumes.find(&name).is_none() {
        return volume_routing_error(
            StatusCode::NOT_FOUND,
            format!("unknown volume '{name}'"),
            &names,
        );
    }
    let Some(client) = commands else {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "volume configuration is not available: this dashboard runs without the \
             volume-command seam (the multi-volume cli assembly installs it)",
        );
    };
    let command = format!("SHOW {name}");
    let reply = match tokio::time::timeout(VOLUME_COMMAND_TIMEOUT, client(&command)).await {
        Ok(reply) => reply,
        Err(_elapsed) => {
            return error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "the volume command for '{name}' timed out — the instance is likely busy \
                     with a long volume command (e.g. a REMOVE draining an upload); retry \
                     once it settles"
                ),
            );
        }
    };
    match reply.strip_prefix("OK: ") {
        Some(payload) => match serde_json::from_str::<serde_json::Value>(payload.trim()) {
            Ok(value) => Json(value).into_response(),
            Err(_) => error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the SHOW reply was not valid JSON — the volume file may be mid-edit; retry",
            ),
        },
        None => error_json(
            StatusCode::NOT_FOUND,
            reply.trim().trim_start_matches("ERR: "),
        ),
    }
}

// --------------------------- the P1 write family + configs (web volume §1.2/§1.5) ---

/// The write-family seam budget: a REMOVE legitimately waits out its
/// 60s drain + 10s unmount windows (K50), so the web call carries the
/// control client's own 120s budget (M1 `EXCHANGE_BUDGET` semantics)
/// instead of the read family's 5s — the dashboard parks behind a long
/// command rather than giving up on it.
const VOLUME_WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// The shared body of the three write routes (P1, plan §1.2): the §1.5
/// write verdict first (403 naming the opt-in key on a non-loopback
/// bind), then one command through the seam with the write budget.
/// Replies: the pinned success shape `{"ok": true, "reply": "..."}` (the
/// `OK: ` prefix stripped — the dashboard toasts the text verbatim), a
/// seam `ERR:` as 409 with its actionable text (a refused mutation is a
/// conflict; the reply text is what the operator needs), a missing seam
/// the family 503, a command that outlives the budget the busy 503.
/// No registry gate on purpose: a DISABLED volume is by definition
/// absent from the registry — the command's own reply is the authority
/// on unknown names.
async fn volume_write_route(state: AppState, name: String, verb: &str) -> Response {
    let AppState::Multi {
        commands,
        writes_allowed,
        ..
    } = state
    else {
        return error_json(
            StatusCode::NOT_FOUND,
            "no volume registry: this dashboard serves a single volume",
        );
    };
    if !writes_allowed {
        return error_json(
            StatusCode::FORBIDDEN,
            "remote administration is disabled: the web UI is bound to a non-loopback \
             address, so the volume-management write routes refuse requests — set \
             `allow_remote_admin = true` in config.toml to manage volumes remotely",
        );
    }
    let Some(client) = commands else {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "volume commands are not available: this dashboard runs without the \
             volume-command seam (the multi-volume cli assembly installs it)",
        );
    };
    let command = format!("{verb} {name}");
    let reply = match tokio::time::timeout(VOLUME_WRITE_TIMEOUT, client(&command)).await {
        Ok(reply) => reply,
        Err(_elapsed) => {
            return error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "the {verb} command for '{name}' timed out — the instance is likely busy \
                     with a long volume command (e.g. a REMOVE draining an upload); retry \
                     once it settles"
                ),
            );
        }
    };
    map_volume_command_reply(&reply)
}

/// The write family's shared seam-reply mapping: an `OK: ` prefix
/// becomes the pinned success shape `{"ok": true, "reply": "..."}` (the
/// prefix stripped — the dashboard toasts the text verbatim); anything
/// else is the seam's actionable `ERR:` as 409 (a refused mutation is a
/// conflict; the reply text is what the operator needs).
fn map_volume_command_reply(reply: &str) -> Response {
    match reply.trim().strip_prefix("OK: ") {
        Some(payload) => Json(serde_json::json!({
            "ok": true,
            "reply": payload.trim(),
        }))
        .into_response(),
        None => error_json(
            StatusCode::CONFLICT,
            reply.trim().trim_start_matches("ERR: "),
        ),
    }
}

/// The CREATE/UPDATE seam budget (P3/P4): these commands carry a whole
/// assembly behind the serialized queue. A CREATE runs the ADD leg (the
/// telegram connect budget alone is 90s, plus the mount); an UPDATE runs
/// REMOVE first (60s drain + 10s release ≈ 70s) and THEN the ADD leg.
/// 180s covers remove 70s + add 90s with margin — the M1
/// `EXCHANGE_BUDGET` philosophy one step wider than the P1 family's
/// 120s, which predates the assembly-carrying commands.
const VOLUME_ASSEMBLY_TIMEOUT: Duration = Duration::from_secs(180);

/// `POST /api/volumes` (P3, the Add Volume form's transport): the body
/// is the form's JSON object — its `name` keys the command line, every
/// other member rides as the compact single-line JSON payload of one
/// `CREATE <name> <json>` through the seam (the command is the authority
/// on the payload's semantics — key space, validation, the controlled
/// toml write; the route is the authority on the HTTP body's shape).
/// Budget [`VOLUME_ASSEMBLY_TIMEOUT`]; success/ERR mapping shared with
/// the write family ([`map_volume_command_reply`]); the §1.5 gates
/// (Origin middleware on the route, the write verdict, the seam) answer
/// before the command is built. Credentials ride the body the same way
/// they ride the volume file — the loopback trust face — and never ride
/// any reply (the command's refusals name keys, M3).
async fn api_volume_create(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let AppState::Multi {
        commands,
        writes_allowed,
        ..
    } = state
    else {
        return error_json(
            StatusCode::NOT_FOUND,
            "no volume registry: this dashboard serves a single volume",
        );
    };
    if !writes_allowed {
        return error_json(
            StatusCode::FORBIDDEN,
            "remote administration is disabled: the web UI is bound to a non-loopback \
             address, so the volume-management write routes refuse requests — set \
             `allow_remote_admin = true` in config.toml to manage volumes remotely",
        );
    }
    let Some(client) = commands else {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "volume commands are not available: this dashboard runs without the \
             volume-command seam (the multi-volume cli assembly installs it)",
        );
    };
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                format!("the request body is not valid JSON: {error} — send the form object"),
            );
        }
    };
    let serde_json::Value::Object(fields) = value else {
        return error_json(
            StatusCode::BAD_REQUEST,
            "the create body must be a JSON object (the form's fields plus its `name`)",
        );
    };
    let name = match fields.get("name") {
        Some(serde_json::Value::String(name)) if !name.trim().is_empty() => name.clone(),
        _ => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "the create body needs a non-empty string `name` (the new volume's name)",
            );
        }
    };
    let mut payload = fields;
    payload.remove("name");
    // Compact serialization: the control channel's line protocol carries
    // the payload as ONE line (a serialized JSON body never spans lines
    // — newlines ride as escapes).
    let json = match serde_json::to_string(&payload) {
        Ok(json) => json,
        Err(error) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                format!("serializing the form payload failed: {error}"),
            );
        }
    };
    let command = format!("CREATE {name} {json}");
    let reply = match tokio::time::timeout(VOLUME_ASSEMBLY_TIMEOUT, client(&command)).await {
        Ok(reply) => reply,
        Err(_elapsed) => {
            return error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "the CREATE command for '{name}' timed out — the instance is likely busy \
                     or the new volume's backend is slow to connect; the volume file may \
                     still have been written — check the volume list and retry once it \
                     settles"
                ),
            );
        }
    };
    map_volume_command_reply(&reply)
}

/// `POST /api/volumes/{name}/remove` (P1): an unmount through the same
/// K50 sequence the control channel's REMOVE runs (the seam forwards to
/// the identical handler).
async fn api_volume_remove(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    volume_write_route(state, name, "REMOVE").await
}

/// `POST /api/volumes/{name}/disable` (P1): `enabled = false` written to
/// the volume file first, then the same unmount as REMOVE.
async fn api_volume_disable(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    volume_write_route(state, name, "DISABLE").await
}

/// `POST /api/volumes/{name}/enable` (P1): `enabled = true` written,
/// then the volume re-assembles through the runtime ADD path.
async fn api_volume_enable(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    volume_write_route(state, name, "ENABLE").await
}

/// `POST /api/volumes/{name}/rebuild` (P6, the Refresh button): one
/// `REBUILD <name>` through the seam — the reply is the instance's
/// ACCEPTANCE (R3: the walk runs as its background task, so the write
/// budget only ever covers the serialized queue, never the walk). The
/// seam's `ERR:` refusals (already running, undrained queue, the K11/
/// telegram gates) ride the family's 409 verbatim.
async fn api_volume_rebuild(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    volume_write_route(state, name, "REBUILD").await
}

/// One CONFIGS reply row parsed into its JSON face: a loadable file as
/// `{name, backend, enabled, running}` plus the P2 sparse markers —
/// `rebuilding: true` while a background REBUILD is in flight and
/// `encrypted: true` while the file carries `enable_encryption` (the
/// Refresh button's gating pair, P6; emitted only when true so a quiet
/// row keeps its P1 shape) — and a schema-broken one as
/// `{name, invalid: true, reason}` (the reason is display text — a
/// re-parse failure keeps the row with the invalid marker rather than
/// dropping it).
fn configs_row_json(line: &str) -> serde_json::Value {
    let mut tokens = line.split_whitespace();
    let name = tokens.next().unwrap_or_default();
    match tokens.next() {
        Some("invalid") => {
            // The row wraps its reason in parentheses; unwrap the pair.
            let reason = tokens.collect::<Vec<_>>().join(" ");
            let reason = reason
                .strip_prefix('(')
                .and_then(|inner| inner.strip_suffix(')'))
                .unwrap_or(&reason);
            serde_json::json!({
                "name": name,
                "invalid": true,
                "reason": reason,
            })
        }
        Some(backend) => {
            let enabled = tokens
                .next()
                .and_then(|flag| flag.strip_prefix("enabled="))
                .and_then(|flag| flag.parse::<bool>().ok());
            let running = tokens.next() == Some("running");
            // The sparse markers (P2): whatever trailing tokens the row
            // carries; unknown ones stay ignored for forward compat.
            let (mut encrypted, mut rebuilding) = (false, false);
            for marker in tokens {
                match marker {
                    "encrypted" => encrypted = true,
                    "rebuilding" => rebuilding = true,
                    _ => {}
                }
            }
            let mut row = serde_json::json!({
                "name": name,
                "backend": backend,
                "enabled": enabled,
                "running": running,
            });
            // Inserted only when true — the object stays byte-equal to
            // the P1 shape for quiet rows (the pins and the frontend's
            // `=== true` reads both rely on the absence).
            if encrypted {
                row["encrypted"] = serde_json::json!(true);
            }
            if rebuilding {
                row["rebuilding"] = serde_json::json!(true);
            }
            row
        }
        None => serde_json::json!({ "name": name, "invalid": true, "reason": "" }),
    }
}

/// `GET /api/volumes/configs` (P1, the config page's data source): one
/// `CONFIGS` through the seam (the configuration FULL set — `/api/volumes`
/// reads the registry, which cannot see disabled volumes), the reply's
/// row lines parsed into JSON. The read budget suffices: CONFIGS is a
/// directory scan plus file parses. An `ERR:` reply carries its text as
/// a 404 (the config endpoint's mapping), a missing seam the family 503.
async fn api_volume_configs(State(state): State<AppState>) -> Response {
    let AppState::Multi { commands, .. } = &state else {
        return error_json(
            StatusCode::NOT_FOUND,
            "no volume registry: this dashboard serves a single volume",
        );
    };
    let Some(client) = commands else {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "volume configurations are not available: this dashboard runs without the \
             volume-command seam (the multi-volume cli assembly installs it)",
        );
    };
    let reply = match tokio::time::timeout(VOLUME_COMMAND_TIMEOUT, client("CONFIGS")).await {
        Ok(reply) => reply,
        Err(_elapsed) => {
            return error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "the CONFIGS listing timed out — the instance is likely busy with a long \
                 volume command (e.g. a REMOVE draining an upload); retry once it settles",
            );
        }
    };
    match reply.strip_prefix("OK: ") {
        Some(rows) => Json(serde_json::Value::Array(
            rows.lines().skip(1).map(configs_row_json).collect(),
        ))
        .into_response(),
        None => error_json(
            StatusCode::NOT_FOUND,
            reply.trim().trim_start_matches("ERR: "),
        ),
    }
}

/// One `files` row as the Python dashboard serialized it: sqlite's
/// `SELECT *` column order with the boolean flags as 0/1 ints (sqlite
/// has no booleans) and `Option` columns as `null`.
fn file_row_json(row: &FileRecord) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "rel_path": row.rel_path,
        "name": row.name,
        "parent_dir": row.parent_dir,
        "size": row.size,
        "mtime": row.mtime,
        "sha256": row.sha256,
        "is_dir": i64::from(row.is_dir),
        "telegram_msg_id": row.telegram_msg_id,
        "is_uploaded": i64::from(row.is_uploaded),
        "is_cached": i64::from(row.is_cached),
        "is_encrypted": i64::from(row.is_encrypted),
        "chunk_count": row.chunk_count,
        "mime_type": row.mime_type,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

/// `GET /api/files`: every row, `updated_at DESC` (the DB mirrors the
/// Python ordering). Multi-volume mode routes by `?volume=` (K23).
async fn api_files(State(state): State<AppState>, uri: Uri) -> Response {
    let volume = match state.resolve(uri.query()) {
        Ok(volume) => volume,
        Err(response) => return response,
    };
    match volume.vfs.db().list_all_files() {
        Ok(rows) => Json(serde_json::Value::Array(
            rows.iter().map(file_row_json).collect(),
        ))
        .into_response(),
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// Derives the `(host, port)` pair the Python handler glued on from its
/// config fields. The frozen [`WebUiConfig`] carries only the URL, so
/// the pair is parsed back out of it — the single-volume URL is built
/// by the caller as `http://{host}:{port}` and the multi-volume one
/// appends the `/vol/<name>` segment (K27), so the authority ends at
/// the first path segment (a URL without a port degrades to the HTTP
/// default).
fn split_webdav_url(url: &str) -> (String, u16) {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().unwrap_or(80)),
        None => (authority.to_string(), 80),
    }
}

/// `GET /api/stats`: the DB aggregates plus the six dashboard fields
/// the Python handler added on top, and the five backend-identity keys
/// the multi-backend adapter added on top of those (all additive — the
/// frozen Python-parity keys are untouched). Multi-volume mode routes
/// by `?volume=` (K23) and answers the SAME 16-key contract with the
/// named volume's own aggregates and identity keys.
async fn api_stats(State(state): State<AppState>, uri: Uri) -> Response {
    let volume = match state.resolve(uri.query()) {
        Ok(volume) => volume,
        Err(response) => return response,
    };
    let stats = match volume.vfs.db().get_stats() {
        Ok(stats) => stats,
        Err(error) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let (webdav_host, webdav_port) = split_webdav_url(&volume.cfg.webdav_url);
    Json(serde_json::json!({
        "total_files": stats.total_files,
        "total_bytes": stats.total_bytes,
        "total_dirs": stats.total_dirs,
        "uploaded_files": stats.uploaded_files,
        "pending_uploads": stats.pending_uploads,
        "drive_letter": volume.cfg.drive_letter,
        "webdav_host": webdav_host,
        "webdav_port": webdav_port,
        "webdav_url": volume.cfg.webdav_url,
        "chat_id": volume.cfg.chat_id,
        "is_configured": volume.cfg.is_configured,
        // Multi-backend identity (the dashboard adapter): reported
        // verbatim from the assembly — the web layer invents nothing.
        "backend": volume.cfg.backend,
        "volume": volume.cfg.volume,
        "remote_delete": volume.cfg.remote_delete,
        "quota_used": volume.cfg.quota.as_ref().map(|quota| quota.used),
        "quota_total": volume.cfg.quota.as_ref().and_then(|quota| quota.total),
    }))
    .into_response()
}

/// Fractional seconds since the Unix epoch (the DB `mtime` convention).
fn now_epoch_f64() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// `POST /api/upload`: the first multipart field must be `file` (the
/// only shape the frontend sends and the Python handler reads); its
/// bytes land in [`Vfs::put`] under `/{filename}` — accepted on the
/// spot, uploaded by the queue asynchronously (WebDAV-PUT semantics).
/// Multi-volume mode routes by `?volume=` (K23) BEFORE any body parsing.
async fn api_upload(State(state): State<AppState>, uri: Uri, mut multipart: Multipart) -> Response {
    let volume = match state.resolve(uri.query()) {
        Ok(volume) => volume,
        Err(response) => return response,
    };
    let Some(field) = multipart.next_field().await.unwrap_or(None) else {
        return error_json(StatusCode::BAD_REQUEST, "Invalid upload");
    };
    if field.name() != Some("file") {
        return error_json(StatusCode::BAD_REQUEST, "Invalid upload");
    }
    let Some(filename) = field.file_name().map(str::to_string) else {
        return error_json(StatusCode::BAD_REQUEST, "No filename");
    };
    let bytes = match field.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return error_json(StatusCode::BAD_REQUEST, "Invalid upload"),
    };
    // The Python handler pasted the name under `/` verbatim; RelPath
    // additionally rejects pathologically shaped names (`..`, empty)
    // that the frontend cannot produce.
    let Ok(rel) = RelPath::new(&format!("/{filename}")) else {
        return error_json(StatusCode::BAD_REQUEST, "Invalid filename");
    };
    match volume.vfs.put(&rel, &bytes, now_epoch_f64()).await {
        Ok(()) => {
            Json(serde_json::json!({ "success": true, "filename": filename })).into_response()
        }
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// `POST /api/delete` with body `{"filename": ...}`: normalizes the
/// name the way the Python handler did (`/`-prefixed, backslashes
/// folded) and issues exactly **one** delete — the Python double call
/// (`delete_file(clean_rel); delete_file(filename)`) was a defect the
/// design doc explicitly fixes here. Files go through
/// [`Vfs::remove_file`] (review High-2 — the route's former direct db
/// write bypassed the VFS layer: no pending-guard sharing, no
/// cache-copy cleanup, no sync-doorbell ring), so the row, its cached
/// copy and the realtime wake are all handled by the one call; the
/// remote side is the K4 gate inside that call (Phase 2): a
/// `remote_delete` transport (baidu/local) deletes the remote object
/// first and a refusal keeps the row, a bit-off transport (telegram)
/// keeps the baseline's remote-stays mirror. A pending upload whose
/// local cache copy still exists is refused with 409 (review H2 / plan
/// F2, the same adjudication as the WebDAV DELETE): that copy is the
/// only copy of the bytes. Directory rows keep the baseline's
/// unconditional row delete — the dashboard renders the delete button
/// on folders too and [`Vfs::remove_file`] refuses directories —
/// behind the same K4 gate (`Vfs::delete_remote_for_row`); a missing
/// row answers the baseline's idempotent no-op success. Multi-volume
/// mode routes by `?volume=` (K23).
async fn api_delete(
    State(state): State<AppState>,
    uri: Uri,
    body: Json<serde_json::Value>,
) -> Response {
    let volume = match state.resolve(uri.query()) {
        Ok(volume) => volume,
        Err(response) => return response,
    };
    let filename = body
        .0
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if filename.is_empty() {
        return error_json(StatusCode::BAD_REQUEST, "No filename provided");
    }
    let clean_rel = format!("/{}", filename.trim_matches('/').replace('\\', "/"));
    let Ok(rel) = RelPath::new(&clean_rel) else {
        // A name that cannot form a valid RelPath can never match a
        // stored row; the baseline's unconditional delete was a no-op
        // success, and the idempotent answer keeps that shape.
        return delete_success(filename);
    };
    match volume.vfs.remove_file(&rel).await {
        Ok(()) => delete_success(filename),
        // The pending-upload guard's 409 face, unchanged: the row's
        // local cache copy is the only copy of the bytes.
        Err(VfsError::UploadPending(_)) => error_json(
            StatusCode::CONFLICT,
            format!("still uploading, try again after it finishes: {filename}"),
        ),
        // Idempotent delete — exactly the no-op success the baseline's
        // unconditional delete_file produced for a missing row.
        Err(VfsError::NotFound(_)) => delete_success(filename),
        // Directories: Vfs::remove_file refuses them, but the frontend
        // renders the delete button on folder rows and the baseline
        // deleted the row unconditionally — keep that shape (rows only;
        // with the remote_delete bit off the remote stays, Python
        // mirror; with it on the K4 gate deletes the dir's remote
        // object first and a refusal keeps the row). No manual doorbell
        // needed: the row delete rings the db-layer files hook (the
        // chokepoint; deletion = tombstone origin).
        Err(VfsError::IsDirectory(_)) => {
            if let Err(error) = volume.vfs.delete_remote_for_row(&rel).await {
                return error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
            match volume.vfs.db().delete_file(&clean_rel) {
                Ok(()) => delete_success(filename),
                Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            }
        }
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// The delete route's success body: Python's shape echoing the raw
/// filename from the request (shared by the file, directory and
/// idempotent-miss arms).
fn delete_success(filename: &str) -> Response {
    Json(serde_json::json!({ "success": true, "deleted": filename })).into_response()
}

/// One parsed single-range byte spec (the `bytes=a-b` family).
enum ByteRange {
    /// `bytes=a-b` (end inclusive).
    Span(u64, u64),
    /// `bytes=a-` (start through the last byte).
    Open(u64),
    /// `bytes=-n` (the final n bytes).
    Suffix(u64),
}

/// Parses a `Range` header value into a single byte range. `None` means
/// the value does not name exactly one well-formed range — a missing
/// `bytes=` unit, several comma-separated ranges, an inverted `a-b`, or
/// non-numeric bounds — and the caller serves the full body instead
/// (HTTP's lenient ignore-a-bad-Range convention).
fn parse_byte_range(value: &str) -> Option<ByteRange> {
    let rest = value.trim().strip_prefix("bytes=")?;
    if rest.contains(',') {
        return None; // multi-range: never served here, ignored wholesale
    }
    let (start, end) = rest.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    match (start.is_empty(), end.is_empty()) {
        (false, false) => {
            let (start, end) = (start.parse().ok()?, end.parse().ok()?);
            (start <= end).then_some(ByteRange::Span(start, end))
        }
        (false, true) => Some(ByteRange::Open(start.parse().ok()?)),
        (true, false) => Some(ByteRange::Suffix(end.parse().ok()?)),
        (true, true) => None,
    }
}

/// Resolves a parsed range against a body of `size` bytes to an
/// inclusive `(start, end)` slice, clamping per RFC 9110 (an end past
/// the last byte is the last byte; a suffix longer than the body is the
/// whole body). `None` is unsatisfiable — 416 territory.
fn resolve_byte_range(spec: ByteRange, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None; // no byte can satisfy any range of an empty body
    }
    match spec {
        ByteRange::Span(start, end) => (start < size).then(|| (start, end.min(size - 1))),
        ByteRange::Open(start) => (start < size).then(|| (start, size - 1)),
        // A suffix-length of zero is unsatisfiable (RFC 9110).
        ByteRange::Suffix(len) => (len > 0).then(|| (size - len.min(size), size - 1)),
    }
}

/// The download routes' content-header pair: a mime guessed from the
/// name (`mimetypes.guess_type` analog) and Python's inline
/// disposition. Shared by the hydrated fallback
/// ([`download_response`]) and the streaming branch
/// ([`streaming_download_response`]) so both faces answer
/// byte-identical headers.
fn download_content_headers(filename: &str) -> (String, String) {
    let mime = mime_guess::from_path(filename).first_or_octet_stream();
    let disposition = format!("inline; filename=\"{filename}\"");
    (mime.as_ref().to_string(), disposition)
}

/// A hydrated download (the R-5 fallback of [`api_download`]): the
/// exact bytes in one `Body` (which sets the Content-Length). A
/// present-and-parseable `Range` header narrows the answer to one
/// slice: 206 + `Content-Range` when satisfiable, 416 +
/// `Content-Range: bytes */size` when not; anything the parser rejects
/// falls through to the full 200 body.
fn download_response(filename: &str, bytes: Vec<u8>, range: Option<&str>) -> Response {
    let (mime, disposition) = download_content_headers(filename);
    if let Some(spec) = range.and_then(parse_byte_range) {
        return match resolve_byte_range(spec, bytes.len() as u64) {
            Some((start, end)) => {
                let slice = &bytes[start as usize..=end as usize];
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, mime),
                        (header::CONTENT_DISPOSITION, disposition),
                        (
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{end}/{}", bytes.len()),
                        ),
                        (header::ACCEPT_RANGES, "bytes".to_string()),
                    ],
                    Body::from(slice.to_vec()),
                )
                    .into_response()
            }
            // Parseable but unsatisfiable: name the size so the client
            // can re-request against reality.
            None => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", bytes.len()))],
                Body::empty(),
            )
                .into_response(),
        };
    }
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, disposition),
            (header::ACCEPT_RANGES, "bytes".to_string()),
        ],
        Body::from(bytes),
    )
        .into_response()
}

/// The streaming branch of a download (SR2 / K36): one bounded
/// `open_range` window per response, served through
/// [`Body::from_stream`] instead of a hydrated local copy. The Range
/// math reuses [`parse_byte_range`] / [`resolve_byte_range`] verbatim,
/// decided against the authoritative row size (K35) — never against
/// bytes already read.
///
/// Header contract: 200 and 206 both carry an EXPLICIT Content-Length
/// (`Body::from_stream` has no length of its own — without the header
/// the answer would degrade to chunked framing) and
/// `Accept-Ranges: bytes`. An unsatisfiable range answers 416 +
/// `bytes */size` with an empty body and no remote call at all; a
/// multi-range header is ignored wholesale (a 200 full-body stream),
/// matching the lenient fallback of [`download_response`].
///
/// Failure semantics: a transport error mid-stream surfaces as a body
/// `Err` after the head may already be on the wire — HTTP cannot
/// change the status code after the first byte, so the body truncates
/// short of the promised Content-Length (the connection ends; the
/// client retries). The route never fabricates content to fill the
/// promise (PCFS parity).
fn streaming_download_response(
    filename: &str,
    handle: RemoteHandle,
    total_size: u64,
    transport: Arc<dyn CloudTransport>,
    range: Option<&str>,
) -> Response {
    let (mime, disposition) = download_content_headers(filename);
    if let Some(spec) = range.and_then(parse_byte_range) {
        return match resolve_byte_range(spec, total_size) {
            Some((start, end)) => {
                let len = end - start + 1;
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, mime),
                        (header::CONTENT_DISPOSITION, disposition),
                        (
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{end}/{total_size}"),
                        ),
                        (header::CONTENT_LENGTH, len.to_string()),
                        (header::ACCEPT_RANGES, "bytes".to_string()),
                    ],
                    Body::from_stream(RangeBody::new(transport, handle, start, len)),
                )
                    .into_response()
            }
            // Decided off the row size alone: no remote call is made.
            None => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total_size}"))],
                Body::empty(),
            )
                .into_response(),
        };
    }
    // Full-body stream: [`RangeBody`] walks the object in bounded
    // `open_range` windows (≤4 MiB each), so memory stays
    // window-granular, never file-granular — including the decrypting
    // wrapper, whose per-call span pull must stay bounded (K47).
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CONTENT_LENGTH, total_size.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
        ],
        Body::from_stream(RangeBody::new(transport, handle, 0, total_size)),
    )
        .into_response()
}

/// Response-body side of the streaming download (the SSE `FramePipe`
/// precedent shape): the [`Stream`] impl walks the requested range in
/// bounded `open_range` windows (at most [`WEB_STREAM_WINDOW`] bytes
/// each — the RangeFile/WindowReader model), forwarding each window's
/// frames to [`Body::from_stream`] verbatim. The transport error type
/// satisfies `Into<BoxError>` directly, so no re-mapping layer is
/// needed. Plaintext transports self-bound their fetches anyway; the
/// window loop additionally bounds the per-call length the decrypting
/// wrapper (K47) turns into one bounded span pull — a whole-body call
/// length would stall the first byte behind the full-ciphertext
/// aggregate (the 2026-09-10 open-ended-playback defect).
const WEB_STREAM_WINDOW: u64 = 4 * 1024 * 1024;

struct RangeBody {
    transport: Arc<dyn CloudTransport>,
    handle: RemoteHandle,
    /// Current window start (advances window by window).
    off: u64,
    /// Bytes still owed to the response body.
    remaining: u64,
    state: RangeBodyState,
}

/// Lifecycle of a [`RangeBody`]: between windows, the `open_range` call
/// in flight, its frame stream flowing, or terminal after an error /
/// full length served.
enum RangeBodyState {
    /// No window in flight: the next poll opens one (or ends at zero
    /// `remaining`).
    Idle,
    /// The `open_range` future (owned `handle`/`transport`, so the boxed
    /// future is `'static`).
    Opening(Pin<Box<dyn std::future::Future<Output = Result<ByteStream, StorageError>> + Send>>),
    /// The current window's frames, plus how much it was asked for and
    /// has served.
    Streaming {
        stream: ByteStream,
        window_len: u64,
        served: u64,
    },
    /// After an error frame (including a short window — stream-M1) or
    /// the full length: nothing more, ever.
    Done,
}

impl RangeBody {
    fn new(transport: Arc<dyn CloudTransport>, handle: RemoteHandle, off: u64, len: u64) -> Self {
        Self {
            transport,
            handle,
            off,
            remaining: len,
            state: RangeBodyState::Idle,
        }
    }
}

impl Stream for RangeBody {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // Take the state out (the boxed futures/streams move with it)
            // so the arms can install the successor state free of borrow
            // conflicts; `Done` is the between-steps placeholder.
            match std::mem::replace(&mut this.state, RangeBodyState::Done) {
                RangeBodyState::Done => return Poll::Ready(None),
                RangeBodyState::Idle => {
                    if this.remaining == 0 {
                        return Poll::Ready(None);
                    }
                    let len = this.remaining.min(WEB_STREAM_WINDOW);
                    let transport = Arc::clone(&this.transport);
                    let handle = this.handle.clone();
                    let off = this.off;
                    this.state = RangeBodyState::Opening(Box::pin(async move {
                        transport.open_range(&handle, off, len).await
                    }));
                }
                RangeBodyState::Opening(mut future) => match future.as_mut().poll(cx) {
                    Poll::Pending => {
                        this.state = RangeBodyState::Opening(future);
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(stream)) => {
                        this.state = RangeBodyState::Streaming {
                            stream,
                            window_len: this.remaining.min(WEB_STREAM_WINDOW),
                            served: 0,
                        };
                    }
                    Poll::Ready(Err(error)) => {
                        return Poll::Ready(Some(Err(error)));
                    }
                },
                RangeBodyState::Streaming {
                    mut stream,
                    window_len,
                    served,
                } => match stream.as_mut().poll_next(cx) {
                    Poll::Pending => {
                        this.state = RangeBodyState::Streaming {
                            stream,
                            window_len,
                            served,
                        };
                        return Poll::Pending;
                    }
                    Poll::Ready(Some(Ok(frame))) => {
                        this.state = RangeBodyState::Streaming {
                            stream,
                            window_len,
                            served: served + frame.len() as u64,
                        };
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Poll::Ready(Some(Err(error))) => {
                        return Poll::Ready(Some(Err(error)));
                    }
                    Poll::Ready(None) => {
                        if served < window_len {
                            // stream-M1 (review RB4): a short window is a
                            // TRUNCATION, not a graceful end — the inner
                            // transport just proved it cannot serve what
                            // its own handle promised. Fail the body with
                            // an error frame (the enc_stream face's
                            // "short read = error" semantics) instead of
                            // ending cleanly at the short window, which
                            // would pass truncation off as the whole
                            // object.
                            return Poll::Ready(Some(Err(StorageError::Unavailable(format!(
                                "range window short: inner transport returned {served} \
                                     of {window_len} bytes (object truncated?)"
                            )))));
                        }
                        this.off += served;
                        this.remaining = this.remaining.saturating_sub(served);
                        this.state = RangeBodyState::Idle;
                    }
                },
            }
        }
    }
}

/// The R-5 fallback body of [`api_download`] (verbatim pre-SR2
/// behavior): hydrate through the VFS, read the whole local copy and
/// answer through [`download_response`].
async fn hydrate_download(
    vfs: &Vfs,
    rel: &RelPath,
    filename: &str,
    range: Option<&str>,
) -> Response {
    match vfs.hydrate(rel).await {
        Ok(path) => match std::fs::read(&path) {
            Ok(bytes) => download_response(filename, bytes, range),
            Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(VfsError::NotFound(_)) | Err(VfsError::IsDirectory(_)) => download_not_found(),
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// The frozen 404 body of the download routes (Python's verbatim
/// plain-text answer).
fn download_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        content_response(
            "text/plain; charset=utf-8",
            DOWNLOAD_NOT_FOUND.as_bytes().to_vec(),
        ),
    )
        .into_response()
}

/// `GET /api/download/{filename}`: streams ranges straight off the
/// remote when the row admits it (SR2 / K36 — a plaintext row on a
/// `range_read` transport with a non-zero size and no cached copy, WF0
/// cache-first), otherwise hydrates through the VFS and answers the local
/// copy (R-5 fallback: encrypted rows, range-incapable transports, 0-byte
/// rows). A single-range
/// `Range` header is honored on both faces; sub-paths resolve as their
/// virtual RelPath; a missing row, a directory or an unusable path all
/// land on Python's verbatim 404 body, and other failures surface as
/// 500 errors. Multi-volume mode routes by `?volume=` (K23) — the
/// volume resolution happens after the path validation so an unusable
/// path keeps the frozen 404 body.
async fn api_download(
    State(state): State<AppState>,
    Path(filename): Path<String>,
    request: Request,
) -> Response {
    let normalized = filename.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    let Ok(rel) = RelPath::new(&format!("/{trimmed}")) else {
        return (
            StatusCode::NOT_FOUND,
            content_response(
                "text/plain; charset=utf-8",
                DOWNLOAD_NOT_FOUND.as_bytes().to_vec(),
            ),
        )
            .into_response();
    };
    let volume = match state.resolve(request.uri().query()) {
        Ok(volume) => volume,
        Err(response) => return response,
    };
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    match volume.vfs.open_read(&rel).await {
        Ok(StreamSource::Stream {
            handle,
            total_size,
            transport,
        }) => streaming_download_response(&filename, handle, total_size, transport, range),
        // The fallback signal and every error keep the pre-SR2 path:
        // hydrate (with its own NotFound/IsDirectory mapping) or the
        // frozen 404/500 answers.
        Ok(StreamSource::Hydrate) => hydrate_download(&volume.vfs, &rel, &filename, range).await,
        Err(VfsError::NotFound(_)) | Err(VfsError::IsDirectory(_)) => download_not_found(),
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// Percent-decodes one query component: `%XX` escapes become their
/// bytes, everything else passes through verbatim — including `+`,
/// which never means space here (path components are not form fields,
/// so a literal `+` in a filename survives). A malformed escape stays
/// literal; the component is data, not a parsed structure.
fn percent_decode(component: &str) -> String {
    fn hex_digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = component.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// First value of `key` in a raw query string (percent-decoded).
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (percent_decode(name) == key).then(|| percent_decode(value))
    })
}

/// Normalizes a `?path=` value into a canonical virtual path: the
/// leading `/` is guaranteed, `\` folds onto `/`, and [`RelPath`]
/// validates the segments. `None` is segment pollution (`..`, `.`,
/// empty segments) — 400 territory.
fn normalize_list_path(raw: &str) -> Option<String> {
    let folded = raw.replace('\\', "/");
    let slashed = if folded.starts_with('/') {
        folded
    } else {
        format!("/{folded}")
    };
    RelPath::new(&slashed)
        .map(|rel| rel.as_str().to_string())
        .ok()
}

/// `GET /api/list?path=/docs`: one directory's direct children as a
/// flat array of `/api/files`-shaped entries (`updated_at DESC`, like
/// `/api/files`) — the frontend assembles the tree. A missing or empty
/// `path` is the root. Existence semantics: a directory with neither a
/// row of its own nor any child 404s, while an existing-but-empty one
/// answers 200 with `entries: []` (the root of a fully empty drive has
/// neither, so it 404s under the same rule until anything exists).
/// Multi-volume mode routes by `?volume=` (K23) — the path validation
/// keeps its frozen order ahead of the volume resolution.
async fn api_list(State(state): State<AppState>, request: Request) -> Response {
    let query = request.uri().query().unwrap_or("");
    let path = match query_param(query, "path") {
        None => "/".to_string(),
        Some(raw) => match normalize_list_path(&raw) {
            Some(path) => path,
            None => return error_json(StatusCode::BAD_REQUEST, "Invalid path"),
        },
    };
    let volume = match state.resolve(Some(query)) {
        Ok(volume) => volume,
        Err(response) => return response,
    };
    let db = volume.vfs.db();
    let mut entries = match db.list_dir(&path) {
        Ok(entries) => entries,
        Err(error) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    if entries.is_empty() && db.get_file(&path).ok().flatten().is_none() {
        return error_json(StatusCode::NOT_FOUND, "Directory not found");
    }
    // `list_dir` orders directories-first/name-ascending; the route
    // contract orders `updated_at DESC` like `/api/files`.
    entries.sort_by(|a, b| {
        b.updated_at
            .unwrap_or_default()
            .total_cmp(&a.updated_at.unwrap_or_default())
    });
    Json(serde_json::json!({
        "path": path,
        "entries": serde_json::Value::Array(entries.iter().map(file_row_json).collect()),
    }))
    .into_response()
}

/// `GET /api/queue`: the four upload-queue counters
/// ([`Vfs::queue_stats`]) plus the DB pending-uploads tally
/// (`get_stats().pending_uploads`). Multi-volume mode routes by
/// `?volume=` (K23).
async fn api_queue(State(state): State<AppState>, uri: Uri) -> Response {
    let volume = match state.resolve(uri.query()) {
        Ok(volume) => volume,
        Err(response) => return response,
    };
    let stats = volume.vfs.queue_stats();
    let pending = match volume.vfs.db().get_stats() {
        Ok(stats) => stats.pending_uploads,
        Err(error) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    Json(serde_json::json!({
        "enqueued": stats.enqueued,
        "succeeded": stats.succeeded,
        "degraded": stats.degraded,
        "retries": stats.retries,
        "pending": pending,
    }))
    .into_response()
}

// ------------------------------------------- MV3: the registry routes (K24) ---

/// `GET /api/volumes` (multi-volume mode only): the registry listing.
/// The numbers are each volume's OWN db metadata (`get_stats`) — never
/// a backend scan (the PCFS Stats anti-lesson); a failed volume carries
/// its reason and null numbers instead of zeros (there is no db to
/// read, and a zero would read as "empty drive"), and a db read failure
/// on a running volume degrades that volume's numbers to null rather
/// than failing the whole listing (the K22 spirit: one broken volume
/// must not hide its siblings).
async fn api_volumes(State(state): State<AppState>) -> Response {
    let volumes = match &state {
        AppState::Multi { volumes, .. } => volumes.snapshot(),
        AppState::Single { .. } => {
            // Unreachable through the multi router; the single-volume
            // table never registers this route (its unknown-route 404
            // IS the frozen behavior).
            return error_json(
                StatusCode::NOT_FOUND,
                "no volume registry: this dashboard serves a single volume",
            );
        }
    };
    Json(serde_json::Value::Array(
        volumes.iter().map(volume_summary_json).collect(),
    ))
    .into_response()
}

/// One `/api/volumes` row: the registry identity plus the db metadata
/// numbers (see [`api_volumes`]) and the queue's outstanding upload
/// count (the same `pending=` observable LIST reports — the §4 drain
/// aid, surfaced for the management page's Pending column; a failed
/// volume has no queue, so its value is `null`, not 0).
fn volume_summary_json(entry: &VolumeUiEntry) -> serde_json::Value {
    let (total_files, total_bytes) = match (&entry.status, &entry.vfs) {
        (VolumeUiStatus::Running, Some(vfs)) => match vfs.db().get_stats() {
            Ok(stats) => (Some(stats.total_files), Some(stats.total_bytes)),
            Err(_db_read_failed) => (None, None),
        },
        _ => (None, None),
    };
    let pending = entry
        .vfs
        .as_ref()
        .map(|vfs| vfs.queue_stats().outstanding());
    let mut body = serde_json::json!({
        "name": entry.name,
        "backend": entry.config.backend,
        "volume_id": entry.config.volume,
        "drive_letter": entry.config.drive_letter,
        "webdav_url": entry.config.webdav_url,
        "status": entry.status.as_str(),
        "pending": pending,
        "quota_used": entry.config.quota.as_ref().map(|quota| quota.used),
        "quota_total": entry.config.quota.as_ref().and_then(|quota| quota.total),
        "total_files": total_files,
        "total_bytes": total_bytes,
    });
    if let VolumeUiStatus::Failed { reason } = &entry.status {
        body["status_reason"] = serde_json::Value::String(reason.clone());
    }
    body
}

/// `GET /api/stats/summary` (multi-volume mode only): the cross-volume
/// aggregate for the dashboard's summary card — Σ files / bytes / dirs
/// / uploaded / pending over the RUNNING volumes' dbs plus the quota
/// sums over their boot snapshots. A distinct shape on purpose (the
/// frozen 16-key `/api/stats` contract stays a per-volume answer); a
/// db read failure on one volume skips its contribution rather than
/// failing the aggregate.
async fn api_stats_summary(State(state): State<AppState>) -> Response {
    let volumes = match &state {
        AppState::Multi { volumes, .. } => volumes.snapshot(),
        AppState::Single { .. } => {
            return error_json(
                StatusCode::NOT_FOUND,
                "no volume registry: this dashboard serves a single volume",
            )
        }
    };
    let mut running = 0u64;
    let (mut total_files, mut total_bytes, mut total_dirs) = (0i64, 0i64, 0i64);
    let (mut uploaded_files, mut pending_uploads) = (0i64, 0i64);
    let mut quota_used: Option<u64> = None;
    let mut quota_total: Option<u64> = None;
    for entry in volumes.iter() {
        if let (VolumeUiStatus::Running, Some(vfs)) = (&entry.status, &entry.vfs) {
            running += 1;
            if let Ok(stats) = vfs.db().get_stats() {
                total_files += stats.total_files;
                total_bytes += stats.total_bytes;
                total_dirs += stats.total_dirs;
                uploaded_files += stats.uploaded_files;
                pending_uploads += stats.pending_uploads;
            }
            if let Some(quota) = &entry.config.quota {
                *quota_used.get_or_insert(0) += quota.used;
                if let Some(total) = quota.total {
                    *quota_total.get_or_insert(0) += total;
                }
            }
        }
    }
    Json(serde_json::json!({
        "volumes": volumes.len(),
        "running": running,
        "total_files": total_files,
        "total_bytes": total_bytes,
        "total_dirs": total_dirs,
        "uploaded_files": uploaded_files,
        "pending_uploads": pending_uploads,
        "quota_used": quota_used,
        "quota_total": quota_total,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stream-M1 (review RB4): a short window is an ERROR, not a clean
    /// end. An inner transport whose handle promises [`PROMISED`] bytes
    /// but serves fewer must surface as an error frame — the same
    /// "short read = error" semantics the encrypted-stream face pins
    /// (enc_stream's span check) — instead of silently truncating the
    /// body at the short window. Today's drivers self-clamp, so only a
    /// misbehaving future driver can trigger this; the test injects
    /// exactly that (a remote holding 40 bytes behind a 100-byte handle).
    #[test]
    fn range_body_short_window_is_an_error_not_a_silent_end() {
        const PROMISED: u64 = 100;
        const SERVED: u64 = 40;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        rt.block_on(async move {
            let mock = Arc::new(cloudkit_core::transport::mock::MockTransport::new());
            mock.connect().await.expect("connect the mock");
            // 40 real bytes behind a handle that promises 100: every
            // window the body opens comes back short.
            let dir = tempfile::tempdir().expect("seed scratch dir");
            let local_path = dir.path().join("short.bin");
            std::fs::write(&local_path, vec![b'x'; SERVED as usize]).expect("seed bytes");
            let receipt = mock
                .upload(&cloudkit_core::transport::UploadJob {
                    rel_path: RelPath::new("/short.bin").expect("valid rel path"),
                    local_path,
                    size: SERVED,
                    chunk_count: 1,
                    chunk_size: SERVED,
                })
                .await
                .expect("seed upload");
            let handle = RemoteHandle {
                first_msg_id: receipt.first_msg_id,
                chunk_msg_ids: receipt.chunk_msg_ids,
                total_size: PROMISED,
                path: None,
            };
            let transport: Arc<dyn CloudTransport> = mock;
            let mut body = RangeBody::new(transport, handle, 0, PROMISED);

            async fn next_item(body: &mut RangeBody) -> Option<Result<Bytes, StorageError>> {
                std::future::poll_fn(|cx| Pin::new(&mut *body).poll_next(cx)).await
            }

            // ① First poll: the window's single frame — the mock's whole
            //    40-byte store, short of the promised window.
            let first = next_item(&mut body).await;
            let frame = match first {
                Some(Ok(frame)) => frame,
                other => panic!("expected the window's first frame, got {other:?}"),
            };
            assert_eq!(
                frame.len() as u64,
                SERVED,
                "the mock serves exactly its stored bytes, short of the window"
            );

            // ② Second poll: the short window must surface as an ERROR
            //    frame (never a silent `None` — that would truncate the
            //    response body at the short window with a clean end).
            let second = next_item(&mut body).await;
            assert!(
                matches!(second, Some(Err(StorageError::Unavailable(_)))),
                "a short window must end the body with an error frame, got {second:?}"
            );
        });
    }
}
