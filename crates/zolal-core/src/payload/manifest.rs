//! Bundle manifest — what files are inside, and how long each one is.
//!
//! ## Wire format (all integers little-endian)
//!
//! ```text
//! manifest := count : u32, entry * count
//! entry    := name_len : u16, name : utf-8, mime_len : u8, mime : utf-8, len : u64
//! ```
//!
//! No version field: the envelope version (inside the encryption) covers the whole plaintext.

use crate::error::{Result, ZolalError};

/// Largest encoded manifest a reader will accept.
///
/// The bundle is authenticated, but only as "made by whoever knew the passphrase", so a length
/// prefix is still never trusted for an allocation. 16 MiB is roomy: an entry is ~50 bytes.
pub const MAX_MANIFEST_LEN: u32 = 16 * 1024 * 1024;

/// Longest file name we write to disk, in bytes. APFS and ext4 both cap a name at 255.
pub const MAX_NAME_BYTES: usize = 255;

/// One file in the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Original file name.
    ///
    /// Sanitised on extraction: path separators and `..` are stripped so a malicious bundle
    /// cannot write outside the output directory. The bundle is authenticated, but it was
    /// authenticated by *whoever made it*, which is not necessarily someone we trust.
    pub name: String,
    /// MIME type, best-effort, for previewing after extraction.
    pub mime: String,
    /// Length of this file's bytes in the payload region.
    pub len: u64,
}

/// The full manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Entries in storage order; byte ranges follow the same order.
    pub entries: Vec<Entry>,
}

impl Manifest {
    /// Total bytes of file content described by this manifest.
    pub fn total_len(&self) -> u64 {
        self.entries.iter().map(|e| e.len).sum()
    }

    /// Size of [`Manifest::encode`]'s output.
    pub fn encoded_len(&self) -> u64 {
        4 + self
            .entries
            .iter()
            .map(|e| 2 + e.name.len() as u64 + 1 + e.mime.len() as u64 + 8)
            .sum::<u64>()
    }

    /// Size of the whole bundle: length prefix, manifest, then every file's bytes.
    ///
    /// This is the plaintext length the envelope seals, known before any file is read.
    pub fn bundle_len(&self) -> u64 {
        4 + self.encoded_len() + self.total_len()
    }

    /// Serialise. Fails only on a name or MIME type too long for its length field.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.encoded_len() as usize);
        let count = u32::try_from(self.entries.len())
            .map_err(|_| ZolalError::InvalidRequest("too many payload files".into()))?;
        out.extend_from_slice(&count.to_le_bytes());
        for e in &self.entries {
            let name_len = u16::try_from(e.name.len()).map_err(|_| {
                ZolalError::InvalidRequest(format!("file name too long: {}", e.name))
            })?;
            let mime_len = u8::try_from(e.mime.len()).map_err(|_| {
                ZolalError::InvalidRequest(format!("MIME type too long: {}", e.mime))
            })?;
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(e.name.as_bytes());
            out.push(mime_len);
            out.extend_from_slice(e.mime.as_bytes());
            out.extend_from_slice(&e.len.to_le_bytes());
        }
        Ok(out)
    }

    /// Parse. Every length is bounds-checked against the input; nothing is allocated from a
    /// length field alone.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cur = Cursor(bytes);
        let count = u32::from_le_bytes(cur.array()?);
        // Each entry is at least 11 bytes, which bounds `count` by the input size.
        if u64::from(count) * 11 > bytes.len() as u64 {
            return Err(corrupt("entry count exceeds manifest size"));
        }
        let mut entries = Vec::with_capacity(count as usize);
        let mut total = 0u64;
        for _ in 0..count {
            let name_len = u16::from_le_bytes(cur.array()?) as usize;
            let name = cur.utf8(name_len)?;
            let mime_len = cur.array::<1>()?[0] as usize;
            let mime = cur.utf8(mime_len)?;
            let len = u64::from_le_bytes(cur.array()?);
            total = total
                .checked_add(len)
                .ok_or_else(|| corrupt("file lengths overflow"))?;
            entries.push(Entry { name, mime, len });
        }
        if !cur.0.is_empty() {
            return Err(corrupt("trailing bytes after the last entry"));
        }
        Ok(Self { entries })
    }

    /// Reduce a stored name to a safe basename for writing to disk.
    ///
    /// Drops everything up to the last `/` or `\`, control characters, and the bidirectional
    /// overrides that can disguise an extension (`photo\u{202E}gpj.exe`). Rejects empty
    /// results, `.` and `..`, so extraction cannot escape the output directory, and trims
    /// anything over [`MAX_NAME_BYTES`] while keeping a short extension.
    pub fn safe_name(name: &str) -> Option<String> {
        let base = name.rsplit(['/', '\\']).next()?;
        let cleaned: String = base
            .chars()
            .filter(|&c| !c.is_control() && !is_bidi_control(c))
            .collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
            return None;
        }
        Some(truncate_name(cleaned, MAX_NAME_BYTES))
    }
}

