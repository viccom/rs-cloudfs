//! v2 chunked AEAD (STREAM construction) test surface — Batch E E-2.
//!
//! Covers: roundtrip (empty / single-chunk / cross-chunk / EOF edges),
//! container layout contracts, fail-closed tamper behavior (bit flips /
//! truncation / forged header / swapped chunk order), cross-chunk
//! `decrypt_range` equivalence with full-decrypt slicing (clamped half-open
//! semantics), streaming I/O granularity (constant-memory shape), and the
//! chunk-size guardrails.

use std::io::{Cursor, Read, Write};

use cloudkit_crypto::v1;
use cloudkit_crypto::v2::{
    AeadV2, DEFAULT_CHUNK_SIZE, HEADER_SIZE, MAGIC, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE, TAG_SIZE,
    VERSION,
};
use cloudkit_crypto::{CryptoError, CryptoScheme};

const PW: &str = "aead v2 测试密码 🔐";
/// Smallest legal chunk size keeps multi-chunk fixtures small.
const S: usize = MIN_CHUNK_SIZE;

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Clamps a half-open range like the documented scheme semantics.
fn clamped_slice(pt: &[u8], start: u64, end: u64) -> Vec<u8> {
    let len = pt.len() as u64;
    let end = end.min(len);
    let start = start.min(end);
    pt[start as usize..end as usize].to_vec()
}

fn scheme_small() -> AeadV2 {
    AeadV2::with_chunk_size(S).expect("min chunk size is legal")
}

// ------------------------------------------------------------- roundtrip ---

#[test]
fn roundtrip_in_memory_across_sizes() {
    // 0=empty file, 1/100=within one chunk, S=exact single chunk,
    // S+1=spill into second chunk, 2S=exact multiple, 2.5 chunks, 3+odd.
    let sizes = [0usize, 1, 100, S, S + 1, 2 * S, 2 * S + S / 2, 3 * S + 17];
    for &n in &sizes {
        let pt = pattern(n);
        let ct = scheme_small().encrypt(PW, &pt);
        let got = scheme_small()
            .decrypt(PW, &ct)
            .unwrap_or_else(|e| panic!("roundtrip failed for size {n}: {e}"));
        assert_eq!(got, pt, "roundtrip mismatch for size {n}");
    }
}

#[test]
fn roundtrip_stream_via_trait_object() {
    let scheme = scheme_small();
    let dyn_scheme: &dyn CryptoScheme = &scheme;
    for &n in &[0usize, 64, S, 2 * S + 5] {
        let pt = pattern(n);
        let mut src = Cursor::new(pt.clone());
        let mut ct = Vec::new();
        let written = dyn_scheme
            .encrypt_stream(PW, &mut src, &mut ct)
            .expect("encrypt_stream");
        assert_eq!(written as usize, ct.len(), "returned length must match");

        let mut out = Vec::new();
        dyn_scheme
            .decrypt_stream(PW, &mut Cursor::new(&ct), &mut out)
            .expect("decrypt_stream");
        assert_eq!(out, pt, "stream roundtrip mismatch for size {n}");
    }
}

// ------------------------------------------------------ layout / format ---

#[test]
fn container_layout_and_overhead() {
    for &n in &[0usize, 1, S, S + 1, 2 * S + 3] {
        let pt = pattern(n);
        let ct = scheme_small().encrypt(PW, &pt);
        let n_chunks = n.div_ceil(S).max(1);
        assert_eq!(
            ct.len(),
            HEADER_SIZE + n + n_chunks * TAG_SIZE,
            "total length for size {n}"
        );
        assert_eq!(&ct[..8], &MAGIC, "magic");
        assert_eq!(ct[8], VERSION, "version byte");
        assert_eq!(ct[9], 0, "reserved byte");
        let iters = u32::from_be_bytes(ct[26..30].try_into().unwrap());
        assert_eq!(iters, v1::PBKDF2_ITERATIONS, "KDF defaults track v1");
        let chunk_field = u32::from_be_bytes(ct[30..34].try_into().unwrap());
        assert_eq!(chunk_field as usize, S, "chunk size field");
    }
}

#[test]
fn fresh_salt_per_encryption() {
    let pt = pattern(1000);
    let a = scheme_small().encrypt(PW, &pt);
    let b = scheme_small().encrypt(PW, &pt);
    assert_ne!(&a[10..26], &b[10..26], "salt must be random");
    assert_ne!(a, b, "ciphertexts must differ");
}

