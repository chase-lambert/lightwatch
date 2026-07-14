use super::latest::{Latest, Published};
use crate::collect::gpu::{self, GpuDevice, amd, nvidia};
use crate::collect::{cpu::CpuCollector, mem::MemCollector, self_metrics::SelfCollector};
use crate::model::*;
use core::time::Duration;
use std::sync::Mutex;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, mpsc};
use std::time::Instant;

/// Pure function: advance a deadline to the first future slot ≥ `now_ns`.
/// Returns the new deadline and the number of skipped intervals.
/// Uses arithmetic division (not a while-loop) to avoid infinite-loop risk
/// at `u128::MAX` with `saturating_add`.
///
/// All three parameters are expressed as integer nanoseconds from some
/// common epoch to keep the function pure and testable without `Instant`.
pub fn advance_deadline(now_ns: u128, deadline_ns: u128, interval_ns: u128) -> (u128, u64) {
    if interval_ns == 0 {
        return (deadline_ns, 0);
    }
    if now_ns < deadline_ns {
        return (deadline_ns, 0);
    }
    // ── arithmetic skip: (now − deadline) / interval + 1 ────────
    let delta = now_ns - deadline_ns;
    // skips = 1 + floor(delta / interval)
    let skips = delta
        .checked_div(interval_ns)
        .unwrap_or(0)
        .saturating_add(1);
    // next = deadline + skips · interval (saturating in case of very-large values)
    let next = deadline_ns.saturating_add(skips.saturating_mul(interval_ns));
    // Clamp skips to u64 (even with a huge delta, u64 is enough to represent ~584 billion years)
    let skips_u64 = if skips > u64::MAX as u128 {
        u64::MAX
    } else {
        skips as u64
    };
    (next, skips_u64)
}

/// The sampler worker: owns the history, collects samples on a deadline schedule,
/// and publishes to the Latest cell.
pub struct Sampler {
    config: HistoryConfig,
    latest: Arc<Latest>,
    notify: mpsc::SyncSender<()>,
    pending_config: Arc<Mutex<Option<HistoryConfig>>>,
    cpu_collector: CpuCollector,
    mem_collector: MemCollector,
    self_collector: SelfCollector,
    history: History,
    gpu_devices: Vec<GpuDevice>,
    seq: u64,
    overruns: u64,
    skipped: u64,
    last_discovery_ns: u64,      // boottime of last GPU discovery
    prev_t_boot_ns: Option<u64>, // previous sample's boottime for gap detection
    #[allow(dead_code)]
    core_count: usize,
}

impl Sampler {
    pub fn new(
        config: HistoryConfig,
        latest: Arc<Latest>,
        notify: SyncSender<()>,
        pending_config: Arc<Mutex<Option<HistoryConfig>>>,
    ) -> Self {
        let core_count = detect_core_count(&config);
        let gpu_devices = gpu::discover("/sys");
        let gpu_ids: Vec<String> = gpu_devices.iter().map(|d| d.pci_id.clone()).collect();
        let history = History::new(&config, core_count, &gpu_ids);

        Sampler {
            cpu_collector: CpuCollector::new("/proc", "/sys"),
            mem_collector: MemCollector::new("/proc"),
            self_collector: SelfCollector::new("/proc"),
            config,
            latest,
            notify,
            pending_config,
            history,
            gpu_devices,
            seq: 0,
            overruns: 0,
            skipped: 0,
            last_discovery_ns: 0,
            prev_t_boot_ns: None,
            core_count,
        }
    }

