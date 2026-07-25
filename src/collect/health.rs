//! Health condition collector: mounts, physical drives, batteries.

use crate::model::{BatteryRow, DriveRow, HealthSnapshot, MediaKind, MountRow, Reading};
use crate::parse::mounts::{capacity_from_statvfs, parse_health_mounts};
use crate::parse::nvme_smart::{
    NVME_IOCTL_ADMIN_CMD, NVME_SMART_LOG_LEN, NvmeAdminCmd, build_smart_log_cmd,
    nvme_controller_dev, nvme_controller_name, parse_nvme_smart_log,
};
use crate::parse::power_supply::{battery_from_attrs, sort_batteries};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// How often the sampler should refresh health (boottime nanoseconds).
pub const HEALTH_REFRESH_NS: u64 = 5_000_000_000; // 5 s

pub struct HealthCollector {
    proc_root: PathBuf,
    sys_root: PathBuf,
    dev_root: PathBuf,
}

impl HealthCollector {
    pub fn new(
        proc_root: impl Into<PathBuf>,
        sys_root: impl Into<PathBuf>,
        dev_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            proc_root: proc_root.into(),
            sys_root: sys_root.into(),
            dev_root: dev_root.into(),
        }
    }

    pub fn sample(&self) -> HealthSnapshot {
        HealthSnapshot {
            mounts: self.sample_mounts(),
            drives: self.sample_drives(),
            batteries: self.sample_batteries(),
        }
    }

    fn sample_mounts(&self) -> Reading<Vec<MountRow>> {
        let path = self.proc_root.join("mounts");
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                return Reading::Unavailable {
                    reason: "cannot read mounts",
                };
            }
        };
        let mut entries = parse_health_mounts(&content);
        entries.sort_by(|a, b| a.target.cmp(&b.target));
        // Dedup mountpoint: first after sort.
        entries.dedup_by(|a, b| a.target == b.target);

        let mut rows = Vec::new();
        for e in entries {
            let Some((total, used, available, use_percent)) =
                statvfs_capacity(Path::new(&e.target))
            else {
                continue;
            };
            rows.push(MountRow {
                mountpoint: e.target,
                source: e.source,
                fstype: e.fstype,
                total_bytes: total,
                used_bytes: used,
                available_bytes: available,
                use_percent,
            });
        }
        Reading::Value(rows)
    }

    fn sample_drives(&self) -> Reading<Vec<DriveRow>> {
        let block_root = self.sys_root.join("block");
        let entries = match fs::read_dir(&block_root) {
            Ok(e) => e,
            Err(_) => {
                return Reading::Unavailable {
                    reason: "cannot read sysfs block",
                };
            }
        };

        let mut names = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    return Reading::Unavailable {
                        reason: "block directory entry error",
                    };
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_whole_disk(&name) {
                names.push(name);
            }
        }
        names.sort();

        // SMART once per NVMe controller.
        let mut smart_cache: HashMap<String, Option<crate::parse::nvme_smart::NvmeSmartLog>> =
            HashMap::new();

        let mut rows = Vec::new();
        for name in names {
            let block_path = block_root.join(&name);
            let model = read_trimmed(block_path.join("device/model"))
                .or_else(|| {
                    nvme_controller_name(&name).and_then(|c| {
                        read_trimmed(self.sys_root.join("class/nvme").join(c).join("model"))
                    })
                })
                .unwrap_or_else(|| name.clone());

            let size_bytes = read_trimmed(block_path.join("size"))
                .and_then(|s| s.parse::<u64>().ok())
                .and_then(|sectors| sectors.checked_mul(512))
                .map(Reading::Value)
                .unwrap_or(Reading::Unavailable { reason: "no size" });

            let rotational = read_trimmed(block_path.join("queue/rotational"))
                .and_then(|s| s.parse::<u8>().ok());
            let kind = classify_media(&name, rotational);

            let mut temp_celsius = temp_from_block_ancestry(&block_path);
            let mut wear = Reading::Unavailable { reason: "no SMART" };
            let mut media_errors = Reading::Unavailable { reason: "no SMART" };
            let mut critical_warning = Reading::Unavailable { reason: "no SMART" };

            if let Some(ctrl_dev) = nvme_controller_dev(&name) {
                // Use host dev root so tests can inject paths.
                let ctrl_path = if ctrl_dev.starts_with("/dev/") {
                    self.dev_root.join(ctrl_dev.trim_start_matches("/dev/"))
                } else {
                    PathBuf::from(&ctrl_dev)
                };
                let cache_key = ctrl_path.to_string_lossy().into_owned();
                let log = smart_cache
                    .entry(cache_key)
                    .or_insert_with(|| read_nvme_smart(&ctrl_path));
                if let Some(log) = log {
                    wear = Reading::Value(log.percentage_used);
                    media_errors = Reading::Value(log.media_errors);
                    critical_warning = Reading::Value(log.critical_warning);
                    if matches!(temp_celsius, Reading::Unavailable { .. })
                        && let Some(t) = log.temp_celsius
                    {
                        temp_celsius = Reading::Value(t as f32);
                    }
                }
            }

            rows.push(DriveRow {
                name,
                model: model.trim().to_string(),
                size_bytes,
                kind,
                temp_celsius,
                wear_percent_used: wear,
                media_errors,
                critical_warning,
            });
        }
        Reading::Value(rows)
    }

    fn sample_batteries(&self) -> Reading<Vec<BatteryRow>> {
        let psy_root = self.sys_root.join("class/power_supply");
        let entries = match fs::read_dir(&psy_root) {
            Ok(e) => e,
            Err(_) => {
                return Reading::Unavailable {
                    reason: "cannot read power_supply",
                };
            }
        };

        let mut rows = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    return Reading::Unavailable {
                        reason: "power_supply directory entry error",
                    };
                }
            };
            let id = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            // Skip non-directories (and dangling).
            if !dir.is_dir() {
                continue;
            }
            let attrs = match read_attr_map(&dir) {
                Ok(m) => m,
                Err(_) => {
                    return Reading::Unavailable {
                        reason: "power_supply attribute read error",
                    };
                }
            };
            if let Some(row) = battery_from_attrs(&id, &attrs) {
                rows.push(row);
            }
        }
        sort_batteries(&mut rows);
        Reading::Value(rows)
    }
}

