//! Multi-series charts for the lightwatch dashboard.
//! Thin iced adapter over pure geometry from graph_geom.

use super::graph_geom::{self, DrawWindow, PlotBounds};
use crate::model::SamplePoint;
use iced::mouse;
use iced::widget::canvas::{self, Event, Frame, Geometry, Path, Stroke};
use iced::{Color, Point, Rectangle, Size};

/// A single series descriptor for the multi-series chart.
pub struct SeriesData {
    pub points: Vec<SamplePoint>,
    pub color: Color,
    pub max_value: f32,
    /// Enable light fill under the line (only for single-series charts).
    pub fill: bool,
}

/// Multi-series chart canvas. Rebuilds geometry every frame from pure
/// graph_geom functions; no Cache involved for dynamic series paths.
pub struct MultiChart {
    pub series: Vec<SeriesData>,
    pub window: DrawWindow,
    pub show_grid: bool,
}

impl MultiChart {
    pub fn new(show_grid: bool) -> Self {
        Self {
            series: Vec::new(),
            window: DrawWindow {
                window_secs: 60.0,
                window_end_ns: 0,
            },
            show_grid,
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
        // Build a fresh geometry each frame.
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

fn draw_multi_chart(frame: &mut Frame, chart: &MultiChart, size: Size) {
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }

    let margin = 4.0;
    let bounds = PlotBounds {
        width: size.width,
        height: size.height,
        margin,
    };

    // Draw grid if enabled
    if chart.show_grid {
        let ys = graph_geom::compute_grid_y(&bounds);
        let grid_stroke = Stroke::default()
            .with_color(Color {
                r: 0.3,
                g: 0.3,
                b: 0.35,
                a: 0.3,
            })
            .with_width(0.5);
        for y in ys {
            let path = Path::line(
                Point::new(bounds.margin, y),
                Point::new(bounds.width - bounds.margin, y),
            );
            frame.stroke(&path, grid_stroke);
        }
    }

    // Draw each series
    for (idx, series) in chart.series.iter().enumerate() {
        let geom = graph_geom::compute_series(
            &series.points,
            series.max_value,
            &chart.window,
            &bounds,
            false, // no decimation by default
        );

        let stroke = Stroke::default()
            .with_color(series.color)
            .with_width(1.5);

        // Stroke all segments for this series
        for seg in &geom.segments {
            if seg.is_empty() {
                continue;
            }
            let path = Path::new(|builder| {
                builder.move_to(Point::new(seg[0].0, seg[0].1));
                for (x, y) in &seg[1..] {
                    builder.line_to(Point::new(*x, *y));
                }
            });
            frame.stroke(&path, stroke);
        }

        // Fill under the first segment (only when fill enabled and single segment)
        if series.fill
            && geom.segments.len() == 1
            && let Some(seg) = geom.segments.first()
            && seg.len() >= 2
        {
            let first = seg[0];
            let last = seg[seg.len() - 1];
            let baseline_y = bounds.margin + (bounds.height - 2.0 * bounds.margin);
            let fill_path = Path::new(|builder| {
                builder.move_to(Point::new(first.0, first.1));
                for (x, y) in &seg[1..] {
                    builder.line_to(Point::new(*x, *y));
                }
                builder.line_to(Point::new(last.0, baseline_y));
                builder.line_to(Point::new(first.0, baseline_y));
                builder.close();
            });
            let fill_color = Color {
                a: 0.1,
                ..series.color
            };
            frame.fill(&fill_path, fill_color);
        }

        // Prevent unused variable warning
        let _ = idx;
    }
}
