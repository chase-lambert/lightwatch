use crate::model::*;
use crate::parse::{parse_self_stat, parse_self_status};

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

        // ── /proc/self/stat for CPU time ──
        let stat_path = format!("{}/self/stat", self.proc_root);
        let stat_content = std::fs::read_to_string(&stat_path).unwrap_or_default();
        let self_stat = parse_self_stat(&stat_content).ok();

        // ── /proc/self/status for memory (VmRSS + RssAnon) ──
        let status_path = format!("{}/self/status", self.proc_root);
        let status_content = std::fs::read_to_string(&status_path).unwrap_or_default();
        let self_status = parse_self_status(&status_content).ok();

        // Total resident (VmRSS) from either source.
        let rss_kb = self_status
            .as_ref()
            .and_then(|status| status.vm_rss_kb)
            .map(Reading::Value)
            .or_else(|| {
                self_stat
                    .as_ref()
                    .map(|stat| Reading::Value(stat.rss_pages * page_size() as u64 / 1024))
            })
            .unwrap_or(Reading::Unavailable {
                reason: "VmRSS unavailable in self status and stat",
            });

        // Anonymous resident memory — only from /proc/self/status.
        let rss_anon_kb = match &self_status {
            Some(s) => match s.rss_anon_kb {
                Some(v) => Reading::Value(v),
                None => Reading::Unavailable {
                    reason: "RssAnon missing in self status",
                },
            },
            None => Reading::Unavailable {
                reason: "cannot parse self status",
            },
        };

        let cpu_percent = match (&self_stat, self.prev_cpu_ticks, self.prev_time_ns) {
            (Some(curr), Some(prev_ticks), Some(prev_ns)) => {
                let curr_ticks = curr.utime + curr.stime;
                let time_delta_ns = now_ns.saturating_sub(prev_ns);
                if time_delta_ns > 0 && curr_ticks >= prev_ticks {
                    let tick_delta = curr_ticks - prev_ticks;
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
            rss_anon_kb,
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
mod collector_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const VALID_STAT: &str = "12345 (lightwatch) S 1234 1234 1234 0 -1 4194560 123 0 0 0 150 25 0 0 20 0 8 0 123456 789012 456 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct ProcFixture {
        root: PathBuf,
    }

    impl ProcFixture {
        fn new(stat: Option<&str>, status: Option<&str>) -> Self {
            let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "lightwatch-self-metrics-{}-{id}",
                std::process::id()
            ));
            let self_dir = root.join("self");
            std::fs::create_dir_all(&self_dir).unwrap();
            if let Some(content) = stat {
                std::fs::write(self_dir.join("stat"), content).unwrap();
            }
            if let Some(content) = status {
                std::fs::write(self_dir.join("status"), content).unwrap();
            }
            Self { root }
        }

        fn root(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for ProcFixture {
        fn drop(&mut self) {
            let self_dir = self.root.join("self");
            let _ = std::fs::remove_file(self_dir.join("stat"));
            let _ = std::fs::remove_file(self_dir.join("status"));
            let _ = std::fs::remove_dir(&self_dir);
            let _ = std::fs::remove_dir(&self.root);
        }
    }

    #[test]
    fn self_collector_smoke() {
        let mut collector = SelfCollector::new("/proc");
        let snap = collector.sample(0, 0, 0);
        assert!(matches!(snap.cpu_percent, Reading::Unavailable { .. }));
        assert!(snap.rss_kb.is_available(), "VmRSS should be available");
        assert!(
            snap.rss_anon_kb.is_available(),
            "RssAnon should be available"
        );
    }

    #[test]
    fn collector_stat_fallback_when_status_unavailable() {
        let fixture = ProcFixture::new(Some(VALID_STAT), None);
        let mut collector = SelfCollector::new(fixture.root().to_str().unwrap());
        let snap = collector.sample(0, 0, 0);
        assert_eq!(snap.rss_kb, Reading::Value(456 * page_size() as u64 / 1024));
        assert!(!snap.rss_anon_kb.is_available());
    }

    #[test]
    fn collector_cpu_unavailable_without_baseline() {
        let mut collector = SelfCollector::new("/proc");
        let snap1 = collector.sample(0, 0, 0);
        assert!(
            !snap1.cpu_percent.is_available(),
            "no baseline on first call"
        );
        let _snap2 = collector.sample(0, 0, 0);
        // Second call may have a baseline.
    }

    #[test]
    fn status_memory_survives_when_stat_cpu_absent() {
        let fixture = ProcFixture::new(None, Some("VmRSS: 200 kB\nRssAnon: 100 kB\n"));
        let mut c = SelfCollector::new(fixture.root().to_str().unwrap());
        let snap = c.sample(0, 0, 0);
        assert!(matches!(snap.cpu_percent, Reading::Unavailable { .. }));
        assert_eq!(snap.rss_kb, Reading::Value(200));
        assert_eq!(snap.rss_anon_kb, Reading::Value(100));
    }

    #[test]
    fn asymmetric_status_fields_one_missing() {
        let content = "VmRSS:   123456 kB\nName:   test\n";
        let s = crate::parse::parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, Some(123456));
        assert_eq!(s.rss_anon_kb, None);
    }

    #[test]
    fn asymmetric_status_fields_one_malformed() {
        let content = "VmRSS:   abc kB\nRssAnon:   100 kB\n";
        let s = crate::parse::parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, None);
        assert_eq!(s.rss_anon_kb, Some(100));
    }

    #[test]
    fn stat_fallback_preserves_independent_rss_anon() {
        let fixture = ProcFixture::new(
            Some(VALID_STAT),
            Some("VmRSS: malformed kB\nRssAnon: 100 kB\n"),
        );
        let mut collector = SelfCollector::new(fixture.root().to_str().unwrap());
        let snap = collector.sample(0, 0, 0);
        assert_eq!(snap.rss_kb, Reading::Value(456 * page_size() as u64 / 1024));
        assert_eq!(snap.rss_anon_kb, Reading::Value(100));
    }

    #[test]
    fn valid_status_vmrss_survives_malformed_rss_anon() {
        let fixture = ProcFixture::new(None, Some("VmRSS: 200 kB\nRssAnon: malformed kB\n"));
        let mut collector = SelfCollector::new(fixture.root().to_str().unwrap());
        let snap = collector.sample(0, 0, 0);
        assert_eq!(snap.rss_kb, Reading::Value(200));
        assert!(matches!(snap.rss_anon_kb, Reading::Unavailable { .. }));
    }
}
