//! C ABI for [`zolal_core`], the layer Swift calls into.
//!
//! Thin by design: translate types, catch panics, nothing else. All logic belongs in
//! `zolal-core` so it stays testable on desktop without a simulator.
//!
//! The ABI is hand-written rather than generated. The surface is five operations (`probe`,
//! `plausibility`, `hide`, `reveal`, `clean`), the generators in this space are pre-1.0, and a
//! stable C header is something any product can review and ship without re-auditing a code
//! generator.
//!
//! ## The contract
//!
//! - **Paths and passphrases cross as NUL-terminated bytes**, never as byte arrays of file
//!   content: a 4 GB video must not be copied into RAM, let alone marshalled.
//! - **Every call is synchronous and blocking.** Swift runs it off the main thread.
//! - **Progress is polled, not pushed.** [`zolal_progress_new`] hands back a handle whose
//!   counters Swift reads on a timer, and whose `cancel` the UI can set. Pushing a callback
//!   thousands of times a second would be worse for SwiftUI and would mean calling a Swift
//!   closure from a Rust thread.
//! - **Errors are a code plus an optional detail struct**, so the UX contract in
//!   [`zolal_core::error`] survives the crossing: "wrong passphrase", "nothing hidden (and the
//!   file looks re-encoded)" and "damaged" stay distinct. (Read that module's docs before
//!   showing the difference to a user.)
//! - **Nothing panics across the boundary.** Every entry point is wrapped in
//!   [`catch_unwind`]; a panic becomes [`ZOLAL_ERR_INTERNAL`] instead of aborting the app. That
//!   is why the release profile unwinds rather than aborts.
//!
//! ## Memory ownership
//!
//! Anything this library hands back is freed by this library: [`zolal_error_free`],
//! [`zolal_reveal_report_free`], [`zolal_progress_free`]. Strings borrowed from a report stay
//! valid until that report is freed.
//!
//! The header in `include/zolal.h` is the canonical description for Swift; the Zolal app's
//! `ZolalKit` wraps it in a Swift API.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs, clippy::all)]