    /// Run the sampler in a loop. Designed to run on its own thread.
    /// Returns when the notify channel is closed (receiver dropped by UI teardown).
    pub fn run(&mut self) {
        let start_epoch = Instant::now();
        let mut interval_ns = self.config.interval.as_nanos();
        // First sample after one interval (allows a baseline for deltas).
        let mut deadline_ns = interval_ns;

        loop {
            // Sleep until the planned deadline when early. If already late,
            // jump to the next future slot and count real missed ticks only.
            let now_ns = start_epoch.elapsed().as_nanos();
            if now_ns < deadline_ns {
                let sleep_ns = deadline_ns - now_ns;
                // Interval is capped at 60s; cast is safe for sleep duration.
                std::thread::sleep(Duration::from_nanos(sleep_ns as u64));
            } else if now_ns > deadline_ns {
                let (next_dl, skips) = advance_deadline(now_ns, deadline_ns, interval_ns);
                self.skipped = self.skipped.saturating_add(skips);
                deadline_ns = next_dl;
                // We advanced to a *future* deadline; sleep the remainder if any.
                let now2 = start_epoch.elapsed().as_nanos();
                if deadline_ns > now2 {
                    std::thread::sleep(Duration::from_nanos((deadline_ns - now2) as u64));
                }
            }
            // now_ns == deadline_ns: sample immediately for this slot.

            // Process any pending config update from the UI (shared slot).
            {
                let mut guard = self.pending_config.lock().unwrap();
                if let Some(new_config) = guard.take() {
                    drop(guard);
                    match self.history.resize(new_config.capacity) {
                        Ok(()) => {
                            self.config = new_config;
                            interval_ns = self.config.interval.as_nanos();
                        }
                        Err(e) => {
                            eprintln!("sampler: config resize failed: {e} — keeping previous");
                        }
                    }
                }
            }

            let sample_start = Instant::now();

            // ── read boottime early for discontinuity detection ──
            let t_boot_start = crate::clock_boottime_ns();

            // Periodic discovery: every ~60s of wall-clock time (boottime-based)
            const DISCOVERY_INTERVAL_NS: u64 = 60_000_000_000; // 60s
            if t_boot_start.saturating_sub(self.last_discovery_ns) >= DISCOVERY_INTERVAL_NS {
                self.last_discovery_ns = t_boot_start;
                self.rescan_gpus();
            }

            // Detect discontinuity (suspend, large clock jump) BEFORE collecting
            // deltas. If we clear baselines here, this tick's CPU/self percentages
            // will be Unavailable rather than a spike. Mem/GPU are stateless and
            // collected anyway.
            let interval_ns_u64 = self.config.interval.as_nanos() as u64;
            let k: u64 = 5;
            if let Some(prev) = self.prev_t_boot_ns {
                let gap_ns = t_boot_start.saturating_sub(prev);
                if gap_ns > k * interval_ns_u64 {
                    // Push a gap marker into every ring
                    let gap_t = prev.saturating_add(interval_ns_u64);
                    push_gap_to_all_rings(&mut self.history, gap_t);
                    // Clear baselines before sampling
                    self.cpu_collector.clear_baseline();
                    self.self_collector.clear_baseline();
                }
            }

            // Collect
            let cpu_snap = self.cpu_collector.sample();
            let mem_snap = self.mem_collector.sample();
            let self_snap = self.self_collector.sample(0, self.overruns, self.skipped);

            // Sample GPUs
            let mut gpu_snaps = Vec::new();
            for dev in &self.gpu_devices {
                let snap = match dev.driver.as_str() {
                    "amdgpu" => amd::sample_amd(dev),
                    "nvidia" => nvidia::sample_nvidia(dev),
                    _ => GpuSnapshot {
                        pci_id: dev.pci_id.clone(),
                        vendor_id: dev.vendor_id.clone(),
                        device_id: dev.device_id.clone(),
                        driver: dev.driver.clone(),
                        name: dev.name.clone(),
                        util_percent: Reading::Unavailable {
                            reason: "unknown driver",
                        },
                        vram_total_kb: Reading::Unavailable {
                            reason: "unknown driver",
                        },
                        vram_used_kb: Reading::Unavailable {
                            reason: "unknown driver",
                        },
                        temp_celsius: Reading::Unavailable {
                            reason: "unknown driver",
                        },
                        power_watts: Reading::Unavailable {
                            reason: "unknown driver",
                        },
                    },
                };
                gpu_snaps.push(snap);
            }

            self.seq += 1;
            let t_boot_ns = crate::clock_boottime_ns();
            let sample_dur = sample_start.elapsed();
            self.prev_t_boot_ns = Some(t_boot_ns);

            let snapshot = Snapshot {
                seq: self.seq,
                t_boot_ns,
                sample_duration_us: sample_dur.as_micros() as u64,
                sampler_overruns: self.overruns,
                ticks_skipped: self.skipped,
                cpu: cpu_snap,
                memory: mem_snap,
                gpus: gpu_snaps,
                self_metrics: self_snap,
            };

            // Push to history rings
            if let Reading::Value(v) = snapshot.cpu.usage_percent {
                self.history.cpu_total.push(SamplePoint::new(t_boot_ns, v));
            } else {
                self.history.cpu_total.push(SamplePoint::gap(t_boot_ns));
            }

            // Per-core
            self.history.cpu_per_core.resize(
                snapshot.cpu.per_core_percent.len(),
                Ring::new(self.config.capacity),
            );
            for (i, core_pct) in snapshot.cpu.per_core_percent.iter().enumerate() {
                if i < self.history.cpu_per_core.len() {
                    match core_pct {
                        Reading::Value(v) => {
                            self.history.cpu_per_core[i].push(SamplePoint::new(t_boot_ns, *v))
                        }
                        _ => self.history.cpu_per_core[i].push(SamplePoint::gap(t_boot_ns)),
                    }
                }
            }

            // CPU temp
            match &snapshot.cpu.temp_celsius {
                Reading::Value(v) => self.history.cpu_temp.push(SamplePoint::new(t_boot_ns, *v)),
                _ => self.history.cpu_temp.push(SamplePoint::gap(t_boot_ns)),
            }

            // CPU freq
            match &snapshot.cpu.freq_mhz {
                Reading::Value(v) => self.history.cpu_freq.push(SamplePoint::new(t_boot_ns, *v)),
                _ => self.history.cpu_freq.push(SamplePoint::gap(t_boot_ns)),
            }

            // Memory
            match &snapshot.memory.used_kb {
                Reading::Value(v) => self
                    .history
                    .mem_used
                    .push(SamplePoint::new(t_boot_ns, *v as f32)),
                _ => self.history.mem_used.push(SamplePoint::gap(t_boot_ns)),
            }
            match &snapshot.memory.swap_used_kb {
                Reading::Value(v) => self
                    .history
                    .swap_used
                    .push(SamplePoint::new(t_boot_ns, *v as f32)),
                _ => self.history.swap_used.push(SamplePoint::gap(t_boot_ns)),
            }

            // Load
            match &snapshot.memory.load_1min {
                Reading::Value(v) => self.history.load1.push(SamplePoint::new(t_boot_ns, *v)),
                _ => self.history.load1.push(SamplePoint::gap(t_boot_ns)),
            }
            match &snapshot.memory.load_5min {
                Reading::Value(v) => self.history.load5.push(SamplePoint::new(t_boot_ns, *v)),
                _ => self.history.load5.push(SamplePoint::gap(t_boot_ns)),
            }
            match &snapshot.memory.load_15min {
                Reading::Value(v) => self.history.load15.push(SamplePoint::new(t_boot_ns, *v)),
                _ => self.history.load15.push(SamplePoint::gap(t_boot_ns)),
            }

            // Self metrics
            match &snapshot.self_metrics.rss_kb {
                Reading::Value(v) => self
                    .history
                    .self_rss
                    .push(SamplePoint::new(t_boot_ns, *v as f32)),
                _ => self.history.self_rss.push(SamplePoint::gap(t_boot_ns)),
            }
            match &snapshot.self_metrics.cpu_percent {
                Reading::Value(v) => self.history.self_cpu.push(SamplePoint::new(t_boot_ns, *v)),
                _ => self.history.self_cpu.push(SamplePoint::gap(t_boot_ns)),
            }

            // GPU histories — match by pci_id, not vector index
            for gsnap in &snapshot.gpus {
                if let Some(gh) = self
                    .history
                    .gpu_series
                    .iter_mut()
                    .find(|g| g.pci_id == gsnap.pci_id)
                {
                    match &gsnap.util_percent {
                        Reading::Value(v) => gh.util.push(SamplePoint::new(t_boot_ns, *v)),
                        _ => gh.util.push(SamplePoint::gap(t_boot_ns)),
                    }
                    match &gsnap.vram_used_kb {
                        Reading::Value(v) => {
                            gh.vram_used.push(SamplePoint::new(t_boot_ns, *v as f32))
                        }
                        _ => gh.vram_used.push(SamplePoint::gap(t_boot_ns)),
                    }
                    match &gsnap.temp_celsius {
                        Reading::Value(v) => gh.temp.push(SamplePoint::new(t_boot_ns, *v)),
                        _ => gh.temp.push(SamplePoint::gap(t_boot_ns)),
                    }
                    match &gsnap.power_watts {
                        Reading::Value(v) => gh.power.push(SamplePoint::new(t_boot_ns, *v)),
                        _ => gh.power.push(SamplePoint::gap(t_boot_ns)),
                    }
                }
            }

            // Publish
            let published = Arc::new(Published {
                snapshot,
                history: self.history.clone(),
            });
            self.latest.publish(published);
            let _ = self.notify.try_send(()); // ignore Full — consumer will pull latest anyway

            // Check for overrun
            let elapsed = sample_start.elapsed();
            if elapsed > self.config.interval {
                self.overruns += 1;
            }

            // Advance deadline by one interval for the next tick.
            // Missed deadlines (e.g. from work overrun) are counted by
            // advance_deadline at the top of the next iteration.
            deadline_ns = deadline_ns.saturating_add(interval_ns);
        }
    }

