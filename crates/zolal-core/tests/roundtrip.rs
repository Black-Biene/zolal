//! Round-trip tests: hide -> reveal -> assert the payload is byte-identical.
//!
//! These are the tests that prove the design: the crypto envelope and all four carriers
//! (JPEG, MP4, PDF, MP3), end to end, through the public API only.
//!
//! **Fixtures must stay synthetic** — `core/` is the directory that would be published if the
//! project is ever open-sourced, so no real personal media belongs here. Every carrier below is
//! generated in-process, and re-parsed afterwards by an independent implementation
//! (`jpeg-decoder`, `mp4`, `lopdf`) to prove the disguise still opens.
//!
//! Argon2 runs with tiny parameters (`FAST`) everywhere except one test that exercises the
//! production defaults, so the suite stays quick.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use jpeg_encoder::{ColorType, Encoder};
use secrecy::SecretString;
use tempfile::TempDir;
use zolal_core::carrier::jpeg::app15_overhead;
use zolal_core::crypto::{region_len, MIN_REGION_LEN, SEALED_CHUNK_LEN};
use zolal_core::progress::{AtomicProgress, NoProgress};
use zolal_core::{
    clean, hide, plausibility, probe, reveal, CarrierFormat, CleanReport, CleanRequest, HideReport,
    HideRequest, KdfParams, RevealReport, RevealRequest, Technique, Verdict, ZolalError,
    JPEG_MAX_OUTPUT,
};

const FAST: KdfParams = KdfParams {
    memory_kib: 8,
    iterations: 1,
    parallelism: 1,
};
const PASS: &str = "correct horse battery staple";

/// Payload sizes that have historically broken things.
///
/// 64 KiB matters specifically: it is the APP15 segment boundary and the AEAD chunk size, and
/// crossing it is what exposed the APP15 reassembly bug found in early channel testing.
/// Anything at or just past a boundary earns a case. These are
/// raw file sizes; [`boundary_sizes`] adds sizes that put the *envelope* exactly on a boundary.
const SIZES: &[u64] = &[
    0,               // empty payload
    1,               // single byte
    65_533,          // one APP15 segment's worth of payload
    65_534,          // one byte more
    64 * 1024,       // one AEAD chunk's worth of payload
    64 * 1024 + 1,   // one byte more
    5 * 1024 * 1024, // multi-chunk, multi-segment
];

// ---------------------------------------------------------------------------------------------
// Fixtures

/// How to build a synthetic carrier.
#[derive(Clone, Copy, Default)]
struct Style {
    exif: bool,
    progressive: bool,
    restart_interval: Option<u16>,
    /// Append a second complete JPEG after the first, the way MPF stores a gain map.
    chained: bool,
}

/// A real, decodable JPEG with enough texture that its scan data contains stuffed `FF 00`s.
fn synth_jpeg(style: Style) -> Vec<u8> {
    let primary = encode(96, 64, style, 7);
    if style.chained {
        let secondary = encode(48, 32, Style::default(), 99);
        [primary, secondary].concat()
    } else {
        primary
    }
}

fn encode(w: u16, h: u16, style: Style, seed: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 3);
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    for py in 0..h {
        for px in 0..w {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let noise = (x & 0x3F) as u8;
            pixels.extend_from_slice(&[
                (px as u8).wrapping_mul(3).wrapping_add(noise),
                (py as u8).wrapping_mul(4),
                noise.wrapping_mul(4),
            ]);
        }
    }
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, 90);
    enc.set_progressive(style.progressive);
    if let Some(n) = style.restart_interval {
        enc.set_restart_interval(n);
    }
    if style.exif {
        // Minimal big-endian TIFF header with an empty IFD.
        enc.add_exif_metadata(b"MM\0*\0\0\0\x08\0\0\0\0\0\0")
            .unwrap();
    }
    enc.encode(&pixels, w, h, ColorType::Rgb).unwrap();
    out
}

/// Deterministic payload bytes, so a failure reproduces.
fn payload_bytes(len: u64, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();
        path
    }

    fn hide(
        &self,
        carrier: &Path,
        payloads: &[PathBuf],
        output: &str,
        technique: Technique,
    ) -> zolal_core::Result<HideReport> {
        hide(
            HideRequest {
                carrier: carrier.to_path_buf(),
                payloads: payloads.to_vec(),
                output: self.path(output),
                passphrase: SecretString::from(PASS),
                technique,
                kdf_params: Some(FAST),
            },
            &NoProgress,
        )
    }

    fn reveal(&self, carrier: &Path, out: &str, pass: &str) -> zolal_core::Result<RevealReport> {
        reveal(
            RevealRequest {
                carrier: carrier.to_path_buf(),
                output_dir: self.path(out),
                passphrase: SecretString::from(pass),
                kdf_params: Some(FAST),
            },
            &NoProgress,
        )
    }

    fn clean(&self, carrier: &Path, output: &str, pass: &str) -> zolal_core::Result<CleanReport> {
        clean(
            CleanRequest {
                carrier: carrier.to_path_buf(),
                output: self.path(output),
                passphrase: SecretString::from(pass),
                kdf_params: Some(FAST),
            },
            &NoProgress,
        )
    }
}

/// Hide, then clean, and check the carrier is an ordinary file of its format again.
///
/// "Ordinary" is checked two ways on purpose: our own reveal must find nothing, and an
/// independent parser must still open it. A strip that produced a clean-looking but broken file
/// would pass the first check alone.
fn clean_restores(
    carrier_name: &str,
    carrier_bytes: &[u8],
    technique: Technique,
    still_valid: impl Fn(&[u8]),
) {
    let env = Env::new();
    let carrier = env.write(carrier_name, carrier_bytes);
    let payload = env.write("in/secret.bin", &payload_bytes(4096, 7));

    env.hide(&carrier, &[payload], "stego", technique).unwrap();
    let stego = env.path("stego");
    assert!(
        fs::read(&stego).unwrap() != carrier_bytes,
        "hide changed nothing"
    );

    let report = env.clean(&stego, "clean", PASS).unwrap();
    assert_eq!(report.technique, technique);
    assert!(report.removed_bytes > 0, "cleaning removed no bytes");

    let cleaned = fs::read(env.path("clean")).unwrap();
    assert_eq!(report.output_size, cleaned.len() as u64);
    still_valid(&cleaned);

    // The payload must be gone, not merely unreachable.
    let err = env.reveal(&env.path("clean"), "out", PASS).unwrap_err();
    assert!(
        matches!(err, ZolalError::NoHiddenData { .. }),
        "cleaned carrier still offers a region: {err:?}"
    );

    // And the bytes should be the original file back, not merely something payload-free.
    assert_eq!(cleaned, carrier_bytes, "cleaned file is not the original");
}

