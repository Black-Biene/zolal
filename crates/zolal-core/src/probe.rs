//! Format sniffing by magic bytes.
//!
//! Extensions are not trusted — users rename files, and chat apps rewrite names freely
//! (Telegram renamed every photo to `photo_<date>.jpg` during testing).

use crate::carrier::mp4::{Mp4Carrier, AVIF_BRANDS, HEIF_BRANDS, QUICKTIME_BRAND};
use crate::carrier::{jpeg::JpegCarrier, pdf::PdfCarrier, Carrier, CarrierFormat};

/// Bytes needed to identify any format we handle.
pub const PROBE_LEN: usize = 16;

/// Identify a carrier format from the first bytes of a file.
///
/// Returns `None` for anything unsupported, including HEIC and MOV — those are normalised on
/// the Swift side, so seeing one here means the Swift layer has a bug. [`describe`] names them
/// explicitly so that error is obvious rather than a generic "unsupported".
pub fn detect(head: &[u8]) -> Option<CarrierFormat> {
    if JpegCarrier::probe(head) {
        Some(CarrierFormat::Jpeg)
    } else if Mp4Carrier::probe(head) {
        Some(CarrierFormat::Mp4)
    } else if PdfCarrier::probe(head) {
        Some(CarrierFormat::Pdf)
    } else {
        None
    }
}

/// Human-readable name for an unsupported file, for error messages.
pub fn describe(head: &[u8]) -> String {
    if head.get(4..8) == Some(b"ftyp".as_slice()) {
        // An ISOBMFF brand we didn't accept — most likely HEIC or QuickTime.
        let brand = head.get(8..12).unwrap_or_default();
        let is = |brands: &[&[u8; 4]]| brands.iter().any(|b| brand == b.as_slice());
        return if is(HEIF_BRANDS) {
            "HEIC (should be converted to JPEG first)"
        } else if is(AVIF_BRANDS) {
            "AVIF (should be converted to JPEG first)"
        } else if is(&[QUICKTIME_BRAND]) {
            "MOV/QuickTime (should be remuxed to MP4 first)"
        } else {
            "unrecognised ISOBMFF brand"
        }
        .to_string();
    }
    if head.starts_with(&[0x89, b'P', b'N', b'G']) {
        return "PNG (not a supported carrier)".to_string();
    }
    "unknown".to_string()
}
