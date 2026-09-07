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
//! Dual-active identity (review P3, Med-2): a `client_id` lives in
//! the client's db, and copying that db to a second machine makes both
//! machines subscribe with the SAME id — the plain origin skip (here
//! AND in the client) would then silence the doorbell between them in
//! both directions, silently degrading realtime to the fallback pull.
//! The registry therefore counts live identified subscribers per
//! (namespace, client_id): a self-origin event is skipped only while
//! exactly one such subscription exists; at two or more it is
//! DELIVERED with `origin` rewritten to `null` — the client also
//! skips frames whose origin equals its own id, so a null origin is
//! what lets the ring through, and the self-echo just costs one
//! idempotent pull pass. The subscription that brings an id to two
//! live subscribers logs a warn naming the likely cause, turning the
//! silent degradation into a diagnosable log line.
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

/// First 8 chars of an identifier for log lines — the same policy as
/// the router's access log (correlatable without pasting a whole,
/// identifier-like key into logs).
fn id_prefix(value: &str) -> String {
    value.chars().take(8).collect()
}

/// The registry proper: per-namespace broadcast channels plus the
/// dual-active detector's per-(namespace, client_id) count of live
/// identified subscribers (anonymous subscriptions never enter).
struct Registry {
    channels: HashMap<String, broadcast::Sender<SubscribeEvent>>,
    id_counts: HashMap<String, HashMap<String, usize>>,
}

/// Per-namespace broadcast registry shared by the push handler
/// (publisher) and every subscribe pump (receiver).
pub struct EventHub {
    registry: Mutex<Registry>,
}

