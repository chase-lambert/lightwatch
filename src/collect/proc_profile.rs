//! One selected-process profile, sampled independently of the table rows.

use super::proc::{clock_ticks_per_sec, cpu_percent_of_machine, online_logical_cpus};
use crate::model::{
    OpenFdCount, ProcessId, ProcessProfileHistory, ProcessProfileSnapshot, PublishedProcessProfile,
    Reading,
};
use crate::parse::{parse_pid_io, parse_pid_stat, parse_pid_status, parse_smaps_rollup};
use std::fs;
use std::path::{Path, PathBuf};

const SLOW_REFRESH_NS: u64 = 5_000_000_000;
const MAX_OPEN_FDS: usize = 4096;
const MAX_COMMAND_BYTES: usize = 4096;

#[derive(Clone, Debug)]
struct Baseline {
    t_boot_ns: u64,
    ticks: u64,
    minor_faults: u64,
    major_faults: u64,
    disk_read_bytes: Option<u64>,
    disk_write_bytes: Option<u64>,
}

struct RateReadings {
    cpu: Reading<f32>,
    minor_faults: Reading<f32>,
    major_faults: Reading<f32>,
    disk_read: Reading<f32>,
    disk_write: Reading<f32>,
}

#[derive(Clone, Debug)]
struct SlowFields {
    pss_kb: Reading<u64>,
    private_kb: Reading<u64>,
    command_line: Reading<String>,
    executable: Reading<String>,
    cgroup: Reading<String>,
    open_fds: Reading<OpenFdCount>,
}

impl Default for SlowFields {
    fn default() -> Self {
        Self {
            pss_kb: unavailable("not sampled yet"),
            private_kb: unavailable("not sampled yet"),
            command_line: unavailable("not sampled yet"),
            executable: unavailable("not sampled yet"),
            cgroup: unavailable("not sampled yet"),
            open_fds: unavailable("not sampled yet"),
        }
    }
}

pub struct ProcessProfileCollector {
    proc_root: PathBuf,
    selected: Option<ProcessId>,
    baseline: Option<Baseline>,
    slow: SlowFields,
    last_slow_ns: Option<u64>,
    last: Option<PublishedProcessProfile>,
    ended: bool,
}

impl ProcessProfileCollector {
    pub fn new(proc_root: impl Into<PathBuf>) -> Self {
        Self {
            proc_root: proc_root.into(),
            selected: None,
            baseline: None,
            slow: SlowFields::default(),
            last_slow_ns: None,
            last: None,
            ended: false,
        }
    }

    pub fn clear_baseline(&mut self) {
        self.baseline = None;
    }

    pub fn push_gap(&mut self, t_boot_ns: u64) {
        if self.ended {
            return;
        }
        if let Some(last) = &mut self.last {
            last.history.push_gap(t_boot_ns);
        }
    }

    pub fn resize(&mut self, capacity: usize) -> Result<(), String> {
        if let Some(last) = &mut self.last {
            last.history.resize(capacity)?;
        }
        Ok(())
    }

