//! Client-side encryption schemes for rs-cloudfs (foundation design D7).
//!
//! Two container formats live side by side behind the [`CryptoScheme`] trait:
//!
//! - **v1 GCM** ([`v1`]): whole-file AES-256-GCM, byte-compatible with the
//!   Python `CyDrive` `CyCrypto` wire format. Frozen for compatibility —
//!   maintained, never extended. Encrypting with v1 requires buffering the
//!   whole plaintext (the format has a single body), which is the documented
//!   reason v2 exists.
//! - **v2 chunked AEAD** (STREAM construction, planned Batch E): chunked
//!   AES-256-GCM with constant-memory streaming and block-granularity
//!   random access (`decrypt_range`).
//!
//! The scheme in use is Entry metadata (chosen per file at encryption time);
//! readers dispatch on [`CryptoSchemeId`]. Schemes are authenticated
//! encryption: any tampering (bit flips, truncation, chunk reordering,
//! header forgery) must surface as an [`CryptoError::AuthFailed`] (or a
//! structural error) — never as wrong plaintext.

pub mod v1;
pub mod v2;

use std::io::{Read, Write};
use std::ops::Range;

pub use v1::GcmV1;
pub use v2::AeadV2;

/// Identifies the container format of an encrypted payload.
///
/// Stored in Entry metadata / sync payloads so the read path can dispatch to
/// the right [`CryptoScheme`] without probing the bytes. The string form is
/// a stable wire identifier (`"gcm-v1"` / `"aead-v2"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CryptoSchemeId {
    /// Whole-file AES-256-GCM, Python `CyCrypto` compatible (frozen).
    GcmV1,
    /// Chunked STREAM-construction AES-256-GCM (streaming + random access).
    AeadV2,
}

impl CryptoSchemeId {
    /// Stable wire identifier for Entry/sync metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            CryptoSchemeId::GcmV1 => "gcm-v1",
            CryptoSchemeId::AeadV2 => "aead-v2",
        }
    }
}

impl std::fmt::Display for CryptoSchemeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Failures produced by the crypto schemes.
///
/// The first two variants are the frozen v1 surface (byte-compatible error
/// behavior with the pre-split `cloudkit_core::crypto`); the rest cover
/// streaming I/O and (with the v2 container) structural parse errors.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// Input shorter than the format's minimum (salt + nonce + tag).
    #[error("ciphertext too short")]
    TooShort,
    /// GCM authentication failed (wrong password or corrupted data).
    #[error("decryption failed (wrong password or corrupted data)")]
    AuthFailed,
    /// Underlying reader/writer failed during streaming encryption/decryption.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// v2 container does not start with the expected magic bytes.
    #[error("not a cloudkit-crypto v2 container (bad magic)")]
    BadMagic,
    /// v2 container version byte is not understood by this build.
    #[error("unsupported container version: {0}")]
    UnsupportedVersion(u8),
    /// v2 container structure is inconsistent (bad lengths/reserved/chunk math).
    #[error("malformed ciphertext structure")]
    Malformed,
    /// Configured chunk size is outside the format guardrails
    /// (container headers carry out-of-guardrail sizes as
    /// [`CryptoError::Malformed`] — untrusted input never borrows the
    /// constructor's error).
    #[error(
        "invalid chunk size {size}: must be between {min} and {max} bytes",
        size = 0,
        min = v2::MIN_CHUNK_SIZE,
        max = v2::MAX_CHUNK_SIZE
    )]
    InvalidChunkSize(usize),
}

/// A client-side encryption container format (foundation D7).
///
/// # Semantic contract
///
/// - **Block boundaries**: schemes are free to chunk internally; chunking is
///   invisible to callers except through the memory/streaming profile of
///   each scheme (v1 buffers the whole message by format necessity; v2
///   processes one chunk at a time — constant memory for a fixed chunk
///   size).
/// - **`encrypt_stream`**: reads plaintext from `src` until EOF and writes
///   the complete container to `dst`; returns the number of ciphertext
///   bytes written. `src` is never read in units larger than the scheme's
///   chunk size (v1 reads once, whole-message, per its frozen format).
/// - **`decrypt_stream`**: reads a complete container from `src` and writes
///   the whole plaintext to `dst`.
/// - **`decrypt_range`**: decrypts the half-open plaintext byte range
///   `range` from an in-memory container, returning exactly the plaintext
///   slice `plaintext[range]` **clamped** to the plaintext length (`end` is
///   capped at the plaintext length; `start` at or beyond it yields an
///   empty vector). Only chunks overlapping the range are processed (v2);
///   v1 necessarily decrypts the whole file first — same output, worse
///   amplification, documented per-method.
/// - **Error semantics**: fail closed. Tampered or truncated input, a
///   forged header, reordered chunks or a wrong password produce
///   [`CryptoError::AuthFailed`] (and v2 structural parse errors for a
///   broken header) — never silent wrong plaintext. Partial plaintext may
///   already have been written by `decrypt_stream` when a later chunk fails
///   authentication; memory-mode APIs (returning `Vec`) never return
///   partial plaintext.
/// - **Concurrency**: implementations are stateless between calls and
///   `&self`-based; the trait requires `Send + Sync` so a scheme instance
///   can be shared across tasks/threads without synchronization.
pub trait CryptoScheme: Send + Sync {
    /// The container format this implementation produces and consumes.
    fn id(&self) -> CryptoSchemeId;

    /// Stream-encrypt `src` (read to EOF) into `dst`; returns ciphertext
    /// length in bytes.
    fn encrypt_stream(
        &self,
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<u64, CryptoError>;

    /// Stream-decrypt a complete container from `src` into `dst`.
    fn decrypt_stream(
        &self,
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<(), CryptoError>;

    /// Decrypt the half-open plaintext `range` (clamped) from an
    /// in-memory container.
    fn decrypt_range(
        &self,
        password: &str,
        ciphertext: &[u8],
        range: Range<u64>,
    ) -> Result<Vec<u8>, CryptoError>;
}

/// Clamp a half-open `[start, end)` range to `[0, len)` with storage-Range
/// semantics: `end` capped at `len`, `start` capped at the (already capped)
/// `end`; an empty or over-run range yields `(0, 0)`.
pub(crate) fn clamp_range(start: u64, end: u64, len: u64) -> (u64, u64) {
    let end = end.min(len);
    let start = start.min(end);
    (start, end)
}
