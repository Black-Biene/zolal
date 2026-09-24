//! Chunked streaming AEAD (XChaCha20-Poly1305, STREAM/BE32).
//!
//! ## Chunking
//!
//! The plaintext is the 2-byte [`InnerHeader`] followed by the payload. It is cut into
//! [`CHUNK_SIZE`] pieces; only the last may be shorter (1 to `CHUNK_SIZE` bytes). Each piece is
//! sealed with nonce `prefix ‖ index (u32 BE) ‖ last-flag`, so a chunk only authenticates at
//! its own position, and only the true final chunk authenticates as final.
//!
//! The chunk count is not stored: it follows from the region length, which the container gives
//! us. That is what makes truncation fatal. Cut a stream short and its new "last" chunk was
//! sealed as non-final, so its tag fails.
//!
//! ## Why the stateless primitive
//!
//! We drive [`StreamBE32`] with explicit positions rather than through `aead-stream`'s
//! `Encryptor`/`Decryptor` wrappers. The construction and nonces are identical. The wrappers
//! assume you learn which chunk is last only when the input ends; we know it up front, and
//! [`OpenReader::new`] needs to retry chunk 0 under the other last-flag to tell a wrong
//! passphrase apart from a stream cut off right after its first chunk.
//!
//! ## Partial plaintext
//!
//! Every chunk is authenticated before a byte of it is released, but truncation is only
//! provable at the end. So a reader of [`OpenReader`] must treat what it has written as
//! **uncommitted until the reader returns end-of-stream**. [`crate::reveal`] does that by
//! extracting into a staging directory that is deleted on any error.

use std::io::{self, Read, Write};

use aead_stream::{NewStream, StreamBE32, StreamPrimitive};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305};
use secrecy::SecretString;
use zeroize::Zeroizing;

use super::envelope::{params_for_version, InnerHeader};
use super::kdf::{derive_key, KdfParams, KEY_LEN};
use super::{
    region_len, CHUNK_SIZE, INNER_HEADER_LEN, MIN_REGION_LEN, NONCE_PREFIX_LEN, SALT_LEN,
    SEALED_CHUNK_LEN,
};
use crate::error::{Result, ZolalError};
use crate::progress::Progress;

type Stream = StreamBE32<XChaCha20Poly1305>;
type NoncePrefix = aead_stream::Nonce<XChaCha20Poly1305, Stream>;

const SALT: usize = SALT_LEN as usize;
/// Salt plus nonce prefix: the only cleartext in a region.
const PREAMBLE_LEN: usize = (SALT_LEN + NONCE_PREFIX_LEN) as usize;
const SEALED: usize = SEALED_CHUNK_LEN as usize;
const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];

/// Encrypt `src` into `dst` as a complete region: salt, nonce prefix, then sealed chunks.
///
/// `src` must yield exactly `plaintext_len` bytes. The caller committed the region size to the
/// container from that number, so a source that turns out shorter or longer is an error rather
/// than a silently wrong file. Writes exactly [`region_len`]`(plaintext_len)` bytes and returns
/// that count.
///
/// Progress is reported as `(plaintext bytes sealed, plaintext_len)`.
pub fn seal_stream(
    src: &mut dyn Read,
    plaintext_len: u64,
    dst: &mut dyn Write,
    passphrase: &SecretString,
    params: KdfParams,
    progress: &dyn Progress,
) -> Result<u64> {
    let total = INNER_HEADER_LEN + plaintext_len;
    let chunks = total.div_ceil(CHUNK_SIZE as u64); // never 0: total >= INNER_HEADER_LEN
    let last = u32::try_from(chunks - 1).map_err(|_| {
        ZolalError::InvalidRequest("payload too large for one envelope (over 256 TiB)".into())
    })?;

    progress.update(0, plaintext_len);
    let preamble = random_preamble()?;
    let key = derive_key(passphrase, &preamble[..SALT], params)?;
    let stream = new_stream(&key, &preamble[SALT..])?;
    dst.write_all(&preamble)?;

    let mut buf = Zeroizing::new(Vec::with_capacity(SEALED));
    let mut done = 0u64;
    for index in 0..=last {
        if progress.is_cancelled() {
            return Err(ZolalError::Cancelled);
        }
        let start = u64::from(index) * CHUNK_SIZE as u64;
        let chunk_len = (total - start).min(CHUNK_SIZE as u64) as usize;

        buf.clear();
        if index == 0 {
            buf.extend_from_slice(&InnerHeader::CURRENT.to_bytes());
        }
        let filled = buf.len();
        buf.resize(chunk_len, 0);
        src.read_exact(&mut buf[filled..])?;
        done += (chunk_len - filled) as u64;

        stream
            .encrypt_in_place(index, index == last, &[], &mut *buf)
            .map_err(|_| internal("chunk encryption failed"))?;
        dst.write_all(&buf)?;
        progress.update(done, plaintext_len);
    }

    if src.read(&mut [0u8; 1])? != 0 {
        return Err(ZolalError::InvalidRequest(
            "payload grew while it was being hidden".into(),
        ));
    }
    Ok(region_len(plaintext_len))
}

