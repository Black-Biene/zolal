//! # zolal-core
//!
//! Hides encrypted payload files inside JPEG, MP4 and PDF carriers using *container*
//! techniques — the payload lives in structurally-unused space (after a JPEG's EOI, in an
//! MP4 `free` box, in an unreferenced PDF object), so the carrier still opens normally in
//! any viewer.
//!
//! All three carriers are implemented: **JPEG** (trailer and APP15), **MP4** (`free` box) and
//! **PDF** (an unreferenced stream object added by an incremental update).
//!
//! ## What this crate is NOT
//!
//! This is **not** forensic-grade steganography. A file-size check reveals that something was
//! added. The threat model is a casual snooper, not an analyst. The
//! [`plausibility`] call exists to keep the UI honest about that.
//!
//! ## Design invariants
//!
//! 1. **No platform dependencies.** Codecs live in the front end (the web page decodes HEIC
//!    and relabels MOV); this crate only ever sees JPEG, MP4, PDF or MP3 bytes.
//! 2. **Streaming everywhere.** The API takes paths, never byte arrays: a 4 GB video must
//!    not be materialised in RAM. Memory does not grow with file size: streaming holds one
//!    64 KiB chunk plus 256 KiB I/O buffers, and the largest allocation is Argon2id's working
//!    set (48 MiB by default) while the key is derived.
//! 3. **Markerless.** Nothing identifiable is written. Candidate regions are located from
//!    container structure, and the AEAD tag is the only validator.
//! 4. **All or nothing on disk.** `hide` and `clean` write to a temporary file and rename it
//!    into place;
//!    `reveal` extracts into a staging directory and moves files out only after the final
//!    chunk has authenticated. An error leaves nothing behind, in particular no partial
//!    plaintext.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all)]

pub mod carrier;
pub mod crypto;
pub mod error;
pub mod payload;
pub mod probe;
pub mod progress;

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use secrecy::SecretString;

use crate::carrier::jpeg::JpegCarrier;
use crate::carrier::mp3::Mp3Carrier;
use crate::carrier::mp4::Mp4Carrier;
use crate::carrier::pdf::PdfCarrier;
use crate::carrier::Carrier;
use crate::crypto::stream::{seal_stream, OpenReader};
use crate::crypto::{envelope, region_len, MIN_REGION_LEN};
use crate::payload::bundle::unique_name;
use crate::payload::{read_bundle, BundleReader};

pub use crate::carrier::{CarrierFormat, Technique};
pub use crate::crypto::kdf::KdfParams;
pub use crate::error::{Result, ZolalError};
pub use crate::progress::Progress;

/// Buffer for writing the output carrier.
const WRITE_BUF: usize = 256 * 1024;

/// What we can tell about a carrier file without a passphrase.
#[derive(Debug, Clone)]
pub struct CarrierInfo {
    /// Detected container format.
    pub format: CarrierFormat,
    /// Size on disk, in bytes.
    pub file_size: u64,
    /// Whether any structurally-plausible hiding region exists.
    ///
    /// This is **not** proof that something is hidden — an ordinary MP4 can contain a `free`
    /// box, and plenty of JPEGs carry trailing bytes. Confirmation requires the passphrase.
    pub has_candidate_region: bool,
}

/// How natural the output file will look. See [`plausibility`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Size growth is unremarkable for this kind of file.
    Natural,
    /// Noticeably larger than expected, but not absurd.
    Noticeable,
    /// So large it invites the question. Suggest a bigger carrier.
    Suspicious,
    /// Over the carrier's hard size limit ([`size_limit`]): [`hide`] will refuse it with
    /// [`ZolalError::PayloadTooLarge`]. The UI should suggest a video carrier instead.
    TooLarge,
}

/// The honest replacement for "capacity".
///
/// Container hiding has no meaningful capacity *limit* — you can append a gigabyte. The real
/// constraint is how odd the result looks, so we report that instead of a byte budget.
#[derive(Debug, Clone)]
pub struct Plausibility {
    /// Predicted output size in bytes.
    pub result_size: u64,
    /// `result_size` divided by the original carrier size.
    pub ratio: f32,
    /// Bucketed judgement, for UI colouring.
    pub verdict: Verdict,
}

