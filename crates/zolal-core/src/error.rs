//! Error types.
//!
//! These variants are deliberately precise. The worst failure mode is silent loss: a carrier
//! sent the wrong way arrives looking perfect with the payload gone, so the integrator needs to
//! know whether a reveal failed because of a typo, a stripped carrier or truncation.
//!
//! ## Don't show the distinction to whoever is holding the file
//!
//! Precise errors are an oracle. [`ZolalError::WrongPassphrase`] means "something shaped like a
//! hidden region is here" and [`ZolalError::NoHiddenData`] means "nothing is" — and they take
//! very different times, because `NoHiddenData` can return before any key derivation. A UI that
//! surfaces either difference tells anyone watching whether a file holds anything, which is the
//! very thing hiding it was meant to prevent.
//!
//! The Zolal app therefore collapses every reveal failure into one outcome and holds it to a
//! minimum duration, so the fast failures look like the slow ones. Do the same unless the only
//! person who ever sees the result is the one who hid the file. [`crate::clean`] is the
//! exception: it only makes sense after a successful reveal, when there is nothing left to give
//! away.

use std::io;

use thiserror::Error;

use crate::carrier::CarrierFormat;

/// Convenient result alias.
pub type Result<T> = std::result::Result<T, ZolalError>;

/// Everything that can go wrong.
#[derive(Debug, Error)]
pub enum ZolalError {
    /// A candidate region exists but the passphrase did not authenticate it.
    ///
    /// Distinct from [`ZolalError::NoHiddenData`]: here we found something shaped right and the
    /// AEAD tag rejected the key, which usually means a typo.
    ///
    /// Being markerless has a cost here: a carrier with unrelated trailing bytes (some camera
    /// makers append their own metadata after the image) also yields this, because there is no
    /// way to tell someone else's bytes from ours under the wrong key.
    #[error("wrong passphrase")]
    WrongPassphrase,

    /// No authenticated payload was found.
    ///
    /// `carrier_looks_reencoded` is a **heuristic, not proof** — we cannot demonstrate that a
    /// file was re-encoded. A JPEG with no trailing bytes, no APP15 segments and stripped EXIF
    /// is the classic "a chat app processed this" signature. A tool whose user is known to be
    /// expecting hidden content can say so — *"If this was sent as a photo or video it was
    /// re-encoded and the hidden content was removed; ask for it as a file or in a .zip."* —
    /// but see the module docs before showing it to anyone else.
    #[error("no hidden data found (carrier looks re-encoded: {carrier_looks_reencoded})")]
    NoHiddenData {
        /// Whether the carrier shows signs of having been re-encoded in transit.
        carrier_looks_reencoded: bool,
    },

    /// The passphrase is right, but the hidden data is damaged or incomplete.
    ///
    /// The first chunk authenticated, so this is not a typo. A later chunk failed, or the
    /// stream ended before its final chunk. Truncation in transit is the usual cause. Nothing is
    /// written to the output directory when this happens.
    #[error("the hidden data is damaged or incomplete")]
    DamagedPayload,

    /// The file is not a JPEG, MP4 or PDF.
    ///
    /// HEIC and MOV should have been normalised before reaching this crate; if they arrive here
    /// it is a bug in the Swift layer, so name the detected type to make that obvious.
    #[error("unsupported carrier format: {detected}")]
    UnsupportedFormat {
        /// Human-readable detected type, e.g. `"HEIC"` or `"unknown"`.
        detected: String,
    },

    /// Hiding this much in this carrier would produce a file too big to still open as what it
    /// pretends to be. Nothing was written.
    ///
    /// Only JPEG has a limit today ([`crate::JPEG_MAX_OUTPUT`]): viewers load the whole photo
    /// into memory, so a huge one fails to open and gives the secret away. The UI should offer
    /// `suggestion` (a video) instead.
    #[error(
        "the result would be {output_size} bytes, over the {limit}-byte limit for this carrier; \
         use a {} carrier instead",
        .suggestion.name()
    )]
    PayloadTooLarge {
        /// What the output would have weighed.
        output_size: u64,
        /// The carrier format's limit ([`crate::size_limit`]).
        limit: u64,
        /// The carrier format to use instead.
        suggestion: CarrierFormat,
    },

    /// The carrier is too small to be a valid file of its claimed type.
    #[error("carrier file is too small or truncated")]
    CarrierTooSmall,

    /// A structurally invalid container (truncated box tree, broken xref, ...).
    #[error("malformed {format} container: {detail}")]
    MalformedContainer {
        /// Which container type failed to parse.
        format: &'static str,
        /// What specifically was wrong.
        detail: String,
    },

    /// The payload was authenticated but its bundle framing is invalid.
    ///
    /// Should be unreachable in practice: the AEAD tag verifies before we parse the bundle, so
    /// this implies a version mismatch or a bug rather than tampering.
    #[error("payload bundle is corrupt: {0}")]
    CorruptBundle(String),

    /// The request cannot be carried out as asked: no payload files, a technique that does not
    /// fit the carrier, a payload that is not a regular file, invalid KDF parameters.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The operation was cancelled via [`crate::progress::Progress`].
    #[error("operation cancelled")]
    Cancelled,

    /// Filesystem error.
    #[error("io error: {0}")]
    Io(io::Error),
}

impl ZolalError {
    /// Wrap this error in an [`io::Error`], for adapters that implement [`io::Read`] or
    /// [`io::Write`] and so can only fail with one. [`From<io::Error>`] unwraps it again, so the
    /// original variant survives the trip through `?`.
    pub(crate) fn into_io(self) -> io::Error {
        io::Error::other(self)
    }
}

impl From<io::Error> for ZolalError {
    fn from(err: io::Error) -> Self {
        // `WrongPassphrase`, `DamagedPayload` and `Cancelled` raised inside the decrypting reader
        // arrive here wrapped; flattening them to `Io` would break the UX contract above.
        if !err.get_ref().is_some_and(|inner| inner.is::<ZolalError>()) {
            return ZolalError::Io(err);
        }
        match err.into_inner().map(|inner| inner.downcast::<ZolalError>()) {
            Some(Ok(inner)) => *inner,
            Some(Err(other)) => ZolalError::Io(io::Error::other(other)),
            None => ZolalError::Io(io::Error::other("wrapped error vanished")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_errors_survive_io_round_trip() {
        let back = ZolalError::from(ZolalError::DamagedPayload.into_io());
        assert!(matches!(back, ZolalError::DamagedPayload));

        let plain = ZolalError::from(io::Error::new(io::ErrorKind::NotFound, "x"));
        assert!(matches!(plain, ZolalError::Io(e) if e.kind() == io::ErrorKind::NotFound));
    }
}
