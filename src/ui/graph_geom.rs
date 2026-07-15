//! Pure graph geometry: age-based X mapping, gap splitting, monotone-cubic
//! smoothing, axis ticks. No iced dependencies.
//!
//! Returns Bézier curve segments and tick positions; the canvas layer in
//! `graph.rs` renders them.

use crate::model::SamplePoint;

/// Parameters for the drawing window (time domain).
#[derive(Clone, Debug)]
pub struct DrawWindow {
    pub window_secs: f64,
    pub window_end_ns: u64,
}

/// Pixel-space bounds of the inner plot area (inside the frame).
#[derive(Clone, Copy, Debug)]
pub struct PlotBounds {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// A cubic Bézier segment from `start` to `end` with control points `c1`, `c2`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BezierSeg {
    pub start: (f32, f32),
    pub c1: (f32, f32),
    pub c2: (f32, f32),
    pub end: (f32, f32),
}

/// Geometry produced for a single series: gap-separated runs of Bézier pieces.
#[derive(Clone, Debug)]
pub struct SeriesGeometry {
    pub bezier_runs: Vec<Vec<BezierSeg>>,
}

// ---------------------------------------------------------------------------
// Coordinate mapping
// ---------------------------------------------------------------------------

/// Map a boottime timestamp to pixel X using age-based position.
/// Returns an X that may fall left of `plot_left` or right of `plot_right` —
/// the canvas clip rect will cut off-screen portions.
fn age_to_x(
    t_boot_ns: u64,
    window_end_ns: u64,
    window_ns: u64,
    plot_left: f32,
    plot_right: f32,
) -> f32 {
    let age_ns = window_end_ns.saturating_sub(t_boot_ns);
    let plot_width = plot_right - plot_left;
    let fraction = age_ns as f64 / window_ns.max(1) as f64;
    let x = plot_right - fraction as f32 * plot_width;
    // No clamping — clip rect handles off-screen x
    x
}

/// Map a value (0..max_value) to pixel Y (0 = bottom, max = top).
fn value_to_y(value: f32, max_value: f32, plot_top: f32, plot_bottom: f32) -> f32 {
    let clamped = value.min(max_value).max(0.0);
    let fraction = clamped / max_value.max(1.0);
    // Y decreases upward: fraction=0 → bottom, fraction=1 → top
    plot_bottom - fraction * (plot_bottom - plot_top)
}

// ---------------------------------------------------------------------------
// Off-screen cull
// ---------------------------------------------------------------------------

/// Cull points wholly outside the visible window, keeping:
/// - The single off-left neighbor adjacent to the first in-window point (for
///   incoming spline slope).
/// - All in-window points.
/// - The newest point (always the last element; may be off-right after a
///   clock jump but normally is in-window).
///
/// This bounds tessellation work regardless of ring age (e.g. after a long
/// suspend — critic finding 2).
fn cull_off_screen(
    points: &[SamplePoint],
    window_start_ns: u64,
    window_end_ns: u64,
) -> Vec<SamplePoint> {
    if points.is_empty() {
        return vec![];
    }

    let first_in = points.iter().position(|p| p.t_boot_ns >= window_start_ns);
    let last_in = points.iter().rposition(|p| p.t_boot_ns <= window_end_ns);

    match (first_in, last_in) {
        (Some(fi), Some(_)) => {
            // Include off-left neighbor for entering slope
            let start = fi.saturating_sub(1);
            points[start..].to_vec()
        }
        (None, Some(_)) => {
            // All points off-left (too old) — just keep the newest
            let last = points.len() - 1;
            vec![points[last]]
        }
        (_, None) => {
            // All points off-right (shouldn't happen) — keep the oldest
            vec![points[0]]
        }
    }
}

/// Coalesce gaps narrower than one pixel of time. A gap whose surrounding data
/// points are less than `min_dt_ns` apart maps to under 1px on screen and cannot
/// be drawn as a gap at all, so rendering it as continuous is visually lossless.
///
/// Doing this *before* decimation bounds the gap-free run count (and thus the
/// Bézier-segment count and tessellation work) by the plot pixel width, even
/// under a pathological gap stream that would otherwise force `O(num_runs)`
/// two-point runs (Codex re-review finding 1).
///
/// Merging is by SPATIAL WIDTH, not gap length: an entire contiguous gap run is
/// dropped iff the data points bracketing the whole run are strictly ordered and
/// closer than one pixel of time. A run whose bracketing data is ≥ 1px apart (a
/// genuinely visible absence), a boundary run (no data on one side), or a run
/// adjacent to a non-monotonic timestamp is preserved — and each preserved run
/// collapses to a single marker.
fn coalesce_subpixel_gaps(points: &[SamplePoint], min_dt_ns: f64) -> Vec<SamplePoint> {
    let mut out: Vec<SamplePoint> = Vec::with_capacity(points.len());
    let mut i = 0;
    while i < points.len() {
        if points[i].value.is_some() {
            out.push(points[i]);
            i += 1;
            continue;
        }
        // Extent of this contiguous gap run [i, j); points[j] (if any) is data.
        let mut j = i;
        while j < points.len() && points[j].value.is_none() {
            j += 1;
        }
        // Data points bracketing the WHOLE run: last kept data before it, and
        // the first data after it. Merge the entire run only if those two are
        // strictly ordered AND closer than one pixel of time — so a genuine
        // (wider) gap, a boundary run, or a run adjacent to a non-monotonic
        // timestamp (nt <= pt) is preserved (findings 1 & 2).
        let prev_t = out.last().filter(|q| q.value.is_some()).map(|q| q.t_boot_ns);
        let next_t = if j < points.len() {
            Some(points[j].t_boot_ns)
        } else {
            None
        };
        let subpixel = matches!(
            (prev_t, next_t),
            (Some(pt), Some(nt)) if nt > pt && ((nt - pt) as f64) < min_dt_ns
        );
        if !subpixel {
            // One marker is enough to split runs; collapsing the whole preserved
            // gap run to a single None keeps total markers bounded by num_runs and
            // avoids a per-marker allocation storm downstream on an all-gap series
            // (Codex final finding 1).
            out.push(points[i]);
        }
        i = j;
    }
    out
}

