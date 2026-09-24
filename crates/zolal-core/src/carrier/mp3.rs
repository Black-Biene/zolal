//! MP3 carrier — a `PRIV` frame in the leading ID3v2 tag.
//!
//! An MP3 file is a run of self-synchronising audio frames, usually preceded by an ID3v2 tag
//! (title, artist, cover art). Players read the tag's size from its 10-byte header and skip the
//! whole tag before looking for audio, so data inside the tag is never played.
//!
//! That is why we hide *in the tag* and not after the last audio frame. Encrypted bytes contain
//! the `FF Ex` frame-sync pattern every few hundred bytes; a decoder that resyncs through a
//! trailer would decode those as audio frames and play bursts of noise at the end of the song.
//!
//! ## What we write
//!
//! One ID3v2 frame of type `PRIV` ("private data", which every tag reader must ignore if it
//! doesn't understand the owner) with an **empty owner identifier**, then the envelope:
//!
//! ```text
//! "PRIV" | size | flags=0 | 0x00 (empty owner) | envelope...
//! ```
//!
//! An empty owner keeps the format markerless: a named owner would be a label saying what wrote
//! it. On reveal every `PRIV` frame with an empty owner is a candidate, newest first, and the AEAD
//! tag decides.
//!
//! ## Keeping the song's own tags
//!
//! The existing frames (title, artist, cover art) are copied byte for byte into the new tag, so
//! the file still looks the same in a music app. The tag's padding is dropped, and so is any
//! earlier `PRIV` frame of our shape, which makes re-hiding replace the payload instead of
//! growing the file. A file with no tag gets a new ID3v2.3 tag.
//!
//! Only ID3v2.3 and v2.4 tags without unsynchronisation, an extended header or a footer are
//! rewritten. Those three change how every frame is laid out; files using them are rare, and we
//! refuse rather than risk damaging the tag. The audio after the tag is never touched, and
//! moving it is safe: MP3 has no absolute offsets (a Xing/VBRI seek table is relative to the
//! first frame).
//!
//! ## Size
//!
//! ID3v2 stores the tag size in 28 bits, so the whole tag (the song's own frames plus the
//! payload) must stay under 256 MiB. Bigger payloads are refused with a suggestion to use a video.

use std::io::{self, Read, SeekFrom, Write};

use super::{
    region_len_mismatch, Carrier, CarrierFormat, CountingWriter, ReadSeek, Region, Technique,
};
use crate::error::{Result, ZolalError};

/// ID3v2 tag header: `"ID3"`, version (2), flags (1), synchsafe size (4).
const TAG_HEADER_LEN: u64 = 10;
/// ID3v2.3/2.4 frame header: id (4), size (4), flags (2).
const FRAME_HEADER_LEN: u64 = 10;
/// Largest value a 28-bit synchsafe size can hold.
pub const MAX_TAG_BODY: u64 = (1 << 28) - 1;
/// Frame type we write and recognise.
const PRIV: &[u8; 4] = b"PRIV";

/// Tag flag bits that change the frame layout: unsynchronisation, extended header, footer.
const UNSUPPORTED_FLAGS: u8 = 0x80 | 0x40 | 0x10;

/// MP3 carrier implementation.
pub struct Mp3Carrier;

impl Carrier for Mp3Carrier {
    fn probe(head: &[u8]) -> bool {
        match head {
            [b'I', b'D', b'3', major, ..] => (2..=4).contains(major),
            [0xFF, b1, b2, ..] => is_layer3_frame_header(*b1, *b2),
            _ => false,
        }
    }

    fn candidate_regions(&self, src: &mut dyn ReadSeek) -> Result<Vec<Region>> {
        let layout = scan(src)?;
        let Tag::Supported { frames, .. } = &layout.tag else {
            return Ok(Vec::new());
        };
        // Newest first: ours is written last in the tag.
        Ok(frames
            .iter()
            .rev()
            .filter(|f| f.ours)
            .map(|f| Region {
                offset: f.offset + FRAME_HEADER_LEN + 1,
                len: f.total - FRAME_HEADER_LEN - 1,
                technique: Technique::Mp3Id3,
                fragmented: false,
            })
            .collect())
    }

