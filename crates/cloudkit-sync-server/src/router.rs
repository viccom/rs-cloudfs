//! HTTP surface: three POST endpoints over a [`SyncStore`] — two JSON
//! (`push`, `pull`) and one SSE doorbell stream (`subscribe`).
//!
//! Error model (family-grade): a configured `secret` must arrive in
//! *every* request body — push, pull AND subscribe. The server is
//! deployed on the public internet, where an ungated pull would hand
//! the whole drive index to whoever can reach the port, and an
//! ungated subscribe would let a stranger watch the doorbell;
//! mismatch or missing -> 403 whose body says where the client
//! configures its secret; malformed JSON -> 400; store failures ->
//! 500. Every error answers `{"error": "..."}` JSON. With *no*
//! configured secret all endpoints stay open — pure loopback /
//! tunnel deployments keep the old behavior (deliberate ruling, not
//! an oversight).
//!
//! Wire compatibility (pull gained its `secret` field, and the
//! doorbell batch added an optional `client_id` everywhere plus the
//! purely additive subscribe endpoint): a new client against an old
//! server is fine (serde ignores the unknown fields); an old client
//! pulling from a secret-configured server gets 403 until it upgrades
//! — see `wire`'s matrix. An old client that never subscribes is
//! entirely unaffected.
//!
//! Access logging: every request emits exactly one line — completions
//! at info (endpoint, namespace-key prefix, row count, max_version,
//! elapsed ms, plus `client=<first 8 chars>` when the request carried
//! a client_id), secret rejections at warn (never the secret itself,
//! only missing/mismatched), 400/500 at error. Subscribe logs its
//! accept line only — there is no "completion" for a long-lived
//! stream, and per-doorbell-event logging would be pure noise (the
//! triggering push already logs its own completion).
//!
//! Bodies are parsed manually (raw bytes -> serde_json) instead of via
//! the `Json` extractor so that every malformed request lands on our
//! own 400 JSON body rather than axum's textual rejections.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use tokio::sync::Semaphore;

use crate::config::DEFAULT_HEARTBEAT;
use crate::events::{sse_response, EventHub};
use crate::store::SyncStore;
use crate::wire::{
    PullRequest, PullResponse, PushRequest, PushResponse, SubscribeEvent, SubscribeRequest,
};

/// Body cap for one push batch. axum's 2 MB default would be too small
/// for a full-drive initial push (tens of thousands of metadata rows);
/// 64 MB covers family-scale drives with ample headroom.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// How many push/pull requests may hold a buffered request body in
/// memory at the same time. Each buffered batch peaks at
/// [`MAX_BODY_BYTES`] (64 MB), so 2 permits cap the worst-case
/// concurrent buffering at 128 MB — a budget a family-grade box
/// survives. A third concurrent batch is shed with 503 + a
/// "retry shortly" hint instead of queueing (a queued batch would
/// burn the client's timeout with no progress signal).
pub const MAX_CONCURRENT_BODY_BUFFERS: usize = 2;

/// The actionable tail of every secret 403: tells the client where its
/// side configures the secret, so a stranger's probe and an honest
/// misconfigured client both get a next step.
const SECRET_HELP: &str = "this server requires a shared secret; send it in the \
     request body's \"secret\" field (client side: sync_secret in config.toml or \
     the CYDRIVE_SYNC_SECRET environment variable)";

/// Shared handler state.
#[derive(Clone)]
struct AppState {
    store: Arc<SyncStore>,
    secret: Option<String>,
    /// The SSE doorbell registry (publish on push, subscribe streams
    /// receive).
    hub: Arc<EventHub>,
    /// SSE keepalive interval (default [`DEFAULT_HEARTBEAT`]; tests
    /// inject ~100 ms).
    heartbeat: Duration,
    /// Concurrency gate bounding how many push/pull request bodies
    /// are buffered at once (memory budget: see
    /// [`MAX_CONCURRENT_BODY_BUFFERS`]). Tests inject their own
    /// semaphore and pre-acquire every permit to pin the shed path.
    body_gate: Arc<Semaphore>,
}

/// The sync API router: `POST /v1/push` + `POST /v1/pull` +
/// `POST /v1/subscribe` (SSE doorbell), heartbeat at the default.
///
/// `store` is shared behind an `Arc` (the store's internal mutex
/// serializes access); `secret` gates *all* endpoints — `None`
/// accepts unauthenticated requests everywhere.
pub fn router(store: Arc<SyncStore>, secret: Option<String>) -> Router {
    router_with_hub(store, secret, Arc::new(EventHub::new()), DEFAULT_HEARTBEAT)
}

/// [`router`] with an injectable SSE heartbeat interval (tests run
/// ~100 ms; production keeps the 20 s default).
pub fn router_with_heartbeat(
    store: Arc<SyncStore>,
    secret: Option<String>,
    heartbeat: Duration,
) -> Router {
    router_with_hub(store, secret, Arc::new(EventHub::new()), heartbeat)
}

/// The fully assembled router with every part injectable — the
/// integration tests pass their own [`EventHub`] to observe the
/// registry (receiver/channel counts) while driving real HTTP. The
/// body-buffering gate is the production default
/// ([`MAX_CONCURRENT_BODY_BUFFERS`] permits).
pub fn router_with_hub(
    store: Arc<SyncStore>,
    secret: Option<String>,
    hub: Arc<EventHub>,
    heartbeat: Duration,
) -> Router {
    router_with_gate(
        store,
        secret,
        hub,
        heartbeat,
        Arc::new(Semaphore::new(MAX_CONCURRENT_BODY_BUFFERS)),
    )
}

