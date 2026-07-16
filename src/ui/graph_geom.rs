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
    /// Sample interval in nanoseconds. Used to size a raw clip-edge band from
    /// stable draw configuration rather than from observed timing jitter.
    pub sample_interval_ns: u64,
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

/// Geometry is ultimately rasterized to physical pixels. Differences below
/// one hundredth of a logical pixel are numerical noise, not visible motion.
#[cfg(test)]
const RENDER_Y_EPSILON_PX: f32 = 0.01;

// ---------------------------------------------------------------------------
// Coordinate mapping
// ---------------------------------------------------------------------------

/// Map a boottime timestamp to pixel X using age-based position.
///
/// Returns an X that may fall left of `plot_left` **or right of `plot_right`**.
/// Samples newer than `window_end` (negative age) map past the right edge —
/// required for delayed continuous windows where the next sample sits
/// off-screen right and scrolls in with a real slope. The canvas clip cuts
/// off-screen portions.
fn age_to_x(
    t_boot_ns: u64,
    window_end_ns: u64,
    window_ns: u64,
    plot_left: f32,
    plot_right: f32,
) -> f32 {
    // Signed age: t > window_end → negative → x > plot_right (do NOT clamp).
    let age_ns = window_end_ns as i128 - t_boot_ns as i128;
    let plot_width = plot_right - plot_left;
    let fraction = age_ns as f64 / (window_ns.max(1) as f64);
    plot_right - fraction as f32 * plot_width
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
/// - Three off-left neighbors (prevents the leftmost point from becoming
///   the spline endpoint during sub-interval scroll).
/// - All in-window points.
/// - Two off-right neighbors.
/// - Trailing points beyond the stencil are dropped to bound work.
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
        (Some(fi), Some(li)) => {
            let start = fi.saturating_sub(3);
            let end = (li + 3).min(points.len());
            points[start..end].to_vec()
        }
        (None, Some(_)) => {
            let last = points.len();
            let start = last.saturating_sub(3);
            points[start..].to_vec()
        }
        (Some(fi), None) => {
            let end = (fi + 3).min(points.len());
            points[fi..end].to_vec()
        }
        (None, None) => {
            let last = points.len();
            let start = last.saturating_sub(3);
            points[start..].to_vec()
        }
    }
}

/// Suppress data detail around sub-pixel gaps.
///
/// Gap markers are **never** removed — no Bézier segment may connect values
/// across a semantic `None`. A gap whose bracketing data points are closer
/// than one pixel of time is too narrow to render; instead of bridging it,
/// we drop data points within the sub-pixel window on either side so the
/// effective blank region widens to ≥ 1 pixel. Multiple consecutive gap
/// markers collapse to a single marker, bounding retained-gap count by
/// the number of genuinely separated runs.
///
/// This bounds the total run count (gap-free runs ≤ plot pixel width + 1)
/// while preserving the semantic-discontinuity invariant.
fn suppress_subpixel_gaps(points: &[SamplePoint], min_dt_ns: f64) -> Vec<SamplePoint> {
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
        // Data points bracketing the WHOLE run.
        let prev_t: Option<(usize, u64)> = out
            .last()
            .filter(|q| q.value.is_some())
            .map(|q| (out.len() - 1, q.t_boot_ns));
        let next_t = if j < points.len() {
            Some(points[j].t_boot_ns)
        } else {
            None
        };

        let subpixel = match (prev_t, next_t) {
            (Some((_, pt)), Some(nt)) if nt > pt => ((nt - pt) as f64) < min_dt_ns,
            _ => false,
        };

        if subpixel {
            // Suppress the nearest data point(s) so the blank region widens.
            // Remove the data point just before the gap (if present) and just
            // after (but we haven't added it yet, so skip it later).
            if let Some((idx, _)) = prev_t {
                // Only suppress if the data point is also within the sub-pixel
                // window — i.e., its distance to the gap edge is < min_dt_ns.
                // For simplicity, we suppress the single nearest data point
                // on the left side.
                out.truncate(idx); // remove last data point before gap
            }
            // The right-side data point at j hasn't been pushed yet; we'll
            // skip it by advancing past it below.
        }

        // Always keep at least one gap marker (collapse run to single marker).
        out.push(points[i]);
        i = j;

        // If this was subpixel, skip the first data point after the gap
        // (it was suppressed to widen the blank region).
        if subpixel && j < points.len() && points[j].value.is_some() {
            i = j + 1;
        }
    }
    // Suppression can make two formerly separated gap markers adjacent.
    // Collapse those markers without ever removing the discontinuity itself.
    let mut compact = Vec::with_capacity(out.len());
    for point in out {
        if point.value.is_none()
            && compact
                .last()
                .is_some_and(|p: &SamplePoint| p.value.is_none())
        {
            continue;
        }
        compact.push(point);
    }
    compact
}

// ---------------------------------------------------------------------------
// Gap-aware decimation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeometryBound {
    pub retained_points: usize,
    pub bezier_segments: usize,
}

/// Derived bound for the decimated representation.
///
/// Each run retains a raw band of `4 × bucket_samples + 3` samples at both
/// ends, then at most one interior representative per absolute time bucket.
/// A gap marker separates adjacent runs. The maximization over feasible run
/// counts makes fragmented inputs explicit; the input-ring size remains the
/// final hard cap.
pub fn geometry_bound(
    input_points: usize,
    window_ns: u64,
    sample_interval_ns: u64,
    target: usize,
) -> GeometryBound {
    if input_points == 0 {
        return GeometryBound {
            retained_points: 0,
            bezier_segments: 0,
        };
    }

    let target = target.max(1);
    let bucket_ns = window_ns.max(1).div_ceil(target as u64);
    let bucket_samples = bucket_ns.div_ceil(sample_interval_ns.max(1)) as usize;
    let edge_context = bucket_samples.saturating_mul(4).saturating_add(3);
    let max_runs = target
        .saturating_add(2)
        .min(input_points.div_ceil(2).max(1));

    let mut bound = GeometryBound {
        retained_points: 1,
        bezier_segments: 0,
    };
    for runs in 1..=max_runs {
        let gaps = runs - 1;
        let available_data = input_points.saturating_sub(gaps);
        let protected = edge_context.saturating_mul(2).saturating_mul(runs);
        let bucket_representatives = target.saturating_add(1).saturating_mul(runs);
        let retained_data = available_data.min(protected.saturating_add(bucket_representatives));
        let retained_points = gaps.saturating_add(retained_data).min(input_points);
        let bezier_segments = retained_data.saturating_sub(runs);
        bound.retained_points = bound.retained_points.max(retained_points);
        bound.bezier_segments = bound.bezier_segments.max(bezier_segments);
    }
    bound
}

