//! Process table collector — scan `/proc`, CPU deltas, userspace rows.

use crate::model::{ProcessId, ProcessRow, Reading, display_name, is_helper_cmdline};
use crate::parse::{parse_pid_io, parse_pid_stat, parse_self_status};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct CpuBaseline {
    ticks: u64, // utime + stime
    t_boot_ns: u64,
    starttime: u64,
}

/// Collects all userspace processes each sample.
pub struct ProcessCollector {
    proc_root: PathBuf,
    prev: HashMap<ProcessId, CpuBaseline>,
}

impl ProcessCollector {
    pub fn new(proc_root: impl Into<PathBuf>) -> Self {
        Self {
            proc_root: proc_root.into(),
            prev: HashMap::new(),
        }
    }

    /// Drop all CPU baselines (after suspend / clock discontinuity).
    pub fn clear_baseline(&mut self) {
        self.prev.clear();
    }

    /// Scan `/proc` and return userspace process rows.
    ///
    /// `t_boot_ns` is the sampler's boottime stamp for this tick — used for
    /// wall-time in CPU%. Percent is of **total machine capacity** (all
    /// online logical CPUs): one fully busy core on a 16-CPU host ≈ 6.25%.
    pub fn sample(&mut self, t_boot_ns: u64) -> Vec<ProcessRow> {
        let n_cpus = online_logical_cpus();
        let mut rows = Vec::new();
        let mut seen = HashMap::new();

        let entries = match fs::read_dir(&self.proc_root) {
            Ok(e) => e,
            Err(_) => return rows,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let pid: u32 = match name.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let dir = entry.path();
            if let Some(row) = self.sample_one(pid, &dir, t_boot_ns, n_cpus, &mut seen) {
                rows.push(row);
            }
        }

        // Drop baselines for processes that disappeared.
        self.prev.retain(|id, _| seen.contains_key(id));
        // Store new baselines.
        for (id, base) in seen {
            self.prev.insert(id, base);
        }

        rows
    }

    fn sample_one(
        &self,
        pid: u32,
        dir: &Path,
        t_boot_ns: u64,
        n_cpus: u32,
        seen: &mut HashMap<ProcessId, CpuBaseline>,
    ) -> Option<ProcessRow> {
        let stat_content = fs::read_to_string(dir.join("stat")).ok()?;
        let stat = parse_pid_stat(&stat_content).ok()?;
        if stat.pid != pid {
            // Path pid and stat pid should match; skip oddities.
            return None;
        }

        let status_content = fs::read_to_string(dir.join("status")).ok()?;
        let status = parse_self_status(&status_content).ok()?;
        // Inclusion: must look like userspace (VmRSS present). Display/sort
        // memory is private RssAnon (GSM-style "what does this process own?").
        let _vm_rss = status.vm_rss_kb?;
        let mem_anon_kb = status.rss_anon_kb.unwrap_or(0);

        let id = ProcessId {
            pid,
            starttime: stat.starttime,
        };
        let ticks = stat.utime.saturating_add(stat.stime);

        let cpu_percent = match self.prev.get(&id) {
            Some(prev) if prev.starttime == stat.starttime => {
                let wall_ns = t_boot_ns.saturating_sub(prev.t_boot_ns);
                if wall_ns == 0 {
                    Reading::Unavailable {
                        reason: "zero wall time",
                    }
                } else if ticks < prev.ticks {
                    Reading::Unavailable {
                        reason: "cpu counter decrease",
                    }
                } else {
                    let tick_delta = ticks - prev.ticks;
                    Reading::Value(cpu_percent_of_machine(
                        tick_delta,
                        wall_ns,
                        clock_ticks_per_sec(),
                        n_cpus,
                    ))
                }
            }
            _ => Reading::Unavailable {
                reason: "no process CPU baseline",
            },
        };

        seen.insert(
            id,
            CpuBaseline {
                ticks,
                t_boot_ns,
                starttime: stat.starttime,
            },
        );

        let (disk_read_bytes, disk_write_bytes) = match fs::read_to_string(dir.join("io")) {
            Ok(content) => match parse_pid_io(&content) {
                Ok(io) => (
                    io.read_bytes
                        .map(Reading::Value)
                        .unwrap_or(Reading::Unavailable {
                            reason: "read_bytes missing",
                        }),
                    io.write_bytes
                        .map(Reading::Value)
                        .unwrap_or(Reading::Unavailable {
                            reason: "write_bytes missing",
                        }),
                ),
                Err(_) => (
                    Reading::Unavailable {
                        reason: "cannot parse io",
                    },
                    Reading::Unavailable {
                        reason: "cannot parse io",
                    },
                ),
            },
            Err(_) => (
                Reading::Unavailable {
                    reason: "io unreadable",
                },
                Reading::Unavailable {
                    reason: "io unreadable",
                },
            ),
        };

        // cmdline: Electron --type= + argv0 basename when exe is unavailable.
        // exe basename beats truncated `comm` (15-char kernel limit).
        let cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
        let exe_base = fs::read_link(dir.join("exe"))
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let name = display_name(&stat.comm, &cmdline, exe_base.as_deref(), pid);

        Some(ProcessRow {
            id,
            name,
            cpu_percent,
            mem_anon_kb,
            disk_read_bytes,
            disk_write_bytes,
        })
    }
}