use std::ffi::{c_char, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr;

use secrecy::SecretString;
use zolal_core::progress::AtomicProgress;
use zolal_core::{
    clean, hide, plausibility, probe, reveal, CarrierFormat, CleanRequest, HideRequest,
    RevealRequest, Technique, Verdict, ZolalError,
};

// --- status codes -----------------------------------------------------------------------------

/// The call succeeded.
pub const ZOLAL_OK: i32 = 0;
/// A region that could hold a payload exists, but the passphrase did not authenticate it.
pub const ZOLAL_ERR_WRONG_PASSPHRASE: i32 = 1;
/// No hidden payload at all. `ZolalError::looks_reencoded` says whether the carrier bears the
/// marks of having been re-encoded in transit.
pub const ZOLAL_ERR_NO_HIDDEN_DATA: i32 = 2;
/// The passphrase was right but the payload is damaged or truncated. Nothing was written.
pub const ZOLAL_ERR_DAMAGED_PAYLOAD: i32 = 3;
/// Not a JPEG, MP4 or PDF. The message names what was detected (e.g. `"HEIC"`), which usually
/// means the import pipeline failed to normalise it.
pub const ZOLAL_ERR_UNSUPPORTED_FORMAT: i32 = 4;
/// The result would exceed the carrier format's size limit. `size`, `limit` and `suggestion`
/// are filled in; suggest the `suggestion` format instead.
pub const ZOLAL_ERR_PAYLOAD_TOO_LARGE: i32 = 5;
/// The carrier is too small or truncated to be a valid file of its type.
pub const ZOLAL_ERR_CARRIER_TOO_SMALL: i32 = 6;
/// The container is structurally invalid.
pub const ZOLAL_ERR_MALFORMED_CONTAINER: i32 = 7;
/// The payload authenticated but its framing is invalid: a version mismatch or a bug.
pub const ZOLAL_ERR_CORRUPT_BUNDLE: i32 = 8;
/// The request itself was impossible: no payloads, a technique that does not fit, and so on.
pub const ZOLAL_ERR_INVALID_REQUEST: i32 = 9;
/// Cancelled through the progress handle. Nothing was left behind.
pub const ZOLAL_ERR_CANCELLED: i32 = 10;
/// A filesystem error.
pub const ZOLAL_ERR_IO: i32 = 11;
/// A null pointer, invalid UTF-8 where text was required, or a panic. Always a caller bug or
/// ours, never the user's.
pub const ZOLAL_ERR_INTERNAL: i32 = 12;

/// JPEG carrier. Matches [`CarrierFormat::Jpeg`].
pub const ZOLAL_FORMAT_JPEG: u8 = 0;
/// MP4 carrier. Matches [`CarrierFormat::Mp4`].
pub const ZOLAL_FORMAT_MP4: u8 = 1;
/// PDF carrier. Matches [`CarrierFormat::Pdf`].
pub const ZOLAL_FORMAT_PDF: u8 = 2;
/// MP3 carrier. Matches [`CarrierFormat::Mp3`].
pub const ZOLAL_FORMAT_MP3: u8 = 3;

/// Let the engine pick the default technique for the detected format.
pub const ZOLAL_TECHNIQUE_AUTO: u8 = 0;
/// Append after the JPEG end-of-image marker (the JPEG default).
pub const ZOLAL_TECHNIQUE_JPEG_TRAILER: u8 = 1;
/// Store across JPEG APP15 metadata segments.
pub const ZOLAL_TECHNIQUE_JPEG_APP15: u8 = 2;
/// Append an MP4 `free` box (the MP4 default).
pub const ZOLAL_TECHNIQUE_MP4_FREE_BOX: u8 = 3;
/// Append an unreferenced PDF stream object (the PDF default).
pub const ZOLAL_TECHNIQUE_PDF_OBJECT: u8 = 4;
/// Add a `PRIV` frame to the MP3's ID3v2 tag (the MP3 default).
pub const ZOLAL_TECHNIQUE_MP3_ID3: u8 = 5;

/// Growth is unremarkable for this kind of file.
pub const ZOLAL_VERDICT_NATURAL: u8 = 0;
/// Noticeably larger than expected, but not absurd.
pub const ZOLAL_VERDICT_NOTICEABLE: u8 = 1;
/// So large it invites the question; suggest a bigger carrier.
pub const ZOLAL_VERDICT_SUSPICIOUS: u8 = 2;
/// Over the hard limit: hiding will be refused. Suggest a video carrier.
pub const ZOLAL_VERDICT_TOO_LARGE: u8 = 3;

// --- shared types -----------------------------------------------------------------------------

/// Detail for a failed call. Zeroed unless the code says otherwise; free with
/// [`zolal_error_free`].
#[repr(C)]
pub struct ZolalErrorDetail {
    /// A human-readable message, or null. Owned by this struct.
    pub message: *mut c_char,
    /// For [`ZOLAL_ERR_NO_HIDDEN_DATA`]: true when the carrier looks re-encoded, which means
    /// telling the user to ask for the file again, sent as a file rather than as a photo.
    pub looks_reencoded: bool,
    /// For [`ZOLAL_ERR_PAYLOAD_TOO_LARGE`]: what the output would have weighed.
    pub size: u64,
    /// For [`ZOLAL_ERR_PAYLOAD_TOO_LARGE`]: the limit it exceeded.
    pub limit: u64,
    /// For [`ZOLAL_ERR_PAYLOAD_TOO_LARGE`]: the carrier format to suggest instead.
    pub suggestion: u8,
}

impl Default for ZolalErrorDetail {
    fn default() -> Self {
        Self {
            message: ptr::null_mut(),
            looks_reencoded: false,
            size: 0,
            limit: 0,
            suggestion: ZOLAL_FORMAT_MP4,
        }
    }
}

/// What [`zolal_probe`] learned about a file.
#[repr(C)]
#[derive(Default)]
pub struct ZolalProbe {
    /// One of the `ZOLAL_FORMAT_*` values.
    pub format: u8,
    /// Size on disk.
    pub file_size: u64,
    /// Whether a region exists that *could* hold hidden data. Never proof: only a passphrase
    /// can confirm.
    pub has_candidate_region: bool,
}

/// What [`zolal_plausibility`] predicts, and what [`zolal_hide`] achieved.
#[repr(C)]
#[derive(Default)]
pub struct ZolalPlausibility {
    /// Predicted (or final) output size.
    pub result_size: u64,
    /// Output size divided by the original carrier size.
    pub ratio: f32,
    /// One of the `ZOLAL_VERDICT_*` values.
    pub verdict: u8,
}

/// What [`zolal_hide`] wrote.
#[repr(C)]
#[derive(Default)]
pub struct ZolalHideReport {
    /// Final size of the written carrier.
    pub output_size: u64,
    /// The technique actually used, resolving [`ZOLAL_TECHNIQUE_AUTO`].
    pub technique: u8,
    /// How natural the result looks.
    pub plausibility: ZolalPlausibility,
}

/// What [`zolal_clean`] wrote.
#[repr(C)]
#[derive(Default)]
pub struct ZolalCleanReport {
    /// Size of the cleaned file.
    pub output_size: u64,
    /// How many bytes the carrier shed.
    pub removed_bytes: u64,
    /// One of the `ZOLAL_TECHNIQUE_*` values: where the payload had been.
    pub technique: u8,
}

/// Opaque handle to the recovered file list. Free with [`zolal_reveal_report_free`].
pub struct ZolalRevealReport {
    files: Vec<CString>,
    total_bytes: u64,
    technique: u8,
}

/// Opaque progress and cancellation handle. Create with [`zolal_progress_new`], free with
/// [`zolal_progress_free`].
///
/// Safe to read from one thread while the operation runs on another: the counters are atomics.
pub struct ZolalProgress(AtomicProgress);

// --- entry points -----------------------------------------------------------------------------

/// Identify a carrier and report whether it could hold (or already holds) a payload.
///
/// # Safety
/// `path` must be a valid NUL-terminated string. `out` must point to writable storage for one
/// [`ZolalProbe`]. `err`, when not null, must point to writable storage for one
/// [`ZolalErrorDetail`].
#[no_mangle]
pub unsafe extern "C" fn zolal_probe(
    path: *const c_char,
    out: *mut ZolalProbe,
    err: *mut ZolalErrorDetail,
) -> i32 {
    guard(err, || {
        let path = unsafe { to_path(path) }?;
        let out = unsafe { as_mut(out) }?;
        let info = probe(&path)?;
        *out = ZolalProbe {
            format: format_code(info.format),
            file_size: info.file_size,
            has_candidate_region: info.has_candidate_region,
        };
        Ok(())
    })
}

/// Predict how natural the output will look, before committing to anything.
///
/// A [`ZOLAL_VERDICT_TOO_LARGE`] answer means [`zolal_hide`] will refuse, so the UI can suggest
/// a video carrier first. Cheap enough to call whenever the selection changes.
///
/// # Safety
/// As [`zolal_probe`], with `out` pointing to one [`ZolalPlausibility`].
#[no_mangle]
pub unsafe extern "C" fn zolal_plausibility(
    carrier: *const c_char,
    payload_bytes: u64,
    out: *mut ZolalPlausibility,
    err: *mut ZolalErrorDetail,
) -> i32 {
    guard(err, || {
        let carrier = unsafe { to_path(carrier) }?;
        let out = unsafe { as_mut(out) }?;
        *out = plausibility_out(&plausibility(&carrier, payload_bytes)?);
        Ok(())
    })
}

/// Hide `payloads` inside `carrier`, writing the result to `output`.
///
/// Blocks until done. `progress` may be null; when it is not, its counters track plaintext
/// bytes sealed and cancelling it aborts with [`ZOLAL_ERR_CANCELLED`], leaving nothing behind.
///
/// The passphrase must already be Unicode NFC (Swift: `precomposedStringWithCanonicalMapping`),
/// or the same phrase typed on another device will derive a different key.
///
/// # Safety
/// `carrier`, `output` and `passphrase` must be valid NUL-terminated strings. `payloads` must
/// point to `payload_count` valid NUL-terminated strings. `progress`, when not null, must come
/// from [`zolal_progress_new`] and must outlive the call. `out` must point to one
/// [`ZolalHideReport`].
#[no_mangle]
pub unsafe extern "C" fn zolal_hide(
    carrier: *const c_char,
    payloads: *const *const c_char,
    payload_count: usize,
    output: *const c_char,
    passphrase: *const c_char,
    technique: u8,
    progress: *const ZolalProgress,
    out: *mut ZolalHideReport,
    err: *mut ZolalErrorDetail,
) -> i32 {
    guard(err, || {
        let carrier = unsafe { to_path(carrier) }?;
        let output = unsafe { to_path(output) }?;
        let passphrase = unsafe { to_secret(passphrase) }?;
        let out = unsafe { as_mut(out) }?;
        if payloads.is_null() && payload_count != 0 {
            return Err(internal("payload list is null"));
        }
        let mut files = Vec::with_capacity(payload_count);
        for i in 0..payload_count {
            // SAFETY: the caller promised `payload_count` valid entries.
            files.push(unsafe { to_path(*payloads.add(i)) }?);
        }

        let report = hide(
            HideRequest {
                carrier,
                payloads: files,
                output,
                passphrase,
                technique: technique_from(technique)?,
                kdf_params: None,
            },
            unsafe { progress_ref(progress) },
        )?;
        *out = ZolalHideReport {
            output_size: report.output_size,
            technique: technique_code(report.technique),
            plausibility: plausibility_out(&report.plausibility),
        };
        Ok(())
    })
}

/// Write `carrier` back out to `output` with its hidden content removed.
///
/// The passphrase must be the one the content was hidden with: the format is markerless, so the
/// only way to know which region is ours — rather than one another tool left — is to find the one
/// that authenticates. Nothing beyond the first chunk is decrypted.
///
/// Returns `ZOLAL_ERR_NO_HIDDEN_DATA` when the carrier holds no plausible region and
/// `ZOLAL_ERR_WRONG_PASSPHRASE` when none authenticates. On any failure nothing is written.
///
/// # Safety
/// As [`zolal_hide`]; `out` must point to one `ZolalCleanReport`.
#[no_mangle]
pub unsafe extern "C" fn zolal_clean(
    carrier: *const c_char,
    output: *const c_char,
    passphrase: *const c_char,
    progress: *const ZolalProgress,
    out: *mut ZolalCleanReport,
    err: *mut ZolalErrorDetail,
) -> i32 {
    guard(err, || {
        let carrier = unsafe { to_path(carrier) }?;
        let output = unsafe { to_path(output) }?;
        let passphrase = unsafe { to_secret(passphrase) }?;
        let out = unsafe { as_mut(out) }?;

        let report = clean(
            CleanRequest {
                carrier,
                output,
                passphrase,
                kdf_params: None,
            },
            unsafe { progress_ref(progress) },
        )?;
        *out = ZolalCleanReport {
            output_size: report.output_size,
            removed_bytes: report.removed_bytes,
            technique: technique_code(report.technique),
        };
        Ok(())
    })
}

/// Recover hidden files from `carrier` into `output_dir`.
///
/// On success `out` receives a report to read with the `zolal_reveal_report_*` calls and free
/// with [`zolal_reveal_report_free`]. On any failure nothing is left in `output_dir`.
///
/// # Safety
/// As [`zolal_hide`]; `out` must point to one `*mut ZolalRevealReport`.
#[no_mangle]
pub unsafe extern "C" fn zolal_reveal(
    carrier: *const c_char,
    output_dir: *const c_char,
    passphrase: *const c_char,
    progress: *const ZolalProgress,
    out: *mut *mut ZolalRevealReport,
    err: *mut ZolalErrorDetail,
) -> i32 {
    guard(err, || {
        let carrier = unsafe { to_path(carrier) }?;
        let output_dir = unsafe { to_path(output_dir) }?;
        let passphrase = unsafe { to_secret(passphrase) }?;
        let slot = unsafe { as_mut(out) }?;

        let report = reveal(
            RevealRequest {
                carrier,
                output_dir,
                passphrase,
                kdf_params: None,
            },
            unsafe { progress_ref(progress) },
        )?;
        let files = report
            .files
            .iter()
            .map(|path| CString::new(path.as_os_str().as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| internal("a recovered path contains a NUL byte"))?;
        *slot = Box::into_raw(Box::new(ZolalRevealReport {
            files,
            total_bytes: report.total_bytes,
            technique: technique_code(report.technique),
        }));
        Ok(())
    })
}

// --- reveal report accessors ------------------------------------------------------------------

/// How many files were recovered.
///
/// # Safety
/// `report` must come from a successful [`zolal_reveal`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn zolal_reveal_report_file_count(report: *const ZolalRevealReport) -> usize {
    match unsafe { report.as_ref() } {
        Some(report) => report.files.len(),
        None => 0,
    }
}

/// Path of recovered file `index`, or null if out of range. Valid until the report is freed.
///
/// # Safety
/// As [`zolal_reveal_report_file_count`].
#[no_mangle]
pub unsafe extern "C" fn zolal_reveal_report_file(
    report: *const ZolalRevealReport,
    index: usize,
) -> *const c_char {
    match unsafe { report.as_ref() }.and_then(|report| report.files.get(index)) {
        Some(path) => path.as_ptr(),
        None => ptr::null(),
    }
}

/// Total plaintext bytes recovered.
///
/// # Safety
/// As [`zolal_reveal_report_file_count`].
#[no_mangle]
pub unsafe extern "C" fn zolal_reveal_report_total_bytes(report: *const ZolalRevealReport) -> u64 {
    unsafe { report.as_ref() }.map_or(0, |report| report.total_bytes)
}

/// Which technique the payload was found under (a `ZOLAL_TECHNIQUE_*` value).
///
/// # Safety
/// As [`zolal_reveal_report_file_count`].
#[no_mangle]
pub unsafe extern "C" fn zolal_reveal_report_technique(report: *const ZolalRevealReport) -> u8 {
    unsafe { report.as_ref() }.map_or(ZOLAL_TECHNIQUE_AUTO, |report| report.technique)
}

/// Release a reveal report. Null is ignored; double-freeing is not allowed.
///
/// # Safety
/// `report` must come from [`zolal_reveal`] and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn zolal_reveal_report_free(report: *mut ZolalRevealReport) {
    if !report.is_null() {
        drop(unsafe { Box::from_raw(report) });
    }
}

