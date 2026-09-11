//! v2 chunked AEAD container — STREAM construction (foundation D7).
//!
//! # Container format
//!
//! ```text
//! offset size  field
//! 0      8     magic = b"CKCRYPT2"
//! 8      1     version = 0x01
//! 9      1     reserved = 0x00 (must be zero; bound by the chunk AAD)
//! 10     16    salt (fresh random per encryption)
//! 26     4     PBKDF2-HMAC-SHA256 iteration count, u32 big-endian
//! 30     4     plaintext chunk size in bytes, u32 big-endian
//!                 (guardrails: 64 KiB..=4 MiB, see [MIN_CHUNK_SIZE])
//! 34     ...   chunk stream
//! ```
//!
//! Chunk `i` (0-based) holds up to `chunk_size` plaintext bytes sealed
//! independently with AES-256-GCM under the PBKDF2-derived key:
//!
//! - **nonce** = `00 00 00 00 || counter_be56(i) || final_flag` — a
//!   4-byte zero prefix, the chunk counter in 7 big-endian bytes (2^56
//!   chunk capacity), and one flag byte (`0x01` for the final chunk, else
//!   `0x00`). The flag lives *inside* the authenticated nonce, so the
//!   nonce spaces of final and non-final chunks are disjoint: dropping the
//!   last chunk makes the new tail decrypt under the wrong flag and fail
//!   authentication (truncation defense, STREAM construction).
//! - **AAD** = the full 34-byte header: salt, KDF parameters and chunk
//!   size are cryptographically bound to every chunk, so splicing a header
//!   from another container (or editing any header field) fails auth.
//! - **ciphertext** = `GCM(ct || tag16)`: every non-final chunk occupies
//!   exactly `chunk_size + 16` bytes; the final chunk holds
//!   `1..=chunk_size + 16` (exactly 16 — tag only — when the whole
//!   plaintext is empty: an empty file is a single empty final chunk).
//!
//! The final chunk is the only short one; an exact multiple of the chunk
//! size ends with a *full* final chunk (no trailing empty chunk except for
//! the empty file). The structure is self-delimiting from the total
//! length: `n_chunks - 1 = (body_len - 16) / (chunk_size + 16)`, validated
//! so the derived final-chunk length lands in `0..=chunk_size` (with 0
//! legal only for the single-chunk/empty form). This is how
//! [`AeadV2::decrypt_range`] locates chunks without reading them.
//!
//! # Key derivation
//!
//! Same family as v1 by default: PBKDF2-HMAC-SHA256, 100 000 iterations
//! ([`crate::v1::PBKDF2_ITERATIONS`]), fresh 16-byte salt per encryption.
//! Unlike v1 (hardcoded, frozen), the iteration count travels in the
//! header so the KDF can evolve without a format break — bounded by
//! [`MAX_HEADER_ITERATIONS`] so an untrusted header cannot amplify the
//! KDF cost (forged headers above the cap are rejected at parse, before
//! any PBKDF2 work; owner fix directive 2026-09-08). Cross-format key
//! and nonce collisions are negligible: keys coincide only on a 2^-128
//! salt collision between containers.
//!
//! # Semantics summary
//!
//! - Whole-ciphertext decryption ignores the reader's configured chunk
//!   size — the container is self-describing (see the
//!   `chunk_size_is_configurable_and_changes_chunking` test).
//! - [`AeadV2::decrypt_range`] decrypts only the chunks overlapping the
//!   requested half-open plaintext range (plus header parsing), with
//!   storage-Range clamping (see the trait docs).
//! - All tampering fails closed: `AuthFailed` for anything that reaches
//!   GCM verification, `Malformed`/`BadMagic`/`UnsupportedVersion` for
//!   structurally broken containers, `TooShort` for sub-header input.

use std::io::{ErrorKind, Read, Write};
use std::ops::Range;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use pbkdf2::pbkdf2_hmac;
use rand::random;
use sha2::Sha256;

