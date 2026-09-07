//! RED-phase tests for `cloudkit_core::credentials` and the config
//! backfill evolution (M5-1: credential vault).
//!
//! Contract under test: the [`cloudkit_core::credentials::CredentialStore`]
//! seam (get / set / idempotent delete, the `InMemoryStore` test double)
//! and [`CyDriveConfig::with_credential_backfill`] — the "file > store"
//! precedence leg of env > file > keyring (the Python flaw of plaintext
//! `config.json` secrets is fixed by relocating them to the OS store,
//! see `docs/rust-rewrite-design.md`, «凭据保管»).

use cloudkit_core::config::CyDriveConfig;
use cloudkit_core::credentials::{CredentialStore, InMemoryStore, BOT_TOKEN, ENCRYPTION_PASSWORD};

// ------------------------------------------------------- store semantics ---

#[test]
fn in_memory_store_get_set_delete_and_chaining() {
    // `with` chains and seeds the store.
    let store = InMemoryStore::new()
        .with(BOT_TOKEN, "111:AA")
        .with(ENCRYPTION_PASSWORD, "sekrit");
    assert_eq!(
        store.get(BOT_TOKEN).expect("get bot_token"),
        Some("111:AA".to_string())
    );
    assert_eq!(
        store.get(ENCRYPTION_PASSWORD).expect("get password"),
        Some("sekrit".to_string())
    );
    // Unknown key: None, not an error.
    assert_eq!(store.get("not-a-key").expect("get unknown"), None);

    // set overwrites.
    store
        .set(BOT_TOKEN, "222:BB")
        .expect("set overwrites bot_token");
    assert_eq!(
        store.get(BOT_TOKEN).expect("get bot_token"),
        Some("222:BB".to_string())
    );

    // delete removes; deleting a missing key is Ok (idempotent).
    store.delete(BOT_TOKEN).expect("delete bot_token");
    assert_eq!(store.get(BOT_TOKEN).expect("get after delete"), None);
    store
        .delete(BOT_TOKEN)
        .expect("delete of a missing key is Ok");

    // Default is a fresh empty store.
    let default = InMemoryStore::default();
    assert_eq!(default.get(BOT_TOKEN).expect("get on default"), None);
}

// ------------------------------------------------------------ backfill -----

#[test]
fn backfill_fills_empty_fields_from_store() {
    let store = InMemoryStore::new()
        .with(BOT_TOKEN, "123456:ABC-DEF")
        .with(ENCRYPTION_PASSWORD, "store-pw");

    // File left both secrets empty/absent.
    let cfg = CyDriveConfig {
        chat_id: 123456789,
        enable_encryption: true,
        ..CyDriveConfig::default()
    };

    let out = cfg.with_credential_backfill(&store);
    assert_eq!(out.bot_token, "123456:ABC-DEF");
    assert_eq!(out.encryption_password.as_deref(), Some("store-pw"));
    assert!(out.is_configured(), "backfilled token configures the drive");
}

#[test]
fn backfill_keeps_file_values() {
    // File beats keyring: non-empty file values must survive backfill.
    let store = InMemoryStore::new()
        .with(BOT_TOKEN, "store:tok")
        .with(ENCRYPTION_PASSWORD, "store-pw");

    let cfg = CyDriveConfig {
        bot_token: "file:tok".to_string(),
        chat_id: 1,
        encryption_password: Some("file-pw".to_string()),
        ..CyDriveConfig::default()
    };

    let out = cfg.with_credential_backfill(&store);
    assert_eq!(out.bot_token, "file:tok");
    assert_eq!(out.encryption_password.as_deref(), Some("file-pw"));
}

#[test]
fn backfill_leaves_other_fields_alone() {
    let store = InMemoryStore::new()
        .with(BOT_TOKEN, "1:a")
        .with(ENCRYPTION_PASSWORD, "store-pw");

    let out = CyDriveConfig::default().with_credential_backfill(&store);

    // Exactly the two secret fields change; every other field keeps its
    // default.
    let expected = CyDriveConfig {
        bot_token: "1:a".to_string(),
        encryption_password: Some("store-pw".to_string()),
        ..CyDriveConfig::default()
    };
    assert_eq!(out, expected);
}