// --- progress ---------------------------------------------------------------------------------

/// Create a progress handle. Free with [`zolal_progress_free`].
#[no_mangle]
pub extern "C" fn zolal_progress_new() -> *mut ZolalProgress {
    Box::into_raw(Box::new(ZolalProgress(AtomicProgress::new())))
}

/// Read the latest `(done, total)`. `total` is 0 before the size is known, so treat the result
/// as indeterminate until it is non-zero. Either out-pointer may be null.
///
/// # Safety
/// `handle` must come from [`zolal_progress_new`] and not yet be freed. `done` and `total`, when
/// not null, must point to writable `uint64_t`s.
#[no_mangle]
pub unsafe extern "C" fn zolal_progress_read(
    handle: *const ZolalProgress,
    done: *mut u64,
    total: *mut u64,
) {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let (d, t) = handle.0.snapshot();
    if !done.is_null() {
        unsafe { *done = d };
    }
    if !total.is_null() {
        unsafe { *total = t };
    }
}

/// Ask the running operation to stop. It aborts at the next chunk boundary with
/// [`ZOLAL_ERR_CANCELLED`], leaving no output file and no partial plaintext.
///
/// # Safety
/// As [`zolal_progress_read`].
#[no_mangle]
pub unsafe extern "C" fn zolal_progress_cancel(handle: *const ZolalProgress) {
    if let Some(handle) = unsafe { handle.as_ref() } {
        handle.0.cancel();
    }
}