fn read_attr_map(dir: &Path) -> Result<BTreeMap<String, String>, std::io::Error> {
    let mut map = BTreeMap::new();
    let entries = fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip write-only or binary-ish names; only small text attrs.
        if name.starts_with('.') {
            continue;
        }
        // Individual attribute read failures are local (permission on one file);
        // only directory traversal errors fail the whole map.
        if let Ok(val) = fs::read_to_string(&path) {
            map.insert(name, val);
        }
    }
    Ok(map)
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whole-disk kernel names only (no partitions).
pub fn is_whole_disk(name: &str) -> bool {
    if name.starts_with("nvme") {
        // nvme0n1 yes; nvme0n1p1 no; nvme0c0n1 (controller) rarely in /sys/block
        return nvme_controller_dev(name).is_some();
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        // mmcblk0 yes; mmcblk0p1 no; mmcblk0boot0 no
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    // sda, sdb, vda — letters only after prefix; sda1 has trailing digits
    for prefix in ["sd", "vd", "hd"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_lowercase())
        {
            return true;
        }
    }
    false
}

fn classify_media(name: &str, rotational: Option<u8>) -> MediaKind {
    if name.starts_with("nvme") {
        return MediaKind::Nvme;
    }
    match rotational {
        Some(1) => MediaKind::Hdd,
        Some(0) => MediaKind::Ssd,
        _ => MediaKind::Unknown,
    }
}

fn statvfs_capacity(path: &Path) -> Option<(u64, u64, u64, f32)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    capacity_from_statvfs(
        st.f_blocks as u64,
        st.f_bfree as u64,
        st.f_bavail as u64,
        st.f_frsize as u64,
    )
}

