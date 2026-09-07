//! Compatibility shim: the crypto implementation moved to the standalone
//! `cloudkit-crypto` crate (Phase 1 Batch E, foundation D7). This module
//! re-exports the public surface so `cloudkit_core::crypto::*` paths
//! (vfs, upload queue and their tests) keep resolving unchanged.
//!
//! v1 GCM behavior and bytes are frozen (Python `CyCrypto` compatibility,
//! red line R6); see `cloudkit_crypto::v1` for the normative home.

pub use cloudkit_crypto::v1::{
    decrypt, derive_key, encrypt, KEY_SIZE, NONCE_SIZE, PBKDF2_ITERATIONS, SALT_SIZE,
};
pub use cloudkit_crypto::{CryptoError, CryptoScheme, CryptoSchemeId, GcmV1};
