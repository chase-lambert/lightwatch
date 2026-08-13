//! Multi-series charts for the lightwatch dashboard.
//! Thin iced adapter over pure geometry from graph_geom.
//!
//! Each chart is a framed plot: Y-axis labels (left gutter), X time ticks
//! (bottom gutter), grid lines inside the plot area, series clipped to the
//! plot rect, and a frame border drawn last.

use super::graph_geom::{self, AxisKind, DrawWindow, PlotBounds};
use super::theme;
use crate::model::SamplePoint;
use iced::mouse;
use iced::widget::canvas::{self, Cache, Event, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Point, Rectangle, Size, Vector};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

/// Pixel gutters around the inner plot rect.
///
/// The left gutter reserves 80 pixels after the two-pixel label origin. At the
/// 10-pixel label size, this fits the current widest tick (`16.0 EiB/s`) with
/// space before the plot. One shared gutter keeps every chart frame aligned.
const GUTTER_LEFT: f32 = 82.0;
const GUTTER_TOP: f32 = 6.0;
const GUTTER_RIGHT: f32 = 6.0;
const GUTTER_BOTTOM: f32 = 22.0;

/// Stable identity for canvas state that may be reattached after disclosure or
/// GPU-list changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChartId {
    Cpu,
    Memory,
    Gpu(Arc<str>),
    ProcessCpu,
    ProcessMemory,
    ProcessIo,
}

/// A generation-scoped series descriptor. Points are linearized once when the
/// publication reaches the UI, then shared by every presentation frame.
#[derive(Clone)]
pub struct SeriesData {
    pub points: Arc<[SamplePoint]>,
    pub color: Color,
    pub max_value: f32,
    /// Enable light fill under the line (only for single-run, single-series charts).
    pub fill: bool,
    /// Per-line alpha (0..1); applied to the stroke color. Legend colour stays
    /// full-opacity.
    pub line_alpha: Option<f32>,
}

/// Multi-series chart canvas. Stable curve paths are rebuilt on cache-key
/// changes; compositor frames only translate and tessellate those paths.
pub struct MultiChart<'a> {
    pub id: ChartId,
    pub generation: u64,
    /// Generation-scoped fingerprint of scale, style, and series membership.
    /// Point content itself is covered by `generation`.
    pub content_signature: u64,
    pub series: &'a [SeriesData],
    pub axis: AxisKind,
    pub window: DrawWindow,
    /// Use gap-aware decimation when the series in-window count far exceeds
    /// the pixel budget.
    pub decimate: bool,
    /// Explicit redraw gate supplied by the Resources-tab visibility policy.
    pub animate: bool,
}

impl<'a> MultiChart<'a> {
    pub fn new(
        id: ChartId,
        generation: u64,
        content_signature: u64,
        series: &'a [SeriesData],
        axis: AxisKind,
        decimate: bool,
    ) -> Self {
        Self {
            id,
            generation,
            content_signature,
            series,
            axis,
            window: DrawWindow {
                sample_interval_ns: 1_000_000_000,
                window_secs: 60.0,
                window_end_ns: 0,
            },
            decimate,
            animate: true,
        }
    }

