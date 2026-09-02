//! CloudTransport implementation over Telegram MTProto via grammers.
//!
//! Built contract-first: the byte-exact pure helpers (caption formatting,
//! remote part naming, flood-wait parsing, range planning) land before any
//! networked engine, pinning Python-baseline compatibility with tests from
//! day one. See the design document at `docs/rust-rewrite-design.md`
//! (repository root) and the chunk-naming erratum in `docs/decisions.md`.

pub mod caption;
pub mod config;
pub mod flood;
pub mod plan;
pub mod range;
pub mod stream;
