//! SSE doorbell plumbing: a per-namespace broadcast registry plus the
//! per-subscription pump that turns pushes into
//! `text/event-stream` frames.
//!
//! Doorbell model (approved design): the stream notifies only *that*
//! something changed (`max_version` + the pusher's `client_id` as
//! `origin`) and never carries data — subscribers pull the rows
//! themselves, so push/pull stays the single source of truth.
//!
//! Delivery policy: one `tokio::sync::broadcast` channel per
//! namespace with a small capacity (16). A subscriber that falls
//! behind loses events — a deliberate ruling: a doorbell ring is
//! idempotent, so one missed ring costs nothing; the next ring, or
//! the client's periodic fallback pull, restores freshness.
//!
//! Registry hygiene: entries are created by subscribers and reaped by
//! the pump task once the last receiver of a namespace goes away (a
//! disconnect or server-side stream drop), so an idle server holds no
//! channels. Subscription count is deliberately uncapped — family
//! scale; a cap would need a policy nobody asked for.
//!
//! Proxy interop: responses carry `X-Accel-Buffering: no` because
//! nginx/openresty buffer proxied responses by default, which would
//! batch doorbell frames until the buffer fills. The keepalive
//! comment (default every 20 s, see `config::DEFAULT_HEARTBEAT`)
//! keeps nginx's default 60 s `proxy_read_timeout` from cutting an
//! otherwise-idle connection.

use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::{header, StatusCode};
use axum::response::Response;
use futures_core::Stream;
use tokio::sync::{broadcast, mpsc};

use crate::wire::SubscribeEvent;

/// Per-namespace broadcast capacity. Small on purpose: a lagging
/// subscriber drops events instead of the server buffering unboundedly
/// (doorbell semantics — see the module docs).
const CHANNEL_CAPACITY: usize = 16;

/// Frame pipe depth between the pump task and the response body: a
/// couple of frames of slack so a briefly slow client does not
/// immediately lag the broadcast channel; beyond that, backpressure
/// (and then broadcast lag — an accepted loss) takes over.
const FRAME_PIPE_CAPACITY: usize = 8;

/// The heartbeat comment frame, byte-exact: SSE comment lines keep
/// intermediaries from timing the connection out while carrying no
/// data the client could mistake for a doorbell.
const KEEPALIVE_FRAME: &[u8] = b": keepalive\n\n";

/// Per-namespace broadcast registry shared by the push handler
/// (publisher) and every subscribe pump (receiver).
pub struct EventHub {
    channels: Mutex<HashMap<String, broadcast::Sender<SubscribeEvent>>>,
}