    fn series_cache_key(&self, bounds: Size) -> SeriesCacheKey {
        SeriesCacheKey {
            id: self.id.clone(),
            generation: self.generation,
            content_signature: self.content_signature,
            width_bits: bounds.width.to_bits(),
            height_bits: bounds.height.to_bits(),
            window_secs_bits: self.window.window_secs.to_bits(),
            sample_interval_ns: self.window.sample_interval_ns,
            decimate: self.decimate,
            series_count: self.series.len(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SeriesCacheKey {
    id: ChartId,
    generation: u64,
    content_signature: u64,
    width_bits: u32,
    height_bits: u32,
    window_secs_bits: u64,
    sample_interval_ns: u64,
    decimate: bool,
    series_count: usize,
}

struct CachedDrawSeries {
    stroke_paths: Vec<Path>,
    fill_path: Option<Path>,
    color: Color,
    line_alpha: Option<f32>,
}

struct CachedSeries {
    key: SeriesCacheKey,
    anchor_window_end_ns: u64,
    series: Vec<CachedDrawSeries>,
}

pub struct ChartState {
    background: Cache,
    chrome: Cache,
    static_window_secs_bits: Cell<Option<u64>>,
    static_axis_key: Cell<Option<u64>>,
    series: RefCell<Option<CachedSeries>>,
    presentation_window_end_ns: Cell<u64>,
}

impl Default for ChartState {
    fn default() -> Self {
        Self {
            background: Cache::new(),
            chrome: Cache::new(),
            static_window_secs_bits: Cell::new(None),
            static_axis_key: Cell::new(None),
            series: RefCell::new(None),
            presentation_window_end_ns: Cell::new(0),
        }
    }
}

impl<Message> canvas::Program<Message> for MultiChart<'_> {
    type State = ChartState;

    fn draw(
        &self,
        state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<iced::Renderer>> {
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return Vec::new();
        }

        let window_bits = self.window.window_secs.to_bits();
        if state.static_window_secs_bits.get() != Some(window_bits) {
            state.background.clear();
            state.chrome.clear();
            state.static_window_secs_bits.set(Some(window_bits));
        }
        let axis_key = axis_key(self.axis);
        if state.static_axis_key.get() != Some(axis_key) {
            state.chrome.clear();
            state.static_axis_key.set(Some(axis_key));
        }

        let plot_bounds = plot_bounds(bounds.size());
        if plot_bounds.right <= plot_bounds.left || plot_bounds.bottom <= plot_bounds.top {
            return Vec::new();
        }

        let app_window_end = self.window.window_end_ns;
        let presentation_window_end =
            advance_presentation(state.presentation_window_end_ns.get(), app_window_end, 0);
        state
            .presentation_window_end_ns
            .set(presentation_window_end);

        let background = state.background.draw(renderer, bounds.size(), |frame| {
            draw_background(frame, self.window.window_secs, bounds.size());
        });

        let key = self.series_cache_key(bounds.size());
        let rebuild = state
            .series
            .borrow()
            .as_ref()
            .is_none_or(|cached| cached.key != key);
        if rebuild {
            *state.series.borrow_mut() = Some(build_cached_series(
                self,
                key,
                presentation_window_end,
                &plot_bounds,
            ));
        }

        let mut series_frame = Frame::new(renderer, bounds.size());
        if let Some(cached) = state.series.borrow().as_ref() {
            draw_moving_series(
                &mut series_frame,
                cached,
                presentation_window_end,
                self.window.window_secs,
                &plot_bounds,
            );
        }

        let chrome = state.chrome.draw(renderer, bounds.size(), |frame| {
            draw_chrome(frame, self.window.window_secs, self.axis, bounds.size());
        });

        vec![background, series_frame.into_geometry(), chrome]
    }

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let Event::Window(iced::window::Event::RedrawRequested(at)) = event else {
            return None;
        };
        if !should_request_next_frame(self.animate, self.series.len()) {
            return None;
        }

        let chart_now_ns = frame_boottime_ns(*at, Instant::now(), crate::clock_boottime_ns());
        let delay_ns = self.window.sample_interval_ns.saturating_mul(2);
        let window_end_ns = chart_now_ns.saturating_sub(delay_ns);
        state.presentation_window_end_ns.set(advance_presentation(
            state.presentation_window_end_ns.get(),
            self.window.window_end_ns,
            window_end_ns,
        ));
        Some(canvas::Action::request_redraw())
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        mouse::Interaction::default()
    }
}

// ---------------------------------------------------------------------------
// Helpers: (x, y) → Point
// ---------------------------------------------------------------------------

fn pt(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn plot_bounds(size: Size) -> PlotBounds {
    PlotBounds {
        left: GUTTER_LEFT,
        top: GUTTER_TOP,
        right: size.width - GUTTER_RIGHT,
        bottom: size.height - GUTTER_BOTTOM,
    }
}

fn plot_rect(bounds: &PlotBounds) -> Rectangle {
    Rectangle {
        x: bounds.left,
        y: bounds.top,
        width: bounds.right - bounds.left,
        height: bounds.bottom - bounds.top,
    }
}

fn draw_background(frame: &mut Frame, window_secs: f64, size: Size) {
    let bounds = plot_bounds(size);
    if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
        return;
    }
    let plot_rect = plot_rect(&bounds);
    let x_ticks = graph_geom::compute_time_ticks(window_secs, bounds.left, bounds.right, 0);

    // This is a separate geometry layer returned before the clipped series.
    // Keeping the fill inside its own clip also preserves iced's paste order.
    frame.with_clip(plot_rect, |clipped| {
        clipped.fill(
            &Path::rectangle(plot_rect.position(), plot_rect.size()),
            theme::PLOT_BG,
        );

        // Grid (horizontal at 25/50/75%, vertical at interior time ticks)
        let grid_ys = graph_geom::compute_grid_y(&bounds);
        let grid_stroke = Stroke::default().with_color(theme::GRID).with_width(0.5);
        for y in grid_ys {
            let path = Path::line(pt(bounds.left, y), pt(bounds.right, y));
            clipped.stroke(&path, grid_stroke);
        }
        for (x, _) in &x_ticks {
            if (*x - bounds.left).abs() < 1.0 || (*x - bounds.right).abs() < 1.0 {
                continue;
            }
            let path = Path::line(pt(*x, bounds.top), pt(*x, bounds.bottom));
            clipped.stroke(&path, grid_stroke);
        }
    });
}

fn draw_chrome(frame: &mut Frame, window_secs: f64, axis: AxisKind, size: Size) {
    let bounds = plot_bounds(size);
    if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
        return;
    }
    let plot_rect = plot_rect(&bounds);
    let label_size = 10.0;
    let x_ticks = graph_geom::compute_time_ticks(window_secs, bounds.left, bounds.right, 0);

    for (y_pos, label) in graph_geom::compute_y_ticks(&bounds, axis) {
        frame.fill_text(Text {
            content: label,
            position: pt(2.0, y_pos - label_size * 0.5),
            color: theme::AXIS_LABEL,
            size: label_size.into(),
            ..Text::default()
        });
    }
    for (x_pos, label) in x_ticks {
        let align_x = if (x_pos - bounds.right).abs() < 1.0 {
            iced::alignment::Horizontal::Right
        } else if (x_pos - bounds.left).abs() < 1.0 {
            iced::alignment::Horizontal::Left
        } else {
            iced::alignment::Horizontal::Center
        };
        frame.fill_text(Text {
            content: label,
            position: pt(x_pos, bounds.bottom + 2.0),
            color: theme::AXIS_LABEL,
            size: label_size.into(),
            align_x: align_x.into(),
            ..Text::default()
        });
    }

    let frame_path = Path::rectangle(plot_rect.position(), plot_rect.size());
    frame.stroke(
        &frame_path,
        Stroke::default()
            .with_color(theme::PLOT_FRAME)
            .with_width(1.0),
    );
}

fn axis_key(axis: AxisKind) -> u64 {
    match axis {
        AxisKind::Percent => 0,
        AxisKind::Bytes { max_bytes } => (1_u64 << 32) | u64::from(max_bytes.to_bits()),
        AxisKind::BytesPerSecond {
            max_bytes_per_second,
        } => (2_u64 << 32) | u64::from(max_bytes_per_second.to_bits()),
    }
}

fn build_cached_series(
    chart: &MultiChart<'_>,
    key: SeriesCacheKey,
    anchor_window_end_ns: u64,
    bounds: &PlotBounds,
) -> CachedSeries {
    let anchor_window = DrawWindow {
        window_end_ns: anchor_window_end_ns,
        ..chart.window.clone()
    };
    let mut cached = Vec::with_capacity(chart.series.len());

    for series in chart.series {
        let geom = graph_geom::compute_series(
            &series.points,
            series.max_value,
            &anchor_window,
            bounds,
            chart.decimate,
        );
        let fill_path = if series.fill
            && geom.bezier_runs.len() == 1
            && let Some(run) = geom.bezier_runs.first()
            && !run.is_empty()
        {
            let first = &run[0];
            let last = &run[run.len() - 1];
            Some(Path::new(|builder| {
                builder.move_to(pt(first.start.0, first.start.1));
                for seg in run {
                    builder.bezier_curve_to(
                        pt(seg.c1.0, seg.c1.1),
                        pt(seg.c2.0, seg.c2.1),
                        pt(seg.end.0, seg.end.1),
                    );
                }
                builder.line_to(pt(last.end.0, bounds.bottom));
                builder.line_to(pt(first.start.0, bounds.bottom));
                builder.close();
            }))
        } else {
            None
        };
        let stroke_paths = geom
            .bezier_runs
            .iter()
            .filter(|run| !run.is_empty())
            .map(|run| {
                Path::new(|builder| {
                    builder.move_to(pt(run[0].start.0, run[0].start.1));
                    for seg in run {
                        builder.bezier_curve_to(
                            pt(seg.c1.0, seg.c1.1),
                            pt(seg.c2.0, seg.c2.1),
                            pt(seg.end.0, seg.end.1),
                        );
                    }
                })
            })
            .collect();

        cached.push(CachedDrawSeries {
            stroke_paths,
            fill_path,
            color: series.color,
            line_alpha: series.line_alpha,
        });
    }

    CachedSeries {
        key,
        anchor_window_end_ns,
        series: cached,
    }
}

fn draw_moving_series(
    frame: &mut Frame,
    cached: &CachedSeries,
    current_window_end_ns: u64,
    window_secs: f64,
    bounds: &PlotBounds,
) {
    let plot_rect = plot_rect(bounds);
    let dx = graph_geom::window_translation_x(
        cached.anchor_window_end_ns,
        current_window_end_ns,
        window_secs,
        bounds.right - bounds.left,
    );
    frame.with_clip(plot_rect, |clipped| {
        clipped.translate(Vector::new(dx, 0.0));
        for series in &cached.series {
            if let Some(fill_path) = &series.fill_path {
                clipped.fill(
                    fill_path,
                    Color {
                        a: 0.1,
                        ..series.color
                    },
                );
            }
            let stroke = Stroke::default()
                .with_color(theme::with_alpha(
                    series.color,
                    series.line_alpha.unwrap_or(1.0),
                ))
                .with_width(1.2);
            for path in &series.stroke_paths {
                clipped.stroke(path, stroke);
            }
        }
    });
}

fn frame_boottime_ns(frame_at: Instant, now: Instant, boot_now_ns: u64) -> u64 {
    if now >= frame_at {
        boot_now_ns.saturating_sub(duration_ns_u64(now.duration_since(frame_at)))
    } else {
        boot_now_ns.saturating_add(duration_ns_u64(frame_at.duration_since(now)))
    }
}

fn advance_presentation(previous: u64, application_anchor: u64, frame_anchor: u64) -> u64 {
    previous.max(application_anchor).max(frame_anchor)
}

fn should_request_next_frame(animate: bool, series_count: usize) -> bool {
    animate && series_count > 0
}

fn duration_ns_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const BOOT_BASE_NS: u64 = 1_000_000_000_000;

