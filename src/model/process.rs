//! Process table rows — latest-only, no history rings.

use super::snapshot::Reading;

/// Stable process identity across PID reuse: Linux `pid` + `stat` starttime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessId {
    pub pid: u32,
    pub starttime: u64,
}

/// One userspace process sample for the Processes tab.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRow {
    pub id: ProcessId,
    pub name: String,
    pub cpu_percent: Reading<f32>,
    /// Private anonymous memory (`RssAnon`) in kB — what the process
    /// uniquely owns. Sort/display key for the Memory column.
    pub mem_anon_kb: u64,
    pub disk_read_bytes: Reading<u64>,
    pub disk_write_bytes: Reading<u64>,
}

/// Column used for sorting the process table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessSortKey {
    Name,
    Cpu,
    Memory,
    DiskRead,
    DiskWrite,
    Pid,
}

/// Pure filter: case-insensitive name substring; digit-only query also matches
/// **pid prefix** (exact qualifies; interior digit substring does not).
pub fn process_matches(row: &ProcessRow, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    if q.chars().all(|c| c.is_ascii_digit()) {
        let pid_str = row.id.pid.to_string();
        if pid_str.starts_with(q) {
            return true;
        }
    }
    row.name
        .to_ascii_lowercase()
        .contains(&q.to_ascii_lowercase())
}

/// Sort key comparison with total order:
/// - available values ordered by `desc` (true = larger first)
/// - **unavailable always last** in both directions
/// - PID ascending as final tie-break
pub fn cmp_process_rows(
    a: &ProcessRow,
    b: &ProcessRow,
    key: ProcessSortKey,
    desc: bool,
) -> std::cmp::Ordering {
    let primary = match key {
        ProcessSortKey::Name => {
            let ord = a
                .name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase());
            if desc {
                ord.reverse()
            } else {
                ord
            }
        }
        ProcessSortKey::Cpu => cmp_reading_f32(&a.cpu_percent, &b.cpu_percent, desc),
        ProcessSortKey::Memory => {
            let ord = a.mem_anon_kb.cmp(&b.mem_anon_kb);
            if desc {
                ord.reverse()
            } else {
                ord
            }
        }
        ProcessSortKey::DiskRead => cmp_reading_u64(&a.disk_read_bytes, &b.disk_read_bytes, desc),
        ProcessSortKey::DiskWrite => {
            cmp_reading_u64(&a.disk_write_bytes, &b.disk_write_bytes, desc)
        }
        ProcessSortKey::Pid => {
            let ord = a.id.pid.cmp(&b.id.pid);
            if desc {
                ord.reverse()
            } else {
                ord
            }
        }
    };
    primary.then_with(|| a.id.pid.cmp(&b.id.pid))
}

