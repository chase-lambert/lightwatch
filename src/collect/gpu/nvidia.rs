use super::GpuDevice;
use crate::model::*;
use std::sync::Mutex;

struct NvmlMetrics {
    util_percent: Reading<f32>,
    vram_total_kb: Reading<u64>,
    vram_used_kb: Reading<u64>,
    temp_celsius: Reading<f32>,
    power_watts: Reading<f32>,
}

fn snapshot_from_metrics(device: &GpuDevice, metrics: NvmlMetrics) -> GpuSnapshot {
    GpuSnapshot {
        pci_id: device.pci_id.clone(),
        vendor_id: device.vendor_id.clone(),
        device_id: device.device_id.clone(),
        driver: device.driver.clone(),
        name: device.name.clone(),
        util_percent: metrics.util_percent,
        vram_total_kb: metrics.vram_total_kb,
        vram_used_kb: metrics.vram_used_kb,
        temp_celsius: metrics.temp_celsius,
        power_watts: metrics.power_watts,
    }
}

/// Power state gate: check if the NVIDIA dGPU is positively active.
/// Returns `true` if NVML operations are allowed.
fn nvidia_power_gate(sys_root: &str, card: &str) -> bool {
    let status_path = format!("{sys_root}/class/drm/{card}/device/power/runtime_status");
    if let Ok(status) = std::fs::read_to_string(&status_path) {
        let status = status.trim();
        status == "active"
    } else {
        false
    }
}

/// Sample NVIDIA GPU metrics via NVML, gated by power state.
/// If the GPU is not active (suspended, unknown, etc.), returns Unavailable
/// without ever touching NVML (this includes NVML init — the gate is
/// fail-closed for *all* NVML entry points).
/// If NVML is not available (no library), returns Unavailable.
pub fn sample_nvidia(device: &GpuDevice) -> GpuSnapshot {
    let sys_root = "/sys";
    let card = &device.drm_card;

    if !nvidia_power_gate(sys_root, card) {
        return GpuSnapshot {
            pci_id: device.pci_id.clone(),
            vendor_id: device.vendor_id.clone(),
            device_id: device.device_id.clone(),
            driver: device.driver.clone(),
            name: device.name.clone(),
            util_percent: Reading::Unavailable {
                reason: "GPU powered down / suspended",
            },
            vram_total_kb: Reading::Unavailable {
                reason: "GPU powered down",
            },
            vram_used_kb: Reading::Unavailable {
                reason: "GPU powered down",
            },
            temp_celsius: Reading::Unavailable {
                reason: "GPU powered down",
            },
            power_watts: Reading::Unavailable {
                reason: "GPU powered down",
            },
        };
    }

    // Attempt NVML (cached where possible)
    match nvml_sample_cached(&device.pci_id) {
        Ok(metrics) => snapshot_from_metrics(device, metrics),
        Err(reason) => GpuSnapshot {
            pci_id: device.pci_id.clone(),
            vendor_id: device.vendor_id.clone(),
            device_id: device.device_id.clone(),
            driver: device.driver.clone(),
            name: device.name.clone(),
            util_percent: Reading::Unavailable { reason },
            vram_total_kb: Reading::Unavailable { reason },
            vram_used_kb: Reading::Unavailable { reason },
            temp_celsius: Reading::Unavailable { reason },
            power_watts: Reading::Unavailable { reason },
        },
    }
}

// ---------------------------------------------------------------------------
// NVML cache — init once, reuse across samples while the power gate is active.
// Cleared on any failure (GpuLost, query error); re-initialised on next active
// sample after clearing.
// ---------------------------------------------------------------------------

/// Cached NVML library handle. The Device handle is derived from this on each
/// call via `device_by_pci_bus_id` (avoids re-init every sample without
/// fighting the Device<'nvml> lifetime).
struct NvmlCache {
    nvml: nvml_wrapper::Nvml,
}

static NVML_CACHE: Mutex<Option<NvmlCache>> = Mutex::new(None);

