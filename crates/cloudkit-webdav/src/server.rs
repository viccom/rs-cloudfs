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

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Mutex;

use dav_server::fakels::FakeLs;
use dav_server::{DavHandler, DavMethod, DavMethodSet};
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

        let handler = DavHandler::builder()
            .filesystem(Box::new(fs))
            .locksystem(FakeLs::new())
            .methods(method_set())
            .principal("cydrive")
            .build_handler();

        let (shutdown, rx) = watch::channel(false);
        let task = tokio::task::spawn(accept_loop(listener, handler, rx));

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
/// task with graceful connection-level shutdown.
async fn accept_loop(
    listener: TcpListener,
    handler: DavHandler,
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
        let handler = handler.clone();
        let mut conn_shutdown = shutdown.clone();
        tokio::task::spawn(async move {
            let service = service_fn(move |req| {
                let handler = handler.clone();
                async move { Ok::<_, Infallible>(handler.handle(req).await) }
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