use crate::{clamp_range, CryptoError, CryptoScheme, CryptoSchemeId};

/// Smallest chunk size the format accepts (guardrail: bounds the per-chunk
/// overhead ratio and the random-access decryption amplification).
pub const MIN_CHUNK_SIZE: usize = 64 * 1024;
/// Largest chunk size the format accepts (guardrail: bounds streaming
/// working memory at chunk_size + fixed overhead).
pub const MAX_CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// Default chunk size (foundation D7 operating range 256 KiB–1 MiB).
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;
/// v2 container header size in bytes.
pub const HEADER_SIZE: usize = 34;
/// AES-GCM tag size appended to every chunk.
pub const TAG_SIZE: usize = 16;
/// Magic bytes opening every v2 container.
pub const MAGIC: [u8; 8] = *b"CKCRYPT2";
/// Current container format version byte.
pub const VERSION: u8 = 0x01;
/// Largest PBKDF2 iteration count accepted from an untrusted header
/// (guardrail: caps forged-header KDF amplification at 10x the default
/// [`crate::v1::PBKDF2_ITERATIONS`] cost while leaving the in-header KDF
/// room to evolve; headers above it are `Malformed` before any PBKDF2
/// work runs).
pub const MAX_HEADER_ITERATIONS: u32 = 1_000_000;
/// Chunk counter capacity of the nonce layout (2^56 chunks).
const MAX_CHUNKS: u64 = 1 << 56;

/// Parsed container header.
struct Header {
    salt: [u8; 16],
    iterations: u32,
    chunk_size: usize,
    /// Exact encoded bytes — reused as the per-chunk AAD.
    bytes: [u8; HEADER_SIZE],
}

impl Header {
    fn encode(salt: [u8; 16], iterations: u32, chunk_size: usize) -> [u8; HEADER_SIZE] {
        let mut h = [0u8; HEADER_SIZE];
        h[..8].copy_from_slice(&MAGIC);
        h[8] = VERSION;
        // h[9] reserved = 0
        h[10..26].copy_from_slice(&salt);
        h[26..30].copy_from_slice(&iterations.to_be_bytes());
        h[30..34].copy_from_slice(&(chunk_size as u32).to_be_bytes());
        h
    }

    /// Parses and validates the fixed header of an in-memory container.
    fn parse(ciphertext: &[u8]) -> Result<Header, CryptoError> {
        if ciphertext.len() < HEADER_SIZE {
            return Err(CryptoError::TooShort);
        }
        if ciphertext[..8] != MAGIC {
            return Err(CryptoError::BadMagic);
        }
        if ciphertext[8] != VERSION {
            return Err(CryptoError::UnsupportedVersion(ciphertext[8]));
        }
        if ciphertext[9] != 0 {
            return Err(CryptoError::Malformed);
        }
        let iterations = u32::from_be_bytes(ciphertext[26..30].try_into().unwrap());
        if iterations == 0 || iterations > MAX_HEADER_ITERATIONS {
            // Untrusted input: reject the KDF-DoS amplification range
            // structurally, before any PBKDF2 work.
            return Err(CryptoError::Malformed);
        }
        let chunk_size = u32::from_be_bytes(ciphertext[30..34].try_into().unwrap()) as usize;
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size) {
            // Untrusted input: never borrow the constructor's error class.
            return Err(CryptoError::Malformed);
        }
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&ciphertext[10..26]);
        Ok(Header {
            salt,
            iterations,
            chunk_size,
            bytes: ciphertext[..HEADER_SIZE].try_into().unwrap(),
        })
    }

    fn derive_key(&self, password: &str) -> [u8; 32] {
        derive_key(password, &self.salt, self.iterations)
    }
}

/// PBKDF2-HMAC-SHA256 → AES-256 key (v1 KDF family, iteration count from
/// the container header).
fn derive_key(password: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut key);
    key
}

/// Derived chunk geometry of a complete container.
struct Layout {
    n_chunks: usize,
    /// Plaintext length of the final chunk (`0` only for the empty file).
    last_plain: usize,
    plain_len: usize,
}