    #[test]
    fn frame_clock_tracks_variable_delivery_time() {
        let now = Instant::now();
        assert_eq!(frame_boottime_ns(now, now, BOOT_BASE_NS), BOOT_BASE_NS);
        assert_eq!(
            frame_boottime_ns(now - Duration::from_millis(7), now, BOOT_BASE_NS),
            BOOT_BASE_NS - 7_000_000,
        );
        assert_eq!(
            frame_boottime_ns(now + Duration::from_millis(3), now, BOOT_BASE_NS),
            BOOT_BASE_NS + 3_000_000,
        );
    }

    #[test]
    fn translation_uses_absolute_anchor_after_many_frames() {
        let anchor = 100_000_000_000;
        let end = anchor + 987_654_321;
        let once = graph_geom::window_translation_x(anchor, end, 60.0, 456.0);
        for _ in 0..10_000 {
            assert_eq!(
                graph_geom::window_translation_x(anchor, end, 60.0, 456.0),
                once
            );
        }
    }

    fn sample_series(points: Arc<[SamplePoint]>) -> Vec<SeriesData> {
        vec![SeriesData {
            points,
            color: Color::from_rgb(0.2, 0.4, 0.6),
            max_value: 100.0,
            fill: false,
            line_alpha: Some(0.8),
        }]
    }

