//! JPEG carrier — trailer (default) and APP15 segments (alternative).
//!
//! ## Reading without loading
//!
//! The marker walk is hand-written and streaming; JPEG crates parse from memory. Segment
//! bodies are skipped by seeking, and only the image's own entropy-coded scan data is read
//! through, which for a photo is a few MB. **The hidden region is never read during a walk**,
//! so a JPEG carrying a 2 GB trailer costs the same to inspect as the bare photo.
//!
//! ## Trailer
//!
//! A JPEG ends at the `FFD9` EOI marker; every decoder stops there. Anything appended is
//! ignored, so the region is simply "end of image" to EOF, and its length comes from the file
//! size, with no length field to store.
//!
//! "End of image" means the end of the *last chained* image. MPF files (HDR gain maps and depth
//! maps from recent iPhones and Androids) store extra images as complete JPEGs right after the
//! primary's EOI, so the walker follows `SOI … EOI` images back to back. Our envelope's salt
//! never begins `FF D8` (see [`crate::crypto`]), so the region can't be mistaken for one more
//! image.
//!
//! ## APP15
//!
//! `FFEF` application segments read as metadata rather than junk at EOF, and survive naive
//! trailer-stripping. The catch: **a segment's payload is capped at 65 533 bytes**, so anything
//! larger is split across several segments, each with its own 4-byte header.
//!
//! Extraction therefore has to *gather* those payloads before decrypting. Reading the region as
//! one contiguous run silently works below 64 KiB and breaks above it — the exact bug that
//! produced two false "destroyed" results in early channel testing.
//! [`Region::fragmented`] exists to stop that recurring.
//!
//! Our segments go straight after the leading APP0/APP1 run (JFIF, EXIF, XMP), ahead of
//! everything else, and in particular ahead of an MPF `APP2`. MPF locates its extra images by
//! offsets relative to its own header, so inserting *before* that header moves the header and
//! the images together and keeps every offset valid. Inserting after it would break the gain
//! map.
//!
//! ## Hiding replaces what was there
//!
//! After a hide, the carrier holds exactly one region. Both techniques drop the header's APP15
//! segments and any non-image bytes after the last image before writing ours. That keeps
//! reveal unambiguous (hiding into an already-used photo replaces its payload). The cost is
//! that vendor trailers are dropped too, such as Samsung's metadata or the video of a Google
//! Motion Photo. The picture itself is never touched.

use std::io::{self, Read, SeekFrom, Write};

use super::{region_len_mismatch, Carrier, CountingWriter, ReadSeek, Region, Technique};
use crate::error::{Result, ZolalError};

/// Largest payload a single APP15 segment can hold: 65 535 minus the 2-byte length field.
pub const APP15_MAX_PAYLOAD: usize = 65_533;

/// JPEG start-of-image marker.
const SOI: [u8; 2] = [0xFF, 0xD8];

const M_SOI: u8 = 0xD8;
const M_EOI: u8 = 0xD9;
const M_SOS: u8 = 0xDA;
const M_TEM: u8 = 0x01;
const M_APP0: u8 = 0xE0;
const M_APP1: u8 = 0xE1;
const M_APP15: u8 = 0xEF;
const EXIF_ID: &[u8; 6] = b"Exif\0\0";

/// Walker buffer. Scan data is read through in blocks this size.
const WALK_BUF: usize = 64 * 1024;
/// Read size after a seek, when what follows is a segment header: 4 bytes are needed, so
/// reading a whole block would drag in the segment body we're about to skip.
const HEADER_READ: usize = 1024;

/// JPEG carrier implementation.
pub struct JpegCarrier;

impl Carrier for JpegCarrier {
    fn probe(head: &[u8]) -> bool {
        head.starts_with(&SOI)
    }

