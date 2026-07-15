use super::graph::{MultiChart, SeriesData};
use super::graph_geom::DrawWindow;
use super::theme;
use crate::model::*;
use crate::sample::latest::{Latest, Published};
use crate::sample::worker::Sampler;
use iced::widget::{Canvas, Space, button, column, container, row, scrollable, text};
use iced::{Alignment, Color, Element, Length, Subscription, Theme};
use std::sync::{Arc, Mutex, mpsc};

/// UI state held by the application.
pub struct Lightwatch {
    latest: Arc<Latest>,
    notify_rx: mpsc::Receiver<()>,
    pending_config: Arc<Mutex<Option<HistoryConfig>>>,
    published: Option<Arc<Published>>,
    last_seen_gen: u64,
    config: HistoryConfig,
    selected_preset: HistoryPreset,
    presets: Vec<HistoryPreset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryPreset {
    M1,
    M5,
    M15,
    M30,
    M60,
}

impl HistoryPreset {
    fn all() -> Vec<Self> {
        vec![Self::M1, Self::M5, Self::M15, Self::M30, Self::M60]
    }
    fn label(&self) -> &str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::M60 => "60m",
        }
    }
    /// Window in seconds for this preset.
    fn window_secs(&self) -> u64 {
        match self {
            Self::M1 => 60,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::M30 => 1800,
            Self::M60 => 3600,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    SampleArrived,
    SelectPreset(HistoryPreset),
}

/// Boot function for the iced application.
pub fn boot(config: HistoryConfig) -> (Lightwatch, iced::Task<Message>) {
    let latest = Arc::new(Latest::new());
    let (notify_tx, notify_rx) = mpsc::sync_channel::<()>(1);
    let pending_config = Arc::new(Mutex::new(None::<HistoryConfig>));

    let s_latest = Arc::clone(&latest);
    let s_notify = notify_tx;
    let s_config = config.clone();
    let s_pending = Arc::clone(&pending_config);
    std::thread::spawn(move || {
        let mut sampler = Sampler::new(s_config, s_latest, s_notify, s_pending);
        sampler.run();
    });

    let initial_preset = match config.window.as_secs() {
        s if s <= 60 => HistoryPreset::M1,
        s if s <= 300 => HistoryPreset::M5,
        s if s <= 900 => HistoryPreset::M15,
        s if s <= 1800 => HistoryPreset::M30,
        _ => HistoryPreset::M60,
    };

    let app = Lightwatch {
        latest,
        notify_rx,
        pending_config,
        published: None,
        last_seen_gen: 0,
        config,
        selected_preset: initial_preset,
        presets: HistoryPreset::all(),
    };
    (app, iced::Task::none())
}

/// Title function
pub fn title(_app: &Lightwatch) -> String {
    "lightwatch".into()
}

/// Update function
pub fn update(app: &mut Lightwatch, message: Message) -> iced::Task<Message> {
    match message {
        Message::SampleArrived => {
            while app.notify_rx.try_recv().is_ok() {}
            if let Some((g, pubd)) = app.latest.pull_if_newer(app.last_seen_gen) {
                app.last_seen_gen = g;
                app.published = Some(pubd);
            }
            iced::Task::none()
        }
        Message::SelectPreset(preset) => {
            // Validate BEFORE mutating selected_preset.
            let interval_ms = app.config.interval.as_millis() as u64;
            let window_secs = preset.window_secs();
            match HistoryConfig::validate(interval_ms, window_secs) {
                Ok(new_config) => {
                    app.selected_preset = preset;
                    *app.pending_config.lock().unwrap() = Some(new_config.clone());
                    app.config = new_config;
                }
                Err(_) => {}
            }
            iced::Task::none()
        }
    }
}

/// View function
pub fn view(app: &Lightwatch) -> Element<'_, Message> {
    let published = match &app.published {
        Some(p) => p,
        None => {
            return container(text("waiting for first sample...").color(theme::TEXT_DIM))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::Center)
                .align_y(Alignment::Center)
                .into();
        }
    };

    let snap = &published.snapshot;
    let hist = &published.history;
    let window_secs = app.config.window.as_secs_f64();
    // Smooth clock: window_end is boottime "now" on each display tick.
    let window_end_ns = crate::clock_boottime_ns();

    let presets = {
        let buttons: Vec<Element<Message>> = app
            .presets
            .iter()
            .map(|p| {
                let is_selected = *p == app.selected_preset;
                let label = text(p.label()).size(11).color(if is_selected {
                    Color::WHITE
                } else {
                    theme::TEXT_DIM
                });
                let mut btn = button(label);
                if is_selected {
                    btn = btn.style(iced::widget::button::primary);
                }
                btn.on_press(Message::SelectPreset(*p)).into()
            })
            .collect();
        row(buttons).spacing(4).padding(4)
    };

    let mut content = column![].spacing(6).padding(8);

    content = content.push(self_strip(snap));
    content = content.push(presets);

    // CPU section (chart-dominant)
    content = content.push(cpu_section(snap, hist, window_secs, window_end_ns));

    // Memory section (dual series)
    content = content.push(memory_section(snap, hist, window_secs, window_end_ns));

    // GPU sections
    for gpu in snap.gpus.iter() {
        let gpu_hist = hist.gpu_series.iter().find(|gh| gh.pci_id == gpu.pci_id);
        content = content.push(gpu_section(gpu, gpu_hist, window_secs, window_end_ns));
    }

    scrollable(container(content).width(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Subscription function: 100ms display tick.
pub fn subscription(_app: &Lightwatch) -> Subscription<Message> {
    iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::SampleArrived)
}

/// Theme function
pub fn theme(_app: &Lightwatch) -> Theme {
    Theme::Dark
}

// ---------------------------------------------------------------------------
// panel helper — surface card with border, rounded corners, padding
// ---------------------------------------------------------------------------

fn panel<'a>(
    header: Element<'a, Message>,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    container(column![header, body].spacing(4))
        .padding(8)
        .style(|_theme| container::Style {
            background: Some(iced::Background::Color(theme::SURFACE)),
            border: iced::Border {
                color: theme::BORDER,
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        })
        .into()
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

fn self_strip(snap: &Snapshot) -> Element<'static, Message> {
    let selfm = &snap.self_metrics;
    let rss = rfmt(&selfm.rss_kb, |v| {
        format!("{:.1} MiB", *v as f64 / 1024.0)
    });
    let cpu = rfmt(&selfm.cpu_percent, |v| format!("{v:.1}%"));
    let dur = format!("{}us", snap.sample_duration_us);

    let items = row![
        text("lightwatch")
            .size(11)
            .color(theme::with_alpha(theme::ACCENT_SELF, 0.7)),
        Space::new().width(Length::Fill),
        text(dur).size(10).color(theme::TEXT_DIM),
        Space::new().width(8),
        text(rss).size(10).color(theme::with_alpha(theme::TEXT, 0.6)),
        Space::new().width(12),
        text(cpu).size(10).color(theme::with_alpha(theme::TEXT, 0.6)),
    ]
    .align_y(Alignment::Center)
    .spacing(0);

    container(items).padding(4).into()
}

fn cpu_section(
    snap: &Snapshot,
    hist: &History,
    window_secs: f64,
    window_end_ns: u64,
) -> Element<'static, Message> {
    let cpu = &snap.cpu;
    let usage = rfmt(&cpu.usage_percent, |v| format!("{v:.1}%"));
    let temp = rfmt_opt(&cpu.temp_celsius, |v| format!("{v:.1}°C"));
    let freq = rfmt_opt(&cpu.freq_mhz, |v| format!("{v:.0} MHz"));

    let core_count = format!("{} core(s)", hist.cpu_per_core.len());
    let hidden_text = if cpu.core_hidden > 0 {
        format!(" (+{} hidden)", cpu.core_hidden)
    } else {
        String::new()
    };

    // Compact header: overall %, temp, freq, core count
    let header = row![
        section_label("CPU"),
        Space::new().width(8),
        text(usage).size(14).color(theme::ACCENT_CPU),
        Space::new().width(12),
        text(temp).size(12).color(theme::TEXT),
        Space::new().width(12),
        text(freq).size(12).color(theme::TEXT),
        Space::new().width(Length::Fill),
        text(format!("{}{}", core_count, hidden_text))
            .size(11)
            .color(theme::TEXT_DIM),
    ]
    .align_y(Alignment::Center);

    // Build multi-core overlay chart (hairball with per-line alpha)
    let mut chart = MultiChart::new(true); // enable decimation for CPU
    for (core_id, ring) in &hist.cpu_per_core {
        let color = theme::core_color(core_id.0);
        chart.series.push(SeriesData {
            points: ring.points(),
            color,
            max_value: 100.0,
            fill: false,
            line_alpha: Some(0.80),
        });
    }
    chart.window = DrawWindow {
        window_secs,
        window_end_ns,
    };

    let canvas = Canvas::new(chart)
        .width(Length::Fill)
        .height(Length::Fixed(180.0));

    // Legend: 4 columns below the chart (GSM-style)
    let cores = &hist.cpu_per_core;
    let n = cores.len();
    let per_col = n.div_ceil(4).max(1);

    let mut legend_cols: Vec<Element<Message>> = Vec::with_capacity(4);
    for col_idx in 0..4 {
        let start = col_idx * per_col;
        let end = ((col_idx + 1) * per_col).min(n);
        if start >= n {
            break;
        }
        let items: Vec<Element<Message>> = cores[start..end]
            .iter()
            .map(|(core_id, _ring)| {
                let pct_str = snap
                    .cpu
                    .per_core_percent
                    .iter()
                    .find(|cr| cr.id == *core_id)
                    .and_then(|cr| match &cr.value {
                        Reading::Value(v) => Some(format!("{:3.0}%", v)),
                        _ => None,
                    })
                    .unwrap_or_else(|| "  —".to_string());
                let color = theme::core_color(core_id.0);
                let label = format!("{} {}", core_id.label(), pct_str);
                legend_chip(&label, color)
            })
            .collect();
        legend_cols.push(column(items).spacing(1).into());
    }

    let legend = row(legend_cols).spacing(16);

    // Wrap in a panel card
    panel(header.into(), column![canvas, legend].spacing(4).into())
}

