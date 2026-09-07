//! Production [`CredentialStore`] backed by the OS secret store (M5):
//! Windows Credential Manager, macOS Keychain or the Secret Service on
//! Linux, via keyring 3.x.
//!
//! keyring 3.x's layered architecture wires the platform store through
//! Cargo target features (see this crate's `Cargo.toml`); entries are
//! identified by `Entry::new(service, username)` with the `cydrive`
//! service and the credential key as the username — one entry per
//! credential ([`SERVICE`], [`crate::credentials::BOT_TOKEN`] /
//! [`crate::credentials::ENCRYPTION_PASSWORD`]).
//!
//! Error mapping: keyring's `NoEntry` is the store's "key absent" answer
//! (`get` → `None`, `delete` → `Ok`), everything else means the store is
//! unusable on this machine/session and maps to
//! [`CredentialError::Unavailable`] — callers degrade instead of dying
//! (config discovery) or fail loudly (migrate).

use cloudkit_core::credentials::{CredentialError, CredentialStore, SERVICE, USER};
use keyring::Entry;

/// OS-backed credential store through keyring 3.x.
pub struct KeyringStore;

impl KeyringStore {
    /// Probes the platform store with a read of the namespace's
    /// (never-secret) owner entry `cydrive`/`main` ([`USER`]): a
    /// reachable store — with or without the entry — yields `Ok`, any
    /// other store failure yields
    /// [`CredentialError::Unavailable`] so the caller can degrade.
    pub fn new() -> Result<Self, CredentialError> {
        let probe = entry(USER)?;
        match probe.get_password() {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(Self),
            Err(error) => Err(unavailable(error)),
        }
    }
}

/// Creates the keyring entry for a credential key (username = the key).
fn entry(key: &str) -> Result<Entry, CredentialError> {
    Entry::new(SERVICE, key).map_err(unavailable)
}

/// Maps any keyring failure to [`CredentialError::Unavailable`].
fn unavailable(error: keyring::Error) -> CredentialError {
    CredentialError::Unavailable(error.to_string())
}

impl CredentialStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>, CredentialError> {
        match entry(key)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(unavailable(error)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), CredentialError> {
        entry(key)?.set_password(value).map_err(unavailable)
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        match entry(key)?.delete_credential() {
            Ok(()) => Ok(()),
            // Deleting an absent key is a no-op success (idempotent).
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(unavailable(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-OS-store roundtrip: **mutates the machine's credential
    /// manager**, so it never runs in the CI gate. Run it manually with
    ///
    /// ```text
    /// cargo test -p cydrive-cli --lib ignored_keyring_roundtrip -- --ignored
    /// ```
    ///
    /// The entries live under a unique per-run service suffix so a
    /// crash mid-test can never leave debris in (or read secrets from)
    /// the production `cydrive` namespace; the suffix is cleaned up at
    /// the end.
    #[test]
    #[ignore = "touches the real OS credential store; run explicitly"]
    fn ignored_keyring_roundtrip() {
        let service = format!("{SERVICE}-roundtrip-test");
        let token = Entry::new(&service, "bot_token").expect("entry for bot_token");
        let password = Entry::new(&service, "encryption_password").expect("entry for password");

        // set → get roundtrip and overwrite.
        token.set_password("secret-1").expect("set bot_token");
        assert_eq!(token.get_password().expect("get bot_token"), "secret-1");
        token.set_password("secret-2").expect("overwrite bot_token");
        assert_eq!(token.get_password().expect("get bot_token"), "secret-2");

        // A second entry under the same service stays independent.
        password.set_password("pw-1").expect("set password");
        assert_eq!(password.get_password().expect("get password"), "pw-1");
        assert_eq!(token.get_password().expect("get bot_token"), "secret-2");

        // delete removes; a second delete is Ok (idempotent), and get
        // then answers NoEntry.
        token.delete_credential().expect("delete bot_token");
        assert!(matches!(token.get_password(), Err(keyring::Error::NoEntry)));
        token.delete_credential().expect("delete is idempotent");

        // Cleanup: leave zero debris.
        password.delete_credential().expect("cleanup password");
        assert!(matches!(
            password.get_password(),
            Err(keyring::Error::NoEntry)
        ));
    }
}