/// Release a progress handle. Must not be called while an operation using it is still running.
///
/// # Safety
/// `handle` must come from [`zolal_progress_new`] and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn zolal_progress_free(handle: *mut ZolalProgress) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// Release the message inside an error detail. Safe to call on a zeroed struct.
///
/// # Safety
/// `err` must point to a [`ZolalErrorDetail`] this library filled in, and its message must not
/// be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn zolal_error_free(err: *mut ZolalErrorDetail) {
    let Some(err) = (unsafe { err.as_mut() }) else {
        return;
    };
    if !err.message.is_null() {
        drop(unsafe { CString::from_raw(err.message) });
        err.message = ptr::null_mut();
    }
}

// --- plumbing ---------------------------------------------------------------------------------

/// Run `body`, turning any error — including a panic — into a status code and filling `err`.
/// Why a call failed: something the engine reported, or a caller bug on this boundary (a null
/// pointer, text that is not UTF-8, a panic). The two are kept apart so a mistake in the Swift
/// layer never looks to the user like a disk problem.
enum Failure {
    Core(ZolalError),
    Internal(String),
}

impl From<ZolalError> for Failure {
    fn from(error: ZolalError) -> Self {
        Failure::Core(error)
    }
}

fn guard(
    err: *mut ZolalErrorDetail,
    body: impl FnOnce() -> std::result::Result<(), Failure>,
) -> i32 {
    // Start from a clean slate so a caller reusing one struct can't read a stale message.
    if let Some(slot) = unsafe { err.as_mut() } {
        *slot = ZolalErrorDetail::default();
    }
    let outcome = catch_unwind(AssertUnwindSafe(body));
    let failure = match outcome {
        Ok(Ok(())) => return ZOLAL_OK,
        Ok(Err(failure)) => failure,
        // A panic must never unwind into Swift: that is undefined behaviour.
        Err(_) => Failure::Internal("internal error".into()),
    };
    let (code, detail) = describe(failure);
    if let Some(slot) = unsafe { err.as_mut() } {
        *slot = detail;
    } else if !detail.message.is_null() {
        // Nobody asked for details, so don't leak the message.
        drop(unsafe { CString::from_raw(detail.message) });
    }
    code
}

