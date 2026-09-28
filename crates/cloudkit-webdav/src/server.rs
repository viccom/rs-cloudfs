//! Hyper assembly for the WebDAV service.
//!
//! dav-server ships only the framework-agnostic [`DavHandler`]; the
//! hyper listener, accept loop and per-connection tasks live here
//! (mirroring the crate's own `examples/hyper.rs`). The locked-down
//! assembly:
//!
//! - `FakeLs` locksystem — Windows Explorer refuses to mount a share
//!   that does not answer LOCK, and dav-server only advertises
//!   LOCK/UNLOCK in OPTIONS when a locksystem is installed;
//! - an explicit method set (PROPFIND/PROPPATCH/GET/HEAD/PUT/DELETE/
//!   MKCOL/MOVE/COPY/OPTIONS/LOCK/UNLOCK — COPY rides the adapter's
//!   built-in `NotImplemented`, 501, by design: Explorer drag-copy goes
//!   through PUT; PROPPATCH answers 207 so MiniRedir does not roll
//!   Explorer copies back);
//! - no auth and no principal negotiation — loopback only is the
//!   contract (production binds 127.0.0.1:8485, compat contract 1).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

use dav_server::body::Body as DavBody;
use dav_server::fakels::FakeLs;
use dav_server::{DavHandler, DavMethod, DavMethodSet};
use http::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::pin::pin;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::CyDriveFs;

/// Errors from assembling the WebDAV service.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The listener could not bind the requested address.
    #[error("bind {addr}: {source}")]
    Bind {
        /// The requested bind address.
        addr: SocketAddr,
        /// The underlying io error.
        source: std::io::Error,
    },
}

/// The running WebDAV service: a hyper HTTP/1.1 listener dispatching
/// into a [`DavHandler`] over [`CyDriveFs`].
///
/// Clone-free handle; `shutdown` is idempotent and drains gracefully
/// (the listener stops accepting and live connections are allowed to
/// finish their in-flight request).
pub struct WebDavServer {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// The dynamic volume table behind the multi-volume dispatch (Phase 3.6
/// / RV1, K51): the `Arc<RwLock<ordered volume table>>` shared handle —
/// one entry per registered volume, in insertion order, so the caller
/// can insert/remove volumes while the server keeps running; every
/// request re-reads the table under a read lock, so a removal 404s and
/// an insertion routes at once, on the same port (no listener restart,
/// no route rebuild). Entries carry the volume's already-built
/// [`DavHandler`] (own FakeLs), so a volume's lock state lives exactly
/// as long as its registration — the same lifetime the pre-dynamic
/// startup table had.
///
/// Clone shares the table; the critical sections are synchronous and
/// short (find + clone the handler), so the lock never spans an await.
/// Removal while a request is in flight lets that request drain on its
/// cloned handler (the shutdown semantics).
#[derive(Clone)]
pub struct RegistryHandle {
    volumes: Arc<RwLock<Vec<(String, DavHandler)>>>,
}

impl RegistryHandle {
    /// Builds the table with the given starting volumes (each volume's
    /// handler is assembled here, one per volume).
    pub fn new(volumes: Vec<(String, CyDriveFs)>) -> Self {
        let handle = Self {
            volumes: Arc::new(RwLock::new(Vec::new())),
        };
        for (name, fs) in volumes {
            handle.insert(name, fs);
        }
        handle
    }

    /// Registers a volume under `name` (appending to the table order)
    /// and assembles its handler; a duplicate name is the caller's
    /// contract error (RV2's ADD refuses it before reaching here).
    pub fn insert(&self, name: impl Into<String>, fs: CyDriveFs) {
        self.write().push((name.into(), volume_handler(fs)));
    }

    /// Removes the volume registered under `name`; `true` when it was
    /// registered. Its handler (and lock state) drops with the entry.
    /// Order-preserving: the table is the ordered volume table (K51),
    /// and volume counts are tiny — the O(n) shift is nothing.
    pub fn remove(&self, name: &str) -> bool {
        let mut volumes = self.write();
        match volumes
            .iter()
            .position(|(registered, _)| registered == name)
        {
            Some(position) => {
                volumes.remove(position);
                true
            }
            None => false,
        }
    }

    /// Whether no volume is registered (the caller may skip the bind).
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// One volume's handler by name (a clone — the table's entry stays
    /// shared and the read lock is released before the request runs).
    fn lookup(&self, name: &str) -> Option<DavHandler> {
        self.read()
            .iter()
            .find(|(registered, _)| registered == name)
            .map(|(_, handler)| handler.clone())
    }