/// Temperature only via this block device's sysfs ancestry (no global hwmon scan).
fn temp_from_block_ancestry(block_path: &Path) -> Reading<f32> {
    // Walk device symlink targets and look for hwmon*/temp*_input nearby.
    let device = block_path.join("device");
    let mut candidates = Vec::new();
    if let Ok(canon) = fs::canonicalize(&device) {
        candidates.push(canon.clone());
        if let Some(parent) = canon.parent() {
            candidates.push(parent.to_path_buf());
        }
    }
    candidates.push(device);

    for base in candidates {
        if let Some(t) = find_temp_in_hwmon(&base) {
            return Reading::Value(t);
        }
        // Also check base/hwmon and base/../hwmon*
        if let Some(t) = find_temp_in_hwmon(&base.join("hwmon")) {
            return Reading::Value(t);
        }
        if let Ok(rd) = fs::read_dir(&base) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("hwmon")
                    && let Some(t) = find_temp_in_hwmon(&e.path())
                {
                    return Reading::Value(t);
                }
            }
        }
    }
    Reading::Unavailable { reason: "no temp" }
}

fn find_temp_in_hwmon(dir: &Path) -> Option<f32> {
    // Prefer temp1_input; else any temp*_input.
    let preferred = dir.join("temp1_input");
    if let Some(t) = read_temp_millic(&preferred) {
        return Some(t);
    }
    let rd = fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("temp")
            && name.ends_with("_input")
            && let Some(t) = read_temp_millic(&e.path())
        {
            return Some(t);
        }
    }
    // Nested single hwmon dir
    let nested = dir.join("hwmon");
    if nested.is_dir() {
        return find_temp_in_hwmon(&nested);
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("hwmon")
                && let Some(t) = find_temp_in_hwmon(&e.path())
            {
                return Some(t);
            }
        }
    }
    None
}

fn read_temp_millic(path: &Path) -> Option<f32> {
    let s = fs::read_to_string(path).ok()?;
    let millic: f32 = s.trim().parse().ok()?;
    Some(millic / 1000.0)
}

/// Open NVMe controller char device and fetch SMART log. Fail-closed.
fn read_nvme_smart(ctrl_path: &Path) -> Option<crate::parse::nvme_smart::NvmeSmartLog> {
    // Open first, then verify the open descriptor is a char device (closes
    // a path-replacement window between stat and open).
    let file = fs::OpenOptions::new().read(true).open(ctrl_path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.file_type().is_char_device() {
        return None;
    }
    let fd = file.as_raw_fd();

    let mut buf = vec![0u8; NVME_SMART_LOG_LEN as usize];
    let ptr = buf.as_mut_ptr() as u64;
    let mut cmd: NvmeAdminCmd = build_smart_log_cmd(ptr);

    let rc = unsafe { nvme_admin_ioctl(fd, &mut cmd) };
    if rc != 0 {
        return None;
    }
    parse_nvme_smart_log(&buf)
}

/// Single centralized unsafe ioctl boundary for NVMe admin commands.
unsafe fn nvme_admin_ioctl(fd: libc::c_int, cmd: *mut NvmeAdminCmd) -> libc::c_int {
    unsafe { libc::ioctl(fd, NVME_IOCTL_ADMIN_CMD, cmd) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_disk_predicates() {
        assert!(is_whole_disk("nvme0n1"));
        assert!(is_whole_disk("nvme10n2"));
        assert!(!is_whole_disk("nvme0n1p1"));
        assert!(is_whole_disk("sda"));
        assert!(!is_whole_disk("sda1"));
        assert!(is_whole_disk("vda"));
        assert!(!is_whole_disk("vda1"));
        assert!(is_whole_disk("mmcblk0"));
        assert!(!is_whole_disk("mmcblk0p1"));
        assert!(!is_whole_disk("loop0"));
        assert!(!is_whole_disk("zram0"));
        assert!(!is_whole_disk("dm-0"));
    }

    #[test]
    fn media_kind() {
        assert_eq!(classify_media("nvme0n1", Some(0)), MediaKind::Nvme);
        assert_eq!(classify_media("sda", Some(1)), MediaKind::Hdd);
        assert_eq!(classify_media("sda", Some(0)), MediaKind::Ssd);
    }
}