    fn rescan_gpus(&mut self) {
        let devices = gpu::discover("/sys");
        // Reconcile: keep current GpuDevice structs but add/remove
        let new_ids: Vec<String> = devices.iter().map(|d| d.pci_id.clone()).collect();
        self.history.reconcile_gpus(&self.config, &new_ids);
        self.gpu_devices = devices;
    }
}

/// Push a gap point (value = None) into every ring in the history at the given
/// timestamp. Used to mark a discontinuity in the sparklines.
fn push_gap_to_all_rings(history: &mut History, t_ns: u64) {
    history.cpu_total.push(SamplePoint::gap(t_ns));
    for ring in &mut history.cpu_per_core {
        ring.push(SamplePoint::gap(t_ns));
    }
    history.cpu_temp.push(SamplePoint::gap(t_ns));
    history.cpu_freq.push(SamplePoint::gap(t_ns));
    history.mem_used.push(SamplePoint::gap(t_ns));
    history.swap_used.push(SamplePoint::gap(t_ns));
    history.load1.push(SamplePoint::gap(t_ns));
    history.load5.push(SamplePoint::gap(t_ns));
    history.load15.push(SamplePoint::gap(t_ns));
    history.self_rss.push(SamplePoint::gap(t_ns));
    history.self_cpu.push(SamplePoint::gap(t_ns));
    for gpu_hist in &mut history.gpu_series {
        gpu_hist.util.push(SamplePoint::gap(t_ns));
        gpu_hist.vram_used.push(SamplePoint::gap(t_ns));
        gpu_hist.temp.push(SamplePoint::gap(t_ns));
        gpu_hist.power.push(SamplePoint::gap(t_ns));
    }
}