impl EventHub {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            registry: Mutex::new(Registry {
                channels: HashMap::new(),
                id_counts: HashMap::new(),
            }),
        }
    }

    /// Poisoned-lock recovery mirrors [`crate::store::SyncStore`]: the
    /// registry is still structurally valid after a panic elsewhere,
    /// so recover instead of cascading.
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attaches a receiver to `ns`'s channel, creating the channel on
    /// first subscription, and registers an identified subscriber's
    /// dual-active count (all under the registry lock, so the count
    /// and the receiver appear in the registry atomically). Returns
    /// the sender alongside — the pump needs it to identify (and
    /// reap) its own entry. Runs under the same lock as
    /// [`Self::reap_if_idle`], so a disconnect-cleanup can never race
    /// a fresh subscriber out of its channel.
    fn subscribe(
        &self,
        ns: &str,
        client_id: Option<&str>,
    ) -> (
        broadcast::Sender<SubscribeEvent>,
        broadcast::Receiver<SubscribeEvent>,
    ) {
        let mut registry = self.lock();
        let sender = registry
            .channels
            .entry(ns.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone();
        if let Some(id) = client_id {
            let count = registry
                .id_counts
                .entry(ns.to_string())
                .or_default()
                .entry(id.to_string())
                .or_insert(0);
            *count += 1;
            if *count >= 2 {
                // Diagnosability (module docs): without this line the
                // copied-db accident degrades realtime silently — the
                // warn names the anomaly and its likely cause.
                tracing::warn!(
                    ns = %id_prefix(ns),
                    client = %id_prefix(id),
                    "duplicate client_id detected (db copied to another machine?); \
                     doorbell self-skip disabled for this id"
                );
            }
        }
        (sender.clone(), sender.subscribe())
    }

    /// Live identified subscriptions sharing one (namespace,
    /// client_id) — 0 when none. The pump's dual-active check; public
    /// as test observability, mirroring [`Self::receiver_count`].
    pub fn id_subscriber_count(&self, ns: &str, client_id: &str) -> usize {
        self.lock()
            .id_counts
            .get(ns)
            .and_then(|per_ns| per_ns.get(client_id))
            .copied()
            .unwrap_or(0)
    }

    /// Doorbell every current receiver of `ns`. Called by the push
    /// handler after its transaction committed. A send error means no
    /// receivers (an entry can transiently exist with none, between a
    /// disconnect and its reap) — dropping the event is correct, there
    /// is nobody to ring. Never creates an entry: publishing into an
    /// unsubscribed namespace is a no-op, so idle namespaces cost
    /// nothing.
    pub fn publish(&self, ns: &str, event: &SubscribeEvent) {
        let registry = self.lock();
        if let Some(sender) = registry.channels.get(ns) {
            let _ = sender.send(event.clone());
        }
    }

    /// Receivers currently attached to `ns` (0 for an unknown
    /// namespace) — test observability.
    pub fn receiver_count(&self, ns: &str) -> usize {
        self.lock()
            .channels
            .get(ns)
            .map_or(0, |sender| sender.receiver_count())
    }

    /// Live namespace entries — the leak check: an entry appears with
    /// the first subscriber of a namespace and disappears with the
    /// last (test observability).
    pub fn channel_count(&self) -> usize {
        self.lock().channels.len()
    }

    /// Releases one identified subscriber's dual-active bookkeeping.
    /// Called from the pump teardown BEFORE the broadcast receiver
    /// drops, keeping the count conservative: an id is only reported
    /// dual-active while every counted subscription's pump is still
    /// live, so a same-id peer's skip decision never acts on a stale
    /// duplicate (the reverse order would briefly report dual-active
    /// for a subscription whose pump already stopped delivering).
    fn release_id(&self, ns: &str, client_id: Option<&str>) {
        let Some(id) = client_id else { return };
        let mut registry = self.lock();
        if let Some(per_ns) = registry.id_counts.get_mut(ns) {
            if let Some(count) = per_ns.get_mut(id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    per_ns.remove(id);
                }
            }
            if per_ns.is_empty() {
                registry.id_counts.remove(ns);
            }
        }
    }

    /// Removes `ns`'s entry when it is the very channel `sender`
    /// subscribed to and no receiver remains. The identity check
    /// (`same_channel`) makes a late cleanup a no-op if the entry was
    /// already reaped and a newer generation created it.
    fn reap_if_idle(&self, ns: &str, sender: &broadcast::Sender<SubscribeEvent>) {
        let mut registry = self.lock();
        if let Some(current) = registry.channels.get(ns) {
            if current.receiver_count() == 0 && current.same_channel(sender) {
                registry.channels.remove(ns);
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
    let (sender, mut doorbell) = hub.subscribe(&ns, client_id.as_deref());
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
                        // Only skips when the subscriber identified
                        // itself AND is the ONLY live subscription with
                        // that id in this namespace: a db copied to
                        // another machine makes two machines share one
                        // client_id, and skipping both would silence the
                        // doorbell between them in BOTH directions (the
                        // client would also skip the frame — its origin
                        // equals its own id). A dual-active id therefore
                        // still gets the ring, with the origin rewritten
                        // to null so the client-side skip cannot
                        // re-silence it (the self-echo costs one
                        // idempotent pull pass — harmless). An anonymous
                        // subscriber receives everything (pull is
                        // idempotent, a foreign ring is harmless).
                        //
                        // The dual-active check reads the registry per
                        // candidate event: doorbells are low-frequency
                        // (one per committed push), so a single locked
                        // count read is negligible — and snapshotting
                        // "am I dual-active" when the pump starts would
                        // be wrong anyway, because a second same-id
                        // subscriber may arrive at any time. The simple
                        // per-event query is the correct one.
                        let frame_event = match client_id.as_deref() {
                            Some(subscriber) if event.origin.as_deref() == Some(subscriber) => {
                                match pump_hub.id_subscriber_count(&pump_ns, subscriber) {
                                    // single-active self-echo: keep the
                                    // skip optimization
                                    1 => continue,
                                    // dual-active same id: deliver with a
                                    // null origin (module docs)
                                    _ => SubscribeEvent {
                                        max_version: event.max_version,
                                        origin: None,
                                    },
                                }
                            }
                            // foreign origin or anonymous subscriber: as-is
                            _ => event,
                        };
                        // serde_json cannot fail on this shape (plain
                        // integer + optional string); the fallback keeps
                        // the production path free of panics regardless.
                        let json = serde_json::to_string(&frame_event)
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
        // Subscription over. Release order: the dual-active count
        // first (a same-id peer's skip decision must not observe this
        // subscription as live once its pump has stopped delivering),
        // then the broadcast receiver so the idle check below sees a
        // world without this subscription, then the reap.
        pump_hub.release_id(&pump_ns, client_id.as_deref());
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
