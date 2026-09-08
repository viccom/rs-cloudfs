//! Lite metadata sync server for CyDrive (sync-lite plan, Batch A).
//!
//! Family-scale sharing of one drive index across several machines or
//! instances: a single standalone binary keeping one SQLite file with
//! two tables (`namespaces`, `rows`) and exposing three endpoints
//! over axum:
//!
//! - `POST /v1/push` — upsert a batch of rows into a namespace; the
//!   server assigns every row the next value of the namespace's
//!   monotonic counter (last pusher wins), answering `{max_version}`.
//! - `POST /v1/pull` — every row of the namespace whose version is
//!   greater than `since`, plus the current counter.
//! - `POST /v1/subscribe` — the SSE doorbell (near-realtime batch):
//!   a `text/event-stream` that rings `{"max_version", "origin"}` on
//!   every committed push; subscribers then pull the data themselves,
//!   so push/pull stays the single source of truth.
//!
//! Deletions travel as tombstone rows (`deleted: true`, empty payload);
//! the server never interprets `payload` beyond storing it verbatim.
//! An optional shared `secret` (request field checked against the
//! `SYNC_SECRET` env) keeps strangers out — family-grade trust, with
//! TLS left to a reverse proxy (see `deploy/cydrive-sync.service`).

pub mod config;
pub mod events;
pub mod router;
pub mod startup;
pub mod store;
pub mod wire;

pub use config::{
    parse_config, ConfigError, SyncServerConfig, DEFAULT_DB_FILENAME, DEFAULT_HEARTBEAT,
    DEFAULT_LISTEN,
};
pub use events::EventHub;
pub use router::{router, router_with_gate, router_with_heartbeat, router_with_hub};
pub use store::{SyncStore, SyncStoreError};
pub use wire::{
    PullRequest, PullResponse, PulledRow, PushRequest, PushResponse, PushRow, SubscribeEvent,
    SubscribeRequest,
};