    fn open_region<'a>(
        &self,
        src: &'a mut dyn ReadSeek,
        region: &Region,
    ) -> Result<Box<dyn Read + 'a>> {
        src.seek(SeekFrom::Start(region.offset))?;
        Ok(Box::new(src.take(region.len)))
    }

    fn strip(&self, src: &mut dyn ReadSeek, dst: &mut dyn Write) -> Result<u64> {
        let layout = scan(src)?;
        let mut out = CountingWriter::new(dst);
        match &layout.tag {
            Tag::Supported { frames, .. } if frames.iter().any(|f| f.ours) => {
                let kept: Vec<_> = frames.iter().filter(|f| !f.ours).collect();
                // A tag left with nothing in it is dropped: it was most likely ours to begin with,
                // and an empty tag would be an artefact the original never had.
                if !kept.is_empty() {
                    let body: u64 = kept.iter().map(|f| f.total).sum();
                    write_tag_header(&mut out, layout.version(), body)?;
                    for f in kept {
                        copy_range(src, &mut out, f.offset, f.offset + f.total)?;
                    }
                }
                copy_range(src, &mut out, layout.audio_start, layout.file_len)?;
            }
            // Nothing of ours: an unchanged copy. Saying "nothing here" is the caller's call.
            _ => copy_range(src, &mut out, 0, layout.file_len)?,
        }
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
        check_technique(technique)?;
        let layout = scan(src)?;
        let plan = layout.plan(region_len)?;

        let mut out = CountingWriter::new(dst);
        write_tag_header(&mut out, layout.version(), plan.tag_body)?;
        for f in &plan.kept {
            copy_range(src, &mut out, f.offset, f.offset + f.total)?;
        }
        write_priv_header(&mut out, layout.version(), 1 + region_len)?;
        out.write_all(&[0])?; // empty owner identifier
        let before = out.count();
        fill(&mut out)?;
        let got = out.count() - before;
        if got != region_len {
            return Err(region_len_mismatch(region_len, got));
        }
        copy_range(src, &mut out, layout.audio_start, layout.file_len)?;
        Ok(out.count())
    }

    fn output_len(
        &self,
        src: &mut dyn ReadSeek,
        technique: Technique,
        region_len: u64,
    ) -> Result<u64> {
        check_technique(technique)?;
        let layout = scan(src)?;
        let plan = layout.plan(region_len)?;
        Ok(TAG_HEADER_LEN + plan.tag_body + (layout.file_len - layout.audio_start))
    }
}

/// True for the second and third bytes of an MPEG audio Layer III frame header (after `FF`).
///
/// Checks the sync bits, a non-reserved MPEG version, Layer III, and a bitrate and sample rate
/// that aren't the reserved values, so random data and other `FF`-prefixed formats (JPEG is
/// `FF D8`, ADTS AAC has layer bits `00`) don't pass.
fn is_layer3_frame_header(b1: u8, b2: u8) -> bool {
    let sync = b1 & 0xE0 == 0xE0;
    let version = (b1 >> 3) & 0b11; // 01 is reserved
    let layer = (b1 >> 1) & 0b11; // 01 is Layer III
    let bitrate = b2 >> 4; // 1111 is invalid
    let sample_rate = (b2 >> 2) & 0b11; // 11 is reserved
    sync && version != 0b01 && layer == 0b01 && bitrate != 0b1111 && sample_rate != 0b11
}

