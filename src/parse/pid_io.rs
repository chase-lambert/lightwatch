//! Parse `/proc/[pid]/io` for cumulative disk bytes.
//!
//! Uses `read_bytes` / `write_bytes` (bytes actually fetched from/to storage),
//! not `rchar` / `wchar` (which include page-cache traffic).

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PidIo {
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsePidIoError {
    EmptyInput,
}

impl std::fmt::Display for ParsePidIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParsePidIoError::EmptyInput => write!(f, "empty /proc/[pid]/io input"),
        }
    }
}

impl std::error::Error for ParsePidIoError {}

/// Parse a `/proc/[pid]/io` body. Empty input is an error; missing fields
/// yield `None` independently.
pub fn parse_pid_io(content: &str) -> Result<PidIo, ParsePidIoError> {
    if content.is_empty() {
        return Err(ParsePidIoError::EmptyInput);
    }
    let mut read_bytes = None;
    let mut write_bytes = None;
    for line in content.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let val = rest.trim();
        match key.trim() {
            "read_bytes" => {
                read_bytes = val.parse().ok();
            }
            "write_bytes" => {
                write_bytes = val.parse().ok();
            }
            _ => {}
        }
    }
    Ok(PidIo {
        read_bytes,
        write_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_both() {
        let content = "rchar: 100\nwchar: 200\nsyscr: 1\nsyscw: 2\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        let io = parse_pid_io(content).unwrap();
        assert_eq!(io.read_bytes, Some(4096));
        assert_eq!(io.write_bytes, Some(8192));
    }

    #[test]
    fn missing_fields_are_none() {
        let io = parse_pid_io("rchar: 1\n").unwrap();
        assert_eq!(io.read_bytes, None);
        assert_eq!(io.write_bytes, None);
    }

    #[test]
    fn empty_errors() {
        assert!(parse_pid_io("").is_err());
    }
}