/// Growth up to this ratio reads as natural. **Provisional**: tune against real device output.
const NATURAL_RATIO: f32 = 1.5;
/// Growth beyond this ratio invites the question. **Provisional**.
const SUSPICIOUS_RATIO: f32 = 3.0;
/// A 12 MP iPhone JPEG is roughly 2–5 MB and a 48 MP one reaches ~15 MB, so a JPEG above
/// these sizes stands out whatever it grew from. **Provisional**.
const JPEG_NATURAL_MAX: u64 = 12 * 1024 * 1024;
const JPEG_SUSPICIOUS_ABOVE: u64 = 25 * 1024 * 1024;

/// Hard ceiling on a JPEG carrier's output: [`hide`] refuses anything bigger and points to a
/// video carrier instead.
///
/// Not about plausibility but about the photo still opening. Viewers load the whole file:
/// macOS ImageIO (the same framework as iOS Photos) needed ~2× the file size in RAM, 2.1 GB for
/// a 1 GiB stego JPEG, and ffmpeg refused that file outright. A photo that won't open gives the
/// secret away. On an iPhone 13 a 49.9 MiB stego photo decoded with ~2 GB to spare and opened
/// in Photos without stutter, so the ceiling holds there; older devices are unmeasured.
pub const JPEG_MAX_OUTPUT: u64 = 50 * 1024 * 1024;

/// The largest output [`hide`] will write for `format`, if it has a limit.
///
/// Only JPEG has one. Players stream video, so MP4 is where big payloads belong. PDF has no
/// limit until its carrier is measured.
pub fn size_limit(format: CarrierFormat) -> Option<u64> {
    match format {
        CarrierFormat::Jpeg => Some(JPEG_MAX_OUTPUT),
        CarrierFormat::Mp4 | CarrierFormat::Pdf | CarrierFormat::Mp3 => None,
    }
}

/// The carrier format to suggest when `format` hits its [`size_limit`].
fn bigger_carrier(format: CarrierFormat) -> CarrierFormat {
    match format {
        CarrierFormat::Jpeg | CarrierFormat::Pdf | CarrierFormat::Mp3 => CarrierFormat::Mp4,
        CarrierFormat::Mp4 => CarrierFormat::Mp4,
    }
}

impl Plausibility {
    fn assess(format: CarrierFormat, carrier_size: u64, result_size: u64) -> Self {
        let ratio = result_size as f32 / carrier_size.max(1) as f32;
        let (natural_max, suspicious_above) = match format {
            CarrierFormat::Jpeg => (JPEG_NATURAL_MAX, JPEG_SUSPICIOUS_ABOVE),
            CarrierFormat::Mp4 | CarrierFormat::Pdf | CarrierFormat::Mp3 => (u64::MAX, u64::MAX),
        };
        let verdict = if size_limit(format).is_some_and(|limit| result_size > limit) {
            Verdict::TooLarge
        } else if ratio > SUSPICIOUS_RATIO || result_size > suspicious_above {
            Verdict::Suspicious
        } else if ratio > NATURAL_RATIO || result_size > natural_max {
            Verdict::Noticeable
        } else {
            Verdict::Natural
        };
        Self {
            result_size,
            ratio,
            verdict,
        }
    }
}

/// Request to hide payload files inside a carrier.
pub struct HideRequest {
    /// Carrier file. Must already be JPEG, MP4, PDF or MP3 (the front end converts other formats).
    pub carrier: PathBuf,
    /// One or more files to hide.
    pub payloads: Vec<PathBuf>,
    /// Where to write the result. Written via temp file + atomic rename, so it may be the
    /// carrier itself.
    pub output: PathBuf,
    /// User passphrase. Zeroized on drop.
    pub passphrase: SecretString,
    /// Which container technique to use; [`Technique::Auto`] picks the default per format.
    pub technique: Technique,
    /// Argon2id cost override. Leave `None`: the current envelope version's parameters are
    /// used. `Some` exists for tests and benchmarks, and a file hidden with `Some(p)` can only
    /// be revealed with the same `Some(p)`.
    pub kdf_params: Option<KdfParams>,
}

/// Request to recover a hidden payload.
pub struct RevealRequest {
    /// The carrier to inspect.
    pub carrier: PathBuf,
    /// Directory to write recovered files into. Created if missing; existing files in it are
    /// never overwritten (a clash gets a ` (2)` suffix).
    pub output_dir: PathBuf,
    /// User passphrase.
    pub passphrase: SecretString,
    /// Argon2id cost override. Leave `None`: every known envelope version's parameters are
    /// tried. `Some` must match what the file was hidden with.
    pub kdf_params: Option<KdfParams>,
}

