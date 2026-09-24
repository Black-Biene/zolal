//! PDF carrier — an unreferenced stream object added by an incremental update.
//!
//! A PDF is read back to front: `startxref` near the end points at a cross-reference section,
//! which locates every object. An *incremental update* appends new objects plus a small
//! cross-reference section chained to the old one via `/Prev`, leaving every existing byte
//! untouched. Our object is referenced from nothing, so no viewer ever renders or even reads it.
//!
//! ## Why not just append, the way JPEG does
//!
//! Trailing junk after `%%EOF` is ignored by most readers, but only because they find
//! `startxref` by scanning *backwards* over the last kilobyte or so. Append 200 MB and that
//! keyword is suddenly 200 MB from the end: strict readers fail to find it and fall back to
//! repair-by-rescanning, or give up. Writing a real update keeps a valid `startxref` and `%%EOF`
//! at the very end of the file, whatever the payload weighs.
//!
//! ## Two cross-reference forms, both handled
//!
//! PDF 1.5 replaced the classic `xref` table with a *cross-reference stream* (an object holding
//! the same table in binary). We append **the form the file already uses**, because a reader
//! following `/Prev` from one form into the other is where implementations differ. Our own
//! cross-reference stream is written **uncompressed**, which is legal and means reveal never
//! needs an inflate dependency.
//!
//! ## Reading without loading
//!
//! Like the other carriers, nothing here parses the document. We read the last kilobyte to find
//! `startxref`, parse one cross-reference section, and read a few object headers — a bounded
//! number of small reads, whatever the file weighs. That is why `lopdf` is not a dependency of
//! this crate: it loads the whole document, which for a carrier holding a gigabyte is exactly
//! what the design forbids. (It survives as a dev-dependency, to re-parse our output in tests.)
//!
//! ## What is refused
//!
//! - **Encrypted PDFs.** Streams in them are expected to be encrypted with the document key, and
//!   a new trailer would have to carry `/Encrypt` and a matching `/ID`. Refused rather than
//!   guessed at.
//! - **Files whose final section we cannot parse** (for instance a compressed cross-reference
//!   stream written by another tool): reveal simply reports nothing hidden, which is true.
//!
//! ## Hiding replaces what was there
//!
//! If the file's last section is one **we** wrote — a single added stream object, laid out
//! exactly as this module lays one out — it is truncated away before the new one is
//! appended, so re-hiding replaces the payload instead of stacking another copy of the secret in
//! the file. The test is deliberately strict; anything that does not match exactly is left alone
//! and we simply append, because destroying somebody's annotations would be far worse than
//! leaving a stale region behind.
//!
//! ## Measured: re-saving the document destroys the payload
//!
//! If anything rewrites the file, the payload is gone. Tested with PDFKit — the framework behind
//! Preview's Save — on real documents: they reopen perfectly and the hidden object is silently
//! dropped, because a rewriter rebuilds the object graph and garbage-collects anything that
//! nothing references.
//!
//! This is **not** specific to PDF. An ImageIO re-save strips a JPEG's trailer, and even a
//! *lossless* AVFoundation passthrough remux drops an MP4's `free` box. The rule for all three
//! carriers is the same: the file has to be passed on byte for byte.
//!
//! It could be made to survive, by attaching the stream somewhere a rewriter deliberately keeps —
//! an embedded-file attachment, say. That was rejected: the secret would then be listed in the
//! viewer's own attachments panel, which defeats the entire point for the casual snooper this is
//! built for. So it is handled in the app's wording, not here.

use std::io::{self, Read, SeekFrom, Write};

use super::{region_len_mismatch, Carrier, CountingWriter, ReadSeek, Region, Technique};
use crate::error::{Result, ZolalError};

/// PDF files begin with this.
const MAGIC: &[u8] = b"%PDF-";

/// How far back from the end we look for the final `startxref`. The spec puts `%%EOF` within the
/// last 1024 bytes; we allow some slack for files with junk after it.
const TAIL_WINDOW: u64 = 4096;
/// Largest single read this module will make. Dictionaries and object headers are far smaller.
const MAX_READ: u64 = 1 << 20;
/// Entries we are willing to collect from one cross-reference section.
///
/// Our own update holds one or two. A section with more is somebody else's, and walking it would
/// only produce candidate regions that cost a key derivation each to reject.
const MAX_SECTION_ENTRIES: u64 = 64;
/// Most candidate regions we hand to reveal, each costing one key derivation.
const MAX_CANDIDATES: usize = 4;
/// A classic cross-reference entry is exactly 20 bytes.
const TABLE_ENTRY: u64 = 20;
/// Classic tables store offsets as 10 digits, so they cannot address beyond this.
const MAX_TABLE_OFFSET: u64 = 9_999_999_999;
/// Bytes we write between the payload and the cross-reference section.
const MID: &[u8] = b"\nendstream\nendobj\n";

/// PDF carrier implementation.
pub struct PdfCarrier;

impl Carrier for PdfCarrier {
    fn probe(head: &[u8]) -> bool {
        head.starts_with(MAGIC)
    }