pub(crate) fn clock_ticks_per_sec() -> u64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as u64 } else { 100 }
}

/// Online logical CPUs for total-capacity CPU% (at least 1).
pub(crate) fn online_logical_cpus() -> u32 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as u32 } else { 1 }
}

/// Process CPU as a percentage of **total** machine capacity.
///
/// `tick_delta` is utime+stime delta; `wall_ns` is wall time between samples;
/// `n_cpus` is online logical CPUs. One core fully busy → `100 / n_cpus`.
pub fn cpu_percent_of_machine(tick_delta: u64, wall_ns: u64, clk_tck: u64, n_cpus: u32) -> f32 {
    if wall_ns == 0 || clk_tck == 0 || n_cpus == 0 {
        return 0.0;
    }
    let cpu_secs = tick_delta as f64 / clk_tck as f64;
    let wall_secs = wall_ns as f64 / 1_000_000_000.0;
    let capacity_secs = wall_secs * n_cpus as f64;
    if capacity_secs <= 0.0 {
        return 0.0;
    }
    (cpu_secs / capacity_secs * 100.0) as f32
}

/// Outcome of an End Process attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillOutcome {
    SignalSent,
    /// Helper was selected; SIGTERM went to the resolved app-root pid.
    SignalSentToRoot {
        root_pid: u32,
    },
    Gone,
    IdentityMismatch,
    PermissionDenied,
    Failed(String),
}

/// Classify a `kill(2)` result (pure; testable).
pub fn classify_kill_result(rc: i32, errno: i32) -> KillOutcome {
    if rc == 0 {
        return KillOutcome::SignalSent;
    }
    match errno {
        e if e == libc::ESRCH => KillOutcome::Gone,
        e if e == libc::EPERM => KillOutcome::PermissionDenied,
        e => KillOutcome::Failed(format!("errno {e}")),
    }
}

/// Re-read `/proc/pid/stat`, verify `(pid, starttime)`, then SIGTERM.
///
/// If the selected process is a Chromium/Electron helper (`--type=` on
/// cmdline), walk parents to the app root and signal **that** process
/// instead — so "End Process" on `slack (renderer)` closes Slack rather than
/// blanking one view.
///
/// Residual TOCTOU between the identity check and `kill` is accepted for v1
/// (no pidfd). Fail closed on mismatch or missing process.
pub fn end_process(proc_root: &Path, id: ProcessId) -> KillOutcome {
    // Confirm the selection still matches before any parent walk.
    if !identity_matches(proc_root, id) {
        return if proc_root.join(id.pid.to_string()).join("stat").exists() {
            KillOutcome::IdentityMismatch
        } else {
            KillOutcome::Gone
        };
    }

    let target = resolve_end_target(proc_root, id);
    if !identity_matches(proc_root, target) {
        return KillOutcome::IdentityMismatch;
    }

    let rc = unsafe { libc::kill(target.pid as i32, libc::SIGTERM) };
    if rc == 0 {
        if target != id {
            KillOutcome::SignalSentToRoot {
                root_pid: target.pid,
            }
        } else {
            KillOutcome::SignalSent
        }
    } else {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        classify_kill_result(rc, errno)
    }
}

