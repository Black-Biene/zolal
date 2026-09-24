//! Payload bundling — several files in, one byte stream out.
//!
//! The bundle is written *inside* the encryption, so nothing here needs to resist tampering:
//! the AEAD tag has already verified the bytes before we parse them. That lets the format stay
//! simple. (Names are still sanitised: authenticated only means "made by someone who knew the
//! passphrase".)
//!
//! ```text
//! [ manifest_len : u32 LE ][ manifest (entries) ][ file_0 bytes ][ file_1 bytes ] ...
//! ```
//!
//! Files are stored back to back with no per-file framing — lengths come from the manifest,
//! which keeps writing single-pass and reading seekable.
//!
//! **No compression.** Payloads are overwhelmingly photos and videos, which are already
//! compressed; running deflate over them burns CPU and battery to gain nothing. If text-heavy
//! payloads ever matter, compress per-entry and flag it in [`Entry`], not globally.

pub mod bundle;
pub mod manifest;

pub use bundle::{plan, read_bundle, BundleReader, Extracted};
pub use manifest::{Entry, Manifest};