fn memory_section(
    snap: &Snapshot,
    hist: &History,
    window_secs: f64,
    window_end_ns: u64,
) -> Element<'static, Message> {
    let mem = &snap.memory;
    let max_mem = mem.total_kb as f32;
    let used = rfmt(&mem.used_kb, |v| {
        format!("{:.1} GiB", *v as f64 / 1_048_576.0)
    });
    let avail = rfmt(&mem.available_kb, |v| {
        format!("{:.1} GiB", *v as f64 / 1_048_576.0)
    });
    let swap = rfmt(&mem.swap_used_kb, |v| {
        format!("{:.1} GiB", *v as f64 / 1_048_576.0)
    });
    let load = format!(
        "{} / {} / {}",
        rstr(&mem.load_1min),
        rstr(&mem.load_5min),
        rstr(&mem.load_15min)
    );

    // Stats chips
    let stats = row![
        stat_box("Used", used, theme::ACCENT_MEM),
        Space::new().width(6),
        stat_box("Avail", avail, theme::ACCENT_MEM),
        Space::new().width(6),
        stat_box("Swap", swap, theme::ACCENT_SWAP),
        Space::new().width(6),
        stat_box("Load", load, theme::ACCENT_LOAD),
    ]
    .spacing(0);

    // Dual-series chart: mem used (with fill) + swap used (stroke only).
    // Both normalize to percent of their own pool (max_value = pool size in KB);
    // the Y axis shows 0–100% uniformly (KD7).
    let mut chart = MultiChart::new(false); // no decimation for two series
    chart.series.push(SeriesData {
        points: hist.mem_used.points(),
        color: theme::ACCENT_MEM,
        max_value: max_mem.max(1.0),
        fill: true,
        line_alpha: None,
    });

    if let Reading::Value(swap_total) = mem.swap_total_kb
        && swap_total > 0
    {
        chart.series.push(SeriesData {
            points: hist.swap_used.points(),
            color: theme::ACCENT_SWAP,
            max_value: swap_total as f32,
            fill: false,
            line_alpha: None,
        });
    }

    chart.window = DrawWindow {
        window_secs,
        window_end_ns,
    };

    let canvas = Canvas::new(chart)
        .width(Length::Fill)
        .height(Length::Fixed(120.0));

    // Header: section label
    let header = section_label("Memory");

    let body = column![stats, canvas].spacing(4);

    panel(header, body.into())
}

