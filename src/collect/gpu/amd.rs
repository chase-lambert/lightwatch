use super::GpuDevice;
use crate::model::*;

/// Sample AMD GPU metrics from sysfs.
pub fn sample_amd(device: &GpuDevice) -> GpuSnapshot {
    let card = &device.drm_card;
    let sys_root = "/sys"; // could be parameterized

    let util = read_amd_util(sys_root, card);
    let vram_total = read_amd_vram_total(sys_root, card);
    let vram_used = read_amd_vram_used(sys_root, card);
    let temp = read_amd_temp(&device.hwmon_path);
    let power = read_amd_power(&device.hwmon_path);

    GpuSnapshot {
        pci_id: device.pci_id.clone(),
        vendor_id: device.vendor_id.clone(),
        device_id: device.device_id.clone(),
        driver: device.driver.clone(),
        name: device.name.clone(),
        util_percent: util,
        vram_total_kb: vram_total,
        vram_used_kb: vram_used,
        temp_celsius: temp,
        power_watts: power,
    }
}

fn read_amd_util(sys_root: &str, card: &str) -> Reading<f32> {
    let path = format!("{sys_root}/class/drm/{card}/device/gpu_busy_percent");
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let val = s.trim().parse::<f32>().unwrap_or(-1.0);
            if val >= 0.0 {
                Reading::Value(val)
            } else {
                Reading::Unavailable {
                    reason: "gpu_busy_percent unreadable",
                }
            }
        }
        Err(_) => Reading::Unavailable {
            reason: "gpu_busy_percent missing",
        },
    }
}

fn read_amd_vram_total(sys_root: &str, card: &str) -> Reading<u64> {
    let path = format!("{sys_root}/class/drm/{card}/device/mem_info_vram_total");
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let val = s.trim().parse::<u64>().unwrap_or(0);
            if val > 0 {
                // Value is in bytes, convert to KiB
                Reading::Value(val / 1024)
            } else {
                Reading::Unavailable {
                    reason: "vram_total zero",
                }
            }
        }
        Err(_) => Reading::Unavailable {
            reason: "vram_total missing",
        },
    }
}

fn read_amd_vram_used(sys_root: &str, card: &str) -> Reading<u64> {
    let path = format!("{sys_root}/class/drm/{card}/device/mem_info_vram_used");
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let val = s.trim().parse::<u64>().unwrap_or(0);
            // Value is in bytes, convert to KiB
            Reading::Value(val / 1024)
        }
        Err(_) => Reading::Unavailable {
            reason: "vram_used missing",
        },
    }
}

fn read_amd_temp(hwmon_path: &Option<String>) -> Reading<f32> {
    let dir = match hwmon_path {
        Some(d) => d,
        None => {
            return Reading::Unavailable {
                reason: "no hwmon for GPU",
            };
        }
    };
    let temp_path = format!("{dir}/temp1_input");
    match std::fs::read_to_string(&temp_path) {
        Ok(s) => {
            let val = s.trim().parse::<f32>().unwrap_or(-1.0);
            if val >= 0.0 {
                // Value is in millidegrees Celsius
                Reading::Value(val / 1000.0)
            } else {
                Reading::Unavailable {
                    reason: "temp unreadable",
                }
            }
        }
        Err(_) => Reading::Unavailable {
            reason: "temp missing",
        },
    }
}

fn read_amd_power(hwmon_path: &Option<String>) -> Reading<f32> {
    let dir = match hwmon_path {
        Some(d) => d,
        None => {
            return Reading::Unavailable {
                reason: "no hwmon for GPU",
            };
        }
    };
    let power_path = format!("{dir}/power1_input");
    match std::fs::read_to_string(&power_path) {
        Ok(s) => {
            let val = s.trim().parse::<f32>().unwrap_or(-1.0);
            if val >= 0.0 {
                // Value is in microwatts, convert to watts
                Reading::Value(val / 1_000_000.0)
            } else {
                Reading::Unavailable {
                    reason: "power unreadable",
                }
            }
        }
        Err(_) => Reading::Unavailable {
            reason: "power missing",
        },
    }
}