/// Map a failure onto its status code and detail.
fn describe(failure: Failure) -> (i32, ZolalErrorDetail) {
    let message = |text: String| match CString::new(text) {
        Ok(text) => text.into_raw(),
        Err(_) => ptr::null_mut(),
    };
    let error = match failure {
        Failure::Core(error) => error,
        Failure::Internal(text) => {
            return (
                ZOLAL_ERR_INTERNAL,
                ZolalErrorDetail {
                    message: message(text),
                    ..ZolalErrorDetail::default()
                },
            )
        }
    };
    let text = error.to_string();
    match error {
        ZolalError::WrongPassphrase => (ZOLAL_ERR_WRONG_PASSPHRASE, ZolalErrorDetail::default()),
        ZolalError::NoHiddenData {
            carrier_looks_reencoded,
        } => (
            ZOLAL_ERR_NO_HIDDEN_DATA,
            ZolalErrorDetail {
                looks_reencoded: carrier_looks_reencoded,
                ..ZolalErrorDetail::default()
            },
        ),
        ZolalError::DamagedPayload => (ZOLAL_ERR_DAMAGED_PAYLOAD, ZolalErrorDetail::default()),
        ZolalError::PayloadTooLarge {
            output_size,
            limit,
            suggestion,
        } => (
            ZOLAL_ERR_PAYLOAD_TOO_LARGE,
            ZolalErrorDetail {
                message: message(text),
                size: output_size,
                limit,
                suggestion: format_code(suggestion),
                ..ZolalErrorDetail::default()
            },
        ),
        ZolalError::UnsupportedFormat { detected } => (
            ZOLAL_ERR_UNSUPPORTED_FORMAT,
            ZolalErrorDetail {
                message: message(detected),
                ..ZolalErrorDetail::default()
            },
        ),
        ZolalError::CarrierTooSmall => (ZOLAL_ERR_CARRIER_TOO_SMALL, ZolalErrorDetail::default()),
        ZolalError::Cancelled => (ZOLAL_ERR_CANCELLED, ZolalErrorDetail::default()),
        other => {
            let code = match other {
                ZolalError::MalformedContainer { .. } => ZOLAL_ERR_MALFORMED_CONTAINER,
                ZolalError::CorruptBundle(_) => ZOLAL_ERR_CORRUPT_BUNDLE,
                ZolalError::InvalidRequest(_) => ZOLAL_ERR_INVALID_REQUEST,
                ZolalError::Io(_) => ZOLAL_ERR_IO,
                _ => ZOLAL_ERR_INTERNAL,
            };
            (
                code,
                ZolalErrorDetail {
                    message: message(text),
                    ..ZolalErrorDetail::default()
                },
            )
        }
    }
}