impl Layout {
    /// Derives the chunk geometry from the ciphertext body length.
    ///
    /// `n_chunks - 1 = (body_len - TAG) / (chunk + TAG)` is the unique
    /// candidate satisfying `last_plain ∈ 0..=chunk_size` (the interval is
    /// half-open of length < 1), so the structure is self-delimiting.
    fn derive(body_len: usize, chunk_size: usize) -> Result<Layout, CryptoError> {
        if body_len < TAG_SIZE {
            // Even the empty file carries one tag-only final chunk.
            return Err(CryptoError::Malformed);
        }
        let stride = chunk_size + TAG_SIZE;
        let n_chunks = (body_len - TAG_SIZE) / stride + 1;
        if n_chunks as u64 >= MAX_CHUNKS {
            return Err(CryptoError::Malformed);
        }
        let last_plain = body_len - TAG_SIZE - (n_chunks - 1) * stride;
        if last_plain > chunk_size || (last_plain == 0 && n_chunks > 1) {
            // Out-of-range tail, or a non-canonical trailing empty chunk
            // (the encoder only emits an empty final chunk for empty files).
            return Err(CryptoError::Malformed);
        }
        Ok(Layout {
            n_chunks,
            last_plain,
            plain_len: (n_chunks - 1) * chunk_size + last_plain,
        })
    }

    /// Offset/length of chunk `i`'s ciphertext within the container.
    fn chunk_span(&self, i: usize, chunk_size: usize) -> (usize, usize) {
        if i + 1 == self.n_chunks {
            (
                HEADER_SIZE + i * (chunk_size + TAG_SIZE),
                self.last_plain + TAG_SIZE,
            )
        } else {
            (
                HEADER_SIZE + i * (chunk_size + TAG_SIZE),
                chunk_size + TAG_SIZE,
            )
        }
    }
}

/// Builds the per-chunk GCM nonce: zero prefix || counter_be56 || final
/// flag (STREAM domain separation between final and non-final chunks).
fn chunk_nonce(index: usize, is_last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..11].copy_from_slice(&((index as u64).to_be_bytes())[1..]);
    n[11] = u8::from(is_last);
    n
}

