//! Bundle reader/writer. Streams; never holds a payload file in memory.
//!
//! The two directions are shaped by who drives them. Hide *pulls* the bundle through
//! [`BundleReader`], a [`Read`] that the encryptor consumes directly. Reveal *pushes* the
//! decrypted stream through [`read_bundle`], which writes each file as its bytes arrive.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};

use super::manifest::{mime_for, numbered_name, MAX_MANIFEST_LEN};
use super::{Entry, Manifest};
use crate::error::{Result, ZolalError};

/// Buffer for writing extracted files. Big enough to keep syscalls rare, small enough for iOS.
const WRITE_BUF: usize = 256 * 1024;

/// Build the manifest for `paths` without reading file contents.
///
/// Only needs metadata, so it's cheap enough to predict the output size before the user
/// commits to anything. Symlinks are followed; anything that isn't a regular file is refused.
pub fn plan(paths: &[PathBuf]) -> Result<Manifest> {
    if paths.is_empty() {
        return Err(ZolalError::InvalidRequest("no payload files given".into()));
    }
    let entries = paths
        .iter()
        .map(|path| {
            let meta = fs::metadata(path)?;
            if !meta.is_file() {
                return Err(ZolalError::InvalidRequest(format!(
                    "not a regular file: {}",
                    path.display()
                )));
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    ZolalError::InvalidRequest(format!("no file name: {}", path.display()))
                })?;
            Ok(Entry {
                mime: mime_for(&name).to_string(),
                name,
                len: meta.len(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Manifest { entries })
}

/// The bundle as a [`Read`]: length prefix, manifest, then each file's bytes in order.
///
/// Files are opened one at a time as the reader reaches them. Each must still be exactly the
/// length [`plan`] recorded, because the envelope size was committed from that number. A file
/// that shrank or grew in between is an error, never a silently wrong bundle.
pub struct BundleReader<'a> {
    prefix: io::Cursor<Vec<u8>>,
    paths: &'a [PathBuf],
    manifest: Manifest,
    index: usize,
    current: Option<File>,
    remaining: u64,
}

impl<'a> BundleReader<'a> {
    /// Prepare to stream `paths`, described by `manifest` (from [`plan`] over the same paths).
    pub fn new(paths: &'a [PathBuf], manifest: Manifest) -> Result<Self> {
        if paths.len() != manifest.entries.len() {
            return Err(ZolalError::InvalidRequest(
                "manifest does not match the payload list".into(),
            ));
        }
        let encoded = manifest.encode()?;
        let mut prefix = Vec::with_capacity(4 + encoded.len());
        prefix.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        prefix.extend_from_slice(&encoded);
        Ok(Self {
            prefix: io::Cursor::new(prefix),
            paths,
            manifest,
            index: 0,
            current: None,
            remaining: 0,
        })
    }

    /// Total bytes this reader will produce.
    pub fn len(&self) -> u64 {
        self.manifest.bundle_len()
    }

    /// Whether [`BundleReader::len`] is zero. It never is: the prefix alone is 8 bytes.
    pub fn is_empty(&self) -> bool {
        false
    }

    fn changed(&self, how: &str) -> io::Error {
        ZolalError::InvalidRequest(format!(
            "{} {how} while it was being hidden",
            self.paths[self.index].display()
        ))
        .into_io()
    }
}

impl Read for BundleReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = self.prefix.read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        loop {
            match self.current.as_mut() {
                Some(file) if self.remaining > 0 => {
                    let want = buf
                        .len()
                        .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
                    let n = file.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(self.changed("shrank"));
                    }
                    self.remaining -= n as u64;
                    return Ok(n);
                }
                Some(file) => {
                    if file.read(&mut [0u8; 1])? != 0 {
                        return Err(self.changed("grew"));
                    }
                    self.current = None;
                    self.index += 1;
                }
                None if self.index < self.paths.len() => {
                    self.current = Some(File::open(&self.paths[self.index])?);
                    self.remaining = self.manifest.entries[self.index].len;
                }
                None => return Ok(0),
            }
        }
    }
}

/// What [`read_bundle`] wrote.
#[derive(Debug, Clone)]
pub struct Extracted {
    /// One path per bundle entry, in bundle order.
    pub files: Vec<PathBuf>,
    /// Each entry's sanitised name, *before* collision numbering. When moving the files
    /// somewhere else, number against these, or a second clash yields `x (2) (2).txt`.
    pub names: Vec<String>,
    /// Sum of the files' lengths.
    pub total_bytes: u64,
}

