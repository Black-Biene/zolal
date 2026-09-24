//! Encryption envelope — markerless, streaming, authenticated.
//!
//! ## Region layout
//!
//! ```text
//! [ salt : 16 ][ nonce_base : 19 ][ chunk_0 ][ chunk_1 ] ... [ chunk_n (final) ]
//!                                  each chunk = up to 64 KiB ciphertext + 16-byte tag
//! ```
//!
//! Every byte is random or indistinguishable from random:
//!
//! - `salt` and `nonce_base` are plaintext but uniformly random, so they leak nothing. One
//!   exception, worth 2⁻¹⁶ of entropy: a salt never begins `FF D8` (a JPEG start-of-image), so
//!   a JPEG trailer region can never be mistaken for a chained MPF image. See [`stream`].
//! - **There is no length field.** The region boundary already gives the length (EOF for a JPEG
//!   trailer, the box size for MP4, `/Length` for PDF), so storing it again would only add a
//!   distinguisher. The chunk count, and so which chunk is last, is derived from it.
//! - **No version byte in the clear.** Format version and cipher suite live in the *first
//!   plaintext bytes of chunk 0*, inside the encryption — a cleartext version marker would be a
//!   fingerprint that makes files identifiable as ours.
//! - **No KDF parameters anywhere.** They can't go inside the encryption (they are needed to
//!   derive the key that opens it) and they can't go outside (a fingerprint). The envelope
//!   version implies them; see [`envelope::kdf_candidates`].
//!
//! ## Why streaming
//!
//! A one-shot AEAD would force the entire payload into RAM. The STREAM construction
//! ([`aead_stream`], BE32) binds each chunk's index into its nonce and marks the last chunk as
//! terminal, so **truncation and reordering are detected**, while peak memory stays at one chunk.
//!
//! It also makes ciphertext length a deterministic function of plaintext length — see
//! [`ciphertext_len`] — which is what allows a single-pass write for MP4 and PDF, where the
//! region size must be committed to the file *before* the bytes are produced.

pub mod envelope;
pub mod kdf;
pub mod stream;

/// Plaintext bytes per chunk.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Poly1305 authentication tag length.
pub const TAG_LEN: u64 = 16;
/// Argon2id salt length.
pub const SALT_LEN: u64 = 16;
/// STREAM nonce prefix length (XChaCha20's 24-byte nonce minus the 5 bytes BE32 uses
/// for the chunk counter and last-block flag).
pub const NONCE_PREFIX_LEN: u64 = 19;
/// Plaintext header written before the payload, inside chunk 0: version + suite id.
pub const INNER_HEADER_LEN: u64 = 2;
/// One sealed chunk on disk: [`CHUNK_SIZE`] bytes of ciphertext plus its tag.
pub const SEALED_CHUNK_LEN: u64 = CHUNK_SIZE as u64 + TAG_LEN;
/// The smallest region that could possibly be an envelope (an empty payload).
///
/// Anything shorter is structurally not ours, so reveal skips it without spending a key
/// derivation on it.
pub const MIN_REGION_LEN: u64 = SALT_LEN + NONCE_PREFIX_LEN + INNER_HEADER_LEN + TAG_LEN;

/// Ciphertext length produced for `plaintext_len` bytes.
///
/// Exact, not an estimate — [`crate::hide`] relies on it to commit a region size up front.
pub fn ciphertext_len(plaintext_len: u64) -> u64 {
    let total = plaintext_len + INNER_HEADER_LEN;
    let chunks = total.div_ceil(CHUNK_SIZE as u64).max(1);
    total + chunks * TAG_LEN
}

/// Total region size for a payload of `plaintext_len` bytes, including the salt and nonce prefix.
pub fn region_len(plaintext_len: u64) -> u64 {
    SALT_LEN + NONCE_PREFIX_LEN + ciphertext_len(plaintext_len)
}

/// Bytes added on top of the raw payload. Used by [`crate::plausibility`].
pub fn overhead_for(plaintext_len: u64) -> u64 {
    region_len(plaintext_len) - plaintext_len
}
