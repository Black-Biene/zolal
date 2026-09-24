//! Exercises the C ABI the way Swift will: raw pointers, status codes, manual frees.
//!
//! The engine itself is covered in `zolal-core`; what is proved here is the crossing — that
//! status codes and error details survive it, that reports can be read and freed, and that
//! cancellation works through the handle.

use std::ffi::{c_char, CStr, CString};
use std::fs;
use std::path::Path;
use std::ptr;

use zolal_ffi::*;

/// A minimal but structurally valid MP4: an `ftyp` box and an `mdat` box.
fn synth_mp4() -> Vec<u8> {
    let mut out = Vec::new();
    let ftyp = b"isom\0\0\x02\0isomiso2";
    out.extend_from_slice(&((8 + ftyp.len()) as u32).to_be_bytes());
    out.extend_from_slice(b"ftyp");
    out.extend_from_slice(ftyp);
    let media = vec![0x11u8; 400];
    out.extend_from_slice(&((8 + media.len()) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&media);
    out
}

fn c(path: &Path) -> CString {
    CString::new(path.to_str().unwrap()).unwrap()
}

/// Hide `payload` in `carrier`, returning the status code.
fn hide_one(carrier: &Path, payload: &Path, output: &Path, pass: &str) -> (i32, ZolalHideReport) {
    let payloads = [c(payload)];
    let pointers: Vec<*const c_char> = payloads.iter().map(|p| p.as_ptr()).collect();
    let pass = CString::new(pass).unwrap();
    let mut report = ZolalHideReport::default();
    let code = unsafe {
        zolal_hide(
            c(carrier).as_ptr(),
            pointers.as_ptr(),
            pointers.len(),
            c(output).as_ptr(),
            pass.as_ptr(),
            ZOLAL_TECHNIQUE_AUTO,
            ptr::null(),
            &mut report,
            ptr::null_mut(),
        )
    };
    (code, report)
}

#[test]
fn hide_and_reveal_through_the_abi() {
    let dir = tempfile::tempdir().unwrap();
    let carrier = dir.path().join("carrier.mp4");
    fs::write(&carrier, synth_mp4()).unwrap();
    let secret = dir.path().join("secret.txt");
    fs::write(&secret, b"meet at noon").unwrap();
    let stego = dir.path().join("stego.mp4");

    // probe: nothing hidden yet
    let mut info = ZolalProbe::default();
    let code = unsafe { zolal_probe(c(&carrier).as_ptr(), &mut info, ptr::null_mut()) };
    assert_eq!(code, ZOLAL_OK);
    assert_eq!(info.format, ZOLAL_FORMAT_MP4);
    assert!(!info.has_candidate_region);

    // plausibility
    let mut plausibility = ZolalPlausibility::default();
    let code = unsafe {
        zolal_plausibility(
            c(&carrier).as_ptr(),
            100,
            &mut plausibility,
            ptr::null_mut(),
        )
    };
    assert_eq!(code, ZOLAL_OK);
    assert!(plausibility.result_size > 0);

    // hide
    let (code, report) = hide_one(&carrier, &secret, &stego, "correct horse");
    assert_eq!(code, ZOLAL_OK);
    assert_eq!(report.technique, ZOLAL_TECHNIQUE_MP4_FREE_BOX);
    assert_eq!(report.output_size, fs::metadata(&stego).unwrap().len());

    // reveal
    let out_dir = dir.path().join("out");
    let pass = CString::new("correct horse").unwrap();
    let mut recovered: *mut ZolalRevealReport = ptr::null_mut();
    let code = unsafe {
        zolal_reveal(
            c(&stego).as_ptr(),
            c(&out_dir).as_ptr(),
            pass.as_ptr(),
            ptr::null(),
            &mut recovered,
            ptr::null_mut(),
        )
    };
    assert_eq!(code, ZOLAL_OK);
    assert!(!recovered.is_null());

    unsafe {
        assert_eq!(zolal_reveal_report_file_count(recovered), 1);
        assert_eq!(zolal_reveal_report_total_bytes(recovered), 12);
        assert_eq!(
            zolal_reveal_report_technique(recovered),
            ZOLAL_TECHNIQUE_MP4_FREE_BOX
        );
        let path = zolal_reveal_report_file(recovered, 0);
        assert!(!path.is_null());
        let path = CStr::from_ptr(path).to_str().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"meet at noon");
        assert!(
            zolal_reveal_report_file(recovered, 1).is_null(),
            "out of range is null"
        );
        zolal_reveal_report_free(recovered);
    }
}

