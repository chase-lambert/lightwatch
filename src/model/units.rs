/// Small formatting helpers.
pub fn bytes_to_human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{size:.0} {}", UNITS[unit_idx])
    } else {
        format!("{size:.1} {}", UNITS[unit_idx])
    }
}

pub fn temp_celsius(temp_millic: f32) -> String {
    format!("{:.1}°C", temp_millic / 1000.0)
}

pub fn freq_mhz(freq_khz: u64) -> String {
    format!("{:.0} MHz", freq_khz as f64 / 1000.0)
}

pub fn percent(value: f32) -> String {
    format!("{value:.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytes_to_human() {
        assert_eq!(bytes_to_human(0), "0 B");
        assert_eq!(bytes_to_human(1023), "1023 B");
        assert_eq!(bytes_to_human(1024), "1.0 KiB");
        assert_eq!(bytes_to_human(1536), "1.5 KiB");
        assert_eq!(bytes_to_human(1048576), "1.0 MiB");
        assert_eq!(bytes_to_human(1073741824), "1.0 GiB");
    }
}