/// Decrypt a whole region from `src` into `dst`, returning the payload length.
///
/// A thin wrapper over [`OpenReader`]; see it for how failures are classified. `dst` receives
/// authenticated chunks as they verify, so on `Err` the caller must discard what was written
/// (see the module docs). A truncated or reordered stream is always an error, never a partial
/// success. Writing out partial plaintext would hand an attacker a truncation oracle.
pub fn open_stream(
    src: &mut dyn Read,
    region_len: u64,
    dst: &mut dyn Write,
    passphrase: &SecretString,
    params: KdfParams,
    progress: &dyn Progress,
) -> Result<u64> {
    let mut reader = OpenReader::new(src, region_len, passphrase, params, progress)?;
    Ok(io::copy(&mut reader, dst)?)
}

/// Pull-based decryptor: a [`Read`] of the payload that authenticates each chunk before
/// releasing any of it.
///
/// Construction reads the preamble, derives the key and verifies chunk 0. That makes `new` the
/// passphrase check, which is what [`crate::reveal`] uses to decide between candidate regions:
///
/// - chunk 0 fails → [`ZolalError::WrongPassphrase`] (or, for a region too short to hold a
///   whole chunk 0, the honest "can't tell", which is also `WrongPassphrase`);
/// - chunk 0 passes but a later chunk fails, or the stream ends early →
///   [`ZolalError::DamagedPayload`], surfaced from `read` wrapped in an [`io::Error`].
///
/// End-of-stream (`Ok(0)`) is only returned after the final chunk has authenticated as final.
pub struct OpenReader<'p, R: Read> {
    src: R,
    stream: Stream,
    region_len: u64,
    /// Total chunk count, derived from `region_len`.
    chunks: u64,
    /// Sealed size of the last chunk (1 to `CHUNK_SIZE` bytes plus the tag).
    last_sealed_len: usize,
    /// Index of the next chunk to load.
    next: u64,
    /// The current chunk's plaintext, and how much of it has been handed out.
    plain: Zeroizing<Vec<u8>>,
    pos: usize,
    /// Region bytes consumed so far, for progress.
    consumed: u64,
    /// Set once a chunk fails, so a caller that ignores the error can't then read a clean EOF.
    poisoned: bool,
    header: InnerHeader,
    progress: &'p dyn Progress,
}

