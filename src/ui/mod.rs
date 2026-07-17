pub mod graph;
pub mod graph_geom;
pub mod prefs;
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
    pub(super) backend: Option<&'static str>,
}

/// Pure decision: propose the atomic default bundle only when the user
/// has not set any of `WGPU_POWER_PREF`, `VK_ICD_FILENAMES`, or
/// `WGPU_BACKEND`.
///
/// - All three vars set to `None` (truly absent) AND radeon ICD file exists →
///   propose `WGPU_POWER_PREF=low` + `VK_ICD_FILENAMES=<radeon path>` +
///   `WGPU_BACKEND=vulkan`.
/// - Any managed var is `Some(_)` (including non-Unicode) → propose nothing;
///   the user is managing GPU selection.
/// - All absent but radeon ICD missing → propose only `WGPU_POWER_PREF=low`.
///
/// This function is pure (no environment mutation) and testable.
fn propose_gpu_env(
    power_pref: Option<&OsStr>,
    vk_icd: Option<&OsStr>,
    backend: Option<&OsStr>,
    radeon_icd_exists: bool,
) -> GpuEnvBundle {
    // If the user has set any managed var, the default bundle is disabled
    // entirely — we never override or mix with user config.
    if power_pref.is_some() || vk_icd.is_some() || backend.is_some() {
        return GpuEnvBundle {
            power_pref: None,
            vk_icd_filenames: None,
            backend: None,
        };
    }
    GpuEnvBundle {
        power_pref: Some("low"),
        vk_icd_filenames: if radeon_icd_exists {
            Some(RADEON_ICD_PATH)
        } else {
            None
        },
        backend: radeon_icd_exists.then_some("vulkan"),
    }
}

/// Use one Tokio worker unless the user has chosen a runtime size explicitly.
/// This policy is independent of GPU selection.
fn propose_tokio_worker_threads(current: Option<&OsStr>) -> Option<&'static str> {
    current.is_none().then_some("1")
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
        std::env::var_os("WGPU_BACKEND").as_deref(),
        radeon_exists,
    );
    if let Some(v) = bundle.power_pref {
        // SAFETY: single-threaded startup — see fn doc.
        unsafe {
            std::env::set_var("WGPU_POWER_PREF", v);
        }
    }
    if let Some(v) = bundle.vk_icd_filenames {
        // SAFETY: single-threaded startup — see fn doc.
        unsafe {
            std::env::set_var("VK_ICD_FILENAMES", v);
        }
    }
    if let Some(v) = bundle.backend {
        // SAFETY: single-threaded startup — see fn doc.
        unsafe {
            std::env::set_var("WGPU_BACKEND", v);
        }
    }
}

/// Apply the bounded executor default during single-threaded startup.
fn apply_tokio_worker_env() {
    let value = propose_tokio_worker_threads(std::env::var_os("TOKIO_WORKER_THREADS").as_deref());
    if let Some(v) = value {
        // SAFETY: called before iced creates its Tokio runtime or any thread.
        unsafe {
            std::env::set_var("TOKIO_WORKER_THREADS", v);
        }
    }
}

// ---------------------------------------------------------------------------
// iced entry point
// ---------------------------------------------------------------------------

pub fn run_gui(config: HistoryConfig) -> iced::Result {
    // Bound the executor and pin GPU preference before iced creates either.
    apply_tokio_worker_env();
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

    // A small helper to build a fake OsStr from a &str for pure env decisions.
    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
    }

    #[test]
    fn all_unset_icd_exists_proposes_full_bundle() {
        let b = propose_gpu_env(None, None, None, true);
        assert_eq!(b.power_pref, Some("low"));
        assert_eq!(b.vk_icd_filenames, Some(RADEON_ICD_PATH));
        assert_eq!(b.backend, Some("vulkan"));
    }

    #[test]
    fn all_unset_icd_missing_proposes_power_pref_only() {
        let b = propose_gpu_env(None, None, None, false);
        assert_eq!(b.power_pref, Some("low"));
        assert_eq!(b.vk_icd_filenames, None);
        assert_eq!(b.backend, None);
    }

    fn assert_gpu_bundle_blocked(
        power_pref: Option<&OsStr>,
        vk_icd: Option<&OsStr>,
        backend: Option<&OsStr>,
    ) {
        let b = propose_gpu_env(power_pref, vk_icd, backend, true);
        assert_eq!(
            b,
            GpuEnvBundle {
                power_pref: None,
                vk_icd_filenames: None,
                backend: None,
            }
        );
    }

    #[test]
    fn power_pref_set_blocks_bundle() {
        assert_gpu_bundle_blocked(os("high"), None, None);
    }

    #[test]
    fn vk_icd_set_blocks_bundle() {
        assert_gpu_bundle_blocked(None, os("/fake/icd.json"), None);
    }

    #[test]
    fn backend_set_blocks_bundle() {
        assert_gpu_bundle_blocked(None, None, os("gl"));
    }

    #[test]
    fn non_unicode_counted_as_present() {
        // A non-Unicode OsStr from system env (simulated via raw bytes).
        // var_os returning Some(non-unicode) must count as "set".
        use std::os::unix::ffi::OsStrExt;
        let raw = OsStr::from_bytes(b"\xFF\xFE");
        assert_gpu_bundle_blocked(Some(raw), None, None);
    }

    #[test]
    fn all_gpu_vars_set_with_non_unicode_backend_blocks_bundle() {
        use std::os::unix::ffi::OsStrExt;
        let raw = OsStr::from_bytes(b"\xFF\xFE");
        assert_gpu_bundle_blocked(os("high"), os("/fake/icd.json"), Some(raw));
    }

    #[test]
    fn tokio_workers_unset_proposes_one() {
        assert_eq!(propose_tokio_worker_threads(None), Some("1"));
    }

    #[test]
    fn tokio_workers_set_is_preserved() {
        assert_eq!(propose_tokio_worker_threads(os("4")), None);
    }

    #[test]
    fn non_unicode_tokio_workers_is_preserved() {
        use std::os::unix::ffi::OsStrExt;
        let raw = OsStr::from_bytes(b"\xFF\xFE");
        assert_eq!(propose_tokio_worker_threads(Some(raw)), None);
    }
}