    /// The table read lock, poisoned-lock recovery per the code-style
    /// norm (a panicked holder must not take the dispatcher down).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<(String, DavHandler)>> {
        self.volumes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The table write lock (same recovery norm).
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<(String, DavHandler)>> {
        self.volumes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl WebDavServer {
    /// Binds `addr` and serves `fs`. Loopback no-auth is the contract
    /// (production binds 127.0.0.1:8485); tests bind 127.0.0.1:0.
    pub async fn serve(fs: CyDriveFs, addr: SocketAddr) -> Result<Self, ServerError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { addr, source })?;

        let (shutdown, rx) = watch::channel(false);
        let task = tokio::task::spawn(accept_loop(
            listener,
            Dispatcher::Single(volume_handler(fs)),
            rx,
        ));

        Ok(Self {
            addr,
            shutdown,
            task: Mutex::new(Some(task)),
        })
    }

    /// Binds `addr` and serves every volume of `registry` on that ONE
    /// port (Phase 2.5 / K20; dynamic since RV1 / K51): each request
    /// dispatches by its `/vol/<name>/` URL prefix through a READ of the
    /// shared table, so volumes the caller registers later are routable
    /// at once and removed ones 404 at once — no listener restart, no
    /// route rebuild. The prefix is a process-level concept — the
    /// volume's [`CyDriveFs`] only ever sees the stripped path (R1).
    /// `/vol/<name>` and `/vol/<name>/` both address the volume root;
    /// anything else (no prefix, `/vol/`, an unregistered volume)
    /// answers 404 without touching any volume.
    pub async fn serve_volumes(
        volumes: RegistryHandle,
        addr: SocketAddr,
    ) -> Result<Self, ServerError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { addr, source })?;

        let router = VolumeRouter { volumes };
        let (shutdown, rx) = watch::channel(false);
        let task = tokio::task::spawn(accept_loop(listener, Dispatcher::Volumes(router), rx));
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

    /// Graceful stop; idempotent. Signals the accept loop and every
    /// live connection, then waits for the accept loop to finish.
    pub async fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        let task = self.task.lock().expect("server task lock").take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

/// Accepts connections until `shutdown` fires, serving each on its own
/// task with graceful connection-level shutdown. `dispatcher` decides,
/// PER REQUEST, which [`DavHandler`] a request reaches — the
/// `service_fn` closure runs once per request on a keep-alive
/// connection, so a single connection may address different volumes.
async fn accept_loop(
    listener: TcpListener,
    dispatcher: Dispatcher,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let (stream, _peer) = tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(_) => break,
            },
        };
        let dispatcher = dispatcher.clone();
        let mut conn_shutdown = shutdown.clone();
        tokio::task::spawn(async move {
            let service = service_fn(move |req| {
                let dispatcher = dispatcher.clone();
                async move { Ok::<_, Infallible>(dispatcher.dispatch(req).await) }
            });
            // hyper 1.x graceful shutdown: on the signal, stop
            // keep-alive and let the in-flight request finish.
            let mut conn =
                pin!(http1::Builder::new().serve_connection(TokioIo::new(stream), service));
            tokio::select! {
                _ = conn_shutdown.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.as_mut().await;
                }
                _ = conn.as_mut() => {}
            }
        });
    }
}

/// The per-request routing decision shared by both serving modes: the
/// single-volume lock-down stays one `DavHandler` at the root; the
/// multi-volume mode maps `/vol/<name>/...` to that volume's handler
/// (Phase 2.5 / K20).
#[derive(Clone)]
enum Dispatcher {
    /// The single-volume mode: every request reaches the one handler.
    Single(DavHandler),
    /// The multi-volume mode: per-volume handlers behind `/vol/<name>/`.
    Volumes(VolumeRouter),
}

impl Dispatcher {
    /// Routes one request to its [`DavHandler`] and runs it.
    async fn dispatch(&self, req: Request<Incoming>) -> http::Response<DavBody> {
        match self {
            Dispatcher::Single(handler) => handler.handle(req).await,
            Dispatcher::Volumes(router) => router.dispatch(req).await,
        }
    }
}

/// The `/vol/<name>/` dispatch (Phase 2.5 / K20; dynamic since RV1 /
/// K51): a read of the shared [`RegistryHandle`] per request resolves
/// the volume's handler — each with its own FakeLs and the same
/// locked-down method set as the single-volume mode. Cloning the router
/// clones the handle (one Arc), never the table.
#[derive(Clone)]
struct VolumeRouter {
    volumes: RegistryHandle,
}

