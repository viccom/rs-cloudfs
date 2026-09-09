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
//!   contract (production binds 127.0.0.1:8080, compat contract 1).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Mutex;

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

impl WebDavServer {
    /// Binds `addr` and serves `fs`. Loopback no-auth is the contract
    /// (production binds 127.0.0.1:8080); tests bind 127.0.0.1:0.
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

    /// Binds `addr` and serves every volume on that ONE port (Phase 2.5
    /// / K20): each request dispatches by its `/vol/<name>/` URL prefix
    /// to that volume's own [`DavHandler`] (own FakeLs). The prefix is a
    /// process-level concept — the volume's [`CyDriveFs`] only ever sees
    /// the stripped path (R1). `/vol/<name>` and `/vol/<name>/` both
    /// address the volume root; anything else (no prefix, `/vol/`, an
    /// unknown volume) answers 404 without touching any volume.
    pub async fn serve_volumes(
        volumes: Vec<(String, CyDriveFs)>,
        addr: SocketAddr,
    ) -> Result<Self, ServerError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| ServerError::Bind { addr, source })?;

        let router = VolumeRouter::new(volumes);
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

/// The `/vol/<name>/` prefix table (Phase 2.5 / K20): one [`DavHandler`]
/// per volume — each with its own FakeLs and the same locked-down
/// method set as the single-volume mode.
#[derive(Clone)]
struct VolumeRouter {
    handlers: HashMap<String, DavHandler>,
}

impl VolumeRouter {
    /// Builds one handler per volume (`FakeLs` instances stay
    /// per-volume).
    fn new(volumes: Vec<(String, CyDriveFs)>) -> Self {
        Self {
            handlers: volumes
                .into_iter()
                .map(|(name, fs)| (name, volume_handler(fs)))
                .collect(),
        }
    }

    /// Strips the `/vol/<name>` segment, rebuilds the request URI
    /// (query preserved) and hands the request to that volume's
    /// handler; an unroutable path answers 404 without touching any
    /// volume.
    async fn dispatch(&self, req: Request<Incoming>) -> http::Response<DavBody> {
        let path = req.uri().path().to_owned();
        let Some((name, tail)) = split_volume_segment(&path) else {
            return not_found();
        };
        let Some(handler) = self.handlers.get(name) else {
            return not_found();
        };
        let (mut parts, body) = req.into_parts();
        // The tail and the query are slices of an already-valid request
        // URI, so the recombination parses; the 404 arm is defensive.
        let path_and_query = match parts.uri.query() {
            Some(query) => format!("{tail}?{query}"),
            None => tail.to_string(),
        };
        match path_and_query.parse::<http::Uri>() {
            Ok(uri) => {
                parts.uri = uri;
                handler.handle(Request::from_parts(parts, body)).await
            }
            Err(_) => not_found(),
        }
    }
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
    use super::split_volume_segment;

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
}
