//! Pure helpers for sysfs power_supply attributes.

use crate::model::{BatteryKind, BatteryRow, Reading};
use std::collections::BTreeMap;

/// Classify a power_supply directory from its `type` and optional `scope`.
///
/// Returns `None` for non-batteries (Mains, USB, …).
pub fn classify_battery(type_str: &str, scope: Option<&str>) -> Option<BatteryKind> {
    if type_str.trim() != "Battery" {
        return None;
    }
    match scope.map(str::trim) {
        Some("Device") => Some(BatteryKind::Device),
        _ => Some(BatteryKind::System), // missing scope → system pack
    }
}

/// Presence of a numeric capacity attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapAttr {
    Absent,
    Malformed,
    Value(u64),
}

impl CapAttr {
    pub fn from_map(attrs: &BTreeMap<String, String>, key: &str) -> Self {
        match attrs.get(key) {
            None => CapAttr::Absent,
            Some(raw) => match parse_u64_attr(raw) {
                Some(v) => CapAttr::Value(v),
                None => CapAttr::Malformed,
            },
        }
    }

    pub fn is_present(self) -> bool {
        !matches!(self, CapAttr::Absent)
    }
}

/// Health percent from energy/charge capacity pairs.
///
/// Rule:
/// 1. If **both** energy values are valid → use energy pair.
/// 2. Else if **either** energy attr is present (malformed or only one side) →
///    incomplete/malformed energy; **do not** fall through to charge.
/// 3. Else if both energy attrs absent and both charge values valid → charge pair.
/// 4. Else incomplete charge / none.
pub fn battery_health_percent(
    energy_full: CapAttr,
    energy_full_design: CapAttr,
    charge_full: CapAttr,
    charge_full_design: CapAttr,
) -> Reading<f32> {
    match (energy_full, energy_full_design) {
        (CapAttr::Value(full), CapAttr::Value(design)) => return ratio_percent(full, design),
        (CapAttr::Absent, CapAttr::Absent) => {
            // Fall through to charge.
        }
        _ => {
            // Any energy presence without a complete valid pair blocks charge.
            return Reading::Unavailable {
                reason: "incomplete energy capacity pair",
            };
        }
    }

    match (charge_full, charge_full_design) {
        (CapAttr::Value(full), CapAttr::Value(design)) => ratio_percent(full, design),
        (CapAttr::Absent, CapAttr::Absent) => Reading::Unavailable {
            reason: "no capacity pair",
        },
        _ => Reading::Unavailable {
            reason: "incomplete charge capacity pair",
        },
    }
}

fn ratio_percent(full: u64, design: u64) -> Reading<f32> {
    if design == 0 {
        return Reading::Unavailable {
            reason: "zero design capacity",
        };
    }
    let pct = (full as f64 / design as f64 * 100.0) as f32;
    // Over-design (>100%) is real on some packs; still report honestly.
    Reading::Value(pct)
}

/// Parse `capacity` percentage; accept 0..=100 only.
pub fn parse_capacity_percent(raw: &str) -> Reading<f32> {
    let s = raw.trim();
    let Ok(v) = s.parse::<f32>() else {
        return Reading::Unavailable {
            reason: "malformed capacity",
        };
    };
    if !(0.0..=100.0).contains(&v) {
        return Reading::Unavailable {
            reason: "capacity out of range",
        };
    }
    Reading::Value(v)
}

pub fn parse_u64_attr(raw: &str) -> Option<u64> {
    raw.trim().parse().ok()
}

pub fn parse_u32_attr(raw: &str) -> Option<u32> {
    raw.trim().parse().ok()
}

/// Build a display label from optional model/manufacturer and fallback id.
pub fn battery_label(model: Option<&str>, manufacturer: Option<&str>, id: &str) -> String {
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    let mfr = manufacturer.map(str::trim).filter(|s| !s.is_empty());
    match (model, mfr) {
        (Some(m), _) => m.to_string(),
        (None, Some(mfr)) => mfr.to_string(),
        (None, None) => id.to_string(),
    }
}

