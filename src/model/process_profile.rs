//! Selected-process detail — one live profile, never an all-process history.

use super::{ProcessId, Reading, Ring, SamplePoint};

#[derive(Clone, Debug)]
pub struct ProcessProfileSnapshot {
    pub id: ProcessId,
    pub name: String,
    pub alive: bool,
    pub state: char,
    pub age_secs: u64,
    pub parent_pid: u32,
    pub uid: Reading<u32>,
    pub thread_count: u32,
    pub priority: i64,
    pub nice: i64,
    pub cpu_percent: Reading<f32>,
    pub user_cpu_secs: f64,
    pub system_cpu_secs: f64,
    pub minor_faults_per_sec: Reading<f32>,
    pub major_faults_per_sec: Reading<f32>,
    pub rss_anon_kb: u64,
    pub rss_total_kb: Reading<u64>,
    pub rss_peak_kb: Reading<u64>,
    pub swap_kb: Reading<u64>,
    pub pss_kb: Reading<u64>,
    pub private_kb: Reading<u64>,
    pub disk_read_bytes_per_sec: Reading<f32>,
    pub disk_write_bytes_per_sec: Reading<f32>,
    pub disk_read_bytes: Reading<u64>,
    pub disk_write_bytes: Reading<u64>,
    pub command_line: Reading<String>,
    pub executable: Reading<String>,
    pub cgroup: Reading<String>,
    pub cpu_affinity: Reading<String>,
    pub open_fds: Reading<OpenFdCount>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenFdCount {
    pub count: usize,
    pub capped: bool,
}

/// Fixed series map: CPU, RssAnon, minor/major faults, block read/write.
#[derive(Clone, Debug)]
pub struct ProcessProfileHistory {
    pub cpu: Ring,
    pub rss_anon: Ring,
    pub minor_faults: Ring,
    pub major_faults: Ring,
    pub disk_read: Ring,
    pub disk_write: Ring,
}

impl ProcessProfileHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            cpu: Ring::new(capacity),
            rss_anon: Ring::new(capacity),
            minor_faults: Ring::new(capacity),
            major_faults: Ring::new(capacity),
            disk_read: Ring::new(capacity),
            disk_write: Ring::new(capacity),
        }
    }

    pub fn resize(&mut self, capacity: usize) -> Result<(), String> {
        let mut candidate = self.clone();
        candidate.cpu.try_resize(capacity)?;
        candidate.rss_anon.try_resize(capacity)?;
        candidate.minor_faults.try_resize(capacity)?;
        candidate.major_faults.try_resize(capacity)?;
        candidate.disk_read.try_resize(capacity)?;
        candidate.disk_write.try_resize(capacity)?;
        *self = candidate;
        Ok(())
    }

    pub fn push(&mut self, t_boot_ns: u64, snapshot: &ProcessProfileSnapshot) {
        push_reading(&mut self.cpu, t_boot_ns, &snapshot.cpu_percent);
        self.rss_anon
            .push(SamplePoint::new(t_boot_ns, snapshot.rss_anon_kb as f32));
        push_reading(
            &mut self.minor_faults,
            t_boot_ns,
            &snapshot.minor_faults_per_sec,
        );
        push_reading(
            &mut self.major_faults,
            t_boot_ns,
            &snapshot.major_faults_per_sec,
        );
        push_reading(
            &mut self.disk_read,
            t_boot_ns,
            &snapshot.disk_read_bytes_per_sec,
        );
        push_reading(
            &mut self.disk_write,
            t_boot_ns,
            &snapshot.disk_write_bytes_per_sec,
        );
    }

    pub fn push_gap(&mut self, t_boot_ns: u64) {
        self.cpu.push(SamplePoint::gap(t_boot_ns));
        self.rss_anon.push(SamplePoint::gap(t_boot_ns));
        self.minor_faults.push(SamplePoint::gap(t_boot_ns));
        self.major_faults.push(SamplePoint::gap(t_boot_ns));
        self.disk_read.push(SamplePoint::gap(t_boot_ns));
        self.disk_write.push(SamplePoint::gap(t_boot_ns));
    }
}

fn push_reading(ring: &mut Ring, t_boot_ns: u64, value: &Reading<f32>) {
    match value {
        Reading::Value(value) => ring.push(SamplePoint::new(t_boot_ns, *value)),
        Reading::Unavailable { .. } => ring.push(SamplePoint::gap(t_boot_ns)),
    }
}

#[derive(Clone, Debug)]
pub struct PublishedProcessProfile {
    pub snapshot: ProcessProfileSnapshot,
    pub history: ProcessProfileHistory,
}