    fn candidate_regions(&self, src: &mut dyn ReadSeek) -> Result<Vec<Region>> {
        let file_len = src.seek(SeekFrom::End(0))?;
        let xref_at = last_startxref(src, file_len)?;
        let last = section_at(src, xref_at, file_len, true)?;

        // Only an appended update can hold our object. A file that was never updated is a
        // clean PDF, and saying so beats making reveal try every content stream in it and then
        // report a wrong passphrase.
        let Some(prev) = appended_update_prev(&last) else {
            return Ok(Vec::new());
        };

        let mut entries = last.entries;
        // Newest (highest offset) first: ours is the last object in the file.
        entries.sort_by_key(|&(_, offset)| std::cmp::Reverse(offset));
        let mut regions = Vec::new();
        for (_, offset) in entries {
            // Objects this revision actually added sit after the section it chains back to.
            if offset <= prev {
                continue;
            }
            if let Some((start, len)) = stream_body(src, offset, file_len)? {
                regions.push(Region {
                    offset: start,
                    len,
                    technique: Technique::PdfObject,
                    fragmented: false,
                });
                if regions.len() == MAX_CANDIDATES {
                    break;
                }
            }
        }
        Ok(regions)
    }

    fn open_region<'a>(
        &self,
        src: &'a mut dyn ReadSeek,
        region: &Region,
    ) -> Result<Box<dyn Read + 'a>> {
        // A stream body is one contiguous run — never fragmented like APP15.
        src.seek(SeekFrom::Start(region.offset))?;
        Ok(Box::new(src.take(region.len)))
    }

    fn strip(&self, src: &mut dyn ReadSeek, dst: &mut dyn Write) -> Result<u64> {
        // `plan_update` already finds where an appended update of ours begins, and returns the
        // file length when there is none — so this is a plain copy for a clean PDF and a
        // truncation back to the original document for a loaded one. Writing no update at all is
        // the point: appending an empty one would grow the file and still leave a candidate.
        let base = plan_update(src, 0)?.base;
        let mut out = CountingWriter::new(dst);
        copy_range(src, &mut out, 0, base)?;
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
        let update = plan_update(src, region_len)?;

        let mut out = CountingWriter::new(dst);
        copy_range(src, &mut out, 0, update.base)?;
        out.write_all(&update.prefix)?;
        let before = out.count();
        fill(&mut out)?;
        let got = out.count() - before;
        if got != region_len {
            return Err(region_len_mismatch(region_len, got));
        }
        out.write_all(&update.suffix)?;
        Ok(out.count())
    }

    fn output_len(
        &self,
        src: &mut dyn ReadSeek,
        technique: Technique,
        region_len: u64,
    ) -> Result<u64> {
        check_technique(technique)?;
        Ok(plan_update(src, region_len)?.total(region_len))
    }
}

/// The offset this section chains back to, if it is an **appended** update: one whose `/Prev`
/// points *backwards*, to something earlier in the file.
///
/// Linearized ("fast web view") files, which Illustrator and many web tools produce, put a
/// cross-reference section at the *front* whose `/Prev` points *forward* to the main one at the
/// end. That is the original document, not an update. Without this test a clean linearized PDF
/// would offer every first-page content stream as a candidate and answer "wrong passphrase"
/// instead of "nothing hidden" — and, worse, could look like one of our own updates to the
/// replace-on-re-hide check below.
fn appended_update_prev(section: &Section) -> Option<u64> {
    let prev = dict_uint(&section.dict, b"Prev")?;
    (prev < section.offset).then_some(prev)
}

