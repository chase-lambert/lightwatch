//! Diagnostic CLI modes: --once and --soak.

use crate::model::*;
use crate::sample::worker;
use std::process;

/// Print one snapshot after waiting one interval for deltas.
/// Exit code is based on whether core sources (/proc/stat, /proc/meminfo)
/// are readable and parseable — not on delta availability (e.g. first
/// sample's "no baseline" CPU% is expected and not an error).
pub fn run_once(config: &HistoryConfig) {
    match worker::sample_once(config) {
        Ok(snap) => {
            print_snapshot(&snap);
            process::exit(0);
        }
        Err(e) => {
            eprintln!("Error: {e}");
            process::exit(1);
        }
    }
}

/// Headless soak test.
pub fn run_soak(config: &HistoryConfig, seconds: u64) {
    worker::run_soak(config, seconds);
    process::exit(0);
}

fn print_snapshot(snap: &Snapshot) {
    println!("=== lightwatch snapshot (seq={}) ===", snap.seq);
    println!();

    // CPU
    println!("CPU:");
    print!("  usage:    ");
    println_reading(&snap.cpu.usage_percent, |v| format!("{v:.1}%"));
    match &snap.cpu.temp_celsius {
        Reading::Value(v) => println!("  temp:     {v:.1}°C"),
        Reading::Unavailable { reason } => println!("  temp:     unavailable ({reason})"),
    }
    match &snap.cpu.freq_mhz {
        Reading::Value(v) => println!("  freq:     {v:.0} MHz"),
        Reading::Unavailable { reason } => println!("  freq:     unavailable ({reason})"),
    }
    println!();

    // Memory
    println!("Memory:");
    print!("  used:     ");
    println_reading(&snap.memory.used_kb, |v| {
        format!("{} kB ({:.1} GiB)", v, *v as f64 / 1_048_576.0)
    });
    print!("  available:");
    println_reading(&snap.memory.available_kb, |v| {
        format!("{} kB ({:.1} GiB)", v, *v as f64 / 1_048_576.0)
    });
    print!("  swap:     ");
    println_reading(&snap.memory.swap_used_kb, |v| {
        format!("{} kB ({:.1} GiB)", v, *v as f64 / 1_048_576.0)
    });
    println!(
        "  load:     {}/{}/{}",
        fmt_load(&snap.memory.load_1min),
        fmt_load(&snap.memory.load_5min),
        fmt_load(&snap.memory.load_15min),
    );
    println!();

    // GPUs
    for gpu in &snap.gpus {
        println!("GPU [{}]:", gpu.pci_id);
        println!("  driver:   {}", gpu.driver);
        print!("  util:     ");
        println_reading(&gpu.util_percent, |v| format!("{v:.1}%"));
        print!("  VRAM:     ");
        match (&gpu.vram_used_kb, &gpu.vram_total_kb) {
            (Reading::Value(u), Reading::Value(t)) => {
                println!("{} / {} KiB ({:.1}%)", u, t, *u as f64 / *t as f64 * 100.0);
            }
            (Reading::Value(u), _) => println!("{} KiB (total unknown)", u),
            _ => println!("unavailable"),
        }
        print!("  temp:     ");
        println_reading(&gpu.temp_celsius, |v| format!("{v:.1}°C"));
        print!("  power:    ");
        println_reading(&gpu.power_watts, |v| format!("{v:.1} W"));
        println!();
    }

    // Self
    println!("Self:");
    print!("  RSS Anon: ");
    println_reading(&snap.self_metrics.rss_anon_kb, |v| {
        format!("{} kB ({:.1} MiB)", v, *v as f64 / 1024.0)
    });
    print!("  RSS (Vm): ");
    println_reading(&snap.self_metrics.rss_kb, |v| {
        format!("{} kB ({:.1} MiB)", v, *v as f64 / 1024.0)
    });
    print!("  CPU:      ");
    println_reading(&snap.self_metrics.cpu_percent, |v| format!("{v:.1}%"));
    println!("  uptime:   {}s", snap.self_metrics.uptime_secs);
    println!("  duration: {} µs", snap.sample_duration_us);
    println!("  overruns: {}", snap.sampler_overruns);
    println!("  skipped:  {}", snap.ticks_skipped);
}

fn println_reading<T: std::fmt::Display, F: FnOnce(&T) -> String>(
    reading: &Reading<T>,
    fmt_val: F,
) {
    match reading {
        Reading::Value(v) => println!("{}", fmt_val(v)),
        Reading::Unavailable { reason } => println!("unavailable ({reason})"),
    }
}

fn fmt_load(r: &Reading<f32>) -> String {
    match r {
        Reading::Value(v) => format!("{v:.2}"),
        Reading::Unavailable { .. } => "n/a".to_string(),
    }
}
