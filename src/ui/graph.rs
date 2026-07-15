//! Multi-series charts for the lightwatch dashboard.
//! Thin iced adapter over pure geometry from graph_geom.
//!
//! Each chart is a framed plot: Y-axis labels (left gutter), X time ticks
//! (bottom gutter), grid lines inside the plot area, series clipped to the
//! plot rect, and a frame border drawn last.

use super::graph_geom::{self, DrawWindow, PlotBounds};
use super::theme;
use crate::model::SamplePoint;
use iced::mouse;
use iced::widget::canvas::{self, Event, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Point, Rectangle, Size};

/// Pixel gutters around the inner plot rect.
const GUTTER_LEFT: f32 = 42.0;
const GUTTER_TOP: f32 = 6.0;
const GUTTER_RIGHT: f32 = 6.0;
const GUTTER_BOTTOM: f32 = 22.0;

/// A single series descriptor for the multi-series chart.
pub struct SeriesData {
    pub points: Vec<SamplePoint>,
    pub color: Color,
    pub max_value: f32,
    /// Enable light fill under the line (only for single-run, single-series charts).
    pub fill: bool,
    /// Per-line alpha (0..1); applied to the stroke color. Legend colour stays
    /// full-opacity.
    pub line_alpha: Option<f32>,
}

/// Multi-series chart canvas. Rebuilds geometry every frame from pure
/// graph_geom functions.
pub struct MultiChart {
    pub series: Vec<SeriesData>,
    pub window: DrawWindow,
    /// Use gap-aware decimation when the series in-window count far exceeds
    /// the pixel budget.
    pub decimate: bool,
}

impl MultiChart {
    pub fn new(decimate: bool) -> Self {
        Self {
            series: Vec::new(),
            window: DrawWindow {
                window_secs: 60.0,
                window_end_ns: 0,
            },
            decimate,
        }
    }
}

impl<Message> canvas::Program<Message> for MultiChart {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        _renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<iced::Renderer>> {
        let mut frame = Frame::new(_renderer, bounds.size());
        draw_multi_chart(&mut frame, self, bounds.size());
        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        None
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

fn draw_multi_chart(frame: &mut Frame, chart: &MultiChart, size: Size) {
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }

    // ---- compute layout --------------------------------------------------
    let bounds = PlotBounds {
        left: GUTTER_LEFT,
        top: GUTTER_TOP,
        right: size.width - GUTTER_RIGHT,
        bottom: size.height - GUTTER_BOTTOM,
    };

    if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
        return; // too small to draw
    }

    let plot_rect = Rectangle {
        x: bounds.left,
        y: bounds.top,
        width: bounds.right - bounds.left,
        height: bounds.bottom - bounds.top,
    };

    // ---- axis label style ------------------------------------------------
    let label_size: f32 = 10.0;
    let axis_color = theme::AXIS_LABEL;

    // ---- 1. Draw grid lines + Y labels + X ticks (outside clip) ---------
    // Grid (horizontal lines at 25%, 50%, 75%)
    let grid_ys = graph_geom::compute_grid_y(&bounds);
    let grid_stroke = Stroke::default()
        .with_color(theme::GRID)
        .with_width(0.5);
    for y in grid_ys {
        let path = Path::line(
            pt(bounds.left, y),
            pt(bounds.right, y),
        );
        frame.stroke(&path, grid_stroke);
    }

    // Y-axis labels (0% … 100% in the left gutter)
    let y_ticks = graph_geom::compute_y_ticks(&bounds);
    for (y_pos, label) in &y_ticks {
        frame.fill_text(Text {
            content: label.clone(),
            position: pt(2.0, *y_pos - label_size * 0.5),
            color: axis_color,
            size: label_size.into(),
            ..Text::default()
        });
    }

    // X-axis time ticks (bottom gutter)
    let x_ticks = graph_geom::compute_time_ticks(
        chart.window.window_secs,
        bounds.left,
        bounds.right,
        chart.window.window_end_ns,
    );
    for (x_pos, label) in &x_ticks {
        // Edge-align the boundary ticks so "now" (at plot_right) and the oldest
        // label (at plot_left) don't overflow the gutter and get clipped.
        let align_x = if (*x_pos - bounds.right).abs() < 1.0 {
            iced::alignment::Horizontal::Right
        } else if (*x_pos - bounds.left).abs() < 1.0 {
            iced::alignment::Horizontal::Left
        } else {
            iced::alignment::Horizontal::Center
        };
        frame.fill_text(Text {
            content: label.clone(),
            position: pt(*x_pos, bounds.bottom + 2.0),
            color: axis_color,
            size: label_size.into(),
            align_x: align_x.into(),
            ..Text::default()
        });
    }

    // ---- 2. Clipped: fills + series curves ------------------------------
    frame.with_clip(plot_rect, |clipped| {
        for series in chart.series.iter() {
            let geom = graph_geom::compute_series(
                &series.points,
                series.max_value,
                &chart.window,
                &bounds,
                chart.decimate,
            );

            if geom.bezier_runs.is_empty() {
                continue;
            }

            let alpha = series.line_alpha.unwrap_or(1.0);
            let stroke = Stroke::default()
                .with_color(theme::with_alpha(series.color, alpha))
                .with_width(1.2);

            // ---- Fill under the first run (single-series charts only) ---
            if series.fill
                && geom.bezier_runs.len() == 1
                && let Some(run) = geom.bezier_runs.first()
                && !run.is_empty()
            {
                let first_seg = &run[0];
                let last_seg = &run[run.len() - 1];
                let baseline_y = bounds.bottom;

                let fill_path = Path::new(|builder| {
                    builder.move_to(pt(first_seg.start.0, first_seg.start.1));
                    for seg in run.iter() {
                        builder.bezier_curve_to(
                            pt(seg.c1.0, seg.c1.1),
                            pt(seg.c2.0, seg.c2.1),
                            pt(seg.end.0, seg.end.1),
                        );
                    }
                    builder.line_to(pt(last_seg.end.0, baseline_y));
                    builder.line_to(pt(first_seg.start.0, baseline_y));
                    builder.close();
                });
                let fill_color = Color {
                    a: 0.1,
                    ..series.color
                };
                clipped.fill(&fill_path, fill_color);
            }

            // ---- Stroke each Bézier run ---------------------------------
            for run in &geom.bezier_runs {
                if run.is_empty() {
                    continue;
                }
                let path = Path::new(|builder| {
                    builder.move_to(pt(run[0].start.0, run[0].start.1));
                    for seg in run.iter() {
                        builder.bezier_curve_to(
                            pt(seg.c1.0, seg.c1.1),
                            pt(seg.c2.0, seg.c2.1),
                            pt(seg.end.0, seg.end.1),
                        );
                    }
                });
                clipped.stroke(&path, stroke);
            }
        }
    });

    // ---- 3. Frame border (on top, never clipped) ------------------------
    let frame_path = Path::rectangle(plot_rect.position(), plot_rect.size());
    let frame_stroke = Stroke::default()
        .with_color(theme::PLOT_FRAME)
        .with_width(1.0);
    frame.stroke(&frame_path, frame_stroke);
}