/// Chunked STREAM-construction AES-256-GCM scheme (v2).
///
/// Configuration is encryption-side only ([`AeadV2::with_chunk_size`]);
/// decryption always honors the container's own header. The scheme is
/// stateless — instances can be freely shared (`Send + Sync` via the
/// trait).
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
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size) {
            return Err(CryptoError::InvalidChunkSize(chunk_size));
        }
        Ok(Self { chunk_size })
    }

    /// The configured plaintext chunk size in bytes.
    pub const fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// In-memory whole-file encryption.
    pub fn encrypt(&self, password: &str, plaintext: &[u8]) -> Vec<u8> {
        let n_chunks = plaintext.len().div_ceil(self.chunk_size).max(1);
        let mut out = Vec::with_capacity(HEADER_SIZE + plaintext.len() + n_chunks * TAG_SIZE);
        let mut cursor = std::io::Cursor::new(plaintext);
        let written = self
            .encrypt_stream(password, &mut cursor, &mut out)
            .expect("in-memory encryption cannot fail on Vec/Cursor");
        debug_assert_eq!(written as usize, out.len());
        out
    }

    /// In-memory whole-file decryption.
    pub fn decrypt(&self, password: &str, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let header = Header::parse(ciphertext)?;
        let layout = Layout::derive(ciphertext.len() - HEADER_SIZE, header.chunk_size)?;
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(header.derive_key(password)));
        let mut out = Vec::with_capacity(layout.plain_len);
        for i in 0..layout.n_chunks {
            let (off, len) = layout.chunk_span(i, header.chunk_size);
            let is_last = i + 1 == layout.n_chunks;
            let chunk = &ciphertext[off..off + len];
            out.extend(decrypt_chunk(&cipher, &header.bytes, i, is_last, chunk)?);
        }
        Ok(out)
    }

    /// In-memory random-access decryption of a half-open plaintext range
    /// (clamped, see the trait docs): only the overlapping chunks plus the
    /// header are processed.
    pub fn decrypt_range(
        &self,
        password: &str,
        ciphertext: &[u8],
        range: Range<u64>,
    ) -> Result<Vec<u8>, CryptoError> {
        let header = Header::parse(ciphertext)?;
        let layout = Layout::derive(ciphertext.len() - HEADER_SIZE, header.chunk_size)?;
        let (start, end) = clamp_range(range.start, range.end, layout.plain_len as u64);
        if start == end {
            return Ok(Vec::new());
        }
        let chunk_size = header.chunk_size;
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(header.derive_key(password)));

        let first = (start / chunk_size as u64) as usize;
        let last = ((end - 1) / chunk_size as u64) as usize;
        let mut covered = Vec::with_capacity(((end - start) as usize).saturating_add(chunk_size));
        for i in first..=last {
            let (off, len) = layout.chunk_span(i, chunk_size);
            let is_last = i + 1 == layout.n_chunks;
            let chunk = &ciphertext[off..off + len];
            covered.extend(decrypt_chunk(&cipher, &header.bytes, i, is_last, chunk)?);
        }
        // `covered` is the plaintext of chunks first..=last concatenated;
        // slice out the requested window within it.
        let skip = (start - first as u64 * chunk_size as u64) as usize;
        let take = (end - start) as usize;
        Ok(covered[skip..skip + take].to_vec())
    }

    /// Streams `src` (read to EOF) into a complete container in `dst`.
    ///
    /// Working set: one chunk-size plaintext buffer plus a one-byte
    /// lookahead — the final chunk is only sealed once EOF is proven, so
    /// no trailing empty chunk is emitted for exact multiples.
    fn encrypt_into(
        &self,
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<u64, CryptoError> {
        let salt: [u8; 16] = random();
        let iterations = crate::v1::PBKDF2_ITERATIONS;
        let header_bytes = Header::encode(salt, iterations, self.chunk_size);
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(derive_key(
            password, &salt, iterations,
        )));

        dst.write_all(&header_bytes)?;
        let mut wrote = HEADER_SIZE as u64;

        let chunk_size = self.chunk_size;
        let mut buf = vec![0u8; chunk_size];
        let mut peek = [0u8; 1];
        let mut carry = false;
        let mut index: u64 = 0;

        loop {
            // Fill one chunk buffer, looping past short reads; only a
            // zero-length read (EOF) ends the fill early.
            let mut filled = usize::from(carry);
            if carry {
                buf[0] = peek[0];
                carry = false;
            }
            while filled < chunk_size {
                match src.read(&mut buf[filled..chunk_size]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }

            // Finality: a full buffer still cannot tell — look one byte
            // ahead. A short fill means EOF was reached during the fill,
            // so this chunk (possibly empty, for an empty file) is final.
            // An empty fill after the first chunk instead means the
            // previous chunk was the sealed final one: break below.
            let is_last = if filled == chunk_size {
                let mut last = false;
                loop {
                    match src.read(&mut peek[..]) {
                        Ok(0) => {
                            last = true;
                            break;
                        }
                        Ok(_) => {
                            carry = true;
                            break;
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                last
            } else {
                true
            };

            if filled == 0 && index > 0 {
                break; // EOF right after a sealed final chunk
            }
            debug_assert!(index < MAX_CHUNKS);
            let ct = cipher
                .encrypt(
                    &Nonce::from(chunk_nonce(index as usize, is_last)),
                    Payload {
                        msg: &buf[..filled],
                        aad: &header_bytes,
                    },
                )
                .expect("AES-256-GCM encryption of an in-memory chunk cannot fail");
            dst.write_all(&ct)?;
            wrote += ct.len() as u64;
            index += 1;
            if is_last {
                break;
            }
        }
        Ok(wrote)
    }

    /// Streams a complete container from `src` into plaintext in `dst`.
    ///
    /// Working set: one chunk-size ciphertext buffer plus a one-byte
    /// lookahead (finality detection mirrors the encrypt side).
    fn decrypt_into(
        &self,
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<(), CryptoError> {
        // Read and validate the fixed header.
        let mut header_bytes = [0u8; HEADER_SIZE];
        read_exact_retrying(src, &mut header_bytes)?;
        let header = Header::parse(&header_bytes)?;

        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(header.derive_key(password)));
        let chunk_size = header.chunk_size;
        let mut buf = vec![0u8; chunk_size + TAG_SIZE];
        let mut peek = [0u8; 1];
        let mut carry = false;
        let mut index: u64 = 0;

        loop {
            let mut filled = usize::from(carry);
            if carry {
                buf[0] = peek[0];
                carry = false;
            }
            while filled < buf.len() {
                match src.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if filled == 0 {
                // No bytes at all: the previous chunk was final (already
                // written). A header-only stream is malformed (an empty
                // file still carries one tag-only chunk).
                return if index == 0 {
                    Err(CryptoError::Malformed)
                } else {
                    Ok(())
                };
            }
            let is_last = if filled == buf.len() {
                let mut last = false;
                loop {
                    match src.read(&mut peek[..]) {
                        Ok(0) => {
                            last = true;
                            break;
                        }
                        Ok(_) => {
                            carry = true;
                            break;
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                last
            } else {
                // Short fill = EOF: this must be the final chunk, and a
                // final chunk always carries at least the tag.
                if filled < TAG_SIZE {
                    return Err(CryptoError::Malformed);
                }
                true
            };
            debug_assert!(index < MAX_CHUNKS);
            let pt = decrypt_chunk(
                &cipher,
                &header.bytes,
                index as usize,
                is_last,
                &buf[..filled],
            )?;
            dst.write_all(&pt)?;
            index += 1;
            if is_last {
                return Ok(());
            }
        }
    }
}

/// A keyed chunk-window reader over an aead_v2 container (K47): parses
/// the 34-byte header and derives the key once, then answers per-chunk
/// authenticated decrypt requests and the container layout arithmetic
/// random-access consumers need to translate plaintext windows into
/// ciphertext spans. Created via [`AeadV2::window_reader`].
pub struct AeadV2Window {
    cipher: Aes256Gcm,
    header_bytes: [u8; HEADER_SIZE],
    chunk_size: usize,
}

// Compile-time contract for the core decrypting-transport wrapper, which
// shares one window across concurrent face reads.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AeadV2Window>();
};

impl AeadV2 {
    /// Opens a chunk-window reader over the fixed 34-byte header of a
    /// container: parses and validates the header and runs the PBKDF2
    /// key derivation exactly once — every later window decrypt reuses
    /// the derived key.
    pub fn window_reader(
        &self,
        password: &str,
        header: &[u8],
    ) -> Result<AeadV2Window, CryptoError> {
        AeadV2Window::open(password, header)
    }
}

impl AeadV2Window {
    /// Parses the header (at least [`HEADER_SIZE`] bytes; only the first
    /// 34 are read) and derives the key once.
    pub fn open(password: &str, header: &[u8]) -> Result<Self, CryptoError> {
        let header = Header::parse(header)?;
        let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(header.derive_key(password)));
        Ok(Self {
            cipher,
            header_bytes: header.bytes,
            chunk_size: header.chunk_size,
        })
    }

    /// The container's plaintext chunk size in bytes (header field).
    pub const fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Number of chunks a plaintext of `plain_len` bytes spans — the
    /// encoder's rule: ceiling division, with the empty file one empty
    /// final chunk and an exact multiple NO trailing empty chunk.
    pub fn n_chunks_for_plain(&self, plain_len: u64) -> u64 {
        plain_len.div_ceil(self.chunk_size as u64).max(1)
    }

    /// Plaintext length of the final chunk for a `plain_len`-byte file
    /// (0 only for the empty file; a full `chunk_size` on exact
    /// multiples).
    pub fn plain_tail(&self, plain_len: u64) -> usize {
        if plain_len == 0 {
            return 0;
        }
        let rem = (plain_len % self.chunk_size as u64) as usize;
        if rem == 0 {
            self.chunk_size
        } else {
            rem
        }
    }

    /// Ciphertext `(offset, length)` covering chunks `first..=last` of a
    /// `plain_len`-byte container, header included — the coordinates a
    /// random-access consumer forwards to storage (`open_range(off,
    /// len)`), and the slice bounds within the whole container bytes.
    ///
    /// # Contract (review stream-M3, RB4)
    ///
    /// `first <= last && last < n_chunks_for_plain(plain_len)` — the
    /// span arithmetic is undefined for an out-of-file range. Every
    /// caller derives `first`/`last` from positions already clamped to
    /// the plaintext total (`WindowSteps.next_window` clamps its window
    /// to `end <= total`; the webdav `RangeFile` and winfsp `WindowReader`
    /// clamp to `total_size`), and the tuple return keeps the function a
    /// pure layout expression — so the precondition is pinned by a
    /// debug assertion rather than widened into a `Result` whose error
    /// arm no honest caller could reach.
    pub fn ciphertext_span(&self, first: u64, last: u64, plain_len: u64) -> (u64, u64) {
        let n = self.n_chunks_for_plain(plain_len);
        debug_assert!(first <= last && last < n, "chunk range outside the file");
        let stride = (self.chunk_size + TAG_SIZE) as u64;
        let offset = HEADER_SIZE as u64 + first * stride;
        // The final chunk carries plain_tail + tag bytes; every earlier
        // chunk is full-width.
        let last_len = if last + 1 == n {
            self.plain_tail(plain_len) as u64 + TAG_SIZE as u64
        } else {
            stride
        };
        (offset, (last - first) * stride + last_len)
    }

    /// Decrypts and authenticates one chunk (`ct` = ciphertext + tag
    /// exactly as addressed by [`AeadV2Window::ciphertext_span`]).
    /// Tampering, a wrong index/finality or a wrong password is
    /// [`CryptoError::AuthFailed`] — never wrong plaintext. An `index`
    /// at/past [`MAX_CHUNKS`] is [`CryptoError::Malformed`]: the nonce
    /// counter cannot address it, so the input is outside the format's
    /// representable domain (review stream-M3, RB4 — a runtime refusal,
    /// not a debug-only assertion).
    pub fn decrypt_chunk(
        &self,
        index: u64,
        is_last: bool,
        ct: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        if index >= MAX_CHUNKS {
            return Err(CryptoError::Malformed);
        }
        decrypt_chunk(
            &self.cipher,
            &self.header_bytes,
            index as usize,
            is_last,
            ct,
        )
    }
}

/// Decrypts one authenticated chunk under the header AAD.
fn decrypt_chunk(
    cipher: &Aes256Gcm,
    header_bytes: &[u8; HEADER_SIZE],
    index: usize,
    is_last: bool,
    chunk: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    cipher
        .decrypt(
            &Nonce::from(chunk_nonce(index, is_last)),
            Payload {
                msg: chunk,
                aad: header_bytes,
            },
        )
        .map_err(|_| CryptoError::AuthFailed)
}

/// `read_exact` with `Interrupted` retry (the std version does not retry).
fn read_exact_retrying(src: &mut dyn Read, buf: &mut [u8]) -> Result<(), CryptoError> {
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => return Err(CryptoError::TooShort),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
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
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<u64, CryptoError> {
        self.encrypt_into(password, src, dst)
    }

    fn decrypt_stream(
        &self,
        password: &str,
        src: &mut dyn Read,
        dst: &mut dyn Write,
    ) -> Result<(), CryptoError> {
        self.decrypt_into(password, src, dst)
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
