use crate::model::*;
use std::sync::{Arc, Mutex};

/// A snapshot bundled with its history for publication.
#[derive(Clone)]
pub struct Published {
    pub snapshot: Snapshot,
    pub history: History,
    pub process_profile: Option<PublishedProcessProfile>,
}

/// A single-slot cell for the latest published data.
/// Sampler writes; UI/consumer pulls.
pub struct Latest {
    state: Mutex<LatestState>,
}

struct LatestState {
    generation: u64,
    payload: Option<Arc<Published>>,
}

impl Latest {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(LatestState {
                generation: 0,
                payload: None,
            }),
        }
    }

    /// Store a new payload and bump the generation.
    /// The generation and payload change under one lock, so readers always get
    /// a pair from the same publication.
    pub fn publish(&self, published: Arc<Published>) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.generation = state
            .generation
            .checked_add(1)
            .expect("publication generation exhausted");
        state.payload = Some(published);
        state.generation
    }

    /// Pull the latest payload with its generation number.
    pub fn pull(&self) -> Option<(u64, Arc<Published>)> {
        let state = self.state.lock().unwrap();
        state
            .payload
            .as_ref()
            .map(|payload| (state.generation, Arc::clone(payload)))
    }

    /// Pull only if the generation is newer than `since`.
    pub fn pull_if_newer(&self, since: u64) -> Option<(u64, Arc<Published>)> {
        let state = self.state.lock().unwrap();
        if state.generation <= since {
            return None;
        }
        state
            .payload
            .as_ref()
            .map(|payload| (state.generation, Arc::clone(payload)))
    }

    /// Get current generation without pulling.
    pub fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
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
    use std::sync::Barrier;

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
                    core_hidden: 0,
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
                    rss_anon_kb: crate::model::Reading::Unavailable { reason: "test" },
                    cpu_percent: crate::model::Reading::Unavailable { reason: "test" },
                    uptime_secs: 0,
                    sample_duration_us: 0,
                    sampler_overruns: 0,
                    ticks_skipped: 0,
                },
                processes: vec![],
                health: crate::model::HealthSnapshot::default(),
            },
            history: History::new(&HistoryConfig::default_config(), &[], &[]),
            process_profile: None,
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

    #[test]
    fn concurrent_pulls_keep_generation_and_payload_coherent() {
        const PUBLICATIONS: u64 = 2_000;
        let latest = Arc::new(Latest::new());
        let start = Arc::new(Barrier::new(2));
        let producer_latest = Arc::clone(&latest);
        let producer_start = Arc::clone(&start);
        let producer = std::thread::spawn(move || {
            producer_start.wait();
            for seq in 1..=PUBLICATIONS {
                assert_eq!(producer_latest.publish(dummy_published(seq)), seq);
            }
        });

        start.wait();
        let mut seen = 0;
        while seen < PUBLICATIONS {
            if let Some((generation, payload)) = latest.pull_if_newer(seen) {
                assert_eq!(generation, payload.snapshot.seq);
                assert!(generation > seen);
                seen = generation;
            }
            std::thread::yield_now();
        }
        producer.join().unwrap();
        assert_eq!(seen, PUBLICATIONS);
    }
}
