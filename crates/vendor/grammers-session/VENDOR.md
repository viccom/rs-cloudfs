# Vendored `grammers-session`

## Source

- Upstream crate: `grammers-session 0.10.0` (LGPL-free, `MIT OR Apache-2.0`)
- Copied from the local cargo registry cache:
  `%USERPROFILE%\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\grammers-session-0.10.0\`
  (identical to the `v0.10.0` tag at https://codeberg.org/Lonami/grammers)
- The crates.io package contains no `LICENSE*` files; the license texts
  live in the upstream repository. The `license = "MIT OR Apache-2.0"`
  field in `Cargo.toml` is preserved from upstream.
  **Supplied 2026-09-02 (M6):** `LICENSE-MIT` and `LICENSE-APACHE` were
  fetched verbatim from `https://codeberg.org/Lonami/grammers`
  (`/raw/branch/master/LICENSE-MIT`, `/raw/branch/master/LICENSE-APACHE`,
  both HTTP 200) and now live in this directory, so the dual-license
  texts ship with the vendored copy instead of only upstream.
- Removed during vendoring (registry packaging artifacts / network-dependent
  tests, not part of the upstream source tree):
  `tests/`, `.cargo_vcs_info.json`, `.cargo-ok`, `Cargo.lock`,
  `Cargo.toml.orig`.

## Why this fork exists

Upstream's `sqlite-storage` feature is implemented on top of `libsql`,
which statically bundles its own SQLite C library (`libsql-ffi`). CyDrive
already links `rusqlite` with the `bundled` feature (via `cydrive-core`),
and on MSVC the two copies of the SQLite symbols collide (`LNK2005`
duplicate symbol errors). The adjudicated fix (rs-CyDrive
`docs/decisions.md`, 2026-09-02 entry) is to vendor the crate in-tree and
port the SQLite backend to rusqlite, then redirect every consumer in the
workspace — including the transitive `grammers-client` dependency — with a
single `[patch.crates-io]` entry in the workspace root `Cargo.toml`.

## Modifications relative to upstream 0.10.0

Only what the storage-backend swap requires; everything else is kept
verbatim so future re-vendor diffs stay minimal:

1. `Cargo.toml`:
   - `libsql` dependency removed; `rusqlite = { version = "0.40", features =
     ["bundled"] }` added (same version + features as `cydrive-core`, so
     the linker sees exactly one SQLite).
   - `tokio` removed from `[dependencies]` (the async `tokio::sync::Mutex`
     around the connection is replaced by `std::sync::Mutex`; the crate no
     longer needs any async primitives). Kept in `[dev-dependencies]` for
     the ported inline test.
   - `toml` dev-dependency and the `[[test]] deps` target dropped together
     with the removed upstream `tests/` directory.
   - `sqlite-storage` feature now gates `dep:rusqlite` instead of
     `dep:libsql`.
2. `src/storages/sqlite.rs`: `SqliteSession` ported from libsql to rusqlite.
   - Same database schema, same statements, same write-through semantics
     (every `Session` trait mutation is immediately committed to the file).
   - `Database(AsyncMutex<libsql::Connection>)` became
     `Database(Mutex<rusqlite::Connection>)` with `std::sync::Mutex`; lock
     poisoning is recovered from via `PoisonError::into_inner` (a panicking
     statement leaves the SQLite handle usable), so the upstream
     `SqliteSessionError::Poisoned` variant was removed along with the
     poison-mapping code paths.
   - `SqliteSession::open(path)` keeps its `async` signature (upstream API
     shape, awaited by grammers-client) but performs the open/init/preload
     synchronously inside: the session file is tiny and this runs once at
     startup, and keeping the crate free of a tokio dependency is worth
     more than offloading a sub-millisecond local file open. This is
     documented by a comment in the source.
   - Dynamic named-parameter binding in `cache_peer` was replaced by a
     single always-bound statement passing `Option` values (binding `NULL`
     explicitly is semantically identical to upstream omitting the binding
     for absent fields).
   - Transactions use `Connection::unchecked_transaction` (rusqlite's
     documented API for connections shared behind a `Mutex`, where a
     `&mut self` `transaction()` is not available).
3. `src/storages/mod.rs`: unchanged (the `sqlite-storage` cfg still gates
   the module).

Everything else — `Session` trait, `MemorySession`, peer/DC/update types,
`message_box`, `session_data`, DC constants — is byte-identical to
upstream.

## Re-vendor guidance

1. Copy the new upstream version from the registry cache over this
   directory (delete it first), re-strip the artifacts listed above.
2. Bump the workspace root `[patch.crates-io]` path if the directory
   layout changed (it should not).
3. Re-apply the modifications listed above to `Cargo.toml` and
   `src/storages/sqlite.rs` (`git diff` against the previous vendored
   commit is the fastest guide; keep everything else verbatim).
4. Run `cargo tree -i grammers-session` and confirm a single, path-patched
   copy; then `cargo test --workspace`.

The crate is intentionally **not** a workspace member: as an external path
dependency rustc caps its lints to `allow`, so `cargo fmt`/`clippy` gates
are not polluted by upstream code style, and Cargo.lock still pins its
transitive deps through the patch.