#[test]
fn cleaning_restores_a_jpeg() {
    let bytes = synth_jpeg(Style {
        exif: true,
        ..Style::default()
    });
    clean_restores("carrier.jpg", &bytes, Technique::JpegTrailer, |b| {
        assert!(b.starts_with(&[0xFF, 0xD8]), "not a JPEG any more");
    });
}

#[test]
fn cleaning_restores_a_jpeg_hidden_in_app15() {
    let bytes = synth_jpeg(Style {
        exif: true,
        ..Style::default()
    });
    clean_restores("carrier.jpg", &bytes, Technique::JpegApp15, |b| {
        assert!(b.starts_with(&[0xFF, 0xD8]), "not a JPEG any more");
    });
}

#[test]
fn cleaning_restores_an_mp4() {
    let bytes = synth_mp4();
    let tracks = mp4_tracks(&bytes);
    clean_restores("carrier.mp4", &bytes, Technique::Mp4FreeBox, move |b| {
        assert_eq!(mp4_tracks(b), tracks, "cleaned MP4 lost its tracks");
    });
}

#[test]
fn cleaning_restores_a_pdf() {
    let bytes = synth_pdf(PdfKind::Classic);
    let pages = pdf_pages(&bytes);
    clean_restores("carrier.pdf", &bytes, Technique::PdfObject, move |b| {
        assert_eq!(pdf_pages(b), pages, "cleaned PDF lost its pages");
    });
}

#[test]
fn cleaning_a_carrier_that_holds_nothing_is_refused_not_silently_copied() {
    let env = Env::new();
    let bytes = synth_jpeg(Style::default());
    let carrier = env.write("plain.jpg", &bytes);
    let err = env.clean(&carrier, "out.jpg", PASS).unwrap_err();
    assert!(
        matches!(err, ZolalError::NoHiddenData { .. }),
        "expected NoHiddenData, got {err:?}"
    );
    assert!(
        !env.path("out.jpg").exists(),
        "nothing should have been written"
    );
}

#[test]
fn cleaning_with_the_wrong_passphrase_writes_nothing() {
    let env = Env::new();
    let bytes = synth_jpeg(Style {
        exif: true,
        ..Style::default()
    });
    let carrier = env.write("carrier.jpg", &bytes);
    let payload = env.write("in/secret.bin", &payload_bytes(2048, 3));
    env.hide(&carrier, &[payload], "stego.jpg", Technique::JpegTrailer)
        .unwrap();

    let err = env
        .clean(&env.path("stego.jpg"), "out.jpg", "not the passphrase")
        .unwrap_err();
    assert!(
        matches!(err, ZolalError::WrongPassphrase),
        "expected WrongPassphrase, got {err:?}"
    );
    assert!(
        !env.path("out.jpg").exists(),
        "nothing should have been written"
    );

    // The stego file itself must be untouched by a failed clean.
    let revealed = env.reveal(&env.path("stego.jpg"), "out", PASS).unwrap();
    assert_eq!(revealed.total_bytes, 2048);
}

