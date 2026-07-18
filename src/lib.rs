use std::process;

use crate::model::history::{DEFAULT_HISTORY_SECS, DEFAULT_INTERVAL_MS};

pub mod collect;
pub mod diag;
pub mod model;
pub mod parse;
pub mod sample;
pub mod ui;

/// Thin helper: get CLOCK_BOOTTIME in nanoseconds.
/// On error, falls back to CLOCK_MONOTONIC.
pub fn clock_boottime_ns() -> u64 {
    let mut tp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut tp) };
    if ret == 0 {
        tp.tv_sec as u64 * 1_000_000_000 + tp.tv_nsec as u64
    } else {
        // fallback: CLOCK_MONOTONIC
        let ret2 = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp) };
        if ret2 == 0 {
            tp.tv_sec as u64 * 1_000_000_000 + tp.tv_nsec as u64
        } else {
            // last resort: system time approximation
            use std::time::SystemTime;
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        }
    }
}

/// How the process should run after argv is parsed.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Gui,
    Once,
    Soak(u64),
}

/// Parsed CLI. Values are raw until `HistoryConfig::validate` / soak bounds.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
    interval_ms: u64,
    history_secs: u64,
}

/// Successful parse outcomes that are not a normal run (help/version).
#[derive(Debug, PartialEq, Eq)]
enum EarlyExit {
    Help,
    Version,
}

const MAX_SOAK_SECONDS: u64 = 86400; // 1 day
const USAGE: &str = "\
lightwatch — native Linux system monitor with bounded resource use

Usage:
  lightwatch [OPTIONS]

Options:
  --once              Print one snapshot after waiting for deltas, then exit
  --soak SECS         Headless soak test for N seconds (0 allowed; max 86400)
  --interval MS       Sampling interval in milliseconds (default 1000)
  --history SECS      History window in seconds (default 60)
  -h, --help          Print this help and exit
  -V, --version       Print version and exit
";

/// Parse argv after the program name. Accepts `--flag value` and `--flag=value`.
fn parse_args<I>(args: I) -> Result<Result<Args, EarlyExit>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut mode = Mode::Gui;
    let mut once = false;
    let mut soak: Option<u64> = None;
    let mut interval_ms = DEFAULT_INTERVAL_MS;
    let mut history_secs = DEFAULT_HISTORY_SECS;

    let mut iter = args.into_iter();
    while let Some(token) = iter.next() {
        // Split --flag=value into flag + value without requiring a following token.
        // Keep `token` intact so unknown-flag errors can show the full original form.
        let (flag, inline_value): (&str, Option<&str>) = match token.split_once('=') {
            Some((name, value)) if name.starts_with('-') => (name, Some(value)),
            _ => (token.as_str(), None),
        };

        match flag {
            "-h" | "--help" => {
                if inline_value.is_some() {
                    return Err("unexpected value for --help".into());
                }
                return Ok(Err(EarlyExit::Help));
            }
            "-V" | "--version" => {
                if inline_value.is_some() {
                    return Err("unexpected value for --version".into());
                }
                return Ok(Err(EarlyExit::Version));
            }
            "--once" => {
                if inline_value.is_some() {
                    return Err("unexpected value for --once".into());
                }
                once = true;
            }
            "--soak" => {
                let raw = take_value("--soak", inline_value, &mut iter)?;
                let secs: u64 = raw
                    .parse()
                    .map_err(|_| format!("invalid --soak value: {raw}"))?;
                soak = Some(secs);
            }
            "--interval" => {
                let raw = take_value("--interval", inline_value, &mut iter)?;
                interval_ms = raw
                    .parse()
                    .map_err(|_| format!("invalid --interval value: {raw}"))?;
            }
            "--history" => {
                let raw = take_value("--history", inline_value, &mut iter)?;
                history_secs = raw
                    .parse()
                    .map_err(|_| format!("invalid --history value: {raw}"))?;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag: {token}"));
            }
            other => {
                return Err(format!("unexpected argument: {other}"));
            }
        }
    }

    if once && soak.is_some() {
        return Err("cannot combine --once and --soak".into());
    }
    if once {
        mode = Mode::Once;
    } else if let Some(secs) = soak {
        mode = Mode::Soak(secs);
    }

    Ok(Ok(Args {
        mode,
        interval_ms,
        history_secs,
    }))
}

fn take_value<I>(flag: &str, inline: Option<&str>, iter: &mut I) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    if let Some(v) = inline {
        if v.is_empty() {
            return Err(format!("missing value for {flag}"));
        }
        return Ok(v.to_string());
    }
    match iter.next() {
        Some(v) if v.starts_with('-') => Err(format!("missing value for {flag}")),
        Some(v) => Ok(v),
        None => Err(format!("missing value for {flag}")),
    }
}

pub fn run() {
    let mut argv = std::env::args();
    let _prog = argv.next(); // skip program name

    let parsed = match parse_args(argv) {
        Ok(inner) => inner,
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Try 'lightwatch --help' for usage.");
            process::exit(1);
        }
    };

    match parsed {
        Err(EarlyExit::Help) => {
            print!("{USAGE}");
            process::exit(0);
        }
        Err(EarlyExit::Version) => {
            println!("lightwatch {}", env!("CARGO_PKG_VERSION"));
            process::exit(0);
        }
        Ok(args) => dispatch(args),
    }
}