// ---------------------------------------------------- tamper / fail-closed ---

#[test]
fn wrong_password_is_auth_failed() {
    let ct = scheme_small().encrypt(PW, &pattern(S + 10));
    assert!(matches!(
        scheme_small().decrypt("not the password", &ct),
        Err(CryptoError::AuthFailed)
    ));
}

#[test]
fn flipped_bit_in_chunk_body_is_rejected() {
    let ct = scheme_small().encrypt(PW, &pattern(3 * S));
    // Flip inside chunk 1's ciphertext body (past chunk 0's ct+tag).
    let mut tampered = ct.clone();
    let off = HEADER_SIZE + (S + TAG_SIZE) + 10;
    tampered[off] ^= 0xFF;
    assert!(
        matches!(
            scheme_small().decrypt(PW, &tampered),
            Err(CryptoError::AuthFailed)
        ),
        "flipped ciphertext byte must fail authentication"
    );
}

#[test]
fn flipped_tag_byte_is_rejected() {
    let ct = scheme_small().encrypt(PW, &pattern(S + 10));
    let mut tampered = ct.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(matches!(
        scheme_small().decrypt(PW, &tampered),
        Err(CryptoError::AuthFailed)
    ));
}

#[test]
fn truncation_at_chunk_boundary_is_auth_failed() {
    let ct = scheme_small().encrypt(PW, &pattern(2 * S));
    // Keep only chunk 0: structurally valid, but it was sealed as
    // non-final — the final-flag nonce domain separation must catch it.
    let truncated = &ct[..HEADER_SIZE + S + TAG_SIZE];
    assert!(matches!(
        scheme_small().decrypt(PW, truncated),
        Err(CryptoError::AuthFailed)
    ));
}

#[test]
fn truncation_mid_chunk_is_rejected() {
    let ct = scheme_small().encrypt(PW, &pattern(2 * S));
    // Cut 5 bytes into chunk 1's ct+tag: either structurally inconsistent
    // (Malformed) or authentication-failed — both are fail-closed.
    let truncated = &ct[..HEADER_SIZE + S + TAG_SIZE + 5];
    assert!(matches!(
        scheme_small().decrypt(PW, truncated),
        Err(CryptoError::AuthFailed | CryptoError::Malformed)
    ));
    // Cutting into the header itself is TooShort territory.
    assert!(matches!(
        scheme_small().decrypt(PW, &ct[..HEADER_SIZE]),
        Err(CryptoError::TooShort | CryptoError::Malformed)
    ));
}

#[test]
fn forged_header_fields_are_rejected() {
    let ct = scheme_small().encrypt(PW, &pattern(S + 10));

    let mut bad_magic = ct.clone();
    bad_magic[0] ^= 0x01;
    assert!(matches!(
        scheme_small().decrypt(PW, &bad_magic),
        Err(CryptoError::BadMagic)
    ));

    let mut bad_version = ct.clone();
    bad_version[8] = VERSION + 1;
    assert!(matches!(
        scheme_small().decrypt(PW, &bad_version),
        Err(CryptoError::UnsupportedVersion(_))
    ));

    let mut bad_reserved = ct.clone();
    bad_reserved[9] = 0xFF;
    assert!(matches!(
        scheme_small().decrypt(PW, &bad_reserved),
        Err(CryptoError::Malformed)
    ));

    // Salt is bound via key + header AAD: swapping it must fail auth.
    let mut bad_salt = ct.clone();
    bad_salt[10] ^= 0x80;
    assert!(matches!(
        scheme_small().decrypt(PW, &bad_salt),
        Err(CryptoError::AuthFailed)
    ));

    // Iteration count is bound by AAD (and changes the key): rejected.
    let mut bad_iters = ct.clone();
    bad_iters[26] ^= 0x01;
    assert!(matches!(
        scheme_small().decrypt(PW, &bad_iters),
        Err(CryptoError::AuthFailed)
    ));

    // Chunk-size field inside the guardrails but different from what was
    // used: re-chunked structure cannot authenticate (AAD-bound).
    let mut bad_chunk = ct.clone();
    bad_chunk[30..34].copy_from_slice(&(2 * S as u32).to_be_bytes());
    assert!(matches!(
        scheme_small().decrypt(PW, &bad_chunk),
        Err(CryptoError::AuthFailed | CryptoError::Malformed)
    ));

    // Chunk-size field outside the guardrails: rejected structurally.
    let mut tiny_chunk = ct.clone();
    tiny_chunk[30..34].copy_from_slice(&1024u32.to_be_bytes());
    assert!(matches!(
        scheme_small().decrypt(PW, &tiny_chunk),
        Err(CryptoError::Malformed)
    ));
}