impl VolumeRouter {
    /// Strips the `/vol/<name>` segment, rebuilds the request URI
    /// (query preserved) and hands the request to that volume's
    /// handler; an unroutable path (or an unregistered volume) answers
    /// 404 without touching any volume.
    ///
    /// **`Destination` rewrite (BUG-FIX, Phase 7 真机 e2e 2026-09-24)**:
    /// the `Destination` header names an absolute URI in the SAME
    /// external namespace as the request line, so it carries the
    /// `/vol/<name>` prefix too. Rewriting only the URI left dav-server
    /// with two disagreeing namespaces — its internal path was
    /// `/dst.txt` while Destination still said `/vol/a/dst.txt` — and
    /// `has_parent(&dest)` failed → **409 Conflict on every MOVE under a
    /// mounted volume** (真机实测：多卷 MOVE 恒 409；单卷因无前缀而正常).
    /// The header must therefore be stripped with the same prefix, and
    /// only when it actually belongs to this volume: a Destination
    /// addressing a different volume (or none) is left alone so the
    /// handler's own validation rejects it rather than us silently
    /// retargeting a cross-volume move.
    async fn dispatch(&self, req: Request<Incoming>) -> http::Response<DavBody> {
        let path = req.uri().path().to_owned();
        let Some((name, tail)) = split_volume_segment(&path) else {
            return not_found();
        };
        // The read lock spans only the lookup; the request runs on the
        // cloned handler after the lock is released.
        let Some(handler) = self.volumes.lookup(name) else {
            return not_found();
        };
        let (mut parts, body) = req.into_parts();
        // The tail and the query are slices of an already-valid request
        // URI, so the recombination parses; the 404 arm is defensive.
        let path_and_query = match parts.uri.query() {
            Some(query) => format!("{tail}?{query}"),
            None => tail.to_string(),
        };
        let Ok(uri) = path_and_query.parse::<http::Uri>() else {
            return not_found();
        };
        // Same-prefix Destination rewrite (see the doc comment): rewrite
        // the header's path component only, preserving its scheme/authority
        // (dav-server validates the host itself). A header whose path does
        // not sit under `/vol/<name>/` is cross-volume or foreign — leave
        // it untouched for the handler to judge.
        let volume_prefix = format!("/vol/{name}");
        if let Some(destination) = parts.headers.get(DESTINATION_HEADER) {
            let Ok(raw) = destination.to_str() else {
                return not_found();
            };
            // A foreign/cross-volume Destination stays untouched (None).
            if let Some(rewritten) = strip_destination_prefix(raw, &volume_prefix) {
                let Ok(value) = http::HeaderValue::from_str(&rewritten) else {
                    return not_found();
                };
                parts.headers.insert(DESTINATION_HEADER, value);
            }
        }
        parts.uri = uri;
        handler.handle(Request::from_parts(parts, body)).await
    }
}

/// The WebDAV `Destination` request header (RFC 4918 §10.3) — a
/// non-standard addition to `http::header`, which has no constant for it.
const DESTINATION_HEADER: &str = "destination";

/// Rewrites a `Destination` header value onto the volume-relative
/// namespace: `/vol/<name>/rest` → `/rest`, keeping the scheme and
/// authority intact (`http://host:port/vol/a/x` →
/// `http://host:port/x`). Origin-form values (no scheme, sent by some
/// clients) are handled too.
///
/// Returns `None` when the header does not address this volume — a path
/// that is not `/vol/<name>` (or `/vol/<name>/…`), or a scheme+authority
/// value with no path at all. Leaving it untouched lets the handler
/// report the real error instead of the router silently retargeting a
/// cross-volume move.
fn strip_destination_prefix(raw: &str, volume_prefix: &str) -> Option<String> {
    // Split the authority off an absolute URI; an origin-form value keeps
    // an empty `head` and is treated as a bare path. Search starts AFTER
    // the `://` separator (searching from the scheme end would land on the
    // separator's own slash).
    let (head, path) = match raw.find("://") {
        Some(scheme_end) => {
            let after_separator = scheme_end + 3;
            // `?` on the Option: scheme://authority with no path has
            // nothing to rewrite (returns None from the helper).
            let offset = raw[after_separator..].find('/')?;
            raw.split_at(after_separator + offset)
        }
        None => ("", raw),
    };
    debug_assert!(path.starts_with('/') || head.is_empty());
    let rest = if path == volume_prefix {
        String::from("/")
    } else {
        format!("/{}", path.strip_prefix(&format!("{volume_prefix}/"))?)
    };
    Some(format!("{head}{rest}"))
}

/// The plain 404 for unrouted requests (no volume touched).
fn not_found() -> http::Response<DavBody> {
    http::Response::builder()
        .status(http::StatusCode::NOT_FOUND)
        .body(DavBody::empty())
        .expect("static 404 response")
}

/// Splits `/vol/<name>[/<rest>]` into the volume name and the
/// volume-relative path (always `/`-rooted: `/vol/a` and `/vol/a/` both
/// yield the volume root `/`). `None` — the 404 case — for anything
/// else: no `/vol/` prefix, a bare `/vol` or `/vol/`. The tail keeps the
/// raw percent-encoded form; [`dav_server::davpath::DavPath`] decodes
/// per segment, so a prefix split must never decode first.
fn split_volume_segment(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/vol/")?;
    if rest.is_empty() {
        return None;
    }
    let (name, tail) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    // An empty name segment (`/vol//x`) is not a volume reference.
    if name.is_empty() {
        return None;
    }
    Some((name, tail))
}