impl<'p, R: Read> OpenReader<'p, R> {
    /// Read the preamble, derive the key with `params`, and authenticate chunk 0.
    ///
    /// Progress is reported as `(region bytes consumed, region_len)`.
    pub fn new(
        mut src: R,
        region_len: u64,
        passphrase: &SecretString,
        params: KdfParams,
        progress: &'p dyn Progress,
    ) -> Result<Self> {
        if region_len < MIN_REGION_LEN {
            return Err(ZolalError::WrongPassphrase);
        }
        let ct_len = region_len - PREAMBLE_LEN as u64;
        let chunks = ct_len.div_ceil(SEALED_CHUNK_LEN);
        if chunks - 1 > u64::from(u32::MAX) {
            return Err(ZolalError::WrongPassphrase); // longer than any stream we can write
        }
        let last_sealed_len = (ct_len - (chunks - 1) * SEALED_CHUNK_LEN) as usize;

        let mut preamble = [0u8; PREAMBLE_LEN];
        src.read_exact(&mut preamble).map_err(eof_is_damage)?;
        if progress.is_cancelled() {
            return Err(ZolalError::Cancelled);
        }
        let key = derive_key(passphrase, &preamble[..SALT], params)?;
        let stream = new_stream(&key, &preamble[SALT..])?;

        let mut reader = Self {
            src,
            stream,
            region_len,
            chunks,
            last_sealed_len,
            next: 0,
            plain: Zeroizing::new(Vec::with_capacity(SEALED)),
            pos: 0,
            consumed: PREAMBLE_LEN as u64,
            poisoned: false,
            header: InnerHeader::CURRENT,
            progress,
        };
        reader.open_first_chunk()?;
        Ok(reader)
    }

    /// The authenticated inner header of this envelope.
    pub fn header(&self) -> InnerHeader {
        self.header
    }

    fn open_first_chunk(&mut self) -> Result<()> {
        let is_last = self.chunks == 1;
        let len = self.sealed_len(0);
        self.read_sealed(len)?;

        // Keep a copy only in the one ambiguous case: a full-size chunk 0 with nothing after it.
        // It is either a wrong key or a stream cut off right after its first chunk.
        let retry = (is_last && len == SEALED).then(|| self.plain.clone());
        if self
            .stream
            .decrypt_in_place(0, is_last, &[], &mut *self.plain)
            .is_err()
        {
            if let Some(mut copy) = retry {
                if self
                    .stream
                    .decrypt_in_place(0, false, &[], &mut *copy)
                    .is_ok()
                {
                    return Err(ZolalError::DamagedPayload);
                }
            }
            return Err(ZolalError::WrongPassphrase);
        }
        self.advance(len);

        // Authenticated from here on, so a bad header is a format problem, not tampering.
        let header = match self.plain.get(..INNER_HEADER_LEN as usize) {
            Some(&[version, suite]) => InnerHeader::from_bytes([version, suite]),
            _ => None,
        }
        .ok_or_else(|| ZolalError::CorruptBundle("unknown envelope cipher suite".into()))?;
        if params_for_version(header.version).is_none() {
            return Err(ZolalError::CorruptBundle(format!(
                "envelope version {} is newer than this build understands",
                header.version
            )));
        }
        self.header = header;
        self.pos = INNER_HEADER_LEN as usize;
        Ok(())
    }

    fn open_next_chunk(&mut self) -> Result<()> {
        if self.progress.is_cancelled() {
            return Err(ZolalError::Cancelled);
        }
        let index = self.next;
        let len = self.sealed_len(index);
        self.read_sealed(len)?;
        // `chunks - 1 <= u32::MAX` was checked in `new`.
        self.stream
            .decrypt_in_place(
                index as u32,
                index + 1 == self.chunks,
                &[],
                &mut *self.plain,
            )
            .map_err(|_| ZolalError::DamagedPayload)?;
        self.advance(len);
        Ok(())
    }

    fn sealed_len(&self, index: u64) -> usize {
        if index + 1 == self.chunks {
            self.last_sealed_len
        } else {
            SEALED
        }
    }

    fn read_sealed(&mut self, len: usize) -> Result<()> {
        self.pos = 0;
        self.plain.clear();
        self.plain.resize(len, 0);
        self.src.read_exact(&mut self.plain).map_err(eof_is_damage)
    }

