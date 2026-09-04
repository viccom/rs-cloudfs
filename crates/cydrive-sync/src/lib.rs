//! Lite metadata sync server for CyDrive (sync-lite plan, Batch A).
//!
//! Family-scale sharing of one drive index across several machines or
//! instances: a single standalone binary keeping one SQLite file with
//! two tables (`namespaces`, `rows`) and exposing exactly two JSON
//! endpoints over axum:
//!
//! - `POST /v1/push` — upsert a batch of rows into a namespace; the
//!   server assigns every row the next value of the namespace's
//!   monotonic counter (last pusher wins), answering `{max_version}`.
//! - `POST /v1/pull` — every row of the namespace whose version is
//!   greater than `since`, plus the current counter.
//!
//! Deletions travel as tombstone rows (`deleted: true`, empty payload);
//! the server never interprets `payload` beyond storing it verbatim.
//! An optional shared `secret` (request field checked against the
//! `SYNC_SECRET` env) keeps strangers out — family-grade trust, with
//! TLS left to a reverse proxy (see `deploy/cydrive-sync.service`).