    pub fn sample(
        &mut self,
        target: Option<ProcessId>,
        display_name: Option<&str>,
        table_rss_anon_kb: Option<u64>,
        t_boot_ns: u64,
        capacity: usize,
    ) -> Option<PublishedProcessProfile> {
        let Some(target) = target else {
            self.reset(None);
            return None;
        };
        if self.selected != Some(target) {
            self.reset(Some(target));
        }
        if self.ended {
            return self.last.clone();
        }

        let dir = self.proc_root.join(target.pid.to_string());
        let before = match read_stat(&dir) {
            Some(value) => value,
            None => return self.freeze_ended(),
        };
        if before.pid != target.pid || before.starttime != target.starttime {
            return self.freeze_ended();
        }
        let status = fs::read_to_string(dir.join("status"))
            .ok()
            .map(|value| parse_pid_status(&value))
            .unwrap_or_default();
        let io = fs::read_to_string(dir.join("io"))
            .ok()
            .and_then(|value| parse_pid_io(&value).ok());

        if self
            .last_slow_ns
            .is_none_or(|last| t_boot_ns.saturating_sub(last) >= SLOW_REFRESH_NS)
        {
            self.slow = read_slow_fields(&dir);
            self.last_slow_ns = Some(t_boot_ns);
        }

        let after = match read_stat(&dir) {
            Some(value) if value.pid == target.pid && value.starttime == target.starttime => value,
            _ => return self.freeze_ended(),
        };

        let ticks = after.utime.saturating_add(after.stime);
        let rates = rates(
            self.baseline.as_ref(),
            t_boot_ns,
            ticks,
            &after,
            io.as_ref(),
        );
        self.baseline = Some(Baseline {
            t_boot_ns,
            ticks,
            minor_faults: after.minor_faults,
            major_faults: after.major_faults,
            disk_read_bytes: io.as_ref().and_then(|v| v.read_bytes),
            disk_write_bytes: io.as_ref().and_then(|v| v.write_bytes),
        });

        let ticks_per_sec = clock_ticks_per_sec() as f64;
        let snapshot = ProcessProfileSnapshot {
            id: target,
            name: display_name.unwrap_or(&after.comm).to_string(),
            alive: true,
            state: after.state,
            age_secs: (t_boot_ns / 1_000_000_000)
                .saturating_sub(after.starttime / clock_ticks_per_sec()),
            parent_pid: after.ppid,
            uid: reading(status.uid, "uid unavailable"),
            thread_count: status.threads.unwrap_or(after.num_threads),
            priority: after.priority,
            nice: after.nice,
            cpu_percent: rates.cpu,
            user_cpu_secs: after.utime as f64 / ticks_per_sec,
            system_cpu_secs: after.stime as f64 / ticks_per_sec,
            minor_faults_per_sec: rates.minor_faults,
            major_faults_per_sec: rates.major_faults,
            rss_anon_kb: status.rss_anon_kb.or(table_rss_anon_kb).unwrap_or(0),
            rss_total_kb: reading(status.vm_rss_kb, "VmRSS unavailable"),
            rss_peak_kb: reading(status.vm_hwm_kb, "VmHWM unavailable"),
            swap_kb: reading(status.vm_swap_kb, "VmSwap unavailable"),
            pss_kb: self.slow.pss_kb.clone(),
            private_kb: self.slow.private_kb.clone(),
            disk_read_bytes_per_sec: rates.disk_read,
            disk_write_bytes_per_sec: rates.disk_write,
            disk_read_bytes: reading(
                io.as_ref().and_then(|v| v.read_bytes),
                "block read unavailable",
            ),
            disk_write_bytes: reading(
                io.as_ref().and_then(|v| v.write_bytes),
                "block write unavailable",
            ),
            command_line: self.slow.command_line.clone(),
            executable: self.slow.executable.clone(),
            cgroup: self.slow.cgroup.clone(),
            cpu_affinity: reading(status.cpu_affinity, "affinity unavailable"),
            open_fds: self.slow.open_fds.clone(),
        };

        let mut history = self
            .last
            .take()
            .map(|last| last.history)
            .unwrap_or_else(|| ProcessProfileHistory::new(capacity));
        history.push(t_boot_ns, &snapshot);
        let published = PublishedProcessProfile { snapshot, history };
        self.last = Some(published.clone());
        Some(published)
    }

    fn reset(&mut self, selected: Option<ProcessId>) {
        self.selected = selected;
        self.baseline = None;
        self.slow = SlowFields::default();
        self.last_slow_ns = None;
        self.last = None;
        self.ended = false;
    }

    fn freeze_ended(&mut self) -> Option<PublishedProcessProfile> {
        if let Some(last) = &mut self.last {
            self.ended = true;
            last.snapshot.alive = false;
        }
        self.last.clone()
    }
}

fn read_stat(dir: &Path) -> Option<crate::parse::PidStat> {
    fs::read_to_string(dir.join("stat"))
        .ok()
        .and_then(|value| parse_pid_stat(&value).ok())
}