/// Read a bundle from `src`, writing each file into `output_dir`.
///
/// File names go through [`Manifest::safe_name`] before anything touches the filesystem, are
/// de-duplicated case-insensitively (APFS is case-insensitive by default), and are created with
/// `create_new`, so nothing already in `output_dir` is ever overwritten.
///
/// Reads `src` to its end even after the last file. With the decrypting reader that is what
/// authenticates the final chunk, so returning `Ok` means the whole stream verified.
pub fn read_bundle(src: &mut dyn Read, output_dir: &Path) -> Result<Extracted> {
    let manifest_len = u32::from_le_bytes(read_array(src)?);
    if manifest_len > MAX_MANIFEST_LEN {
        return Err(ZolalError::CorruptBundle(format!(
            "manifest claims {manifest_len} bytes"
        )));
    }
    let mut raw = vec![0u8; manifest_len as usize];
    src.read_exact(&mut raw).map_err(eof_is_corrupt)?;
    let manifest = Manifest::decode(&raw)?;

    let mut taken = HashSet::new();
    let mut files = Vec::with_capacity(manifest.entries.len());
    let mut names = Vec::with_capacity(manifest.entries.len());
    for (i, entry) in manifest.entries.iter().enumerate() {
        let base = Manifest::safe_name(&entry.name).unwrap_or_else(|| format!("file-{}", i + 1));
        let name = unique_name(&base, |candidate| taken.contains(&candidate.to_lowercase()))?;
        taken.insert(name.to_lowercase());
        names.push(base);

        let path = output_dir.join(&name);
        let mut out = BufWriter::with_capacity(WRITE_BUF, File::create_new(&path)?);
        let copied = io::copy(&mut (&mut *src).take(entry.len), &mut out)?;
        if copied != entry.len {
            return Err(ZolalError::CorruptBundle(format!(
                "payload ends inside file {}",
                i + 1
            )));
        }
        out.into_inner().map_err(io::IntoInnerError::into_error)?;
        files.push(path);
    }

    if src.read(&mut [0u8; 1])? != 0 {
        return Err(ZolalError::CorruptBundle("data after the last file".into()));
    }
    Ok(Extracted {
        files,
        names,
        total_bytes: manifest.total_len(),
    })
}

/// First of `name`, `name (2)`, `name (3)`, … that `taken` rejects.
pub(crate) fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> Result<String> {
    if !taken(name) {
        return Ok(name.to_string());
    }
    (2..10_000)
        .map(|n| numbered_name(name, n))
        .find(|candidate| !taken(candidate))
        .ok_or_else(|| ZolalError::InvalidRequest(format!("too many files named {name}")))
}

fn read_array<const N: usize>(src: &mut dyn Read) -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    src.read_exact(&mut buf).map_err(eof_is_corrupt)?;
    Ok(buf)
}

fn eof_is_corrupt(e: io::Error) -> ZolalError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        ZolalError::CorruptBundle("payload ends inside the manifest".into())
    } else {
        e.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_bytes(manifest: &Manifest, bodies: &[&[u8]]) -> Vec<u8> {
        let encoded = manifest.encode().unwrap();
        let mut out = (encoded.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&encoded);
        for body in bodies {
            out.extend_from_slice(body);
        }
        out
    }

    fn entry(name: &str, len: usize) -> Entry {
        Entry {
            name: name.into(),
            mime: "application/octet-stream".into(),
            len: len as u64,
        }
    }

    #[test]
    fn reader_output_matches_plan_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.txt");
        fs::write(&a, vec![7u8; 100_000]).unwrap();
        fs::write(&b, b"").unwrap();
        let paths = vec![a, b];

        let manifest = plan(&paths).unwrap();
        let mut reader = BundleReader::new(&paths, manifest.clone()).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, manifest.bundle_len());

        let out = tempfile::tempdir().unwrap();
        let got = read_bundle(&mut &bytes[..], out.path()).unwrap();
        assert_eq!(got.total_bytes, 100_000);
        assert_eq!(fs::read(&got.files[0]).unwrap(), vec![7u8; 100_000]);
        assert_eq!(fs::read(&got.files[1]).unwrap(), b"");
    }

    #[test]
    fn a_payload_that_changes_size_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        fs::write(&a, [1u8; 10]).unwrap();
        let paths = vec![a.clone()];
        let manifest = plan(&paths).unwrap();

        fs::write(&a, [1u8; 5]).unwrap();
        let err = BundleReader::new(&paths, manifest.clone())
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert!(
            matches!(ZolalError::from(err), ZolalError::InvalidRequest(m) if m.contains("shrank"))
        );

        fs::write(&a, [1u8; 20]).unwrap();
        let err = BundleReader::new(&paths, manifest)
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert!(
            matches!(ZolalError::from(err), ZolalError::InvalidRequest(m) if m.contains("grew"))
        );
    }

    #[test]
    fn hostile_names_stay_inside_the_output_dir() {
        let manifest = Manifest {
            entries: vec![
                entry("../../escape.txt", 1),
                entry("/etc/passwd", 1),
                entry("..", 1),
                entry("C:\\Windows\\evil.dll", 1),
                entry("Same.txt", 1),
                entry("same.TXT", 1),
            ],
        };
        let bytes = bundle_bytes(&manifest, &[b"1", b"2", b"3", b"4", b"5", b"6"]);
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("out");
        fs::create_dir(&out).unwrap();

        let got = read_bundle(&mut &bytes[..], &out).unwrap();
        let names: Vec<_> = got
            .files
            .iter()
            .map(|p| {
                assert_eq!(p.parent().unwrap(), out, "{p:?} escaped");
                p.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "escape.txt",
                "passwd",
                "file-3",
                "evil.dll",
                "Same.txt",
                "same (2).TXT"
            ]
        );
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            1,
            "nothing outside out/"
        );
    }

    #[test]
    fn framing_errors_are_corrupt_bundle() {
        let out = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            entries: vec![entry("a", 10)],
        };
        let short = bundle_bytes(&manifest, &[b"12345"]);
        assert!(matches!(
            read_bundle(&mut &short[..], out.path()),
            Err(ZolalError::CorruptBundle(_))
        ));

        let out = tempfile::tempdir().unwrap();
        let long = bundle_bytes(&manifest, &[b"0123456789", b"extra"]);
        assert!(matches!(
            read_bundle(&mut &long[..], out.path()),
            Err(ZolalError::CorruptBundle(_))
        ));

        let huge = (MAX_MANIFEST_LEN + 1).to_le_bytes();
        assert!(matches!(
            read_bundle(&mut &huge[..], out.path()),
            Err(ZolalError::CorruptBundle(_))
        ));
    }
}
