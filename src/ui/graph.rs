use crate::model::SamplePoint;
use iced::mouse;
use iced::widget::canvas::{self, Cache, Event, Frame, Geometry, Path, Stroke};
use iced::{Color, Point, Rectangle};

/// A sparkline canvas that draws a time-series from `SamplePoint` rings.
pub struct Sparkline {
    pub points: Vec<SamplePoint>,
    pub color: Color,
    pub max_value: f32,
    pub window_secs: f64,
    cache: Cache,
}

impl Sparkline {
    pub fn new(color: Color) -> Self {
        Self {
            points: Vec::new(),
            color,
            max_value: 100.0,
            window_secs: 900.0,
            cache: Cache::new(),
        }
    }

    pub fn update(&mut self, points: Vec<SamplePoint>, max_value: f32, window_secs: f64) {
        self.points = points;
        self.max_value = max_value.max(1.0);
        self.window_secs = window_secs.max(1.0);
        self.cache.clear();
    }
}

impl<Message> canvas::Program<Message> for Sparkline {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<iced::Renderer>> {
        let geom = self.cache.draw(renderer, bounds.size(), |frame| {
            draw_sparkline(
                frame,
                &self.points,
                self.color,
                self.max_value,
                self.window_secs,
            );
        });
        vec![geom]
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

fn draw_sparkline(
    frame: &mut Frame,
    points: &[SamplePoint],
    color: Color,
    max_value: f32,
    window_secs: f64,
) {
    if points.len() < 2 {
        return;
    }

    let width = frame.width();
    let height = frame.height();
    if width <= 0.0 || height <= 0.0 {
        return;
    }

    let margin = 2.0;
    let plot_width = width - 2.0 * margin;
    let plot_height = height - 2.0 * margin;

    // Compute time range. Right edge = latest point's t_boot_ns (including
    // gaps — not only non-gap values). Left edge = right − window_secs.
    // Convert to u64 ns for the computation.
    let window_ns = (window_secs * 1e9) as u64;
    let window_end = points.last().map(|pt| pt.t_boot_ns).unwrap_or(0);
    let window_start = window_end.saturating_sub(window_ns);

    // Build segments: connect consecutive non-gap points within the window.
    let mut segments: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut current: Vec<(f32, f32)> = Vec::new();

    for pt in points {
        // Exclude points older than the window (no clamping to left edge).
        if pt.t_boot_ns < window_start {
            continue;
        }
        if let Some(val) = pt.value {
            // Map timestamp to X position within the window
            let t_offset = pt.t_boot_ns.saturating_sub(window_start);
            let x =
                margin + ((t_offset as f64 / window_ns.max(1) as f64).min(1.0) as f32 * plot_width);
            let y = margin + plot_height - (val / max_value * plot_height);
            current.push((x, y));
        } else {
            if current.len() >= 2 {
                segments.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() >= 2 {
        segments.push(current);
    }

    let stroke = Stroke::default().with_color(color).with_width(1.5);

    for seg in &segments {
        let path = Path::new(|builder| {
            builder.move_to(Point::new(seg[0].0, seg[0].1));
            for (x, y) in &seg[1..] {
                builder.line_to(Point::new(*x, *y));
            }
        });
        frame.stroke(&path, stroke);
    }

    // Fill under the first segment
    if let Some(seg) = segments.first()
        && seg.len() >= 2
    {
        let first = seg[0];
        let last = seg[seg.len() - 1];
        let fill_path = Path::new(|builder| {
            builder.move_to(Point::new(first.0, first.1));
            for (x, y) in &seg[1..] {
                builder.line_to(Point::new(*x, *y));
            }
            builder.line_to(Point::new(last.0, margin + plot_height));
            builder.line_to(Point::new(first.0, margin + plot_height));
            builder.close();
        });
        let fill_color = Color { a: 0.1, ..color };
        frame.fill(&fill_path, fill_color);
    }
}