fn read_slow_fields(dir: &Path) -> SlowFields {
    let smaps = fs::read_to_string(dir.join("smaps_rollup"))
        .ok()
        .map(|value| parse_smaps_rollup(&value));
    SlowFields {
        pss_kb: reading(
            smaps.as_ref().and_then(|value| value.pss_kb),
            "PSS unavailable",
        ),
        private_kb: reading(
            smaps.as_ref().and_then(|value| value.private_kb()),
            "private memory unavailable",
        ),
        command_line: fs::read(dir.join("cmdline"))
            .ok()
            .map(format_command_line)
            .filter(|value| !value.is_empty())
            .map(Reading::Value)
            .unwrap_or_else(|| unavailable("command line unavailable")),
        executable: fs::read_link(dir.join("exe"))
            .ok()
            .map(|value| Reading::Value(value.to_string_lossy().into_owned()))
            .unwrap_or_else(|| unavailable("executable unavailable")),
        cgroup: fs::read_to_string(dir.join("cgroup"))
            .ok()
            .and_then(|value| {
                value
                    .lines()
                    .find(|line| line.starts_with("0::"))
                    .or_else(|| value.lines().next())
                    .map(str::to_string)
            })
            .map(Reading::Value)
            .unwrap_or_else(|| unavailable("cgroup unavailable")),
        open_fds: count_open_fds(&dir.join("fd")),
    }
}

fn format_command_line(mut bytes: Vec<u8>) -> String {
    bytes.truncate(MAX_COMMAND_BYTES);
    String::from_utf8_lossy(&bytes)
        .split('\0')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn count_open_fds(path: &Path) -> Reading<OpenFdCount> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return unavailable("open files unavailable"),
    };
    let count = entries
        .take(MAX_OPEN_FDS + 1)
        .filter_map(Result::ok)
        .count();
    Reading::Value(OpenFdCount {
        count: count.min(MAX_OPEN_FDS),
        capped: count > MAX_OPEN_FDS,
    })
}

fn rates(
    previous: Option<&Baseline>,
    t_boot_ns: u64,
    ticks: u64,
    stat: &crate::parse::PidStat,
    io: Option<&crate::parse::PidIo>,
) -> RateReadings {
    let Some(previous) = previous else {
        return unavailable_rates("no profile baseline");
    };
    let wall_ns = t_boot_ns.saturating_sub(previous.t_boot_ns);
    if wall_ns == 0 {
        return unavailable_rates("zero profile interval");
    }
    let secs = wall_ns as f64 / 1_000_000_000.0;
    RateReadings {
        cpu: delta(ticks, previous.ticks)
            .map(|value| {
                Reading::Value(cpu_percent_of_machine(
                    value,
                    wall_ns,
                    clock_ticks_per_sec(),
                    online_logical_cpus(),
                ))
            })
            .unwrap_or_else(|| unavailable("CPU counter decreased")),
        minor_faults: rate(
            stat.minor_faults,
            previous.minor_faults,
            secs,
            "fault counter decreased",
        ),
        major_faults: rate(
            stat.major_faults,
            previous.major_faults,
            secs,
            "fault counter decreased",
        ),
        disk_read: optional_rate(
            io.and_then(|value| value.read_bytes),
            previous.disk_read_bytes,
            secs,
            "block read unavailable",
            "block read counter decreased",
        ),
        disk_write: optional_rate(
            io.and_then(|value| value.write_bytes),
            previous.disk_write_bytes,
            secs,
            "block write unavailable",
            "block write counter decreased",
        ),
    }
}

fn unavailable_rates(reason: &'static str) -> RateReadings {
    RateReadings {
        cpu: unavailable(reason),
        minor_faults: unavailable(reason),
        major_faults: unavailable(reason),
        disk_read: unavailable(reason),
        disk_write: unavailable(reason),
    }
}

fn rate(current: u64, previous: u64, secs: f64, reason: &'static str) -> Reading<f32> {
    delta(current, previous)
        .map(|value| Reading::Value((value as f64 / secs) as f32))
        .unwrap_or_else(|| unavailable(reason))
}

fn optional_rate(
    current: Option<u64>,
    previous: Option<u64>,
    secs: f64,
    unavailable_reason: &'static str,
    decrease_reason: &'static str,
) -> Reading<f32> {
    match (current, previous) {
        (Some(current), Some(previous)) => rate(current, previous, secs, decrease_reason),
        _ => unavailable(unavailable_reason),
    }
}

fn delta(current: u64, previous: u64) -> Option<u64> {
    current.checked_sub(previous)
}

fn reading<T>(value: Option<T>, reason: &'static str) -> Reading<T> {
    value
        .map(Reading::Value)
        .unwrap_or_else(|| unavailable(reason))
}