/// Outcome of a successful [`hide`].
#[derive(Debug, Clone)]
pub struct HideReport {
    /// Final size of the written carrier.
    pub output_size: u64,
    /// Technique actually used (resolves [`Technique::Auto`]).
    pub technique: Technique,
    /// Plausibility of the result, for a post-hoc UI warning.
    pub plausibility: Plausibility,
}

/// Outcome of a successful [`reveal`].
#[derive(Debug, Clone)]
pub struct RevealReport {
    /// Paths of the recovered files, in bundle order.
    pub files: Vec<PathBuf>,
    /// Total plaintext bytes recovered.
    pub total_bytes: u64,
    /// Where the payload was found.
    pub technique: Technique,
}

/// Identify a carrier file and report whether it could hold (or already holds) a payload.
pub fn probe(path: &Path) -> Result<CarrierInfo> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let format = sniff(&mut file)?;
    let regions = carrier_for(format)?.candidate_regions(&mut file)?;
    Ok(CarrierInfo {
        format,
        file_size,
        has_candidate_region: regions.iter().any(|r| r.len >= MIN_REGION_LEN),
    })
}

/// Predict how natural the output will look before committing to the operation.
///
/// `payload_bytes` is the total size of the files to hide. The prediction is for the default
/// technique. The carrier side is exact (it accounts for earlier hidden content that [`hide`]
/// would drop); only the manifest is left out, tens of bytes per file.
/// [`HideReport::plausibility`] is exact. A [`Verdict::TooLarge`] here means [`hide`] will
/// refuse, so the UI can suggest a video before the user commits.
///
/// Cheap and side-effect free: it walks the carrier's structure but never its hidden data.
pub fn plausibility(carrier: &Path, payload_bytes: u64) -> Result<Plausibility> {
    let mut file = File::open(carrier)?;
    let carrier_size = file.metadata()?.len();
    let format = sniff(&mut file)?;
    let result_size = carrier_for(format)?.output_len(
        &mut file,
        Technique::Auto.resolve(format),
        region_len(payload_bytes),
    )?;
    Ok(Plausibility::assess(format, carrier_size, result_size))
}

/// Hide `payloads` inside `carrier`, writing the result to `output`.
///
/// Streams throughout; reports progress as plaintext bytes sealed and honours cancellation via
/// `progress`. Any earlier hidden content in the carrier is replaced (see [`carrier::jpeg`]).
pub fn hide(req: HideRequest, progress: &dyn Progress) -> Result<HideReport> {
    let mut carrier_file = File::open(&req.carrier)?;
    let carrier_size = carrier_file.metadata()?.len();
    let format = sniff(&mut carrier_file)?;
    let carrier = carrier_for(format)?;
    let technique = req.technique.resolve(format);
    if technique.format() != Some(format) {
        return Err(ZolalError::InvalidRequest(format!(
            "{technique:?} can't be used on a {} carrier",
            format.name()
        )));
    }

    let manifest = payload::plan(&req.payloads)?;
    let bundle_len = manifest.bundle_len();
    let region = region_len(bundle_len);

    // Refuse before any key derivation or writing: the exact size is known up front.
    let expected_size = carrier.output_len(&mut carrier_file, technique, region)?;
    if let Some(limit) = size_limit(format).filter(|&limit| expected_size > limit) {
        return Err(ZolalError::PayloadTooLarge {
            output_size: expected_size,
            limit,
            suggestion: bigger_carrier(format),
        });
    }

    let params = req.kdf_params.unwrap_or_else(envelope::current_params);
    let mut bundle = BundleReader::new(&req.payloads, manifest)?;

    let mut out = TempFile::create_beside(&req.output)?;
    {
        let mut writer = BufWriter::with_capacity(WRITE_BUF, &mut out.file);
        let written = carrier.embed(
            &mut carrier_file,
            &mut writer,
            technique,
            region,
            &mut |dst| {
                seal_stream(
                    &mut bundle,
                    bundle_len,
                    dst,
                    &req.passphrase,
                    params,
                    progress,
                )
                .map(drop)
            },
        )?;
        if written != expected_size {
            // The size limit was checked against the prediction, so the two must agree.
            return Err(ZolalError::Io(io::Error::other(format!(
                "internal error: predicted {expected_size} bytes, wrote {written}"
            ))));
        }
        writer
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
    }
    let output_size = out.commit(&req.output)?;

    Ok(HideReport {
        output_size,
        technique,
        plausibility: Plausibility::assess(format, carrier_size, output_size),
    })
}

