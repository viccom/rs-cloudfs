//! Credential vault seam (M5): the two CyDrive secrets — the Telegram
//! bot token and the optional client-side encryption password — move out
//! of plaintext config files into an OS-backed secret store.
//!
//! Contract source: `docs/rust-rewrite-design.md`, «凭据保管» — the
//! Python version kept `bot_token` / `encryption_password` in a
//! world-readable `config.json`, which is a defect this module fixes.
//! The seam is a trait so the core stays store-agnostic: production
//! wires the keyring-backed [`crate::credentials`] implementation in the
//! CLI crate (`KeyringStore`), tests and store-less fallbacks use
//! [`InMemoryStore`].

use std::collections::HashMap;
use std::sync::Mutex;

/// Credential key holding the Telegram bot token (`"<id>:<secret>"`).
pub const BOT_TOKEN: &str = "bot_token";

/// Credential key holding the optional client-side encryption password.
pub const ENCRYPTION_PASSWORD: &str = "encryption_password";

/// keyring service namespace for all CyDrive entries (the
/// `Entry::new(service, username)` first argument; the username is the
/// credential key, see [`USER`]).
pub const SERVICE: &str = "cydrive";

/// Logical keyring account owning CyDrive's entries. Entries carry the
/// credential key as their username (`cydrive` service + `bot_token` /
/// `encryption_password` users), so this constant names the account the
/// namespace belongs to rather than an entry username itself.
pub const USER: &str = "main";

/// Failures of a [`CredentialStore`] implementation: either the OS store
/// is unusable on this platform/session ([`CredentialError::Unavailable`])
/// or the local filesystem leg failed ([`CredentialError::Io`]).
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// No usable OS secret store (platform unsupported, service locked,
    /// ...). The message carries the backend's diagnosis.
    #[error("credential store unavailable: {0}")]
    Unavailable(String),
    /// IO failure in a file-backed store.
    #[error("credential store io: {0}")]
    Io(#[from] std::io::Error),
}

/// OS-backed secret storage seam: keyring in production, in-memory in
/// tests (and as the degraded fallback when no OS store exists).
///
/// `delete` of a key that is not present is `Ok(())` — idempotent, so
/// cleanup paths never need to probe first.
pub trait CredentialStore {
    /// Returns the stored value, or `None` when the key is absent.
    fn get(&self, key: &str) -> Result<Option<String>, CredentialError>;
    /// Stores (creating or overwriting) the value for `key`.
    fn set(&self, key: &str, value: &str) -> Result<(), CredentialError>;
    /// Removes the key; deleting an absent key is `Ok(())`.
    fn delete(&self, key: &str) -> Result<(), CredentialError>;
}

/// Test double for [`CredentialStore`]; also the fallback the CLI
/// degrades to when no OS secret store is available (a missing keyring
/// must never refuse CyDrive from starting).
///
/// The map sits behind a [`Mutex`] so `&self` operations are safe to
/// share across threads, mirroring the real store's semantics.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    entries: Mutex<HashMap<String, String>>,
}

impl InMemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style seeding: inserts `key` = `value` and returns the
    /// store for chaining (`InMemoryStore::new().with(k1, v1).with(k2, v2)`).
    ///
    /// Never panics (the in-memory set has no failure mode).
    pub fn with(self, key: &str, value: &str) -> Self {
        self.set(key, value).expect("in-memory set cannot fail");
        self
    }
}

impl CredentialStore for InMemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, CredentialError> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(entries.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), CredentialError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.remove(key);
        Ok(())
    }
}