    fn candidate_regions(&self, src: &mut dyn ReadSeek) -> Result<Vec<Region>> {
        let layout = scan(src)?;
        let mut regions = Vec::new();
        let trailer = layout.file_len - layout.image_end;
        if trailer > 0 {
            regions.push(Region {
                offset: layout.image_end,
                len: trailer,
                technique: Technique::JpegTrailer,
                fragmented: false,
            });
        }
        if let Some(first) = layout.app15.first() {
            regions.push(Region {
                offset: first.offset + 4,
                len: layout.app15.iter().map(|s| s.payload_len()).sum(),
                technique: Technique::JpegApp15,
                fragmented: true,
            });
        }
        Ok(regions)
    }

    fn open_region<'a>(
        &self,
        src: &'a mut dyn ReadSeek,
        region: &Region,
    ) -> Result<Box<dyn Read + 'a>> {
        if !region.fragmented {
            src.seek(SeekFrom::Start(region.offset))?;
            return Ok(Box::new(src.take(region.len)));
        }
        let parts = scan_header(src)?
            .iter()
            .map(|s| (s.offset + 4, s.payload_len()))
            .collect();
        Ok(Box::new(Gather {
            src,
            parts,
            next: 0,
            remaining: 0,
        }))
    }

    fn strip(&self, src: &mut dyn ReadSeek, dst: &mut dyn Write) -> Result<u64> {
        // Both places a JPEG can hold a region go at once: the trailer past EOI, and any APP15
        // segments in the header. What is left is the image exactly as a camera would write it.
        let layout = scan(src)?;
        let mut out = CountingWriter::new(dst);
        copy_without(src, &mut out, 0, layout.image_end, &layout.app15)?;
        Ok(out.count())
    }

    fn embed(
        &self,
        src: &mut dyn ReadSeek,
        dst: &mut dyn Write,
        technique: Technique,
        region_len: u64,
        fill: &mut dyn FnMut(&mut dyn Write) -> Result<()>,
    ) -> Result<u64> {
        let layout = scan(src)?;
        let mut out = CountingWriter::new(dst);
        match technique {
            Technique::JpegTrailer => {
                copy_without(src, &mut out, 0, layout.image_end, &layout.app15)?;
                let before = out.count();
                fill(&mut out)?;
                let got = out.count() - before;
                if got != region_len {
                    return Err(region_len_mismatch(region_len, got));
                }
            }
            Technique::JpegApp15 => {
                copy_without(src, &mut out, 0, layout.insert_at, &layout.app15)?;
                let mut segments = App15Writer::new(&mut out);
                fill(&mut segments)?;
                let got = segments.finish()?;
                if got != region_len {
                    return Err(region_len_mismatch(region_len, got));
                }
                copy_without(
                    src,
                    &mut out,
                    layout.insert_at,
                    layout.image_end,
                    &layout.app15,
                )?;
            }
            other => {
                return Err(ZolalError::InvalidRequest(format!(
                    "{other:?} does not apply to a JPEG carrier"
                )))
            }
        }
        Ok(out.count())
    }

    fn output_len(
        &self,
        src: &mut dyn ReadSeek,
        technique: Technique,
        region_len: u64,
    ) -> Result<u64> {
        let layout = scan(src)?;
        // Everything up to the end of the last image, minus the APP15 segments embed drops.
        let kept = layout.image_end - layout.app15.iter().map(|s| s.len).sum::<u64>();
        match technique {
            Technique::JpegTrailer => Ok(kept + region_len),
            Technique::JpegApp15 => Ok(kept + region_len + app15_overhead(region_len)),
            other => Err(ZolalError::InvalidRequest(format!(
                "{other:?} does not apply to a JPEG carrier"
            ))),
        }
    }

    fn looks_reencoded(&self, src: &mut dyn ReadSeek) -> Result<bool> {
        looks_reencoded(src)
    }
}

/// Heuristic: does this JPEG look like a chat app re-encoded it?
///
/// Feeds [`crate::error::ZolalError::NoHiddenData`] so "nothing found" can explain itself.
/// Deliberately a *hint*, never a claim — re-encoding cannot be proven, and a plain camera JPEG
/// can legitimately match. Signature: no trailing bytes after the last image, no APP15
/// segments, and EXIF stripped.
pub fn looks_reencoded(src: &mut dyn ReadSeek) -> Result<bool> {
    let layout = scan(src)?;
    Ok(layout.file_len == layout.image_end && layout.app15.is_empty() && !layout.has_exif)
}

