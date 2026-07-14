use crate::model::*;
use crate::parse::parse_self_stat;

/// Collector for self (lightwatch process) metrics.
pub struct SelfCollector {
    proc_root: String,
    prev_cpu_ticks: Option<u64>, // utime + stime from previous sample
    prev_time_ns: Option<u64>,   // boottime from previous sample for CPU% calc
    start_time: u64,             // boottime at construction for uptime
}

impl SelfCollector {
    pub fn new(proc_root: &str) -> Self {
        Self {
            proc_root: proc_root.to_string(),
            prev_cpu_ticks: None,
            prev_time_ns: None,
            start_time: crate::clock_boottime_ns(),
        }
    }

    /// Clear delta baselines so the next sample starts fresh (used after
    /// a discontinuity like suspend/resume).
    pub fn clear_baseline(&mut self) {
        self.prev_cpu_ticks = None;
        self.prev_time_ns = None;
    }

    pub fn sample(&mut self, sample_duration_us: u64, overruns: u64, skipped: u64) -> SelfSnapshot {
        let now_ns = crate::clock_boottime_ns();
        let uptime_secs = (now_ns.saturating_sub(self.start_time)) / 1_000_000_000;

        let stat_path = format!("{}/self/stat", self.proc_root);
        let stat_content = std::fs::read_to_string(&stat_path).unwrap_or_default();
        let self_stat = parse_self_stat(&stat_content).ok();

        let rss_kb = match &self_stat {
            Some(s) => {
                // rss_pages * page_size (usually 4KiB), but /proc/self/stat Rss is in pages
                // page_size is usually 4096 on x86_64
                let pagesize = page_size();
                Reading::Value(s.rss_pages * pagesize as u64 / 1024)
            }
            None => Reading::Unavailable {
                reason: "cannot parse self stat",
            },
        };

        let cpu_percent = match (&self_stat, self.prev_cpu_ticks, self.prev_time_ns) {
            (Some(curr), Some(prev_ticks), Some(prev_ns)) => {
                let curr_ticks = curr.utime + curr.stime;
                let time_delta_ns = now_ns.saturating_sub(prev_ns);
                if time_delta_ns > 0 && curr_ticks >= prev_ticks {
                    let tick_delta = curr_ticks - prev_ticks;
                    // Convert ticks to seconds: ticks / CLK_TCK (usually 100)
                    let clk_tck = clock_ticks_per_sec();
                    let cpu_secs = tick_delta as f64 / clk_tck as f64;
                    let wall_secs = time_delta_ns as f64 / 1_000_000_000.0;
                    Reading::Value((cpu_secs / wall_secs * 100.0) as f32)
                } else {
                    Reading::Unavailable {
                        reason: "counter decrease or zero time",
                    }
                }
            }
            _ => Reading::Unavailable {
                reason: "no self CPU baseline",
            },
        };

        // Update prev
        if let Some(s) = &self_stat {
            self.prev_cpu_ticks = Some(s.utime + s.stime);
            self.prev_time_ns = Some(now_ns);
        }

        SelfSnapshot {
            rss_kb,
            cpu_percent,
            uptime_secs,
            sample_duration_us,
            sampler_overruns: overruns,
            ticks_skipped: skipped,
        }
    }
}

fn page_size() -> usize {
    // On Linux, we can get this via sysconf or just assume 4096
    // We'll try sysconf first
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz > 0 { sz as usize } else { 4096 }
}

fn clock_ticks_per_sec() -> u64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as u64
    } else {
        100 // common default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_collector_smoke() {
        let mut collector = SelfCollector::new("/proc");
        let snap = collector.sample(0, 0, 0);
        // First call: no CPU baseline yet
        assert!(matches!(snap.cpu_percent, Reading::Unavailable { .. }));
    }
}