fn gpu_section(
    gpu: &GpuSnapshot,
    gpu_hist: Option<&GpuHistory>,
    window_secs: f64,
    window_end_ns: u64,
) -> Element<'static, Message> {
    let util = rfmt(&gpu.util_percent, |v| format!("{v:.1}%"));
    let vram = match (&gpu.vram_used_kb, &gpu.vram_total_kb) {
        (Reading::Value(u), Reading::Value(t)) => {
            let pct = if *t > 0 {
                *u as f64 / *t as f64 * 100.0
            } else {
                0.0
            };
            format!("{:.0}% ({:.0} MiB)", pct, *u as f64 / 1024.0)
        }
        _ => "--".into(),
    };
    let temp = rfmt_opt(&gpu.temp_celsius, |v| format!("{v:.1}°C"));
    let power = rfmt_opt(&gpu.power_watts, |v| format!("{v:.1} W"));

    let title = format!("{} -- {}", gpu.name, gpu.pci_id);

    let stats = row![
        stat_box("Util", util, theme::ACCENT_GPU),
        Space::new().width(6),
        stat_box("VRAM", vram, theme::ACCENT_GPU),
        Space::new().width(6),
        stat_box("Temp", temp, theme::ACCENT_TEMP),
        Space::new().width(6),
        stat_box("Power", power, theme::ACCENT_WARN),
    ]
    .spacing(0);

    let header = text(title).size(13).color(theme::TEXT);

    let mut body = column![stats].spacing(4);

    if let Some(gh) = gpu_hist {
        let mut chart = MultiChart::new(false);
        chart.series.push(SeriesData {
            points: gh.util.points(),
            color: theme::ACCENT_GPU,
            max_value: 100.0,
            fill: true,
            line_alpha: None,
        });
        chart.window = DrawWindow {
            window_secs,
            window_end_ns,
        };

        let canvas = Canvas::new(chart)
            .width(Length::Fill)
            .height(Length::Fixed(80.0));
        body = body.push(canvas);
    }

    panel(
        header.into(),
        body.into(),
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn section_label(text_str: &str) -> Element<'static, Message> {
    text(text_str.to_owned())
        .size(13)
        .color(theme::TEXT)
        .into()
}

