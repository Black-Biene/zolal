//! Envelope framing — the 2-byte inner header that lives *inside* the encryption.
//!
//! Keeping version and cipher-suite identifiers encrypted is what preserves the markerless
//! property: any cleartext version byte would be a stable fingerprint identifying the file as
//! ours. One byte of foresight now avoids a format break later.

use crate::crypto::kdf::KdfParams;

/// Current envelope version.
pub const VERSION: u8 = 1;

/// Cipher suite identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Suite {
    /// XChaCha20-Poly1305 with STREAM/BE32 chunking, Argon2id KDF.
    XChaCha20Poly1305Argon2id = 1,
}

/// The inner header, encrypted as the first bytes of chunk 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InnerHeader {
    /// Envelope version.
    pub version: u8,
    /// Cipher suite in use.
    pub suite: Suite,
}

impl InnerHeader {
    /// The header every new envelope is written with.
    pub const CURRENT: Self = Self {
        version: VERSION,
        suite: Suite::XChaCha20Poly1305Argon2id,
    };

    /// Serialise to the 2 bytes that prefix the plaintext.
    pub fn to_bytes(self) -> [u8; 2] {
        [self.version, self.suite as u8]
    }

    /// Parse from the first 2 plaintext bytes.
    pub fn from_bytes(bytes: [u8; 2]) -> Option<Self> {
        let suite = match bytes[1] {
            1 => Suite::XChaCha20Poly1305Argon2id,
            _ => return None,
        };
        Some(Self {
            version: bytes[0],
            suite,
        })
    }
}

/// KDF parameters that a given envelope version implies.
///
/// Parameters are not stored per-file; the version selects them. When Argon2id is retuned
/// (see [`crate::crypto::kdf`]), bump [`VERSION`] and map the old value to the old costs here so
/// existing files keep opening.
pub fn params_for_version(version: u8) -> Option<KdfParams> {
    match version {
        1 => Some(KdfParams::default()),
        _ => None,
    }
}

/// Parameters new envelopes are sealed with.
pub fn current_params() -> KdfParams {
    // VERSION is always mapped; the fallback only exists so this can't panic.
    params_for_version(VERSION).unwrap_or_default()
}

/// Every parameter set reveal should try, newest first, without duplicates.
///
/// The version is encrypted, so reveal can't read it before deriving the key. It derives with
/// each candidate in turn and lets the first chunk's tag say which one was right.
pub fn kdf_candidates() -> Vec<KdfParams> {
    let mut out: Vec<KdfParams> = Vec::new();
    for version in (1..=VERSION).rev() {
        if let Some(params) = params_for_version(version) {
            if !out.contains(&params) {
                out.push(params);
            }
        }
    }
    out
}

/// Reserved for a future decoy slot.
///
/// A second independent envelope at a passphrase-derived offset would give plausible
/// deniability (reveal a harmless payload under a duress passphrase). Out of v1 scope, but the
/// layout already permits it — don't design it out.
pub mod decoy {}