fn detect_core_count(_config: &HistoryConfig) -> usize {
    // Parse /proc/stat to count cpuN lines
    let content = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut count = 0;
    for line in content.lines() {
        if line.starts_with("cpu") && !line.starts_with("cpu ") {
            // Must have a digit after "cpu"
            let rest = &line[3..];
            if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                count += 1;
            }
        }
    }
    count.max(1)
}

/// Run a single sample collection for diagnostic modes (--once, --soak).
/// Returns `Ok(Snapshot)` if core sources (/proc/stat, /proc/meminfo) are
/// readable and parseable. Returns `Err(reason)` if a core source is
/// fundamentally unreadable, regardless of delta availability.
pub fn sample_once(config: &HistoryConfig) -> Result<Snapshot, String> {
    // ═══ pre-check core sources before delta collection ═══
    // Exit status is based on read/parse health, not on whether deltas
    // produce values (e.g. first-sample "no baseline" is OK).
    let stat_content = std::fs::read_to_string("/proc/stat")
        .map_err(|e| format!("cannot read /proc/stat: {e}"))?;
    if crate::parse::parse_proc_stat(&stat_content).is_err() {
        return Err("/proc/stat unparseable".into());
    }

    let meminfo_content = std::fs::read_to_string("/proc/meminfo")
        .map_err(|e| format!("cannot read /proc/meminfo: {e}"))?;
    let meminfo = crate::parse::parse_meminfo(&meminfo_content);
    if !meminfo.has_required() {
        return Err("/proc/meminfo missing MemTotal or MemAvailable".into());
    }

    // Core sources are healthy — proceed with delta collection.
    let mut cpu = CpuCollector::new("/proc", "/sys");
    let mem = MemCollector::new("/proc");
    let mut self_coll = SelfCollector::new("/proc");
    let gpu_devices = gpu::discover("/sys");

    // Take baseline, wait, take second sample for deltas
    let _baseline = cpu.sample(); // prime the baseline
    let _baseline_self = self_coll.sample(0, 0, 0);

    std::thread::sleep(config.interval);

    let cpu_snap = cpu.sample();
    let mem_snap = mem.sample();
    let self_snap = self_coll.sample(0, 0, 0);

    let mut gpu_snaps = Vec::new();
    for dev in &gpu_devices {
        let snap = match dev.driver.as_str() {
            "amdgpu" => amd::sample_amd(dev),
            "nvidia" => nvidia::sample_nvidia(dev),
            _ => GpuSnapshot {
                pci_id: dev.pci_id.clone(),
                vendor_id: dev.vendor_id.clone(),
                device_id: dev.device_id.clone(),
                driver: dev.driver.clone(),
                name: dev.name.clone(),
                util_percent: Reading::Unavailable {
                    reason: "unknown driver",
                },
                vram_total_kb: Reading::Unavailable {
                    reason: "unknown driver",
                },
                vram_used_kb: Reading::Unavailable {
                    reason: "unknown driver",
                },
                temp_celsius: Reading::Unavailable {
                    reason: "unknown driver",
                },
                power_watts: Reading::Unavailable {
                    reason: "unknown driver",
                },
            },
        };
        gpu_snaps.push(snap);
    }

    Ok(Snapshot {
        seq: 1,
        t_boot_ns: crate::clock_boottime_ns(),
        sample_duration_us: 0,
        sampler_overruns: 0,
        ticks_skipped: 0,
        cpu: cpu_snap,
        memory: mem_snap,
        gpus: gpu_snaps,
        self_metrics: self_snap,
    })
}

