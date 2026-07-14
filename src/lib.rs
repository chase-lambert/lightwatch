use clap::Parser;
use std::process;

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

#[derive(Parser, Debug)]
#[command(
    name = "lightwatch",
    about = "Native Linux system monitor with bounded resource use"
)]
struct Cli {
    /// Print one snapshot after waiting for deltas, then exit
    #[arg(long)]
    once: bool,

    /// Headless soak test for N seconds
    #[arg(long)]
    soak: Option<u64>,

    /// Sampling interval in milliseconds
    #[arg(long, default_value = "1000")]
    interval: u64,

    /// History window in seconds
    #[arg(long, default_value = "900")]
    history: u64,
}

pub fn run() {
    let cli = Cli::parse();

    // Validate history config
    let config = match model::HistoryConfig::validate(cli.interval, cli.history) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            process::exit(1);
        }
    };

    if cli.once {
        diag::run_once(&config);
    } else if let Some(seconds) = cli.soak {
        // Reject absurd soak durations (> 1 day) to prevent overflow
        const MAX_SOAK_SECONDS: u64 = 86400; // 1 day
        if seconds > MAX_SOAK_SECONDS {
            eprintln!("Error: --soak value must be <= {MAX_SOAK_SECONDS}s (24 hours)");
            process::exit(1);
        }
        diag::run_soak(&config, seconds);
    } else {
        // GUI mode
        if let Err(e) = ui::run_gui(config) {
            eprintln!("Error: {e}");
            process::exit(1);
        }
    }
}