fn identity_matches(proc_root: &Path, id: ProcessId) -> bool {
    let content = match fs::read_to_string(proc_root.join(id.pid.to_string()).join("stat")) {
        Ok(c) => c,
        Err(_) => return false,
    };
    match parse_pid_stat(&content) {
        Ok(p) => p.pid == id.pid && p.starttime == id.starttime,
        Err(_) => false,
    }
}

/// Walk from a helper process up to the first ancestor without `--type=`.
///
/// Bounded walk; fails closed to the original id if the chain breaks.
pub fn resolve_end_target(proc_root: &Path, id: ProcessId) -> ProcessId {
    let mut current = id;
    let mut visited = std::collections::HashSet::from([id]);
    for _ in 0..32 {
        let dir = proc_root.join(current.pid.to_string());
        let stat = match fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|content| parse_pid_stat(&content).ok())
        {
            Some(stat) if stat.pid == current.pid && stat.starttime == current.starttime => stat,
            _ => return id,
        };
        let cmdline = match fs::read(dir.join("cmdline")) {
            Ok(cmdline) if !cmdline.is_empty() => cmdline,
            _ => return id,
        };
        if !is_helper_cmdline(&cmdline) {
            return current;
        }
        if stat.ppid <= 1 {
            return id;
        }
        let parent_dir = proc_root.join(stat.ppid.to_string());
        let parent_stat = match fs::read_to_string(parent_dir.join("stat")) {
            Ok(c) => match parse_pid_stat(&c) {
                Ok(s) => s,
                Err(_) => return id,
            },
            Err(_) => return id,
        };
        let parent = ProcessId {
            pid: parent_stat.pid,
            starttime: parent_stat.starttime,
        };
        if parent.pid != stat.ppid || !visited.insert(parent) {
            return id;
        }
        current = parent;
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIX_ID: AtomicU64 = AtomicU64::new(0);

    struct ProcTree {
        root: PathBuf,
    }

    impl ProcTree {
        /// Create a uniquely owned empty directory. Uses `create_dir` (not
        /// `create_dir_all`) so a pre-existing path cannot be claimed; `Drop`
        /// only removes what we created.
        fn new() -> Self {
            let id = FIX_ID.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!(
                "lightwatch-proc-{}-{id}-{nanos}",
                std::process::id()
            ));
            fs::create_dir(&root).expect("unique process fixture dir");
            Self { root }
        }

        fn add_proc(&self, spec: ProcSpec<'_>) {
            let dir = self.root.join(spec.pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            // After comm: state ppid pgrp session tty tpgid flags ... utime stime ... starttime
            let after = format!(
                "S {} 1 1 0 -1 0 0 0 0 0 {} {} 0 0 20 0 1 0 {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
                spec.ppid, spec.utime, spec.stime, spec.starttime
            );
            let stat = format!("{} ({}) {after}", spec.pid, spec.comm);
            fs::write(dir.join("stat"), stat).unwrap();
            let mut status = format!("Name:\t{}\n", spec.comm);
            if let Some(kb) = spec.vm_rss_kb {
                status.push_str(&format!("VmRSS:\t{kb} kB\n"));
            }
            // Prefer explicit Anon; tests that only set VmRSS get matching Anon.
            if let Some(kb) = spec.rss_anon_kb.or(spec.vm_rss_kb) {
                status.push_str(&format!("RssAnon:\t{kb} kB\n"));
            }
            fs::write(dir.join("status"), status).unwrap();
            if let Some((r, w)) = spec.io {
                fs::write(
                    dir.join("io"),
                    format!("read_bytes: {r}\nwrite_bytes: {w}\n"),
                )
                .unwrap();
            }
            if let Some(cmd) = spec.cmdline {
                let mut bytes = Vec::new();
                for (i, part) in cmd.iter().enumerate() {
                    if i > 0 {
                        bytes.push(0);
                    }
                    bytes.extend_from_slice(part.as_bytes());
                }
                bytes.push(0);
                fs::write(dir.join("cmdline"), bytes).unwrap();
            }
        }
    }

    struct ProcSpec<'a> {
        pid: u32,
        comm: &'a str,
        ppid: u32,
        utime: u64,
        stime: u64,
        starttime: u64,
        vm_rss_kb: Option<u64>,
        /// Defaults to `vm_rss_kb` when omitted (tests that only care about inclusion).
        rss_anon_kb: Option<u64>,
        io: Option<(u64, u64)>,
        cmdline: Option<&'a [&'a str]>,
    }

    impl Drop for ProcTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn skips_without_vmrss() {
        let tree = ProcTree::new();
        tree.add_proc(ProcSpec {
            pid: 1,
            comm: "kthread",
            ppid: 0,
            utime: 0,
            stime: 0,
            starttime: 100,
            vm_rss_kb: None,
            rss_anon_kb: None,
            io: None,
            cmdline: None,
        });
        tree.add_proc(ProcSpec {
            pid: 2,
            comm: "app",
            ppid: 1,
            utime: 10,
            stime: 5,
            starttime: 200,
            vm_rss_kb: Some(4096),
            rss_anon_kb: None,
            io: Some((100, 200)),
            cmdline: Some(&["app"]),
        });
        let mut c = ProcessCollector::new(&tree.root);
        let rows = c.sample(1_000_000_000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "app");
        assert_eq!(rows[0].mem_anon_kb, 4096);
        assert!(matches!(rows[0].cpu_percent, Reading::Unavailable { .. }));
    }

    #[test]
    fn cpu_percent_on_second_sample() {
        let tree = ProcTree::new();
        tree.add_proc(ProcSpec {
            pid: 7,
            comm: "busy",
            ppid: 1,
            utime: 100,
            stime: 0,
            starttime: 50,
            vm_rss_kb: Some(1024),
            rss_anon_kb: None,
            io: None,
            cmdline: None,
        });
        let mut c = ProcessCollector::new(&tree.root);
        let _ = c.sample(1_000_000_000);
        // Advance ticks: +100 ticks over 1 second wall → one full core-second.
        // As a fraction of the whole machine: 100 / n_cpus.
        tree.add_proc(ProcSpec {
            pid: 7,
            comm: "busy",
            ppid: 1,
            utime: 200,
            stime: 0,
            starttime: 50,
            vm_rss_kb: Some(1024),
            rss_anon_kb: None,
            io: None,
            cmdline: None,
        });
        let rows = c.sample(2_000_000_000);
        assert_eq!(rows.len(), 1);
        let pct = rows[0].cpu_percent.value().copied().unwrap();
        let n = online_logical_cpus() as f32;
        let expected = 100.0 / n; // one saturated core on this host
        assert!(
            (pct - expected).abs() < 0.5,
            "pct={pct} expected≈{expected} (n_cpus={n})"
        );
    }

    #[test]
    fn cpu_percent_of_machine_scales_with_cores() {
        // 1 core-second of work over 1s wall.
        let one_core = cpu_percent_of_machine(100, 1_000_000_000, 100, 1);
        assert!((one_core - 100.0).abs() < 0.01, "one_core={one_core}");
        let sixteen = cpu_percent_of_machine(100, 1_000_000_000, 100, 16);
        assert!((sixteen - 6.25).abs() < 0.01, "sixteen={sixteen}");
        // 136% of one core on 16 CPUs ≈ 8.5% of machine (GSM-style).
        let wild = cpu_percent_of_machine(136, 1_000_000_000, 100, 16);
        assert!((wild - 8.5).abs() < 0.01, "wild={wild}");
    }

    #[test]
    fn pid_reuse_new_starttime_rebaselines() {
        let tree = ProcTree::new();
        tree.add_proc(ProcSpec {
            pid: 7,
            comm: "old",
            ppid: 1,
            utime: 100,
            stime: 0,
            starttime: 50,
            vm_rss_kb: Some(1024),
            rss_anon_kb: None,
            io: None,
            cmdline: None,
        });
        let mut c = ProcessCollector::new(&tree.root);
        let _ = c.sample(1_000_000_000);
        // Same pid, new starttime → new process; no CPU yet.
        tree.add_proc(ProcSpec {
            pid: 7,
            comm: "new",
            ppid: 1,
            utime: 500,
            stime: 0,
            starttime: 999,
            vm_rss_kb: Some(2048),
            rss_anon_kb: None,
            io: None,
            cmdline: None,
        });
        let rows = c.sample(2_000_000_000);
        assert_eq!(rows[0].id.starttime, 999);
        assert!(matches!(rows[0].cpu_percent, Reading::Unavailable { .. }));
        assert_eq!(rows[0].mem_anon_kb, 2048);
    }

    #[test]
    fn classify_kill_errno() {
        assert_eq!(classify_kill_result(0, 0), KillOutcome::SignalSent);
        assert_eq!(classify_kill_result(-1, libc::ESRCH), KillOutcome::Gone);
        assert_eq!(
            classify_kill_result(-1, libc::EPERM),
            KillOutcome::PermissionDenied
        );
    }

    #[test]
    fn end_process_identity_mismatch() {
        let tree = ProcTree::new();
        tree.add_proc(ProcSpec {
            pid: 42,
            comm: "x",
            ppid: 1,
            utime: 1,
            stime: 0,
            starttime: 100,
            vm_rss_kb: Some(1),
            rss_anon_kb: None,
            io: None,
            cmdline: None,
        });
        let outcome = end_process(
            &tree.root,
            ProcessId {
                pid: 42,
                starttime: 999, // wrong
            },
        );
        assert_eq!(outcome, KillOutcome::IdentityMismatch);
    }

    #[test]
    fn end_process_gone() {
        let tree = ProcTree::new();
        let outcome = end_process(
            &tree.root,
            ProcessId {
                pid: 404,
                starttime: 1,
            },
        );
        assert_eq!(outcome, KillOutcome::Gone);
    }

    #[test]
    fn resolve_end_target_walks_electron_helper_to_root() {
        let tree = ProcTree::new();
        // main slack
        tree.add_proc(ProcSpec {
            pid: 100,
            comm: "slack",
            ppid: 1,
            utime: 1,
            stime: 0,
            starttime: 1000,
            vm_rss_kb: Some(50_000),
            rss_anon_kb: None,
            io: None,
            cmdline: Some(&["/usr/lib/slack/slack"]),
        });
        // renderer child
        tree.add_proc(ProcSpec {
            pid: 200,
            comm: "slack",
            ppid: 100,
            utime: 1,
            stime: 0,
            starttime: 2000,
            vm_rss_kb: Some(80_000),
            rss_anon_kb: None,
            io: None,
            cmdline: Some(&["/usr/lib/slack/slack", "--type=renderer"]),
        });
        let helper = ProcessId {
            pid: 200,
            starttime: 2000,
        };
        let root = resolve_end_target(&tree.root, helper);
        assert_eq!(root.pid, 100);
        assert_eq!(root.starttime, 1000);

        // Name labeling
        let mut c = ProcessCollector::new(&tree.root);
        let rows = c.sample(1_000_000_000);
        let renderer = rows.iter().find(|r| r.id.pid == 200).unwrap();
        assert_eq!(renderer.name, "slack (renderer)");
        let main = rows.iter().find(|r| r.id.pid == 100).unwrap();
        assert_eq!(main.name, "slack");
    }

    #[test]
    fn resolve_end_target_falls_back_when_parent_cmdline_is_unreadable_or_empty() {
        let tree = ProcTree::new();
        tree.add_proc(ProcSpec {
            pid: 100,
            comm: "slack",
            ppid: 1,
            utime: 1,
            stime: 0,
            starttime: 1000,
            vm_rss_kb: Some(1),
            rss_anon_kb: None,
            io: None,
            cmdline: None,
        });
        tree.add_proc(ProcSpec {
            pid: 200,
            comm: "slack",
            ppid: 100,
            utime: 1,
            stime: 0,
            starttime: 2000,
            vm_rss_kb: Some(1),
            rss_anon_kb: None,
            io: None,
            cmdline: Some(&["slack", "--type=renderer"]),
        });
        let selected = ProcessId {
            pid: 200,
            starttime: 2000,
        };

        assert_eq!(resolve_end_target(&tree.root, selected), selected);

        fs::write(tree.root.join("100/cmdline"), []).unwrap();
        assert_eq!(resolve_end_target(&tree.root, selected), selected);
    }

    #[test]
    fn resolve_end_target_falls_back_when_parent_stat_has_another_pid() {
        let tree = ProcTree::new();
        tree.add_proc(ProcSpec {
            pid: 100,
            comm: "slack",
            ppid: 1,
            utime: 1,
            stime: 0,
            starttime: 1000,
            vm_rss_kb: Some(1),
            rss_anon_kb: None,
            io: None,
            cmdline: Some(&["slack"]),
        });
        tree.add_proc(ProcSpec {
            pid: 200,
            comm: "slack",
            ppid: 100,
            utime: 1,
            stime: 0,
            starttime: 2000,
            vm_rss_kb: Some(1),
            rss_anon_kb: None,
            io: None,
            cmdline: Some(&["slack", "--type=renderer"]),
        });
        let selected = ProcessId {
            pid: 200,
            starttime: 2000,
        };

        let parent_stat = fs::read_to_string(tree.root.join("100/stat")).unwrap();
        fs::write(
            tree.root.join("100/stat"),
            parent_stat.replacen("100 (", "101 (", 1),
        )
        .unwrap();

        assert_eq!(resolve_end_target(&tree.root, selected), selected);
    }

    #[test]
    fn resolve_end_target_falls_back_on_broken_or_cyclic_ancestry() {
        let tree = ProcTree::new();
        for (pid, ppid, starttime) in [(200, 300, 2000), (300, 200, 3000)] {
            tree.add_proc(ProcSpec {
                pid,
                comm: "slack",
                ppid,
                utime: 1,
                stime: 0,
                starttime,
                vm_rss_kb: Some(1),
                rss_anon_kb: None,
                io: None,
                cmdline: Some(&["slack", "--type=renderer"]),
            });
        }
        let selected = ProcessId {
            pid: 200,
            starttime: 2000,
        };
        assert_eq!(resolve_end_target(&tree.root, selected), selected);

        fs::write(tree.root.join("300/stat"), "malformed").unwrap();
        assert_eq!(resolve_end_target(&tree.root, selected), selected);

        fs::remove_file(tree.root.join("300/stat")).unwrap();
        assert_eq!(resolve_end_target(&tree.root, selected), selected);
    }

    #[test]
    fn resolve_end_target_falls_back_at_depth_limit() {
        let tree = ProcTree::new();
        for pid in 200..=232 {
            tree.add_proc(ProcSpec {
                pid,
                comm: "slack",
                ppid: pid + 1,
                utime: 1,
                stime: 0,
                starttime: u64::from(pid) * 10,
                vm_rss_kb: Some(1),
                rss_anon_kb: None,
                io: None,
                cmdline: Some(&["slack", "--type=renderer"]),
            });
        }
        tree.add_proc(ProcSpec {
            pid: 233,
            comm: "slack",
            ppid: 1,
            utime: 1,
            stime: 0,
            starttime: 2330,
            vm_rss_kb: Some(1),
            rss_anon_kb: None,
            io: None,
            cmdline: Some(&["slack"]),
        });
        let selected = ProcessId {
            pid: 200,
            starttime: 2000,
        };

        assert_eq!(resolve_end_target(&tree.root, selected), selected);
    }

    /// Live SIGTERM against an owned disposable child, with starttime verify.
    #[test]
    fn end_process_signals_owned_child() {
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::Duration;

        let mut child = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        // Wait briefly so /proc/pid/stat is visible.
        thread::sleep(Duration::from_millis(50));
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("child stat");
        let parsed = parse_pid_stat(&stat).expect("parse child stat");
        assert_eq!(parsed.pid, pid);

        let outcome = end_process(
            Path::new("/proc"),
            ProcessId {
                pid,
                starttime: parsed.starttime,
            },
        );
        assert_eq!(outcome, KillOutcome::SignalSent);

        // Reap; process should exit promptly after SIGTERM.
        let status = child.wait().expect("wait child");
        assert!(
            !status.success() || status.code().is_none(),
            "child should not exit 0 after SIGTERM: {status:?}"
        );
    }
}