/// Run a headless soak test: sample at interval for a total duration, print summary lines.
pub fn run_soak(config: &HistoryConfig, total_seconds: u64) {
    let mut cpu = CpuCollector::new("/proc", "/sys");
    let mem = MemCollector::new("/proc");
    let mut self_coll = SelfCollector::new("/proc");
    // baseline
    let _ = cpu.sample();
    let _ = self_coll.sample(0, 0, 0);

    let start = std::time::Instant::now();
    // Use checked arithmetic: the CLI validator already bounds `total_seconds`
    // to ≤ 86400, so this is a belt-and-suspenders guard.
    let end = start
        .checked_add(Duration::from_secs(total_seconds))
        .expect("soak duration overflow");

    let mut sample_count: u64 = 0;
    let mut sum_rss: u64 = 0;
    let mut sum_self_cpu: f32 = 0.0;
    let mut sum_cpu: f32 = 0.0;
    let mut max_rss: u64 = 0;

    println!(
        "soak: sampling every {:?} for {}s ...",
        config.interval, total_seconds
    );

    while Instant::now() < end {
        let cpu_snap = cpu.sample();
        let _mem_snap = mem.sample();
        let self_snap = self_coll.sample(0, 0, 0);

        sample_count += 1;

        if let Reading::Value(rss) = self_snap.rss_kb {
            sum_rss += rss;
            max_rss = max_rss.max(rss);
            print!("  [{:>3}] rss={:>8} kB", sample_count, rss);
        }
        if let Reading::Value(sc) = self_snap.cpu_percent {
            sum_self_cpu += sc;
            print!("  self_cpu={:.1}%", sc);
        }
        if let Reading::Value(c) = cpu_snap.usage_percent {
            sum_cpu += c;
            print!("  cpu={:.1}%", c);
        }
        println!();

        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() || remaining < config.interval {
            if !remaining.is_zero() {
                std::thread::sleep(remaining);
            }
            break;
        }
        std::thread::sleep(config.interval);
    }

    let avg_rss = if sample_count > 0 {
        sum_rss / sample_count
    } else {
        0
    };
    let avg_self_cpu = if sample_count > 0 {
        sum_self_cpu / sample_count as f32
    } else {
        0.0
    };
    let avg_cpu = if sample_count > 0 {
        sum_cpu / sample_count as f32
    } else {
        0.0
    };

    println!("--- soak summary ---");
    println!("  samples:     {sample_count}");
    println!(
        "  avg_rss:     {avg_rss} kB  ({:.1} MiB)",
        avg_rss as f64 / 1024.0
    );
    println!(
        "  max_rss:     {max_rss} kB  ({:.1} MiB)",
        max_rss as f64 / 1024.0
    );
    println!("  avg_self_cpu: {avg_self_cpu:.2}%");
    println!("  avg_sys_cpu:  {avg_cpu:.2}%");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 s interval expressed in ns.
    const I_NS: u128 = 1_000_000_000;

    #[test]
    fn advance_deadline_already_future() {
        // deadline is already ahead — no skips
        let (next, skips) = advance_deadline(0, 10 * I_NS, I_NS);
        assert_eq!(next, 10 * I_NS);
        assert_eq!(skips, 0);
    }

    #[test]
    fn advance_deadline_single_miss() {
        // now = 1.2s, deadline = 1s, interval = 1s
        // should advance deadline by one interval: 2s, skip=1
        let (next, skips) = advance_deadline(12 * I_NS / 10, I_NS, I_NS);
        assert_eq!(next, 2 * I_NS);
        assert_eq!(skips, 1);
    }

    #[test]
    fn advance_deadline_multi_miss() {
        // now = 5s, deadline = 1s, interval = 1s
        // should skip to 6s (5 intervals skipped: 2,3,4,5,6)
        let (next, skips) = advance_deadline(5 * I_NS, I_NS, I_NS);
        assert_eq!(next, 6 * I_NS);
        assert_eq!(skips, 5);
    }

    #[test]
    fn advance_deadline_exactly_on_deadline() {
        // now == deadline — should advance by one interval
        let (next, skips) = advance_deadline(5 * I_NS, 5 * I_NS, I_NS);
        assert_eq!(next, 6 * I_NS);
        assert_eq!(skips, 1);
    }

    #[test]
    fn advance_deadline_large_jump() {
        // Simulate a suspend-like jump: now = 100s, deadline = 1s
        // Should skip 99 intervals: deadline advances to 100s, then 101s
        let (next, skips) = advance_deadline(100 * I_NS, I_NS, I_NS);
        assert_eq!(next, 101 * I_NS);
        assert_eq!(skips, 100);
    }

    #[test]
    fn advance_deadline_zero_interval() {
        // Zero interval is a guard case — should just return unchanged
        let (next, skips) = advance_deadline(100, 50, 0);
        assert_eq!(next, 50);
        assert_eq!(skips, 0);
    }
}
