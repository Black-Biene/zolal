//! Argon2id key derivation.
//!
//! ## Parameters are a real decision, not a default
//!
//! Desktop-grade memory-hard settings (64+ MiB) risk the OS killing the app on older iPhones,
//! which are simultaneously holding media buffers. The defaults below were measured on an
//! iPhone 13: about 65 ms per derivation, and a fixed ~50 MB memory spike that survived 1.7 GB
//! of other allocations without the app being killed. Older devices have not been measured.
//!
//! ## Where the parameters live: nowhere
//!
//! They cannot be stored inside the encrypted envelope, because they are needed to derive the
//! key that opens it. Storing them in the clear would fingerprint our format and hand an
//! attacker their cracking setup for free. So the **envelope version implies them**
//! ([`crate::crypto::envelope::params_for_version`]), and reveal simply tries each known set,
//! newest first. A retune therefore costs one extra derivation per older version on a *failed*
//! reveal, which is the price of carrying no marker.

use argon2::{Algorithm, Argon2, Params, Version};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use crate::error::{Result, ZolalError};

/// Memory cost in KiB. See the module docs for how it was measured.
pub const ARGON2_MEMORY_KIB: u32 = 48 * 1024;
/// Time cost (iterations).
pub const ARGON2_ITERATIONS: u32 = 3;
/// Parallelism. Kept at 1 — mobile cores are better spent on the streaming cipher.
pub const ARGON2_PARALLELISM: u32 = 1;
/// Derived key length (XChaCha20 key).
pub const KEY_LEN: usize = 32;

/// Argon2id cost parameters, versioned so old files keep opening after a retune.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory cost in KiB.
    pub memory_kib: u32,
    /// Iterations.
    pub iterations: u32,
    /// Lanes.
    pub parallelism: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            memory_kib: ARGON2_MEMORY_KIB,
            iterations: ARGON2_ITERATIONS,
            parallelism: ARGON2_PARALLELISM,
        }
    }
}

/// Derive the payload encryption key from a passphrase and salt.
///
/// Returns a [`Zeroizing`] key so it is wiped on drop. Argon2's own working memory is wiped too
/// (the crate's `zeroize` feature).
pub fn derive_key(
    passphrase: &SecretString,
    salt: &[u8],
    params: KdfParams,
) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let argon_params = Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        Some(KEY_LEN),
    )
    .map_err(|e| ZolalError::InvalidRequest(format!("invalid Argon2 parameters: {e}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);

    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(passphrase.expose_secret().as_bytes(), salt, key.as_mut())
        .map_err(|e| match e {
            argon2::Error::OutOfMemory => ZolalError::Io(std::io::ErrorKind::OutOfMemory.into()),
            other => ZolalError::InvalidRequest(format!("key derivation failed: {other}")),
        })?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: KdfParams = KdfParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    };

    #[test]
    fn deterministic_and_salt_sensitive() {
        let pass = SecretString::from("correct horse");
        let a = derive_key(&pass, &[1; 16], FAST).unwrap();
        let b = derive_key(&pass, &[1; 16], FAST).unwrap();
        let c = derive_key(&pass, &[2; 16], FAST).unwrap();
        assert_eq!(*a, *b);
        assert_ne!(*a, *c);
    }

    #[test]
    fn rejects_impossible_params() {
        let bad = KdfParams {
            memory_kib: 1,
            ..FAST
        };
        let err = derive_key(&SecretString::from("x"), &[0; 16], bad).unwrap_err();
        assert!(matches!(err, ZolalError::InvalidRequest(_)));
    }
}