fn check_technique(technique: Technique) -> Result<()> {
    if technique == Technique::Mp3Id3 {
        Ok(())
    } else {
        Err(ZolalError::InvalidRequest(format!(
            "{technique:?} does not apply to an MP3 carrier"
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// Structure

/// One frame inside the tag.
#[derive(Debug, Clone, Copy)]
struct Frame {
    offset: u64,
    /// Whole frame length including its header.
    total: u64,
    /// A `PRIV` frame with an empty owner and flags 0: the shape we write.
    ours: bool,
}

#[derive(Debug)]
enum Tag {
    /// No leading ID3v2 tag.
    Absent,
    /// A v2.3/v2.4 tag we can rewrite.
    Supported { major: u8, frames: Vec<Frame> },
    /// A tag we won't rewrite (v2.2, or a layout flag we don't handle), and why.
    Unsupported(&'static str),
}

struct Layout {
    tag: Tag,
    /// Where the audio (everything after the leading tag) starts.
    audio_start: u64,
    file_len: u64,
}

/// What an embed will write, worked out before writing anything.
struct Plan {
    kept: Vec<Frame>,
    /// Tag body length: the kept frames plus our frame.
    tag_body: u64,
}

impl Layout {
    /// ID3v2 major version to write: the existing tag's, or 3 for a new tag.
    fn version(&self) -> u8 {
        match self.tag {
            Tag::Supported { major, .. } => major,
            _ => 3,
        }
    }

    fn plan(&self, region_len: u64) -> Result<Plan> {
        let kept: Vec<Frame> = match &self.tag {
            Tag::Absent => Vec::new(),
            Tag::Supported { frames, .. } => frames.iter().filter(|f| !f.ours).copied().collect(),
            Tag::Unsupported(why) => {
                return Err(ZolalError::MalformedContainer {
                    format: "MP3",
                    detail: format!(
                        "its ID3 tag {why}, which Zolal doesn't rewrite; re-save the song's tags \
                         with a tag editor (as ID3v2.3) and try again"
                    ),
                })
            }
        };
        let ours = FRAME_HEADER_LEN + 1 + region_len;
        let tag_body = kept.iter().map(|f| f.total).sum::<u64>() + ours;
        if tag_body > MAX_TAG_BODY {
            return Err(ZolalError::PayloadTooLarge {
                output_size: TAG_HEADER_LEN + tag_body + (self.file_len - self.audio_start),
                limit: MAX_TAG_BODY,
                suggestion: CarrierFormat::Mp4,
            });
        }
        Ok(Plan { kept, tag_body })
    }
}

/// Read the leading tag's header and frame headers. Never reads frame bodies or audio, except
/// the first byte of each `PRIV` frame (its owner).
fn scan(src: &mut dyn ReadSeek) -> Result<Layout> {
    let file_len = src.seek(SeekFrom::End(0))?;
    let mut hdr = [0u8; TAG_HEADER_LEN as usize];
    if file_len >= TAG_HEADER_LEN {
        read_exact_at(src, 0, &mut hdr)?;
    }
    if &hdr[..3] != b"ID3" {
        return Ok(Layout {
            tag: Tag::Absent,
            audio_start: 0,
            file_len,
        });
    }

    let (major, flags) = (hdr[3], hdr[5]);
    let size = synchsafe(&hdr[6..10]).ok_or_else(|| malformed("ID3 tag size is not synchsafe"))?;
    let footer = if major == 4 && flags & 0x10 != 0 {
        10
    } else {
        0
    };
    let tag_end = TAG_HEADER_LEN + size + footer;
    if tag_end > file_len {
        return Err(malformed("ID3 tag runs past the end of the file"));
    }

    let tag = if major != 3 && major != 4 {
        Tag::Unsupported("is ID3v2.2 or older")
    } else if flags & UNSUPPORTED_FLAGS != 0 {
        Tag::Unsupported("uses unsynchronisation, an extended header or a footer")
    } else {
        Tag::Supported {
            major,
            frames: scan_frames(src, major, TAG_HEADER_LEN + size)?,
        }
    };
    Ok(Layout {
        tag,
        audio_start: tag_end,
        file_len,
    })
}

/// Walk the frames in `[10, body_end)`, stopping at padding.
fn scan_frames(src: &mut dyn ReadSeek, major: u8, body_end: u64) -> Result<Vec<Frame>> {
    let mut frames = Vec::new();
    let mut pos = TAG_HEADER_LEN;
    while pos + FRAME_HEADER_LEN <= body_end {
        let mut fh = [0u8; FRAME_HEADER_LEN as usize];
        read_exact_at(src, pos, &mut fh)?;
        // Padding (zeros) ends the frames; so does anything that isn't a frame id.
        if !fh[..4]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            break;
        }
        let size = if major == 4 {
            synchsafe(&fh[4..8]).ok_or_else(|| malformed("ID3 frame size is not synchsafe"))?
        } else {
            u64::from(u32::from_be_bytes([fh[4], fh[5], fh[6], fh[7]]))
        };
        let total = FRAME_HEADER_LEN + size;
        if pos + total > body_end {
            return Err(malformed("ID3 frame runs past the end of the tag"));
        }
        let ours = &fh[..4] == PRIV && fh[8..10] == [0, 0] && size > 1 && {
            let mut owner = [0u8; 1];
            read_exact_at(src, pos + FRAME_HEADER_LEN, &mut owner)?;
            owner[0] == 0
        };
        frames.push(Frame {
            offset: pos,
            total,
            ours,
        });
        pos += total;
    }
    Ok(frames)
}

/// Decode a 4-byte synchsafe integer (7 bits per byte), or `None` if a high bit is set.
fn synchsafe(b: &[u8]) -> Option<u64> {
    b.iter().try_fold(0u64, |acc, &x| {
        (x < 0x80).then_some((acc << 7) | u64::from(x))
    })
}

fn to_synchsafe(n: u64) -> [u8; 4] {
    debug_assert!(n <= MAX_TAG_BODY);
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

fn write_tag_header(dst: &mut dyn Write, major: u8, body: u64) -> io::Result<()> {
    dst.write_all(&[b'I', b'D', b'3', major, 0, 0])?;
    dst.write_all(&to_synchsafe(body))
}

fn write_priv_header(dst: &mut dyn Write, major: u8, body: u64) -> io::Result<()> {
    dst.write_all(PRIV)?;
    if major == 4 {
        dst.write_all(&to_synchsafe(body))?;
    } else {
        // `plan` keeps the whole tag under 2^28, so this always fits.
        dst.write_all(&(body as u32).to_be_bytes())?;
    }
    dst.write_all(&[0, 0])
}

fn read_exact_at(src: &mut dyn ReadSeek, offset: u64, buf: &mut [u8]) -> Result<()> {
    src.seek(SeekFrom::Start(offset))?;
    src.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            malformed("file ends inside the ID3 tag")
        } else {
            e.into()
        }
    })
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

fn malformed(detail: &str) -> ZolalError {
    ZolalError::MalformedContainer {
        format: "MP3",
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_accepts_mp3_and_rejects_lookalikes() {
        assert!(Mp3Carrier::probe(b"ID3\x03\x00\x00\x00\x00\x00\x00"));
        assert!(Mp3Carrier::probe(&[0xFF, 0xFB, 0x90, 0x00])); // MPEG-1 L3 128k 44.1k
        assert!(Mp3Carrier::probe(&[0xFF, 0xF3, 0x64, 0x00])); // MPEG-2 L3
        assert!(!Mp3Carrier::probe(&[0xFF, 0xD8, 0xFF, 0xE0])); // JPEG
        assert!(!Mp3Carrier::probe(&[0xFF, 0xF1, 0x50, 0x80])); // ADTS AAC
        assert!(!Mp3Carrier::probe(&[0xFF, 0xFB, 0xF0, 0x00])); // bad bitrate
        assert!(!Mp3Carrier::probe(b"ID3\x09"));
        assert!(!Mp3Carrier::probe(b"%PDF-1.7"));
    }

    #[test]
    fn synchsafe_roundtrips() {
        for n in [0, 1, 127, 128, 0x0123_4567, MAX_TAG_BODY] {
            assert_eq!(synchsafe(&to_synchsafe(n)), Some(n));
        }
        assert_eq!(synchsafe(&[0x80, 0, 0, 0]), None);
    }
}