// ---------------------------------------------------------------------------
// Gap-aware decimation
// ---------------------------------------------------------------------------

/// Decimate sample points preserving gap markers and segment endpoints.
///
/// Splits into gap-free runs and applies ONE global stride (derived from the
/// total decimatable point count) across every run, so a handful of long runs
/// share a single budget rather than each claiming `target_count`.
/// `None` (gap) points are never elided (critic cycle-2 finding).
///
/// Resource policy under heavy fragmentation: each gap-free run keeps both
/// endpoints, so output scales with `num_runs`. That is kept bounded upstream by
/// `coalesce_subpixel_gaps` (called before decimation in `compute_series`), which
/// merges gaps narrower than one pixel — so `num_runs` can never exceed the plot
/// pixel width, and total output stays `O(plot_width)` even under a pathological
/// per-tick gap stream. `worst_case_geometry_within_budget` measures both bursty
/// and adversarial cases and confirms both stay within the frame budget.
pub fn decimate_points(points: &[SamplePoint], target_count: usize) -> Vec<SamplePoint> {
    // Split into gap-free runs interleaved with gap markers (singleton runs).
    let mut runs: Vec<Vec<SamplePoint>> = Vec::new();
    let mut current: Vec<SamplePoint> = Vec::new();
    for pt in points {
        if pt.value.is_some() {
            current.push(*pt);
        } else {
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
            runs.push(vec![*pt]); // gap marker
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }

    // GLOBAL budget: total decimatable points across ALL gap-free runs of
    // length >= 2. A single stride is derived from this total and applied to
    // every run, so a handful of long runs share one budget instead of each
    // claiming `target_count` (critic cycle-2 finding). Gaps and single points
    // are always preserved and never count against the budget.
    let target = target_count.max(2);
    let total_data: usize = runs.iter().filter(|r| r.len() >= 2).map(|r| r.len()).sum();

    // Already within budget → nothing to drop.
    if total_data <= target {
        return runs.into_iter().flatten().collect();
    }
    let stride = total_data.div_ceil(target).max(1);

    let mut result = Vec::new();
    for run in runs {
        if run.len() < 2 {
            // Gap marker or lone point — always kept.
            result.extend(run);
        } else {
            // Decimate with the shared global stride, preserving both endpoints.
            result.push(run[0]);
            let mut i = stride;
            while i < run.len() - 1 {
                result.push(run[i]);
                i += stride;
            }
            result.push(run[run.len() - 1]);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Monotone-cubic (Fritsch–Carlson) smoothing → Bézier segments
// ---------------------------------------------------------------------------

/// Fritsch–Carlson interior tangent at point `i` given left slope `sL`,
/// right slope `sR`, left interval length `hL`, and right interval length `hR`.
fn interior_tangent(sl: f32, sr: f32, hl: f32, hr: f32) -> f32 {
    // If slopes have opposite signs or either is zero, tangent = 0 (local extremum)
    if sl * sr <= 0.0 {
        return 0.0;
    }

    // Weighted harmonic mean (Fritsch–Carlson 1980)
    let wl = 2.0 * hr + hl;
    let wr = hr + 2.0 * hl;
    let d = (wl + wr) / (wl / sl + wr / sr);

    // Monotonicity safeguard: the tangent magnitude must not exceed
    // 3× the smallest adjacent slope magnitude.
    let max_slope = 3.0 * sl.abs().min(sr.abs());
    if d.abs() > max_slope {
        d.signum() * max_slope
    } else {
        d
    }
}

/// Endpoint tangent using the Fritsch–Carlson one-sided quadratic estimate,
/// clamped for monotonicity.
fn endpoint_tangent(s0: f32, s1: f32, h0: f32, h1: f32) -> f32 {
    // Quadratic estimate
    let d = ((2.0 * h0 + h1) * s0 - h0 * s1) / (h0 + h1);

    // If the tangent points opposite to the initial slope, clamp to zero
    if d * s0 <= 0.0 {
        return 0.0;
    }
    // If slopes have opposite signs, the tangent must not exceed 3×|s0|
    if s0 * s1 <= 0.0 && d.abs() > 3.0 * s0.abs() {
        3.0 * s0
    } else {
        d
    }
}

/// Compute tangent-magnitude upper bound for endpoint monotonicity.
/// For endpoint with adjacent slope `s0`, the tangent must not exceed
/// 3×|s0| to prevent overshoot into the first interval.
fn endpoint_clamp(d: f32, s0: f32) -> f32 {
    let limit = 3.0 * s0.abs();
    if d.abs() > limit {
        d.signum() * limit
    } else {
        d
    }
}

/// Compute monotone-cubic Bézier pieces for a gap-free run of (x, y) points.
/// Returns a vec of `BezierSeg` — one per adjacent pair.
///
/// Assumes strictly increasing x (the caller must skip/coalesce `dx ≤ 0`
/// points first — see `coalesce_degenerate_x`).
fn smooth_run(points: &[(f32, f32)]) -> Vec<BezierSeg> {
    let n = points.len();
    if n < 2 {
        return vec![];
    }

    let xs: Vec<f32> = points.iter().map(|p| p.0).collect();
    let ys: Vec<f32> = points.iter().map(|p| p.1).collect();

    // Compute slopes
    let mut dx: Vec<f32> = Vec::with_capacity(n - 1);
    let mut slopes: Vec<f32> = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let dxi = xs[i + 1] - xs[i];
        dx.push(dxi);
        if dxi > 0.0 {
            slopes.push((ys[i + 1] - ys[i]) / dxi);
        } else {
            slopes.push(0.0); // degenerate; shouldn't happen after coalesce
        }
    }

    // Compute tangents
    let mut tangents = vec![0.0f32; n];

    if n == 2 {
        // Single interval: use secant slope as tangent (monotone line)
        let s = slopes[0];
        tangents[0] = endpoint_clamp(s, s);
        tangents[1] = endpoint_clamp(s, s);
    } else {
        // Endpoints
        tangents[0] = endpoint_tangent(slopes[0], slopes[1], dx[0], dx[1]);
        tangents[n - 1] = {
            let i = n - 1;
            let j = i - 1;
            let k = i - 2;
            let dl = slopes[k];
            let dr = slopes[j];
            let hl = dx[k];
            let hr = dx[j];
            endpoint_tangent(dr, dl, hr, hl)
        };

        // Interior points
        for i in 1..n - 1 {
            tangents[i] = interior_tangent(
                slopes[i - 1], // left slope
                slopes[i],     // right slope
                dx[i - 1],     // left interval length
                dx[i],         // right interval length
            );
        }
    }

    // Build Bézier segments
    let mut segs = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let h = dx[i];
        let p0 = (xs[i], ys[i]);
        let p3 = (xs[i + 1], ys[i + 1]);
        let c1 = (xs[i] + h / 3.0, ys[i] + tangents[i] * h / 3.0);
        let c2 = (xs[i + 1] - h / 3.0, ys[i + 1] - tangents[i + 1] * h / 3.0);
        segs.push(BezierSeg {
            start: p0,
            c1,
            c2,
            end: p3,
        });
    }
    segs
}

/// Coalesce points with `dx ≤ 0` (duplicate or non-monotonic timestamps).
/// Keeps the first of any run of non-strictly-increasing x values, dropping
/// subsequent degenerate neighbors. This prevents divide-by-zero in smoothing
/// and invalid tangents (critic finding 5).
fn coalesce_degenerate_x(coords: &[(f32, f32)]) -> Vec<(f32, f32)> {
    if coords.is_empty() {
        return vec![];
    }
    let mut out = vec![coords[0]];
    for &(x, y) in &coords[1..] {
        if x > out.last().unwrap().0 {
            out.push((x, y));
        }
        // else: dx ≤ 0, drop this point
    }
    out
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compute series geometry: off-screen cull, optional gap-aware decimation,
/// coordinate mapping, gap splitting, and monotone-cubic Bézier smoothing.
pub fn compute_series(
    points: &[SamplePoint],
    max_value: f32,
    window: &DrawWindow,
    bounds: &PlotBounds,
    decimate: bool,
) -> SeriesGeometry {
    let window_ns = (window.window_secs * 1e9) as u64;
    let plot_width = bounds.right - bounds.left;
    let plot_height = bounds.bottom - bounds.top;

    if plot_width <= 0.0 || plot_height <= 0.0 {
        return SeriesGeometry {
            bezier_runs: Vec::new(),
        };
    }

    let window_start_ns = window.window_end_ns.saturating_sub(window_ns);

    // 1. Cull off-screen (keep window points + off-left neighbor)
    let culled = cull_off_screen(points, window_start_ns, window.window_end_ns);

    // 2. Coalesce sub-pixel gaps (bounds run count by pixel width; visually
    //    lossless — a gap narrower than 1px can't be drawn). f64 throughout so a
    //    fractional plot width doesn't inflate the one-pixel duration.
    let min_dt_ns = window_ns as f64 / (plot_width as f64).max(1.0);
    let coalesced = coalesce_subpixel_gaps(&culled, min_dt_ns);

    // 3. Optional decimation (gap-aware, one global stride across runs)
    let working = if decimate {
        let in_window = coalesced
            .iter()
            .filter(|p| p.t_boot_ns >= window_start_ns)
            .count();
        let target = (plot_width as usize).max(1);
        if in_window > target * 2 {
            decimate_points(&coalesced, target)
        } else {
            coalesced
        }
    } else {
        coalesced
    };

    // 3. Map to (x, y) coordinates with gap preservation
    let coords: Vec<Option<(f32, f32)>> = working
        .iter()
        .map(|pt| {
            let x = age_to_x(
                pt.t_boot_ns,
                window.window_end_ns,
                window_ns,
                bounds.left,
                bounds.right,
            );
            let y = pt
                .value
                .map(|v| value_to_y(v, max_value, bounds.top, bounds.bottom))?;
            Some((x, y))
        })
        .collect();

    // 4. Split into gap-free runs, smooth each, return Bézier pieces
    let mut bezier_runs: Vec<Vec<BezierSeg>> = Vec::new();
    let mut current_run: Vec<(f32, f32)> = Vec::new();

    for coord in coords {
        match coord {
            Some((x, y)) => current_run.push((x, y)),
            None => {
                // Gap encountered — finalise current run
                if current_run.len() >= 2 {
                    let cleaned = coalesce_degenerate_x(&current_run);
                    let segs = smooth_run(&cleaned);
                    if !segs.is_empty() {
                        bezier_runs.push(segs);
                    }
                }
                current_run.clear();
            }
        }
    }
    // Final run
    if current_run.len() >= 2 {
        let cleaned = coalesce_degenerate_x(&current_run);
        let segs = smooth_run(&cleaned);
        if !segs.is_empty() {
            bezier_runs.push(segs);
        }
    }

    SeriesGeometry { bezier_runs }
}

/// Compute Y tick positions and labels for the 0–100% axis.
/// Returns (y_position, label_string) in bottom-to-top order.
pub fn compute_y_ticks(bounds: &PlotBounds) -> Vec<(f32, String)> {
    let plot_height = bounds.bottom - bounds.top;
    let mut ticks = Vec::with_capacity(5);
    for pct in [0u8, 25, 50, 75, 100] {
        let fraction = pct as f32 / 100.0;
        let y = bounds.bottom - fraction * plot_height;
        ticks.push((y, format!("{}%", pct)));
    }
    ticks
}

/// Compute horizontal grid-line Y positions (25%, 50%, 75% — edges are the frame).
pub fn compute_grid_y(bounds: &PlotBounds) -> Vec<f32> {
    let plot_height = bounds.bottom - bounds.top;
    let mut lines = Vec::with_capacity(3);
    for pct in [0.25_f32, 0.50, 0.75] {
        let y = bounds.bottom - pct * plot_height;
        lines.push(y);
    }
    lines
}

/// Compute X time-tick positions and labels.
///
/// Generates ticks at regular intervals within the window. The rightmost tick
/// is "now", and ticks proceed left toward the oldest edge. Returns (x, label)
/// pairs in left-to-right order.
pub fn compute_time_ticks(
    window_secs: f64,
    plot_left: f32,
    plot_right: f32,
    // Unused: tick x is derived directly from age (see below), never from an
    // absolute timestamp — that avoids a u64 underflow when uptime < window.
    _window_end_ns: u64,
) -> Vec<(f32, String)> {
    let window_ns = (window_secs * 1e9) as u64;
    let window_s = window_secs as u64;
    let plot_width = plot_right - plot_left;

    // Choose a reasonable tick interval
    let tick_interval_s = if window_s <= 60 {
        10
    } else if window_s <= 300 {
        30
    } else if window_s <= 900 {
        120
    } else {
        300
    };

    // Map an age (in seconds) directly to pixel x, mirroring age_to_x's
    // right-edge fraction math. No absolute timestamp is constructed, so this
    // cannot underflow even when the machine's uptime is shorter than the
    // window (e.g. a 60m preset opened in the first hour after boot).
    let x_for_age = |age_s: u64| -> f32 {
        let age_ns = age_s.saturating_mul(1_000_000_000);
        let fraction = (age_ns as f64 / window_ns.max(1) as f64).min(1.0) as f32;
        plot_right - fraction * plot_width
    };

    let mut ticks = Vec::new();
    ticks.push((x_for_age(0), "now".to_string())); // age 0 → right edge

    let mut age_s = tick_interval_s;
    while age_s <= window_s {
        ticks.push((x_for_age(age_s), format_time_label(age_s)));
        age_s += tick_interval_s;
    }

    ticks
}

fn format_time_label(age_s: u64) -> String {
    if age_s < 60 {
        format!("{}s", age_s)
    } else {
        let m = age_s / 60;
        let s = age_s % 60;
        if s == 0 {
            format!("{}m", m)
        } else {
            format!("{}m{}s", m, s)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ----- helpers -----

    fn make_window(end_ns: u64) -> DrawWindow {
        DrawWindow {
            window_secs: 60.0,
            window_end_ns: end_ns,
        }
    }

    fn make_bounds() -> PlotBounds {
        PlotBounds {
            left: 40.0,
            top: 4.0,
            right: 496.0,
            bottom: 104.0,
        }
    }

    fn pt(val: f32, t_ns: u64) -> SamplePoint {
        SamplePoint::new(t_ns, val)
    }

    fn gap(t_ns: u64) -> SamplePoint {
        SamplePoint::gap(t_ns)
    }

    // ----- age_to_x -----

    #[test]
    fn age_to_x_latest_at_right_edge() {
        let x = age_to_x(100_000_000_000, 100_000_000_000, 60_000_000_000, 40.0, 496.0);
        assert!((x - 496.0).abs() < 0.5);
    }

    #[test]
    fn age_to_x_old_at_left_edge() {
        let x = age_to_x(40_000_000_000, 100_000_000_000, 60_000_000_000, 40.0, 496.0);
        assert!((x - 40.0).abs() < 0.5);
    }

    #[test]
    fn age_to_x_older_than_window_returns_off_left() {
        // Now returns an x value (no longer None); x may be < plot_left
        let x = age_to_x(30_000_000_000, 100_000_000_000, 60_000_000_000, 40.0, 496.0);
        assert!(x < 40.0);
    }

    #[test]
    fn age_to_x_future_clamped_to_right() {
        let x = age_to_x(110_000_000_000, 100_000_000_000, 60_000_000_000, 40.0, 496.0);
        // t_boot > window_end → saturating_sub = 0 → age = 0 → right edge
        assert!((x - 496.0).abs() < 0.5);
    }

    #[test]
    fn early_uptime_places_at_right() {
        let x = age_to_x(5_000_000_000, 5_000_000_000, 60_000_000_000, 40.0, 496.0);
        assert!((x - 496.0).abs() < 0.5);
    }

    #[test]
    fn scroll_without_new_samples_shifts_x() {
        let x1 = age_to_x(50_000_000_000, 100_000_000_000, 60_000_000_000, 40.0, 496.0);
        let x2 = age_to_x(50_000_000_000, 101_000_000_000, 60_000_000_000, 40.0, 496.0);
        assert!(x2 < x1);
    }

    // ----- off-screen cull -----

    #[test]
    fn cull_keeps_all_when_all_in_window() {
        let pts = vec![pt(10.0, 50_000_000_000), pt(20.0, 60_000_000_000)];
        let c = cull_off_screen(&pts, 40_000_000_000, 70_000_000_000);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn cull_keeps_off_left_neighbor() {
        let pts = vec![pt(5.0, 30_000_000_000), pt(10.0, 50_000_000_000), pt(20.0, 60_000_000_000)];
        let c = cull_off_screen(&pts, 45_000_000_000, 70_000_000_000);
        // Off-left neighbor at 30ns + in-window at 50ns, 60ns
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].t_boot_ns, 30_000_000_000);
    }

    #[test]
    fn cull_all_off_left_keeps_newest_only() {
        let pts = vec![pt(5.0, 10_000_000_000), pt(10.0, 20_000_000_000), pt(20.0, 30_000_000_000)];
        let c = cull_off_screen(&pts, 40_000_000_000, 70_000_000_000);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].t_boot_ns, 30_000_000_000);
    }

    #[test]
    fn cull_empty_returns_empty() {
        let c = cull_off_screen(&[], 0, 100);
        assert!(c.is_empty());
    }

    // ----- decimate_points (gap-aware) -----

    #[test]
    fn decimate_preserves_gaps() {
        let pts = vec![
            pt(10.0, 1),
            pt(20.0, 2),
            gap(3),
            pt(30.0, 4),
            pt(40.0, 5),
        ];
        let dec = decimate_points(&pts, 2);
        // Should have gap preserved
        let has_gap = dec.iter().any(|p| p.value.is_none());
        assert!(has_gap);
    }

    #[test]
    fn decimate_preserves_endpoints() {
        let pts = (0..100).map(|i| pt(i as f32, i as u64)).collect::<Vec<_>>();
        let dec = decimate_points(&pts, 10);
        // First point preserved
        assert_eq!(dec[0].t_boot_ns, 0);
        // Last point preserved
        assert_eq!(dec[dec.len() - 1].t_boot_ns, 99);
    }

    #[test]
    fn decimate_gaps_at_every_stride() {
        // Dense series with None gaps at every stride offset (critic cycle-2 test)
        let mut pts = Vec::new();
        for i in 0..200u64 {
            if i % 7 == 0 {
                pts.push(gap(i));
            } else {
                pts.push(pt((i % 100) as f32, i));
            }
        }
        let dec = decimate_points(&pts, 20);
        let gap_count = dec.iter().filter(|p| p.value.is_none()).count();
        // Gaps must not have been elided
        assert!(gap_count > 0);
        // Every gap in original should have a matching gap in decimated
        for p in &pts {
            if p.value.is_none() {
                assert!(dec.iter().any(|d| d.t_boot_ns == p.t_boot_ns));
            }
        }
    }

    // ----- smooth_run -----

    #[test]
    fn smooth_single_point_returns_empty() {
        let segs = smooth_run(&[(0.0, 50.0)]);
        assert!(segs.is_empty());
    }

    #[test]
    fn smooth_two_points_produces_one_bezier() {
        let segs = smooth_run(&[(0.0, 0.0), (100.0, 100.0)]);
        assert_eq!(segs.len(), 1);
        // Roughly: start, c1, c2, end should progress left to right
        assert!(segs[0].start.0 < segs[0].c1.0);
        assert!(segs[0].c1.0 < segs[0].c2.0);
        assert!(segs[0].c2.0 < segs[0].end.0);
    }

    #[test]
    fn smooth_flat_line_is_straight() {
        let segs = smooth_run(&[(0.0, 50.0), (100.0, 50.0), (200.0, 50.0)]);
        // All control points should be along the same horizontal line
        for seg in &segs {
            assert!((seg.c1.1 - 50.0).abs() < 0.01, "c1.y = {}", seg.c1.1);
            assert!((seg.c2.1 - 50.0).abs() < 0.01, "c2.y = {}", seg.c2.1);
        }
    }

    #[test]
    fn smooth_monotone_no_overshoot() {
        // Strictly increasing x, strictly increasing y
        let pts: Vec<(f32, f32)> = (0..10).map(|i| (i as f32 * 10.0, i as f32 * 8.0)).collect();
        let segs = smooth_run(&pts);
        // Every Bézier point should stay within [0, 72] on y
        for seg in &segs {
            let ys = [seg.start.1, seg.c1.1, seg.c2.1, seg.end.1];
            for &y in &ys {
                assert!(y >= 0.0 && y <= 72.0, "y={y} out of range");
            }
        }
    }

    #[test]
    fn smooth_monotone_peak_no_overshoot() {
        // Peak at middle: no part of curve should exceed the peak
        let pts = vec![
            (0.0, 0.0),
            (10.0, 50.0),
            (20.0, 100.0),
            (30.0, 50.0),
            (40.0, 0.0),
        ];
        let segs = smooth_run(&pts);
        for seg in &segs {
            let ys = [seg.start.1, seg.c1.1, seg.c2.1, seg.end.1];
            for &y in &ys {
                assert!(y >= 0.0 && y <= 100.0, "y={y} out of range");
            }
        }
    }

    #[test]
    fn smooth_bezier_x_monotonic() {
        // x values within each Bézier should be monotonic
        let pts: Vec<(f32, f32)> = (0..5)
            .map(|i| (i as f32 * 25.0, ((i * 13) % 80) as f32))
            .collect();
        let segs = smooth_run(&pts);
        for seg in &segs {
            assert!(seg.start.0 <= seg.c1.0);
            assert!(seg.c1.0 <= seg.c2.0);
            assert!(seg.c2.0 <= seg.end.0);
        }
    }

    // ----- degenerate-x coalesce -----

    #[test]
    fn coalesce_drops_duplicate_x() {
        let pts = vec![(0.0, 10.0), (0.0, 20.0), (1.0, 30.0)];
        let cleaned = coalesce_degenerate_x(&pts);
        assert_eq!(cleaned.len(), 2);
        assert_eq!(cleaned[0], (0.0, 10.0));
        assert_eq!(cleaned[1], (1.0, 30.0));
    }

    #[test]
    fn coalesce_drops_non_monotonic_x() {
        let pts = vec![(0.0, 10.0), (5.0, 20.0), (3.0, 30.0), (6.0, 40.0)];
        let cleaned = coalesce_degenerate_x(&pts);
        // (3.0, 30.0) should be dropped because 3 <= 5
        assert_eq!(cleaned.len(), 3);
        assert_eq!(cleaned[0], (0.0, 10.0));
        assert_eq!(cleaned[1], (5.0, 20.0));
        assert_eq!(cleaned[2], (6.0, 40.0));
    }

    #[test]
    fn coalesce_empty() {
        assert!(coalesce_degenerate_x(&[]).is_empty());
    }

    #[test]
    fn coalesce_single_point() {
        assert_eq!(coalesce_degenerate_x(&[(1.0, 10.0)]), vec![(1.0, 10.0)]);
    }

    // ----- ticks -----

    #[test]
    fn y_ticks_range() {
        let bounds = make_bounds(); // top=4, bottom=104, height=100
        let ticks = compute_y_ticks(&bounds);
        assert_eq!(ticks.len(), 5);
        // 0% at bottom, 100% at top
        assert!((ticks[0].1 == "0%"));
        assert!((ticks[4].1 == "100%"));
        assert!((ticks[0].0 - 104.0).abs() < 0.5); // 0% → bottom
        assert!((ticks[4].0 - 4.0).abs() < 0.5); // 100% → top
    }

    #[test]
    fn time_ticks_60s_window() {
        let window_end = 120_000_000_000u64;
        let ticks = compute_time_ticks(60.0, 40.0, 496.0, window_end);
        assert!(!ticks.is_empty());
        // Should include "now" and some time labels
        assert!(ticks.iter().any(|(_, l)| l == "now"));
        // 60s window with 10s interval → 7 ticks (0s, 10s, ..., 60s)
        assert_eq!(ticks.len(), 7);
    }

    #[test]
    fn grid_y_positions() {
        let bounds = make_bounds();
        let ys = compute_grid_y(&bounds);
        assert_eq!(ys.len(), 3);
        // 25%: top(4) + 0.75*100 = 79
        // 50%: top(4) + 0.50*100 = 54
        // 75%: top(4) + 0.25*100 = 29
        assert!((ys[0] - 79.0).abs() < 0.5);
        assert!((ys[1] - 54.0).abs() < 0.5);
        assert!((ys[2] - 29.0).abs() < 0.5);
    }

    // ----- compute_series integration -----

    #[test]
    fn compute_series_gaps_split_bezier_runs() {
        // Second-scale spacing in a 60s window so the gap is wider than 1px and
        // is preserved (a sub-pixel gap would correctly be coalesced away).
        let s = 1_000_000_000u64;
        let pts = vec![
            pt(10.0, 10 * s),
            pt(20.0, 20 * s),
            gap(25 * s),
            pt(30.0, 30 * s),
            pt(40.0, 40 * s),
        ];
        let window = make_window(40 * s);
        let bounds = make_bounds();
        let geom = compute_series(&pts, 100.0, &window, &bounds, false);
        // Two gap-free runs → two Vec<BezierSeg>
        assert_eq!(geom.bezier_runs.len(), 2);
    }

    // ----- sub-pixel gap coalescing (Codex re-review finding 1) -----

    #[test]
    fn coalesce_drops_single_subpixel_gap() {
        // Gap between two data points 1ns apart, threshold 100ns → dropped.
        let pts = vec![pt(10.0, 0), gap(1), pt(20.0, 2)];
        let out = coalesce_subpixel_gaps(&pts, 100.0);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|p| p.value.is_some()));
    }

    #[test]
    fn coalesce_keeps_wide_gap() {
        // Gap whose neighbors are 500ns apart, threshold 100ns → preserved.
        let pts = vec![pt(10.0, 0), gap(250), pt(20.0, 500)];
        let out = coalesce_subpixel_gaps(&pts, 100.0);
        assert_eq!(out.len(), 3);
        assert!(out[1].value.is_none());
    }

    #[test]
    fn coalesce_drops_whole_subpixel_gap_run() {
        // A contiguous multi-gap run bracketed by data <1px apart is merged as a
        // whole (finding 2): value,value,gap,gap,value with bracketing 3ns < 100.
        let pts = vec![pt(10.0, 0), pt(11.0, 1), gap(2), gap(3), pt(20.0, 4)];
        let out = coalesce_subpixel_gaps(&pts, 100.0);
        assert!(out.iter().all(|p| p.value.is_some()), "whole gap run should merge");
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn coalesce_keeps_wide_multi_gap_run() {
        // A contiguous gap run whose bracketing data is > 1px apart is a real
        // absence and is preserved — collapsed to a single marker (finding 1).
        let pts = vec![pt(10.0, 0), gap(100), gap(200), pt(20.0, 300)];
        let out = coalesce_subpixel_gaps(&pts, 100.0);
        let gaps = out.iter().filter(|p| p.value.is_none()).count();
        assert_eq!(gaps, 1);
        assert_eq!(out.len(), 3); // pt, single gap marker, pt
    }

    #[test]
    fn coalesce_allgap_collapses_to_single_marker() {
        // An all-unavailable 7200-point series must not retain 7200 markers (a
        // per-marker allocation storm downstream); the preserved leading gap run
        // collapses to one marker, and the pipeline yields no geometry.
        let pts: Vec<SamplePoint> = (0..7200u64).map(|i| gap(i * 1_000_000_000)).collect();
        let out = coalesce_subpixel_gaps(&pts, 1000.0);
        assert!(out.len() <= 1, "all-gap should collapse to <=1 marker, got {}", out.len());

        let bounds = make_bounds();
        let window = DrawWindow {
            window_secs: 60.0,
            window_end_ns: 7200 * 1_000_000_000,
        };
        let geom = compute_series(&pts, 100.0, &window, &bounds, true);
        assert!(geom.bezier_runs.is_empty());
    }

    #[test]
    fn coalesce_preserves_gap_at_nonmonotonic_timestamp() {
        // Finding 1 (HIGH): when the data after the gap is NOT strictly later
        // (reversed/duplicate timestamp), nt <= pt, so the gap must NOT be
        // treated as sub-pixel and erased — that would draw a line across a real
        // gap. Threshold is huge to prove the monotonic guard, not the distance.
        let pts = vec![pt(10.0, 100), gap(120), pt(20.0, 50), pt(30.0, 200)];
        let out = coalesce_subpixel_gaps(&pts, 1_000_000.0);
        assert!(
            out.iter().any(|p| p.value.is_none()),
            "gap adjacent to a non-monotonic timestamp must be preserved"
        );
    }

    #[test]
    fn coalesce_fractional_width_threshold() {
        // Finding 4 (LOW): with a fractional pixel width the f64 threshold must
        // not merge a gap that spans MORE than one pixel. window 100ns / width
        // 3.5px → 28.57ns/px; a gap bracketing 30ns (>1px) must be preserved,
        // whereas the old u64-truncated width (→3 → 33.3ns/px) would wrongly merge.
        let min_dt = 100.0f64 / 3.5;
        let pts = vec![pt(10.0, 0), gap(15), pt(20.0, 30)];
        let out = coalesce_subpixel_gaps(&pts, min_dt);
        assert!(out.iter().any(|p| p.value.is_none()), "supra-pixel gap must survive");
    }

    #[test]
    fn coalesce_bounds_pathological_run_count() {
        // Per-tick value,value,gap,gap stream at 1s spacing in a 2h window: every
        // gap run is sub-pixel and must coalesce wholesale, collapsing ~1800 runs
        // into ~1 (findings 1 & 2 combined).
        let window_ns = 7200.0 * 1e9;
        let plot_width = 700.0;
        let min_dt = window_ns / plot_width; // ~10.3s per pixel
        let mut pts = Vec::new();
        for i in 0..7200u64 {
            if i % 4 >= 2 {
                pts.push(gap(i * 1_000_000_000));
            } else {
                pts.push(pt((i % 100) as f32, i * 1_000_000_000));
            }
        }
        let out = coalesce_subpixel_gaps(&pts, min_dt);
        let remaining_gaps = out.iter().filter(|p| p.value.is_none()).count();
        // Only a boundary gap run with no following data can remain.
        assert!(
            remaining_gaps <= 2,
            "sub-pixel gaps not bounded: {remaining_gaps} remain (expected <= 2)"
        );
    }

    #[test]
    fn compute_series_empty_points() {
        let window = make_window(100);
        let bounds = make_bounds();
        let geom = compute_series(&[], 100.0, &window, &bounds, false);
        assert!(geom.bezier_runs.is_empty());
    }

    #[test]
    fn compute_series_all_gaps() {
        let pts = vec![gap(1), gap(2), gap(3)];
        let window = make_window(4);
        let bounds = make_bounds();
        let geom = compute_series(&pts, 100.0, &window, &bounds, false);
        assert!(geom.bezier_runs.is_empty());
    }

    #[test]
    fn compute_series_single_point() {
        let pts = vec![pt(50.0, 2_000_000)];
        let window = make_window(2_000_000);
        let bounds = make_bounds();
        let geom = compute_series(&pts, 100.0, &window, &bounds, false);
        // Need at least 2 points for a Bézier run
        assert!(geom.bezier_runs.is_empty());
    }

    #[test]
    fn compute_series_boundary_crossing() {
        // One point inside window, one off-left neighbor
        let window_end = 70_000_000_000u64;
        let pts = vec![
            pt(10.0, 5_000_000_000),  // off-left
            pt(20.0, 15_000_000_000), // in-window
        ];
        let window = DrawWindow {
            window_secs: 60.0,
            window_end_ns: window_end,
        };
        let bounds = make_bounds();
        let geom = compute_series(&pts, 100.0, &window, &bounds, false);
        // Two points = one Bézier, but the off-left point maps to x < plot_left
        // Both should be kept by cull → one run
        assert_eq!(geom.bezier_runs.len(), 1);
        let seg = &geom.bezier_runs[0][0];
        // start.x should be off-left (< 40.0)
        assert!(seg.start.0 < 40.0);
        // end.x should be in-window (>= 40.0)
        assert!(seg.end.0 >= 40.0);
    }

    // ----- early-uptime tick safety (finding 2, HIGH) -----

    #[test]
    fn time_ticks_early_uptime_no_underflow() {
        // Uptime (window_end_ns) far shorter than the window must not panic and
        // must keep every tick x within the plot span. A 60m window opened 5s
        // after boot would underflow the old absolute-timestamp math.
        let ticks = compute_time_ticks(3600.0, 40.0, 496.0, 5_000_000_000);
        assert!(!ticks.is_empty());
        for (x, _) in &ticks {
            assert!(
                *x >= 40.0 - 0.5 && *x <= 496.0 + 0.5,
                "tick x {x} out of plot span [40,496]"
            );
        }
    }

    // ----- worst-case geometry budget (finding 4) -----

    #[test]
    fn worst_case_geometry_within_budget() {
        // 256 series × 7200 points (max supported bounds), fragmented with
        // periodic gap bursts (suspend-like), decimation ON. The PRIMARY
        // assertion is deterministic: the global decimation budget must cap each
        // series near the pixel target (~700 segs) regardless of fragmentation —
        // without it, a fragmented series would retain thousands of points and
        // blow up tessellation. Timing is a loose secondary sanity check, made
        // build-mode aware because `cargo test` is an unoptimized debug build
        // (~137ms here) while the shipped RELEASE build is ~10x faster (~21ms,
        // well under the 100ms display tick).
        let bounds = PlotBounds {
            left: 40.0,
            top: 4.0,
            right: 740.0, // ~700px plot width
            bottom: 204.0,
        };
        let window = DrawWindow {
            window_secs: 7200.0,
            window_end_ns: 7200 * 1_000_000_000,
        };

        let mut pts = Vec::with_capacity(7200);
        for i in 0..7200u64 {
            let t = i * 1_000_000_000;
            if (i / 60) % 20 == 0 {
                pts.push(gap(t)); // ~5% gaps, in bursts
            } else {
                pts.push(pt((i % 100) as f32, t));
            }
        }

        let start = std::time::Instant::now();
        let mut total_segs = 0usize;
        for _ in 0..256 {
            let geom = compute_series(&pts, 100.0, &window, &bounds, true);
            total_segs += geom.bezier_runs.iter().map(|r| r.len()).sum::<usize>();
        }
        let elapsed = start.elapsed();

        // Primary: on realistic bursty-gap data the global stride bounds each
        // series near the pixel target (~700) (critic finding 4 / cycle-2).
        let per_series = total_segs / 256;
        assert!(
            per_series < 900,
            "global decimation not bounding output: {per_series} segs/series (pixel target ~700)"
        );

        // Adversarial fragmentation (Codex re-review finding 1): a repeating
        // value,value,gap stream WOULD force ~2400 two-point runs (614k segs,
        // ~192ms release — over the frame tick) without mitigation. Sub-pixel gap
        // coalescing collapses these invisible per-tick gaps up front, so this
        // case is now bounded to the SAME ~700 segs/series as bursty data.
        let mut frag = Vec::with_capacity(7200);
        for i in 0..7200u64 {
            let t = i * 1_000_000_000;
            if i % 3 == 2 {
                frag.push(gap(t));
            } else {
                frag.push(pt((i % 100) as f32, t));
            }
        }
        let frag_start = std::time::Instant::now();
        let mut frag_segs = 0usize;
        for _ in 0..256 {
            let geom = compute_series(&frag, 100.0, &window, &bounds, true);
            frag_segs += geom.bezier_runs.iter().map(|r| r.len()).sum::<usize>();
        }
        let frag_elapsed = frag_start.elapsed();

        // The adversarial case must now be bounded like the bursty case, proving
        // coalescing defeats the O(num_runs) blow-up.
        let frag_per_series = frag_segs / 256;
        assert!(
            frag_per_series < 900,
            "coalescing failed to bound fragmentation: {frag_per_series} segs/series"
        );

        // Secondary sanity on BOTH cases: catch an order-of-magnitude slowdown.
        // Loose and build-mode aware (debug is ~10x slower than shipped release);
        // both stay well under the 100ms display tick in release.
        let ceiling_ms: u128 = if cfg!(debug_assertions) { 400 } else { 60 };
        assert!(
            elapsed.as_millis() < ceiling_ms,
            "bursty 256×7200 build took {elapsed:?} (segs={total_segs}), exceeds {ceiling_ms}ms"
        );
        assert!(
            frag_elapsed.as_millis() < ceiling_ms,
            "adversarial 256×7200 build took {frag_elapsed:?} (segs={frag_segs}), exceeds {ceiling_ms}ms"
        );
    }

    // ----- deterministic successive-frame continuity (finding 6) -----

    // Evaluate one axis of a cubic Bézier (Bernstein form) at parameter t.
    fn bez1(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
        let mt = 1.0 - t;
        mt * mt * mt * a + 3.0 * mt * mt * t * b + 3.0 * mt * t * t * c + t * t * t * d
    }

    /// Evaluate the ACTUAL smoothed cubic at the point where its x crosses
    /// `target_x` (bisection; Bézier x is monotonic within a segment). Returns
    /// the real curve y — sensitive to control-point / tangent changes, unlike a
    /// straight interp between segment endpoints.
    fn curve_y_at_x(geom: &SeriesGeometry, target_x: f32) -> Option<f32> {
        for run in &geom.bezier_runs {
            for seg in run {
                let xlo = seg.start.0.min(seg.end.0);
                let xhi = seg.start.0.max(seg.end.0);
                if target_x < xlo || target_x > xhi {
                    continue;
                }
                let (mut lo, mut hi) = (0.0f32, 1.0f32);
                for _ in 0..40 {
                    let mid = 0.5 * (lo + hi);
                    let x = bez1(seg.start.0, seg.c1.0, seg.c2.0, seg.end.0, mid);
                    if x < target_x {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let t = 0.5 * (lo + hi);
                return Some(bez1(seg.start.1, seg.c1.1, seg.c2.1, seg.end.1, t));
            }
        }
        None
    }

    #[test]
    fn successive_frame_continuity_at_clip_boundary() {
        // Prove the stutter fix on a REAL ring with LIVE eviction: push a new
        // sample as the clock crosses each second (evicting the oldest), while
        // advancing window_end in sub-sample (0.1s) steps. Assert the ACTUAL
        // smoothed cubic evaluated at the visible left clip edge (x = plot_left)
        // moves continuously across those eviction events — no pop.
        use crate::model::Ring;

        let bounds = make_bounds(); // left=40, right=496
        let window_secs = 60.0;
        let sample_v = |s: u64| 50.0 + 15.0 * (s as f32 * 0.2).sin(); // smooth

        // Capacity 62 = 60s window + 2 edge-guard; prime with 80 samples.
        let mut ring = Ring::new(62);
        for i in 0..80u64 {
            ring.push(SamplePoint::new(i * 1_000_000_000, sample_v(i)));
        }

        // Sweep window_end 79.0s → 84.0s in 0.1s steps; push real samples as the
        // clock crosses each integer second so eviction is genuinely exercised.
        let mut next_push_s = 80u64;
        let mut prev: Option<f32> = None;
        let mut observed = 0;
        for step in 0..=50u64 {
            let end_ns = 79_000_000_000 + step * 100_000_000;
            let end_s = end_ns / 1_000_000_000;
            while next_push_s <= end_s {
                ring.push(SamplePoint::new(
                    next_push_s * 1_000_000_000,
                    sample_v(next_push_s),
                ));
                next_push_s += 1;
            }

            let window = DrawWindow {
                window_secs,
                window_end_ns: end_ns,
            };
            let pts = ring.points();
            let geom = compute_series(&pts, 100.0, &window, &bounds, false);

            // The curve MUST cross the left clip edge on EVERY frame — a frame
            // with no crossing is exactly the disappearance (pop) this test
            // exists to catch (finding 3). So require it, don't skip.
            let y = curve_y_at_x(&geom, bounds.left)
                .unwrap_or_else(|| panic!("no curve at left clip edge on frame {step} (segment disappeared)"));
            if let Some(py) = prev {
                // One 0.1s step is ~1/10 of a sample; the left-edge y must move
                // far less than a full sample's worth (max ~3.0), including
                // across the push/evict events. Threshold < 1 sample.
                let dy = (y - py).abs();
                assert!(
                    dy < 1.5,
                    "left-edge y discontinuity at step {step}: prev={py} y={y} dy={dy}"
                );
            }
            prev = Some(y);
            observed += 1;
        }
        assert_eq!(observed, 51, "expected a left-edge crossing on every frame");
    }
}
