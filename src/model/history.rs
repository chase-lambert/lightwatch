use super::snapshot::Reading;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Shared constants
// ---------------------------------------------------------------------------

/// Maximum logical CPUs supported (single allocation boundary).
pub const MAX_CPU_CORES: usize = 256;
/// Length of the core-color palette.
pub const CORE_PALETTE_LEN: usize = 16;
/// Default history window in seconds (1 minute, GSM-style).
pub const DEFAULT_HISTORY_SECS: u64 = 60;
/// Default sample interval in milliseconds.
pub const DEFAULT_INTERVAL_MS: u64 = 1000;

// ---------------------------------------------------------------------------
// SamplePoint
// ---------------------------------------------------------------------------

/// A single point in a metric ring: timestamp (boottime nanos) and optional value.
#[derive(Clone, Copy, Debug)]
pub struct SamplePoint {
    pub t_boot_ns: u64,
    pub value: Option<f32>,
}

impl SamplePoint {
    pub fn new(t_boot_ns: u64, value: f32) -> Self {
        Self {
            t_boot_ns,
            value: Some(value),
        }
    }

    pub fn gap(t_boot_ns: u64) -> Self {
        Self {
            t_boot_ns,
            value: None,
        }
    }
}

// ---------------------------------------------------------------------------
// CoreId and CoreReading — stable per-core identity
// ---------------------------------------------------------------------------

/// Stable core identifier: numeric index parsed from `cpuN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CoreId(pub u32);

impl CoreId {
    pub fn label(&self) -> String {
        format!("cpu{}", self.0)
    }
}

/// A labeled per-core CPU reading.
#[derive(Clone, Debug)]
pub struct CoreReading {
    pub id: CoreId,
    pub label: String,
    pub value: Reading<f32>,
}

/// Normalize a list of core readings: sort by id, truncate to MAX_CPU_CORES.
/// Returns the normalized list and the count of hidden cores.
pub fn normalize_cores(cores: Vec<CoreReading>) -> (Vec<CoreReading>, usize) {
    let mut sorted = cores;
    sorted.sort_by_key(|c| c.id);
    let hidden = if sorted.len() > MAX_CPU_CORES {
        sorted.len() - MAX_CPU_CORES
    } else {
        0
    };
    sorted.truncate(MAX_CPU_CORES);
    debug_assert!(sorted.len() <= MAX_CPU_CORES);
    (sorted, hidden)
}

/// Normalize a list of core ids: sort, truncate to MAX_CPU_CORES.
/// Returns the clamped ids and the count of hidden cores.
pub fn normalize_core_ids(ids: &[CoreId]) -> (Vec<CoreId>, usize) {
    let mut sorted = ids.to_vec();
    sorted.sort();
    let hidden = if sorted.len() > MAX_CPU_CORES {
        sorted.len() - MAX_CPU_CORES
    } else {
        0
    };
    sorted.truncate(MAX_CPU_CORES);
    debug_assert!(sorted.len() <= MAX_CPU_CORES);
    (sorted, hidden)
}

// ---------------------------------------------------------------------------
// HistoryConfig — validated configuration
// ---------------------------------------------------------------------------

pub const MIN_INTERVAL_MS: u64 = 100;
pub const MAX_INTERVAL_MS: u64 = 60_000;
pub const MAX_WINDOW_SECS: u64 = 7200; // 2 hours
pub const MAX_POINTS_PER_SERIES: usize = 7200;

/// Validated history configuration. Can only be constructed via `validate()`.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryConfig {
    pub interval: Duration,
    pub window: Duration,
    pub capacity: usize,
}

impl HistoryConfig {
    /// Default: 1 s interval, 60 s window → 60 capacity (GSM-aligned).
    pub fn default_config() -> Self {
        Self {
            interval: Duration::from_millis(DEFAULT_INTERVAL_MS),
            window: Duration::from_secs(DEFAULT_HISTORY_SECS),
            capacity: DEFAULT_HISTORY_SECS as usize,
        }
    }