/// Files and directories in `dir`, including hidden ones. Missing dir counts as empty.
fn entries(dir: &Path) -> Vec<String> {
    match fs::read_dir(dir) {
        Ok(rd) => rd
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Bundle overhead for a single file called `payload.bin`: prefix + manifest.
fn bundle_overhead() -> u64 {
    let env = Env::new();
    let p = env.write("payload.bin", b"");
    zolal_core::payload::plan(&[p]).unwrap().bundle_len()
}

/// Raw sizes for `payload.bin` that land the envelope exactly on a boundary.
fn boundary_sizes() -> Vec<u64> {
    let k = bundle_overhead();
    let chunk = 64 * 1024;
    let preamble = 35; // salt + nonce prefix
                       // (bundle length, region length it must produce). The inner header adds 2 plaintext bytes.
    let cases = [
        (65_533 - MIN_REGION_LEN, 65_533), // region fills one APP15 segment
        (65_534 - MIN_REGION_LEN, 65_534), // ...and spills 1 byte into a second
        (chunk - 2, preamble + chunk + 16), // plaintext fills one AEAD chunk
        (chunk - 1, preamble + (chunk + 1) + 2 * 16), // ...and spills 1 byte into a second
    ];
    cases
        .iter()
        .map(|&(bundle, region)| {
            assert_eq!(region_len(bundle), region, "bundle {bundle}");
            bundle - k
        })
        .collect()
}

fn roundtrip(technique: Technique) {
    let env = Env::new();
    let carrier_bytes = synth_jpeg(Style {
        exif: true,
        ..Style::default()
    });
    let carrier = env.write("carrier.jpg", &carrier_bytes);

    let sizes: Vec<u64> = SIZES.iter().copied().chain(boundary_sizes()).collect();
    for (i, &size) in sizes.iter().enumerate() {
        let data = payload_bytes(size, i as u64);
        let payload = env.write(&format!("in{i}/payload.bin"), &data);
        let stego = format!("stego{i}.jpg");

        let report = env
            .hide(&carrier, &[payload], &stego, technique)
            .unwrap_or_else(|e| panic!("hide {size}: {e}"));
        let region = region_len(bundle_overhead() + size);
        let framing = match technique {
            Technique::JpegApp15 => app15_overhead(region),
            _ => 0,
        };
        assert_eq!(
            report.output_size,
            carrier_bytes.len() as u64 + region + framing,
            "size {size}: output is carrier + envelope + framing, nothing else"
        );

        let out = format!("out{i}");
        let revealed = env
            .reveal(&env.path(&stego), &out, PASS)
            .unwrap_or_else(|e| panic!("reveal {size}: {e}"));
        assert_eq!(revealed.technique, technique);
        assert_eq!(revealed.total_bytes, size);
        assert_eq!(revealed.files, [env.path(&out).join("payload.bin")]);
        assert!(
            fs::read(&revealed.files[0]).unwrap() == data,
            "size {size}: payload differs"
        );
        assert_eq!(
            entries(&env.path(&out)),
            ["payload.bin"],
            "no staging left behind"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The tests the scaffold promised

#[test]
fn jpeg_trailer_roundtrip() {
    roundtrip(Technique::JpegTrailer);
}

#[test]
fn jpeg_app15_roundtrip() {
    // The 5 MiB case spans 81 segments, so success here is the P1 reassembly check.
    roundtrip(Technique::JpegApp15);
}

/// A real, playable MP4 (a subtitle track with a few samples), built with the `mp4` crate so the
/// fixture stays synthetic. Small, but a genuine ISOBMFF box tree — ftyp, moov with a track,
/// mdat — not the hand-rolled stand-in the unit tests use.
fn synth_mp4() -> Vec<u8> {
    use mp4::{Bytes, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TtxtConfig};

    let config = Mp4Config {
        major_brand: "isom".parse().unwrap(),
        minor_version: 512,
        compatible_brands: vec!["isom".parse().unwrap(), "iso2".parse().unwrap()],
        timescale: 1000,
    };
    let mut writer = Mp4Writer::write_start(Cursor::new(Vec::new()), &config).unwrap();
    writer
        .add_track(&TrackConfig::from(MediaConfig::TtxtConfig(TtxtConfig {})))
        .unwrap();
    for i in 0..8u32 {
        writer
            .write_sample(
                1,
                &Mp4Sample {
                    start_time: u64::from(i) * 1000,
                    duration: 1000,
                    rendering_offset: 0,
                    is_sync: true,
                    bytes: Bytes::from(vec![0x20 + i as u8; 64]),
                },
            )
            .unwrap();
    }
    writer.write_end().unwrap();
    writer.into_writer().into_inner()
}

/// Track count according to the independent `mp4` parser — our proof the stego file still opens
/// as a valid MP4, not just that our own walker likes it.
fn mp4_tracks(bytes: &[u8]) -> usize {
    mp4::Mp4Reader::read_header(Cursor::new(bytes), bytes.len() as u64)
        .expect("stego file must still parse as MP4")
        .tracks()
        .len()
}

#[test]
fn mp4_freebox_roundtrip() {
    let env = Env::new();
    let carrier_bytes = synth_mp4();
    let carrier = env.write("carrier.mp4", &carrier_bytes);
    let tracks = mp4_tracks(&carrier_bytes);
    assert_eq!(tracks, 1, "fixture sanity");
    let k = bundle_overhead();

    for (i, &size) in SIZES.iter().enumerate() {
        let data = payload_bytes(size, i as u64);
        let payload = env.write(&format!("in{i}/payload.bin"), &data);
        let stego = format!("stego{i}.mp4");

        let report = env
            .hide(&carrier, &[payload], &stego, Technique::Auto)
            .unwrap_or_else(|e| panic!("hide {size}: {e}"));
        assert_eq!(report.technique, Technique::Mp4FreeBox);
        // Output is the carrier plus one `free` box (8-byte header) around the envelope.
        assert_eq!(
            report.output_size,
            carrier_bytes.len() as u64 + 8 + region_len(k + size),
            "size {size}"
        );

        let stego_bytes = fs::read(env.path(&stego)).unwrap();
        assert_eq!(
            &stego_bytes[..carrier_bytes.len()],
            &carrier_bytes[..],
            "video untouched"
        );
        assert_eq!(
            mp4_tracks(&stego_bytes),
            tracks,
            "size {size}: still a valid MP4"
        );

        let out = format!("out{i}");
        let revealed = env
            .reveal(&env.path(&stego), &out, PASS)
            .unwrap_or_else(|e| panic!("reveal {size}: {e}"));
        assert_eq!(revealed.technique, Technique::Mp4FreeBox);
        assert_eq!(revealed.total_bytes, size);
        assert!(
            fs::read(&revealed.files[0]).unwrap() == data,
            "size {size}: payload differs"
        );
    }

    // Wrong passphrase on a real hidden MP4 payload is distinguished from a clean carrier.
    env.hide(
        &carrier,
        &[env.write("s.txt", b"x")],
        "s.mp4",
        Technique::Auto,
    )
    .unwrap();
    assert!(matches!(
        env.reveal(&env.path("s.mp4"), "bad", "nope"),
        Err(ZolalError::WrongPassphrase)
    ));
    assert!(matches!(
        env.reveal(&carrier, "clean", PASS),
        Err(ZolalError::NoHiddenData { .. })
    ));
}

/// Which cross-reference form a synthetic PDF should use.
#[derive(Clone, Copy, Debug)]
enum PdfKind {
    /// A classic `xref` table, as PDF 1.4 and many later files still use.
    Classic,
    /// A cross-reference *stream*, which PDF 1.5+ introduced.
    XrefStream,
    /// A classic file that somebody has already incrementally updated once.
    AlreadyUpdated,
}

/// A small but structurally valid PDF: catalog, page tree, one page, one content stream.
fn synth_pdf(kind: PdfKind) -> Vec<u8> {
    let content = b"BT /F1 12 Tf 20 100 Td (hello) Tj ET";
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>".to_vec(),
        stream,
    ];

    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len() as u64);
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }

    if let PdfKind::XrefStream = kind {
        let xref = out.len() as u64;
        let mut data = Vec::new();
        let mut entry = |kind: u8, at: u64, gen: u16| {
            data.push(kind);
            data.extend_from_slice(&(at as u32).to_be_bytes());
            data.extend_from_slice(&gen.to_be_bytes());
        };
        entry(0, 0, 0xFFFF);
        for offset in &offsets {
            entry(1, *offset, 0);
        }
        entry(1, xref, 0);
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
        return out;
    }

    let xref = out.len() as u64;
    out.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f\r\n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n\r\n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );

    if let PdfKind::AlreadyUpdated = kind {
        // A second revision, the way an annotation tool would leave one behind.
        let note = out.len() as u64;
        out.extend_from_slice(b"5 0 obj\n<< /Note (a previous revision) >>\nendobj\n");
        let second = out.len() as u64;
        out.extend_from_slice(b"xref\n5 1\n");
        out.extend_from_slice(format!("{note:010} 00000 n\r\n").as_bytes());
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size 6 /Root 1 0 R /Prev {xref} >>\nstartxref\n{second}\n%%EOF\n"
            )
            .as_bytes(),
        );
    }
    out
}

