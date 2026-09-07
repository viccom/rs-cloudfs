//! Byte-compatible port of the Python `CyDrive` client-side encryption
//! (`cydrive/crypto.py`).
//!
//! Wire format (frozen compatibility contract): `[16B salt][12B nonce]
//! [AES-256-GCM ciphertext+16B tag]`, key derived with PBKDF2-HMAC-SHA256
//! over 100,000 iterations, no AAD. Data encrypted by the Python client
//! must decrypt here and vice versa.
//!
//! Migrated verbatim from `cloudkit-core/src/crypto.rs` (Batch E, foundation
//! D7): function bodies are unchanged; only the shared `CryptoError` enum
//! moved to the crate root. The Python interop vector test moved along
//! (`tests/gcm_v1.rs` + `tests/compat/fixtures/crypto_vector.json`).
//!
//! As a [`CryptoScheme`](crate::CryptoScheme) implementation the frozen
//! whole-file format necessarily buffers the entire message in
//! `encrypt_stream`/`decrypt_stream`, and `decrypt_range` amplifies to a
//! full-file decryption followed by a slice — this is the format, not an
//! implementation choice; use [`AeadV2`](crate::AeadV2) for constant-memory
//! streaming and block-granularity random access.

use std::io::{Read, Write};

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use pbkdf2::pbkdf2_hmac;
use rand::random;
use sha2::Sha256;

use crate::{clamp_range, CryptoScheme, CryptoSchemeId};

// The frozen error surface stays importable from this module's path
// (`cloudkit_crypto::v1::CryptoError`), matching the pre-split
// `cloudkit_core::crypto::CryptoError` import shape.
pub use crate::CryptoError;

/// Salt size in bytes, prepended to every ciphertext.
pub const SALT_SIZE: usize = 16;
/// GCM nonce size in bytes, prepended after the salt.
pub const NONCE_SIZE: usize = 12;
/// Derived AES-256 key size in bytes.
pub const KEY_SIZE: usize = 32;
/// PBKDF2 iteration count, must match the Python implementation.
pub const PBKDF2_ITERATIONS: u32 = 100_000;
/// AES-GCM authentication tag size appended to the ciphertext body.
const GCM_TAG_SIZE: usize = 16;

/// Derives the AES-256 key from a UTF-8 password and salt via
/// PBKDF2-HMAC-SHA256 with [`PBKDF2_ITERATIONS`] iterations.
pub fn derive_key(password: &str, salt: &[u8]) -> [u8; KEY_SIZE] {
    let mut key = [0u8; KEY_SIZE];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, PBKDF2_ITERATIONS, &mut key);
    key
}

/// Encrypts `plaintext` under `password`.
///
/// Returns `salt(16) || nonce(12) || AES-256-GCM(ct + tag16)` with a fresh
/// random salt and nonce per call; length is always `plaintext.len() + 44`.
pub fn encrypt(password: &str, plaintext: &[u8]) -> Vec<u8> {
    let salt: [u8; SALT_SIZE] = random();
    let nonce: [u8; NONCE_SIZE] = random();
    let key = derive_key(password, &salt);
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key));
    // No AAD: identical to the Python `AESGCM.encrypt(nonce, pt, None)`.
    let body = cipher
        .encrypt(&Nonce::from(nonce), plaintext)
        .expect("AES-256-GCM encryption of an in-memory buffer cannot fail");
    let mut out = Vec::with_capacity(SALT_SIZE + NONCE_SIZE + body.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&body);
    out
}

/// Decrypts a payload produced by [`encrypt`] (or by the Python client).
pub fn decrypt(password: &str, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if data.len() < SALT_SIZE + NONCE_SIZE + GCM_TAG_SIZE {
        return Err(CryptoError::TooShort);
    }
    let (salt, rest) = data.split_at(SALT_SIZE);
    let (nonce, body) = rest.split_at(NONCE_SIZE);
    let key = derive_key(password, salt);
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(key));
    let nonce_bytes: [u8; NONCE_SIZE] = nonce.try_into().expect("nonce length checked above");
    cipher
        .decrypt(&Nonce::from(nonce_bytes), body)
        .map_err(|_| CryptoError::AuthFailed)
}

/// [`CryptoScheme`] adapter for the frozen v1 GCM format.
///
/// Stateless value type; `Default` is provided for ergonomics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcmV1;

impl GcmV1 {
    /// Creates the (stateless) v1 scheme handle.
    pub fn new() -> Self {
        Self
    }
}

impl CryptoScheme for GcmV1 {
    fn id(&self) -> CryptoSchemeId {
        CryptoSchemeId::GcmV1
    }

    fn encrypt_stream(
        &self,
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<u64, CryptoError> {
        // Frozen single-body format: the whole message must be in memory to
        // seal it (documented at the trait and in the module docs).
        let mut plaintext = Vec::new();
        src.read_to_end(&mut plaintext)?;
        let ciphertext = encrypt(password, &plaintext);
        dst.write_all(&ciphertext)?;
        Ok(ciphertext.len() as u64)
    }

    fn decrypt_stream(
        &self,
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<(), CryptoError> {
        let mut ciphertext = Vec::new();
        src.read_to_end(&mut ciphertext)?;
        let plaintext = decrypt(password, &ciphertext)?;
        dst.write_all(&plaintext)?;
        Ok(())
    }

    fn decrypt_range(
        &self,
        password: &str,
        ciphertext: &[u8],
        range: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, CryptoError> {
        let plaintext = decrypt(password, ciphertext)?;
        let (start, end) = clamp_range(range.start, range.end, plaintext.len() as u64);
        Ok(plaintext[start as usize..end as usize].to_vec())
    }
}