    /// Validate and construct. Returns `Ok(config)` or `Err(reason)`.
    pub fn validate(interval_ms: u64, window_secs: u64) -> Result<Self, String> {
        // 1. interval bounds
        if interval_ms < MIN_INTERVAL_MS {
            return Err(format!(
                "interval {interval_ms}ms < minimum {MIN_INTERVAL_MS}ms"
            ));
        }
        if interval_ms > MAX_INTERVAL_MS {
            return Err(format!(
                "interval {interval_ms}ms > maximum {MAX_INTERVAL_MS}ms"
            ));
        }

        // 2. window bounds
        if window_secs == 0 {
            return Err("window must be > 0 seconds".into());
        }
        if window_secs > MAX_WINDOW_SECS {
            return Err(format!(
                "window {window_secs}s > maximum {MAX_WINDOW_SECS}s"
            ));
        }

        let interval = Duration::from_millis(interval_ms);
        let window = Duration::from_secs(window_secs);

        if window < interval {
            return Err(format!(
                "window {window_secs}s must be >= interval {}ms",
                interval_ms
            ));
        }

        // 3. capacity
        let capacity_f = window.as_secs_f64() / interval.as_secs_f64();
        let capacity = capacity_f.floor() as usize;
        if capacity == 0 {
            return Err("capacity must be >= 1".into());
        }
        if capacity > MAX_POINTS_PER_SERIES {
            return Err(format!(
                "capacity {capacity} exceeds maximum {MAX_POINTS_PER_SERIES}"
            ));
        }

        Ok(Self {
            interval,
            window,
            capacity,
        })
    }
}

// ---------------------------------------------------------------------------
// Ring — fixed-capacity circular buffer for SamplePoint
// ---------------------------------------------------------------------------

/// A fixed-capacity ring buffer of `SamplePoint`s.
/// Push N + K values; oldest are silently dropped.
/// Resize is atomic: builds new ring, swaps on success, leaves old intact on failure.
#[derive(Clone, Debug)]
pub struct Ring {
    buf: Vec<SamplePoint>,
    write: usize,
    len: usize,
    capacity: usize,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Ring capacity must be >= 1");
        Self {
            buf: vec![SamplePoint::gap(0); capacity],
            write: 0,
            len: 0,
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, point: SamplePoint) {
        self.buf[self.write] = point;
        self.write = (self.write + 1) % self.capacity;
        if self.len < self.capacity {
            self.len += 1;
        }
    }

    /// Return points in chronological order (oldest first).
    pub fn points(&self) -> Vec<SamplePoint> {
        if self.len == 0 {
            return vec![];
        }
        let mut out = Vec::with_capacity(self.len);
        if self.len == self.capacity {
            // fully wrapped
            for i in 0..self.capacity {
                out.push(self.buf[(self.write + i) % self.capacity]);
            }
        } else {
            // not yet wrapped
            for i in 0..self.len {
                out.push(self.buf[i]);
            }
        }
        out
    }

    /// Return the most recent point, if any.
    pub fn latest(&self) -> Option<SamplePoint> {
        if self.len == 0 {
            return None;
        }
        let idx = if self.write == 0 {
            self.capacity - 1
        } else {
            self.write - 1
        };
        Some(self.buf[idx])
    }

