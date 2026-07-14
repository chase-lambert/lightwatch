use crate::model::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

/// A snapshot bundled with its history for publication.
#[derive(Clone)]
pub struct Published {
    pub snapshot: Snapshot,
    pub history: History,
}

/// A single-slot cell for the latest published data.
/// Sampler writes; UI/consumer pulls.
pub struct Latest {
    generation: AtomicU64,
    payload: Mutex<Option<Arc<Published>>>,
}

impl Latest {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            payload: Mutex::new(None),
        }
    }

    /// Store a new payload and bump the generation.
    /// `Release` ordering ensures the payload is visible before the generation increment.
    pub fn publish(&self, published: Arc<Published>) -> u64 {
        let mut guard = self.payload.lock().unwrap();
        *guard = Some(published);
        drop(guard);
        self.generation.fetch_add(1, Ordering::Release) + 1
    }

    /// Pull the latest payload with its generation number.
    pub fn pull(&self) -> Option<(u64, Arc<Published>)> {
        let g = self.generation.load(Ordering::Acquire);
        let guard = self.payload.lock().unwrap();
        guard.as_ref().map(|arc| (g, Arc::clone(arc)))
    }

    /// Pull only if the generation is newer than `since`.
    pub fn pull_if_newer(&self, since: u64) -> Option<(u64, Arc<Published>)> {
        let g = self.generation.load(Ordering::Acquire);
        if g <= since {
            return None;
        }
        self.pull()
    }

    /// Get current generation without pulling.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

impl Default for Latest {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_published(seq: u64) -> Arc<Published> {
        Arc::new(Published {
            snapshot: Snapshot {
                seq,
                t_boot_ns: 0,
                sample_duration_us: 0,
                sampler_overruns: 0,
                ticks_skipped: 0,
                cpu: CpuSnapshot {
                    usage_percent: crate::model::Reading::Unavailable { reason: "test" },
                    per_core_percent: vec![],
                    temp_celsius: crate::model::Reading::Unavailable { reason: "test" },
                    freq_mhz: crate::model::Reading::Unavailable { reason: "test" },
                },
                memory: MemorySnapshot {
                    total_kb: 0,
                    used_kb: crate::model::Reading::Unavailable { reason: "test" },
                    available_kb: crate::model::Reading::Unavailable { reason: "test" },
                    swap_total_kb: crate::model::Reading::Unavailable { reason: "test" },
                    swap_used_kb: crate::model::Reading::Unavailable { reason: "test" },
                    load_1min: crate::model::Reading::Unavailable { reason: "test" },
                    load_5min: crate::model::Reading::Unavailable { reason: "test" },
                    load_15min: crate::model::Reading::Unavailable { reason: "test" },
                },
                gpus: vec![],
                self_metrics: SelfSnapshot {
                    rss_kb: crate::model::Reading::Unavailable { reason: "test" },
                    cpu_percent: crate::model::Reading::Unavailable { reason: "test" },
                    uptime_secs: 0,
                    sample_duration_us: 0,
                    sampler_overruns: 0,
                    ticks_skipped: 0,
                },
            },
            history: History::new(&HistoryConfig::default_config(), 0, &[]),
        })
    }

    #[test]
    fn publish_and_pull() {
        let latest = Latest::new();
        assert!(latest.pull().is_none());

        latest.publish(dummy_published(1));
        let (g, pubd) = latest.pull().unwrap();
        assert_eq!(g, 1);
        assert_eq!(pubd.snapshot.seq, 1);
    }

    #[test]
    fn multiple_publishes_overwrite() {
        let latest = Latest::new();
        latest.publish(dummy_published(1));
        latest.publish(dummy_published(2));
        let (g, pubd) = latest.pull().unwrap();
        assert_eq!(g, 2);
        assert_eq!(pubd.snapshot.seq, 2);
    }

    #[test]
    fn pull_if_newer() {
        let latest = Latest::new();
        assert!(latest.pull_if_newer(0).is_none());

        latest.publish(dummy_published(1));
        assert!(latest.pull_if_newer(1).is_none());
        assert!(latest.pull_if_newer(0).is_some());
    }

    #[test]
    fn generation_monotonic() {
        let latest = Latest::new();
        let g0 = latest.generation();
        latest.publish(dummy_published(1));
        let g1 = latest.generation();
        latest.publish(dummy_published(2));
        let g2 = latest.generation();
        assert!(g0 == 0);
        assert!(g1 > g0);
        assert!(g2 > g1);
    }
}
