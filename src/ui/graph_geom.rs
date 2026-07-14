//! Pure graph geometry: age-based X mapping, gap splitting, grid lines.
//! No iced dependencies. Returns raw (x, y) segments and grid Y positions.

use crate::model::SamplePoint;

/// Parameters for the drawing window (time domain).
#[derive(Clone, Debug)]
pub struct DrawWindow {
    pub window_secs: f64,
    pub window_end_ns: u64,
}

/// Pixel-space bounds of the plot area (excluding margins).
#[derive(Clone, Copy, Debug)]
pub struct PlotBounds {
    pub width: f32,
    pub height: f32,
    pub margin: f32,
}

/// A set of gap-separated polyline segments.
pub struct SeriesGeometry {
    pub segments: Vec<Vec<(f32, f32)>>,
}

/// Map a boottime timestamp to pixel X using age-based position.
fn age_to_x(
    t_boot_ns: u64,
    window_end_ns: u64,
    window_ns: u64,
    plot_width: f32,
    margin: f32,
) -> Option<f32> {
    let age_ns = window_end_ns.saturating_sub(t_boot_ns);
    if age_ns > window_ns {
        return None;
    }
    let right = margin + plot_width;
    let fraction = (age_ns as f64 / window_ns.max(1) as f64).min(1.0) as f32;
    let x = right - fraction * plot_width;
    Some(x)
}

/// Map a value and max_value to pixel Y.
fn value_to_y(value: f32, max_value: f32, plot_height: f32, margin: f32) -> f32 {
    let clamped = value.min(max_value).max(0.0);
    let fraction = clamped / max_value.max(1.0);
    margin + plot_height - fraction * plot_height
}

/// Compute gap-separated polyline segments for a single series.
pub fn compute_series(
    points: &[SamplePoint],
    max_value: f32,
    window: &DrawWindow,
    bounds: &PlotBounds,
    decimate: bool,
) -> SeriesGeometry {
    let window_ns = (window.window_secs * 1e9) as u64;
    let plot_width = bounds.width - 2.0 * bounds.margin;
    let plot_height = bounds.height - 2.0 * bounds.margin;

    if plot_width <= 0.0 || plot_height <= 0.0 {
        return SeriesGeometry {
            segments: Vec::new(),
        };
    }

    let mut coords: Vec<Option<(f32, f32)>> = points
        .iter()
        .map(|pt| {
            let x = age_to_x(
                pt.t_boot_ns,
                window.window_end_ns,
                window_ns,
                plot_width,
                bounds.margin,
            )?;
            let y = pt
                .value
                .map(|v| value_to_y(v, max_value, plot_height, bounds.margin))?;
            Some((x, y))
        })
        .collect();

    if decimate {
        let in_window_count = coords.iter().filter(|c| c.is_some()).count();
        let target = plot_width as usize;
        if in_window_count > target * 2 {
            let stride = ((in_window_count as f64) / (target as f64)).ceil() as usize;
            let stride = stride.max(1);
            let mut decimated = Vec::with_capacity(coords.len() / stride);
            for (i, chunk) in coords.chunks(stride).enumerate() {
                if i == 0 {
                    decimated.extend_from_slice(chunk);
                } else if let Some(first) = chunk.first() {
                    decimated.push(*first);
                }
            }
            coords = decimated;
        }
    }

    let mut segments: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut current: Vec<(f32, f32)> = Vec::new();

    for coord in coords {
        match coord {
            Some((x, y)) => current.push((x, y)),
            None => {
                if current.len() >= 2 {
                    segments.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        }
    }
    if current.len() >= 2 {
        segments.push(current);
    }

    SeriesGeometry { segments }
}

/// Compute Y positions for horizontal grid lines at 25 percent, 50 percent, 75 percent.
pub fn compute_grid_y(bounds: &PlotBounds) -> Vec<f32> {
    let plot_height = bounds.height - 2.0 * bounds.margin;
    let base = bounds.margin + plot_height;
    let mut lines = Vec::with_capacity(3);
    for pct in [0.25_f32, 0.50, 0.75] {
        let y = base - pct * plot_height;
        lines.push(y);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_window(end_ns: u64) -> DrawWindow {
        DrawWindow {
            window_secs: 60.0,
            window_end_ns: end_ns,
        }
    }

    fn make_bounds() -> PlotBounds {
        PlotBounds {
            width: 500.0,
            height: 100.0,
            margin: 2.0,
        }
    }

    #[test]
    fn age_to_x_latest_at_right_edge() {
        let x = age_to_x(100_000_000_000, 100_000_000_000, 60_000_000_000, 496.0, 2.0).unwrap();
        assert!((x - 498.0).abs() < 0.5);
    }

    #[test]
    fn age_to_x_old_at_left_edge() {
        let window_ns: u64 = 60_000_000_000;
        let x = age_to_x(40_000_000_000, 100_000_000_000, window_ns, 496.0, 2.0).unwrap();
        assert!((x - 2.0).abs() < 0.5);
    }

    #[test]
    fn age_to_x_older_than_window_excluded() {
        let window_ns: u64 = 60_000_000_000;
        assert!(age_to_x(30_000_000_000, 100_000_000_000, window_ns, 496.0, 2.0).is_none());
    }

    #[test]
    fn age_to_x_future_clamped_to_right() {
        let window_ns: u64 = 60_000_000_000;
        let x = age_to_x(110_000_000_000, 100_000_000_000, window_ns, 496.0, 2.0).unwrap();
        assert!((x - 498.0).abs() < 0.5);
    }

    #[test]
    fn early_uptime_places_at_right() {
        let window_ns: u64 = 60_000_000_000;
        let x = age_to_x(5_000_000_000, 5_000_000_000, window_ns, 496.0, 2.0).unwrap();
        assert!((x - 498.0).abs() < 0.5);
    }

    #[test]
    fn scroll_without_new_samples_shifts_x() {
        let window_ns: u64 = 60_000_000_000;
        let x1 = age_to_x(50_000_000_000, 100_000_000_000, window_ns, 496.0, 2.0).unwrap();
        let x2 = age_to_x(50_000_000_000, 101_000_000_000, window_ns, 496.0, 2.0).unwrap();
        assert!(x2 < x1);
    }

    #[test]
    fn compute_series_gaps_split_segments() {
        let points = vec![
            SamplePoint::new(0, 10.0),
            SamplePoint::new(1, 20.0),
            SamplePoint::gap(2),
            SamplePoint::new(3, 30.0),
            SamplePoint::new(4, 40.0),
        ];
        let window = make_window(4);
        let bounds = make_bounds();
        let geom = compute_series(&points, 100.0, &window, &bounds, false);
        assert_eq!(geom.segments.len(), 2);
    }

    #[test]
    fn grid_lines_pct() {
        let bounds = make_bounds();
        let ys = compute_grid_y(&bounds);
        assert_eq!(ys.len(), 3);
        assert!((ys[0] - 74.0).abs() < 0.5);
        assert!((ys[1] - 50.0).abs() < 0.5);
        assert!((ys[2] - 26.0).abs() < 0.5);
    }
}