/// Page count according to `lopdf` — our proof the stego file still opens as a real PDF, not
/// just that our own reader likes it.
fn pdf_pages(bytes: &[u8]) -> usize {
    lopdf::Document::load_from(Cursor::new(bytes))
        .expect("stego file must still parse as a PDF")
        .get_pages()
        .len()
}

#[test]
fn pdf_object_roundtrip() {
    let k = bundle_overhead();
    for kind in [
        PdfKind::Classic,
        PdfKind::XrefStream,
        PdfKind::AlreadyUpdated,
    ] {
        let env = Env::new();
        let carrier_bytes = synth_pdf(kind);
        let carrier = env.write("carrier.pdf", &carrier_bytes);
        assert_eq!(pdf_pages(&carrier_bytes), 1, "{kind:?} fixture sanity");

        for (i, &size) in SIZES.iter().enumerate() {
            let data = payload_bytes(size, i as u64);
            let payload = env.write(&format!("in{i}/payload.bin"), &data);
            let stego = format!("stego{i}.pdf");

            let report = env
                .hide(&carrier, &[payload], &stego, Technique::Auto)
                .unwrap_or_else(|e| panic!("{kind:?} hide {size}: {e}"));
            assert_eq!(report.technique, Technique::PdfObject);

            let stego_bytes = fs::read(env.path(&stego)).unwrap();
            assert_eq!(
                &stego_bytes[..carrier_bytes.len()],
                &carrier_bytes[..],
                "{kind:?}: the original document is untouched"
            );
            assert_eq!(report.output_size, stego_bytes.len() as u64);
            assert!(stego_bytes.ends_with(b"%%EOF\n"));
            // The disguise: a real parser still reads it, and still sees one page.
            assert_eq!(pdf_pages(&stego_bytes), 1, "{kind:?} size {size}");
            // The envelope really is inside the document, not appended past its end.
            assert!(
                report.output_size > carrier_bytes.len() as u64 + region_len(k + size),
                "{kind:?}: output carries the update's own framing too"
            );

            let out = format!("out{i}");
            let revealed = env
                .reveal(&env.path(&stego), &out, PASS)
                .unwrap_or_else(|e| panic!("{kind:?} reveal {size}: {e}"));
            assert_eq!(revealed.technique, Technique::PdfObject);
            assert_eq!(revealed.total_bytes, size);
            assert!(
                fs::read(&revealed.files[0]).unwrap() == data,
                "{kind:?} size {size}: payload differs"
            );
        }

        // A clean document reports nothing hidden; a real one with the wrong key says so.
        assert!(matches!(
            env.reveal(&carrier, "clean", PASS),
            Err(ZolalError::NoHiddenData { .. })
        ));
        env.hide(
            &carrier,
            &[env.write("s.txt", b"x")],
            "s.pdf",
            Technique::Auto,
        )
        .unwrap();
        assert!(matches!(
            env.reveal(&env.path("s.pdf"), "bad", "nope"),
            Err(ZolalError::WrongPassphrase)
        ));
    }
}

#[test]
fn wrong_passphrase_is_distinguishable_from_absent_payload() {
    let env = Env::new();
    let bare = env.write("bare.jpg", &synth_jpeg(Style::default()));
    let with_exif = env.write(
        "exif.jpg",
        &synth_jpeg(Style {
            exif: true,
            ..Style::default()
        }),
    );
    let payload = env.write("secret.txt", b"meet at noon");

    // Nothing there: NoHiddenData, with the re-encoding hint only when EXIF is gone too.
    assert!(matches!(
        env.reveal(&bare, "out", PASS),
        Err(ZolalError::NoHiddenData {
            carrier_looks_reencoded: true
        })
    ));
    assert!(matches!(
        env.reveal(&with_exif, "out", PASS),
        Err(ZolalError::NoHiddenData {
            carrier_looks_reencoded: false
        })
    ));
    // A few stray trailing bytes are too short to be an envelope: still "nothing here".
    let mut stray = fs::read(&with_exif).unwrap();
    stray.extend_from_slice(&[0u8; (MIN_REGION_LEN - 1) as usize]);
    let stray = env.write("stray.jpg", &stray);
    assert!(matches!(
        env.reveal(&stray, "out", PASS),
        Err(ZolalError::NoHiddenData { .. })
    ));

    // Something there, wrong key: WrongPassphrase, for both techniques.
    for technique in [Technique::JpegTrailer, Technique::JpegApp15] {
        let name = format!("{technique:?}.jpg");
        env.hide(&with_exif, std::slice::from_ref(&payload), &name, technique)
            .unwrap();
        assert!(matches!(
            env.reveal(&env.path(&name), "out", "correct horse battery stapl"),
            Err(ZolalError::WrongPassphrase)
        ));
    }
    assert!(
        entries(&env.path("out")).is_empty(),
        "failures leave nothing behind"
    );
}

/// Hide a 5 MiB payload with the trailer technique; return (stego bytes, region offset).
fn big_trailer_stego(env: &Env) -> (Vec<u8>, usize) {
    let carrier = synth_jpeg(Style::default());
    let carrier_path = env.write("carrier.jpg", &carrier);
    let payload = env.write("big.bin", &payload_bytes(5 * 1024 * 1024, 42));
    env.hide(
        &carrier_path,
        &[payload],
        "stego.jpg",
        Technique::JpegTrailer,
    )
    .unwrap();
    (fs::read(env.path("stego.jpg")).unwrap(), carrier.len())
}

fn assert_damaged(env: &Env, name: &str, bytes: &[u8]) {
    let path = env.write(name, bytes);
    let out = format!("out-{name}");
    let err = env.reveal(&path, &out, PASS).unwrap_err();
    assert!(matches!(err, ZolalError::DamagedPayload), "{name}: {err:?}");
    assert!(
        entries(&env.path(&out)).is_empty(),
        "{name}: partial output left behind"
    );
}