fn cmp_reading_f32(a: &Reading<f32>, b: &Reading<f32>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.value(), b.value()) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater, // unavailable last
        (Some(_), None) => Ordering::Less,
        (Some(av), Some(bv)) => {
            let ord = av.partial_cmp(bv).unwrap_or(Ordering::Equal);
            if desc {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

fn cmp_reading_u64(a: &Reading<u64>, b: &Reading<u64>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.value(), b.value()) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(av), Some(bv)) => {
            let ord = av.cmp(bv);
            if desc {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// Result of applying search + sort (full filtered set — no display cap).
#[derive(Clone, Debug, PartialEq)]
pub struct VisibleProcesses {
    /// Rows to render (already sorted).
    pub rows: Vec<ProcessRow>,
    /// Same as `rows.len()`; kept for UI count labels.
    pub match_count: usize,
}

/// Filter and sort process rows for the table body (all matches; UI scrolls).
pub fn visible_processes(
    all: &[ProcessRow],
    query: &str,
    key: ProcessSortKey,
    desc: bool,
) -> VisibleProcesses {
    let mut matched: Vec<&ProcessRow> = all
        .iter()
        .filter(|r| process_matches(r, query))
        .collect();
    matched.sort_by(|a, b| cmp_process_rows(a, b, key, desc));
    let match_count = matched.len();
    let rows = matched.into_iter().cloned().collect();
    VisibleProcesses { rows, match_count }
}

/// True when `id` appears in the current visible set (search + sort).
pub fn selection_is_visible(
    all: &[ProcessRow],
    query: &str,
    key: ProcessSortKey,
    desc: bool,
    id: ProcessId,
) -> bool {
    visible_processes(all, query, key, desc)
        .rows
        .iter()
        .any(|r| r.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu: Option<f32>, mem: u64, dread: Option<u64>) -> ProcessRow {
        ProcessRow {
            id: ProcessId {
                pid,
                starttime: pid as u64 * 10,
            },
            name: name.into(),
            cpu_percent: match cpu {
                Some(v) => Reading::Value(v),
                None => Reading::Unavailable {
                    reason: "no baseline",
                },
            },
            mem_anon_kb: mem,
            disk_read_bytes: match dread {
                Some(v) => Reading::Value(v),
                None => Reading::Unavailable {
                    reason: "no io",
                },
            },
            disk_write_bytes: Reading::Unavailable { reason: "no io" },
        }
    }

    #[test]
    fn name_match_case_insensitive() {
        let r = row(1, "LightWatch", Some(1.0), 100, None);
        assert!(process_matches(&r, "light"));
        assert!(process_matches(&r, "WATCH"));
        assert!(!process_matches(&r, "chrome"));
    }

    #[test]
    fn pid_prefix_not_interior() {
        let r = row(12345, "x", Some(1.0), 100, None);
        assert!(process_matches(&r, "123"));
        assert!(process_matches(&r, "12345"));
        assert!(!process_matches(&r, "234")); // interior
        assert!(!process_matches(&r, "45"));
    }

    #[test]
    fn digit_query_also_tries_name() {
        let r = row(7, "codec2", Some(1.0), 100, None);
        assert!(process_matches(&r, "2")); // name substring
    }

    #[test]
    fn sort_memory_desc_default_top() {
        let all = vec![
            row(1, "a", Some(1.0), 100, None),
            row(2, "b", Some(1.0), 500, None),
            row(3, "c", Some(1.0), 200, None),
        ];
        let v = visible_processes(&all, "", ProcessSortKey::Memory, true);
        assert_eq!(v.rows[0].id.pid, 2);
        assert_eq!(v.rows[1].id.pid, 3);
        assert_eq!(v.rows[2].id.pid, 1);
    }

    #[test]
    fn unavailable_cpu_sorts_last_both_directions() {
        let all = vec![
            row(1, "a", None, 100, None),
            row(2, "b", Some(50.0), 100, None),
            row(3, "c", Some(10.0), 100, None),
        ];
        let desc = visible_processes(&all, "", ProcessSortKey::Cpu, true);
        assert_eq!(
            desc.rows.iter().map(|r| r.id.pid).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
        let asc = visible_processes(&all, "", ProcessSortKey::Cpu, false);
        assert_eq!(
            asc.rows.iter().map(|r| r.id.pid).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    #[test]
    fn empty_query_shows_all_sorted() {
        let all: Vec<_> = (1..=30)
            .map(|i| row(i, "p", Some(1.0), i as u64 * 10, None))
            .collect();
        let v = visible_processes(&all, "", ProcessSortKey::Memory, true);
        assert_eq!(v.rows.len(), 30);
        assert_eq!(v.match_count, 30);
        assert_eq!(v.rows[0].id.pid, 30);
        assert_eq!(v.rows[29].id.pid, 1);
    }

    #[test]
    fn search_finds_quiet_process() {
        let mut all: Vec<_> = (1..=25)
            .map(|i| row(i, "hog", Some(90.0), 10_000, None))
            .collect();
        all.push(row(99, "lightwatch", Some(0.5), 50, None));
        let all_view = visible_processes(&all, "", ProcessSortKey::Memory, true);
        assert!(all_view.rows.iter().any(|r| r.name == "lightwatch"));
        let found = visible_processes(&all, "light", ProcessSortKey::Memory, true);
        assert_eq!(found.match_count, 1);
        assert_eq!(found.rows[0].name, "lightwatch");
    }

    #[test]
    fn pid_tiebreak_stable() {
        let all = vec![
            row(5, "a", Some(10.0), 100, None),
            row(3, "b", Some(10.0), 100, None),
            row(7, "c", Some(10.0), 100, None),
        ];
        let v = visible_processes(&all, "", ProcessSortKey::Memory, true);
        assert_eq!(
            v.rows.iter().map(|r| r.id.pid).collect::<Vec<_>>(),
            vec![3, 5, 7]
        );
    }

    #[test]
    fn selection_hidden_by_filter() {
        let all = vec![
            row(1, "chrome", Some(1.0), 100, None),
            row(2, "lightwatch", Some(1.0), 50, None),
        ];
        let id = ProcessId {
            pid: 1,
            starttime: 10,
        };
        assert!(selection_is_visible(
            &all,
            "",
            ProcessSortKey::Memory,
            true,
            id
        ));
        assert!(!selection_is_visible(
            &all,
            "light",
            ProcessSortKey::Memory,
            true,
            id
        ));
    }
}
