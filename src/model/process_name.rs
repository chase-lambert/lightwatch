//! Pure helpers for process display names and helper detection.
//!
//! Linux `comm` is truncated to 15 characters (`gnome-system-mo`). Prefer the
//! executable basename (or argv0 basename) for the full short name, without
//! pulling in the rest of the cmdline (Chrome-style arg noise).
//!
//! Electron/Chromium helpers still get a quiet `(renderer)`-style suffix from
//! `--type=` so End Process targeting stays clear.

/// Extract Chromium/Electron `--type=` value from a null-separated cmdline.
pub fn electron_type(cmdline: &[u8]) -> Option<&str> {
    for arg in cmdline.split(|&b| b == 0) {
        if arg.is_empty() {
            continue;
        }
        if let Some(rest) = arg.strip_prefix(b"--type=") {
            return std::str::from_utf8(rest).ok().filter(|s| !s.is_empty());
        }
    }
    None
}

/// True when this looks like a Chromium/Electron helper (not the app root).
pub fn is_helper_cmdline(cmdline: &[u8]) -> bool {
    electron_type(cmdline).is_some()
}

/// Basename of the first cmdline argument (argv0), if present.
pub fn cmdline_argv0_base(cmdline: &[u8]) -> Option<&str> {
    let first = cmdline.split(|&b| b == 0).find(|a| !a.is_empty())?;
    let s = std::str::from_utf8(first).ok()?;
    let base = s.rsplit('/').next().unwrap_or(s);
    if base.is_empty() { None } else { Some(base) }
}

/// Pick a full short process name: exe basename → argv0 basename → comm → `[pid]`.
///
/// Does **not** include remaining cmdline arguments.
pub fn base_name<'a>(
    comm: &'a str,
    cmdline: &'a [u8],
    exe_base: Option<&'a str>,
    pid: u32,
) -> String {
    if let Some(e) = exe_base.map(str::trim).filter(|s| !s.is_empty()) {
        return e.to_string();
    }
    if let Some(a0) = cmdline_argv0_base(cmdline) {
        return a0.to_string();
    }
    if !comm.is_empty() {
        return comm.to_string();
    }
    format!("[{pid}]")
}

/// Build the table name: full short base + optional Electron role suffix.
pub fn display_name(comm: &str, cmdline: &[u8], exe_base: Option<&str>, pid: u32) -> String {
    let base = base_name(comm, cmdline, exe_base, pid);
    if let Some(t) = electron_type(cmdline) {
        let short = t.split('/').next_back().unwrap_or(t);
        format!("{base} ({short})")
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                v.push(0);
            }
            v.extend_from_slice(p.as_bytes());
        }
        v.push(0);
        v
    }

    #[test]
    fn type_from_cmdline() {
        let c = cmd(&["/usr/lib/slack/slack", "--type=renderer", "--no-sandbox"]);
        assert_eq!(electron_type(&c), Some("renderer"));
        assert!(is_helper_cmdline(&c));
    }

    #[test]
    fn main_has_no_type() {
        let c = cmd(&["/usr/lib/slack/slack"]);
        assert_eq!(electron_type(&c), None);
        assert!(!is_helper_cmdline(&c));
    }

    #[test]
    fn exe_beats_truncated_comm() {
        let c = cmd(&["gnome-system-monitor"]);
        // /proc/comm is 15 chars: "gnome-system-mo"
        assert_eq!(
            display_name("gnome-system-mo", &c, Some("gnome-system-monitor"), 1),
            "gnome-system-monitor"
        );
    }

    #[test]
    fn argv0_when_exe_missing() {
        let c = cmd(&["/usr/bin/gnome-system-monitor"]);
        assert_eq!(
            display_name("gnome-system-mo", &c, None, 1),
            "gnome-system-monitor"
        );
    }

    #[test]
    fn ignores_extra_argv() {
        let c = cmd(&[
            "/opt/google/chrome/chrome",
            "--type=renderer",
            "--enable-crashpad",
            "https://example.com",
        ]);
        assert_eq!(
            display_name("chrome", &c, Some("chrome"), 9),
            "chrome (renderer)"
        );
    }

    #[test]
    fn display_without_type() {
        let c = cmd(&["lightwatch"]);
        assert_eq!(
            display_name("lightwatch", &c, Some("lightwatch"), 1),
            "lightwatch"
        );
    }

    #[test]
    fn falls_back_to_comm() {
        assert_eq!(display_name("bash", &[], None, 1), "bash");
    }
}
