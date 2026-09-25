//! MP4 / ISOBMFF carrier — top-level `free` box.
//!
//! An ISOBMFF file is a flat sequence of boxes, each `size` (4 bytes, big-endian) + `type`
//! (4 bytes) then a body. `free` and `skip` are defined as ignorable padding, so appending one
//! after the existing boxes is valid and changes nothing a player depends on — no `moov`
//! rewrite, no `stco`/`co64` chunk-offset fixups, because nothing already in the file moves.
//!
//! That inertness is measurable: MP4s sent through Telegram the *normal* way came back
//! byte-identical, while MOVs were transcoded. We still never rely on that (the product rule is
//! "send as a file"), and MOV is normalised to MP4 on import rather than supported as a carrier.
//!
//! ## Reading without loading
//!
//! The box walk is hand-written and streaming, like the JPEG one. It reads only the 8- or
//! 16-byte header of each top-level box and seeks over the body, so a video carrying a 2 GB
//! hidden payload costs the same to inspect as the bare video. The runtime never depends on the
//! `mp4` crate (which parses from memory); that crate is a test-only dependency.
//!
//! ## Two box-size forms
//!
//! `size` is normally the whole box length in a 32-bit field. `size == 1` means the real length
//! is a 64-bit `largesize` right after the type (so the header is 16 bytes) — this is how boxes
//! above 4 GiB, and our own big payloads, are expressed. `size == 0` means "this box runs to
//! end of file"; some muxers write it for a not-yet-finalised `mdat`. We can *read* such a file,
//! but we can't safely append after an open-ended box (our bytes would be swallowed into it), so
//! [`Mp4Carrier::embed`] refuses it and asks for a re-muxed file.
//!
//! ## Hiding replaces what was there
//!
//! Like JPEG, a hide leaves exactly one region of ours. Any trailing run of `free`/`skip` boxes
//! at the end of the file is dropped before our box is appended, so re-hiding into an
//! already-used carrier replaces the payload instead of growing the file each time. Interior
//! padding boxes (used for alignment inside the file) are left untouched. On reveal every
//! `free`/`skip` body is a candidate, newest (last in the file) first, and the AEAD tag decides.

use std::io::{self, Read, SeekFrom, Write};

use super::{region_len_mismatch, Carrier, CountingWriter, ReadSeek, Region, Technique};
use crate::error::{Result, ZolalError};

/// Header size of a 32-bit box: `size` (4) + `type` (4).
pub const BOX_HEADER_LEN: u64 = 8;
/// Header size of a 64-bit box: `size`=1 (4) + `type` (4) + `largesize` (8).
pub const BOX_HEADER_LEN_64: u64 = 16;
/// Above this, the 32-bit `size` field can't express the box and we must use `largesize`.
pub const LARGE_BOX_THRESHOLD: u64 = u32::MAX as u64;

/// Box type we append and recognise.
const FREE: &[u8; 4] = b"free";
/// Also ignorable padding; some tools write this instead of `free`.
const SKIP: &[u8; 4] = b"skip";

/// HEIF-family major brands (HEIC stills and sequences). ISOBMFF too, but never a carrier:
/// the front end converts them to JPEG first.
pub const HEIF_BRANDS: &[&[u8; 4]] = &[
    b"heic", b"heix", b"hevc", b"heim", b"heis", b"mif1", b"msf1",
];
/// AVIF brands. Also ISOBMFF, also an image.
pub const AVIF_BRANDS: &[&[u8; 4]] = &[b"avif", b"avis"];
/// QuickTime's major brand. The front end relabels MOV as MP4 first.
pub const QUICKTIME_BRAND: &[u8; 4] = b"qt  ";

/// MP4 carrier implementation.
pub struct Mp4Carrier;

impl Carrier for Mp4Carrier {
    fn probe(head: &[u8]) -> bool {
        // ISOBMFF starts with a `ftyp` box; the size field precedes the type. HEIC, AVIF and
        // MOV share that shape, so the major brand has to rule them out, or a HEIC reaching us
        // would be reported as "MP4" instead of as the front-end bug it is.
        // `get` + `as_slice` avoids both a panic on short input and slice/array comparison
        // ambiguity.
        if head.get(4..8) != Some(b"ftyp".as_slice()) {
            return false;
        }
        match head.get(8..12) {
            Some(brand) => !HEIF_BRANDS
                .iter()
                .chain(AVIF_BRANDS)
                .chain([&QUICKTIME_BRAND])
                .any(|b| brand == b.as_slice()),
            None => false,
        }
    }

