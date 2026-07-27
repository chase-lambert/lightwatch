//! Selected-process status and aggregate memory parsers.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PidStatus {
    pub uid: Option<u32>,
    pub threads: Option<u32>,
    pub vm_rss_kb: Option<u64>,
    pub vm_hwm_kb: Option<u64>,
    pub rss_anon_kb: Option<u64>,
    pub vm_swap_kb: Option<u64>,
    pub cpu_affinity: Option<String>,
}

pub fn parse_pid_status(content: &str) -> PidStatus {
    let mut out = PidStatus::default();
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "Uid" => out.uid = value.split_whitespace().next().and_then(|v| v.parse().ok()),
            "Threads" => out.threads = value.parse().ok(),
            "VmRSS" => out.vm_rss_kb = parse_kb(value),
            "VmHWM" => out.vm_hwm_kb = parse_kb(value),
            "RssAnon" => out.rss_anon_kb = parse_kb(value),
            "VmSwap" => out.vm_swap_kb = parse_kb(value),
            "Cpus_allowed_list" if !value.is_empty() => out.cpu_affinity = Some(value.to_string()),
            _ => {}
        }
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SmapsRollup {
    pub pss_kb: Option<u64>,
    pub private_clean_kb: Option<u64>,
    pub private_dirty_kb: Option<u64>,
}

impl SmapsRollup {
    pub fn private_kb(&self) -> Option<u64> {
        Some(
            self.private_clean_kb?
                .saturating_add(self.private_dirty_kb?),
        )
    }
}

pub fn parse_smaps_rollup(content: &str) -> SmapsRollup {
    let mut out = SmapsRollup::default();
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key {
            "Pss" => out.pss_kb = parse_kb(value.trim()),
            "Private_Clean" => out.private_clean_kb = parse_kb(value.trim()),
            "Private_Dirty" => out.private_dirty_kb = parse_kb(value.trim()),
            _ => {}
        }
    }
    out
}

fn parse_kb(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let number = parts.next()?.parse().ok()?;
    (parts.next()? == "kB").then_some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_fields_are_independent() {
        let parsed = parse_pid_status(
            "Uid:\t1000 1000 1000 1000\nThreads:\t9\nVmRSS:\t40 kB\n\
             VmHWM:\t80 kB\nRssAnon:\t30 kB\nVmSwap:\t5 kB\n\
             Cpus_allowed_list:\t0-3,8\n",
        );
        assert_eq!(parsed.uid, Some(1000));
        assert_eq!(parsed.threads, Some(9));
        assert_eq!(parsed.vm_rss_kb, Some(40));
        assert_eq!(parsed.vm_hwm_kb, Some(80));
        assert_eq!(parsed.rss_anon_kb, Some(30));
        assert_eq!(parsed.vm_swap_kb, Some(5));
        assert_eq!(parsed.cpu_affinity.as_deref(), Some("0-3,8"));
    }

    #[test]
    fn smaps_private_is_clean_plus_dirty() {
        let parsed = parse_smaps_rollup("Pss: 123 kB\nPrivate_Clean: 4 kB\nPrivate_Dirty: 5 kB\n");
        assert_eq!(parsed.pss_kb, Some(123));
        assert_eq!(parsed.private_kb(), Some(9));
    }
}