    #[test]
    fn presentation_never_reverses_and_preserves_large_forward_jumps() {
        assert_eq!(advance_presentation(300, 200, 250), 300);
        assert_eq!(advance_presentation(300, 450, 400), 450);
        assert_eq!(advance_presentation(450, 451, 10_000), 10_000);
    }

    #[test]
    fn redraw_chain_requires_both_visibility_permission_and_series() {
        assert!(!should_request_next_frame(false, 1));
        assert!(!should_request_next_frame(true, 0));
        assert!(should_request_next_frame(true, 1));
    }

    #[test]
    fn cache_key_retains_warm_frames_and_invalidates_every_geometry_input() {
        let points: Arc<[SamplePoint]> = Arc::from([
            SamplePoint::new(1_000_000_000, 10.0),
            SamplePoint::new(2_000_000_000, 20.0),
        ]);
        let series = sample_series(points);
        let size = Size::new(640.0, 240.0);
        let chart = MultiChart::new(ChartId::Cpu, 7, 11, &series, AxisKind::Percent, true);
        let warm = chart.series_cache_key(size);

        assert_eq!(warm, chart.series_cache_key(size));

        let mut changed = MultiChart::new(ChartId::Cpu, 8, 11, &series, AxisKind::Percent, true);
        assert_ne!(warm, changed.series_cache_key(size));
        changed.generation = 7;
        changed.content_signature = 12;
        assert_ne!(warm, changed.series_cache_key(size));
        changed.content_signature = 11;
        changed.window.window_secs = 300.0;
        assert_ne!(warm, changed.series_cache_key(size));
        changed.window.window_secs = 60.0;
        changed.window.sample_interval_ns = 500_000_000;
        assert_ne!(warm, changed.series_cache_key(size));
        changed.window.sample_interval_ns = 1_000_000_000;
        changed.decimate = false;
        assert_ne!(warm, changed.series_cache_key(size));
        changed.decimate = true;
        assert_ne!(warm, changed.series_cache_key(Size::new(641.0, 240.0)));
    }