/// Bytes of overhead APP15 framing adds for a payload of `n` bytes.
///
/// Needed up-front so the caller can precompute the exact output length.
pub fn app15_overhead(n: u64) -> u64 {
    let segments = n.div_ceil(APP15_MAX_PAYLOAD as u64);
    segments * 4 // marker (2) + length field (2) per segment
}

// ---------------------------------------------------------------------------------------------
// Structure

/// A marker segment with a length field: `FF xx`, a 2-byte length, then the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    /// Offset of the segment's `FF`.
    offset: u64,
    /// Whole segment, marker included.
    len: u64,
}

impl Segment {
    fn end(&self) -> u64 {
        self.offset + self.len
    }

    fn payload_len(&self) -> u64 {
        self.len - 4
    }
}

/// What one streaming pass learns about a JPEG.
#[derive(Debug)]
struct Layout {
    /// APP15 segments before the first SOS, in file order.
    app15: Vec<Segment>,
    /// Where our APP15 segments go: the end of the leading APP0/APP1/APP15 run.
    insert_at: u64,
    /// Whether the header carries an EXIF APP1.
    has_exif: bool,
    /// Just past the EOI of the last chained image. Everything after it is the trailer.
    image_end: u64,
    file_len: u64,
}

/// Header facts, collected while walking the primary image up to its first scan.
struct HeaderFacts {
    app15: Vec<Segment>,
    insert_at: u64,
    has_exif: bool,
}

/// Walk the whole file: the primary image, then any images chained after it.
fn scan(src: &mut dyn ReadSeek) -> Result<Layout> {
    let mut w = Walker::new(src)?;
    let mut facts = HeaderFacts {
        app15: Vec::new(),
        insert_at: SOI.len() as u64,
        has_exif: false,
    };
    let mut image_end = walk_image(&mut w, Some(&mut facts), false)?;

    // MPF and friends: complete JPEGs back to back after the primary.
    while w.len - image_end >= 2 {
        let mut head = [0u8; 2];
        w.exact(&mut head)?;
        w.seek_to(image_end)?;
        if head != SOI {
            break;
        }
        match walk_image(&mut w, None, false) {
            Ok(end) => image_end = end,
            // Starts like an image but isn't one: treat it as trailing data.
            Err(ZolalError::MalformedContainer { .. }) => break,
            Err(e) => return Err(e),
        }
    }

    Ok(Layout {
        app15: facts.app15,
        insert_at: facts.insert_at,
        has_exif: facts.has_exif,
        image_end,
        file_len: w.len,
    })
}

/// Walk only as far as the first scan. Enough to gather APP15 segments, and cheap.
fn scan_header(src: &mut dyn ReadSeek) -> Result<Vec<Segment>> {
    let mut w = Walker::new(src)?;
    let mut facts = HeaderFacts {
        app15: Vec::new(),
        insert_at: SOI.len() as u64,
        has_exif: false,
    };
    walk_image(&mut w, Some(&mut facts), true)?;
    Ok(facts.app15)
}