    /// Resize to a new capacity. On failure (bad capacity) returns `Err` and
    /// leaves self unchanged. On success builds a new ring keeping the newest N
    /// points.
    pub fn try_resize(&mut self, new_capacity: usize) -> Result<(), String> {
        if new_capacity == 0 {
            return Err("capacity must be >= 1".into());
        }
        let mut new_ring = Ring::new(new_capacity);
        // copy the newest min(len, new_capacity) points
        let keep = self.len.min(new_capacity);
        let all = self.points();
        let recent = &all[all.len() - keep..];
        for p in recent {
            new_ring.push(*p);
        }
        *self = new_ring;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// History — collection of named rings (one per metric series)
// ---------------------------------------------------------------------------

/// Named metric series rings.
#[derive(Clone, Debug)]
pub struct History {
    pub cpu_total: Ring,
    /// Keyed per-core rings: (CoreId, Ring). Indexed by stable core id, not position.
    pub cpu_per_core: Vec<(CoreId, Ring)>,
    pub cpu_temp: Ring,
    pub cpu_freq: Ring,
    pub mem_used: Ring,
    pub swap_used: Ring,
    pub load1: Ring,
    pub load5: Ring,
    pub load15: Ring,
    pub self_rss: Ring,
    pub self_cpu: Ring,
    pub gpu_series: Vec<GpuHistory>,
}

#[derive(Clone, Debug)]
pub struct GpuHistory {
    pub pci_id: String,
    pub util: Ring,
    pub vram_used: Ring,
    pub temp: Ring,
    pub power: Ring,
}

impl History {
    /// Create a new History. `core_ids` are the initial core identifiers;
    /// rings are created for each.
    pub fn new(config: &HistoryConfig, core_ids: &[CoreId], gpu_ids: &[String]) -> Self {
        let cap = config.capacity;
        let cpu_per_core: Vec<(CoreId, Ring)> = core_ids
            .iter()
            .map(|&id| (id, Ring::new(cap)))
            .collect();
        let gpu_series: Vec<GpuHistory> = gpu_ids
            .iter()
            .map(|id| GpuHistory {
                pci_id: id.clone(),
                util: Ring::new(cap),
                vram_used: Ring::new(cap),
                temp: Ring::new(cap),
                power: Ring::new(cap),
            })
            .collect();
        Self {
            cpu_total: Ring::new(cap),
            cpu_per_core,
            cpu_temp: Ring::new(cap),
            cpu_freq: Ring::new(cap),
            mem_used: Ring::new(cap),
            swap_used: Ring::new(cap),
            load1: Ring::new(cap),
            load5: Ring::new(cap),
            load15: Ring::new(cap),
            self_rss: Ring::new(cap),
            self_cpu: Ring::new(cap),
            gpu_series,
        }
    }

    /// Atomically resize all rings to a new capacity. On any failure, leaves
    /// self unchanged.
    pub fn resize(&mut self, new_capacity: usize) -> Result<(), String> {
        // quick validate
        if new_capacity == 0 {
            return Err("capacity must be >= 1".into());
        }
        // clone self to try resizing
        let mut candidate = self.clone();
        candidate.cpu_total.try_resize(new_capacity)?;
        for (_, ring) in &mut candidate.cpu_per_core {
            ring.try_resize(new_capacity)?;
        }
        candidate.cpu_temp.try_resize(new_capacity)?;
        candidate.cpu_freq.try_resize(new_capacity)?;
        candidate.mem_used.try_resize(new_capacity)?;
        candidate.swap_used.try_resize(new_capacity)?;
        candidate.load1.try_resize(new_capacity)?;
        candidate.load5.try_resize(new_capacity)?;
        candidate.load15.try_resize(new_capacity)?;
        candidate.self_rss.try_resize(new_capacity)?;
        candidate.self_cpu.try_resize(new_capacity)?;
        for gpu in &mut candidate.gpu_series {
            gpu.util.try_resize(new_capacity)?;
            gpu.vram_used.try_resize(new_capacity)?;
            gpu.temp.try_resize(new_capacity)?;
            gpu.power.try_resize(new_capacity)?;
        }
        *self = candidate;
        Ok(())
    }

    /// Reconcile per-core rings against a normalized core list.
    ///
    /// - `permit_removal = true` (authoritative complete topology): remove
    ///   vanished cores first, then add any new cores from the snapshot.
    /// - `permit_removal = false` (partial sample): add new cores only when
    ///   there is room under MAX_CPU_CORES; never remove existing rings.
    ///   Partial samples must pass `false`.
    pub fn reconcile_cores(
        &mut self,
        config: &HistoryConfig,
        snapshot_cores: &[CoreReading],
        permit_removal: bool,
    ) {
        let cap = config.capacity;

        if permit_removal {
            // Authoritative topology: removal first (makes room), then adds.
            let snap_ids: std::collections::HashSet<CoreId> =
                snapshot_cores.iter().map(|c| c.id).collect();
            self.cpu_per_core
                .retain(|(id, _)| snap_ids.contains(id));
            for core in snapshot_cores {
                if !self.cpu_per_core.iter().any(|(id, _)| *id == core.id) {
                    let ring = Ring::new(cap);
                    self.cpu_per_core.push((core.id, ring));
                }
            }
        } else {
            // Partial sample: only add new cores when room remains;
            // never remove existing rings even if absent from snapshot.
            for core in snapshot_cores {
                if !self.cpu_per_core.iter().any(|(id, _)| *id == core.id)
                    && self.cpu_per_core.len() < MAX_CPU_CORES
                {
                    let ring = Ring::new(cap);
                    self.cpu_per_core.push((core.id, ring));
                }
            }
        }

        debug_assert!(self.cpu_per_core.len() <= MAX_CPU_CORES);
    }

    /// Reconcile GPU series: add new GPUs, remove gone ones (keeps matching by pci_id).
    pub fn reconcile_gpus(&mut self, config: &HistoryConfig, gpu_ids: &[String]) {
        let cap = config.capacity;
        // remove GPUs not in the new list
        self.gpu_series.retain(|g| gpu_ids.contains(&g.pci_id));
        // add GPUs that are new
        for id in gpu_ids {
            if !self.gpu_series.iter().any(|g| g.pci_id == *id) {
                self.gpu_series.push(GpuHistory {
                    pci_id: id.clone(),
                    util: Ring::new(cap),
                    vram_used: Ring::new(cap),
                    temp: Ring::new(cap),
                    power: Ring::new(cap),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CpuSnapshot {
    pub usage_percent: Reading<f32>,
    /// Labeled per-core readings with stable CoreId (already normalized).
    pub per_core_percent: Vec<CoreReading>,
    /// Number of cores hidden due to MAX_CPU_CORES truncation.
    pub core_hidden: usize,
    pub temp_celsius: Reading<f32>,
    pub freq_mhz: Reading<f32>,
}

#[derive(Clone, Debug)]
pub struct MemorySnapshot {
    pub total_kb: u64,
    pub used_kb: Reading<u64>,
    pub available_kb: Reading<u64>,
    pub swap_total_kb: Reading<u64>,
    pub swap_used_kb: Reading<u64>,
    pub load_1min: Reading<f32>,
    pub load_5min: Reading<f32>,
    pub load_15min: Reading<f32>,
}

#[derive(Clone, Debug)]
pub struct GpuSnapshot {
    pub pci_id: String,
    pub vendor_id: String,
    pub device_id: String,
    pub driver: String,
    pub name: String,
    pub util_percent: Reading<f32>,
    pub vram_total_kb: Reading<u64>,
    pub vram_used_kb: Reading<u64>,
    pub temp_celsius: Reading<f32>,
    pub power_watts: Reading<f32>,
}

#[derive(Clone, Debug)]
pub struct SelfSnapshot {
    pub rss_kb: Reading<u64>,
    pub cpu_percent: Reading<f32>,
    pub uptime_secs: u64,
    pub sample_duration_us: u64,
    pub sampler_overruns: u64,
    pub ticks_skipped: u64,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub seq: u64,
    pub t_boot_ns: u64,
    pub sample_duration_us: u64,
    pub sampler_overruns: u64,
    pub ticks_skipped: u64,
    pub cpu: CpuSnapshot,
    pub memory: MemorySnapshot,
    pub gpus: Vec<GpuSnapshot>,
    pub self_metrics: SelfSnapshot,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_push_and_read() {
        let mut ring = Ring::new(3);
        ring.push(SamplePoint::new(1, 10.0));
        ring.push(SamplePoint::new(2, 20.0));
        assert_eq!(ring.len(), 2);
        let pts = ring.points();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].value, Some(10.0));
        assert_eq!(pts[1].value, Some(20.0));
    }

    #[test]
    fn ring_wraps() {
        let mut ring = Ring::new(3);
        for i in 0..5 {
            ring.push(SamplePoint::new(i, i as f32));
        }
        let pts = ring.points();
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0].value, Some(2.0));
        assert_eq!(pts[1].value, Some(3.0));
        assert_eq!(pts[2].value, Some(4.0));
    }

    #[test]
    fn ring_resize_keeps_newest() {
        let mut ring = Ring::new(5);
        for i in 0..10 {
            ring.push(SamplePoint::new(i, i as f32));
        }
        ring.try_resize(3).unwrap();
        let pts = ring.points();
        assert_eq!(pts.len(), 3);
        // newest 3: 7,8,9
        assert_eq!(pts[0].value, Some(7.0));
        assert_eq!(pts[1].value, Some(8.0));
        assert_eq!(pts[2].value, Some(9.0));
    }

    #[test]
    fn ring_resize_failure_leaves_unchanged() {
        let mut ring = Ring::new(5);
        for i in 0..10 {
            ring.push(SamplePoint::new(i, i as f32));
        }
        let before = ring.points();
        assert!(ring.try_resize(0).is_err());
        let after = ring.points();
        assert_eq!(before.len(), after.len());
    }

    #[test]
    fn ring_latest() {
        let mut ring = Ring::new(3);
        assert!(ring.latest().is_none());
        ring.push(SamplePoint::new(1, 10.0));
        assert_eq!(ring.latest().unwrap().value, Some(10.0));
    }

    #[test]
    fn history_config_default() {
        let c = HistoryConfig::default_config();
        assert_eq!(c.capacity, 60);
        assert_eq!(c.interval, Duration::from_secs(1));
        assert_eq!(c.window, Duration::from_secs(60));
    }

    #[test]
    fn history_config_reject_zero_interval() {
        assert!(HistoryConfig::validate(0, 60).is_err());
    }

    #[test]
    fn history_config_reject_interval_below_min() {
        assert!(HistoryConfig::validate(50, 60).is_err());
    }

    #[test]
    fn history_config_reject_interval_above_max() {
        assert!(HistoryConfig::validate(120_000, 60).is_err());
    }

    #[test]
    fn history_config_reject_window_above_max() {
        assert!(HistoryConfig::validate(1000, 10_000).is_err());
    }

    #[test]
    fn history_config_reject_window_less_than_interval() {
        assert!(HistoryConfig::validate(5000, 1).is_err());
    }

    #[test]
    fn history_config_reject_capacity_zero() {
        // 10ms interval, 5ms window = 0 capacity
        assert!(HistoryConfig::validate(10, 0).is_err());
    }

    #[test]
    fn history_config_reject_capacity_above_max_points() {
        // 10ms interval, 7201 second window = 720100 capacity
        let r = HistoryConfig::validate(10, 7201);
        assert!(r.is_err());
    }

    #[test]
    fn history_config_accept_valid() {
        let c = HistoryConfig::validate(1000, 60).unwrap();
        assert_eq!(c.capacity, 60);
        assert_eq!(c.interval, Duration::from_millis(1000));
        assert_eq!(c.window, Duration::from_secs(60));
    }

    #[test]
    fn history_default_no_reconcile() {
        let config = HistoryConfig::default_config();
        let core_ids: Vec<CoreId> = (0..4).map(CoreId).collect();
        let hist = History::new(&config, &core_ids, &[]);
        assert_eq!(hist.cpu_per_core.len(), 4);
        assert_eq!(hist.gpu_series.len(), 0);
    }

    #[test]
    fn history_reconcile_gpus_add_and_remove() {
        let config = HistoryConfig::default_config();
        let core_ids: Vec<CoreId> = (0..2).map(CoreId).collect();
        let mut hist = History::new(&config, &core_ids, &["0000:01:00.0".into()]);
        assert_eq!(hist.gpu_series.len(), 1);
        hist.reconcile_gpus(&config, &["0000:04:00.0".into()]);
        assert_eq!(hist.gpu_series.len(), 1);
        assert_eq!(hist.gpu_series[0].pci_id, "0000:04:00.0");
    }

    #[test]
    fn history_resize_atomic() {
        let config = HistoryConfig::default_config();
        let core_ids = vec![CoreId(0)];
        let mut hist = History::new(&config, &core_ids, &[]);
        hist.resize(100).unwrap();
        assert_eq!(hist.cpu_total.capacity(), 100);
    }

    // ── Core identity tests ──

    #[test]
    fn normalize_cores_sorts_and_truncates() {
        let cores: Vec<CoreReading> = vec![
            CoreReading {
                id: CoreId(5),
                label: "cpu5".into(),
                value: Reading::Value(50.0),
            },
            CoreReading {
                id: CoreId(0),
                label: "cpu0".into(),
                value: Reading::Value(10.0),
            },
            CoreReading {
                id: CoreId(2),
                label: "cpu2".into(),
                value: Reading::Value(30.0),
            },
        ];
        let (norm, hidden) = normalize_cores(cores);
        assert_eq!(hidden, 0);
        assert_eq!(norm.len(), 3);
        assert_eq!(norm[0].id, CoreId(0));
        assert_eq!(norm[1].id, CoreId(2));
        assert_eq!(norm[2].id, CoreId(5));
    }

    #[test]
    fn normalize_cores_clamps_to_max() {
        let cores: Vec<CoreReading> = (0..260)
            .map(|i| CoreReading {
                id: CoreId(i),
                label: format!("cpu{i}"),
                value: Reading::Value(0.0),
            })
            .collect();
        assert_eq!(cores.len(), 260);
        let (norm, hidden) = normalize_cores(cores);
        assert_eq!(norm.len(), 256);
        assert_eq!(hidden, 4);
    }

    #[test]
    fn normalize_core_ids_sorts_and_truncates() {
        let ids = vec![CoreId(300), CoreId(0), CoreId(5), CoreId(2)];
        let (norm, hidden) = normalize_core_ids(&ids);
        assert_eq!(hidden, 0);
        assert_eq!(norm.len(), 4);
        assert_eq!(norm[0], CoreId(0));
        assert_eq!(norm[1], CoreId(2));
        assert_eq!(norm[2], CoreId(5));
        assert_eq!(norm[3], CoreId(300));
    }

    #[test]
    fn normalize_core_ids_clamps_to_max() {
        let ids: Vec<CoreId> = (0..260).map(CoreId).collect();
        assert_eq!(ids.len(), 260);
        let (norm, hidden) = normalize_core_ids(&ids);
        assert_eq!(norm.len(), 256);
        assert_eq!(hidden, 4);
    }

    #[test]
    fn reconcile_cores_adds_new_preserves_existing() {
        let config = HistoryConfig::default_config();
        let initial = vec![CoreId(0), CoreId(1)];
        let mut hist = History::new(&config, &initial, &[]);
        assert_eq!(hist.cpu_per_core.len(), 2);

        let snap = vec![
            CoreReading {
                id: CoreId(0),
                label: "cpu0".into(),
                value: Reading::Value(10.0),
            },
            CoreReading {
                id: CoreId(1),
                label: "cpu1".into(),
                value: Reading::Value(20.0),
            },
            CoreReading {
                id: CoreId(2),
                label: "cpu2".into(),
                value: Reading::Value(30.0),
            },
        ];
        hist.reconcile_cores(&config, &snap, true);
        // should now have 3 cores (adds cpu2, keeps existing)
        assert_eq!(hist.cpu_per_core.len(), 3);
        // new ring must be empty — no fabricated t=0 sample
        let (_, ring2) = hist
            .cpu_per_core
            .iter()
            .find(|(id, _)| *id == CoreId(2))
            .expect("cpu2 should exist");
        assert_eq!(ring2.len(), 0, "new ring must be empty, no t=0 sample");
    }

    #[test]
    fn reconcile_cores_removes_when_permitted() {
        let config = HistoryConfig::default_config();
        let initial = vec![CoreId(0), CoreId(1), CoreId(2)];
        let mut hist = History::new(&config, &initial, &[]);
        assert_eq!(hist.cpu_per_core.len(), 3);

        let snap = vec![CoreReading {
            id: CoreId(0),
            label: "cpu0".into(),
            value: Reading::Value(10.0),
        }];
        hist.reconcile_cores(&config, &snap, true);
        assert_eq!(hist.cpu_per_core.len(), 1);
        assert_eq!(hist.cpu_per_core[0].0, CoreId(0));
    }

    #[test]
    fn reconcile_cores_no_removal_when_not_permitted() {
        let config = HistoryConfig::default_config();
        let initial = vec![CoreId(0), CoreId(1), CoreId(2)];
        let mut hist = History::new(&config, &initial, &[]);

        let snap = vec![CoreReading {
            id: CoreId(0),
            label: "cpu0".into(),
            value: Reading::Value(10.0),
        }];
        // partial sample — don't remove
        hist.reconcile_cores(&config, &snap, false);
        assert_eq!(hist.cpu_per_core.len(), 3);
    }

    #[test]
    fn reconcile_cores_empty_twice_does_not_remove_rings() {
        // Regression: old bug tracked prev_core_count (became 0 after one
        // empty) and `0 >= 0` would permit removal on the second empty sample.
        let config = HistoryConfig::default_config();
        let initial = vec![CoreId(0), CoreId(1), CoreId(2)];
        let mut hist = History::new(&config, &initial, &[]);
        assert_eq!(hist.cpu_per_core.len(), 3);

        let empty: Vec<CoreReading> = vec![];
        // First empty sample: no removal (empty → permit_removal = false)
        hist.reconcile_cores(&config, &empty, false);
        assert_eq!(hist.cpu_per_core.len(), 3);
        // Second empty sample: still no removal
        hist.reconcile_cores(&config, &empty, false);
        assert_eq!(hist.cpu_per_core.len(), 3);
    }

    #[test]
    fn reconcile_cores_partial_smaller_without_authority_does_not_remove() {
        // A partial nonempty sample (fewer cores than last authoritative count)
        // must not remove rings. Only a complete/authoritative topology can remove.
        let config = HistoryConfig::default_config();
        let initial = vec![CoreId(0), CoreId(1), CoreId(2), CoreId(3)];
        let mut hist = History::new(&config, &initial, &[]);
        assert_eq!(hist.cpu_per_core.len(), 4);

        // Partial sample: only 2 of 4 cores
        let partial = vec![
            CoreReading {
                id: CoreId(0),
                label: "cpu0".into(),
                value: Reading::Value(10.0),
            },
            CoreReading {
                id: CoreId(1),
                label: "cpu1".into(),
                value: Reading::Value(20.0),
            },
        ];
        // permit_removal=false: partial, not authoritative
        hist.reconcile_cores(&config, &partial, false);
        assert_eq!(
            hist.cpu_per_core.len(),
            4,
            "partial sample must not remove rings"
        );
    }

    #[test]
    fn reconcile_cores_new_core_ring_empty_then_real_push() {
        // Bug fix: new rings must be empty on insert; the worker pushes the
        // real timestamped sample afterward. No fabricated t=0 point.
        let config = HistoryConfig::default_config();
        let initial = vec![CoreId(0)];
        let mut hist = History::new(&config, &initial, &[]);

        let snap = vec![
            CoreReading {
                id: CoreId(0),
                label: "cpu0".into(),
                value: Reading::Value(10.0),
            },
            CoreReading {
                id: CoreId(1),
                label: "cpu1".into(),
                value: Reading::Value(50.0),
            },
        ];
        hist.reconcile_cores(&config, &snap, true);
        assert_eq!(hist.cpu_per_core.len(), 2);

        // New ring must be empty
        let (_, ring1) = hist
            .cpu_per_core
            .iter()
            .find(|(id, _)| *id == CoreId(1))
            .expect("cpu1 should exist");
        assert_eq!(ring1.len(), 0, "new ring must start empty");

        // Simulate worker pushing the real timestamped sample
        let (_id, ring1_mut) = hist
            .cpu_per_core
            .iter_mut()
            .find(|(id, _)| *id == CoreId(1))
            .unwrap();
        let real_t = 42_000_000_000u64;
        ring1_mut.push(SamplePoint::new(real_t, 50.0));
        assert_eq!(ring1_mut.len(), 1);
        let pts = ring1_mut.points();
        assert_eq!(pts[0].t_boot_ns, real_t);
        assert_eq!(pts[0].value, Some(50.0));
    }

    #[test]
    fn reconcile_cores_full_no_new_without_removal() {
        // 256 retained rings + partial sample with new CoreId + permit_removal=false
        // → must not exceed MAX_CPU_CORES; new id is skipped.
        let config = HistoryConfig::default_config();
        let initial: Vec<CoreId> = (0..MAX_CPU_CORES as u32).map(CoreId).collect();
        let mut hist = History::new(&config, &initial, &[]);
        assert_eq!(hist.cpu_per_core.len(), MAX_CPU_CORES);

        // Partial sample: include only cpu0 and a brand new cpu256
        let partial = vec![
            CoreReading {
                id: CoreId(0),
                label: "cpu0".into(),
                value: Reading::Value(10.0),
            },
            CoreReading {
                id: CoreId(256),
                label: "cpu256".into(),
                value: Reading::Value(50.0),
            },
        ];
        hist.reconcile_cores(&config, &partial, false);
        assert_eq!(
            hist.cpu_per_core.len(),
            MAX_CPU_CORES,
            "must not exceed MAX_CPU_CORES when adding without removal"
        );
        // cpu256 must not have been inserted
        assert!(
            !hist.cpu_per_core.iter().any(|(id, _)| *id == CoreId(256)),
            "new id must be skipped when already at MAX_CPU_CORES"
        );
    }

    #[test]
    fn reconcile_cores_removal_first_makes_room_then_adds() {
        // When permit_removal=true, removal happens first, making room for
        // new cores that replace old ones (e.g., topology change 0-7 → 8-15).
        let config = HistoryConfig::default_config();
        let initial: Vec<CoreId> = (0..8).map(CoreId).collect();
        let mut hist = History::new(&config, &initial, &[]);
        assert_eq!(hist.cpu_per_core.len(), 8);

        // Complete topology sample: ids 4-11 (4 old, 4 new)
        let snap: Vec<CoreReading> = (4..12)
            .map(|i| CoreReading {
                id: CoreId(i),
                label: format!("cpu{i}"),
                value: Reading::Value(0.0),
            })
            .collect();
        hist.reconcile_cores(&config, &snap, true);
        assert_eq!(hist.cpu_per_core.len(), 8);
        // Old cores 0-3 should have been removed, 8-11 added
        let ids: Vec<CoreId> = hist
            .cpu_per_core
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert!(!ids.contains(&CoreId(0)));
        assert!(!ids.contains(&CoreId(3)));
        assert!(ids.contains(&CoreId(10)));
        assert!(ids.contains(&CoreId(11)));
    }
}