/// Request to write a carrier back out with its hidden content removed.
pub struct CleanRequest {
    /// The carrier to clean.
    pub carrier: PathBuf,
    /// Where to write the cleaned file. Written via temp file + atomic rename, so it may be the
    /// carrier itself.
    pub output: PathBuf,
    /// User passphrase. Required, and not for decryption's sake: the format is markerless, so
    /// the only way to know *which* region is ours — rather than a region some other tool left —
    /// is to find the one that authenticates.
    pub passphrase: SecretString,
    /// Argon2id cost override. Leave `None`.
    pub kdf_params: Option<KdfParams>,
}

/// Outcome of a successful [`clean`].
#[derive(Debug, Clone)]
pub struct CleanReport {
    /// Size of the cleaned file.
    pub output_size: u64,
    /// How many bytes the carrier shed.
    pub removed_bytes: u64,
    /// Where the payload had been.
    pub technique: Technique,
}

/// Remove hidden content from `carrier`, writing an ordinary file of the same format to `output`.
///
/// The passphrase must be the one the content was hidden with. That is not about decrypting it —
/// nothing is decrypted beyond the first chunk — but about *identification*: hiding is markerless,
/// so a candidate region is only known to be ours when its AEAD tag verifies. Stripping a region
/// on structure alone could throw away something another tool put there.
///
/// Errors match [`reveal`]: [`ZolalError::NoHiddenData`] when there is no plausible region at all,
/// [`ZolalError::WrongPassphrase`] when none authenticates. On any error nothing is written.
pub fn clean(req: CleanRequest, progress: &dyn Progress) -> Result<CleanReport> {
    let mut file = File::open(&req.carrier)?;
    let carrier_size = file.metadata()?.len();
    let format = sniff(&mut file)?;
    let carrier = carrier_for(format)?;

    let regions: Vec<_> = carrier
        .candidate_regions(&mut file)?
        .into_iter()
        .filter(|r| r.len >= MIN_REGION_LEN)
        .collect();
    if regions.is_empty() {
        return Err(ZolalError::NoHiddenData {
            carrier_looks_reencoded: carrier.looks_reencoded(&mut file)?,
        });
    }

    let kdf_candidates = match req.kdf_params {
        Some(params) => vec![params],
        None => envelope::kdf_candidates(),
    };

    // Same search as `reveal`, and stopping at chunk 0: proving the region is ours costs one key
    // derivation, not a walk through the whole payload.
    let mut found = None;
    'search: for region in &regions {
        for &params in &kdf_candidates {
            let src = carrier.open_region(&mut file, region)?;
            match OpenReader::new(src, region.len, &req.passphrase, params, progress) {
                Ok(_) => {
                    found = Some(region.technique);
                    break 'search;
                }
                Err(ZolalError::WrongPassphrase) => continue,
                Err(e) => return Err(e),
            }
        }
    }
    let Some(technique) = found else {
        return Err(ZolalError::WrongPassphrase);
    };

    file.seek(SeekFrom::Start(0))?;
    let mut out = TempFile::create_beside(&req.output)?;
    {
        let mut writer = BufWriter::with_capacity(WRITE_BUF, &mut out.file);
        carrier.strip(&mut file, &mut writer)?;
        writer
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
    }
    let output_size = out.commit(&req.output)?;

    Ok(CleanReport {
        output_size,
        removed_bytes: carrier_size.saturating_sub(output_size),
        technique,
    })
}

