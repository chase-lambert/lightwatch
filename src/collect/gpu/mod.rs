pub mod amd;
pub mod nvidia;

/// A discovered GPU device.
#[derive(Clone, Debug)]
pub struct GpuDevice {
    pub pci_id: String,             // e.g. "0000:04:00.0"
    pub vendor_id: String,          // e.g. "1002"
    pub device_id: String,          // e.g. "1681"
    pub driver: String,             // e.g. "amdgpu"
    pub name: String,               // human-readable
    pub drm_card: String,           // e.g. "card0"
    pub hwmon_path: Option<String>, // path to hwmon for temp/power
}

/// Discover GPU devices from /sys/class/drm/cardN.
/// `sysfs_root` defaults to "/sys" but is injectable for tests.
pub fn discover(sys_root: &str) -> Vec<GpuDevice> {
    let drm_dir = format!("{sys_root}/class/drm");
    let mut devices = Vec::new();

    let Ok(entries) = std::fs::read_dir(&drm_dir) else {
        return devices;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only match cardN where N is digits (skip card0-DP-1 etc.)
        if !name.starts_with("card") {
            continue;
        }
        let suffix = &name[4..]; // after "card"
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        // Resolve device symlink
        let device_link = format!("{drm_dir}/{name}/device");
        let Ok(device_path) = std::fs::read_link(&device_link) else {
            continue;
        };
        let device_path = device_path.to_string_lossy().into_owned();

        // Read vendor, device, uevent
        let vendor = read_sysfs_one(&format!("{drm_dir}/{name}/device/vendor"));
        let device = read_sysfs_one(&format!("{drm_dir}/{name}/device/device"));
        let uevent = read_sysfs_uevent(&format!("{drm_dir}/{name}/device/uevent"));

        // Get PCI_SLOT_NAME from uevent
        let pci_id = uevent.get("PCI_SLOT_NAME").cloned().unwrap_or_default();
        if pci_id.is_empty() {
            continue;
        }

        // Heuristic: skip if PCI path doesn't match device link (outside GPU)
        if !device_path.contains(&pci_id) {
            continue;
        }

        let driver = uevent.get("DRIVER").cloned().unwrap_or_default();
        let vendor_id = vendor.trim_start_matches("0x").to_string();
        let device_id = device.trim_start_matches("0x").to_string();

        // Find hwmon for this device
        let hwmon_path = find_device_hwmon(&format!("{drm_dir}/{name}/device"));

        // Human-readable name
        let name_human = match driver.as_str() {
            "amdgpu" => format!("AMD GPU ({vendor_id}:{device_id})"),
            "nvidia" => format!("NVIDIA GPU ({vendor_id}:{device_id})"),
            _ => format!("GPU {driver} ({vendor_id}:{device_id})"),
        };

        // Deduplicate by PCI id
        if devices.iter().any(|d: &GpuDevice| d.pci_id == pci_id) {
            continue;
        }

        devices.push(GpuDevice {
            pci_id,
            vendor_id,
            device_id,
            driver,
            name: name_human,
            drm_card: name,
            hwmon_path,
        });
    }

    devices
}

fn read_sysfs_one(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_sysfs_uevent(path: &str) -> std::collections::HashMap<String, String> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut map = std::collections::HashMap::new();
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

fn find_device_hwmon(device_path: &str) -> Option<String> {
    let hwmon_dir = format!("{device_path}/hwmon");
    let Ok(entries) = std::fs::read_dir(&hwmon_dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("hwmon") {
            return Some(format!("{hwmon_dir}/{name}"));
        }
    }
    None
}
