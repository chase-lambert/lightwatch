//! Health tab rows — latest-only condition metrics (no history rings).

use super::Reading;

/// One block-backed filesystem mount with capacity from `statvfs`.
#[derive(Clone, Debug, PartialEq)]
pub struct MountRow {
    pub mountpoint: String,
    pub source: String,
    pub fstype: String,
    pub total_bytes: u64,
    /// Used including reserved blocks: `total − f_bfree·frsize`.
    pub used_bytes: u64,
    /// Unprivileged available: `f_bavail·frsize`.
    pub available_bytes: u64,
    /// `used / total * 100` when total > 0.
    pub use_percent: f32,
}

/// Physical media classification for drive cards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Nvme,
    Ssd,
    Hdd,
    Unknown,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Nvme => "NVMe",
            MediaKind::Ssd => "SSD",
            MediaKind::Hdd => "HDD",
            MediaKind::Unknown => "Disk",
        }
    }
}

/// One whole-disk drive card.
#[derive(Clone, Debug, PartialEq)]
pub struct DriveRow {
    /// Kernel block name, e.g. `nvme0n1`.
    pub name: String,
    pub model: String,
    pub size_bytes: Reading<u64>,
    pub kind: MediaKind,
    pub temp_celsius: Reading<f32>,
    /// NVMe SMART percentage used (wear); omit in UI when Unavailable.
    pub wear_percent_used: Reading<u8>,
    /// Full 128-bit SMART media/data integrity error counter.
    pub media_errors: Reading<u128>,
    pub critical_warning: Reading<u8>,
}

/// System pack vs Device-scope peripheral.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryKind {
    System,
    Device,
}

/// One battery row (system or peripheral).
#[derive(Clone, Debug, PartialEq)]
pub struct BatteryRow {
    /// Sysfs power_supply name (`BAT0`, `hidpp_battery_11`).
    pub id: String,
    pub kind: BatteryKind,
    /// Short display label (model preferred).
    pub label: String,
    pub charge_percent: Reading<f32>,
    /// Fallback when percentage is missing (e.g. `Full`, `Normal`).
    pub capacity_level: Reading<String>,
    pub health_percent: Reading<f32>,
    pub cycle_count: Reading<u32>,
}

/// Condition snapshot for the Health tab.
///
/// Each list is a `Reading`: `Unavailable` means the source could not be
/// enumerated; `Value(vec![])` means none present.
#[derive(Clone, Debug, PartialEq)]
pub struct HealthSnapshot {
    pub mounts: Reading<Vec<MountRow>>,
    pub drives: Reading<Vec<DriveRow>>,
    pub batteries: Reading<Vec<BatteryRow>>,
}

impl HealthSnapshot {
    pub fn unavailable(reason: &'static str) -> Self {
        Self {
            mounts: Reading::Unavailable { reason },
            drives: Reading::Unavailable { reason },
            batteries: Reading::Unavailable { reason },
        }
    }
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        Self::unavailable("not sampled")
    }
}