/// Sample via the cached NVML handle. On cache miss or any failure the cache
/// is cleared and we attempt a fresh init + device resolution, still behind
/// the power gate (the caller has already verified the gate).
fn nvml_sample_cached(pci_id: &str) -> Result<NvmlMetrics, &'static str> {
    let mut guard = NVML_CACHE.lock().unwrap();

    // Try the cached Nvml first.
    if let Some(ref cache) = *guard {
        match cache.nvml.device_by_pci_bus_id(pci_id) {
            Ok(device) => {
                // query_metrics now returns Result; on GpuLost, clear cache and fall through
                match query_metrics(&device) {
                    Ok(snap) => return Ok(snap),
                    Err(_) => {
                        // Hard failure (GpuLost) — clear cache, re-init next time
                        *guard = None;
                    }
                }
            }
            Err(_) => {
                // Device resolution failed (maybe device disappeared) —
                // clear cache and fall through to re-init.
                *guard = None;
            }
        }
    }

    // No cache or stale — init fresh.
    let init_result = nvml_wrapper::Nvml::init();
    match init_result {
        Ok(nvml) => {
            // Resolve device inside a sub-scope so `device` borrow is
            // released before we move `nvml` into the cache.
            let device = nvml
                .device_by_pci_bus_id(pci_id)
                .map_err(|_| "NVML device_by_pci_bus_id failed")?;
            // On GpuLost here, Err propagates and nvml is dropped (not cached).
            match query_metrics(&device) {
                Ok(snap) => {
                    *guard = Some(NvmlCache { nvml });
                    Ok(snap)
                }
                Err(e) => {
                    *guard = None;
                    Err(e)
                }
            }
        }
        Err(_) => {
            *guard = None;
            Err("NVML init failed")
        }
    }
}

/// Query all metrics from a single device in one shot.
/// Returns `Err` on hard failures (GpuLost) — caller should clear any cache.
/// Field-level soft failures become `Unavailable` without clearing the cache.
fn query_metrics(device: &nvml_wrapper::Device) -> Result<NvmlMetrics, &'static str> {
    // Helper: on GpuLost -> return Err; on other error -> Unavailable.
    fn is_lost(e: &nvml_wrapper::error::NvmlError) -> bool {
        matches!(e, nvml_wrapper::error::NvmlError::GpuLost)
    }

    let util = match device.utilization_rates() {
        Ok(u) => Reading::Value(u.gpu as f32),
        Err(e) if is_lost(&e) => return Err("NVML GPU lost"),
        Err(_) => Reading::Unavailable {
            reason: "NVML util query failed",
        },
    };

    // Call memory_info once; extract both total and used.
    let (vram_total, vram_used) = match device.memory_info() {
        Ok(m) => (
            Reading::Value(m.total / 1024),
            Reading::Value(m.used / 1024),
        ),
        Err(e) if is_lost(&e) => return Err("NVML GPU lost"),
        Err(_) => (
            Reading::Unavailable {
                reason: "NVML memory query failed",
            },
            Reading::Unavailable {
                reason: "NVML memory query failed",
            },
        ),
    };

    let temp = match device.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
    {
        Ok(t) => Reading::Value(t as f32),
        Err(e) if is_lost(&e) => return Err("NVML GPU lost"),
        Err(_) => Reading::Unavailable {
            reason: "NVML temp query failed",
        },
    };

    let power = match device.power_usage() {
        Ok(p) => Reading::Value((p as f32) / 1000.0), // mW -> W
        Err(e) if is_lost(&e) => return Err("NVML GPU lost"),
        Err(_) => Reading::Unavailable {
            reason: "NVML power query failed",
        },
    };

    Ok(NvmlMetrics {
        util_percent: util,
        vram_total_kb: vram_total,
        vram_used_kb: vram_used,
        temp_celsius: temp,
        power_watts: power,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_metrics_keep_discovered_identity() {
        let device = GpuDevice {
            pci_id: "0000:01:00.0".into(),
            vendor_id: "10de".into(),
            device_id: "25a2".into(),
            driver: "nvidia".into(),
            name: "NVIDIA GPU (10de:25a2)".into(),
            drm_card: "card1".into(),
            hwmon_path: None,
        };
        let snapshot = snapshot_from_metrics(
            &device,
            NvmlMetrics {
                util_percent: Reading::Value(25.0),
                vram_total_kb: Reading::Value(4_000),
                vram_used_kb: Reading::Value(1_000),
                temp_celsius: Reading::Value(42.0),
                power_watts: Reading::Value(10.0),
            },
        );

        assert_eq!(snapshot.pci_id, device.pci_id);
        assert_eq!(snapshot.vendor_id, device.vendor_id);
        assert_eq!(snapshot.device_id, device.device_id);
        assert_eq!(snapshot.driver, device.driver);
        assert_eq!(snapshot.name, device.name);
    }
}