#[test]
fn truncated_region_fails_closed() {
    let env = Env::new();
    let (stego, region_start) = big_trailer_stego(&env);
    let chunk = SEALED_CHUNK_LEN as usize;
    let preamble = 35;
    let last_chunk_start =
        region_start + preamble + ((stego.len() - region_start - preamble - 1) / chunk) * chunk;

    assert_damaged(&env, "minus-one.jpg", &stego[..stego.len() - 1]);
    assert_damaged(&env, "minus-last-chunk.jpg", &stego[..last_chunk_start]);
    assert_damaged(
        &env,
        "half.jpg",
        &stego[..region_start + (stego.len() - region_start) / 2],
    );
    assert_damaged(
        &env,
        "chunk-zero-only.jpg",
        &stego[..region_start + preamble + chunk],
    );
}

#[test]
fn reordered_or_altered_region_fails_closed() {
    let env = Env::new();
    let (stego, region_start) = big_trailer_stego(&env);
    let chunk = SEALED_CHUNK_LEN as usize;
    let at = |i: usize| region_start + 35 + i * chunk;

    let mut swapped = stego.clone();
    let tmp = swapped[at(1)..at(2)].to_vec();
    swapped.copy_within(at(2)..at(3), at(1));
    swapped[at(2)..at(3)].copy_from_slice(&tmp);
    assert_damaged(&env, "swapped.jpg", &swapped);

    let mut flipped = stego.clone();
    flipped[at(40) + 7] ^= 0x01;
    assert_damaged(&env, "flipped.jpg", &flipped);

    let mut extended = stego.clone();
    extended.push(0);
    assert_damaged(&env, "extended.jpg", &extended);
}

/// `(offset, len)` of every APP15 segment before the first scan, in a well-formed JPEG.
fn app15_segments(jpeg: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 2;
    loop {
        let marker = jpeg[pos + 1];
        let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize + 2;
        if marker == 0xEF {
            out.push((pos, len));
        }
        if marker == 0xDA {
            return out;
        }
        pos += len;
    }
}

#[test]
fn app15_segments_dropped_or_swapped_fail_closed() {
    let env = Env::new();
    let carrier = env.write("carrier.jpg", &synth_jpeg(Style::default()));
    let payload = env.write("big.bin", &payload_bytes(1024 * 1024, 5));
    env.hide(&carrier, &[payload], "stego.jpg", Technique::JpegApp15)
        .unwrap();
    let stego = fs::read(env.path("stego.jpg")).unwrap();
    let segs = app15_segments(&stego);
    assert!(segs.len() > 10);

    // Chunk 0 lives in the first two segments; damage further in must say "damaged".
    let (off, len) = segs[5];
    let dropped = [&stego[..off], &stego[off + len..]].concat();
    assert_damaged(&env, "dropped.jpg", &dropped);

    let ((a, alen), (b, blen)) = (segs[5], segs[6]);
    assert_eq!(alen, blen);
    let mut swapped = stego.clone();
    swapped[a..a + alen].copy_from_slice(&stego[b..b + blen]);
    swapped[b..b + blen].copy_from_slice(&stego[a..a + alen]);
    assert_damaged(&env, "swapped.jpg", &swapped);
}

#[test]
fn carrier_still_opens_after_embedding() {
    let styles = [
        (
            "baseline+exif",
            Style {
                exif: true,
                ..Style::default()
            },
        ),
        (
            "progressive",
            Style {
                progressive: true,
                ..Style::default()
            },
        ),
        (
            "restart-markers",
            Style {
                restart_interval: Some(2),
                ..Style::default()
            },
        ),
        (
            "mpf-chained",
            Style {
                chained: true,
                exif: true,
                ..Style::default()
            },
        ),
    ];
    let env = Env::new();
    let payload = env.write("p.bin", &payload_bytes(300_000, 9));

    for (name, style) in styles {
        let original = synth_jpeg(style);
        let carrier = env.write(&format!("{name}.jpg"), &original);
        let want = decode(&original);

        for technique in [Technique::JpegTrailer, Technique::JpegApp15] {
            let out = format!("{name}-{technique:?}.jpg");
            env.hide(&carrier, std::slice::from_ref(&payload), &out, technique)
                .unwrap();
            let stego = fs::read(env.path(&out)).unwrap();
            assert_eq!(
                decode(&stego),
                want,
                "{name} / {technique:?}: pixels changed"
            );

            if style.chained {
                // The second image (the gain map, in a real MPF file) must survive intact.
                let secondary = &original[encode(96, 64, style, 7).len()..];
                match technique {
                    Technique::JpegTrailer => assert_eq!(&stego[..original.len()], &original[..]),
                    _ => assert!(stego.ends_with(secondary)),
                }
            }
            env.reveal(&env.path(&out), &format!("out-{out}"), PASS)
                .unwrap();
        }
    }
}

fn decode(jpeg: &[u8]) -> (u16, u16, Vec<u8>) {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(jpeg));
    let pixels = decoder.decode().expect("carrier must still decode");
    let info = decoder.info().unwrap();
    (info.width, info.height, pixels)
}

// ---------------------------------------------------------------------------------------------
// Behaviour around the edges

#[test]
fn hiding_again_replaces_the_previous_payload() {
    let env = Env::new();
    let original = synth_jpeg(Style::default());
    let carrier = env.write("carrier.jpg", &original);
    let a = env.write("a.txt", b"first secret");
    let b = env.write("b.txt", b"second secret");
    let c = env.write("c.txt", b"third");

    env.hide(&carrier, &[a], "one.jpg", Technique::JpegTrailer)
        .unwrap();
    env.hide(&env.path("one.jpg"), &[b], "two.jpg", Technique::JpegApp15)
        .unwrap();
    let got = env.reveal(&env.path("two.jpg"), "out2", PASS).unwrap();
    assert_eq!(entries(&env.path("out2")), ["b.txt"]);
    assert_eq!(got.technique, Technique::JpegApp15);

    let report = env
        .hide(
            &env.path("two.jpg"),
            std::slice::from_ref(&c),
            "three.jpg",
            Technique::JpegTrailer,
        )
        .unwrap();
    let c_bundle = zolal_core::payload::plan(&[c]).unwrap().bundle_len();
    assert_eq!(
        report.output_size,
        original.len() as u64 + region_len(c_bundle)
    );
    env.reveal(&env.path("three.jpg"), "out3", PASS).unwrap();
    assert_eq!(entries(&env.path("out3")), ["c.txt"]);
}

