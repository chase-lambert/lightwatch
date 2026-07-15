pub mod graph;
pub mod graph_geom;
pub mod theme;
pub mod view;

use crate::model::HistoryConfig;
use std::ffi::OsStr;
use std::path::Path;

// ---------------------------------------------------------------------------
// GPU pin — prefer integrated (AMD) GPU via env vars at UI startup
// ---------------------------------------------------------------------------

/// Path to the Radeon Vulkan ICD manifest. Existence is checked before
/// injecting `VK_ICD_FILENAMES` into the process environment.
const RADEON_ICD_PATH: &str = "/usr/share/vulkan/icd.d/radeon_icd.json";

/// Result of the pure decision: which env vars to set (if any).
#[derive(Debug, PartialEq, Eq)]
struct GpuEnvBundle {
    pub(super) power_pref: Option<&'static str>,
    pub(super) vk_icd_filenames: Option<&'static str>,
}

/// Pure decision: propose the atomic default bundle only when the user
/// has not set either `WGPU_POWER_PREF` or `VK_ICD_FILENAMES`.
///
/// - Both vars set to `None` (truly absent) AND radeon ICD file exists →
///   propose `WGPU_POWER_PREF=low` + `VK_ICD_FILENAMES=<radeon path>`.
/// - Either var is `Some(_)` (including non-Unicode) → propose nothing;
///   the user is managing GPU selection.
/// - Both absent but radeon ICD missing → propose only `WGPU_POWER_PREF=low`.
///
/// This function is pure (no environment mutation) and testable.
fn propose_gpu_env(
    power_pref: Option<&OsStr>,
    vk_icd: Option<&OsStr>,
    radeon_icd_exists: bool,
) -> GpuEnvBundle {
    // If the user has set either var at all, the default bundle is disabled
    // entirely — we never override or mix with user config.
    if power_pref.is_some() || vk_icd.is_some() {
        return GpuEnvBundle {
            power_pref: None,
            vk_icd_filenames: None,
        };
    }
    GpuEnvBundle {
        power_pref: Some("low"),
        vk_icd_filenames: if radeon_icd_exists {
            Some(RADEON_ICD_PATH)
        } else {
            None
        },
    }
}

/// Apply the GPU pin decision at startup (before any threads or wgpu init).
///
/// # Safety
///
/// `set_var` is called only during single-threaded startup in `run_gui`,
/// before `iced::application(…).run()` spawns the event loop / sampler /
/// wgpu adapter probe. No other thread exists at this point, so there is
/// no concurrent env access and the unsafety is contained to this boundary.
fn apply_gpu_env() {
    let radeon_exists = Path::new(RADEON_ICD_PATH).exists();
    let bundle = propose_gpu_env(
        std::env::var_os("WGPU_POWER_PREF").as_deref(),
        std::env::var_os("VK_ICD_FILENAMES").as_deref(),
        radeon_exists,
    );
    if let Some(v) = bundle.power_pref {
        // SAFETY: single-threaded startup — see fn doc.
        unsafe { std::env::set_var("WGPU_POWER_PREF", v); }
    }
    if let Some(v) = bundle.vk_icd_filenames {
        // SAFETY: single-threaded startup — see fn doc.
        unsafe { std::env::set_var("VK_ICD_FILENAMES", v); }
    }
}

// ---------------------------------------------------------------------------
// iced entry point
// ---------------------------------------------------------------------------

pub fn run_gui(config: HistoryConfig) -> iced::Result {
    // Pin GPU preference before any threads or wgpu adapter probe.
    // See apply_gpu_env safety doc.
    apply_gpu_env();

    let boot_config = config.clone();
    iced::application(
        move || view::boot(boot_config.clone()),
        view::update,
        view::view,
    )
    .title(view::title)
    .subscription(view::subscription)
    .theme(view::theme)
    .window_size((800.0, 900.0))
    .run()
}

// ---------------------------------------------------------------------------
// Tests — pure decide function only (no env mutation, safe for parallel test
// runners and Rust 2024).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // A small helper to build a fake OsStr from a &str for the propose_gpu_env
    // calls in tests.
    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
    }

    #[test]
    fn both_unset_icd_exists_proposes_both() {
        let b = propose_gpu_env(None, None, true);
        assert_eq!(b.power_pref, Some("low"));
        assert_eq!(b.vk_icd_filenames, Some(RADEON_ICD_PATH));
    }

    #[test]
    fn both_unset_icd_missing_proposes_power_pref_only() {
        let b = propose_gpu_env(None, None, false);
        assert_eq!(b.power_pref, Some("low"));
        assert_eq!(b.vk_icd_filenames, None);
    }

    #[test]
    fn power_pref_set_blocks_bundle() {
        let b = propose_gpu_env(os("high"), None, true);
        assert_eq!(b.power_pref, None);
        assert_eq!(b.vk_icd_filenames, None);
    }

    #[test]
    fn vk_icd_set_blocks_bundle() {
        let b = propose_gpu_env(None, os("/fake/icd.json"), true);
        assert_eq!(b.power_pref, None);
        assert_eq!(b.vk_icd_filenames, None);
    }

    #[test]
    fn both_set_blocks_bundle() {
        let b = propose_gpu_env(os("high"), os("/fake/icd.json"), true);
        assert_eq!(b.power_pref, None);
        assert_eq!(b.vk_icd_filenames, None);
    }

    #[test]
    fn non_unicode_counted_as_present() {
        // A non-Unicode OsStr from system env (simulated via raw bytes).
        // var_os returning Some(non-unicode) must count as "set".
        use std::os::unix::ffi::OsStrExt;
        let raw = OsStr::from_bytes(b"\xFF\xFE");
        let b = propose_gpu_env(Some(raw), None, true);
        assert_eq!(b.power_pref, None);
        assert_eq!(b.vk_icd_filenames, None);
    }
}