fn internal(what: &str) -> Failure {
    Failure::Internal(what.to_string())
}

/// Borrow a C string as a path. Paths cross as raw bytes, not UTF-8: Apple filesystems allow
/// names that are not valid UTF-8, and a photo library is exactly where they turn up.
///
/// # Safety
/// `raw` must be null or a valid NUL-terminated string.
unsafe fn to_path(raw: *const c_char) -> std::result::Result<PathBuf, Failure> {
    if raw.is_null() {
        return Err(internal("null path"));
    }
    let bytes = unsafe { CStr::from_ptr(raw) }.to_bytes();
    Ok(Path::new(std::ffi::OsStr::from_bytes(bytes)).to_path_buf())
}

/// Copy a C string into a [`SecretString`], which wipes itself on drop. The caller's own buffer
/// remains the caller's responsibility.
///
/// # Safety
/// As [`to_path`].
unsafe fn to_secret(raw: *const c_char) -> std::result::Result<SecretString, Failure> {
    if raw.is_null() {
        return Err(internal("null passphrase"));
    }
    let text = unsafe { CStr::from_ptr(raw) }
        .to_str()
        .map_err(|_| internal("passphrase is not valid UTF-8"))?;
    Ok(SecretString::from(text))
}

/// # Safety
/// `raw` must be null or point to writable storage for one `T`.
unsafe fn as_mut<'a, T>(raw: *mut T) -> std::result::Result<&'a mut T, Failure> {
    unsafe { raw.as_mut() }.ok_or_else(|| internal("null out-parameter"))
}

