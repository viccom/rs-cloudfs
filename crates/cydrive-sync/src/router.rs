//! HTTP surface: two POST/JSON endpoints over a [`SyncStore`].
//!
//! Error model (family-grade): a configured `secret` must arrive in
//! the push request body (mismatch or missing -> 403); malformed JSON
//! -> 400; store failures -> 500. Every error answers
//! `{"error": "..."}` JSON. The pull wire shape
//! (`{key, since}`) carries no secret field, so only push is gated —
//! reading stays open within whatever network boundary the operator
//! puts around the port (loopback default / reverse proxy).
//!
//! Bodies are parsed manually (raw bytes -> serde_json) instead of via
//! the `Json` extractor so that every malformed request lands on our
//! own 400 JSON body rather than axum's textual rejections.

use std::sync::Arc;

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

/// Shared handler state.
#[derive(Clone)]
struct AppState {
    store: Arc<SyncStore>,
    secret: Option<String>,
}

/// The sync API router: `POST /v1/push` + `POST /v1/pull`.
///
/// `store` is shared behind an `Arc` (the store's internal mutex
/// serializes access); `secret` is the push gate — `None` accepts
/// unauthenticated pushes.
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

/// The push gate: 403 unless the request secret equals the configured
/// one. (Plain equality — family-grade threat model, not a timing-
/// hardened comparison.)
fn secret_forbidden(configured: &str, offered: &Option<String>) -> bool {
    offered.as_deref() != Some(configured)
}

async fn push_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let request: PushRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid push request JSON: {err}"),
            );
        }
    };
    if let Some(expected) = &state.secret {
        if secret_forbidden(expected, &request.secret) {
            return error_response(StatusCode::FORBIDDEN, "invalid or missing secret");
        }
    }
    match state.store.push(&request.key, &request.rows) {
        Ok(max_version) => {
            tracing::debug!(
                namespace = %request.key,
                rows = request.rows.len(),
                max_version,
                "push accepted"
            );
            (StatusCode::OK, Json(PushResponse { max_version })).into_response()
        }
        Err(err) => {
            tracing::error!(namespace = %request.key, error = %err, "push failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal store error")
        }
    }
}

async fn pull_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let request: PullRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid pull request JSON: {err}"),
            );
        }
    };
    match state.store.pull(&request.key, request.since) {
        Ok((rows, max_version)) => {
            tracing::debug!(
                namespace = %request.key,
                rows = rows.len(),
                max_version,
                "pull served"
            );
            (StatusCode::OK, Json(PullResponse { rows, max_version })).into_response()
        }
        Err(err) => {
            tracing::error!(namespace = %request.key, error = %err, "pull failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal store error")
        }
    }
}