/// Walk one image from its SOI. Returns the offset just past its EOI (or, with `header_only`,
/// just past the first SOS header). `facts`, when given, collects header facts up to the first
/// scan.
fn walk_image(
    w: &mut Walker,
    mut facts: Option<&mut HeaderFacts>,
    header_only: bool,
) -> Result<u64> {
    let mut soi = [0u8; 2];
    w.exact(&mut soi)?;
    if soi != SOI {
        return Err(malformed("missing start-of-image"));
    }

    let mut leading_run = true;
    let mut marker = w.marker()?;
    loop {
        match marker {
            M_EOI => return Ok(w.pos),
            M_SOI => return Err(malformed("start-of-image inside an image")),
            M_TEM | 0xD0..=0xD7 => marker = w.marker()?, // standalone, no length
            _ => {
                let offset = w.pos - 2;
                let body = u64::from(w.u16()?)
                    .checked_sub(2)
                    .ok_or_else(|| malformed("segment length below 2"))?;

                let mut consumed = 0;
                if let Some(facts) = facts.as_deref_mut() {
                    leading_run &= matches!(marker, M_APP0 | M_APP1 | M_APP15);
                    if leading_run {
                        facts.insert_at = offset + 4 + body;
                    }
                    if marker == M_APP15 {
                        facts.app15.push(Segment {
                            offset,
                            len: body + 4,
                        });
                    }
                    if marker == M_APP1 && body >= EXIF_ID.len() as u64 {
                        let mut id = [0u8; 6];
                        w.exact(&mut id)?;
                        consumed = id.len() as u64;
                        facts.has_exif |= &id == EXIF_ID;
                    }
                }
                w.skip(body - consumed)?;

                if marker == M_SOS {
                    if header_only {
                        return Ok(w.pos);
                    }
                    facts = None; // header facts end at the first scan
                    marker = w.skip_scan()?;
                } else {
                    marker = w.marker()?;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming reader

/// A forward reader with its own buffer and cheap seeks.
///
/// `BufReader` would do, except that after skipping a segment it refills a whole block. For an
/// APP15 carrier that means reading most of every 64 KiB segment just to find the next 4-byte
/// header. Here a refill after a seek is small, and only scan data is read in big blocks.
struct Walker<'a> {
    src: &'a mut dyn ReadSeek,
    buf: Box<[u8]>,
    /// Unread window: `buf[start..end]`, beginning at file offset `pos`.
    start: usize,
    end: usize,
    pos: u64,
    len: u64,
}

impl<'a> Walker<'a> {
    fn new(src: &'a mut dyn ReadSeek) -> Result<Self> {
        let len = src.seek(SeekFrom::End(0))?;
        src.seek(SeekFrom::Start(0))?;
        Ok(Self {
            src,
            buf: vec![0u8; WALK_BUF].into_boxed_slice(),
            start: 0,
            end: 0,
            pos: 0,
            len,
        })
    }

    /// The unread window, refilled with up to `want` bytes if it's empty. Empty only at EOF.
    fn fill(&mut self, want: usize) -> Result<&[u8]> {
        if self.start == self.end {
            let cap = want.clamp(1, self.buf.len());
            let n = loop {
                match self.src.read(&mut self.buf[..cap]) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            };
            self.start = 0;
            self.end = n;
        }
        Ok(&self.buf[self.start..self.end])
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        self.pos += n as u64;
    }

    fn exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < out.len() {
            let avail = self.fill(HEADER_READ)?;
            if avail.is_empty() {
                return Err(malformed("file ends inside a segment"));
            }
            let n = avail.len().min(out.len() - done);
            out[done..done + n].copy_from_slice(&avail[..n]);
            self.consume(n);
            done += n;
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.exact(&mut b)?;
        Ok(b[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let mut b = [0u8; 2];
        self.exact(&mut b)?;
        Ok(u16::from_be_bytes(b))
    }

    fn skip(&mut self, n: u64) -> Result<()> {
        if n > self.len - self.pos {
            return Err(malformed("segment runs past the end of the file"));
        }
        let buffered = (self.end - self.start) as u64;
        if n <= buffered {
            self.consume(n as usize);
            return Ok(());
        }
        self.seek_to(self.pos + n)
    }

    fn seek_to(&mut self, pos: u64) -> Result<()> {
        self.src.seek(SeekFrom::Start(pos))?;
        self.start = 0;
        self.end = 0;
        self.pos = pos;
        Ok(())
    }

    /// The next marker code. Expects to be on its `FF`; skips fill bytes.
    fn marker(&mut self) -> Result<u8> {
        if self.u8()? != 0xFF {
            return Err(malformed(format!(
                "expected a marker at offset {}",
                self.pos - 1
            )));
        }
        let mut code = self.u8()?;
        while code == 0xFF {
            code = self.u8()?;
        }
        if code == 0x00 {
            return Err(malformed("stuffed byte outside scan data"));
        }
        Ok(code)
    }

    /// Skip entropy-coded scan data, returning the marker that ends it. Inside scan data `FF`
    /// is always followed by `00` (a stuffed byte) or `D0`–`D7` (a restart marker); anything
    /// else is a real marker.
    fn skip_scan(&mut self) -> Result<u8> {
        loop {
            let window = self.fill(WALK_BUF)?;
            if window.is_empty() {
                return Err(malformed("scan data runs to the end of the file"));
            }
            match window.iter().position(|&b| b == 0xFF) {
                None => {
                    let n = window.len();
                    self.consume(n);
                }
                Some(i) => {
                    self.consume(i + 1);
                    let mut code = self.u8()?;
                    while code == 0xFF {
                        code = self.u8()?;
                    }
                    if code != 0x00 && !(0xD0..=0xD7).contains(&code) {
                        return Ok(code);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Writing

/// Copy `[from, to)` of `src` to `dst`, leaving out the `skip` segments that fall inside it.
fn copy_without(
    src: &mut dyn ReadSeek,
    dst: &mut dyn Write,
    from: u64,
    to: u64,
    skip: &[Segment],
) -> Result<()> {
    let mut pos = from;
    for seg in skip.iter().filter(|s| s.offset >= from && s.end() <= to) {
        copy_range(src, dst, pos, seg.offset)?;
        pos = seg.end();
    }
    copy_range(src, dst, pos, to)
}

fn copy_range(src: &mut dyn ReadSeek, dst: &mut dyn Write, from: u64, to: u64) -> Result<()> {
    if to <= from {
        return Ok(());
    }
    src.seek(SeekFrom::Start(from))?;
    let copied = io::copy(&mut src.take(to - from), dst)?;
    if copied != to - from {
        return Err(malformed("carrier changed while it was being read"));
    }
    Ok(())
}

/// Frames everything written to it as consecutive full APP15 segments.
///
/// [`Write::flush`] deliberately does **not** emit a short segment: every segment but the last
/// must be full, or the output stops matching [`app15_overhead`].
struct App15Writer<'a> {
    out: &'a mut dyn Write,
    buf: Vec<u8>,
    written: u64,
}

impl<'a> App15Writer<'a> {
    fn new(out: &'a mut dyn Write) -> Self {
        Self {
            out,
            buf: Vec::with_capacity(APP15_MAX_PAYLOAD),
            written: 0,
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let len = (self.buf.len() + 2) as u16; // <= 65 535 by construction
        self.out.write_all(&[0xFF, M_APP15])?;
        self.out.write_all(&len.to_be_bytes())?;
        self.out.write_all(&self.buf)?;
        self.written += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Emit the final (possibly short) segment; returns the payload bytes framed.
    fn finish(mut self) -> io::Result<u64> {
        self.emit()?;
        Ok(self.written)
    }
}

impl Write for App15Writer<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = data.len().min(APP15_MAX_PAYLOAD - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == APP15_MAX_PAYLOAD {
            self.emit()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Reads APP15 payloads back to back, skipping each segment's header.
struct Gather<'a> {
    src: &'a mut dyn ReadSeek,
    /// `(offset, len)` of each segment payload, in file order.
    parts: Vec<(u64, u64)>,
    next: usize,
    remaining: u64,
}

impl Read for Gather<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.remaining == 0 {
            let Some(&(offset, len)) = self.parts.get(self.next) else {
                return Ok(0);
            };
            self.src.seek(SeekFrom::Start(offset))?;
            self.remaining = len;
            self.next += 1;
        }
        let want = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.src.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn malformed(detail: impl Into<String>) -> ZolalError {
    ZolalError::MalformedContainer {
        format: "JPEG",
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Seek};

    fn seg(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, marker];
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    /// Structurally valid (not decodable) JPEG. Scan data exercises stuffed bytes, a restart
    /// marker and fill bytes before EOI.
    fn jpeg(header: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SOI.to_vec();
        out.extend(seg(M_APP0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
        for s in header {
            out.extend_from_slice(s);
        }
        out.extend(seg(0xDB, &[0u8; 65])); // DQT
        out.extend(seg(0xC0, &[8, 0, 8, 0, 8, 1, 1, 0x11, 0])); // SOF0
        out.extend(seg(M_SOS, &[1, 1, 0, 0, 63, 0]));
        out.extend_from_slice(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xFF]);
        out.extend_from_slice(&[0xFF, M_EOI]);
        out
    }

    fn layout(bytes: &[u8]) -> Result<Layout> {
        scan(&mut Cursor::new(bytes))
    }

    #[test]
    fn finds_end_of_image_trailer_and_exif() {
        let plain = jpeg(&[]);
        let mut with = jpeg(&[seg(M_APP1, b"Exif\0\0MM\0*")]);
        let image_len = with.len() as u64;
        with.extend_from_slice(b"trailing");

        let l = layout(&with).unwrap();
        assert_eq!(l.image_end, image_len);
        assert_eq!(l.file_len - l.image_end, 8);
        assert!(l.has_exif);
        assert!(!layout(&plain).unwrap().has_exif);
        assert!(looks_reencoded(&mut Cursor::new(&plain)).unwrap());
        assert!(!looks_reencoded(&mut Cursor::new(&with)).unwrap());
    }

    #[test]
    fn follows_chained_images_but_not_lookalikes() {
        let primary = jpeg(&[]);
        let secondary = jpeg(&[]);
        let mut file = [primary.clone(), secondary.clone()].concat();
        let both = file.len() as u64;
        file.extend_from_slice(b"payload");
        assert_eq!(layout(&file).unwrap().image_end, both);

        // FF D8 followed by junk is trailing data, not an image.
        let fake = [primary.clone(), vec![0xFF, 0xD8, 0x00, 0x11, 0x22]].concat();
        assert_eq!(layout(&fake).unwrap().image_end, primary.len() as u64);
    }

    #[test]
    fn gathers_app15_and_places_insertion_after_leading_run() {
        let a = seg(M_APP15, &[1; 10]);
        let b = seg(M_APP15, &[2; 20]);
        let icc = seg(0xE2, b"ICC_PROFILE\0");
        let file = jpeg(&[seg(M_APP1, b"Exif\0\0"), a.clone(), icc, b.clone()]);
        let l = layout(&file).unwrap();

        assert_eq!(l.app15.len(), 2);
        assert_eq!(l.app15[0].payload_len(), 10);
        assert_eq!(l.app15[1].payload_len(), 20);
        // After APP0, APP1 and the first APP15; before the ICC APP2.
        assert_eq!(l.insert_at, l.app15[0].end());

        let regions = JpegCarrier
            .candidate_regions(&mut Cursor::new(&file))
            .unwrap();
        assert_eq!(regions.len(), 1);
        assert!(regions[0].fragmented);
        let mut src = Cursor::new(&file);
        let mut got = Vec::new();
        JpegCarrier
            .open_region(&mut src, &regions[0])
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, [vec![1; 10], vec![2; 20]].concat());
    }

    fn embed(file: &[u8], technique: Technique, region: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        JpegCarrier
            .embed(
                &mut Cursor::new(file),
                &mut out,
                technique,
                region.len() as u64,
                &mut |w| Ok(w.write_all(region)?),
            )
            .unwrap();
        out
    }

    fn read_back(file: &[u8], technique: Technique) -> Vec<u8> {
        let regions = JpegCarrier
            .candidate_regions(&mut Cursor::new(file))
            .unwrap();
        let region = regions.iter().find(|r| r.technique == technique).unwrap();
        let mut src = Cursor::new(file);
        let mut got = Vec::new();
        JpegCarrier
            .open_region(&mut src, region)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        got
    }

    #[test]
    fn app15_embed_splits_and_reassembles() {
        let region: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let carrier = jpeg(&[]);
        let out = embed(&carrier, Technique::JpegApp15, &region);

        let l = layout(&out).unwrap();
        assert_eq!(l.app15.len(), 4); // 3 full segments + 1 short
        assert_eq!(
            out.len() as u64,
            carrier.len() as u64 + region.len() as u64 + app15_overhead(region.len() as u64)
        );
        assert_eq!(read_back(&out, Technique::JpegApp15), region);
    }

    #[test]
    fn embedding_replaces_previous_regions() {
        let carrier = [jpeg(&[seg(M_APP15, b"old app15")]), b"old trailer".to_vec()].concat();
        let clean_len = jpeg(&[]).len();

        let trailer = embed(&carrier, Technique::JpegTrailer, b"new");
        assert_eq!(trailer.len(), clean_len + 3);
        assert_eq!(read_back(&trailer, Technique::JpegTrailer), b"new");
        assert!(layout(&trailer).unwrap().app15.is_empty());

        let app15 = embed(&carrier, Technique::JpegApp15, b"new");
        for (technique, out) in [
            (Technique::JpegTrailer, &trailer),
            (Technique::JpegApp15, &app15),
        ] {
            let predicted = JpegCarrier
                .output_len(&mut Cursor::new(&carrier), technique, 3)
                .unwrap();
            assert_eq!(predicted, out.len() as u64, "{technique:?}");
        }
        let l = layout(&app15).unwrap();
        assert_eq!(l.app15.len(), 1);
        assert_eq!(l.file_len, l.image_end, "old trailer dropped");
        assert_eq!(read_back(&app15, Technique::JpegApp15), b"new");
    }

    #[test]
    fn fill_that_lies_about_its_length_is_caught() {
        let err = JpegCarrier
            .embed(
                &mut Cursor::new(jpeg(&[])),
                &mut Vec::new(),
                Technique::JpegTrailer,
                10,
                &mut |w| Ok(w.write_all(b"short")?),
            )
            .unwrap_err();
        assert!(matches!(err, ZolalError::Io(_)));
    }

    #[test]
    fn hostile_input_errors_and_never_panics() {
        let good = [jpeg(&[seg(M_APP15, &[7; 40])]), b"tail".to_vec()].concat();
        for cut in 0..good.len() {
            let _ = layout(&good[..cut]);
        }
        for i in 0..good.len() {
            for bit in [0x01, 0x80] {
                let mut bad = good.clone();
                bad[i] ^= bit;
                let _ = layout(&bad);
                let _ = JpegCarrier.candidate_regions(&mut Cursor::new(&bad));
            }
        }
        let no_eoi = &jpeg(&[])[..jpeg(&[]).len() - 2];
        assert!(matches!(
            layout(no_eoi),
            Err(ZolalError::MalformedContainer { .. })
        ));
        let bad_len = [SOI.to_vec(), vec![0xFF, 0xE0, 0x00, 0x01]].concat();
        assert!(matches!(
            layout(&bad_len),
            Err(ZolalError::MalformedContainer { .. })
        ));
    }

    /// Counts the bytes actually read from the underlying source.
    struct Counting<R> {
        inner: R,
        read: u64,
    }
    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }
    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn walking_never_reads_the_hidden_region() {
        // A 1 GiB trailer, sparse on disk: if the walk read it, this test would crawl.
        let mut file = tempfile::tempfile().unwrap();
        let image = jpeg(&[]);
        file.write_all(&image).unwrap();
        file.set_len(image.len() as u64 + (1 << 30)).unwrap();
        let mut src = Counting {
            inner: file,
            read: 0,
        };
        let regions = JpegCarrier.candidate_regions(&mut src).unwrap();
        assert_eq!(regions[0].len, 1 << 30);
        assert!(src.read < 64 * 1024, "read {} bytes", src.read);

        // APP15: skipping 150 full segments must not read their payloads.
        let region = vec![0xA5u8; 150 * APP15_MAX_PAYLOAD];
        let stego = embed(&image, Technique::JpegApp15, &region);
        let mut src = Counting {
            inner: Cursor::new(&stego),
            read: 0,
        };
        JpegCarrier.candidate_regions(&mut src).unwrap();
        assert!(
            src.read < stego.len() as u64 / 20,
            "read {} of {} bytes",
            src.read,
            stego.len()
        );
    }
}
