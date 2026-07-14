use super::GpuDevice;
use crate::model::*;
use std::sync::Mutex;

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
        Ok(snap) => snap,
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
fn nvml_sample_cached(pci_id: &str) -> Result<GpuSnapshot, &'static str> {
    let mut guard = NVML_CACHE.lock().unwrap();

    // Try the cached Nvml first.
    if let Some(ref cache) = *guard {
        match cache.nvml.device_by_pci_bus_id(pci_id) {
            Ok(device) => {
                // query_metrics now returns Result; on GpuLost, clear cache and fall through
                match query_metrics(&device, pci_id) {
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
            match query_metrics(&device, pci_id) {
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
fn query_metrics(device: &nvml_wrapper::Device, pci_id: &str) -> Result<GpuSnapshot, &'static str> {
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

    Ok(GpuSnapshot {
        pci_id: pci_id.to_string(),
        vendor_id: String::new(),
        device_id: String::new(),
        driver: "nvidia".to_string(),
        name: "NVIDIA GPU".to_string(),
        util_percent: util,
        vram_total_kb: vram_total,
        vram_used_kb: vram_used,
        temp_celsius: temp,
        power_watts: power,
    })
}