#[test]
fn the_ux_error_contract_survives_the_crossing() {
    let dir = tempfile::tempdir().unwrap();
    let carrier = dir.path().join("carrier.mp4");
    fs::write(&carrier, synth_mp4()).unwrap();
    // Big enough to span several 64 KiB chunks: within a single chunk, damage and a wrong key
    // are genuinely indistinguishable, so only a multi-chunk payload can tell them apart.
    let secret = dir.path().join("s.bin");
    fs::write(&secret, vec![9u8; 200 * 1024]).unwrap();
    let stego = dir.path().join("stego.mp4");
    assert_eq!(hide_one(&carrier, &secret, &stego, "right").0, ZOLAL_OK);

    let reveal = |file: &Path, pass: &str| {
        let pass = CString::new(pass).unwrap();
        let mut out: *mut ZolalRevealReport = ptr::null_mut();
        let mut err = ZolalErrorDetail::default();
        let code = unsafe {
            zolal_reveal(
                c(file).as_ptr(),
                c(&dir.path().join("out")).as_ptr(),
                pass.as_ptr(),
                ptr::null(),
                &mut out,
                &mut err,
            )
        };
        (code, err)
    };

    // Wrong key on a real payload, versus nothing hidden at all: still distinct.
    let (code, mut err) = reveal(&stego, "wrong");
    assert_eq!(code, ZOLAL_ERR_WRONG_PASSPHRASE);
    unsafe { zolal_error_free(&mut err) };

    let (code, mut err) = reveal(&carrier, "right");
    assert_eq!(code, ZOLAL_ERR_NO_HIDDEN_DATA);
    assert!(
        !err.looks_reencoded,
        "an MP4 carries no re-encoding heuristic"
    );
    unsafe { zolal_error_free(&mut err) };

    // A damaged payload is its own answer, not "wrong passphrase". Corrupt bytes in place:
    // truncating an MP4 instead breaks the box header, which the container catches first.
    let mut corrupt = fs::read(&stego).unwrap();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0x01;
    let damaged = dir.path().join("damaged.mp4");
    fs::write(&damaged, &corrupt).unwrap();
    let (code, mut err) = reveal(&damaged, "right");
    assert_eq!(code, ZOLAL_ERR_DAMAGED_PAYLOAD);
    unsafe { zolal_error_free(&mut err) };

    // Truncation is caught by the container itself, which is a clearer answer still.
    let mut cut = fs::read(&stego).unwrap();
    cut.truncate(cut.len() - 1);
    let truncated = dir.path().join("truncated.mp4");
    fs::write(&truncated, &cut).unwrap();
    let (code, mut err) = reveal(&truncated, "right");
    assert_eq!(code, ZOLAL_ERR_MALFORMED_CONTAINER);
    unsafe { zolal_error_free(&mut err) };

    // An unconverted HEIC names itself, so the Swift import bug is obvious.
    let heic = dir.path().join("photo.heic");
    fs::write(&heic, b"\0\0\0\x18ftypheic\0\0\0\0mif1heic").unwrap();
    let mut err = ZolalErrorDetail::default();
    let mut info = ZolalProbe::default();
    let code = unsafe { zolal_probe(c(&heic).as_ptr(), &mut info, &mut err) };
    assert_eq!(code, ZOLAL_ERR_UNSUPPORTED_FORMAT);
    let message = unsafe { CStr::from_ptr(err.message) }.to_str().unwrap();
    assert!(message.starts_with("HEIC"), "{message}");
    unsafe { zolal_error_free(&mut err) };
    assert!(err.message.is_null(), "freeing clears the pointer");
}

#[test]
fn cancelling_through_the_handle_stops_the_work() {
    let dir = tempfile::tempdir().unwrap();
    let carrier = dir.path().join("carrier.mp4");
    fs::write(&carrier, synth_mp4()).unwrap();
    let payload = dir.path().join("big.bin");
    fs::write(&payload, vec![7u8; 512 * 1024]).unwrap();
    let output = dir.path().join("never.mp4");

    let handle = zolal_progress_new();
    unsafe { zolal_progress_cancel(handle) };

    let payloads = [c(&payload)];
    let pointers: Vec<*const c_char> = payloads.iter().map(|p| p.as_ptr()).collect();
    let pass = CString::new("pw").unwrap();
    let mut report = ZolalHideReport::default();
    let code = unsafe {
        zolal_hide(
            c(&carrier).as_ptr(),
            pointers.as_ptr(),
            pointers.len(),
            c(&output).as_ptr(),
            pass.as_ptr(),
            ZOLAL_TECHNIQUE_AUTO,
            handle,
            &mut report,
            ptr::null_mut(),
        )
    };
    assert_eq!(code, ZOLAL_ERR_CANCELLED);
    assert!(!output.exists(), "a cancelled hide leaves nothing behind");

    let (mut done, mut total) = (1u64, 1u64);
    unsafe { zolal_progress_read(handle, &mut done, &mut total) };
    assert_eq!(done, 0);
    unsafe { zolal_progress_free(handle) };
}

#[test]
fn null_arguments_are_refused_rather_than_dereferenced() {
    let mut info = ZolalProbe::default();
    assert_eq!(
        unsafe { zolal_probe(ptr::null(), &mut info, ptr::null_mut()) },
        ZOLAL_ERR_INTERNAL
    );
    assert_eq!(
        unsafe {
            zolal_probe(
                c(Path::new("/nope")).as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        ZOLAL_ERR_INTERNAL
    );
    // The accessors tolerate null rather than crashing the app.
    unsafe {
        assert_eq!(zolal_reveal_report_file_count(ptr::null()), 0);
        assert!(zolal_reveal_report_file(ptr::null(), 0).is_null());
        zolal_reveal_report_free(ptr::null_mut());
        zolal_progress_free(ptr::null_mut());
        zolal_progress_cancel(ptr::null());
        zolal_error_free(ptr::null_mut());
    }
}

#[test]
fn a_missing_file_is_an_io_error_with_a_message() {
    let mut info = ZolalProbe::default();
    let mut err = ZolalErrorDetail::default();
    let code = unsafe {
        zolal_probe(
            c(Path::new("/definitely/not/here.jpg")).as_ptr(),
            &mut info,
            &mut err,
        )
    };
    assert_eq!(code, ZOLAL_ERR_IO);
    assert!(!err.message.is_null());
    unsafe { zolal_error_free(&mut err) };
}