    fn candidate_regions(&self, src: &mut dyn ReadSeek) -> Result<Vec<Region>> {
        let layout = scan(src)?;
        // Newest first: our appended box is last in the file, so try it before any interior
        // padding a muxer left behind.
        Ok(layout
            .boxes
            .iter()
            .rev()
            .filter(|b| b.is_padding())
            .map(|b| Region {
                offset: b.offset + b.header_len,
                len: b.total - b.header_len,
                technique: Technique::Mp4FreeBox,
                fragmented: false,
            })
            .collect())
    }

    fn open_region<'a>(
        &self,
        src: &'a mut dyn ReadSeek,
        region: &Region,
    ) -> Result<Box<dyn Read + 'a>> {
        // A `free` box body is always one contiguous run — never fragmented like APP15.
        src.seek(SeekFrom::Start(region.offset))?;
        Ok(Box::new(src.take(region.len)))
    }

    fn strip(&self, src: &mut dyn ReadSeek, dst: &mut dyn Write) -> Result<u64> {
        // `kept_end` is already "where the real boxes stop": trailing free/skip padding, which is
        // where we hide, sits after it. No free header is written at all — an empty one would be
        // an artefact the original never had.
        let layout = scan(src)?;
        let kept_end = layout.kept_end()?;
        let mut out = CountingWriter::new(dst);
        copy_range(src, &mut out, 0, kept_end)?;
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
        if technique != Technique::Mp4FreeBox {
            return Err(ZolalError::InvalidRequest(format!(
                "{technique:?} does not apply to an MP4 carrier"
            )));
        }
        let layout = scan(src)?;
        let kept_end = layout.kept_end()?;

        let mut out = CountingWriter::new(dst);
        copy_range(src, &mut out, 0, kept_end)?;
        write_free_header(&mut out, region_len)?;
        let before = out.count();
        fill(&mut out)?;
        let got = out.count() - before;
        if got != region_len {
            return Err(region_len_mismatch(region_len, got));
        }
        Ok(out.count())
    }

    fn output_len(
        &self,
        src: &mut dyn ReadSeek,
        technique: Technique,
        region_len: u64,
    ) -> Result<u64> {
        if technique != Technique::Mp4FreeBox {
            return Err(ZolalError::InvalidRequest(format!(
                "{technique:?} does not apply to an MP4 carrier"
            )));
        }
        let kept_end = scan(src)?.kept_end()?;
        Ok(kept_end + free_box_overhead(region_len) + region_len)
    }
}

/// Header bytes a `free` box adds for a body of `n` bytes.
pub fn free_box_overhead(n: u64) -> u64 {
    if n + BOX_HEADER_LEN > LARGE_BOX_THRESHOLD {
        BOX_HEADER_LEN_64
    } else {
        BOX_HEADER_LEN
    }
}

/// Write a `free` box header for a body of `region_len` bytes, using `largesize` when the
/// 32-bit `size` field can't hold the total.
fn write_free_header(dst: &mut dyn Write, region_len: u64) -> io::Result<()> {
    let overhead = free_box_overhead(region_len);
    let total = overhead + region_len;
    if overhead == BOX_HEADER_LEN_64 {
        dst.write_all(&1u32.to_be_bytes())?;
        dst.write_all(FREE)?;
        dst.write_all(&total.to_be_bytes())?;
    } else {
        dst.write_all(&(total as u32).to_be_bytes())?;
        dst.write_all(FREE)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Structure

/// One top-level box header.
#[derive(Debug, Clone, Copy)]
struct BoxHeader {
    offset: u64,
    header_len: u64,
    /// Whole box length including the header.
    total: u64,
    kind: [u8; 4],
    /// True when the box declared `size == 0` (runs to EOF). Nothing can follow it.
    open_ended: bool,
}

impl BoxHeader {
    fn is_padding(&self) -> bool {
        &self.kind == FREE || &self.kind == SKIP
    }
}

/// What one streaming pass over the top-level boxes learns.
struct Layout {
    boxes: Vec<BoxHeader>,
    file_len: u64,
}

impl Layout {
    /// Byte offset our box is appended at: the end of the file, minus any trailing run of
    /// `free`/`skip` boxes (dropped so re-hiding replaces rather than accumulates).
    ///
    /// Errors if the file ends in an open-ended box, which we can't append after.
    fn kept_end(&self) -> Result<u64> {
        let mut kept_end = self.file_len;
        for b in self.boxes.iter().rev() {
            if b.is_padding() {
                kept_end = b.offset;
            } else {
                break;
            }
        }
        // The only place an open-ended box can sit is last. If it survived the strip above, our
        // appended bytes would be swallowed into it.
        if let Some(last) = self.boxes.iter().rev().find(|b| b.offset < kept_end) {
            if last.open_ended {
                return Err(ZolalError::MalformedContainer {
                    format: "MP4",
                    detail: "file ends in an open-ended box (size 0); re-mux it before hiding"
                        .into(),
                });
            }
        }
        Ok(kept_end)
    }
}

/// Walk the top-level boxes, reading only their headers.
fn scan(src: &mut dyn ReadSeek) -> Result<Layout> {
    let file_len = src.seek(SeekFrom::End(0))?;
    let mut boxes = Vec::new();
    let mut pos = 0;
    while pos < file_len {
        let b = parse_box(src, pos, file_len)?;
        pos = b.offset + b.total; // total >= header_len >= 8, so this always advances
        boxes.push(b);
    }
    if boxes.is_empty() {
        return Err(malformed("no boxes"));
    }
    Ok(Layout { boxes, file_len })
}

/// Parse one box header at `pos`.
fn parse_box(src: &mut dyn ReadSeek, pos: u64, file_len: u64) -> Result<BoxHeader> {
    let mut hdr = [0u8; 8];
    read_exact_at(src, pos, &mut hdr)?;
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&hdr[4..8]);

    let (total, header_len, open_ended) = match size32 {
        0 => (file_len - pos, BOX_HEADER_LEN, true),
        1 => {
            let mut big = [0u8; 8];
            read_exact_at(src, pos + 8, &mut big)?;
            (u64::from_be_bytes(big), BOX_HEADER_LEN_64, false)
        }
        n => (u64::from(n), BOX_HEADER_LEN, false),
    };

    if total < header_len {
        return Err(malformed("box smaller than its own header"));
    }
    if total > file_len - pos {
        return Err(malformed("box runs past the end of the file"));
    }
    Ok(BoxHeader {
        offset: pos,
        header_len,
        total,
        kind,
        open_ended,
    })
}

/// Read exactly `buf.len()` bytes starting at `offset`, mapping a short read to a malformed
/// error rather than a bare EOF.
fn read_exact_at(src: &mut dyn ReadSeek, offset: u64, buf: &mut [u8]) -> Result<()> {
    src.seek(SeekFrom::Start(offset))?;
    src.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            malformed("file ends inside a box header")
        } else {
            e.into()
        }
    })
}

