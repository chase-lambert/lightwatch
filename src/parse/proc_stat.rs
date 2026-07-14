/// Parse /proc/stat — CPU aggregate and per-core counters.
/// Returns (cpu_total_jiffies, Vec<(label, jiffies)>).
/// The "cpu " line is aggregate; "cpuN" lines are per-core.
/// Formula: user + nice + system + idle + iowait + irq + softirq + steal
/// (guest is already accounted in user/nice on Linux, so we do NOT add guest separately.)
pub fn parse_proc_stat(content: &str) -> Result<ProcStat, ParseError> {
    let mut total: Option<CpuJiffies> = None;
    let mut cores: Vec<(String, CpuJiffies)> = Vec::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("cpu") {
            if rest.is_empty() {
                continue; // skip bare "cpu" line
            }
            // "cpu " is the aggregate line (rest starts with space)
            if rest.starts_with(' ') {
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if fields.len() < 8 {
                    continue;
                }
                let j = parse_jiffies(&fields)?;
                total = Some(j);
            } else {
                // "cpuN" is a per-core line; extract label
                let (label, rest2) = rest.split_once(' ').unwrap_or((rest, ""));
                let name = format!("cpu{label}");
                let fields: Vec<&str> = rest2.split_whitespace().collect();
                if fields.len() < 8 {
                    continue;
                }
                let j = parse_jiffies(&fields)?;
                cores.push((name, j));
            }
        }
    }

    match total {
        Some(t) => Ok(ProcStat { total: t, cores }),
        None => Err(ParseError::MissingField("cpu aggregate line")),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcStat {
    pub total: CpuJiffies,
    pub cores: Vec<(String, CpuJiffies)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CpuJiffies {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuJiffies {
    pub fn total(&self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }

    pub fn idle_total(&self) -> u64 {
        self.idle.saturating_add(self.iowait)
    }
}

fn parse_jiffies(fields: &[&str]) -> Result<CpuJiffies, ParseError> {
    Ok(CpuJiffies {
        user: fields[0].parse().map_err(|_| ParseError::NotANumber)?,
        nice: fields[1].parse().map_err(|_| ParseError::NotANumber)?,
        system: fields[2].parse().map_err(|_| ParseError::NotANumber)?,
        idle: fields[3].parse().map_err(|_| ParseError::NotANumber)?,
        iowait: fields[4].parse().map_err(|_| ParseError::NotANumber)?,
        irq: fields[5].parse().map_err(|_| ParseError::NotANumber)?,
        softirq: fields[6].parse().map_err(|_| ParseError::NotANumber)?,
        steal: fields[7].parse().map_err(|_| ParseError::NotANumber)?,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseError {
    MissingField(&'static str),
    NotANumber,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingField(field) => write!(f, "missing field: {field}"),
            ParseError::NotANumber => write!(f, "not a number"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Check if any individual counter field decreased between readings.
pub fn fields_decreased(prev: &CpuJiffies, curr: &CpuJiffies) -> bool {
    curr.user < prev.user
        || curr.nice < prev.nice
        || curr.system < prev.system
        || curr.idle < prev.idle
        || curr.iowait < prev.iowait
        || curr.irq < prev.irq
        || curr.softirq < prev.softirq
        || curr.steal < prev.steal
}

/// Compute CPU usage percent between two ProcStat readings.
/// Returns None if baseline is invalid (any field decreased, zero delta,
/// or idle_delta > total_delta).
pub fn cpu_percent(prev: &ProcStat, curr: &ProcStat) -> Option<f32> {
    if fields_decreased(&prev.total, &curr.total) {
        return None;
    }

    let prev_total = prev.total.total();
    let curr_total = curr.total.total();
    let prev_idle = prev.total.idle_total();
    let curr_idle = curr.total.idle_total();

    if curr_total <= prev_total {
        return None;
    }
    let total_delta = curr_total - prev_total;
    if total_delta == 0 {
        return None;
    }
    let idle_delta = curr_idle.saturating_sub(prev_idle);
    if idle_delta > total_delta {
        return None; // idle grew more than total — impossible under normal ops
    }
    let used_delta = total_delta - idle_delta;
    Some((used_delta as f32 / total_delta as f32) * 100.0)
}

/// Per-core CPU percent, matched by label. Cores present in only one set
/// get None (new core = no baseline; removed core = dropped).
/// Any individual field decrease → None for that core.
pub fn per_core_percent(prev: &ProcStat, curr: &ProcStat) -> Vec<(String, Option<f32>)> {
    let prev_map: std::collections::HashMap<&str, &CpuJiffies> =
        prev.cores.iter().map(|(n, j)| (n.as_str(), j)).collect();

    let mut results = Vec::new();
    for (name, curr_j) in &curr.cores {
        let pct = if let Some(prev_j) = prev_map.get(name.as_str()) {
            if fields_decreased(prev_j, curr_j) {
                None
            } else {
                let p_total = prev_j.total();
                let c_total = curr_j.total();
                let p_idle = prev_j.idle_total();
                let c_idle = curr_j.idle_total();
                if c_total <= p_total {
                    None
                } else {
                    let t_delta = c_total - p_total;
                    let i_delta = c_idle.saturating_sub(p_idle);
                    if i_delta > t_delta {
                        None
                    } else {
                        let u_delta = t_delta - i_delta;
                        Some((u_delta as f32 / t_delta as f32) * 100.0)
                    }
                }
            }
        } else {
            None
        };
        results.push((name.clone(), pct));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stat() -> &'static str {
        "cpu  100 20 50 800 10 5 5 0 0 0
cpu0 50 10 25 400 5 2 2 0 0 0
cpu1 50 10 25 400 5 3 3 0 0 0
intr 12345 0 0
ctxt 67890
btime 1234567890
processes 1234
"
    }

    #[test]
    fn parse_normal() {
        let p = parse_proc_stat(sample_stat()).unwrap();
        assert_eq!(p.total.user, 100);
        assert_eq!(p.total.idle, 800);
        assert_eq!(p.total.total(), 990); // 100+20+50+800+10+5+5+0
        assert_eq!(p.cores.len(), 2);
        assert_eq!(p.cores[0].0, "cpu0");
        assert_eq!(p.cores[1].0, "cpu1");
    }

    #[test]
    fn cpu_percent_normal() {
        let prev = parse_proc_stat(sample_stat()).unwrap();
        // 10% increase in used, 5% increase in idle on aggregate
        let next = "cpu  110 20 55 850 10 5 5 0 0 0
cpu0 55 10 27 420 5 2 2 0 0 0
cpu1 55 10 28 430 5 3 3 0 0 0
";
        let curr = parse_proc_stat(next).unwrap();
        let pct = cpu_percent(&prev, &curr).unwrap();
        // prev total = 990, curr total = 1055, total_delta = 65
        // prev idle = 810, curr idle = 860, idle_delta = 50
        // used_delta = 15, pct = 15/65 * 100 = 23.08%
        assert!((pct - 23.08).abs() < 0.1);
    }

    #[test]
    fn counter_decrease() {
        let prev = parse_proc_stat(sample_stat()).unwrap();
        let next = "cpu  90 20 50 800 10 5 5 0 0 0
cpu0 45 10 25 400 5 2 2 0 0 0
";
        let curr = parse_proc_stat(next).unwrap();
        assert!(cpu_percent(&prev, &curr).is_none());
    }

    #[test]
    fn no_guest_double_count() {
        // guest should NOT be added separately; it's already in user/nice
        let content = "cpu  100 20 50 800 10 5 5 0 10 20
cpu0 50 10 25 400 5 2 2 0 5 10
";
        let p = parse_proc_stat(content).unwrap();
        // Only first 8 fields: user, nice, system, idle, iowait, irq, softirq, steal
        // guest (9th) and guest_nice (10th) are ignored
        assert_eq!(p.total.user, 100);
        assert_eq!(p.total.steal, 0);
    }

    #[test]
    fn per_core_percent_matching() {
        let prev = parse_proc_stat(sample_stat()).unwrap();
        let next = "cpu  200 40 100 1600 20 10 10 0 0 0
cpu0 100 20 50 800 10 4 4 0 0 0
cpu1 100 20 50 800 10 6 6 0 0 0
";
        let curr = parse_proc_stat(next).unwrap();
        let results = per_core_percent(&prev, &curr);
        assert_eq!(results.len(), 2);
        // both cores doubled exactly, so percent should be same
        for (_, pct) in results {
            assert!(pct.is_some());
        }
    }

    #[test]
    fn cpu_percent_field_decrease_is_none() {
        let prev = parse_proc_stat(sample_stat()).unwrap();
        // user decreased from 100 to 90 (all other fields same/increased)
        let next = "cpu  90 20 50 850 10 5 5 0 0 0
cpu0 50 10 25 425 5 2 2 0 0 0
cpu1 50 10 25 425 5 3 3 0 0 0
";
        let curr = parse_proc_stat(next).unwrap();
        assert!(cpu_percent(&prev, &curr).is_none());
    }

    #[test]
    fn cpu_percent_idle_delta_exceeds_total_delta() {
        // The `idle_delta > total_delta` guard in cpu_percent is a
        // belt-and-suspenders check — after `fields_decreased` above it
        // should never fire with valid Linux counters.  We verify that
        // a normal field increase does *not* trip the guard.
        let prev = parse_proc_stat(sample_stat()).unwrap();
        // All fields increase so neither guard triggers.
        let content = "cpu  110 20 55 850 10 5 5 0 0 0
cpu0 50 10 25 425 5 2 2 0 0 0
cpu1 50 10 25 425 5 3 3 0 0 0
";
        let curr = parse_proc_stat(content).unwrap();
        assert!(cpu_percent(&prev, &curr).is_some());
    }

    #[test]
    fn per_core_field_decrease_is_none() {
        let prev = parse_proc_stat(sample_stat()).unwrap();
        // cpu0 user decreased; cpu1 is fine
        let next = "cpu  110 20 55 850 10 5 5 0 0 0
cpu0 40 10 25 425 5 2 2 0 0 0
cpu1 60 10 30 425 5 3 3 0 0 0
";
        let curr = parse_proc_stat(next).unwrap();
        let results = per_core_percent(&prev, &curr);
        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_none()); // cpu0: user decreased
        assert!(results[1].1.is_some()); // cpu1: all fields ok
    }
}