/// Best-effort MIME type from a file name, for previews after extraction.
///
/// Only the types someone is likely to hide; everything else is `application/octet-stream`.
pub fn mime_for(name: &str) -> &'static str {
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => return "application/octet-stream",
    };
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" => "text/plain",
        "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    with_stem_budget(name, "", max)
}

/// `name` with ` (n)` before its extension, still within [`MAX_NAME_BYTES`]. Used to avoid
/// collisions, so the suffix is never the part that gets truncated.
pub(crate) fn numbered_name(name: &str, n: u32) -> String {
    with_stem_budget(name, &format!(" ({n})"), MAX_NAME_BYTES)
}

/// Rebuild `name` as `stem + suffix + .ext`, cutting the stem so the result fits in `max`.
fn with_stem_budget(name: &str, suffix: &str, max: usize) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, Some(ext)),
        _ => (name, None),
    };
    let ext = ext.map(|e| format!(".{e}")).unwrap_or_default();
    let budget = max.saturating_sub(suffix.len() + ext.len());
    let mut cut = budget.min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..cut])
}

fn corrupt(what: &str) -> ZolalError {
    ZolalError::CorruptBundle(format!("manifest: {what}"))
}

/// Minimal bounds-checked reader over the manifest bytes.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            return Err(corrupt("truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn utf8(&mut self, n: usize) -> Result<String> {
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| corrupt("name is not UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, len: u64) -> Entry {
        Entry {
            name: name.into(),
            mime: mime_for(name).into(),
            len,
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let m = Manifest {
            entries: vec![entry("clip.mp4", 5_000_000), entry("notes – é.txt", 0)],
        };
        let bytes = m.encode().unwrap();
        assert_eq!(bytes.len() as u64, m.encoded_len());
        assert_eq!(Manifest::decode(&bytes).unwrap(), m);
    }

    #[test]
    fn decode_rejects_garbage_without_allocating_from_it() {
        assert!(Manifest::decode(&[]).is_err());
        assert!(Manifest::decode(&u32::MAX.to_le_bytes()).is_err());
        let mut bytes = Manifest {
            entries: vec![entry("a", 1)],
        }
        .encode()
        .unwrap();
        bytes.push(0);
        assert!(Manifest::decode(&bytes).is_err());
        let overflow = Manifest {
            entries: vec![entry("a", u64::MAX), entry("b", 1)],
        };
        assert!(Manifest::decode(&overflow.encode().unwrap()).is_err());
    }

    #[test]
    fn safe_name_strips_traversal_and_disguises() {
        let cases = [
            ("../../etc/passwd", Some("passwd")),
            ("..\\..\\boot.ini", Some("boot.ini")),
            ("/abs/path/photo.jpg", Some("photo.jpg")),
            ("..", None),
            ("dir/..", None),
            ("dir/", None),
            ("  ", None),
            ("a\0b\nc.txt", Some("abc.txt")),
            ("photo\u{202E}gpj.exe", Some("photogpj.exe")),
            ("normal name.pdf", Some("normal name.pdf")),
        ];
        for (input, want) in cases {
            assert_eq!(Manifest::safe_name(input).as_deref(), want, "{input:?}");
        }
    }

    #[test]
    fn long_names_keep_their_extension() {
        let long = format!("{}.jpg", "é".repeat(200));
        let safe = Manifest::safe_name(&long).unwrap();
        assert!(safe.len() <= MAX_NAME_BYTES);
        assert!(safe.ends_with(".jpg"));
    }
}