fn rfmt<T: std::fmt::Display>(r: &Reading<T>, f: impl FnOnce(&T) -> String) -> String {
    match r {
        Reading::Value(v) => f(v),
        Reading::Unavailable { .. } => "--".into(),
    }
}

fn rfmt_opt<T: std::fmt::Display>(r: &Reading<T>, f: impl FnOnce(&T) -> String) -> String {
    rfmt(r, f)
}

fn rstr<T: std::fmt::Display>(r: &Reading<T>) -> String {
    match r {
        Reading::Value(v) => format!("{v:.2}"),
        Reading::Unavailable { .. } => "--".into(),
    }
}

fn stat_box(label: &str, value: String, color: Color) -> Element<'static, Message> {
    container(
        column![
            text(label.to_owned()).size(10).color(theme::TEXT_DIM),
            text(value).size(13).color(color),
        ]
        .spacing(1),
    )
    .padding(4)
    .style(move |_theme| container::Style {
        background: Some(iced::Background::Color(theme::SURFACE)),
        ..Default::default()
    })
    .into()
}

/// A small colored legend chip: a color square + label.
fn legend_chip(label: &str, color: Color) -> Element<'static, Message> {
    let label_owned = label.to_owned();
    let swatch = container(Space::new().width(8).height(8))
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(color)),
            ..Default::default()
        });
    row![
        swatch,
        Space::new().width(4),
        text(label_owned).size(10).color(theme::TEXT_DIM),
    ]
    .align_y(Alignment::Center)
    .into()
}