fn check_technique(technique: Technique) -> Result<()> {
    if technique == Technique::PdfObject {
        Ok(())
    } else {
        Err(ZolalError::InvalidRequest(format!(
            "{technique:?} does not apply to a PDF carrier"
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// Writing

/// Everything the update is made of, with the payload body left as a hole in the middle.
#[derive(Debug)]
struct Update {
    /// Where the original file is kept up to: its length, or the start of a previous update of
    /// ours that is being replaced.
    base: u64,
    /// The object header, written before the payload.
    prefix: Vec<u8>,
    /// `endstream`/`endobj`, the cross-reference section, trailer, `startxref` and `%%EOF`.
    suffix: Vec<u8>,
}

impl Update {
    fn total(&self, region_len: u64) -> u64 {
        self.base + self.prefix.len() as u64 + region_len + self.suffix.len() as u64
    }
}

/// Work out the update to append: which section to chain to, where to truncate, and the exact
/// bytes on either side of the payload.
fn plan_update(src: &mut dyn ReadSeek, region_len: u64) -> Result<Update> {
    let file_len = src.seek(SeekFrom::End(0))?;
    let xref_at = last_startxref(src, file_len)?;
    let last = section_at(src, xref_at, file_len, true)?;
    if dict_value(&last.dict, b"Encrypt").is_some() {
        return Err(ZolalError::InvalidRequest(
            "this PDF is encrypted; hiding in it is not supported".into(),
        ));
    }

    // If the last section is one of ours, drop it and chain to what it chained to, so hiding
    // again replaces the payload instead of stacking a second one.
    match previous_hide_base(src, &last, file_len)? {
        Some(base) => {
            let prev = appended_update_prev(&last)
                .ok_or_else(|| malformed("update section without a backward /Prev"))?;
            let chain_to = section_at(src, prev, file_len, false)?;
            build_update(&chain_to, base, region_len)
        }
        None => build_update(&last, file_len, region_len),
    }
}

/// Build the bytes around the payload, chaining to `section` and starting at `base`.
fn build_update(section: &Section, base: u64, region_len: u64) -> Result<Update> {
    let size = dict_uint(&section.dict, b"Size").ok_or_else(|| malformed("no /Size in trailer"))?;
    let root = dict_value(&section.dict, b"Root")
        .ok_or_else(|| malformed("no /Root in trailer"))?
        .to_vec();
    // Carried over so the updated document keeps its identity and metadata.
    let mut carried = Vec::new();
    for key in [b"ID".as_slice(), b"Info".as_slice()] {
        if let Some(value) = dict_value(&section.dict, key) {
            carried.extend_from_slice(b" /");
            carried.extend_from_slice(key);
            carried.push(b' ');
            carried.extend_from_slice(value);
        }
    }
    let carried = String::from_utf8_lossy(&carried).into_owned();
    let root = String::from_utf8_lossy(&root).into_owned();
    let prev = section.offset;

    let payload_obj = size; // the next free object number
    let prefix = format!("{payload_obj} 0 obj\n<< /Length {region_len} >>\nstream\n").into_bytes();
    let xref_start = base + prefix.len() as u64 + region_len + MID.len() as u64;

    let mut suffix = MID.to_vec();
    match section.form {
        XrefForm::Table => {
            if base > MAX_TABLE_OFFSET || xref_start > MAX_TABLE_OFFSET {
                return Err(ZolalError::InvalidRequest(
                    "the result would be too large for this PDF's cross-reference table \
                     (offsets over 10 digits); use a video carrier instead"
                        .into(),
                ));
            }
            // One subsection holding our single object. Entries are exactly 20 bytes.
            suffix.extend_from_slice(
                format!("xref\n{payload_obj} 1\n{base:010} 00000 n\r\n").as_bytes(),
            );
            suffix.extend_from_slice(
                format!(
                    "trailer\n<< /Size {} /Root {root} /Prev {prev}{carried} >>\n",
                    payload_obj + 1
                )
                .as_bytes(),
            );
        }
        XrefForm::Stream => {
            // The cross-reference stream is itself an object, so it lists itself too.
            let xref_obj = payload_obj + 1;
            let mut data = Vec::new();
            push_xref_entry(&mut data, base);
            push_xref_entry(&mut data, xref_start);
            suffix.extend_from_slice(
                format!(
                    "{xref_obj} 0 obj\n<< /Type /XRef /Size {} /Index [{payload_obj} 1 {xref_obj} 1] \
                     /W [1 8 2] /Root {root} /Prev {prev}{carried} /Length {} >>\nstream\n",
                    xref_obj + 1,
                    data.len()
                )
                .as_bytes(),
            );
            suffix.extend_from_slice(&data);
            suffix.extend_from_slice(b"\nendstream\nendobj\n");
        }
    }
    suffix.extend_from_slice(format!("startxref\n{xref_start}\n%%EOF\n").as_bytes());

    Ok(Update {
        base,
        prefix,
        suffix,
    })
}

/// One in-use entry in the `/W [1 8 2]` form we write: type 1, 8-byte offset, 2-byte generation.
fn push_xref_entry(data: &mut Vec<u8>, offset: u64) {
    data.push(1);
    data.extend_from_slice(&offset.to_be_bytes());
    data.extend_from_slice(&[0, 0]);
}

/// If `section` is an update we wrote, the offset its payload object starts at — which is the
/// length the file had before that hide, so truncating there restores the original exactly.
///
/// Deliberately strict: a single added stream object (plus, in the stream form, the
/// cross-reference stream itself), laid out byte for byte the way [`build_update`] lays it out.
fn previous_hide_base(
    src: &mut dyn ReadSeek,
    section: &Section,
    file_len: u64,
) -> Result<Option<u64>> {
    let Some(prev) = appended_update_prev(section) else {
        return Ok(None);
    };
    let expected = match section.form {
        XrefForm::Table => 1,
        XrefForm::Stream => 2, // the payload object and the cross-reference stream
    };
    if section.entries.len() != expected {
        return Ok(None);
    }
    // The payload object is the one that is not the cross-reference stream itself.
    let Some(&(_, payload_off)) = section
        .entries
        .iter()
        .find(|&&(_, off)| off != section.offset)
    else {
        return Ok(None);
    };
    if payload_off <= prev {
        return Ok(None); // not something this revision added
    }
    let Some((body_start, body_len)) = stream_body(src, payload_off, file_len)? else {
        return Ok(None);
    };
    // Our writer puts exactly `MID` between the payload body and the section.
    if body_start + body_len + MID.len() as u64 != section.offset {
        return Ok(None);
    }
    Ok(Some(payload_off))
}

// ---------------------------------------------------------------------------------------------
// Reading

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XrefForm {
    /// A classic `xref` table followed by a `trailer` dictionary.
    Table,
    /// A cross-reference stream object (PDF 1.5+), whose dictionary is the trailer.
    Stream,
}

/// One cross-reference section.
struct Section {
    /// Where the section starts — what `startxref` points at, and what our `/Prev` will say.
    offset: u64,
    form: XrefForm,
    /// In-use objects listed here, as `(object number, offset)`. Empty when the section is too
    /// big to be ours, or encoded in a way we do not decode.
    entries: Vec<(u64, u64)>,
    /// The trailer dictionary (for a stream section, the stream's own dictionary).
    dict: Vec<u8>,
}

/// Offset of the last cross-reference section, from the `startxref` near the end of the file.
fn last_startxref(src: &mut dyn ReadSeek, file_len: u64) -> Result<u64> {
    let window = TAIL_WINDOW.min(file_len);
    let buf = read_at(src, file_len - window, window)?;
    let pos = rfind(&buf, b"startxref")
        .ok_or_else(|| malformed("no startxref near the end of the file"))?;
    let (offset, _) =
        read_uint(&buf, pos + 9).ok_or_else(|| malformed("startxref without an offset"))?;
    if offset >= file_len {
        return Err(malformed("startxref points past the end of the file"));
    }
    Ok(offset)
}

/// Parse the cross-reference section at `offset`, reading its entries only when asked (and only
/// when there are few enough to be interesting).
fn section_at(
    src: &mut dyn ReadSeek,
    offset: u64,
    file_len: u64,
    want_entries: bool,
) -> Result<Section> {
    let head = read_at(src, offset, 16)?;
    let is_table = head.starts_with(b"xref")
        && head
            .get(4)
            .is_none_or(|b| matches!(b, b' ' | b'\r' | b'\n' | b'\t'));
    if is_table {
        table_section(src, offset, file_len, want_entries)
    } else {
        stream_section(src, offset, file_len, want_entries)
    }
}

/// A classic `xref` table: subsection headers, fixed-width entries, then `trailer`.
fn table_section(
    src: &mut dyn ReadSeek,
    offset: u64,
    file_len: u64,
    want_entries: bool,
) -> Result<Section> {
    let mut pos = offset + 4; // past "xref"
    let mut entries = Vec::new();
    let mut total = 0u64;
    let trailer_at = loop {
        let win = read_at(src, pos, 64)?;
        let i = skip_ws(&win, 0);
        if win[i..].starts_with(b"trailer") {
            break pos + (i + 7) as u64;
        }
        let (first, i) =
            read_uint(&win, i).ok_or_else(|| malformed("bad cross-reference subsection"))?;
        let (count, i) =
            read_uint(&win, i).ok_or_else(|| malformed("bad cross-reference subsection"))?;

        // Entries start after the end of the header line.
        let mut j = i;
        while win.get(j).is_some_and(|b| matches!(b, b' ' | b'\r')) {
            j += 1;
        }
        if win.get(j) == Some(&b'\n') {
            j += 1;
        }
        let entries_start = pos + j as u64;
        let span = count
            .checked_mul(TABLE_ENTRY)
            .ok_or_else(|| malformed("absurd subsection size"))?;
        if entries_start + span > file_len {
            return Err(malformed(
                "cross-reference table runs past the end of the file",
            ));
        }

        total = total.saturating_add(count);
        if want_entries && total <= MAX_SECTION_ENTRIES {
            let bytes = read_at(src, entries_start, span)?;
            for k in 0..count {
                let Some(entry) =
                    bytes.get((k * TABLE_ENTRY) as usize..((k + 1) * TABLE_ENTRY) as usize)
                else {
                    break;
                };
                // "oooooooooo ggggg n\r\n": the type character sits at index 17.
                if entry[17] == b'n' {
                    if let Some(at) = ascii_uint(&entry[0..10]) {
                        entries.push((first + k, at));
                    }
                }
            }
        }
        pos = entries_start + span;
    };

    let dict = dict_at(src, trailer_at, file_len)?;
    Ok(Section {
        offset,
        form: XrefForm::Table,
        entries: if total <= MAX_SECTION_ENTRIES {
            entries
        } else {
            Vec::new()
        },
        dict,
    })
}

/// A cross-reference stream: an object whose dictionary is the trailer and whose body is the
/// table in binary. We decode only uncompressed ones (which is what we write).
fn stream_section(
    src: &mut dyn ReadSeek,
    offset: u64,
    file_len: u64,
    want_entries: bool,
) -> Result<Section> {
    let win = read_at(src, offset, MAX_READ.min(file_len - offset))?;
    let i = skip_ws(&win, 0);
    let (_, i) = read_uint(&win, i).ok_or_else(|| malformed("not a cross-reference section"))?;
    let (_, i) = read_uint(&win, i).ok_or_else(|| malformed("not a cross-reference section"))?;
    let i = keyword(&win, i, b"obj").ok_or_else(|| malformed("not a cross-reference section"))?;
    let (ds, de) =
        dict_span(&win, i).ok_or_else(|| malformed("cross-reference stream has no dictionary"))?;
    let dict = win[ds..de].to_vec();

    let mut entries = Vec::new();
    // A compressed table is somebody else's (we always write ours uncompressed), so there is
    // nothing of ours to find in it, and skipping it keeps us free of an inflate dependency.
    if want_entries && dict_value(&dict, b"Filter").is_none() {
        if let Some(body) = stream_start(&win, de) {
            entries = xref_stream_entries(&win, body, &dict);
        }
    }
    Ok(Section {
        offset,
        form: XrefForm::Stream,
        entries,
        dict,
    })
}

/// Decode in-use entries from an uncompressed cross-reference stream body.
fn xref_stream_entries(win: &[u8], body: usize, dict: &[u8]) -> Vec<(u64, u64)> {
    let Some(widths) = dict_uint_array(dict, b"W") else {
        return Vec::new();
    };
    if widths.len() < 3 {
        return Vec::new();
    }
    let (w0, w1, w2) = (widths[0] as usize, widths[1] as usize, widths[2] as usize);
    let stride = w0 + w1 + w2;
    if stride == 0 || w1 == 0 || w1 > 8 {
        return Vec::new();
    }
    let index = dict_uint_array(dict, b"Index")
        .unwrap_or_else(|| vec![0, dict_uint(dict, b"Size").unwrap_or(0)]);

    let mut entries = Vec::new();
    let mut at = body;
    let mut seen = 0u64;
    for pair in index.chunks(2) {
        let [first, count] = pair else { break };
        for k in 0..*count {
            seen += 1;
            if seen > MAX_SECTION_ENTRIES {
                return entries;
            }
            let Some(row) = win.get(at..at + stride) else {
                return entries;
            };
            at += stride;
            // A zero-width type field means type 1 (in use), per the spec's default.
            let kind = if w0 == 0 { 1 } else { be_uint(&row[..w0]) };
            if kind == 1 {
                entries.push((first + k, be_uint(&row[w0..w0 + w1])));
            }
        }
    }
    entries
}

/// The byte range of a plain, uncompressed stream object's body, if `offset` holds one.
///
/// `None` for anything we would not have written: a compressed stream, a cross-reference stream,
/// an indirect `/Length`, or a body running past the end of the file.
fn stream_body(src: &mut dyn ReadSeek, offset: u64, file_len: u64) -> Result<Option<(u64, u64)>> {
    if offset >= file_len {
        return Ok(None);
    }
    let win = read_at(src, offset, MAX_READ.min(file_len - offset))?;
    let i = skip_ws(&win, 0);
    let Some((_, i)) = read_uint(&win, i) else {
        return Ok(None);
    };
    let Some((_, i)) = read_uint(&win, i) else {
        return Ok(None);
    };
    let Some(i) = keyword(&win, i, b"obj") else {
        return Ok(None);
    };
    let Some((ds, de)) = dict_span(&win, i) else {
        return Ok(None);
    };
    let dict = &win[ds..de];
    if dict_value(dict, b"Filter").is_some() || dict_value(dict, b"Type") == Some(b"/XRef") {
        return Ok(None);
    }
    let Some(raw_len) = dict_value(dict, b"Length") else {
        return Ok(None);
    };
    if raw_len.contains(&b'R') {
        return Ok(None); // indirect /Length: not ours, and resolving it means parsing more
    }
    let Some((len, _)) = read_uint(raw_len, 0) else {
        return Ok(None);
    };
    let Some(body) = stream_start(&win, de) else {
        return Ok(None);
    };
    let start = offset + body as u64;
    if start + len > file_len {
        return Ok(None);
    }
    Ok(Some((start, len)))
}

/// Index just past `stream` and its end-of-line marker, starting the search at `from`.
fn stream_start(win: &[u8], from: usize) -> Option<usize> {
    let i = keyword(win, from, b"stream")?;
    let mut k = i;
    if win.get(k) == Some(&b'\r') {
        k += 1;
    }
    // The spec requires a newline after `stream`; a lone CR is not allowed.
    if win.get(k) == Some(&b'\n') {
        Some(k + 1)
    } else {
        None
    }
}

/// Read the dictionary starting at `offset`.
fn dict_at(src: &mut dyn ReadSeek, offset: u64, file_len: u64) -> Result<Vec<u8>> {
    let win = read_at(src, offset, MAX_READ.min(file_len - offset))?;
    let (ds, de) = dict_span(&win, 0).ok_or_else(|| malformed("trailer has no dictionary"))?;
    Ok(win[ds..de].to_vec())
}

// ---------------------------------------------------------------------------------------------
// Minimal PDF syntax helpers
//
// Just enough to read a trailer dictionary and an object header. Not a PDF parser.

/// Skip whitespace and comments.
fn skip_ws(buf: &[u8], mut i: usize) -> usize {
    loop {
        while buf
            .get(i)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0'))
        {
            i += 1;
        }
        if buf.get(i) == Some(&b'%') {
            while buf.get(i).is_some_and(|&b| b != b'\n' && b != b'\r') {
                i += 1;
            }
        } else {
            return i;
        }
    }
}

/// Read a decimal integer, skipping leading whitespace. Returns the value and the index after it.
fn read_uint(buf: &[u8], i: usize) -> Option<(u64, usize)> {
    let start = skip_ws(buf, i);
    let mut end = start;
    while buf.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    Some((ascii_uint(buf.get(start..end)?)?, end))
}

fn ascii_uint(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(value)
}

fn be_uint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
}

/// Index just past `kw` if it sits at `i` (after whitespace).
fn keyword(buf: &[u8], i: usize, kw: &[u8]) -> Option<usize> {
    let start = skip_ws(buf, i);
    buf.get(start..)?
        .starts_with(kw)
        .then_some(start + kw.len())
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// Byte range of the `<< ... >>` at or after `i`, matching nesting and ignoring strings.
fn dict_span(buf: &[u8], i: usize) -> Option<(usize, usize)> {
    let start = skip_ws(buf, i);
    if !buf.get(start..)?.starts_with(b"<<") {
        return None;
    }
    let mut depth = 0usize;
    let mut j = start;
    while j < buf.len() {
        if buf[j..].starts_with(b"<<") {
            depth += 1;
            j += 2;
        } else if buf[j..].starts_with(b">>") {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return Some((start, j));
            }
        } else if buf[j] == b'(' {
            j = skip_literal_string(buf, j);
        } else {
            j += 1;
        }
    }
    None
}

/// Index just past a `( ... )` string, honouring nesting and backslash escapes.
fn skip_literal_string(buf: &[u8], mut j: usize) -> usize {
    let mut depth = 0usize;
    while j < buf.len() {
        match buf[j] {
            b'\\' => j += 2,
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => j += 1,
        }
    }
    j
}

/// Raw bytes of `key`'s value in a dictionary, which must include its `<<` and `>>`.
fn dict_value<'a>(dict: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let inner = dict.get(2..dict.len().checked_sub(2)?)?;
    let mut i = 0;
    loop {
        i = skip_ws(inner, i);
        if inner.get(i) != Some(&b'/') {
            return None;
        }
        let name_end = name_end(inner, i + 1);
        let value_start = skip_ws(inner, name_end);
        let value_end = value_end(inner, value_start);
        if &inner[i + 1..name_end] == key {
            return inner.get(value_start..value_end);
        }
        if value_end <= i {
            return None; // no progress: malformed
        }
        i = value_end;
    }
}

/// Index just past a name's characters (a name runs to the next delimiter or whitespace).
fn name_end(buf: &[u8], mut i: usize) -> usize {
    while buf.get(i).is_some_and(|b| {
        !matches!(
            b,
            b' ' | b'\t'
                | b'\r'
                | b'\n'
                | b'\x0c'
                | b'\0'
                | b'/'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'('
                | b')'
                | b'%'
        )
    }) {
        i += 1;
    }
    i
}

/// Index just past one value token, whatever kind it is.
fn value_end(buf: &[u8], i: usize) -> usize {
    match buf.get(i) {
        Some(b'/') => name_end(buf, i + 1),
        Some(b'(') => skip_literal_string(buf, i),
        Some(b'[') => {
            let mut depth = 0usize;
            let mut j = i;
            while j < buf.len() {
                match buf[j] {
                    b'[' => {
                        depth += 1;
                        j += 1;
                    }
                    b']' => {
                        depth -= 1;
                        j += 1;
                        if depth == 0 {
                            return j;
                        }
                    }
                    b'(' => j = skip_literal_string(buf, j),
                    _ => j += 1,
                }
            }
            j
        }
        Some(b'<') if buf.get(i + 1) == Some(&b'<') => {
            dict_span(buf, i).map_or(buf.len(), |(_, end)| end)
        }
        Some(b'<') => buf[i..]
            .iter()
            .position(|&b| b == b'>')
            .map_or(buf.len(), |p| i + p + 1),
        Some(b) if b.is_ascii_digit() => {
            // Either a number or an indirect reference, `12 0 R`.
            let (_, after) = read_uint(buf, i).unwrap_or((0, i));
            if let Some((_, after_gen)) = read_uint(buf, after) {
                if let Some(after_r) = keyword(buf, after_gen, b"R") {
                    return after_r;
                }
            }
            after
        }
        Some(_) => name_end(buf, i),
        None => i,
    }
}

fn dict_uint(dict: &[u8], key: &[u8]) -> Option<u64> {
    let value = dict_value(dict, key)?;
    if value.contains(&b'R') {
        return None; // an indirect reference, which we never write and do not resolve
    }
    read_uint(value, 0).map(|(n, _)| n)
}

/// The integers in an array value such as `/W [1 8 2]`.
fn dict_uint_array(dict: &[u8], key: &[u8]) -> Option<Vec<u64>> {
    let value = dict_value(dict, key)?;
    let inner = value.strip_prefix(b"[")?.strip_suffix(b"]")?;
    let mut out = Vec::new();
    let mut i = 0;
    while let Some((n, next)) = read_uint(inner, i) {
        if next == i {
            break;
        }
        out.push(n);
        i = next;
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// I/O helpers

/// Read up to `len` bytes at `offset`. A short result means end of file, not an error.
fn read_at(src: &mut dyn ReadSeek, offset: u64, len: u64) -> Result<Vec<u8>> {
    let len = len.min(MAX_READ) as usize;
    src.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    buf.truncate(filled);
    Ok(buf)
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

fn malformed(detail: impl Into<String>) -> ZolalError {
    ZolalError::MalformedContainer {
        format: "PDF",
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The four objects every fixture shares: catalog, page tree, one page, one content stream.
    fn body_objects() -> Vec<Vec<u8>> {
        let content = b"BT /F1 12 Tf 20 100 Td (hello) Tj ET";
        let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream.extend_from_slice(content);
        stream.extend_from_slice(b"\nendstream");
        vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>".to_vec(),
            stream,
        ]
    }

    /// Write `%PDF` header and the shared objects; returns the bytes and each object's offset.
    fn body() -> (Vec<u8>, Vec<u64>) {
        let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in body_objects().iter().enumerate() {
            offsets.push(out.len() as u64);
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(object);
            out.extend_from_slice(b"\nendobj\n");
        }
        (out, offsets)
    }

    /// A valid PDF with a classic cross-reference table. `extra` goes inside the trailer dict.
    fn classic_pdf(extra: &str) -> Vec<u8> {
        let (mut out, offsets) = body();
        let xref = out.len() as u64;
        out.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f\r\n");
        for offset in &offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n\r\n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R{extra} >>\nstartxref\n{xref}\n%%EOF\n",
                offsets.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// A valid PDF whose cross-reference section is an (uncompressed) stream, as PDF 1.5+ writes.
    fn xref_stream_pdf() -> Vec<u8> {
        let (mut out, offsets) = body();
        let xref = out.len() as u64;
        let entry = |data: &mut Vec<u8>, kind: u8, at: u64, gen: u16| {
            data.push(kind);
            data.extend_from_slice(&(at as u32).to_be_bytes());
            data.extend_from_slice(&gen.to_be_bytes());
        };
        let mut data = Vec::new();
        entry(&mut data, 0, 0, 0xFFFF); // the free list head
        for offset in &offsets {
            entry(&mut data, 1, *offset, 0);
        }
        entry(&mut data, 1, xref, 0); // the cross-reference stream lists itself
        out.extend_from_slice(
            format!(
                "5 0 obj\n<< /Type /XRef /Size 6 /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
                data.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        out.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        out
    }

    fn embed(pdf: &[u8], region: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        PdfCarrier
            .embed(
                &mut Cursor::new(pdf),
                &mut out,
                Technique::PdfObject,
                region.len() as u64,
                &mut |w| Ok(w.write_all(region)?),
            )
            .unwrap();
        out
    }

    fn regions(pdf: &[u8]) -> Vec<Region> {
        PdfCarrier.candidate_regions(&mut Cursor::new(pdf)).unwrap()
    }

    fn read_back(pdf: &[u8]) -> Vec<u8> {
        let regions = regions(pdf);
        let region = regions.first().expect("a candidate region");
        let mut got = Vec::new();
        PdfCarrier
            .open_region(&mut Cursor::new(pdf), region)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        got
    }

    #[test]
    fn a_clean_pdf_offers_nothing() {
        // No /Prev means the file was never updated, so none of its streams can be ours. Saying
        // "nothing hidden" beats making reveal try every content stream and cry wrong passphrase.
        for pdf in [classic_pdf(""), xref_stream_pdf()] {
            assert!(regions(&pdf).is_empty());
        }
    }

    #[test]
    fn embed_appends_an_update_that_reads_back() {
        let region: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        for pdf in [classic_pdf(""), xref_stream_pdf()] {
            let out = embed(&pdf, &region);
            assert_eq!(&out[..pdf.len()], &pdf[..], "original bytes untouched");
            assert!(out.ends_with(b"%%EOF\n"), "file still ends with %%EOF");

            // The update's startxref must point at the section we just wrote.
            let tail = String::from_utf8_lossy(&out[out.len() - 40..]).into_owned();
            let at: u64 = tail
                .rsplit("startxref")
                .next()
                .unwrap()
                .trim()
                .trim_end_matches("%%EOF")
                .trim()
                .parse()
                .unwrap();
            assert!(at > pdf.len() as u64 && at < out.len() as u64);

            let found = regions(&out);
            assert_eq!(found.len(), 1, "exactly one candidate: our object");
            assert_eq!(found[0].len, region.len() as u64);
            assert_eq!(read_back(&out), region);
        }
    }

    #[test]
    fn the_appended_section_matches_the_form_already_in_use() {
        // A classic file gets a classic table; a 1.5+ file gets a cross-reference stream.
        let classic = embed(&classic_pdf(""), b"x");
        assert!(classic.ends_with(b"%%EOF\n"));
        assert!(String::from_utf8_lossy(&classic[classic_pdf("").len()..]).contains("\nxref\n"));

        let modern = embed(&xref_stream_pdf(), b"x");
        let appended = String::from_utf8_lossy(&modern[xref_stream_pdf().len()..]).into_owned();
        assert!(appended.contains("/Type /XRef"), "{appended}");
        assert!(!appended.contains("\nxref\n"));
    }

    #[test]
    fn output_len_matches_embed() {
        for pdf in [classic_pdf(""), xref_stream_pdf()] {
            for len in [0u64, 1, 5000] {
                let predicted = PdfCarrier
                    .output_len(&mut Cursor::new(&pdf), Technique::PdfObject, len)
                    .unwrap();
                let region = vec![7u8; len as usize];
                assert_eq!(predicted, embed(&pdf, &region).len() as u64, "len {len}");
            }
        }
    }

    #[test]
    fn re_hiding_replaces_the_previous_update() {
        for pdf in [classic_pdf(""), xref_stream_pdf()] {
            let first = embed(&pdf, b"the first secret");
            let second = embed(&first, b"the second secret");
            // The old update is truncated away, not stacked on top of.
            assert_eq!(second.len(), pdf.len() + (second.len() - pdf.len()));
            assert_eq!(
                second.len() as u64,
                PdfCarrier
                    .output_len(&mut Cursor::new(&pdf), Technique::PdfObject, 17)
                    .unwrap(),
                "same size as hiding into the original"
            );
            assert_eq!(&second[..pdf.len()], &pdf[..]);
            assert_eq!(read_back(&second), b"the second secret");
            assert_eq!(regions(&second).len(), 1, "no stale region left behind");
        }
    }

    #[test]
    fn a_foreign_update_is_never_truncated() {
        // Somebody else's incremental update (two added objects) must survive untouched: losing
        // a user's annotations would be far worse than leaving a stale region behind.
        let base = classic_pdf("");
        let mut updated = base.clone();
        let prev_xref = {
            let tail = String::from_utf8_lossy(&base[base.len() - 40..]).into_owned();
            tail.rsplit("startxref")
                .next()
                .unwrap()
                .trim()
                .trim_end_matches("%%EOF")
                .trim()
                .parse::<u64>()
                .unwrap()
        };
        let five = updated.len() as u64;
        updated.extend_from_slice(b"5 0 obj\n<< /Note (annotation) >>\nendobj\n");
        let six = updated.len() as u64;
        updated.extend_from_slice(b"6 0 obj\n<< /Note (another) >>\nendobj\n");
        let xref = updated.len() as u64;
        updated.extend_from_slice(b"xref\n5 2\n");
        updated
            .extend_from_slice(format!("{five:010} 00000 n\r\n{six:010} 00000 n\r\n").as_bytes());
        updated.extend_from_slice(
            format!(
                "trailer\n<< /Size 7 /Root 1 0 R /Prev {prev_xref} >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );

        let out = embed(&updated, b"secret");
        assert_eq!(&out[..updated.len()], &updated[..], "foreign update kept");
        assert_eq!(read_back(&out), b"secret");
    }

    /// A linearized ("fast web view") file: the `startxref` at the end points at a section near
    /// the *front*, whose `/Prev` points *forward* to the main one. Real Illustrator and web
    /// output looks like this.
    fn linearized_pdf() -> Vec<u8> {
        let (mut out, offsets) = body();
        let main_xref = out.len() as u64;
        out.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f\r\n");
        for offset in &offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n\r\n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n0\n%%EOF\n",
                offsets.len() + 1
            )
            .as_bytes(),
        );
        // The front section: same entries, but chaining *forward* to the main one.
        let front = out.len() as u64;
        out.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f\r\n");
        for offset in &offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n\r\n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Prev {main_xref} >>\nstartxref\n{front}\n%%EOF\n",
                offsets.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn a_linearized_file_is_not_mistaken_for_an_update() {
        // Its `/Prev` points forward, so it is the original document, not an appended revision.
        // Getting this wrong means a clean PDF answers "wrong passphrase" instead of "nothing
        // hidden", and risks the replace-on-re-hide check truncating a real document.
        let pdf = linearized_pdf();
        assert!(
            regions(&pdf).is_empty(),
            "no candidates in a clean linearized file"
        );

        // Hiding still works, and still leaves the original bytes alone.
        let out = embed(&pdf, b"secret");
        assert_eq!(&out[..pdf.len()], &pdf[..]);
        assert_eq!(read_back(&out), b"secret");
        assert_eq!(regions(&out).len(), 1);
    }

    #[test]
    fn encrypted_pdfs_are_refused() {
        let encrypted = classic_pdf(" /Encrypt 9 0 R /ID [<0102> <0304>]");
        let err = PdfCarrier
            .output_len(&mut Cursor::new(&encrypted), Technique::PdfObject, 10)
            .unwrap_err();
        assert!(matches!(err, ZolalError::InvalidRequest(m) if m.contains("encrypted")));
    }

    #[test]
    fn the_document_identity_is_carried_over() {
        let pdf = classic_pdf(" /ID [<0102> <0304>] /Info 9 0 R");
        let out = embed(&pdf, b"x");
        let appended = String::from_utf8_lossy(&out[pdf.len()..]).into_owned();
        assert!(appended.contains("/ID [<0102> <0304>]"), "{appended}");
        assert!(appended.contains("/Info 9 0 R"), "{appended}");
        assert!(appended.contains("/Root 1 0 R"), "{appended}");
    }

    #[test]
    fn a_classic_table_cannot_address_beyond_ten_digits() {
        // Offsets are 10 fixed digits, so a classic file has a hard ceiling. Refuse rather than
        // write a table that silently addresses the wrong place.
        let pdf = classic_pdf("");
        let section = section_at(
            &mut Cursor::new(&pdf),
            last_startxref(&mut Cursor::new(&pdf), pdf.len() as u64).unwrap(),
            pdf.len() as u64,
            false,
        )
        .unwrap();
        let err = build_update(&section, MAX_TABLE_OFFSET, 100).unwrap_err();
        assert!(
            matches!(err, ZolalError::InvalidRequest(m) if m.contains("cross-reference table"))
        );
    }

    #[test]
    fn hostile_input_errors_and_never_panics() {
        let try_regions =
            |bytes: &[u8]| PdfCarrier.candidate_regions(&mut Cursor::new(bytes.to_vec()));
        let good = embed(&classic_pdf(""), &[3u8; 80]);
        for cut in 0..good.len() {
            let _ = try_regions(&good[..cut]);
        }
        for i in (0..good.len()).step_by(3) {
            for bit in [0x01, 0x80] {
                let mut bad = good.clone();
                bad[i] ^= bit;
                let _ = try_regions(&bad);
                let _ = PdfCarrier.output_len(&mut Cursor::new(&bad), Technique::PdfObject, 10);
            }
        }
        // No startxref at all: an error, never a panic and never a false candidate.
        let junk = b"%PDF-1.7\nnothing here at all\n";
        assert!(matches!(
            try_regions(junk),
            Err(ZolalError::MalformedContainer { .. })
        ));
    }

    #[test]
    fn dictionary_parsing_handles_awkward_values() {
        let dict =
            b"<< /A 1 0 R /B (a (nested) \\) string) /C << /D [1 2 [3]] >> /E /Name /F 42 >>";
        assert_eq!(dict_value(dict, b"A"), Some(b"1 0 R".as_slice()));
        assert_eq!(dict_value(dict, b"E"), Some(b"/Name".as_slice()));
        assert_eq!(dict_uint(dict, b"F"), Some(42));
        assert_eq!(
            dict_uint(dict, b"A"),
            None,
            "indirect refs are not integers"
        );
        assert_eq!(dict_value(dict, b"missing"), None);
        assert_eq!(
            dict_uint_array(b"<< /W [1 8 2] >>", b"W"),
            Some(vec![1, 8, 2])
        );
    }
}