/// Build a [`BatteryRow`] from a flat attribute map (keys = sysfs filenames).
pub fn battery_from_attrs(id: &str, attrs: &BTreeMap<String, String>) -> Option<BatteryRow> {
    let type_str = attrs.get("type").map(|s| s.as_str())?;
    let scope = attrs.get("scope").map(|s| s.as_str());
    let kind = classify_battery(type_str, scope)?;

    let energy_full = CapAttr::from_map(attrs, "energy_full");
    let energy_full_design = CapAttr::from_map(attrs, "energy_full_design");
    let charge_full = CapAttr::from_map(attrs, "charge_full");
    let charge_full_design = CapAttr::from_map(attrs, "charge_full_design");

    let health_percent = battery_health_percent(
        energy_full,
        energy_full_design,
        charge_full,
        charge_full_design,
    );

    let charge_percent = match attrs.get("capacity") {
        Some(c) => parse_capacity_percent(c),
        None => Reading::Unavailable {
            reason: "no capacity",
        },
    };

    let capacity_level = match attrs.get("capacity_level") {
        Some(l) if !l.trim().is_empty() => Reading::Value(l.trim().to_string()),
        _ => Reading::Unavailable {
            reason: "no capacity_level",
        },
    };

    let cycle_count = match attrs.get("cycle_count").and_then(|s| parse_u32_attr(s)) {
        Some(n) => Reading::Value(n),
        None => Reading::Unavailable {
            reason: "no cycle_count",
        },
    };

    let label = battery_label(
        attrs.get("model_name").map(|s| s.as_str()),
        attrs.get("manufacturer").map(|s| s.as_str()),
        id,
    );

    Some(BatteryRow {
        id: id.to_string(),
        kind,
        label,
        charge_percent,
        capacity_level,
        health_percent,
        cycle_count,
    })
}