    #[test]
    fn repeated_views_borrow_the_same_generation_owned_points() {
        let points: Arc<[SamplePoint]> = Arc::from([
            SamplePoint::new(1_000_000_000, 10.0),
            SamplePoint::new(2_000_000_000, 20.0),
        ]);
        let storage = Arc::as_ptr(&points);
        let series = sample_series(points);

        let first = MultiChart::new(ChartId::Memory, 3, 9, &series, AxisKind::Percent, false);
        let second = MultiChart::new(ChartId::Memory, 3, 9, &series, AxisKind::Percent, false);

        assert_eq!(Arc::as_ptr(&first.series[0].points), storage);
        assert_eq!(Arc::as_ptr(&second.series[0].points), storage);
        assert_eq!(
            first.series_cache_key(Size::new(400.0, 180.0)),
            second.series_cache_key(Size::new(400.0, 180.0)),
        );
    }

    #[test]
    fn axis_key_changes_with_kind_and_scale() {
        assert_ne!(
            axis_key(AxisKind::Percent),
            axis_key(AxisKind::Bytes { max_bytes: 100.0 })
        );
        assert_ne!(
            axis_key(AxisKind::Bytes { max_bytes: 100.0 }),
            axis_key(AxisKind::Bytes { max_bytes: 200.0 })
        );
        assert_ne!(
            axis_key(AxisKind::Bytes { max_bytes: 100.0 }),
            axis_key(AxisKind::BytesPerSecond {
                max_bytes_per_second: 100.0,
            })
        );
    }
}