/// Recover hidden files from `carrier` into `output_dir`.
///
/// Tries each candidate region in priority order; the AEAD tag decides which (if any) is real.
/// Errors, in the order the UI should read them:
///
/// - [`ZolalError::NoHiddenData`]: no region big enough to be ours exists at all;
/// - [`ZolalError::WrongPassphrase`]: regions exist, none authenticates;
/// - [`ZolalError::DamagedPayload`]: one authenticated, then broke.
///
/// On any error, nothing is left in `output_dir`.
pub fn reveal(req: RevealRequest, progress: &dyn Progress) -> Result<RevealReport> {
    let mut file = File::open(&req.carrier)?;
    let format = sniff(&mut file)?;
    let carrier = carrier_for(format)?;
    let regions: Vec<_> = carrier
        .candidate_regions(&mut file)?
        .into_iter()
        .filter(|r| r.len >= MIN_REGION_LEN)
        .collect();
    if regions.is_empty() {
        return Err(ZolalError::NoHiddenData {
            carrier_looks_reencoded: carrier.looks_reencoded(&mut file)?,
        });
    }

    let kdf_candidates = match req.kdf_params {
        Some(params) => vec![params],
        None => envelope::kdf_candidates(),
    };
    for region in &regions {
        for &params in &kdf_candidates {
            let src = carrier.open_region(&mut file, region)?;
            let mut opened =
                match OpenReader::new(src, region.len, &req.passphrase, params, progress) {
                    Ok(reader) => reader,
                    Err(ZolalError::WrongPassphrase) => continue,
                    Err(e) => return Err(e),
                };
            let (files, total_bytes) = extract(&mut opened, &req.output_dir)?;
            return Ok(RevealReport {
                files,
                total_bytes,
                technique: region.technique,
            });
        }
    }
    Err(ZolalError::WrongPassphrase)
}

/// Unbundle into a staging directory, then move the files out once the stream has verified.
fn extract(src: &mut dyn Read, output_dir: &Path) -> Result<(Vec<PathBuf>, u64)> {
    fs::create_dir_all(output_dir)?;
    let staging = Staging::create(output_dir)?;
    let extracted = read_bundle(src, &staging.dir)?;
    let files = staging.commit(&extracted.files, &extracted.names, output_dir)?;
    Ok((files, extracted.total_bytes))
}

/// Read the first bytes and identify the format. Leaves the file at offset 0.
fn sniff(file: &mut File) -> Result<CarrierFormat> {
    let mut head = [0u8; probe::PROBE_LEN];
    let mut n = 0;
    while n < head.len() {
        match file.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    file.seek(SeekFrom::Start(0))?;
    let head = &head[..n];
    if head.is_empty() {
        return Err(ZolalError::CarrierTooSmall);
    }
    probe::detect(head).ok_or_else(|| ZolalError::UnsupportedFormat {
        detected: probe::describe(head),
    })
}

/// The carrier implementation for `format`, if it exists yet.
fn carrier_for(format: CarrierFormat) -> Result<&'static dyn Carrier> {
    match format {
        CarrierFormat::Jpeg => Ok(&JpegCarrier),
        CarrierFormat::Mp4 => Ok(&Mp4Carrier),
        CarrierFormat::Pdf => Ok(&PdfCarrier),
        CarrierFormat::Mp3 => Ok(&Mp3Carrier),
    }
}

/// Random suffix for temporary names. Deliberately generic: a leftover file after a crash
/// shouldn't announce what made it.
fn temp_name(prefix: &str) -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|e| ZolalError::Io(io::Error::other(format!("OS random source failed: {e}"))))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(".{prefix}{hex}.tmp"))
}

/// A file written beside its destination and renamed into place, or deleted if dropped first.
struct TempFile {
    path: PathBuf,
    file: File,
    committed: bool,
}

impl TempFile {
    fn create_beside(dest: &Path) -> Result<Self> {
        let dir = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        loop {
            let path = dir.join(temp_name("")?);
            match File::create_new(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        committed: false,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Flush to stable storage and rename over `dest`. Returns the file's size.
    fn commit(mut self, dest: &Path) -> Result<u64> {
        self.file.sync_all()?;
        let len = self.file.metadata()?.len();
        fs::rename(&self.path, dest)?;
        self.committed = true;
        Ok(len)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A private directory inside the output directory, removed with everything in it on drop.
struct Staging {
    dir: PathBuf,
}

impl Staging {
    fn create(parent: &Path) -> Result<Self> {
        loop {
            let dir = parent.join(temp_name("reveal-")?);
            match fs::create_dir(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Move `staged` files into `output_dir` under `names`, numbering around anything already
    /// there (including files moved earlier in this same commit).
    fn commit(
        self,
        staged: &[PathBuf],
        names: &[String],
        output_dir: &Path,
    ) -> Result<Vec<PathBuf>> {
        let mut out = Vec::with_capacity(staged.len());
        for (path, name) in staged.iter().zip(names) {
            let name = unique_name(name, |candidate| {
                output_dir.join(candidate).symlink_metadata().is_ok()
            })?;
            let dest = output_dir.join(name);
            fs::rename(path, &dest)?;
            out.push(dest);
        }
        Ok(out)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
