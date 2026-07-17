//! UI section visibility prefs — simple XDG line file, no extra deps.
//!
//! Path: `$XDG_CONFIG_HOME/lightwatch/ui.conf` or `~/.config/lightwatch/ui.conf`.
//! When neither `XDG_CONFIG_HOME` nor `HOME` is set, persistence is skipped.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Which dashboard sections are shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionVisibility {
    pub show_cpu: bool,
    pub show_memory: bool,
    /// PCI IDs the user has hidden. Empty → all GPUs visible by default.
    /// Stale IDs (device gone) are kept intentionally — never garbage-collected.
    pub hidden_gpus: BTreeSet<String>,
}

impl Default for SectionVisibility {
    fn default() -> Self {
        Self {
            show_cpu: true,
            show_memory: true,
            hidden_gpus: BTreeSet::new(),
        }
    }
}

/// Stable section identity for toggles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionId {
    Cpu,
    Memory,
    Gpu(String),
}

impl SectionVisibility {
    pub fn is_gpu_visible(&self, pci_id: &str) -> bool {
        !self.hidden_gpus.contains(pci_id)
    }

    /// Toggle a section in place (always mutates for the three section kinds).
    pub fn toggle(&mut self, id: &SectionId) {
        match id {
            SectionId::Cpu => {
                self.show_cpu = !self.show_cpu;
            }
            SectionId::Memory => {
                self.show_memory = !self.show_memory;
            }
            SectionId::Gpu(pci) => {
                if self.hidden_gpus.contains(pci) {
                    self.hidden_gpus.remove(pci);
                } else {
                    self.hidden_gpus.insert(pci.clone());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve the prefs file path from environment.
///
/// Priority: `XDG_CONFIG_HOME` → `$HOME/.config` → `None` (skip persistence).
pub fn resolve_prefs_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        let xdg = xdg.trim();
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("lightwatch").join("ui.conf"));
        }
    }
    if let Some(home) = home {
        let home = home.trim();
        if !home.is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("lightwatch")
                    .join("ui.conf"),
            );
        }
    }
    None
}

