use super::snapshot::Reading;
use std::time::Duration;

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
    /// Default: 1 s interval, 15 min window → 900 capacity.
    pub fn default_config() -> Self {
        Self {
            interval: Duration::from_secs(1),
            window: Duration::from_secs(900),
            capacity: 900,
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
    pub cpu_per_core: Vec<Ring>,
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
    pub fn new(config: &HistoryConfig, core_count: usize, gpu_ids: &[String]) -> Self {
        let cap = config.capacity;
        let cpu_per_core: Vec<Ring> = (0..core_count).map(|_| Ring::new(cap)).collect();
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
        for ring in &mut candidate.cpu_per_core {
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
    pub per_core_percent: Vec<Reading<f32>>,
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
        assert_eq!(c.capacity, 900);
        assert_eq!(c.interval, Duration::from_secs(1));
        assert_eq!(c.window, Duration::from_secs(900));
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
        let hist = History::new(&config, 4, &[]);
        assert_eq!(hist.cpu_per_core.len(), 4);
        assert_eq!(hist.gpu_series.len(), 0);
    }

    #[test]
    fn history_reconcile_gpus_add_and_remove() {
        let config = HistoryConfig::default_config();
        let mut hist = History::new(&config, 2, &["0000:01:00.0".into()]);
        assert_eq!(hist.gpu_series.len(), 1);
        hist.reconcile_gpus(&config, &["0000:04:00.0".into()]);
        assert_eq!(hist.gpu_series.len(), 1);
        assert_eq!(hist.gpu_series[0].pci_id, "0000:04:00.0");
    }

    #[test]
    fn history_resize_atomic() {
        let config = HistoryConfig::default_config();
        let mut hist = History::new(&config, 1, &[]);
        hist.resize(100).unwrap();
        assert_eq!(hist.cpu_total.capacity(), 100);
    }
}
