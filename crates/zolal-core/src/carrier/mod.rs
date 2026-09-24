//! Carrier formats — the one abstraction every format difference collapses into.
//!
//! Adding a format later means adding one file here and nothing elsewhere.

pub mod jpeg;
pub mod mp4;
pub mod pdf;

use std::io::{self, Read, Seek, Write};

use crate::error::{Result, ZolalError};

/// A source we can read and seek — carriers are always real files on disk.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// Container formats we accept as carriers.
///
/// HEIC and MOV are deliberately absent: they are normalised to JPEG and MP4 respectively
/// on the Swift side before this crate ever sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierFormat {
    /// JPEG image.
    Jpeg,
    /// MP4 / ISOBMFF video.
    Mp4,
    /// PDF document.
    Pdf,
}

impl CarrierFormat {
    /// Short display name.
    pub fn name(self) -> &'static str {
        match self {
            CarrierFormat::Jpeg => "JPEG",
            CarrierFormat::Mp4 => "MP4",
            CarrierFormat::Pdf => "PDF",
        }
    }
}

/// Which container trick to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Technique {
    /// Pick the default for the detected format.
    Auto,
    /// Append after the JPEG `FFD9` EOI marker. Simplest; default for JPEG.
    JpegTrailer,
    /// Store across `FFEF` APP15 segments, so it reads as metadata rather than junk at EOF.
    ///
    /// Survives naive trailer-stripping, at the cost of segmentation (see [`jpeg`]).
    JpegApp15,
    /// Append a top-level ISOBMFF `free` box. Default for MP4.
    Mp4FreeBox,
    /// Append an unreferenced stream object via a PDF incremental update. Default for PDF.
    PdfObject,
}

impl Technique {
    /// Resolve [`Technique::Auto`] to the default technique for `format`.
    pub fn resolve(self, format: CarrierFormat) -> Technique {
        match (self, format) {
            (Technique::Auto, CarrierFormat::Jpeg) => Technique::JpegTrailer,
            (Technique::Auto, CarrierFormat::Mp4) => Technique::Mp4FreeBox,
            (Technique::Auto, CarrierFormat::Pdf) => Technique::PdfObject,
            (explicit, _) => explicit,
        }
    }

    /// The format this technique applies to, or `None` for [`Technique::Auto`].
    pub fn format(self) -> Option<CarrierFormat> {
        match self {
            Technique::Auto => None,
            Technique::JpegTrailer | Technique::JpegApp15 => Some(CarrierFormat::Jpeg),
            Technique::Mp4FreeBox => Some(CarrierFormat::Mp4),
            Technique::PdfObject => Some(CarrierFormat::Pdf),
        }
    }
}

/// A byte range that could hold an envelope.
#[derive(Debug, Clone)]
pub struct Region {
    /// Byte offset from the start of the file (of the first fragment, when fragmented).
    pub offset: u64,
    /// Length of the region in bytes (summed over fragments).
    pub len: u64,
    /// Which technique produced regions of this shape.
    pub technique: Technique,
    /// True when the bytes are not contiguous on disk and must be gathered first.
    ///
    /// Only APP15 sets this today. Forgetting it is exactly the bug that produced two false
    /// "destroyed" results during early channel testing: payloads over 64 KiB span several
    /// segments, and reading them as one run corrupts the read.
    pub fragmented: bool,
}

/// Implemented once per container format.
pub trait Carrier {
    /// Cheap magic-byte test against the first few bytes of a file.
    fn probe(head: &[u8]) -> bool
    where
        Self: Sized;

    /// Byte ranges that could hold an envelope, in priority order.
    ///
    /// Returning regions is structural and passphrase-free, which is what keeps the format
    /// markerless: we enumerate *possible* hiding places and let the AEAD tag decide.
    fn candidate_regions(&self, src: &mut dyn ReadSeek) -> Result<Vec<Region>>;

    /// A reader over the bytes of `region`, gathering fragments if needed.
    ///
    /// Pull-based on purpose: the decryptor reads chunk 0 and stops there if the tag fails, so
    /// a wrong passphrase never streams the rest of a multi-gigabyte region.
    fn open_region<'a>(
        &self,
        src: &'a mut dyn ReadSeek,
        region: &Region,
    ) -> Result<Box<dyn Read + 'a>>;

    /// Stream `src` to `dst`, splicing in a region of exactly `region_len` bytes produced by
    /// `fill`, using `technique` (already resolved, and valid for this format).
    ///
    /// Implementations **must not** buffer the whole file. `region_len` is known in advance
    /// because ciphertext length is a deterministic function of plaintext length, which is what
    /// lets MP4 write its box size and PDF its `/Length` in a single pass. They must also check
    /// that `fill` wrote exactly `region_len` bytes (see [`CountingWriter`]).
    ///
    /// Returns the number of bytes written to `dst`.
    fn embed(
        &self,
        src: &mut dyn ReadSeek,
        dst: &mut dyn Write,
        technique: Technique,
        region_len: u64,
        fill: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
    ) -> Result<u64>;

    /// Exact size of what [`Carrier::embed`] would write for a region of `region_len` bytes.
    ///
    /// Walks the structure only, so it's cheap. Exact rather than "carrier size + region"
    /// because embedding drops earlier hidden content: re-hiding into a heavy stego file must
    /// not be refused for weight it's about to lose. [`crate::hide`] checks size limits
    /// against it and then checks `embed` wrote exactly that much.
    fn output_len(
        &self,
        src: &mut dyn ReadSeek,
        technique: Technique,
        region_len: u64,
    ) -> Result<u64>;

    /// Stream `src` to `dst` with any hidden region left out, so the result is an ordinary file
    /// of this format again.
    ///
    /// This is deliberately **not** `embed` with a zero-length region: that works by accident for
    /// JPEG but would leave an empty `free` box in an MP4 and a pointless appended update in a
    /// PDF — artefacts the original never had, and in the PDF case still a candidate region. Each
    /// format instead truncates to the boundary it already knows how to compute.
    ///
    /// Stripping a carrier that holds nothing is a no-op copy, not an error: the caller decides
    /// whether there was anything there, and saying so here would be an existence oracle.
    ///
    /// Returns the number of bytes written to `dst`.
    fn strip(&self, src: &mut dyn ReadSeek, dst: &mut dyn Write) -> Result<u64>;

    /// Heuristic for [`ZolalError::NoHiddenData`]: does this carrier look like something (a
    /// chat app, usually) re-encoded it? A hint for the UI, never a claim.
    fn looks_reencoded(&self, src: &mut dyn ReadSeek) -> Result<bool> {
        let _ = src;
        Ok(false)
    }
}

/// A [`Write`] that counts what passes through, so `embed` can prove the region came out at
/// exactly the length it committed to.
pub struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    count: u64,
}

impl<'a> CountingWriter<'a> {
    /// Wrap `inner`.
    pub fn new(inner: &'a mut dyn Write) -> Self {
        Self { inner, count: 0 }
    }

    /// Bytes written so far.
    pub fn count(&self) -> u64 {
        self.count
    }
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The error for a `fill` that produced a different length than it promised. Always a bug.
pub(crate) fn region_len_mismatch(expected: u64, got: u64) -> ZolalError {
    ZolalError::Io(io::Error::other(format!(
        "internal error: region should be {expected} bytes, {got} were written"
    )))
}