#[test]
fn forged_iteration_count_above_cap_is_rejected_before_kdf() {
    // KDF DoS clamp (owner fix directive 2026-09-08): an untrusted header
    // must not be able to amplify PBKDF2 work past the accepted cap. The
    // rejection happens structurally at header parse — before any KDF
    // work — so a forged 1_000_001-iteration header fails in Malformed,
    // not in AuthFailed after burning the derived-key cost.
    let ct = scheme_small().encrypt(PW, &pattern(S + 10));

    let mut absurd = ct.clone();
    absurd[26..30].copy_from_slice(&1_000_001u32.to_be_bytes());
    assert!(matches!(
        scheme_small().decrypt(PW, &absurd),
        Err(CryptoError::Malformed)
    ));

    let mut u32max = ct.clone();
    u32max[26..30].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        scheme_small().decrypt(PW, &u32max),
        Err(CryptoError::Malformed)
    ));
}

#[test]
fn swapped_chunk_order_is_rejected() {
    let ct = scheme_small().encrypt(PW, &pattern(3 * S));
    let ct_len = S + TAG_SIZE;
    let mut swapped = ct.clone();
    // Exchange chunk 0 and chunk 2 ciphertext blocks wholesale.
    let (a, b) = (HEADER_SIZE, HEADER_SIZE + 2 * ct_len);
    for i in 0..ct_len {
        swapped.swap(a + i, b + i);
    }
    assert!(matches!(
        scheme_small().decrypt(PW, &swapped),
        Err(CryptoError::AuthFailed)
    ));
}

// ---------------------------------------------------------- decrypt_range ---

#[test]
fn decrypt_range_matches_full_decrypt_slices() {
    let pt = pattern(2 * S + 1234);
    let ct = scheme_small().encrypt(PW, &pt);
    let scheme = scheme_small();

    let cases: &[(u64, u64)] = &[
        (0, 0),                                         // empty at start
        (1, 1),                                         // empty mid-file
        (3, 200),                                       // within chunk 0
        (S as u64 - 10, S as u64 + 10),                 // crosses the chunk boundary
        (S as u64 + 1, 2 * S as u64 + 5),               // spans chunks 1..2
        (2 * S as u64, pt.len() as u64),                // the whole short final chunk
        (pt.len() as u64 - 5, pt.len() as u64),         // EOF edge
        (0, pt.len() as u64),                           // full file
        (pt.len() as u64, pt.len() as u64 + 100),       // start at EOF: empty
        (pt.len() as u64 + 7, pt.len() as u64 + 99),    // start past EOF: empty
        (pt.len() as u64 - 100, pt.len() as u64 + 999), // end clamped
    ];
    for &(start, end) in cases {
        let got = scheme
            .decrypt_range(PW, &ct, start..end)
            .unwrap_or_else(|e| panic!("decrypt_range({start}..{end}) failed: {e}"));
        assert_eq!(
            got,
            clamped_slice(&pt, start, end),
            "decrypt_range({start}..{end})"
        );
    }
}

#[test]
fn decrypt_range_empty_file_yields_empty() {
    let ct = scheme_small().encrypt(PW, b"");
    let got = scheme_small()
        .decrypt_range(PW, &ct, 0..100)
        .expect("range on empty");
    assert!(got.is_empty());
}

// -------------------------------------------------- streaming memory shape ---

/// Reader that records the largest single `read` it was asked for.
struct GranularityReader<R> {
    inner: R,
    max_read: usize,
    total_read: u64,
}

impl<R: Read> GranularityReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            max_read: 0,
            total_read: 0,
        }
    }
}

impl<R: Read> Read for GranularityReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.max_read = self.max_read.max(buf.len());
        let n = self.inner.read(buf)?;
        self.total_read += n as u64;
        Ok(n)
    }
}

/// Writer that records the largest single `write` it received.
struct GranularityWriter {
    inner: Vec<u8>,
    max_write: usize,
}

impl GranularityWriter {
    fn new() -> Self {
        Self {
            inner: Vec::new(),
            max_write: 0,
        }
    }
}

