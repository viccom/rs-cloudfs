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
//!
//! The SSE doorbell batch adds the same-shaped optional `client_id`
//! to push/pull plus a `SubscribeRequest`/`SubscribeEvent` pair: all
//! optional fields follow the `secret` pattern (`default` +
//! skip-when-`None`), so old clients and old servers interoperate
//! exactly as before — subscribing is a purely additive endpoint.

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
    /// Who is pushing (the SSE doorbell's `origin`). Serialized away
    /// when `None` (the `secret`-field pattern), so an old client's
    /// push bytes are unchanged and old servers ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
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
    /// Who is pulling — access-log correlation only, same optional
    /// shape rules as `secret` (omitted when `None`, so old wire bytes
    /// are unchanged in both directions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// `POST /v1/subscribe` request body (the SSE doorbell). Same gate as
/// pull: a configured secret must arrive here too — a stranger must
/// not watch the doorbell any more than read the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SubscribeRequest {
    /// Namespace key to watch.
    pub key: String,
    /// Shared secret; required only when the server configured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// This subscriber's identity: events whose `origin` equals it
    /// (the subscriber's own pushes) are NOT delivered to this stream
    /// — except while the id is dual-active (two live subscriptions
    /// share it, the copied-db accident; see `events`' ruling), in
    /// which case they ARE delivered, with the origin rewritten to
    /// `null`. An anonymous subscriber (`None`) receives everything —
    /// pulling is idempotent, so a foreign-origin doorbell is
    /// harmless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// One SSE doorbell frame's data payload: *that* something changed
/// (`max_version`) and who pushed it (`origin` — `null` when the
/// pusher sent no client_id). The field set is fixed and `origin` is
/// always serialized (never omitted), so the client-side parser can
/// stay dumb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SubscribeEvent {
    /// The namespace counter after the triggering push — the client's
    /// next pull cursor hint.
    pub max_version: i64,
    /// The pusher's `client_id`, or `null` if it sent none — or if the
    /// server rewrote it: the one subscriber-side rewrite is the
    /// dual-active case (two live subscriptions share the origin id,
    /// the copied-db accident), where the pump delivers the
    /// self-origin event with a `null` origin so the CLIENT's own skip
    /// (origin equals its id) cannot re-silence the very doorbell this
    /// delivery exists to restore. See `events`' dual-active ruling.
    pub origin: Option<String>,
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