#[test]
fn several_files_with_clashing_names_and_no_clobbering() {
    let env = Env::new();
    let carrier = env.write("carrier.jpg", &synth_jpeg(Style::default()));
    let x1 = env.write("a/x.txt", b"one");
    let x2 = env.write("b/x.txt", b"two");
    let photo = env.write("c/été 📷.jpg", &synth_jpeg(Style::default()));
    env.hide(
        &carrier,
        &[x1, x2, photo],
        "stego.jpg",
        Technique::JpegTrailer,
    )
    .unwrap();

    let existing = env.write("out/x.txt", b"already here");
    let got = env.reveal(&env.path("stego.jpg"), "out", PASS).unwrap();
    let names: Vec<_> = got
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["x (2).txt", "x (3).txt", "été 📷.jpg"]);
    assert_eq!(fs::read(&existing).unwrap(), b"already here");
    assert_eq!(fs::read(&got.files[0]).unwrap(), b"one");
    assert_eq!(fs::read(&got.files[1]).unwrap(), b"two");
}

#[test]
fn hiding_in_place_overwrites_the_carrier_atomically() {
    let env = Env::new();
    let carrier = env.write("photo.jpg", &synth_jpeg(Style::default()));
    let payload = env.write("p.txt", b"in place");
    hide(
        HideRequest {
            carrier: carrier.clone(),
            payloads: vec![payload],
            output: carrier.clone(),
            passphrase: SecretString::from(PASS),
            technique: Technique::Auto,
            kdf_params: Some(FAST),
        },
        &NoProgress,
    )
    .unwrap();
    env.reveal(&carrier, "out", PASS).unwrap();
    let mut left = entries(env.dir.path());
    left.sort();
    assert_eq!(
        left,
        ["out", "p.txt", "photo.jpg"],
        "no temp file left behind"
    );
}

#[test]
fn bad_requests_are_refused_cleanly() {
    let env = Env::new();
    let jpeg = env.write("c.jpg", &synth_jpeg(Style::default()));
    let payload = env.write("p.txt", b"x");
    let png = env.write("c.png", b"\x89PNG\r\n\x1a\n0000000000");
    let heic = env.write("c.heic", b"\0\0\0\x18ftypheic\0\0\0\0mif1heic");
    let broken_pdf = env.write("c.pdf", b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n");
    let empty = env.write("empty.jpg", b"");

    let unsupported = |path: &Path| match env.hide(
        path,
        std::slice::from_ref(&payload),
        "o.jpg",
        Technique::Auto,
    ) {
        Err(ZolalError::UnsupportedFormat { detected }) => detected,
        other => panic!("{path:?}: {other:?}"),
    };
    assert!(unsupported(&png).starts_with("PNG"));
    assert!(unsupported(&heic).starts_with("HEIC"));
    // Every format is implemented now, so an unparseable one is malformed, not unsupported:
    // this PDF has no cross-reference section at all.
    assert!(matches!(
        env.hide(
            &broken_pdf,
            std::slice::from_ref(&payload),
            "o.pdf",
            Technique::Auto
        ),
        Err(ZolalError::MalformedContainer { format: "PDF", .. })
    ));
    assert!(matches!(
        env.hide(
            &empty,
            std::slice::from_ref(&payload),
            "o.jpg",
            Technique::Auto
        ),
        Err(ZolalError::CarrierTooSmall)
    ));
    assert!(matches!(
        env.hide(
            &jpeg,
            std::slice::from_ref(&payload),
            "o.jpg",
            Technique::Mp4FreeBox
        ),
        Err(ZolalError::InvalidRequest(_))
    ));
    assert!(matches!(
        env.hide(&jpeg, &[], "o.jpg", Technique::Auto),
        Err(ZolalError::InvalidRequest(_))
    ));
    assert!(matches!(
        env.hide(
            &jpeg,
            &[env.dir.path().to_path_buf()],
            "o.jpg",
            Technique::Auto
        ),
        Err(ZolalError::InvalidRequest(_))
    ));
    assert!(!env.path("o.jpg").exists());
}

#[test]
fn cancellation_leaves_nothing_behind() {
    let env = Env::new();
    let carrier = env.write("c.jpg", &synth_jpeg(Style::default()));
    let payload = env.write("p.bin", &payload_bytes(1024 * 1024, 1));
    env.hide(
        &carrier,
        std::slice::from_ref(&payload),
        "stego.jpg",
        Technique::Auto,
    )
    .unwrap();

    let cancelled = AtomicProgress::new();
    cancelled.cancel();
    let err = hide(
        HideRequest {
            carrier: carrier.clone(),
            payloads: vec![payload],
            output: env.path("never.jpg"),
            passphrase: SecretString::from(PASS),
            technique: Technique::Auto,
            kdf_params: Some(FAST),
        },
        &cancelled,
    )
    .unwrap_err();
    assert!(matches!(err, ZolalError::Cancelled));

    let err = reveal(
        RevealRequest {
            carrier: env.path("stego.jpg"),
            output_dir: env.path("out"),
            passphrase: SecretString::from(PASS),
            kdf_params: Some(FAST),
        },
        &cancelled,
    )
    .unwrap_err();
    assert!(matches!(err, ZolalError::Cancelled));

    let mut left = entries(env.dir.path());
    left.sort();
    assert_eq!(left, ["c.jpg", "p.bin", "stego.jpg"]);
}

#[test]
fn progress_reaches_the_end() {
    let env = Env::new();
    let carrier = env.write("c.jpg", &synth_jpeg(Style::default()));
    let payload = env.write("p.bin", &payload_bytes(300_000, 3));
    let progress = AtomicProgress::new();
    hide(
        HideRequest {
            carrier,
            payloads: vec![payload],
            output: env.path("s.jpg"),
            passphrase: SecretString::from(PASS),
            technique: Technique::Auto,
            kdf_params: Some(FAST),
        },
        &progress,
    )
    .unwrap();
    let (done, total) = progress.snapshot();
    assert!(total > 300_000 && done == total, "{done}/{total}");
}

#[test]
fn probe_and_plausibility() {
    let env = Env::new();
    let original = synth_jpeg(Style::default());
    let carrier = env.write("c.jpg", &original);
    let info = probe(&carrier).unwrap();
    assert_eq!(info.format, CarrierFormat::Jpeg);
    assert_eq!(info.file_size, original.len() as u64);
    assert!(!info.has_candidate_region);

    let small = env.write("small.txt", &[1; 100]);
    let report = env
        .hide(&carrier, &[small], "s.jpg", Technique::Auto)
        .unwrap();
    assert!(probe(&env.path("s.jpg")).unwrap().has_candidate_region);
    assert!(report.plausibility.ratio > 1.0);

    // A payload a fraction of the carrier reads as natural; ten times it does not.
    let n = original.len() as u64;
    assert_eq!(
        plausibility(&carrier, n / 4).unwrap().verdict,
        Verdict::Natural
    );
    assert_eq!(
        plausibility(&carrier, n).unwrap().verdict,
        Verdict::Noticeable
    );
    assert_eq!(
        plausibility(&carrier, 10 * n).unwrap().verdict,
        Verdict::Suspicious
    );
    let p = plausibility(&carrier, 1000).unwrap();
    assert_eq!(p.result_size, n + region_len(1000));
}

#[test]
fn production_kdf_parameters_roundtrip() {
    // The only test on real Argon2 costs (48 MiB, t=3): proves the `None` path, where reveal
    // tries every known version's parameters.
    let env = Env::new();
    let carrier = env.write("c.jpg", &synth_jpeg(Style::default()));
    let payload = env.write("p.txt", b"default costs");
    let request = |pass: &str| RevealRequest {
        carrier: env.path("s.jpg"),
        output_dir: env.path("out"),
        passphrase: SecretString::from(pass),
        kdf_params: None,
    };
    hide(
        HideRequest {
            carrier,
            payloads: vec![payload],
            output: env.path("s.jpg"),
            passphrase: SecretString::from(PASS),
            technique: Technique::Auto,
            kdf_params: None,
        },
        &NoProgress,
    )
    .unwrap();
    assert!(matches!(
        reveal(request("wrong"), &NoProgress),
        Err(ZolalError::WrongPassphrase)
    ));
    let got = reveal(request(PASS), &NoProgress).unwrap();
    assert_eq!(fs::read(&got.files[0]).unwrap(), b"default costs");

    // Fast-param files don't open under production params, and vice versa.
    assert!(matches!(
        env.reveal(&env.path("s.jpg"), "out-fast", PASS),
        Err(ZolalError::WrongPassphrase)
    ));
}

// ---------------------------------------------------------------------------------------------
// The JPEG size limit: a photo must never turn into something viewers can't open

/// A payload file of `len` zero bytes that costs no disk space (sparse).
fn sparse(env: &Env, name: &str, len: u64) -> PathBuf {
    let path = env.write(name, b"");
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len)
        .unwrap();
    path
}

