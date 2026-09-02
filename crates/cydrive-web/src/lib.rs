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
//!   dashboard fields.
//! - `POST /api/upload` — multipart field `file` staged through
//!   [`Vfs::put`] (accepted ≠ uploaded; the queue uploads async).
//! - `POST /api/delete` — `{"filename": ...}`, a single `delete_file`
//!   call (fixing the Python double-call defect per the design doc).
//! - `GET /api/download/{filename}` — hydrate through the VFS and
//!   answer the bytes with Python's inline disposition; unknown files
//!   get Python's verbatim 404 body.
//!
//! Production binds `127.0.0.1:8088` (the caller's concern); tests bind
//! `127.0.0.1:0`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use rust_embed::RustEmbed;
use tokio::net::TcpListener;
use tokio::sync::watch;

use cydrive_core::database::FileRecord;
use cydrive_core::rel_path::RelPath;
use cydrive_core::vfs::{Vfs, VfsError};

/// Configuration knobs for the dashboard: the source of the
/// `/api/stats` extra fields plus nothing else — binding goes through
/// [`WebUiServer::serve`].
pub struct WebUiConfig {
    /// Windows drive letter reported by `/api/stats` (`"Y:"`).
    pub drive_letter: String,
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
}

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

/// The embedded `static/` tree (copied verbatim from the Python
/// dashboard; zero frontend changes is the acceptance bar).
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
    /// Binds `addr` and serves the dashboard over `vfs`.
    pub async fn serve(
        vfs: Arc<Vfs>,
        cfg: WebUiConfig,
        addr: SocketAddr,
    ) -> Result<Self, WebUiError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| WebUiError::Bind { addr, source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| WebUiError::Bind { addr, source })?;

        let app = router(vfs, cfg);
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

        Ok(Self {
            addr,
            shutdown,
            task: Mutex::new(Some(task)),
        })
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

/// Assembles the route table over the shared state.
fn router(vfs: Arc<Vfs>, cfg: WebUiConfig) -> axum::Router {
    let state = AppState {
        vfs,
        cfg: Arc::new(cfg),
    };
    axum::Router::new()
        .route("/", get(index))
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
        .route("/static/{*path}", get(static_asset))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

/// Shared handler state.
#[derive(Clone)]
struct AppState {
    vfs: Arc<Vfs>,
    cfg: Arc<WebUiConfig>,
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
/// Python ordering).
async fn api_files(State(state): State<AppState>) -> Response {
    match state.vfs.db().list_all_files() {
        Ok(rows) => Json(serde_json::Value::Array(
            rows.iter().map(file_row_json).collect(),
        ))
        .into_response(),
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// Derives the `(host, port)` pair the Python handler glued on from its
/// config fields. The frozen [`WebUiConfig`] carries only the URL, so
/// the pair is parsed back out of it — the URL is always built by the
/// caller as `http://{host}:{port}`, which this reverses (a URL without
/// a port degrades to the HTTP default).
fn split_webdav_url(url: &str) -> (String, u16) {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    match rest.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().unwrap_or(80)),
        None => (rest.to_string(), 80),
    }
}

/// `GET /api/stats`: the DB aggregates plus the six dashboard fields
/// the Python handler added on top.
async fn api_stats(State(state): State<AppState>) -> Response {
    let stats = match state.vfs.db().get_stats() {
        Ok(stats) => stats,
        Err(error) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let (webdav_host, webdav_port) = split_webdav_url(&state.cfg.webdav_url);
    Json(serde_json::json!({
        "total_files": stats.total_files,
        "total_bytes": stats.total_bytes,
        "total_dirs": stats.total_dirs,
        "uploaded_files": stats.uploaded_files,
        "pending_uploads": stats.pending_uploads,
        "drive_letter": state.cfg.drive_letter,
        "webdav_host": webdav_host,
        "webdav_port": webdav_port,
        "webdav_url": state.cfg.webdav_url,
        "chat_id": state.cfg.chat_id,
        "is_configured": state.cfg.is_configured,
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
async fn api_upload(State(state): State<AppState>, mut multipart: Multipart) -> Response {
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
    match state.vfs.put(&rel, &bytes, now_epoch_f64()).await {
        Ok(()) => {
            Json(serde_json::json!({ "success": true, "filename": filename })).into_response()
        }
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// `POST /api/delete` with body `{"filename": ...}`: normalizes the
/// name the way the Python handler did (`/`-prefixed, backslashes
/// folded) but issues exactly **one** `delete_file` call — the Python
/// double call (`delete_file(clean_rel); delete_file(filename)`) was a
/// defect the design doc explicitly fixes here. The remote is never
/// touched (baseline mirror). Python also best-effort removed the
/// cached copy; the frozen [`WebUiConfig`] carries no cache path, so
/// that cleanup is left to the cache LRU (a stale orphan is inert: the
/// row is gone, and a re-upload overwrites it).
async fn api_delete(State(state): State<AppState>, body: Json<serde_json::Value>) -> Response {
    let filename = body
        .0
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if filename.is_empty() {
        return error_json(StatusCode::BAD_REQUEST, "No filename provided");
    }
    let clean_rel = format!("/{}", filename.trim_matches('/').replace('\\', "/"));
    match state.vfs.db().delete_file(&clean_rel) {
        Ok(()) => Json(serde_json::json!({ "success": true, "deleted": filename })).into_response(),
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// A hydrated download: Python's inline disposition, a mime guessed
/// from the name (`mimetypes.guess_type` analog) and the exact bytes
/// (which sets the Content-Length).
fn download_response(filename: &str, bytes: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(filename).first_or_octet_stream();
    let disposition = format!("inline; filename=\"{filename}\"");
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        Body::from(bytes),
    )
        .into_response()
}

/// `GET /api/download/{filename}`: hydrate through the VFS (cache hit
/// or remote pull with LRU eviction) and answer the bytes. Sub-paths
/// resolve as their virtual RelPath; a missing row, a directory or an
/// unusable path all land on Python's verbatim 404 body, and other
/// failures surface as 500 errors.
async fn api_download(State(state): State<AppState>, Path(filename): Path<String>) -> Response {
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
    match state.vfs.hydrate(&rel).await {
        Ok(path) => match std::fs::read(&path) {
            Ok(bytes) => download_response(&filename, bytes),
            Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        },
        Err(VfsError::NotFound(_)) | Err(VfsError::IsDirectory(_)) => (
            StatusCode::NOT_FOUND,
            content_response(
                "text/plain; charset=utf-8",
                DOWNLOAD_NOT_FOUND.as_bytes().to_vec(),
            ),
        )
            .into_response(),
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}
