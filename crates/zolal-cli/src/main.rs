//! Developer CLI — exercises the engine on desktop, where iteration is fast and there's no
//! simulator in the loop. **Not shipped in the app.**
//!
//! ```text
//! zolal hide   <carrier> <output> <payload...>   [--pass <passphrase>] [--technique trailer|app15]
//! zolal reveal <carrier> <output-dir>            [--pass <passphrase>]
//! zolal clean  <carrier> <output>                [--pass <passphrase>]
//! zolal probe  <carrier>
//! ```
//!
//! The passphrase comes from `--pass` or, to keep it out of shell history and `ps`, from the
//! `ZOLAL_PASS` environment variable.
//!
//! Unlike an app for end users, this tool tells failures apart on purpose ("wrong passphrase",
//! "nothing hidden", "damaged"): it is for the person who hid the file. See
//! [`zolal_core::error`] for why a user-facing UI should not.
//!
//! Deliberately argument-parser-free: one less dependency to license-audit, and the surface is
//! four commands.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use secrecy::SecretString;
use zolal_core::{
    clean, hide, probe, reveal, CleanRequest, HideRequest, Progress, RevealRequest, Technique,
    ZolalError,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("hide") => cmd_hide(&args[1..]),
        Some("reveal") => cmd_reveal(&args[1..]),
        Some("clean") => cmd_clean(&args[1..]),
        Some("probe") => cmd_probe(&args[1..]),
        _ => Err(CliError::Usage(String::new())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(msg)) => {
            if !msg.is_empty() {
                eprintln!("error: {msg}\n");
            }
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(CliError::Core(e)) => {
            eprintln!("error: {}", explain(&e));
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
zolal — developer CLI (not shipped)

USAGE:
    zolal hide   <carrier> <output> <payload...> [--pass <passphrase>] [--technique trailer|app15]
    zolal reveal <carrier> <output-dir>          [--pass <passphrase>]
    zolal clean  <carrier> <output>              [--pass <passphrase>]
    zolal probe  <carrier>

hide    hide one or more files inside a carrier
reveal  recover the hidden files
clean   write a copy of the carrier with its hidden files removed
probe   report the format, and whether a region that could hold data exists

The passphrase can also come from the ZOLAL_PASS environment variable.
Carriers: JPEG, MP4, PDF and MP3. Convert HEIC->JPEG and MOV->MP4 before use.";

enum CliError {
    Usage(String),
    Core(ZolalError),
}

impl From<ZolalError> for CliError {
    fn from(e: ZolalError) -> Self {
        CliError::Core(e)
    }
}

type CliResult = Result<(), CliError>;

struct Args {
    positional: Vec<String>,
    pass: Option<String>,
    technique: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, CliError> {
    let mut out = Args {
        positional: Vec::new(),
        pass: None,
        technique: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| CliError::Usage(format!("{flag} needs a value")))
        };
        match arg.as_str() {
            "--pass" => out.pass = Some(value("--pass")?),
            "--technique" => out.technique = Some(value("--technique")?),
            flag if flag.starts_with("--") => {
                return Err(CliError::Usage(format!("unknown option {flag}")))
            }
            _ => out.positional.push(arg.clone()),
        }
    }
    Ok(out)
}

fn passphrase(args: &Args) -> Result<SecretString, CliError> {
    args.pass
        .clone()
        .or_else(|| std::env::var("ZOLAL_PASS").ok())
        .filter(|p| !p.is_empty())
        .map(SecretString::from)
        .ok_or_else(|| CliError::Usage("a passphrase is required (--pass or ZOLAL_PASS)".into()))
}

fn cmd_hide(raw: &[String]) -> CliResult {
    let args = parse(raw)?;
    let [carrier, output, payloads @ ..] = args.positional.as_slice() else {
        return Err(CliError::Usage(
            "hide needs <carrier> <output> <payload...>".into(),
        ));
    };
    if payloads.is_empty() {
        return Err(CliError::Usage(
            "hide needs at least one payload file".into(),
        ));
    }
    let technique = match args.technique.as_deref() {
        None => Technique::Auto,
        Some("trailer") => Technique::JpegTrailer,
        Some("app15") => Technique::JpegApp15,
        Some(other) => return Err(CliError::Usage(format!("unknown technique {other}"))),
    };
    let carrier = PathBuf::from(carrier);
    let carrier_size = std::fs::metadata(&carrier).map_err(ZolalError::from)?.len();

    let progress = StderrProgress::default();
    let report = hide(
        HideRequest {
            carrier,
            payloads: payloads.iter().map(PathBuf::from).collect(),
            output: PathBuf::from(output),
            passphrase: passphrase(&args)?,
            technique,
            kdf_params: None,
        },
        &progress,
    );
    progress.finish();
    let report = report?;

    println!(
        "hid {} file(s) in {output} using {:?}",
        payloads.len(),
        report.technique
    );
    println!(
        "size {} -> {} (x{:.2}, {:?})",
        human(carrier_size),
        human(report.output_size),
        report.plausibility.ratio,
        report.plausibility.verdict
    );
    Ok(())
}

fn cmd_reveal(raw: &[String]) -> CliResult {
    let args = parse(raw)?;
    let [carrier, output_dir] = args.positional.as_slice() else {
        return Err(CliError::Usage(
            "reveal needs <carrier> <output-dir>".into(),
        ));
    };
    let progress = StderrProgress::default();
    let report = reveal(
        RevealRequest {
            carrier: PathBuf::from(carrier),
            output_dir: PathBuf::from(output_dir),
            passphrase: passphrase(&args)?,
            kdf_params: None,
        },
        &progress,
    );
    progress.finish();
    let report = report?;

    println!(
        "recovered {} file(s), {} (found via {:?}):",
        report.files.len(),
        human(report.total_bytes),
        report.technique
    );
    for file in &report.files {
        println!("  {}", file.display());
    }
    Ok(())
}

fn cmd_clean(raw: &[String]) -> CliResult {
    let args = parse(raw)?;
    let [carrier, output] = args.positional.as_slice() else {
        return Err(CliError::Usage("clean needs <carrier> <output>".into()));
    };
    let progress = StderrProgress::default();
    let report = clean(
        CleanRequest {
            carrier: PathBuf::from(carrier),
            output: PathBuf::from(output),
            passphrase: passphrase(&args)?,
            kdf_params: None,
        },
        &progress,
    );
    progress.finish();
    let report = report?;

    println!(
        "removed {} (found via {:?}); {output} is now {}",
        human(report.removed_bytes),
        report.technique,
        human(report.output_size)
    );
    Ok(())
}

fn cmd_probe(raw: &[String]) -> CliResult {
    let args = parse(raw)?;
    let [carrier] = args.positional.as_slice() else {
        return Err(CliError::Usage("probe needs <carrier>".into()));
    };
    let info = probe(&PathBuf::from(carrier))?;
    println!("format:    {}", info.format.name());
    println!("size:      {}", human(info.file_size));
    println!(
        "candidate: {}",
        if info.has_candidate_region {
            "yes (a region that could hold hidden data; only a passphrase can confirm)"
        } else {
            "no"
        }
    );
    Ok(())
}

/// The UX-contract errors, worded the way the app should word them.
fn explain(e: &ZolalError) -> String {
    match e {
        ZolalError::WrongPassphrase => "wrong passphrase".into(),
        ZolalError::NoHiddenData {
            carrier_looks_reencoded: true,
        } => "nothing hidden in this file. It looks re-encoded: if it was sent as a photo, \
              the hidden content was stripped in transit. Ask for it as a file or a .zip."
            .into(),
        ZolalError::NoHiddenData { .. } => "nothing hidden in this file".into(),
        ZolalError::DamagedPayload => "the passphrase is right, but the hidden data is damaged \
                                       or incomplete (was the file cut short in transfer?)"
            .into(),
        ZolalError::PayloadTooLarge {
            output_size,
            limit,
            suggestion,
        } => format!(
            "too big to hide in this file: the result would be {} (the limit is {}), and a file \
             that size may not open at all. Hide it in a video ({}) instead.",
            human(*output_size),
            human(*limit),
            suggestion.name()
        ),
        other => other.to_string(),
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Prints a percentage to stderr when it changes, but only on a terminal: piped or logged
/// output would otherwise fill with carriage-return noise.
struct StderrProgress {
    enabled: bool,
    last: AtomicU64,
}

impl Default for StderrProgress {
    fn default() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
            last: AtomicU64::new(0),
        }
    }
}

impl StderrProgress {
    fn finish(&self) {
        if self.enabled && self.last.load(Ordering::Relaxed) > 0 {
            eprintln!();
        }
    }
}

impl Progress for StderrProgress {
    fn update(&self, done: u64, total: u64) {
        if !self.enabled || total == 0 {
            return;
        }
        let pct = (done * 100 / total).max(1);
        if self.last.swap(pct, Ordering::Relaxed) != pct {
            eprint!("\r{pct:3}%");
        }
    }
}