/// The locked-down per-volume (and single-volume) handler: same builder
/// chain the crate has always assembled.
fn volume_handler(fs: CyDriveFs) -> DavHandler {
    DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(FakeLs::new())
        .methods(method_set())
        .principal("cydrive")
        .build_handler()
}

/// The locked-down method set. COPY stays allowed so the adapter's
/// `NotImplemented` (501) answers it, matching the design doc's
/// "Explorer drag-copy goes through PUT". PROPPATCH is required by
/// Windows MiniRedir: it closes every Explorer copy with an mtime
/// -preserving PROPPATCH and rolls the copy back (DELETE) on anything
/// but a 2xx/207 answer.
fn method_set() -> DavMethodSet {
    let mut set = DavMethodSet::none();
    for method in [
        DavMethod::PropFind,
        DavMethod::PropPatch,
        DavMethod::Get,
        DavMethod::Head,
        DavMethod::Put,
        DavMethod::Delete,
        DavMethod::Options,
        DavMethod::MkCol,
        DavMethod::Move,
        DavMethod::Copy,
        DavMethod::Lock,
        DavMethod::Unlock,
    ] {
        set.add(method);
    }
    set
}

#[cfg(test)]
mod tests {
    use super::{split_volume_segment, strip_destination_prefix};

    #[test]
    fn splits_legal_volume_paths() {
        assert_eq!(split_volume_segment("/vol/a"), Some(("a", "/")));
        assert_eq!(split_volume_segment("/vol/a/"), Some(("a", "/")));
        assert_eq!(split_volume_segment("/vol/a/x.txt"), Some(("a", "/x.txt")));
        assert_eq!(split_volume_segment("/vol/a/d/f"), Some(("a", "/d/f")));
        // A longer volume name must not fall apart at its first letter
        // (`/vol/ab` is volume `ab`, not volume `a` plus `b/x`).
        assert_eq!(split_volume_segment("/vol/ab/x"), Some(("ab", "/x")));
        // Percent-encoded tails pass through verbatim (DavPath decodes).
        assert_eq!(
            split_volume_segment("/vol/a/%E4%B8%AD.txt"),
            Some(("a", "/%E4%B8%AD.txt"))
        );
    }

    #[test]
    fn unroutable_paths_are_none() {
        assert_eq!(split_volume_segment("/"), None);
        assert_eq!(split_volume_segment("/x.txt"), None);
        assert_eq!(split_volume_segment("/volume/a/x"), None);
        assert_eq!(split_volume_segment("/vol"), None);
        assert_eq!(split_volume_segment("/vol/"), None);
        // `/vol//x` has an empty volume name segment — not a volume.
        assert_eq!(split_volume_segment("/vol//x"), None);
    }

    /// BUG-FIX (Phase 7 真机 e2e): the `Destination` header carries the
    /// same `/vol/<name>` prefix as the request line, so the router must
    /// strip it from BOTH. These are the cases the fix had to get right —
    /// the absolute-URI split in particular (搜索必须从 `://` 之后开始，
    /// 否则落在分隔符自身的斜杠上)。
    #[test]
    fn destination_prefix_strips_same_volume_only() {
        let p = "/vol/a";
        // Absolute URI: scheme+authority preserved, path rewritten.
        assert_eq!(
            strip_destination_prefix("http://127.0.0.1:8485/vol/a/dst.txt", p).as_deref(),
            Some("http://127.0.0.1:8485/dst.txt")
        );
        // Deep tail keeps every remaining segment.
        assert_eq!(
            strip_destination_prefix("http://h/vol/a/d/e.txt", p).as_deref(),
            Some("http://h/d/e.txt")
        );
        // The volume root itself maps to `/`.
        assert_eq!(
            strip_destination_prefix("http://h/vol/a", p).as_deref(),
            Some("http://h/")
        );
        // Origin-form value (no scheme) is handled too.
        assert_eq!(
            strip_destination_prefix("/vol/a/dst.txt", p).as_deref(),
            Some("/dst.txt")
        );
        // Cross-volume / foreign targets are left ALONE (None) so the
        // handler reports the real error rather than silently retargeting.
        assert_eq!(strip_destination_prefix("http://h/vol/b/x", p), None);
        assert_eq!(strip_destination_prefix("http://h/other/x", p), None);
        // Prefix must match on a segment boundary (`/vol/ab` is not `/vol/a`).
        assert_eq!(strip_destination_prefix("http://h/vol/ab/x", p), None);
        // scheme://authority with no path — nothing to rewrite.
        assert_eq!(strip_destination_prefix("http://h", p), None);
    }
}