/// Copy `[from, to)` of `src` to `dst`.
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

fn malformed(detail: impl Into<String>) -> ZolalError {
    ZolalError::MalformedContainer {
        format: "MP4",
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A 32-bit box: `size` + `type` + body.
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = (body.len() + 8) as u32;
        let mut out = total.to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// Minimal ISOBMFF shape for the walker: ftyp, a moov stand-in, mdat. Not playable — the
    /// integration tests use the `mp4` crate for a real one.
    fn synth_mp4() -> Vec<u8> {
        [
            boxed(b"ftyp", b"isom\0\0\x02\0isomiso2"),
            boxed(b"moov", &[0u8; 200]),
            boxed(b"mdat", &vec![0x11u8; 500]),
        ]
        .concat()
    }

    fn scan_bytes(bytes: &[u8]) -> Result<Layout> {
        scan(&mut Cursor::new(bytes))
    }

    fn embed(file: &[u8], region: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Mp4Carrier
            .embed(
                &mut Cursor::new(file),
                &mut out,
                Technique::Mp4FreeBox,
                region.len() as u64,
                &mut |w| Ok(w.write_all(region)?),
            )
            .unwrap();
        out
    }

    fn read_back(file: &[u8]) -> Vec<u8> {
        let regions = Mp4Carrier
            .candidate_regions(&mut Cursor::new(file))
            .unwrap();
        let region = regions.first().expect("a candidate region");
        let mut got = Vec::new();
        Mp4Carrier
            .open_region(&mut Cursor::new(file), region)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        got
    }

    #[test]
    fn walks_boxes_and_finds_no_region_in_a_clean_file() {
        let file = synth_mp4();
        let layout = scan_bytes(&file).unwrap();
        assert_eq!(layout.boxes.len(), 3);
        assert_eq!(&layout.boxes[0].kind, b"ftyp");
        assert_eq!(layout.file_len, file.len() as u64);
        assert!(Mp4Carrier
            .candidate_regions(&mut Cursor::new(&file))
            .unwrap()
            .is_empty());
        assert_eq!(layout.kept_end().unwrap(), file.len() as u64);
    }

    #[test]
    fn embed_appends_a_free_box_that_reads_back() {
        let file = synth_mp4();
        let region: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let out = embed(&file, &region);

        assert_eq!(&out[..file.len()], &file[..], "original bytes untouched");
        assert_eq!(
            out.len() as u64,
            file.len() as u64 + 8 + region.len() as u64
        );
        let layout = scan_bytes(&out).unwrap();
        assert_eq!(layout.boxes.len(), 4);
        assert!(layout.boxes[3].is_padding());
        assert_eq!(read_back(&out), region);
    }

    #[test]
    fn re_hiding_replaces_the_trailing_free_box() {
        let file = synth_mp4();
        let first = embed(&file, b"first payload, longer");
        let second = embed(&first, b"second");
        // Exactly one free box, holding the new payload — not two stacked.
        let padding = scan_bytes(&second)
            .unwrap()
            .boxes
            .iter()
            .filter(|b| b.is_padding())
            .count();
        assert_eq!(padding, 1);
        assert_eq!(read_back(&second), b"second");
        assert_eq!(second.len(), file.len() + 8 + "second".len());
    }

    #[test]
    fn interior_padding_is_preserved_and_still_a_candidate() {
        // A free box between moov and mdat (alignment padding) must survive a hide and stay
        // readable as a lower-priority candidate.
        let file = [
            boxed(b"ftyp", b"isom\0\0\x02\0isomiso2"),
            boxed(b"free", &[0xAA; 30]),
            boxed(b"mdat", &[0x11; 100]),
        ]
        .concat();
        let out = embed(&file, b"payload");

        let regions = Mp4Carrier
            .candidate_regions(&mut Cursor::new(&out))
            .unwrap();
        assert_eq!(regions.len(), 2, "our new box + the interior one");
        // Newest (ours, at EOF) is tried first.
        let mut ours = Vec::new();
        Mp4Carrier
            .open_region(&mut Cursor::new(&out), &regions[0])
            .unwrap()
            .read_to_end(&mut ours)
            .unwrap();
        assert_eq!(ours, b"payload");
    }

    #[test]
    fn output_len_matches_embed() {
        let file = synth_mp4();
        for len in [0u64, 1, 40_000] {
            let predicted = Mp4Carrier
                .output_len(&mut Cursor::new(&file), Technique::Mp4FreeBox, len)
                .unwrap();
            let region = vec![0u8; len as usize];
            assert_eq!(predicted, embed(&file, &region).len() as u64, "len {len}");
        }
    }

    #[test]
    fn free_header_uses_largesize_past_the_32_bit_limit() {
        let mut small = Vec::new();
        write_free_header(&mut small, 100).unwrap();
        assert_eq!(small, [0, 0, 0, 108, b'f', b'r', b'e', b'e']);

        // A body that pushes the 32-bit total over u32::MAX must switch to the 64-bit form,
        // without allocating the body.
        let region_len = u64::from(u32::MAX);
        let mut big = Vec::new();
        write_free_header(&mut big, region_len).unwrap();
        assert_eq!(big[..4], 1u32.to_be_bytes());
        assert_eq!(&big[4..8], FREE);
        assert_eq!(big[8..16], (BOX_HEADER_LEN_64 + region_len).to_be_bytes());
    }

    #[test]
    fn open_ended_last_box_is_refused_for_embedding() {
        // size == 0 mdat: readable, but we can't append after it.
        let mut file = boxed(b"ftyp", b"isom\0\0\x02\0isomiso2");
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(b"mdat");
        file.extend_from_slice(&[0x11; 100]);

        let layout = scan_bytes(&file).unwrap();
        assert!(layout.boxes.last().unwrap().open_ended);
        let err = Mp4Carrier
            .output_len(&mut Cursor::new(&file), Technique::Mp4FreeBox, 10)
            .unwrap_err();
        assert!(matches!(err, ZolalError::MalformedContainer { .. }));
    }

    #[test]
    fn largesize_box_is_parsed() {
        // A box using the 64-bit size form must be walked correctly.
        let mut file = boxed(b"ftyp", b"isom\0\0\x02\0isomiso2");
        let body = [0x22u8; 40];
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(b"mdat");
        file.extend_from_slice(&(16u64 + body.len() as u64).to_be_bytes());
        file.extend_from_slice(&body);

        let layout = scan_bytes(&file).unwrap();
        assert_eq!(layout.boxes.len(), 2);
        assert_eq!(layout.boxes[1].header_len, 16);
        assert_eq!(layout.boxes[1].total, 16 + body.len() as u64);
    }

    #[test]
    fn hostile_input_errors_and_never_panics() {
        let good = embed(&synth_mp4(), &[7u8; 50]);
        for cut in 0..good.len() {
            let _ = scan_bytes(&good[..cut]);
        }
        for i in 0..good.len() {
            for bit in [0x01, 0x80] {
                let mut bad = good.clone();
                bad[i] ^= bit;
                let _ = scan_bytes(&bad);
                let _ = Mp4Carrier.candidate_regions(&mut Cursor::new(&bad));
            }
        }
        // A box claiming more than the file holds.
        let mut overrun = boxed(b"ftyp", b"isom\0\0\x02\0isomiso2");
        overrun.extend_from_slice(&0xFFFF_FFFEu32.to_be_bytes());
        overrun.extend_from_slice(b"mdat");
        assert!(matches!(
            scan_bytes(&overrun),
            Err(ZolalError::MalformedContainer { .. })
        ));
    }
}