/// Sort batteries: system first (by id), then device (by id).
pub fn sort_batteries(rows: &mut [BatteryRow]) {
    rows.sort_by(|a, b| {
        let ka = match a.kind {
            BatteryKind::System => 0,
            BatteryKind::Device => 1,
        };
        let kb = match b.kind {
            BatteryKind::System => 0,
            BatteryKind::Device => 1,
        };
        ka.cmp(&kb).then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn mains_skipped() {
        assert!(classify_battery("Mains", None).is_none());
    }

    #[test]
    fn system_and_device() {
        assert_eq!(classify_battery("Battery", None), Some(BatteryKind::System));
        assert_eq!(
            classify_battery("Battery", Some("Device")),
            Some(BatteryKind::Device)
        );
    }

    #[test]
    fn health_from_energy_pair() {
        match battery_health_percent(
            CapAttr::Value(60_920_000),
            CapAttr::Value(70_000_000),
            CapAttr::Absent,
            CapAttr::Absent,
        ) {
            Reading::Value(v) => assert!((v - 87.02857).abs() < 0.01),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn health_rejects_zero_design() {
        assert!(matches!(
            battery_health_percent(
                CapAttr::Value(1),
                CapAttr::Value(0),
                CapAttr::Absent,
                CapAttr::Absent
            ),
            Reading::Unavailable { .. }
        ));
    }

    #[test]
    fn incomplete_energy_does_not_fall_through_to_charge() {
        assert!(matches!(
            battery_health_percent(
                CapAttr::Value(100),
                CapAttr::Absent,
                CapAttr::Value(50),
                CapAttr::Value(100)
            ),
            Reading::Unavailable {
                reason: "incomplete energy capacity pair"
            }
        ));
    }

    #[test]
    fn malformed_energy_blocks_charge_fallback() {
        // Two malformed energy values must not permit charge pair.
        assert!(matches!(
            battery_health_percent(
                CapAttr::Malformed,
                CapAttr::Malformed,
                CapAttr::Value(50),
                CapAttr::Value(100)
            ),
            Reading::Unavailable {
                reason: "incomplete energy capacity pair"
            }
        ));
    }

    #[test]
    fn partial_energy_with_valid_charge_still_blocks() {
        assert!(matches!(
            battery_health_percent(
                CapAttr::Absent,
                CapAttr::Value(100),
                CapAttr::Value(80),
                CapAttr::Value(100)
            ),
            Reading::Unavailable {
                reason: "incomplete energy capacity pair"
            }
        ));
    }

    #[test]
    fn health_from_charge_when_no_energy() {
        match battery_health_percent(
            CapAttr::Absent,
            CapAttr::Absent,
            CapAttr::Value(800),
            CapAttr::Value(1000),
        ) {
            Reading::Value(v) => assert!((v - 80.0).abs() < 0.01),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn over_design_health_reported() {
        match battery_health_percent(
            CapAttr::Value(110),
            CapAttr::Value(100),
            CapAttr::Absent,
            CapAttr::Absent,
        ) {
            Reading::Value(v) => assert!((v - 110.0).abs() < 0.01),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn capacity_range() {
        assert!(matches!(
            parse_capacity_percent("98"),
            Reading::Value(v) if (v - 98.0).abs() < 0.01
        ));
        assert!(matches!(
            parse_capacity_percent("101"),
            Reading::Unavailable { .. }
        ));
        assert!(matches!(
            parse_capacity_percent("nope"),
            Reading::Unavailable { .. }
        ));
    }

    #[test]
    fn system_battery_row() {
        let attrs = map(&[
            ("type", "Battery"),
            ("capacity", "98"),
            ("energy_full", "60920000"),
            ("energy_full_design", "70000000"),
            ("cycle_count", "191"),
            ("model_name", "L21D4PE0"),
            ("manufacturer", "Sunwoda"),
        ]);
        let row = battery_from_attrs("BAT0", &attrs).unwrap();
        assert_eq!(row.kind, BatteryKind::System);
        assert_eq!(row.label, "L21D4PE0");
        assert!(matches!(row.charge_percent, Reading::Value(v) if (v - 98.0).abs() < 0.01));
        assert!(matches!(row.cycle_count, Reading::Value(191)));
        assert!(matches!(row.health_percent, Reading::Value(_)));
    }

    #[test]
    fn sparse_peripheral() {
        let attrs = map(&[
            ("type", "Battery"),
            ("scope", "Device"),
            ("capacity_level", "Full"),
            ("model_name", "MX Anywhere 2S"),
            ("manufacturer", "Logitech"),
        ]);
        let row = battery_from_attrs("hidpp_battery_11", &attrs).unwrap();
        assert_eq!(row.kind, BatteryKind::Device);
        assert!(matches!(row.charge_percent, Reading::Unavailable { .. }));
        assert_eq!(row.capacity_level, Reading::Value("Full".into()));
    }

    #[test]
    fn sort_system_before_device() {
        let mut rows = vec![
            BatteryRow {
                id: "hid".into(),
                kind: BatteryKind::Device,
                label: "m".into(),
                charge_percent: Reading::Unavailable { reason: "x" },
                capacity_level: Reading::Unavailable { reason: "x" },
                health_percent: Reading::Unavailable { reason: "x" },
                cycle_count: Reading::Unavailable { reason: "x" },
            },
            BatteryRow {
                id: "BAT0".into(),
                kind: BatteryKind::System,
                label: "b".into(),
                charge_percent: Reading::Unavailable { reason: "x" },
                capacity_level: Reading::Unavailable { reason: "x" },
                health_percent: Reading::Unavailable { reason: "x" },
                cycle_count: Reading::Unavailable { reason: "x" },
            },
        ];
        sort_batteries(&mut rows);
        assert_eq!(rows[0].id, "BAT0");
        assert_eq!(rows[1].id, "hid");
    }
}