impl EventHub {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Poisoned-lock recovery mirrors [`crate::store::SyncStore`]: the
    /// map is still structurally valid after a panic elsewhere, so
    /// recover instead of cascading.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, broadcast::Sender<SubscribeEvent>>> {
        self.channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attaches a receiver to `ns`'s channel, creating the channel on
    /// first subscription. Returns the sender alongside — the pump
    /// needs it to identify (and reap) its own entry. Runs under the
    /// registry lock together with [`Self::reap_if_idle`], so a
    /// disconnect-cleanup can never race a fresh subscriber out of its
    /// channel.
    fn subscribe(
        &self,
        ns: &str,
    ) -> (
        broadcast::Sender<SubscribeEvent>,
        broadcast::Receiver<SubscribeEvent>,
    ) {
        let mut channels = self.lock();
        let sender = channels
            .entry(ns.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone();
        (sender.clone(), sender.subscribe())
    }

    /// Doorbell every current receiver of `ns`. Called by the push
    /// handler after its transaction committed. A send error means no
    /// receivers (an entry can transiently exist with none, between a
    /// disconnect and its reap) — dropping the event is correct, there
    /// is nobody to ring. Never creates an entry: publishing into an
    /// unsubscribed namespace is a no-op, so idle namespaces cost
    /// nothing.
    pub fn publish(&self, ns: &str, event: &SubscribeEvent) {
        let channels = self.lock();
        if let Some(sender) = channels.get(ns) {
            let _ = sender.send(event.clone());
        }
    }

    /// Receivers currently attached to `ns` (0 for an unknown
    /// namespace) — test observability.
    pub fn receiver_count(&self, ns: &str) -> usize {
        self.lock()
            .get(ns)
            .map_or(0, |sender| sender.receiver_count())
    }

    /// Live namespace entries — the leak check: an entry appears with
    /// the first subscriber of a namespace and disappears with the
    /// last (test observability).
    pub fn channel_count(&self) -> usize {
        self.lock().len()
    }

    /// Removes `ns`'s entry when it is the very channel `sender`
    /// subscribed to and no receiver remains. The identity check
    /// (`same_channel`) makes a late cleanup a no-op if the entry was
    /// already reaped and a newer generation created it.
    fn reap_if_idle(&self, ns: &str, sender: &broadcast::Sender<SubscribeEvent>) {
        let mut channels = self.lock();
        if let Some(current) = channels.get(ns) {
            if current.receiver_count() == 0 && current.same_channel(sender) {
                channels.remove(ns);
            }
        }
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

/// The response body side of a subscription: frames the pump task
/// pushes through an mpsc pipe. `tokio::sync::mpsc::Receiver` exposes
/// `poll_recv`, which is all [`Body::from_stream`] needs; dropping
/// this stream drops the pipe, the pump's `closed()` fires, and the
/// pump reaps its registry entry — that IS the disconnect detection.
struct FramePipe {
    frames: mpsc::Receiver<Bytes>,
}

impl Stream for FramePipe {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // mpsc::Receiver is Unpin, so Pin is transparent here.
        self.get_mut()
            .frames
            .poll_recv(cx)
            .map(|frame| frame.map(Ok))
    }
}

/// Builds the `200 text/event-stream` response for one subscriber and
/// spawns its pump task — the contract's `tokio::select!` loop:
/// broadcast recv (doorbell) vs heartbeat interval vs client
/// disconnect (`frame_tx.closed()` — the body was dropped, so the
/// subscriber is gone). `heartbeat` must be non-zero (the config path
/// enforces >= 1 s; tests inject ~100 ms).
pub(crate) fn sse_response(
    hub: Arc<EventHub>,
    ns: String,
    client_id: Option<String>,
    heartbeat: Duration,
) -> Response {
    let (sender, mut doorbell) = hub.subscribe(&ns);
    let (frame_tx, frames) = mpsc::channel(FRAME_PIPE_CAPACITY);

    let pump_ns = ns.clone();
    let pump_hub = Arc::clone(&hub);
    tokio::spawn(async move {
        // interval_at (not interval): the first keepalive comes a full
        // period after connect, not immediately — a just-connected
        // subscriber wants doorbells, not a greeting comment.
        let mut keepalive =
            tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
        loop {
            tokio::select! {
                biased;
                // client disconnect: the response body (and with it the
                // pipe receiver) is gone — stop and clean up
                _ = frame_tx.closed() => break,
                // doorbell first: an event waiting behind a due
                // keepalive would ring up to one heartbeat late
                event = doorbell.recv() => match event {
                    Ok(event) => {
                        // Origin skip: never ring the pusher's own bell —
                        // the same client_id that pushed already knows.
                        // Only skips when the subscriber actually
                        // identified itself; an anonymous subscriber
                        // receives everything (pull is idempotent, a
                        // foreign ring is harmless).
                        let skip = match (&client_id, &event.origin) {
                            (Some(subscriber), Some(origin)) => subscriber == origin,
                            _ => false,
                        };
                        if skip {
                            continue;
                        }
                        // serde_json cannot fail on this shape (plain
                        // integer + optional string); the fallback keeps
                        // the production path free of panics regardless.
                        let json = serde_json::to_string(&event)
                            .unwrap_or_else(|_| "{\"max_version\":0,\"origin\":null}".to_string());
                        if frame_tx.send(Bytes::from(format!("data: {json}\n\n"))).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        // Slow subscriber missed `missed` rings — accepted
                        // doorbell loss (module docs); the next ring or the
                        // client's fallback pull restores freshness.
                        tracing::warn!(
                            ns = %pump_ns,
                            missed,
                            "subscribe stream lagged; doorbell events dropped"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Registry entry (and with it the sender) is gone:
                        // only reachable at shutdown.
                        break;
                    }
                },
                _ = keepalive.tick() => {
                    if frame_tx.send(Bytes::from_static(KEEPALIVE_FRAME)).await.is_err() {
                        break;
                    }
                },
            }
        }
        // Subscription over. Release the broadcast receiver FIRST so
        // the idle check sees a world without this subscription; then
        // reap the entry if the namespace went quiet.
        drop(doorbell);
        pump_hub.reap_if_idle(&pump_ns, &sender);
    });

    let mut response = Response::new(Body::from_stream(FramePipe { frames }));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}