impl Write for GranularityWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.max_write = self.max_write.max(buf.len());
        self.inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn streaming_io_never_exceeds_chunk_granularity() {
    // 9.5 MiB of plaintext through 1 MiB chunks: far larger than any single
    // chunk, so a whole-file-buffering implementation would have to ask for
    // the entire plaintext in one read (or emit it in one write).
    let n = 9 * DEFAULT_CHUNK_SIZE + DEFAULT_CHUNK_SIZE / 2;
    let pt = pattern(n);
    let scheme = AeadV2::new(); // default 1 MiB chunks

    let mut src = GranularityReader::new(Cursor::new(&pt));
    let mut sink = GranularityWriter::new();
    let written = scheme
        .encrypt_stream(PW, &mut src, &mut sink)
        .expect("encrypt_stream");
    assert_eq!(src.total_read as usize, n, "all plaintext consumed");
    assert!(
        src.max_read <= DEFAULT_CHUNK_SIZE,
        "encrypt reads must stay within one chunk, saw {}",
        src.max_read
    );
    assert!(
        sink.max_write <= DEFAULT_CHUNK_SIZE + TAG_SIZE,
        "encrypt writes must stay within one chunk + tag (+ header), saw {}",
        sink.max_write
    );
    assert_eq!(written as usize, sink.inner.len(), "returned length");
    let ct = sink.inner;

    let mut src = GranularityReader::new(Cursor::new(&ct));
    let mut out = GranularityWriter::new();
    scheme
        .decrypt_stream(PW, &mut src, &mut out)
        .expect("decrypt_stream");
    assert_eq!(out.inner, pt, "stream roundtrip must be byte-exact");
    assert!(
        src.max_read <= DEFAULT_CHUNK_SIZE + TAG_SIZE,
        "decrypt reads must stay within one chunk slot, saw {}",
        src.max_read
    );
    assert!(
        out.max_write <= DEFAULT_CHUNK_SIZE,
        "decrypt writes must stay within one chunk, saw {}",
        out.max_write
    );
}

// ------------------------------------------------------------ guardrails ---

#[test]
fn chunk_size_guardrails() {
    assert_eq!(MIN_CHUNK_SIZE, 64 * 1024, "floor");
    assert_eq!(MAX_CHUNK_SIZE, 4 * 1024 * 1024, "ceiling");
    assert_eq!(DEFAULT_CHUNK_SIZE, 1024 * 1024, "default 1 MiB");

    assert_eq!(AeadV2::new().chunk_size(), DEFAULT_CHUNK_SIZE);
    assert_eq!(
        AeadV2::with_chunk_size(MIN_CHUNK_SIZE)
            .expect("min is legal")
            .chunk_size(),
        MIN_CHUNK_SIZE
    );
    assert_eq!(
        AeadV2::with_chunk_size(MAX_CHUNK_SIZE)
            .expect("max is legal")
            .chunk_size(),
        MAX_CHUNK_SIZE
    );
    for &bad in &[
        0usize,
        1,
        MIN_CHUNK_SIZE - 1,
        MAX_CHUNK_SIZE + 1,
        usize::MAX,
    ] {
        assert!(
            matches!(
                AeadV2::with_chunk_size(bad),
                Err(CryptoError::InvalidChunkSize(got)) if got == bad
            ),
            "chunk size {bad} must be rejected"
        );
    }
}

#[test]
fn chunk_size_is_configurable_and_changes_chunking() {
    let pt = pattern(2 * MIN_CHUNK_SIZE + 5);
    let small = AeadV2::with_chunk_size(MIN_CHUNK_SIZE).expect("legal");
    let big = AeadV2::with_chunk_size(MAX_CHUNK_SIZE).expect("legal");

    let ct_small = small.encrypt(PW, &pt);
    let ct_big = big.encrypt(PW, &pt);
    // 3 chunks vs 1 chunk: two extra tags of overhead.
    assert_eq!(ct_small.len() - ct_big.len(), 2 * TAG_SIZE);
    assert_eq!(small.decrypt(PW, &ct_small).unwrap(), pt);
    assert_eq!(big.decrypt(PW, &ct_big).unwrap(), pt);
    // A ciphertext must decrypt under any conforming configuration: the
    // container's own chunk size governs, not the reader's.
    assert_eq!(
        big.decrypt(PW, &ct_small).unwrap(),
        pt,
        "reader config must not override container chunking"
    );
}
