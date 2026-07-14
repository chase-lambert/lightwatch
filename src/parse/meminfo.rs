use std::collections::HashMap;

/// Parsed /proc/meminfo.
#[derive(Clone, Debug, Default)]
pub struct MemInfo {
    fields: HashMap<String, u64>,
}

impl MemInfo {
    pub fn get(&self, key: &str) -> Option<u64> {
        self.fields.get(key).copied()
    }

    pub fn has_required(&self) -> bool {
        self.fields.contains_key("MemTotal") && self.fields.contains_key("MemAvailable")
    }
}

/// Parse /proc/meminfo content. All values are in kB.
/// Missing lines are silently omitted; required fields checked separately.
pub fn parse_meminfo(content: &str) -> MemInfo {
    let mut fields = HashMap::new();
    for line in content.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            let value_str = rest.trim();
            // value may have " kB" suffix
            let value_part = value_str.split_whitespace().next().unwrap_or("0");
            if let Ok(v) = value_part.parse::<u64>() {
                fields.insert(key.to_string(), v);
            }
        }
    }
    MemInfo { fields }
}

/// Compute used memory: MemTotal - MemAvailable (saturating).
/// Returns None if MemTotal or MemAvailable is missing.
pub fn mem_used(meminfo: &MemInfo) -> Option<u64> {
    let total = meminfo.get("MemTotal")?;
    let available = meminfo.get("MemAvailable")?;
    Some(total.saturating_sub(available))
}

/// Compute swap used: SwapTotal - SwapFree (saturating).
/// Returns None if either is missing.
pub fn swap_used(meminfo: &MemInfo) -> Option<u64> {
    let total = meminfo.get("SwapTotal")?;
    let free = meminfo.get("SwapFree")?;
    Some(total.saturating_sub(free))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "MemTotal:       16000000 kB
MemFree:          8000000 kB
MemAvailable:    12000000 kB
Buffers:           500000 kB
Cached:           3500000 kB
SwapCached:             0 kB
SwapTotal:        2000000 kB
SwapFree:         1800000 kB
";

    #[test]
    fn parse_normal() {
        let m = parse_meminfo(SAMPLE);
        assert_eq!(m.get("MemTotal"), Some(16000000));
        assert_eq!(m.get("MemAvailable"), Some(12000000));
        assert!(m.has_required());
    }

    #[test]
    fn mem_used_saturating() {
        let m = parse_meminfo(SAMPLE);
        let used = mem_used(&m).unwrap();
        // 16_000_000 - 12_000_000 = 4_000_000
        assert_eq!(used, 4000000);
    }

    #[test]
    fn mem_used_when_available_gt_total() {
        let content = "MemTotal: 100 kB\nMemAvailable: 200 kB\n";
        let m = parse_meminfo(content);
        let used = mem_used(&m).unwrap();
        assert_eq!(used, 0); // saturating
    }

    #[test]
    fn missing_total() {
        let content = "MemAvailable: 100 kB\n";
        let m = parse_meminfo(content);
        assert!(!m.has_required());
        assert!(mem_used(&m).is_none());
    }

    #[test]
    fn missing_available() {
        let content = "MemTotal: 100 kB\n";
        let m = parse_meminfo(content);
        assert!(!m.has_required());
        assert!(mem_used(&m).is_none());
    }

    #[test]
    fn both_required_fields_present() {
        let content = "MemTotal: 100 kB\nMemAvailable: 80 kB\n";
        let m = parse_meminfo(content);
        assert!(m.has_required());
        assert_eq!(mem_used(&m), Some(20));
    }

    #[test]
    fn test_swap_used() {
        let m = parse_meminfo(SAMPLE);
        let sw = super::swap_used(&m).unwrap();
        assert_eq!(sw, 200000); // 2000000 - 1800000
    }

    #[test]
    fn optional_fields_independent() {
        let content = "MemTotal: 100 kB\nMemAvailable: 80 kB\nSomeRandom: 50 kB\n";
        let m = parse_meminfo(content);
        assert!(m.has_required());
        assert_eq!(m.get("SomeRandom"), Some(50));
    }
}