#[test]
fn jpeg_output_is_capped_and_points_to_a_video() {
    let env = Env::new();
    let original = synth_jpeg(Style::default());
    let carrier = env.write("c.jpg", &original);
    let n = original.len() as u64;
    let k = bundle_overhead();

    // Largest payload whose output still fits. Bisect, because the envelope grows in 17-byte
    // steps at chunk boundaries, so the limit itself may not be reachable exactly.
    let fits = |size: u64| n + region_len(k + size) <= JPEG_MAX_OUTPUT;
    let (mut lo, mut hi) = (0, JPEG_MAX_OUTPUT);
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let largest = sparse(&env, "fits/payload.bin", lo);
    let report = env
        .hide(&carrier, &[largest], "fits.jpg", Technique::JpegTrailer)
        .unwrap();
    assert!(report.output_size <= JPEG_MAX_OUTPUT);
    assert!(report.output_size > JPEG_MAX_OUTPUT - 64);
    assert_eq!(report.plausibility.verdict, Verdict::Suspicious);

    // One byte more is refused, for both techniques, before anything is written.
    let over = sparse(&env, "over/payload.bin", hi);
    for technique in [Technique::JpegTrailer, Technique::JpegApp15] {
        match env.hide(&carrier, std::slice::from_ref(&over), "over.jpg", technique) {
            Err(ZolalError::PayloadTooLarge {
                output_size,
                limit,
                suggestion,
            }) => {
                assert_eq!(limit, JPEG_MAX_OUTPUT);
                assert!(output_size > limit);
                assert_eq!(suggestion, CarrierFormat::Mp4, "suggest a video");
            }
            other => panic!("{technique:?}: {other:?}"),
        }
    }

    // A gigabyte is refused just as fast: the check needs only the payload's size.
    let huge = sparse(&env, "huge/payload.bin", 1 << 30);
    assert!(matches!(
        env.hide(&carrier, &[huge], "over.jpg", Technique::Auto),
        Err(ZolalError::PayloadTooLarge { .. })
    ));

    let mut left = entries(env.dir.path());
    left.sort();
    assert_eq!(
        left,
        ["c.jpg", "fits", "fits.jpg", "huge", "over"],
        "no output, no temp file"
    );

    // The UI hears about it before the user commits.
    assert_eq!(
        plausibility(&carrier, JPEG_MAX_OUTPUT).unwrap().verdict,
        Verdict::TooLarge
    );
}

#[test]
fn a_heavy_stego_photo_can_still_be_reused() {
    // A photo already carrying 60 MiB of hidden data is over the limit on its own. Hiding
    // something small in it drops the old payload, so the result fits and must be allowed.
    let env = Env::new();
    let original = synth_jpeg(Style::default());
    let heavy = sparse(&env, "heavy.jpg", 0);
    fs::write(&heavy, &original).unwrap();
    fs::File::options()
        .write(true)
        .open(&heavy)
        .unwrap()
        .set_len(original.len() as u64 + 60 * 1024 * 1024)
        .unwrap();

    let predicted = plausibility(&heavy, 5).unwrap();
    assert_eq!(predicted.result_size, original.len() as u64 + region_len(5));
    assert_ne!(predicted.verdict, Verdict::TooLarge);

    let small = env.write("s.txt", b"small");
    let report = env
        .hide(&heavy, &[small], "reused.jpg", Technique::Auto)
        .unwrap();
    assert!(report.output_size < 1024 * 1024);
    env.reveal(&env.path("reused.jpg"), "out", PASS).unwrap();
}

// ---------------------------------------------------------------------------------------------
// MP3

