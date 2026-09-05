//! Wire protocol types for the sync-lite HTTP API (v1).
//!
//! These structs are the frozen JSON contract shared by this server
//! and the client (cydrive-cli's sync module, Batch B): field names
//! are `snake_case`, `secret` is optional, and deletions travel as
//! tombstone rows (`deleted: true` with an empty payload).
//!
//! Compatibility matrix (pull gaining its optional `secret` field):
//!
//! | direction | outcome |
//! |---|---|
//! | new client -> old server | pull carries `secret`; the old
//!   server's serde ignores unknown fields, so `{key, since}` parsing
//!   is unaffected — works |
//! | old client -> new server (secret configured) | pull 403s until
//!   the client upgrades — expected; the 403 body names the fix |
//! | server without a configured secret | both endpoints stay open —
//!   pure loopback / tunnel deployments keep the old behavior |

use serde::{Deserialize, Serialize};

/// `POST /v1/push` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PushRequest {
    /// Namespace key (client-derived; the server treats it as opaque).
    pub key: String,
    /// Shared secret; required only when the server configured one.
    pub secret: Option<String>,
    /// Rows to upsert in batch order (each consumes one version).
    pub rows: Vec<PushRow>,
}

/// One row inside a [`PushRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PushRow {
    /// Logical path within the namespace.
    pub rel_path: String,
    /// `true` marks a tombstone (deletion).
    pub deleted: bool,
    /// Opaque row payload (the client's serialized file + chunks
    /// metadata). The server stores it verbatim, never interprets it.
    pub payload: String,
}

/// `POST /v1/push` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PushResponse {
    /// The namespace counter after this batch — the highest version
    /// handed out; the client records it as its new pull cursor.
    pub max_version: i64,
}

/// `POST /v1/pull` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PullRequest {
    /// Namespace key.
    pub key: String,
    /// Return only rows with `version > since` (0 = everything).
    pub since: i64,
    /// Shared secret; required only when the server configured one.
    ///
    /// Serialization form: the field is *omitted* when `None`
    /// (`skip_serializing_if`) rather than emitted as `null` — a
    /// secretless pull then stays byte-identical to the pre-secret
    /// wire form, the most compatible shape for every intermediary.
    /// (A `null` would also be harmless: old servers ignore unknown
    /// fields regardless of value — see the module matrix.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// `POST /v1/pull` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PullResponse {
    /// Changed rows, ordered by version ascending (apply order).
    pub rows: Vec<PulledRow>,
    /// The namespace counter at pull time.
    pub max_version: i64,
}

/// One row inside a [`PullResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PulledRow {
    /// Logical path within the namespace.
    pub rel_path: String,
    /// Server-assigned monotonic version.
    pub version: i64,
    /// `true` for tombstones.
    pub deleted: bool,
    /// The stored payload, verbatim (empty for tombstones by
    /// convention — the server does not enforce it).
    pub payload: String,
}
