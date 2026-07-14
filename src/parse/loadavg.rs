/// Parsed /proc/loadavg: three load averages and running/thread info.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadAvg {
    pub load_1min: f32,
    pub load_5min: f32,
    pub load_15min: f32,
    pub running_threads: u32,
    pub total_threads: u32,
}

/// Parse /proc/loadavg content.
/// Format: "0.81 0.79 0.85 2/3933 2567160"
pub fn parse_loadavg(content: &str) -> Result<LoadAvg, ParseLoadError> {
    let parts: Vec<&str> = content.split_whitespace().collect();
    if parts.len() < 4 {
        return Err(ParseLoadError::MissingField);
    }
    let load_1min = parts[0].parse().map_err(|_| ParseLoadError::NotANumber)?;
    let load_5min = parts[1].parse().map_err(|_| ParseLoadError::NotANumber)?;
    let load_15min = parts[2].parse().map_err(|_| ParseLoadError::NotANumber)?;

    // parts[3] is "running/total" e.g., "2/3933"
    let (running_str, total_str) = parts[3].split_once('/').unwrap_or(("0", "0"));
    let running_threads = running_str.parse().unwrap_or(0);
    let total_threads = total_str.parse().unwrap_or(0);

    Ok(LoadAvg {
        load_1min,
        load_5min,
        load_15min,
        running_threads,
        total_threads,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseLoadError {
    MissingField,
    NotANumber,
}

impl std::fmt::Display for ParseLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseLoadError::MissingField => write!(f, "missing field in loadavg"),
            ParseLoadError::NotANumber => write!(f, "not a number in loadavg"),
        }
    }
}

impl std::error::Error for ParseLoadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normal() {
        let l = parse_loadavg("0.81 0.79 0.85 2/3933 2567160\n").unwrap();
        assert!((l.load_1min - 0.81).abs() < 0.001);
        assert!((l.load_5min - 0.79).abs() < 0.001);
        assert!((l.load_15min - 0.85).abs() < 0.001);
        assert_eq!(l.running_threads, 2);
        assert_eq!(l.total_threads, 3933);
    }

    #[test]
    fn parse_missing_field() {
        assert!(parse_loadavg("0.81 0.79").is_err());
    }
}
