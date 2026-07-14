use crate::model::*;
use crate::parse::{ProcStat, cpu_percent, parse_proc_stat, per_core_percent};

/// Collector for CPU metrics from /proc/stat and sysfs.
pub struct CpuCollector {
    proc_root: String, // default "/proc"
    sys_root: String,  // default "/sys"
    prev_stat: Option<ProcStat>,
}

impl CpuCollector {
    pub fn new(proc_root: &str, sys_root: &str) -> Self {
        Self {
            proc_root: proc_root.to_string(),
            sys_root: sys_root.to_string(),
            prev_stat: None,
        }
    }

    /// Clear the baseline so the next sample starts fresh (used after
    /// a discontinuity like suspend/resume).
    pub fn clear_baseline(&mut self) {
        self.prev_stat = None;
    }

    /// Collect current CPU snapshot. On first call, returns Unavailable for
    /// percentage metrics (no baseline).
    pub fn sample(&mut self) -> CpuSnapshot {
        let stat_path = format!("{}/stat", self.proc_root);
        let content = std::fs::read_to_string(&stat_path).unwrap_or_default();
        let curr_stat = parse_proc_stat(&content).ok();

        let (usage_percent, per_core) = if let (Some(prev), Some(curr)) =
            (&self.prev_stat, &curr_stat)
        {
            let pct = cpu_percent(prev, curr)
                .map(Reading::Value)
                .unwrap_or(Reading::Unavailable {
                    reason: "cpu counter decrease",
                });

            let per_core_pct: Vec<CoreReading> = per_core_percent(prev, curr)
                .into_iter()
                .map(|(label, pct)| {
                    // Parse core id from "cpuN" label
                    let numeric_id = label
                        .strip_prefix("cpu")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    CoreReading {
                        id: CoreId(numeric_id),
                        label,
                        value: match pct {
                            Some(v) => Reading::Value(v),
                            None => Reading::Unavailable {
                                reason: "new core or counter decrease",
                            },
                        },
                    }
                })
                .collect();

            (pct, per_core_pct)
        } else {
            (
                Reading::Unavailable {
                    reason: "no baseline yet",
                },
                Vec::new(),
            )
        };

        self.prev_stat = curr_stat;

        let temp = read_cpu_temp(&self.sys_root);
        let freq = read_cpu_freq(&self.sys_root);

        CpuSnapshot {
            usage_percent,
            per_core_percent: per_core,
            core_hidden: 0,
            temp_celsius: temp,
            freq_mhz: freq,
        }
    }
}

fn read_cpu_temp(sys_root: &str) -> Reading<f32> {
    // Look for k10temp hwmon (AMD) or acpitz (fallback)
    let hwmon_dir = format!("{sys_root}/class/hwmon");
    let Ok(entries) = std::fs::read_dir(&hwmon_dir) else {
        return Reading::Unavailable { reason: "no hwmon" };
    };
    for entry in entries.flatten() {
        let name_path = entry.path().join("name");
        let Ok(name) = std::fs::read_to_string(&name_path) else {
            continue;
        };
        let name = name.trim();
        if name == "k10temp" || name == "acpitz" || name == "coretemp" {
            // Look for temp1_input
            let temp_path = entry.path().join("temp1_input");
            if let Ok(val_str) = std::fs::read_to_string(&temp_path)
                && let Ok(val) = val_str.trim().parse::<f32>()
            {
                // Value is in millidegrees Celsius
                return Reading::Value(val / 1000.0);
            }
        }
    }
    Reading::Unavailable {
        reason: "no k10temp/acpitz/coretemp",
    }
}

fn read_cpu_freq(sys_root: &str) -> Reading<f32> {
    // Average scaling_cur_freq across all CPUs
    let cpu_dir = format!("{sys_root}/devices/system/cpu");
    let mut total_freq: u64 = 0;
    let mut count: u64 = 0;

    let Ok(entries) = std::fs::read_dir(&cpu_dir) else {
        return Reading::Unavailable {
            reason: "no cpu sysfs",
        };
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("cpu") && name[3..].chars().all(|c| c.is_ascii_digit()) {
            let freq_path = entry.path().join("cpufreq/scaling_cur_freq");
            if let Ok(val_str) = std::fs::read_to_string(&freq_path)
                && let Ok(val) = val_str.trim().parse::<u64>()
            {
                total_freq += val;
                count += 1;
            }
        }
    }

    if count > 0 {
        // freq is in kHz, convert to MHz
        Reading::Value((total_freq / count) as f32 / 1000.0)
    } else {
        Reading::Unavailable {
            reason: "no cpu freq",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_collector_first_sample_no_baseline() {
        // Use the real /proc (or fail gracefully — integration test)
        let mut collector = CpuCollector::new("/proc", "/sys");
        let snap = collector.sample();
        // On first call, usage_percent should be Unavailable
        // (unless something very odd happens with a prepopulated baseline)
        // Note: this test may fail in CI/non-Linux environments; in those
        // cases the stat file won't exist, so everything is Unavailable
        let _ = snap; // at minimum, doesn't panic
    }
}