    fn advance(&mut self, sealed_len: usize) {
        self.next += 1;
        self.consumed += sealed_len as u64;
        self.progress.update(self.consumed, self.region_len);
    }
}

impl<R: Read> Read for OpenReader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.poisoned {
            return Err(ZolalError::DamagedPayload.into_io());
        }
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos == self.plain.len() {
            if self.next == self.chunks {
                return Ok(0);
            }
            if let Err(e) = self.open_next_chunk() {
                self.poisoned = !matches!(e, ZolalError::Cancelled);
                self.plain.clear();
                self.pos = 0;
                return Err(e.into_io());
            }
        }
        let n = out.len().min(self.plain.len() - self.pos);
        out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Random salt and nonce prefix. The salt never starts with a JPEG SOI (see [`super`]).
fn random_preamble() -> Result<[u8; PREAMBLE_LEN]> {
    let mut preamble = [0u8; PREAMBLE_LEN];
    loop {
        getrandom::fill(&mut preamble).map_err(|e| {
            ZolalError::Io(io::Error::other(format!("OS random source failed: {e}")))
        })?;
        if preamble[..2] != JPEG_SOI {
            return Ok(preamble);
        }
    }
}

fn new_stream(key: &[u8; KEY_LEN], nonce_prefix: &[u8]) -> Result<Stream> {
    let aead = XChaCha20Poly1305::new_from_slice(key).map_err(|_| internal("bad key length"))?;
    let nonce = NoncePrefix::try_from(nonce_prefix).map_err(|_| internal("bad nonce length"))?;
    Ok(Stream::from_aead(aead, &nonce))
}

/// The region was shorter than its own length said, so the file changed under us.
fn eof_is_damage(e: io::Error) -> ZolalError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        ZolalError::DamagedPayload
    } else {
        e.into()
    }
}

