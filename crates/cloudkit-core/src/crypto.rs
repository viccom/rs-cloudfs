//! Byte-compatible port of the Python `CyDrive` client-side encryption
//! (`cydrive/crypto.py`).
//!
//! Wire format (frozen compatibility contract): `[16B salt][12B nonce]
//! [AES-256-GCM ciphertext+16B tag]`, key derived with PBKDF2-HMAC-SHA256
//! over 100,000 iterations, no AAD. Data encrypted by the Python client
//! must decrypt here and vice versa.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use pbkdf2::pbkdf2_hmac;
use rand::random;
use sha2::Sha256;

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

/// Failures produced by [`decrypt`].
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// Input shorter than `SALT_SIZE + NONCE_SIZE + tag`.
    #[error("ciphertext too short")]
    TooShort,
    /// GCM authentication failed (wrong password or corrupted data).
    #[error("decryption failed (wrong password or corrupted data)")]
    AuthFailed,
}

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