fn dispatch(args: Args) {
    let config = match model::HistoryConfig::validate(args.interval_ms, args.history_secs) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            process::exit(1);
        }
    };

    match args.mode {
        Mode::Once => diag::run_once(&config),
        Mode::Soak(seconds) => {
            if seconds > MAX_SOAK_SECONDS {
                eprintln!("Error: --soak value must be <= {MAX_SOAK_SECONDS}s (24 hours)");
                process::exit(1);
            }
            diag::run_soak(&config, seconds);
        }
        Mode::Gui => {
            if let Err(e) = ui::run_gui(config) {
                eprintln!("Error: {e}");
                process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn args(tokens: &[&str]) -> Result<Result<Args, EarlyExit>, String> {
        parse_args(tokens.iter().map(|s| (*s).to_string()))
    }

    fn ok(tokens: &[&str]) -> Args {
        match args(tokens).unwrap() {
            Ok(a) => a,
            Err(e) => panic!("expected Args, got EarlyExit::{e:?}"),
        }
    }

    #[test]
    fn bare_defaults_to_gui() {
        let a = ok(&[]);
        assert_eq!(a.mode, Mode::Gui);
        assert_eq!(a.interval_ms, DEFAULT_INTERVAL_MS);
        assert_eq!(a.history_secs, DEFAULT_HISTORY_SECS);
    }

    #[test]
    fn once_mode() {
        assert_eq!(ok(&["--once"]).mode, Mode::Once);
    }

    #[test]
    fn soak_space_and_equals() {
        assert_eq!(ok(&["--soak", "30"]).mode, Mode::Soak(30));
        assert_eq!(ok(&["--soak=45"]).mode, Mode::Soak(45));
    }

    #[test]
    fn soak_zero_allowed() {
        assert_eq!(ok(&["--soak", "0"]).mode, Mode::Soak(0));
    }

    #[test]
    fn interval_and_history() {
        let a = ok(&["--interval", "500", "--history=120"]);
        assert_eq!(a.interval_ms, 500);
        assert_eq!(a.history_secs, 120);
    }

    #[test]
    fn last_wins_for_valued_flags() {
        let a = ok(&[
            "--interval",
            "200",
            "--interval=400",
            "--history",
            "10",
            "--history",
            "20",
        ]);
        assert_eq!(a.interval_ms, 400);
        assert_eq!(a.history_secs, 20);
    }

    #[test]
    fn once_repeated_stays_once() {
        assert_eq!(ok(&["--once", "--once"]).mode, Mode::Once);
    }

    #[test]
    fn once_and_soak_conflict() {
        let err = args(&["--once", "--soak", "1"]).unwrap_err();
        assert!(err.contains("cannot combine"), "{err}");
    }

    #[test]
    fn unknown_flag() {
        let err = args(&["--bogus"]).unwrap_err();
        assert!(err.contains("unknown flag"), "{err}");
    }

    #[test]
    fn unknown_flag_preserves_inline_form() {
        let err = args(&["--bogus=5"]).unwrap_err();
        assert!(err.contains("--bogus=5"), "{err}");
    }

    #[test]
    fn help_is_positional_not_unconditional() {
        // Intentional: we do not scan ahead for --help the way clap does.
        let err = args(&["--soak", "abc", "--help"]).unwrap_err();
        assert!(err.contains("invalid --soak"), "{err}");
    }

    #[test]
    fn missing_value_at_end() {
        let err = args(&["--interval"]).unwrap_err();
        assert!(err.contains("missing value"), "{err}");
    }

    #[test]
    fn missing_value_when_next_is_flag() {
        let err = args(&["--interval", "--once"]).unwrap_err();
        assert!(err.contains("missing value"), "{err}");
    }

    #[test]
    fn empty_equals_value() {
        let err = args(&["--soak="]).unwrap_err();
        assert!(err.contains("missing value"), "{err}");
    }

    #[test]
    fn non_numeric_value() {
        let err = args(&["--soak", "abc"]).unwrap_err();
        assert!(err.contains("invalid --soak"), "{err}");
    }

    #[test]
    fn bare_token_rejected() {
        let err = args(&["positional"]).unwrap_err();
        assert!(err.contains("unexpected argument"), "{err}");
    }

    #[test]
    fn double_dash_terminator_not_special() {
        let err = args(&["--"]).unwrap_err();
        assert!(err.contains("unknown flag"), "{err}");
    }

    #[test]
    fn help_and_version() {
        assert_eq!(args(&["--help"]).unwrap(), Err(EarlyExit::Help));
        assert_eq!(args(&["-h"]).unwrap(), Err(EarlyExit::Help));
        assert_eq!(args(&["--version"]).unwrap(), Err(EarlyExit::Version));
        assert_eq!(args(&["-V"]).unwrap(), Err(EarlyExit::Version));
    }

    #[test]
    fn help_rejects_inline_value() {
        let err = args(&["--help=true"]).unwrap_err();
        assert!(err.contains("unexpected value"), "{err}");
    }
}