fn internal(what: &str) -> ZolalError {
    ZolalError::Io(io::Error::other(format!("internal error: {what}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{AtomicProgress, NoProgress};

    const FAST: KdfParams = KdfParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    };
    const CHUNK: usize = CHUNK_SIZE;

    fn pass(s: &str) -> SecretString {
        SecretString::from(s)
    }

    /// Deterministic filler, so failures reproduce.
    fn data(len: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn seal(plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let n = seal_stream(
            &mut &plain[..],
            plain.len() as u64,
            &mut out,
            &pass("pw"),
            FAST,
            &NoProgress,
        )
        .unwrap();
        assert_eq!(n, out.len() as u64);
        out
    }

    fn open(region: &[u8], pw: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        open_stream(
            &mut &region[..],
            region.len() as u64,
            &mut out,
            &pass(pw),
            FAST,
            &NoProgress,
        )?;
        Ok(out)
    }

    /// Offset of sealed chunk `i` inside a region.
    fn chunk_at(i: usize) -> usize {
        PREAMBLE_LEN + i * SEALED
    }

    #[test]
    fn roundtrip_across_chunk_boundaries() {
        // The inner header takes 2 bytes of chunk 0, so boundaries sit at CHUNK - 2.
        for len in [
            0,
            1,
            CHUNK - 3,
            CHUNK - 2,
            CHUNK - 1,
            2 * CHUNK - 2,
            3 * CHUNK + 5,
        ] {
            let plain = data(len);
            let region = seal(&plain);
            assert_eq!(region.len() as u64, region_len(len as u64), "len {len}");
            assert_eq!(open(&region, "pw").unwrap(), plain, "len {len}");
        }
    }

    #[test]
    fn wrong_passphrase_is_reported_as_such() {
        let region = seal(&data(3 * CHUNK));
        assert!(matches!(
            open(&region, "nope"),
            Err(ZolalError::WrongPassphrase)
        ));
    }

    #[test]
    fn truncation_is_damage_not_success() {
        let plain = data(3 * CHUNK); // chunks: 0, 1, 2, 3 (3 holds the 2 header bytes' spill)
        let region = seal(&plain);
        let cuts = [
            region.len() - 1,  // one byte short
            chunk_at(3),       // exactly the last chunk removed
            chunk_at(2) + 100, // mid-chunk
            chunk_at(1),       // only chunk 0 left: the retry path
        ];
        for cut in cuts {
            let err = open(&region[..cut], "pw").unwrap_err();
            assert!(
                matches!(err, ZolalError::DamagedPayload),
                "cut {cut}: {err:?}"
            );
        }
    }

    #[test]
    fn truncation_inside_chunk_zero_cannot_be_told_from_a_wrong_key() {
        // Documented limitation: without a marker, a half chunk 0 is unverifiable.
        let region = seal(&data(3 * CHUNK));
        let err = open(&region[..chunk_at(0) + 1000], "pw").unwrap_err();
        assert!(matches!(err, ZolalError::WrongPassphrase));
    }

    #[test]
    fn reordering_and_tampering_fail() {
        let region = seal(&data(3 * CHUNK));
        let swap = |a: usize, b: usize| {
            let mut r = region.clone();
            let (ca, cb) = (chunk_at(a), chunk_at(b));
            let tmp = r[ca..ca + SEALED].to_vec();
            r.copy_within(cb..cb + SEALED, ca);
            r[cb..cb + SEALED].copy_from_slice(&tmp);
            r
        };
        assert!(matches!(
            open(&swap(1, 2), "pw"),
            Err(ZolalError::DamagedPayload)
        ));
        // Chunk 0 out of place looks exactly like a wrong key.
        assert!(matches!(
            open(&swap(0, 1), "pw"),
            Err(ZolalError::WrongPassphrase)
        ));

        let mut flipped = region.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(matches!(
            open(&flipped, "pw"),
            Err(ZolalError::DamagedPayload)
        ));

        let mut extended = region.clone();
        extended.push(0);
        assert!(matches!(
            open(&extended, "pw"),
            Err(ZolalError::DamagedPayload)
        ));
    }

    #[test]
    fn no_plaintext_released_from_a_failing_chunk() {
        let plain = data(3 * CHUNK);
        let mut region = seal(&plain);
        region[chunk_at(1) + 5] ^= 0x80;
        let mut out = Vec::new();
        let err = open_stream(
            &mut &region[..],
            region.len() as u64,
            &mut out,
            &pass("pw"),
            FAST,
            &NoProgress,
        )
        .unwrap_err();
        assert!(matches!(err, ZolalError::DamagedPayload));
        // Only chunk 0's verified bytes got through; nothing from the forged chunk.
        assert_eq!(out, plain[..CHUNK - 2]);
    }

    #[test]
    fn short_or_long_source_is_rejected() {
        let plain = data(1000);
        let mut out = Vec::new();
        let short = seal_stream(
            &mut &plain[..],
            2000,
            &mut out,
            &pass("pw"),
            FAST,
            &NoProgress,
        );
        assert!(
            matches!(short, Err(ZolalError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof)
        );
        let long = seal_stream(
            &mut &plain[..],
            10,
            &mut Vec::new(),
            &pass("pw"),
            FAST,
            &NoProgress,
        );
        assert!(matches!(long, Err(ZolalError::InvalidRequest(_))));
    }

    #[test]
    fn cancellation_stops_sealing() {
        let progress = AtomicProgress::new();
        progress.cancel();
        let plain = data(10);
        let err = seal_stream(
            &mut &plain[..],
            10,
            &mut Vec::new(),
            &pass("pw"),
            FAST,
            &progress,
        )
        .unwrap_err();
        assert!(matches!(err, ZolalError::Cancelled));
    }

    #[test]
    fn preamble_never_looks_like_a_jpeg() {
        for _ in 0..1000 {
            assert_ne!(random_preamble().unwrap()[..2], JPEG_SOI);
        }
    }
}