fn env_prefs_path() -> Option<PathBuf> {
    resolve_prefs_path(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Parse / format
// ---------------------------------------------------------------------------

/// Parse a `ui.conf` body into visibility prefs.
pub fn parse_ui_conf(text: &str) -> SectionVisibility {
    let mut v = SectionVisibility::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "show_cpu" => {
                if let Some(b) = parse_bool(val) {
                    v.show_cpu = b;
                }
            }
            "show_memory" => {
                if let Some(b) = parse_bool(val) {
                    v.show_memory = b;
                }
            }
            "hide_gpu" => {
                if !val.is_empty() {
                    v.hidden_gpus.insert(val.to_string());
                }
            }
            _ => {} // unknown keys ignored
        }
    }
    v
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Format visibility prefs as a `ui.conf` body.
pub fn format_ui_conf(v: &SectionVisibility) -> String {
    let mut out = String::new();
    out.push_str("# lightwatch UI section visibility\n");
    out.push_str(&format!(
        "show_cpu={}\n",
        if v.show_cpu { "1" } else { "0" }
    ));
    out.push_str(&format!(
        "show_memory={}\n",
        if v.show_memory { "1" } else { "0" }
    ));
    for pci in &v.hidden_gpus {
        out.push_str("hide_gpu=");
        out.push_str(pci);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

/// Load prefs from the env-resolved path, or defaults if missing/unreadable.
pub fn load_ui_prefs() -> SectionVisibility {
    let Some(path) = env_prefs_path() else {
        return SectionVisibility::default();
    };
    load_ui_prefs_from(&path)
}

fn load_ui_prefs_from(path: &Path) -> SectionVisibility {
    match fs::read_to_string(path) {
        Ok(text) => parse_ui_conf(&text),
        Err(_) => SectionVisibility::default(),
    }
}

/// Best-effort save. Failures are silent (never break the GUI).
pub fn save_ui_prefs(v: &SectionVisibility) {
    let Some(path) = env_prefs_path() else {
        return;
    };
    let _ = save_ui_prefs_to(&path, v);
}

/// Atomic write: temp file in same dir, then rename.
pub fn save_ui_prefs_to(path: &Path, v: &SectionVisibility) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = format_ui_conf(v);
    let tmp = path.with_extension(format!("conf.{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn parse_defaults_on_empty() {
        let v = parse_ui_conf("");
        assert!(v.show_cpu);
        assert!(v.show_memory);
        assert!(v.hidden_gpus.is_empty());
    }

    #[test]
    fn parse_flags_and_hide_list() {
        let text = "\
# comment
show_cpu=0
show_memory=1
hide_gpu=0000:01:00.0
hide_gpu=0000:02:00.0
junk line
unknown=1
show_cpu=maybe
";
        let v = parse_ui_conf(text);
        assert!(!v.show_cpu);
        assert!(v.show_memory);
        assert_eq!(
            v.hidden_gpus,
            BTreeSet::from(["0000:01:00.0".to_string(), "0000:02:00.0".to_string()])
        );
    }

    #[test]
    fn format_round_trip() {
        let mut v = SectionVisibility::default();
        v.show_cpu = false;
        v.hidden_gpus.insert("0000:01:00.0".to_string());
        let text = format_ui_conf(&v);
        let back = parse_ui_conf(&text);
        assert_eq!(v, back);
    }

    #[test]
    fn path_xdg_wins() {
        let p = resolve_prefs_path(Some("/xdg/config"), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/config/lightwatch/ui.conf"));
    }

    #[test]
    fn path_home_fallback() {
        let p = resolve_prefs_path(None, Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/lightwatch/ui.conf"));
    }

    #[test]
    fn path_empty_xdg_falls_to_home() {
        let p = resolve_prefs_path(Some("  "), Some("/home/u")).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/lightwatch/ui.conf"));
    }

    #[test]
    fn path_neither_is_none() {
        assert!(resolve_prefs_path(None, None).is_none());
        assert!(resolve_prefs_path(Some(""), Some("")).is_none());
    }

    #[test]
    fn toggle_cpu_memory() {
        let mut v = SectionVisibility::default();
        v.toggle(&SectionId::Cpu);
        assert!(!v.show_cpu);
        v.toggle(&SectionId::Cpu);
        assert!(v.show_cpu);
        v.toggle(&SectionId::Memory);
        assert!(!v.show_memory);
    }

    #[test]
    fn toggle_gpu_round_trip_and_idempotent_pair() {
        let mut v = SectionVisibility::default();
        let id = SectionId::Gpu("0000:01:00.0".to_string());
        assert!(v.is_gpu_visible("0000:01:00.0"));
        v.toggle(&id);
        assert!(!v.is_gpu_visible("0000:01:00.0"));
        assert!(v.hidden_gpus.contains("0000:01:00.0"));
        v.toggle(&id);
        assert!(v.is_gpu_visible("0000:01:00.0"));
        // double hide then double show
        v.toggle(&id);
        v.toggle(&id);
        assert!(v.is_gpu_visible("0000:01:00.0"));
    }

    #[test]
    fn unknown_pci_default_visible() {
        let v = SectionVisibility::default();
        assert!(v.is_gpu_visible("0000:99:00.0"));
    }

    #[test]
    fn atomic_save_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("lightwatch-prefs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.conf");
        let mut v = SectionVisibility::default();
        v.show_memory = false;
        v.hidden_gpus.insert("0000:0a:00.0".to_string());
        save_ui_prefs_to(&path, &v).unwrap();
        let loaded = load_ui_prefs_from(&path);
        assert_eq!(v, loaded);
        let _ = fs::remove_dir_all(&dir);
    }
}