/// # Safety
/// `raw` must be null or come from [`zolal_progress_new`] and outlive the call.
unsafe fn progress_ref<'a>(raw: *const ZolalProgress) -> &'a dyn zolal_core::Progress {
    match unsafe { raw.as_ref() } {
        Some(handle) => &handle.0,
        None => &zolal_core::progress::NoProgress,
    }
}

fn plausibility_out(value: &zolal_core::Plausibility) -> ZolalPlausibility {
    ZolalPlausibility {
        result_size: value.result_size,
        ratio: value.ratio,
        verdict: match value.verdict {
            Verdict::Natural => ZOLAL_VERDICT_NATURAL,
            Verdict::Noticeable => ZOLAL_VERDICT_NOTICEABLE,
            Verdict::Suspicious => ZOLAL_VERDICT_SUSPICIOUS,
            Verdict::TooLarge => ZOLAL_VERDICT_TOO_LARGE,
        },
    }
}

fn format_code(format: CarrierFormat) -> u8 {
    match format {
        CarrierFormat::Jpeg => ZOLAL_FORMAT_JPEG,
        CarrierFormat::Mp4 => ZOLAL_FORMAT_MP4,
        CarrierFormat::Pdf => ZOLAL_FORMAT_PDF,
        CarrierFormat::Mp3 => ZOLAL_FORMAT_MP3,
    }
}

fn technique_code(technique: Technique) -> u8 {
    match technique {
        Technique::Auto => ZOLAL_TECHNIQUE_AUTO,
        Technique::JpegTrailer => ZOLAL_TECHNIQUE_JPEG_TRAILER,
        Technique::JpegApp15 => ZOLAL_TECHNIQUE_JPEG_APP15,
        Technique::Mp4FreeBox => ZOLAL_TECHNIQUE_MP4_FREE_BOX,
        Technique::PdfObject => ZOLAL_TECHNIQUE_PDF_OBJECT,
        Technique::Mp3Id3 => ZOLAL_TECHNIQUE_MP3_ID3,
    }
}

fn technique_from(code: u8) -> std::result::Result<Technique, Failure> {
    Ok(match code {
        ZOLAL_TECHNIQUE_AUTO => Technique::Auto,
        ZOLAL_TECHNIQUE_JPEG_TRAILER => Technique::JpegTrailer,
        ZOLAL_TECHNIQUE_JPEG_APP15 => Technique::JpegApp15,
        ZOLAL_TECHNIQUE_MP4_FREE_BOX => Technique::Mp4FreeBox,
        ZOLAL_TECHNIQUE_PDF_OBJECT => Technique::PdfObject,
        ZOLAL_TECHNIQUE_MP3_ID3 => Technique::Mp3Id3,
        other => {
            return Err(Failure::Core(ZolalError::InvalidRequest(format!(
                "unknown technique code {other}"
            ))))
        }
    })
}
