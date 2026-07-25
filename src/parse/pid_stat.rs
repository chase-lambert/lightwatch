//! Parse `/proc/[pid]/stat` for process table rows.
//!
//! Format: `pid (comm) state ppid ... utime stime ... starttime ...`
//! Comm may contain spaces and parentheses; locate the closing `)` of comm,
//! then whitespace-split the remainder.

/// Fields needed for process CPU%, identity, and parent walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PidStat {
    pub pid: u32,
    pub comm: String,
    /// Parent pid (`stat` field 4).
    pub ppid: u32,
    pub utime: u64,
    pub stime: u64,
    /// Clock ticks since boot at process start (`stat` field 22). Stable
    /// identity together with `pid` across PID reuse.
    pub starttime: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsePidStatError {
    BadFormat,
    NotANumber,
}

impl std::fmt::Display for ParsePidStatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParsePidStatError::BadFormat => write!(f, "bad /proc/[pid]/stat format"),
            ParsePidStatError::NotANumber => write!(f, "not a number in /proc/[pid]/stat"),
        }
    }
}

impl std::error::Error for ParsePidStatError {}

/// Parse a `/proc/[pid]/stat` body.
pub fn parse_pid_stat(content: &str) -> Result<PidStat, ParsePidStatError> {
    let content = content.trim();
    if content.is_empty() {
        return Err(ParsePidStatError::BadFormat);
    }

    let open = content.find('(').ok_or(ParsePidStatError::BadFormat)?;
    let close = content.rfind(')').ok_or(ParsePidStatError::BadFormat)?;
    if close <= open {
        return Err(ParsePidStatError::BadFormat);
    }

    let pid: u32 = content[..open]
        .trim()
        .parse()
        .map_err(|_| ParsePidStatError::NotANumber)?;
    let comm = content[open + 1..close].to_string();

    // After ") ": state ppid ... (field 3 onward in man page).
    let after = content.get(close + 1..).ok_or(ParsePidStatError::BadFormat)?;
    let after = after.trim_start();
    let fields: Vec<&str> = after.split_whitespace().collect();
    // Need indices 1 (ppid), 11 (utime), 12 (stime), 19 (starttime) after comm.
    if fields.len() < 20 {
        return Err(ParsePidStatError::BadFormat);
    }

    let ppid = fields[1]
        .parse()
        .map_err(|_| ParsePidStatError::NotANumber)?;
    let utime = fields[11]
        .parse()
        .map_err(|_| ParsePidStatError::NotANumber)?;
    let stime = fields[12]
        .parse()
        .map_err(|_| ParsePidStatError::NotANumber)?;
    let starttime = fields[19]
        .parse()
        .map_err(|_| ParsePidStatError::NotANumber)?;

    Ok(PidStat {
        pid,
        comm,
        ppid,
        utime,
        stime,
        starttime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stat() -> &'static str {
        // pid=12345 comm=lightwatch utime=150 stime=25 starttime=999888
        // After ")": state(0) ppid(1) pgrp(2) session(3) tty(4) tpgid(5) flags(6)
        // minflt(7) cminflt(8) majflt(9) cmajflt(10) utime(11) stime(12)
        // cutime(13) cstime(14) priority(15) nice(16) num_threads(17)
        // itrealvalue(18) starttime(19) ...
        "12345 (lightwatch) S 1234 1234 1234 0 -1 4194560 123 0 0 0 150 25 0 0 20 0 8 0 999888 789012 456 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"
    }

    #[test]
    fn parse_normal() {
        let s = parse_pid_stat(sample_stat()).unwrap();
        assert_eq!(s.pid, 12345);
        assert_eq!(s.comm, "lightwatch");
        assert_eq!(s.ppid, 1234);
        assert_eq!(s.utime, 150);
        assert_eq!(s.stime, 25);
        assert_eq!(s.starttime, 999888);
    }

    #[test]
    fn parse_comm_with_spaces() {
        let content = "99 (my process name) S 1 1 1 0 -1 0 0 0 0 0 100 50 0 0 20 0 1 0 1000 2000 300 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let s = parse_pid_stat(content).unwrap();
        assert_eq!(s.pid, 99);
        assert_eq!(s.comm, "my process name");
        assert_eq!(s.ppid, 1);
        assert_eq!(s.utime, 100);
        assert_eq!(s.stime, 50);
        assert_eq!(s.starttime, 1000);
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_pid_stat("").is_err());
    }

    #[test]
    fn rejects_truncated() {
        assert!(parse_pid_stat("1 (x) S 1 1").is_err());
    }
}