/// Which leading ID3v2 tag a synthetic MP3 should have.
#[derive(Clone, Copy, Debug)]
enum Id3 {
    /// No tag: the file starts with an audio frame.
    None,
    /// ID3v2.3 with a title frame and 64 bytes of padding.
    V3Padded,
    /// ID3v2.4 (synchsafe frame sizes) with a title frame and no padding.
    V4,
}

/// Silent MPEG-1 Layer III audio: 40 frames at 128 kbit/s, 44.1 kHz. All-zero side info and
/// main data decode as silence, so this is a real, playable (if quiet) MP3.
fn mp3_audio() -> Vec<u8> {
    let mut frame = vec![0u8; 417]; // 144 * 128000 / 44100, no padding bit
    frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    frame.repeat(40)
}

/// The title frame every tagged fixture carries, so tests can check it survives.
fn mp3_title_frame(major: u8) -> Vec<u8> {
    let body = b"\x00Test song";
    let mut f = b"TIT2".to_vec();
    let n = body.len() as u32;
    f.extend_from_slice(&if major == 4 {
        [0, 0, 0, n as u8] // synchsafe; small enough for one byte
    } else {
        n.to_be_bytes()
    });
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(body);
    f
}

fn synth_mp3(tag: Id3) -> Vec<u8> {
    let (major, padding) = match tag {
        Id3::None => return mp3_audio(),
        Id3::V3Padded => (3, 64),
        Id3::V4 => (4, 0),
    };
    let mut body = mp3_title_frame(major);
    body.resize(body.len() + padding, 0);
    let n = body.len();
    let mut out = vec![b'I', b'D', b'3', major, 0, 0];
    out.extend_from_slice(&[0, 0, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&mp3_audio());
    out
}

/// Independent check that `bytes` is still the song: a well-formed ID3v2 header whose size
/// lands exactly on the untouched audio, and the title frame still in the tag.
fn assert_mp3_intact(bytes: &[u8], tag: Id3) {
    let audio = mp3_audio();
    assert!(bytes.ends_with(&audio), "audio frames changed");
    let tag_len = bytes.len() - audio.len();
    if tag_len == 0 {
        return;
    }
    assert_eq!(&bytes[..3], b"ID3");
    let size = bytes[6..10]
        .iter()
        .fold(0usize, |acc, &b| (acc << 7) | usize::from(b));
    assert_eq!(10 + size, tag_len, "tag size doesn't reach the audio");
    let major = bytes[3];
    if !matches!(tag, Id3::None) {
        let title = mp3_title_frame(major);
        assert_eq!(&bytes[10..10 + title.len()], &title[..], "title frame lost");
    }
}

#[test]
fn mp3_id3_roundtrip() {
    for tag in [Id3::None, Id3::V3Padded, Id3::V4] {
        let env = Env::new();
        let carrier_bytes = synth_mp3(tag);
        let carrier = env.write("song.mp3", &carrier_bytes);
        assert_eq!(probe(&carrier).unwrap().format, CarrierFormat::Mp3);
        let k = bundle_overhead();

        for (i, &size) in SIZES.iter().enumerate() {
            let data = payload_bytes(size, i as u64);
            let payload = env.write(&format!("in{i}/payload.bin"), &data);
            let stego = format!("stego{i}.mp3");

            let report = env
                .hide(&carrier, &[payload], &stego, Technique::Auto)
                .unwrap_or_else(|e| panic!("{tag:?} hide {size}: {e}"));
            assert_eq!(report.technique, Technique::Mp3Id3);

            // Tag header + the song's frames (padding dropped) + PRIV header + owner + envelope.
            let frames = match tag {
                Id3::None => 0,
                Id3::V3Padded => mp3_title_frame(3).len(),
                Id3::V4 => mp3_title_frame(4).len(),
            } as u64;
            let expected = 10 + frames + 10 + 1 + region_len(k + size) + mp3_audio().len() as u64;
            assert_eq!(report.output_size, expected, "{tag:?} size {size}");

            let stego_bytes = fs::read(env.path(&stego)).unwrap();
            assert_mp3_intact(&stego_bytes, tag);

            let out = format!("out{i}");
            let revealed = env
                .reveal(&env.path(&stego), &out, PASS)
                .unwrap_or_else(|e| panic!("{tag:?} reveal {size}: {e}"));
            assert_eq!(revealed.technique, Technique::Mp3Id3);
            assert!(
                fs::read(&revealed.files[0]).unwrap() == data,
                "{tag:?} size {size}: payload differs"
            );
        }

        // Hiding again replaces the payload instead of stacking a second one.
        let once = fs::metadata(env.path("stego1.mp3")).unwrap().len();
        let again = env
            .hide(
                &env.path("stego1.mp3"),
                &[env.path("in1/payload.bin")],
                "twice.mp3",
                Technique::Auto,
            )
            .unwrap();
        assert_eq!(again.output_size, once, "{tag:?}: re-hiding grew the file");

        assert!(matches!(
            env.reveal(&env.path("stego1.mp3"), "bad", "nope"),
            Err(ZolalError::WrongPassphrase)
        ));
        assert!(matches!(
            env.reveal(&carrier, "clean", PASS),
            Err(ZolalError::NoHiddenData { .. })
        ));
    }
}

#[test]
fn cleaning_restores_an_mp3() {
    for tag in [Id3::None, Id3::V4] {
        clean_restores("song.mp3", &synth_mp3(tag), Technique::Mp3Id3, move |b| {
            assert_mp3_intact(b, tag);
        });
    }
}

#[test]
fn mp3_tags_we_cant_rewrite_are_refused_not_damaged() {
    let env = Env::new();
    let payload = env.write("p.txt", b"x");
    let mut v22 = vec![b'I', b'D', b'3', 2, 0, 0, 0, 0, 0, 0];
    v22.extend_from_slice(&mp3_audio());
    let mut unsync = synth_mp3(Id3::V4);
    unsync[5] = 0x80;
    for (name, bytes) in [("v22.mp3", v22), ("unsync.mp3", unsync)] {
        let carrier = env.write(name, &bytes);
        match env.hide(
            &carrier,
            std::slice::from_ref(&payload),
            "o.mp3",
            Technique::Auto,
        ) {
            Err(ZolalError::MalformedContainer { format: "MP3", .. }) => {}
            other => panic!("{name}: {other:?}"),
        }
        assert!(!env.path("o.mp3").exists(), "{name}: wrote output anyway");
    }
}