fn unavailable<T>(reason: &'static str) -> Reading<T> {
    Reading::Unavailable { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        id: ProcessId,
    }

    impl Fixture {
        fn new() -> Self {
            let unique = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "lightwatch-profile-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            let fixture = Self {
                root,
                id: ProcessId {
                    pid: 42,
                    starttime: 500,
                },
            };
            fixture.write(100, 20, 10, 1, 1000, 2000);
            fixture
        }

        fn write(&self, utime: u64, stime: u64, minor: u64, major: u64, read: u64, write: u64) {
            self.write_for(self.id, (utime, stime, minor, major, read, write));
        }

        fn write_for(&self, id: ProcessId, counters: (u64, u64, u64, u64, u64, u64)) {
            let (utime, stime, minor, major, read, write) = counters;
            let dir = self.root.join(id.pid.to_string());
            fs::create_dir_all(dir.join("fd")).unwrap();
            let after = format!(
                "S 1 1 1 0 -1 0 {minor} 0 {major} 0 {utime} {stime} 0 0 20 0 3 0 {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
                id.starttime
            );
            fs::write(dir.join("stat"), format!("{} (fixture) {after}", id.pid)).unwrap();
            fs::write(
                dir.join("status"),
                "Uid:\t1000 1000 1000 1000\nThreads:\t3\nVmRSS:\t200 kB\n\
                 VmHWM:\t300 kB\nRssAnon:\t150 kB\nVmSwap:\t4 kB\n\
                 Cpus_allowed_list:\t0-3\n",
            )
            .unwrap();
            fs::write(
                dir.join("io"),
                format!("read_bytes: {read}\nwrite_bytes: {write}\n"),
            )
            .unwrap();
            fs::write(
                dir.join("smaps_rollup"),
                "Pss: 120 kB\nPrivate_Clean: 5 kB\nPrivate_Dirty: 100 kB\n",
            )
            .unwrap();
            fs::write(dir.join("cmdline"), b"/bin/fixture\0--work\0").unwrap();
            fs::write(dir.join("cgroup"), "0::/user.slice/test.scope\n").unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn sticky_target_accumulates_rates_and_history() {
        let fixture = Fixture::new();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        let first = collector
            .sample(
                Some(fixture.id),
                Some("fixture"),
                Some(150),
                1_000_000_000,
                8,
            )
            .unwrap();
        assert!(matches!(
            first.snapshot.cpu_percent,
            Reading::Unavailable { .. }
        ));
        fixture.write(200, 40, 20, 3, 5000, 8000);
        let second = collector
            .sample(
                Some(fixture.id),
                Some("fixture"),
                Some(150),
                2_000_000_000,
                8,
            )
            .unwrap();
        assert!(matches!(second.snapshot.cpu_percent, Reading::Value(_)));
        assert_eq!(second.history.cpu.len(), 2);
        assert_eq!(
            second.snapshot.disk_read_bytes_per_sec,
            Reading::Value(4000.0)
        );
        assert_eq!(second.snapshot.minor_faults_per_sec, Reading::Value(10.0));
    }

    #[test]
    fn exit_freezes_last_good_profile_until_selection_clears() {
        let fixture = Fixture::new();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        let live = collector
            .sample(Some(fixture.id), None, None, 1_000_000_000, 8)
            .unwrap();
        fs::remove_file(fixture.root.join(fixture.id.pid.to_string()).join("stat")).unwrap();
        let ended = collector
            .sample(Some(fixture.id), None, None, 2_000_000_000, 8)
            .unwrap();
        assert!(!ended.snapshot.alive);
        assert_eq!(ended.history.cpu.len(), live.history.cpu.len());
        assert!(
            collector
                .sample(None, None, None, 3_000_000_000, 8)
                .is_none()
        );
    }

    #[test]
    fn denied_status_keeps_profile_and_uses_table_memory() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.root.join(fixture.id.pid.to_string()).join("status")).unwrap();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        let profile = collector
            .sample(Some(fixture.id), Some("fixture"), Some(777), 1, 8)
            .unwrap();
        assert!(profile.snapshot.alive);
        assert_eq!(profile.snapshot.rss_anon_kb, 777);
        assert!(matches!(profile.snapshot.uid, Reading::Unavailable { .. }));
    }

    #[test]
    fn transient_first_stat_miss_retries_instead_of_latching_ended() {
        let fixture = Fixture::new();
        let stat = fixture.root.join(fixture.id.pid.to_string()).join("stat");
        fs::remove_file(&stat).unwrap();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        assert!(
            collector
                .sample(Some(fixture.id), None, None, 1_000_000_000, 8)
                .is_none()
        );

        fixture.write(100, 20, 10, 1, 1000, 2000);
        let profile = collector
            .sample(Some(fixture.id), None, None, 2_000_000_000, 8)
            .unwrap();
        assert!(profile.snapshot.alive);
    }

    #[test]
    fn pid_reuse_freezes_the_original_identity() {
        let fixture = Fixture::new();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        collector
            .sample(Some(fixture.id), None, None, 1_000_000_000, 8)
            .unwrap();
        let replacement = ProcessId {
            pid: fixture.id.pid,
            starttime: fixture.id.starttime + 1,
        };
        fixture.write_for(replacement, (1, 1, 1, 1, 1, 1));

        let profile = collector
            .sample(Some(fixture.id), None, None, 2_000_000_000, 8)
            .unwrap();
        assert_eq!(profile.snapshot.id, fixture.id);
        assert!(!profile.snapshot.alive);
    }

    #[test]
    fn replacing_selection_starts_fresh_history() {
        let fixture = Fixture::new();
        let replacement = ProcessId {
            pid: 43,
            starttime: 600,
        };
        fixture.write_for(replacement, (5, 2, 1, 0, 50, 60));
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        collector
            .sample(Some(fixture.id), None, None, 1_000_000_000, 8)
            .unwrap();

        let profile = collector
            .sample(Some(replacement), None, None, 2_000_000_000, 8)
            .unwrap();
        assert_eq!(profile.snapshot.id, replacement);
        assert_eq!(profile.history.cpu.len(), 1);
        assert!(matches!(
            profile.snapshot.cpu_percent,
            Reading::Unavailable { .. }
        ));
    }

    #[test]
    fn resize_updates_all_six_rings_atomically() {
        let fixture = Fixture::new();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        collector
            .sample(Some(fixture.id), None, None, 1_000_000_000, 8)
            .unwrap();
        fixture.write(200, 40, 20, 3, 5000, 8000);
        collector
            .sample(Some(fixture.id), None, None, 2_000_000_000, 8)
            .unwrap();

        collector.resize(1).unwrap();
        let profile = collector
            .sample(Some(fixture.id), None, None, 3_000_000_000, 1)
            .unwrap();
        for ring in [
            &profile.history.cpu,
            &profile.history.rss_anon,
            &profile.history.minor_faults,
            &profile.history.major_faults,
            &profile.history.disk_read,
            &profile.history.disk_write,
        ] {
            assert_eq!(ring.capacity(), 1);
            assert_eq!(ring.len(), 1);
        }
    }

    #[test]
    fn decreased_counters_become_rate_gaps() {
        let fixture = Fixture::new();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        collector
            .sample(Some(fixture.id), None, None, 1_000_000_000, 8)
            .unwrap();
        fixture.write(1, 1, 1, 0, 1, 1);
        let profile = collector
            .sample(Some(fixture.id), None, None, 2_000_000_000, 8)
            .unwrap();

        for ring in [
            &profile.history.cpu,
            &profile.history.minor_faults,
            &profile.history.major_faults,
            &profile.history.disk_read,
            &profile.history.disk_write,
        ] {
            assert_eq!(ring.latest().unwrap().value, None);
        }
        assert!(profile.history.rss_anon.latest().unwrap().value.is_some());
    }

    #[test]
    fn explicit_discontinuity_gaps_every_profile_ring() {
        let fixture = Fixture::new();
        let mut collector = ProcessProfileCollector::new(&fixture.root);
        collector
            .sample(Some(fixture.id), None, None, 1_000_000_000, 8)
            .unwrap();
        collector.push_gap(1_500_000_000);
        let profile = collector.last.as_ref().unwrap();

        for ring in [
            &profile.history.cpu,
            &profile.history.rss_anon,
            &profile.history.minor_faults,
            &profile.history.major_faults,
            &profile.history.disk_read,
            &profile.history.disk_write,
        ] {
            let latest = ring.latest().unwrap();
            assert_eq!(latest.t_boot_ns, 1_500_000_000);
            assert_eq!(latest.value, None);
        }
    }
}
