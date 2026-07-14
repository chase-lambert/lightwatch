/// Parse /proc/self/stat to extract utime, stime (for CPU%) and other self metrics.
/// Format: pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt
/// cmajflt utime stime cutime cstime priority nice num_threads ...
///
/// We need fields (1-indexed): 14=utime, 15=stime, 24=rss
/// But the comm field may contain spaces and parens, making simple splitting tricky.
/// Strategy: find the closing ')' of comm, then split the rest on whitespace.

#[derive(Clone, Debug, PartialEq)]
pub struct SelfStat {
    pub utime: u64,
    pub stime: u64,
    pub rss_pages: u64, // field 24: resident set size in pages
}

/// Parse /proc/self/stat.
pub fn parse_self_stat(content: &str) -> Result<SelfStat, ParseSelfError> {
    // Find the closing paren of the comm field
    let close_paren = content.rfind(')').ok_or(ParseSelfError::BadFormat)?;
    let after_comm = &content[close_paren + 2..]; // skip ") "
    let fields: Vec<&str> = after_comm.split_whitespace().collect();

    // After removing "(comm) " we have: state (field 3 in full, but field 1 after comm)
    // Full index: 1=pid, 2=comm, 3=state, 4=ppid, ..., 14=utime, 15=stime, 24=rss
    // After removing pid and comm: field 0=state, 1=ppid, ..., 11=utime, 12=stime, 21=rss
    if fields.len() < 22 {
        return Err(ParseSelfError::BadFormat);
    }

    let utime = fields[11].parse().map_err(|_| ParseSelfError::NotANumber)?;
    let stime = fields[12].parse().map_err(|_| ParseSelfError::NotANumber)?;
    let rss_pages = fields[21].parse().map_err(|_| ParseSelfError::NotANumber)?;

    Ok(SelfStat {
        utime,
        stime,
        rss_pages,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseSelfError {
    BadFormat,
    NotANumber,
}

impl std::fmt::Display for ParseSelfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseSelfError::BadFormat => write!(f, "bad /proc/self/stat format"),
            ParseSelfError::NotANumber => write!(f, "not a number in /proc/self/stat"),
        }
    }
}

impl std::error::Error for ParseSelfError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normal() {
        // Real-ish /proc/self/stat content
        let content = "12345 (lightwatch) S 1234 1234 1234 0 -1 4194560 123 0 0 0 150 25 0 0 20 0 8 0 123456 789012 456 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let s = parse_self_stat(content).unwrap();
        assert_eq!(s.utime, 150);
        assert_eq!(s.stime, 25);
        assert_eq!(s.rss_pages, 456);
    }

    #[test]
    fn parse_comm_with_spaces() {
        let content = "99 (my process name) S 1 1 1 0 -1 0 0 0 0 0 100 50 0 0 20 0 1 0 1000 2000 300 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let s = parse_self_stat(content).unwrap();
        assert_eq!(s.utime, 100);
        assert_eq!(s.stime, 50);
        assert_eq!(s.rss_pages, 300);
    }
}
