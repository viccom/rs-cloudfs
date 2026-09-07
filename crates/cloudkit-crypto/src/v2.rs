//! v2 chunked AEAD container (STREAM construction) — RED-phase stub.
//!
//! The public shape (constants, constructors, inherent + trait methods) is
//! fixed here so the test surface compiles and fails on assertions; the
//! format specification and implementation land in the green commit.

use std::io::{Read, Write};
use std::ops::Range;

use crate::{CryptoError, CryptoScheme, CryptoSchemeId};

/// Smallest chunk size the format accepts (guardrail).
pub const MIN_CHUNK_SIZE: usize = 64 * 1024;
/// Largest chunk size the format accepts (guardrail).
pub const MAX_CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// Default chunk size (foundation D7: 256 KiB–1 MiB operating range).
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;
/// v2 container header size in bytes.
pub const HEADER_SIZE: usize = 34;
/// AES-GCM tag size appended to every chunk.
pub const TAG_SIZE: usize = 16;
/// Magic bytes opening every v2 container.
pub const MAGIC: [u8; 8] = *b"CKCRYPT2";
/// Current container format version byte.
pub const VERSION: u8 = 0x01;

/// Chunked STREAM-construction AES-256-GCM scheme (v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AeadV2 {
    chunk_size: usize,
}

impl AeadV2 {
    /// Creates the scheme with the default 1 MiB chunk size.
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Creates the scheme with a specific plaintext chunk size.
    ///
    /// Fails with [`CryptoError::InvalidChunkSize`] outside
    /// [`MIN_CHUNK_SIZE`]..=[`MAX_CHUNK_SIZE`].
    pub fn with_chunk_size(chunk_size: usize) -> Result<Self, CryptoError> {
        Ok(Self { chunk_size })
    }

    /// The configured plaintext chunk size in bytes.
    pub const fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// In-memory whole-file encryption.
    pub fn encrypt(&self, _password: &str, _plaintext: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// In-memory whole-file decryption.
    pub fn decrypt(&self, _password: &str, _ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(Vec::new())
    }

    /// In-memory random-access decryption of a half-open plaintext range.
    pub fn decrypt_range(
        &self,
        _password: &str,
        _ciphertext: &[u8],
        _range: Range<u64>,
    ) -> Result<Vec<u8>, CryptoError> {
        Ok(Vec::new())
    }
}

impl Default for AeadV2 {
    fn default() -> Self {
        Self::new()
    }
}

impl CryptoScheme for AeadV2 {
    fn id(&self) -> CryptoSchemeId {
        CryptoSchemeId::AeadV2
    }

    fn encrypt_stream(
        &self,
        _password: &str,
        _src: &mut dyn Read,
        _dst: &mut dyn Write,
    ) -> Result<u64, CryptoError> {
        Ok(0)
    }

    fn decrypt_stream(
        &self,
        _password: &str,
        _src: &mut dyn Read,
        _dst: &mut dyn Write,
    ) -> Result<(), CryptoError> {
        Ok(())
    }

    fn decrypt_range(
        &self,
        password: &str,
        ciphertext: &[u8],
        range: Range<u64>,
    ) -> Result<Vec<u8>, CryptoError> {
        AeadV2::decrypt_range(self, password, ciphertext, range)
    }
}