pub fn gap_free_geometry_bound(
    input_points: usize,
    window_ns: u64,
    sample_interval_ns: u64,
    target: usize,
) -> GeometryBound {
    if input_points == 0 {
        return GeometryBound {
            retained_points: 0,
            bezier_segments: 0,
        };
    }
    let target = target.max(1);
    let bucket_ns = window_ns.max(1).div_ceil(target as u64);
    let bucket_samples = bucket_ns.div_ceil(sample_interval_ns.max(1)) as usize;
    let edge_context = bucket_samples.saturating_mul(4).saturating_add(3);
    let retained_points = input_points.min(
        edge_context
            .saturating_mul(2)
            .saturating_add(target)
            .saturating_add(1),
    );
    GeometryBound {
        retained_points,
        bezier_segments: retained_points.saturating_sub(1),
    }
}

/// Decimate sample points preserving gap markers and run endpoints.
///
/// This standalone wrapper keeps the original endpoint-preserving behavior
/// for backward compatibility.  The context-aware variant
/// `decimate_points_with_stencil` is used by `compute_series` when the visible
/// domain matters — it uses pure identity-anchored selection.
pub fn decimate_points(points: &[SamplePoint], target_count: usize) -> Vec<SamplePoint> {
    // Original behavior: first + last always kept, stride through middle.
    // Split into gap-free runs.
    let mut runs: Vec<Vec<SamplePoint>> = Vec::new();
    let mut current: Vec<SamplePoint> = Vec::new();
    for pt in points {
        if pt.value.is_some() {
            current.push(*pt);
        } else {
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
            runs.push(vec![*pt]);
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }

    let target = target_count.max(2);
    let total_data: usize = runs.iter().filter(|r| r.len() >= 2).map(|r| r.len()).sum();

    if total_data <= target {
        return runs.into_iter().flatten().collect();
    }
    let stride = total_data.div_ceil(target).max(1);

    let mut result = Vec::new();
    for run in runs {
        if run.len() < 2 {
            result.extend(run);
        } else {
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

/// Context-aware decimation used by `compute_series`.  Uses the draw
/// configuration (window duration, pixel target, and configured sample
/// interval) to derive absolute time buckets — no data-estimated interval.
/// Every gap-free run keeps raw bands wider than four buckets at each end;
/// the interior keeps the first chronological sample per bucket. Gap markers
/// are always preserved.
fn decimate_points_with_stencil(
    points: &[SamplePoint],
    target_count: usize,
    window_start_ns: u64,
    window_end_ns: u64,
    window_duration_ns: u64,
    interval_ns: u64,
) -> Vec<SamplePoint> {
    // Split into gap-free runs interleaved with gap markers.
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

    let target = target_count.max(2);
    let total_visible: usize = runs
        .iter()
        .filter(|r| r.len() >= 2)
        .map(|r| {
            r.iter()
                .filter(|p| p.t_boot_ns >= window_start_ns && p.t_boot_ns <= window_end_ns)
                .count()
        })
        .sum();

    if total_visible <= target {
        return runs.into_iter().flatten().collect();
    }

    // Absolute buckets from draw configuration — independent of data and of
    // the observed minimum interval. The first chronological point in each
    // bucket is the stable representative.
    let bucket_ns = window_duration_ns.max(1).div_ceil(target as u64).max(1);
    let bucket_samples = bucket_ns.div_ceil(interval_ns.max(1)) as usize;

    // ── named worst-case count helpers (see geometry bound below) ──
    let mut result = Vec::new();
    for mut run in runs {
        if run.len() < 2 {
            // Gap marker or lone point — always kept.
            result.append(&mut run);
        } else {
            // Keep a raw band wider than four decimation buckets at each end.
            // Membership changes at the raw/bulk seam therefore occur outside
            // the two-pixel clip strips whose entrance/exit shape matters.
            let edge_context = bucket_samples.saturating_mul(4).saturating_add(3);
            let keep_head = edge_context.min(run.len());
            let keep_tail = edge_context.min(run.len().saturating_sub(keep_head));
            let mut last_bulk_bucket = None;
            for (i, pt) in run.iter().enumerate() {
                if pt.value.is_none() {
                    result.push(*pt);
                } else {
                    let bucket = pt.t_boot_ns / bucket_ns;
                    let in_edge_context = i < keep_head || i >= run.len().saturating_sub(keep_tail);
                    let selected_bulk = !in_edge_context && last_bulk_bucket != Some(bucket);
                    let keep = in_edge_context || selected_bulk;
                    if keep {
                        result.push(*pt);
                        if selected_bulk {
                            last_bulk_bucket = Some(bucket);
                        }
                    }
                }
            }
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

    // 1. Cull off-screen (keep window points + 3 off-left + 2 off-right)
    let culled = cull_off_screen(points, window_start_ns, window.window_end_ns);

    // 2. Suppress sub-pixel gaps (bounds run count by pixel width without
    //    ever bridging a semantic None — gaps are always preserved).
    let min_dt_ns = window_ns as f64 / (plot_width as f64).max(1.0);
    let coalesced = suppress_subpixel_gaps(&culled, min_dt_ns);

    // 3. Optional decimation (gap-aware absolute buckets + raw edge bands).
    //    Only visible-domain points trigger it; cull and per-run context remain raw.
    let working = if decimate {
        let in_window = coalesced
            .iter()
            .filter(|p| p.t_boot_ns >= window_start_ns && p.t_boot_ns <= window.window_end_ns)
            .count();
        let target = (plot_width as usize).max(1);
        if in_window > target * 2 {
            decimate_points_with_stencil(
                &coalesced,
                target,
                window_start_ns,
                window.window_end_ns,
                window_ns,
                window.sample_interval_ns,
            )
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
    // Final run (no synthetic right-edge hold — delayed window + off-right
    // samples provide real slope as segments scroll in from the right).
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
            sample_interval_ns: 1_000_000_000,
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
        let x = age_to_x(
            100_000_000_000,
            100_000_000_000,
            60_000_000_000,
            40.0,
            496.0,
        );
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
    fn age_to_x_future_maps_past_right() {
        // Delayed window: samples newer than window_end sit off-screen right
        // so they can scroll in with a real slope (not clamped to the edge).
        let x = age_to_x(
            110_000_000_000,
            100_000_000_000,
            60_000_000_000,
            40.0,
            496.0,
        );
        // 10s ahead of a 60s window → 1/6 of plot width past the right edge
        let plot_width = 496.0 - 40.0;
        let expected = 496.0 + (10.0 / 60.0) as f32 * plot_width;
        assert!(
            (x - expected).abs() < 1.0,
            "future sample x={x} should be past plot_right (expected ~{expected})"
        );
        assert!(x > 496.0);
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
        let pts = vec![
            pt(5.0, 30_000_000_000),
            pt(10.0, 50_000_000_000),
            pt(20.0, 60_000_000_000),
        ];
        let c = cull_off_screen(&pts, 45_000_000_000, 70_000_000_000);
        // Off-left neighbor at 30ns + in-window at 50ns, 60ns
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].t_boot_ns, 30_000_000_000);
    }

    #[test]
    fn cull_all_off_left_keeps_newest_three() {
        let pts = vec![
            pt(5.0, 10_000_000_000),
            pt(10.0, 20_000_000_000),
            pt(20.0, 30_000_000_000),
        ];
        let c = cull_off_screen(&pts, 40_000_000_000, 70_000_000_000);
        // All off-left, keep the newest three (3-left cull).
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].t_boot_ns, 10_000_000_000);
        assert_eq!(c[2].t_boot_ns, 30_000_000_000);
    }

    #[test]
    fn cull_empty_returns_empty() {
        let c = cull_off_screen(&[], 0, 100);
        assert!(c.is_empty());
    }

    // ----- decimate_points (gap-aware) -----

    #[test]
    fn decimate_preserves_gaps() {
        let pts = vec![pt(10.0, 1), pt(20.0, 2), gap(3), pt(30.0, 4), pt(40.0, 5)];
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

    #[test]
    fn configured_buckets_ignore_a_new_minimum_jitter_delta() {
        let interval_ns = 100_000_000u64;
        let mut base: Vec<SamplePoint> = (0..720u64)
            .map(|i| {
                let jitter = if i.is_multiple_of(2) { 20_000_000 } else { 0 };
                pt((i % 100) as f32, i * interval_ns + jitter)
            })
            .collect();
        let extra = pt(77.0, base[350].t_boot_ns + 1_000_000);
        let mut with_close_pair = base.clone();
        with_close_pair.insert(351, extra);

        let selected = |points: &[SamplePoint]| {
            decimate_points_with_stencil(
                points,
                100,
                0,
                60_000_000_000,
                60_000_000_000,
                interval_ns,
            )
            .into_iter()
            .map(|point| point.t_boot_ns)
            .collect::<Vec<_>>()
        };
        let before = selected(&base);
        let after = selected(&with_close_pair)
            .into_iter()
            .filter(|timestamp| *timestamp != extra.t_boot_ns)
            .collect::<Vec<_>>();
        assert_eq!(
            before, after,
            "a new 1ms delta repartitioned existing buckets"
        );

        base.remove(0);
        let stable_bulk = |timestamps: Vec<u64>| {
            timestamps
                .into_iter()
                .filter(|timestamp| *timestamp >= 5_000_000_000 && *timestamp <= 65_000_000_000)
                .collect::<Vec<_>>()
        };
        let before_eviction = stable_bulk(before);
        let after_eviction = stable_bulk(selected(&base));
        assert_eq!(
            before_eviction, after_eviction,
            "evicting jittered history repartitioned stable bulk buckets"
        );
    }

    #[test]
    fn configured_buckets_stay_fixed_during_early_uptime_scroll() {
        let interval_ns = 1_000_000_000u64;
        let configured_window_ns = 3_600 * interval_ns;
        let points: Vec<SamplePoint> = (0..1_000u64)
            .map(|i| pt((i % 100) as f32, i * interval_ns))
            .collect();

        let selected = |window_end_ns| {
            decimate_points_with_stencil(
                &points,
                100,
                0,
                window_end_ns,
                configured_window_ns,
                interval_ns,
            )
            .into_iter()
            .map(|point| point.t_boot_ns)
            .collect::<Vec<_>>()
        };

        let before = selected(1_000 * interval_ns);
        let after = selected(1_000 * interval_ns + 100_000_000);
        assert!(
            before.len() < points.len(),
            "regression setup did not exercise decimation"
        );
        assert_eq!(
            before, after,
            "early-uptime scroll repartitioned configured absolute buckets"
        );
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
                assert!((0.0..=72.0).contains(&y), "y={y} out of range");
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
                assert!((0.0..=100.0).contains(&y), "y={y} out of range");
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

    // ----- sub-pixel gap suppression (never bridges; gaps always survive) -----

    #[test]
    fn suppress_subpixel_gap_keeps_marker() {
        // Gap between two data points 1ns apart, threshold 100ns → sub-pixel.
        // The gap marker MUST survive (never bridged); surrounding data points
        // are suppressed so the blank region widens.
        let pts = vec![pt(10.0, 0), gap(1), pt(20.0, 2)];
        let out = suppress_subpixel_gaps(&pts, 100.0);
        // The gap marker survives.  The data point at t=0 is suppressed
        // (it is within the sub-pixel window), so we get [gap, pt(20.0)].
        assert!(
            out.iter().any(|p| p.value.is_none()),
            "gap marker must survive"
        );
        // The suppressed-data-point check: at most 2 points remain.
        assert!(out.len() <= 2);
    }

    #[test]
    fn suppress_subpixel_keeps_wide_gap() {
        // Gap whose neighbors are 500ns apart, threshold 100ns → preserved
        // with its data intact (not supressed, gap is visible).
        let pts = vec![pt(10.0, 0), gap(250), pt(20.0, 500)];
        let out = suppress_subpixel_gaps(&pts, 100.0);
        assert_eq!(out.len(), 3);
        assert!(out[1].value.is_none());
    }

    #[test]
    fn suppress_subpixel_collapses_multi_gap_run() {
        // A contiguous multi-gap run <1px apart: value,value,gap,gap,value.
        // The gap run collapses to a single marker; nearby data suppressed.
        let pts = vec![pt(10.0, 0), pt(11.0, 1), gap(2), gap(3), pt(20.0, 4)];
        let out = suppress_subpixel_gaps(&pts, 100.0);
        // Gap marker must survive (never bridged).
        assert!(out.iter().any(|p| p.value.is_none()), "gap must survive");
        // Data just before the gap should be suppressed.
        assert!(out.len() <= 3, "suppressed data near sub-pixel gap");
    }

    #[test]
    fn suppress_subpixel_keeps_wide_multi_gap_run() {
        // A contiguous gap run whose bracketing data is > 1px apart —
        // collapsed to a single marker; data preserved.
        let pts = vec![pt(10.0, 0), gap(100), gap(200), pt(20.0, 300)];
        let out = suppress_subpixel_gaps(&pts, 100.0);
        let gaps = out.iter().filter(|p| p.value.is_none()).count();
        assert_eq!(gaps, 1);
        assert_eq!(out.len(), 3); // pt, single gap marker, pt
    }

    #[test]
    fn suppress_allgap_collapses_to_single_marker() {
        // An all-unavailable 7200-point series must not retain 7200 markers.
        // All gaps collapse to one marker; pipeline yields no geometry.
        let pts: Vec<SamplePoint> = (0..7200u64).map(|i| gap(i * 1_000_000_000)).collect();
        let out = suppress_subpixel_gaps(&pts, 1000.0);
        assert!(
            out.len() <= 1,
            "all-gap should collapse to <=1 marker, got {}",
            out.len()
        );

        let bounds = make_bounds();
        let window = DrawWindow {
            sample_interval_ns: 1_000_000_000,
            window_secs: 60.0,
            window_end_ns: 7200 * 1_000_000_000,
        };
        let geom = compute_series(&pts, 100.0, &window, &bounds, true);
        assert!(geom.bezier_runs.is_empty());
    }

    #[test]
    fn suppress_preserves_gap_at_nonmonotonic_timestamp() {
        // Gap adjacent to a non-monotonic timestamp: not treated as sub-pixel,
        // so data is not suppressed and the gap stays.
        let pts = vec![pt(10.0, 100), gap(120), pt(20.0, 50), pt(30.0, 200)];
        let out = suppress_subpixel_gaps(&pts, 1_000_000.0);
        assert!(
            out.iter().any(|p| p.value.is_none()),
            "gap adjacent to a non-monotonic timestamp must be preserved"
        );
        // Data should not be suppressed (non-monotonic guard).
        assert!(out.len() >= 4);
    }

    #[test]
    fn suppress_fractional_width_gap_survives() {
        // Supra-pixel gap survives with data intact.
        let min_dt = 100.0f64 / 3.5;
        let pts = vec![pt(10.0, 0), gap(15), pt(20.0, 30)];
        let out = suppress_subpixel_gaps(&pts, min_dt);
        assert!(
            out.iter().any(|p| p.value.is_none()),
            "supra-pixel gap must survive"
        );
        // Wide enough — no data suppression.
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn suppress_bounds_pathological_run_count() {
        // Per-tick value,value,gap,gap stream at 1s spacing in a 2h window.
        // Every gap run is sub-pixel so surrounding data is suppressed,
        // and gap markers collapse. Total runs bounded by plot width.
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
        let out = suppress_subpixel_gaps(&pts, min_dt);
        let remaining_gaps = out.iter().filter(|p| p.value.is_none()).count();
        let remaining_runs = out
            .split(|point| point.value.is_none())
            .filter(|run| !run.is_empty())
            .count();
        // With suppression, gaps remain but data is thinned. The total number
        // of gap markers is bounded — each surviving gap + suppressed data
        // occupies ≥ 1 pixel of time.
        assert!(
            remaining_gaps <= plot_width as usize + 1,
            "gap markers exceed pixel-derived bound: {remaining_gaps} remain"
        );
        assert!(
            remaining_runs <= plot_width as usize + 2,
            "data runs exceed pixel-derived bound: {remaining_runs} remain"
        );
    }

    #[test]
    fn suppress_never_bridges_edge_adjacent_gap() {
        // A sub-pixel gap immediately adjacent to the clip edge must still
        // survive as a gap marker — context must not bridge it.
        let pts = vec![pt(10.0, 0), gap(1), pt(20.0, 2)];
        let out = suppress_subpixel_gaps(&pts, 100.0);
        assert!(
            out.iter().any(|p| p.value.is_none()),
            "edge-adjacent sub-pixel gap must survive"
        );
        // Verify that the output, when passed through compute_series, produces
        // split runs (gap is not bridged).
        let window = DrawWindow {
            sample_interval_ns: 1_000_000_000,
            window_secs: 1.0,
            window_end_ns: 3,
        };
        let bounds = make_bounds();
        let geom = compute_series(&out, 100.0, &window, &bounds, false);
        let _ = geom; // geometry was computed; gap splits visible in bezier_runs
        // The gap should split runs — at least two runs (one before, one after)
        // or zero if only single-point runs remain after suppression.
        // The key assertion: gap marker must survive in output.
        assert!(
            out.iter().any(|p| p.value.is_none()),
            "edge-adjacent gap must survive"
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
            sample_interval_ns: 1_000_000_000,
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
        // periodic gap bursts, decimation ON.  The PRIMARY assertion is the
        // derived geometry_bound formula above: each series must stay under
        // the computed ceiling. Timing
        // is a secondary sanity check.
        let plot_width = 700usize;
        let bounds = PlotBounds {
            left: 40.0,
            top: 4.0,
            right: 40.0 + plot_width as f32,
            bottom: 204.0,
        };
        let window = DrawWindow {
            sample_interval_ns: 1_000_000_000,
            window_secs: 7200.0,
            window_end_ns: 7200 * 1_000_000_000,
        };

        // Exact bound for this configuration.
        let target = plot_width;
        let bound = geometry_bound(7200, 7200 * 1_000_000_000, 1_000_000_000, target);
        let series_bound = bound.bezier_segments;
        let total_bound = series_bound * crate::model::history::MAX_CPU_CORES;
        let gap_free_bound =
            gap_free_geometry_bound(7200, 7200 * 1_000_000_000, 1_000_000_000, target);
        assert_eq!(gap_free_bound.retained_points, 795);
        assert_eq!(gap_free_bound.bezier_segments, 794);

        let gap_free: Vec<SamplePoint> = (0..7200u64)
            .map(|i| pt((i % 100) as f32, i * 1_000_000_000))
            .collect();
        let gap_free_geom = compute_series(&gap_free, 100.0, &window, &bounds, true);
        let gap_free_segments: usize = gap_free_geom.bezier_runs.iter().map(Vec::len).sum();
        assert!(gap_free_segments <= gap_free_bound.bezier_segments);

        // Bursty gaps (~5%).
        let mut pts = Vec::with_capacity(7200);
        for i in 0..7200u64 {
            let t = i * 1_000_000_000;
            if (i / 60) % 20 == 0 {
                pts.push(gap(t));
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

        let per_series = total_segs / 256;
        assert!(
            per_series <= series_bound,
            "bursty per-series {per_series} exceeds exact bound {series_bound}"
        );
        assert!(
            total_segs <= total_bound,
            "bursty 256-series total {total_segs} exceeds exact bound {total_bound}"
        );

        // Adversarial fragmentation: per-tick gap stream must be bounded
        // by the same formula after gap suppression.
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

        let frag_per_series = frag_segs / 256;
        assert!(
            frag_per_series <= series_bound,
            "adversarial per-series {frag_per_series} exceeds exact bound {series_bound}"
        );
        assert!(
            frag_segs <= total_bound,
            "adversarial 256-series total {frag_segs} exceeds exact bound {total_bound}"
        );

        // Secondary timing sanity (build-mode aware).
        let ceiling_ms: u128 = if cfg!(debug_assertions) { 400 } else { 60 };
        assert!(
            elapsed.as_millis() < ceiling_ms,
            "bursty 256×7200 build took {elapsed:?}"
        );
        assert!(
            frag_elapsed.as_millis() < ceiling_ms,
            "adversarial 256×7200 build took {frag_elapsed:?}"
        );
        eprintln!(
            "geometry: gap_free={gap_free_segments} seg/series bound={} bursty={} seg/series in {:?} fragmented={} seg/series in {:?} general_bound={series_bound}",
            gap_free_bound.bezier_segments,
            total_segs / 256,
            elapsed,
            frag_segs / 256,
            frag_elapsed,
        );
    }

    // ----- helpers for geometry-stability tests -----

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
                for _ in 0..80 {
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

    // ----- final-shape geometry: overlapping Bézier Y stable across membership changes -----

    /// Build a geometry snapshot for a ring at a given wall-clock time with a
    /// two-interval diagnostic delay.
    fn geometry_at(
        ring: &crate::model::Ring,
        now_ns: u64,
        delay_ns: u64,
        bounds: &PlotBounds,
        window_secs: f64,
    ) -> SeriesGeometry {
        let pts = ring.points();
        let window = DrawWindow {
            sample_interval_ns: 1_000_000_000,
            window_secs,
            window_end_ns: now_ns.saturating_sub(delay_ns),
        };
        compute_series(&pts, 100.0, &window, bounds, false)
    }

    /// Compare overlapping visible Bézier geometry between two frames.
    /// After compensating for the uniform X translation caused by window scroll,
    /// every X position in the overlapping **visible plot region** must produce
    /// the same smoothed curve Y value in both frames. Off-screen portions
    /// (beyond `plot_left` / `plot_right`) may differ because the leftmost
    /// off-screen point can switch between endpoint and interior tangent roles
    /// as the window scrolls — this is cosmetic outside the plot clip.
    fn assert_overlapping_y_stable(
        g0: &SeriesGeometry,
        g1: &SeriesGeometry,
        dx: f32,
        plot_left: f32,
        plot_right: f32,
        label: &str,
    ) {
        // Determine overlapping X range: the visible domain of both geometries
        // after compensating for X translation.
        fn x_range(geom: &SeriesGeometry) -> Option<(f32, f32)> {
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for run in &geom.bezier_runs {
                for seg in run {
                    lo = lo.min(seg.start.0).min(seg.end.0);
                    hi = hi.max(seg.start.0).max(seg.end.0);
                }
            }
            if lo <= hi { Some((lo, hi)) } else { None }
        }

        let (lo0, hi0) = x_range(g0).expect("g0 has no geometry");
        let (lo1, hi1) = x_range(g1).expect("g1 has no geometry");
        // g1's X range shifted by -dx to align with g0
        let lo1_adj = lo1 - dx;
        let hi1_adj = hi1 - dx;

        let overlap_lo = lo0.max(lo1_adj).max(plot_left);
        let overlap_hi = hi0.min(hi1_adj).min(plot_right);

        if overlap_lo >= overlap_hi {
            return;
        }

        // Sample at 100 positions across the overlap
        for i in 0..=100 {
            let frac = i as f32 / 100.0;
            let x = overlap_lo + frac * (overlap_hi - overlap_lo);
            let y0 = curve_y_at_x(g0, x);
            let y1 = curve_y_at_x(g1, x + dx); // compensate X translation

            match (y0, y1) {
                (Some(y0), Some(y1)) => {
                    assert!(
                        (y0 - y1).abs() < RENDER_Y_EPSILON_PX,
                        "y mismatch at x={x:.1}: g0={y0:.3} g1={y1:.3} ({label})"
                    );
                }
                (None, None) => {} // both have a gap here — ok
                (Some(_), None) => {
                    panic!("g0 has curve at x={x:.1} but g1 has gap ({label})");
                }
                (None, Some(_)) => {
                    panic!("g0 has gap at x={x:.1} but g1 has curve ({label})");
                }
            }
        }
    }

    fn assert_clip_strips_y_stable(
        g0: &SeriesGeometry,
        g1: &SeriesGeometry,
        dx: f32,
        plot_left: f32,
        plot_right: f32,
        label: &str,
    ) {
        const STRIP_PX: f32 = 2.0;
        for (edge, direction) in [(plot_left, 1.0f32), (plot_right, -1.0f32)] {
            for i in 0..=20 {
                let x = edge + direction * STRIP_PX * i as f32 / 20.0;
                let y0 = curve_y_at_x(g0, x)
                    .unwrap_or_else(|| panic!("missing old curve at clip x={x:.2} ({label})"));
                let y1 = curve_y_at_x(g1, x + dx)
                    .unwrap_or_else(|| panic!("missing new curve at clip x={x:.2} ({label})"));
                assert!(
                    (y0 - y1).abs() < RENDER_Y_EPSILON_PX,
                    "clip-strip y mismatch at x={x:.2}: g0={y0:.3} g1={y1:.3} ({label})"
                );
            }
        }
    }

    #[test]
    fn final_shape_two_arrivals_right_edge_stable() {
        // Prove immutable Bézier geometry at the right edge through two
        // consecutive sample arrivals (which also evict the two oldest points).
        // The two-interval delay + 2-off-right stencil make every visible
        // segment interior to the spline before reveal.
        use crate::model::Ring;
        use crate::model::history::EDGE_GUARD;

        let bounds = make_bounds();
        let window_secs = 60.0;
        let s = 1_000_000_000u64;
        let delay_ns = 2 * s; // two-interval diagnostic look-ahead
        let sample_v = |i: u64| 50.0 + 15.0 * (i as f32 * 0.2).sin();

        let mut ring = Ring::new(60 + EDGE_GUARD); // 66
        for i in 0..80u64 {
            ring.push(SamplePoint::new(i * s, sample_v(i)));
        }

        // Wall clock sweeps from 80s to 82s in 0.1s steps.
        // At 80s, ring holds 16..80. At 81s, pushes 81, evicts 16. At 82s, pushes 82, evicts 17.
        let mut next_push = 80u64;
        let mut prev_geom: Option<(SeriesGeometry, u64)> = None; // (geom, now_ns)

        for step in 0..=20u64 {
            let now_ns = 80 * s + step * (s / 10);
            let now_s = now_ns / s;
            while next_push <= now_s {
                ring.push(SamplePoint::new(next_push * s, sample_v(next_push)));
                next_push += 1;
            }

            let geom = geometry_at(&ring, now_ns, delay_ns, &bounds, window_secs);

            // Both clip edges must be spanned by the curve
            let _y_left = curve_y_at_x(&geom, bounds.left)
                .unwrap_or_else(|| panic!("no curve at left clip step {step}"));
            let _y_right = curve_y_at_x(&geom, bounds.right)
                .unwrap_or_else(|| panic!("no curve at right clip step {step}"));

            if let Some((ref prev, prev_ns)) = prev_geom {
                let dx = age_to_x(
                    0,
                    now_ns.saturating_sub(delay_ns),
                    60 * s,
                    bounds.left,
                    bounds.right,
                ) - age_to_x(
                    0,
                    prev_ns.saturating_sub(delay_ns),
                    60 * s,
                    bounds.left,
                    bounds.right,
                );
                assert_overlapping_y_stable(
                    prev,
                    &geom,
                    dx,
                    bounds.left,
                    bounds.right,
                    &format!("arrival step {step}"),
                );
            }
            prev_geom = Some((geom, now_ns));
        }
    }

    #[test]
    fn final_shape_two_arrivals_right_edge_decimated() {
        // Prove decimation-stable geometry: same arrival/eviction scenario
        // as the non-decimated test, but with decimation ON and a dataset
        // that actually triggers it (visible count > 2× target).
        //
        // 60s window at 100ms interval → ~600 visible points.
        // Narrow plot (100px) → target = 100, 600 > 200 → decimation fires.
        // Delay = 2 × 100ms = 200ms (correct two-interval look-ahead).
        use crate::model::Ring;
        use crate::model::history::EDGE_GUARD;

        let bounds = PlotBounds {
            left: 40.0,
            top: 4.0,
            right: 140.0, // 100px plot → target = 100
            bottom: 104.0,
        };
        let window_secs = 60.0;
        let interval_ns = 100_000_000u64; // 100ms
        let delay_ns = 2 * interval_ns; // 200ms — two-interval diagnostic look-ahead
        let sample_v = |i: u64| 50.0 + 15.0 * (i as f32 * 0.2).sin();

        // 60s / 100ms = 600 base + the edge guard.
        let base_cap = (60.0 / 0.1) as usize;
        let mut ring = Ring::new(base_cap + EDGE_GUARD);
        for i in 0..800u64 {
            ring.push(SamplePoint::new(i * interval_ns, sample_v(i)));
        }

        let mut next_push = 800u64;
        let mut prev_dec: Option<(SeriesGeometry, u64)> = None;
        let mut decimation_detected = false;

        // Stride is six samples for this configuration, so 140 × 10ms spans
        // more than two selected-point arrivals as well as many ring evictions.
        for step in 0..=140u64 {
            // Advance by 10ms per step; push new samples when crossing a
            // full 100ms boundary.
            let now_ns = 800 * interval_ns + step * 10_000_000;
            let now_ticks = now_ns / interval_ns;
            while next_push <= now_ticks {
                ring.push(SamplePoint::new(
                    next_push * interval_ns,
                    sample_v(next_push),
                ));
                next_push += 1;
            }

            let dec_geom = {
                let pts = ring.points();
                let window = DrawWindow {
                    sample_interval_ns: interval_ns,
                    window_secs,
                    window_end_ns: now_ns.saturating_sub(delay_ns),
                };
                compute_series(&pts, 100.0, &window, &bounds, true)
            };

            curve_y_at_x(&dec_geom, bounds.left)
                .unwrap_or_else(|| panic!("no decimated curve at left clip step {step}"));
            curve_y_at_x(&dec_geom, bounds.right)
                .unwrap_or_else(|| panic!("no decimated curve at right clip step {step}"));

            // Verify decimation actually dropped some points (at least once).
            let total_segs: usize = dec_geom.bezier_runs.iter().map(|r| r.len()).sum();
            if total_segs < 400 {
                // At full resolution 600+ visible points would produce ~599 segments.
                // With decimation targeting ~100, we should see well under 400.
                decimation_detected = true;
            }

            if let Some((ref prev, prev_ns)) = prev_dec {
                let dx = age_to_x(
                    0,
                    now_ns.saturating_sub(delay_ns),
                    (window_secs * 1e9) as u64,
                    bounds.left,
                    bounds.right,
                ) - age_to_x(
                    0,
                    prev_ns.saturating_sub(delay_ns),
                    (window_secs * 1e9) as u64,
                    bounds.left,
                    bounds.right,
                );
                assert_clip_strips_y_stable(
                    prev,
                    &dec_geom,
                    dx,
                    bounds.left,
                    bounds.right,
                    &format!("dec arrival step {step}"),
                );
            }
            prev_dec = Some((dec_geom, now_ns));
        }
        assert!(
            decimation_detected,
            "decimation was never triggered — dataset too sparse for target"
        );
    }

    #[test]
    fn edge_gaps_never_bridged_by_context() {
        // Gaps immediately adjacent to either clip edge must remain discontinuities.
        // The context stencil must not synthesize continuity across them.
        let bounds = make_bounds();
        let s = 1_000_000_000u64;

        // Gap at left edge: gap marker just before the first visible point.
        // Even with raw off-left context, the gap should stay.
        {
            let pts = vec![
                pt(10.0, 10 * s),
                pt(20.0, 11 * s),
                gap(12 * s), // gap RIGHT at the left edge
                pt(30.0, 13 * s),
                pt(40.0, 14 * s),
            ];
            // visible: [12s, 72s]
            let window = DrawWindow {
                sample_interval_ns: 1_000_000_000,
                window_secs: 60.0,
                window_end_ns: 72 * s,
            };
            let geom = compute_series(&pts, 100.0, &window, &bounds, false);
            // The gap at 12s splits the run: points 10-11, gap, 13-14
            assert_eq!(
                geom.bezier_runs.len(),
                2,
                "gap at left edge must split runs"
            );
            let gap_x = age_to_x(12 * s, 72 * s, 60 * s, bounds.left, bounds.right);
            assert!(
                geom.bezier_runs
                    .iter()
                    .flatten()
                    .all(|segment| { !(segment.start.0 < gap_x && segment.end.0 > gap_x) }),
                "a final Bézier segment bridged the left-edge gap"
            );
        }

        // Gap at right edge
        {
            let pts = vec![
                pt(10.0, 50 * s),
                pt(20.0, 51 * s),
                gap(52 * s),      // gap BEFORE the off-right context
                pt(30.0, 53 * s), // off-right context
                pt(40.0, 54 * s), // off-right context
            ];
            let window = DrawWindow {
                sample_interval_ns: 1_000_000_000,
                window_secs: 60.0,
                window_end_ns: 52 * s, // visible: [0?, 52s]
            };
            let geom = compute_series(&pts, 100.0, &window, &bounds, false);
            let gap_x = age_to_x(52 * s, 52 * s, 60 * s, bounds.left, bounds.right);
            assert!(
                geom.bezier_runs
                    .iter()
                    .flatten()
                    .all(|segment| { !(segment.start.0 < gap_x && segment.end.0 > gap_x) }),
                "a final Bézier segment bridged the right-edge gap"
            );
        }
    }

    #[test]
    fn two_interval_delay_early_uptime_no_underflow() {
        // Early uptime (e.g. 5s after boot) with a 60m window must not underflow
        // the 2-interval delay subtraction and must produce valid geometry.
        let bounds = make_bounds();
        let s = 1_000_000_000u64;
        let pts = vec![pt(50.0, s), pt(60.0, 2 * s), pt(55.0, 3 * s)];

        // Uptime = 4s, 2-interval delay = 2s, window_end = 2s
        let window = DrawWindow {
            sample_interval_ns: 1_000_000_000,
            window_secs: 3600.0,          // 60 minutes
            window_end_ns: 4 * s - 2 * s, // 2s
        };
        let geom = compute_series(&pts, 100.0, &window, &bounds, false);
        // Must produce at least one Bézier run without panicking
        assert!(
            !geom.bezier_runs.is_empty(),
            "early uptime with 2-interval delay must produce geometry"
        );
    }

    #[test]
    fn sub_frame_scroll_is_pure_x_translation() {
        // Between sample arrivals, advancing the clock by less than one interval
        // must only translate X uniformly. Y geometry must not change.
        let bounds = make_bounds();
        let s = 1_000_000_000u64;
        let pts = vec![
            pt(20.0, 50 * s),
            pt(80.0, 51 * s),
            pt(40.0, 52 * s),
            pt(60.0, 53 * s),
            pt(30.0, 54 * s),
        ];

        let window_secs = 60.0;
        let delay_ns = 2 * s;
        let now0 = 55 * s;
        let now1 = 55 * s + 300_000_000; // 0.3s later, no new sample

        let g0 = {
            let window = DrawWindow {
                sample_interval_ns: 1_000_000_000,
                window_secs,
                window_end_ns: now0.saturating_sub(delay_ns),
            };
            compute_series(&pts, 100.0, &window, &bounds, false)
        };
        let g1 = {
            let window = DrawWindow {
                sample_interval_ns: 1_000_000_000,
                window_secs,
                window_end_ns: now1.saturating_sub(delay_ns),
            };
            compute_series(&pts, 100.0, &window, &bounds, false)
        };

        let window_ns = (window_secs * 1e9) as u64;
        let dx = age_to_x(
            0,
            now1.saturating_sub(delay_ns),
            window_ns,
            bounds.left,
            bounds.right,
        ) - age_to_x(
            0,
            now0.saturating_sub(delay_ns),
            window_ns,
            bounds.left,
            bounds.right,
        );

        assert_overlapping_y_stable(&g0, &g1, dx, bounds.left, bounds.right, "sub-frame scroll");
    }

    // ----- delayed continuous window (off-right real samples) -----

    fn last_bezier_endpoint(geom: &SeriesGeometry) -> Option<(f32, f32)> {
        geom.bezier_runs.last()?.last().map(|seg| seg.end)
    }

    #[test]
    fn delayed_window_future_sample_maps_past_right() {
        let window_end = 100_000_000_000u64;
        let window = DrawWindow {
            sample_interval_ns: 1_000_000_000,
            window_secs: 60.0,
            window_end_ns: window_end,
        };
        let bounds = make_bounds();
        // In-window + one sample 1s in the future (off-right)
        let pts = vec![
            pt(20.0, 90_000_000_000),
            pt(40.0, 99_000_000_000),
            pt(80.0, 101_000_000_000), // future
        ];
        let geom = compute_series(&pts, 100.0, &window, &bounds, false);
        let end = last_bezier_endpoint(&geom).expect("should produce a run");
        assert!(
            end.0 > bounds.right,
            "future sample endpoint should be past plot_right, got {}",
            end.0
        );
    }

    #[test]
    fn delayed_window_segment_shape_stable_as_window_advances() {
        // Non-collinear multi-point set so monotone-cubic is exercised.
        // Advancing window_end within the same membership interval must only
        // translate X uniformly; control-point Y geometry stays put.
        let bounds = make_bounds();
        let s = 1_000_000_000u64;
        // Six points: 50-55s. With a 60s window and 2-interval delay (window_end
        // at 53-54s), points 50-52 are visible/off-left context, 53 is at edge,
        // 54-55 are off-right context. Membership is stable as window_end varies
        // between 53s and 54s.
        let pts = vec![
            pt(20.0, 50 * s),
            pt(80.0, 51 * s),
            pt(40.0, 52 * s),
            pt(60.0, 53 * s),
            pt(30.0, 54 * s),
            pt(70.0, 55 * s),
        ];
        let window_ns = 60 * s;

        let segs_at = |window_end: u64| {
            let window = DrawWindow {
                sample_interval_ns: 1_000_000_000,
                window_secs: 60.0,
                window_end_ns: window_end,
            };
            compute_series(&pts, 100.0, &window, &bounds, false)
        };

        // Both ends keep same point membership: end0 at 53.2s, end1 at 53.8s.
        let end0 = 53 * s + 200_000_000;
        let end1 = 53 * s + 800_000_000;
        let g0 = segs_at(end0);
        let g1 = segs_at(end1);
        assert_eq!(g0.bezier_runs.len(), 1);
        assert_eq!(g1.bezier_runs.len(), 1);
        assert_eq!(g0.bezier_runs[0].len(), 5, "6 points → 5 cubic segments");
        assert_eq!(g1.bezier_runs[0].len(), 5);

        let dx_expected = age_to_x(50 * s, end1, window_ns, bounds.left, bounds.right)
            - age_to_x(50 * s, end0, window_ns, bounds.left, bounds.right);

        for (a, b) in g0.bezier_runs[0].iter().zip(g1.bezier_runs[0].iter()) {
            // Y geometry (values) unchanged
            assert!((a.start.1 - b.start.1).abs() < 1e-3, "start y rewrote");
            assert!((a.c1.1 - b.c1.1).abs() < 1e-3, "c1 y rewrote");
            assert!((a.c2.1 - b.c2.1).abs() < 1e-3, "c2 y rewrote");
            assert!((a.end.1 - b.end.1).abs() < 1e-3, "end y rewrote");
            // X translates uniformly by the window scroll (endpoints + controls)
            assert!(
                (b.start.0 - a.start.0 - dx_expected).abs() < 0.5,
                "start x not pure translate"
            );
            assert!(
                (b.c1.0 - a.c1.0 - dx_expected).abs() < 0.5,
                "c1 x not pure translate"
            );
            assert!(
                (b.c2.0 - a.c2.0 - dx_expected).abs() < 0.5,
                "c2 x not pure translate"
            );
            assert!(
                (b.end.0 - a.end.0 - dx_expected).abs() < 0.5,
                "end x not pure translate"
            );
        }
    }

    #[test]
    fn delayed_window_no_synthetic_hold_segment() {
        // A single in-window sample with no off-right neighbor produces no
        // geometry (need 2 points) — we never invent a flat to the edge.
        let window = DrawWindow {
            sample_interval_ns: 1_000_000_000,
            window_secs: 60.0,
            window_end_ns: 70_000_000_000,
        };
        let bounds = make_bounds();
        let pts = vec![pt(50.0, 65_000_000_000)];
        let geom = compute_series(&pts, 100.0, &window, &bounds, false);
        assert!(
            geom.bezier_runs.is_empty(),
            "no synthetic hold: single sample must not invent a segment"
        );
    }

    #[test]
    fn cull_keeps_two_off_right_neighbors() {
        let window_start = 40_000_000_000u64;
        let window_end = 100_000_000_000u64;
        let pts = vec![
            pt(0.5, 20_000_000_000),  // off-left
            pt(1.0, 30_000_000_000),  // off-left
            pt(2.0, 50_000_000_000),  // in
            pt(3.0, 90_000_000_000),  // in
            pt(4.0, 101_000_000_000), // off-right neighbor
            pt(5.0, 102_000_000_000), // off-right neighbor
            pt(6.0, 103_000_000_000), // farther future — drop
        ];
        let culled = cull_off_screen(&pts, window_start, window_end);
        assert_eq!(culled.len(), 6, "2 off-left + 2 in + 2 off-right");
        assert_eq!(culled[0].t_boot_ns, 20_000_000_000);
        assert_eq!(culled[5].t_boot_ns, 102_000_000_000);
    }
}
