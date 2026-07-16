/// Parse /proc/self/status to extract VmRSS (total resident) and
/// RssAnon (anonymous resident memory, private footprint).
///
/// Format is a series of `FieldName:\tvalue kB` lines.  Each field is
/// parsed independently — a malformed or missing VmRSS does not prevent
/// RssAnon from being returned (and vice versa).  Only truly empty input
/// produces an `Err`; all other content returns `Ok` with each field as
/// `Some(val)` or `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct SelfStatus {
    pub vm_rss_kb: Option<u64>,
    pub rss_anon_kb: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseSelfStatusError {
    NotANumber,
    WrongUnit,
    EmptyInput,
}

impl std::fmt::Display for ParseSelfStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseSelfStatusError::NotANumber => {
                write!(f, "not a number in /proc/self/status")
            }
            ParseSelfStatusError::WrongUnit => {
                write!(f, "wrong/missing unit in /proc/self/status (expected kB)")
            }
            ParseSelfStatusError::EmptyInput => {
                write!(f, "empty /proc/self/status input")
            }
        }
    }
}

impl std::error::Error for ParseSelfStatusError {}

pub fn parse_self_status(content: &str) -> Result<SelfStatus, ParseSelfStatusError> {
    if content.is_empty() {
        return Err(ParseSelfStatusError::EmptyInput);
    }

    let mut vm_rss_kb: Option<u64> = None;
    let mut rss_anon_kb: Option<u64> = None;

    for line in content.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        let mut parts = rest.split_whitespace();
        let val_str = parts.next();
        let unit_str = parts.next();

        match key {
            "VmRSS" => {
                vm_rss_kb = parse_kb(val_str, unit_str).ok();
            }
            "RssAnon" => {
                rss_anon_kb = parse_kb(val_str, unit_str).ok();
            }
            _ => {}
        }
    }

    Ok(SelfStatus {
        vm_rss_kb,
        rss_anon_kb,
    })
}

fn parse_kb(val_str: Option<&str>, unit_str: Option<&str>) -> Result<u64, ParseSelfStatusError> {
    let val_str = val_str.ok_or(ParseSelfStatusError::NotANumber)?;
    let unit_str = unit_str.ok_or(ParseSelfStatusError::WrongUnit)?;
    if unit_str != "kB" {
        return Err(ParseSelfStatusError::WrongUnit);
    }
    val_str
        .parse::<u64>()
        .map_err(|_| ParseSelfStatusError::NotANumber)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_both_fields() {
        let content = "\
Name:   lightwatch
VmRSS:    123456 kB
RssAnon:   45678 kB
VmSize:   999999 kB
";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, Some(123456));
        assert_eq!(s.rss_anon_kb, Some(45678));
    }

    #[test]
    fn parse_reordered() {
        let content = "\
RssAnon:   100 kB
VmRSS:     200 kB
";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.rss_anon_kb, Some(100));
        assert_eq!(s.vm_rss_kb, Some(200));
    }

    #[test]
    fn parse_ignores_unrelated_fields() {
        let content = "\
VmPeak:   500000 kB
VmRSS:    300000 kB
VmHWM:    400000 kB
RssAnon:  100000 kB
RssFile:   50000 kB
RssShmem:      0 kB
";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, Some(300000));
        assert_eq!(s.rss_anon_kb, Some(100000));
    }

    #[test]
    fn missing_vmrss_rssanon_still_ok() {
        let content = "RssAnon:   100 kB\n";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, None);
        assert_eq!(s.rss_anon_kb, Some(100));
    }

    #[test]
    fn missing_rssanon_vmrss_still_ok() {
        let content = "VmRSS:   200 kB\n";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, Some(200));
        assert_eq!(s.rss_anon_kb, None);
    }

    #[test]
    fn malformed_vmrss_rssanon_ok() {
        let content = "VmRSS:   abc kB\nRssAnon:   100 kB\n";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, None);
        assert_eq!(s.rss_anon_kb, Some(100));
    }

    #[test]
    fn malformed_rssanon_vmrss_ok() {
        let content = "VmRSS:   200 kB\nRssAnon:   abc kB\n";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, Some(200));
        assert_eq!(s.rss_anon_kb, None);
    }

    #[test]
    fn wrong_unit_vmrss_rssanon_ok() {
        let content = "VmRSS:   100 MB\nRssAnon:   100 kB\n";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, None);
        assert_eq!(s.rss_anon_kb, Some(100));
    }

    #[test]
    fn missing_unit_vmrss_rssanon_ok() {
        let content = "VmRSS:   100\nRssAnon:   100 kB\n";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, None);
        assert_eq!(s.rss_anon_kb, Some(100));
    }

    #[test]
    fn empty_input_is_error() {
        let err = parse_self_status("").unwrap_err();
        assert!(matches!(err, ParseSelfStatusError::EmptyInput));
    }

    #[test]
    fn line_without_colon_is_skipped() {
        let content = "not a key value line\nVmRSS:   123 kB\nRssAnon:   456 kB\n";
        let s = parse_self_status(content).unwrap();
        assert_eq!(s.vm_rss_kb, Some(123));
        assert_eq!(s.rss_anon_kb, Some(456));
    }
}
