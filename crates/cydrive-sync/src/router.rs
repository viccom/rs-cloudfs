//! HTTP surface: two POST/JSON endpoints over a [`SyncStore`].
//!
//! Error model (family-grade): a configured `secret` must arrive in
//! *every* request body — push AND pull. The server is deployed on
//! the public internet, where an ungated pull would hand the whole
//! drive index to whoever can reach the port; mismatch or missing ->
//! 403 whose body says where the client configures its secret;
//! malformed JSON -> 400; store failures -> 500. Every error answers
//! `{"error": "..."}` JSON. With *no* configured secret both endpoints
//! stay open — pure loopback / tunnel deployments keep the old
//! behavior (deliberate ruling, not an oversight).
//!
//! Wire compatibility (pull gained its `secret` field): a new client
//! against an old server is fine (serde ignores the unknown field);
//! an old client pulling from a secret-configured server gets 403
//! until it upgrades — see `wire`'s matrix.
//!
//! Access logging: every request emits exactly one line — completions
//! at info (endpoint, namespace-key prefix, row count, max_version,
//! elapsed ms), secret rejections at warn (never the secret itself,
//! only missing/mismatched), 400/500 at error.
//!
//! Bodies are parsed manually (raw bytes -> serde_json) instead of via
//! the `Json` extractor so that every malformed request lands on our
//! own 400 JSON body rather than axum's textual rejections.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use crate::store::SyncStore;
use crate::wire::{PullRequest, PullResponse, PushRequest, PushResponse};

/// Body cap for one push batch. axum's 2 MB default would be too small
/// for a full-drive initial push (tens of thousands of metadata rows);
/// 64 MB covers family-scale drives with ample headroom.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

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
}

/// The sync API router: `POST /v1/push` + `POST /v1/pull`.
///
/// `store` is shared behind an `Arc` (the store's internal mutex
/// serializes access); `secret` gates *both* endpoints — `None`
/// accepts unauthenticated requests everywhere.
pub fn router(store: Arc<SyncStore>, secret: Option<String>) -> Router {
    Router::new()
        .route("/v1/push", post(push_handler))
        .route("/v1/pull", post(pull_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(AppState { store, secret })
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
            tracing::info!(
                endpoint = "push",
                ns = %ns_prefix(&request.key),
                rows = request.rows.len(),
                max_version,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "push accepted"
            );
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
            tracing::info!(
                endpoint = "pull",
                ns = %ns_prefix(&request.key),
                rows = rows.len(),
                max_version,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "pull served"
            );
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