/// [`router_with_hub`] plus an injectable body-buffering gate — the
/// gate tests hold every permit of their own semaphore to pin the
/// 503 shed answer without racing real concurrent requests.
pub fn router_with_gate(
    store: Arc<SyncStore>,
    secret: Option<String>,
    hub: Arc<EventHub>,
    heartbeat: Duration,
    body_gate: Arc<Semaphore>,
) -> Router {
    Router::new()
        .route("/v1/push", post(push_handler))
        .route("/v1/pull", post(pull_handler))
        .route("/v1/subscribe", post(subscribe_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(AppState {
            store,
            secret,
            hub,
            heartbeat,
            body_gate,
        })
}

/// Error body shape shared by 400/403/500.
#[derive(Serialize)]
struct ApiError {
    error: String,
}

/// Every non-success answer: `{"error": "..."}` with the given status.
fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

/// The shared secret gate: 403 unless the request secret equals the
/// configured one. (Plain equality — family-grade threat model, not a
/// timing-hardened comparison.)
fn secret_forbidden(configured: &str, offered: &Option<String>) -> bool {
    offered.as_deref() != Some(configured)
}

/// The 403 for a failed secret check, with a WARN access line. Never
/// logs the offered or expected secret — only which way it failed.
fn secret_rejected(endpoint: &str, key: &str) -> Response {
    tracing::warn!(
        endpoint,
        ns = %ns_prefix(key),
        "request rejected: secret missing or mismatched"
    );
    error_response(
        StatusCode::FORBIDDEN,
        format!("missing or invalid secret; {SECRET_HELP}"),
    )
}

/// First 8 chars of the namespace key — enough to correlate a log line
/// with a client's key without pasting the (identifier-like) full key
/// into logs.
fn ns_prefix(key: &str) -> String {
    key.chars().take(8).collect()
}

async fn push_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let started = Instant::now();
    let request: PushRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            tracing::error!(endpoint = "push", error = %err, "push rejected: malformed JSON");
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid push request JSON: {err}"),
            );
        }
    };
    if let Some(expected) = &state.secret {
        if secret_forbidden(expected, &request.secret) {
            return secret_rejected("push", &request.key);
        }
    }
    match state.store.push(&request.key, &request.rows) {
        Ok(max_version) => {
            // Ring the doorbell AFTER the transaction committed: a
            // subscriber that pulls on this ring must see the pushed
            // rows. Events carry no data — just max_version + origin.
            state.hub.publish(
                &request.key,
                &SubscribeEvent {
                    max_version,
                    origin: request.client_id.clone(),
                },
            );
            match &request.client_id {
                Some(client_id) => tracing::info!(
                    endpoint = "push",
                    ns = %ns_prefix(&request.key),
                    client = %ns_prefix(client_id),
                    rows = request.rows.len(),
                    max_version,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "push accepted"
                ),
                None => tracing::info!(
                    endpoint = "push",
                    ns = %ns_prefix(&request.key),
                    rows = request.rows.len(),
                    max_version,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "push accepted"
                ),
            }
            (StatusCode::OK, Json(PushResponse { max_version })).into_response()
        }
        Err(err) => {
            tracing::error!(
                endpoint = "push",
                ns = %ns_prefix(&request.key),
                error = %err,
                "push failed"
            );
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal store error")
        }
    }
}

async fn pull_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let started = Instant::now();
    let request: PullRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            tracing::error!(endpoint = "pull", error = %err, "pull rejected: malformed JSON");
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid pull request JSON: {err}"),
            );
        }
    };
    if let Some(expected) = &state.secret {
        if secret_forbidden(expected, &request.secret) {
            return secret_rejected("pull", &request.key);
        }
    }
    match state.store.pull(&request.key, request.since) {
        Ok((rows, max_version)) => {
            match &request.client_id {
                Some(client_id) => tracing::info!(
                    endpoint = "pull",
                    ns = %ns_prefix(&request.key),
                    client = %ns_prefix(client_id),
                    rows = rows.len(),
                    max_version,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "pull served"
                ),
                None => tracing::info!(
                    endpoint = "pull",
                    ns = %ns_prefix(&request.key),
                    rows = rows.len(),
                    max_version,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "pull served"
                ),
            }
            (StatusCode::OK, Json(PullResponse { rows, max_version })).into_response()
        }
        Err(err) => {
            tracing::error!(
                endpoint = "pull",
                ns = %ns_prefix(&request.key),
                error = %err,
                "pull failed"
            );
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal store error")
        }
    }
}

/// `POST /v1/subscribe` — the SSE doorbell stream. Same secret gate
/// as push/pull (passing the gate happens before any streaming, so a
/// rejected subscribe answers a complete 403 and never hangs a
/// connection open); an accepted request receives `text/event-stream`
/// doorbell frames until the client disconnects. Subscription count
/// is uncapped (family scale).
async fn subscribe_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let request: SubscribeRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            tracing::error!(
                endpoint = "subscribe",
                error = %err,
                "subscribe rejected: malformed JSON"
            );
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid subscribe request JSON: {err}"),
            );
        }
    };
    if let Some(expected) = &state.secret {
        if secret_forbidden(expected, &request.secret) {
            return secret_rejected("subscribe", &request.key);
        }
    }
    // One line at accept time — a long-lived stream has no completion
    // line, and an invisible connection is an ops blind spot.
    // Per-event traffic is deliberately NOT logged: the triggering
    // push already logged its own completion line.
    match &request.client_id {
        Some(client_id) => tracing::info!(
            endpoint = "subscribe",
            ns = %ns_prefix(&request.key),
            client = %ns_prefix(client_id),
            "subscribe accepted"
        ),
        None => tracing::info!(
            endpoint = "subscribe",
            ns = %ns_prefix(&request.key),
            "subscribe accepted"
        ),
    }
    sse_response(state.hub, request.key, request.client_id, state.heartbeat)
}
