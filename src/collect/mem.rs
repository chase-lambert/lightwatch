use crate::model::*;
use crate::parse::{mem_used, parse_meminfo, swap_used};

/// Collector for memory and swap metrics from /proc/meminfo.
pub struct MemCollector {
    proc_root: String,
}

impl MemCollector {
    pub fn new(proc_root: &str) -> Self {
        Self {
            proc_root: proc_root.to_string(),
        }
    }

    pub fn sample(&self) -> MemorySnapshot {
        let path = format!("{}/meminfo", self.proc_root);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let meminfo = parse_meminfo(&content);

        if !meminfo.has_required() {
            return MemorySnapshot {
                total_kb: 0,
                used_kb: Reading::Unavailable {
                    reason: "meminfo missing MemTotal or MemAvailable",
                },
                available_kb: Reading::Unavailable {
                    reason: "meminfo missing",
                },
                swap_total_kb: Reading::Unavailable {
                    reason: "meminfo missing",
                },
                swap_used_kb: Reading::Unavailable {
                    reason: "meminfo missing",
                },
                load_1min: Reading::Unavailable {
                    reason: "meminfo missing",
                },
                load_5min: Reading::Unavailable {
                    reason: "meminfo missing",
                },
                load_15min: Reading::Unavailable {
                    reason: "meminfo missing",
                },
            };
        }

        let total_kb = meminfo.get("MemTotal").unwrap_or(0);
        let used = mem_used(&meminfo)
            .map(Reading::Value)
            .unwrap_or(Reading::Unavailable {
                reason: "meminfo missing MemTotal or MemAvailable",
            });
        let available =
            meminfo
                .get("MemAvailable")
                .map(Reading::Value)
                .unwrap_or(Reading::Unavailable {
                    reason: "no MemAvailable",
                });
        let swap_total =
            meminfo
                .get("SwapTotal")
                .map(Reading::Value)
                .unwrap_or(Reading::Unavailable {
                    reason: "no SwapTotal",
                });
        let swap_used_val =
            swap_used(&meminfo)
                .map(Reading::Value)
                .unwrap_or(Reading::Unavailable {
                    reason: "no swap info",
                });

        // Load is collected separately; we read /proc/loadavg here too
        let load_path = format!("{}/loadavg", self.proc_root);
        let load_content = std::fs::read_to_string(&load_path).unwrap_or_default();
        let loadavg = crate::parse::parse_loadavg(&load_content);
        let (l1, l5, l15) = match loadavg {
            Ok(l) => (
                Reading::Value(l.load_1min),
                Reading::Value(l.load_5min),
                Reading::Value(l.load_15min),
            ),
            Err(_) => (
                Reading::Unavailable {
                    reason: "loadavg unreadable",
                },
                Reading::Unavailable {
                    reason: "loadavg unreadable",
                },
                Reading::Unavailable {
                    reason: "loadavg unreadable",
                },
            ),
        };

        MemorySnapshot {
            total_kb,
            used_kb: used,
            available_kb: available,
            swap_total_kb: swap_total,
            swap_used_kb: swap_used_val,
            load_1min: l1,
            load_5min: l5,
            load_15min: l15,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_collector_smoke() {
        let collector = MemCollector::new("/proc");
        let snap = collector.sample();
        // at minimum, doesn't panic
        // Might be Unavailable in test environments without /proc
        let _ = snap;
    }
}
