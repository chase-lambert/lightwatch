use super::graph::Sparkline;
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
            app.selected_preset = preset;
            // Build and validate a new config with the same interval but new window.
            let interval_ms = app.config.interval.as_millis() as u64;
            let window_secs = preset.window_secs();
            match HistoryConfig::validate(interval_ms, window_secs) {
                Ok(new_config) => {
                    // Write to shared slot. Mutex always succeeds — the sampler
                    // will pick up the latest config on its next iteration.
                    // Overwrites any pending config not yet consumed.
                    *app.pending_config.lock().unwrap() = Some(new_config.clone());
                    app.config = new_config;
                }
                Err(_) => {
                    // Validation failed — keep previous config, preset is
                    // already visually selected (user can retry or pick
                    // another).
                }
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

    let preset_row = {
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
    content = content.push(preset_row);
    content = content.push(section_header("CPU".into()));
    content = content.push(cpu_section(snap, hist, window_secs));
    content = content.push(section_header("Memory".into()));
    content = content.push(memory_section(snap, hist, window_secs));

    for gpu in snap.gpus.iter() {
        content = content.push(section_header(format!("GPU — {}", gpu.pci_id)));
        let gpu_hist = hist.gpu_series.iter().find(|gh| gh.pci_id == gpu.pci_id);
        content = content.push(gpu_section(gpu, gpu_hist, window_secs));
    }

    scrollable(container(content).width(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Subscription function
pub fn subscription(_app: &Lightwatch) -> Subscription<Message> {
    iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::SampleArrived)
}

/// Theme function
pub fn theme(_app: &Lightwatch) -> Theme {
    Theme::Dark
}

// ── Section builders ──

fn section_header(label: String) -> Element<'static, Message> {
    container(text(label).size(13).color(theme::TEXT))
        .padding(4)
        .into()
}

fn self_strip(snap: &Snapshot) -> Element<'static, Message> {
    let selfm = &snap.self_metrics;
    let rss = rfmt(&selfm.rss_kb, |v| {
        format!("{:.1} MiB RSS", *v as f64 / 1024.0)
    });
    let cpu = rfmt(&selfm.cpu_percent, |v| format!("{v:.1}% self"));
    let dur = format!("{}µs", snap.sample_duration_us);

    let items = row![
        text("lightwatch").size(13).color(theme::ACCENT_SELF),
        Space::new().width(Length::Fill),
        text(dur).size(11).color(theme::TEXT_DIM),
        Space::new().width(8),
        text(rss).size(11).color(theme::TEXT),
        Space::new().width(12),
        text(cpu).size(11).color(theme::TEXT),
        Space::new().width(12),
        text(format!("over:{}", snap.sampler_overruns))
            .size(11)
            .color(theme::TEXT_DIM),
        Space::new().width(8),
        text(format!("skip:{}", snap.ticks_skipped))
            .size(11)
            .color(theme::TEXT_DIM),
    ]
    .align_y(Alignment::Center)
    .spacing(0);

    container(items)
        .padding(4)
        .style(|_theme| container::Style {
            background: Some(iced::Background::Color(theme::SURFACE)),
            ..Default::default()
        })
        .into()
}

fn cpu_section(snap: &Snapshot, hist: &History, window_secs: f64) -> Element<'static, Message> {
    let cpu = &snap.cpu;
    let usage = rfmt(&cpu.usage_percent, |v| format!("{v:.1}%"));
    let temp = rfmt_opt(&cpu.temp_celsius, |v| format!("{v:.1}°C"));
    let freq = rfmt_opt(&cpu.freq_mhz, |v| format!("{v:.0} MHz"));

    let stats = row![
        stat_box("Usage".into(), usage, theme::ACCENT_CPU),
        Space::new().width(8),
        stat_box("Temp".into(), temp, theme::ACCENT_TEMP),
        Space::new().width(8),
        stat_box("Freq".into(), freq, theme::ACCENT_FREQ),
    ];

    let mut sp = Sparkline::new(theme::ACCENT_CPU);
    sp.update(hist.cpu_total.points(), 100.0, window_secs);
    let sparkline = Canvas::new(sp)
        .width(Length::Fill)
        .height(Length::Fixed(80.0));

    column![stats, sparkline].spacing(4).padding(4).into()
}

fn memory_section(snap: &Snapshot, hist: &History, window_secs: f64) -> Element<'static, Message> {
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

    let stats = row![
        stat_box("Used".into(), used, theme::ACCENT_MEM),
        Space::new().width(8),
        stat_box("Avail".into(), avail, theme::ACCENT_MEM),
        Space::new().width(8),
        stat_box("Swap".into(), swap, theme::ACCENT_SWAP),
        Space::new().width(8),
        stat_box("Load".into(), load, theme::ACCENT_LOAD),
    ];

    let mut sp = Sparkline::new(theme::ACCENT_MEM);
    sp.update(hist.mem_used.points(), max_mem, window_secs);
    let sparkline = Canvas::new(sp)
        .width(Length::Fill)
        .height(Length::Fixed(60.0));

    column![stats, sparkline].spacing(4).padding(4).into()
}

fn gpu_section(
    gpu: &GpuSnapshot,
    gpu_hist: Option<&GpuHistory>,
    window_secs: f64,
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
        _ => "—".into(),
    };
    let temp = rfmt_opt(&gpu.temp_celsius, |v| format!("{v:.1}°C"));
    let power = rfmt_opt(&gpu.power_watts, |v| format!("{v:.1} W"));

    let stats = row![
        stat_box("Util".into(), util, theme::ACCENT_GPU),
        Space::new().width(8),
        stat_box("VRAM".into(), vram, theme::ACCENT_GPU),
        Space::new().width(8),
        stat_box("Temp".into(), temp, theme::ACCENT_TEMP),
        Space::new().width(8),
        stat_box("Power".into(), power, theme::ACCENT_WARN),
    ];

    let mut content = column![stats].spacing(4).padding(4);

    if let Some(gh) = gpu_hist {
        let mut sp = Sparkline::new(theme::ACCENT_GPU);
        sp.update(gh.util.points(), 100.0, window_secs);
        let sparkline = Canvas::new(sp)
            .width(Length::Fill)
            .height(Length::Fixed(50.0));
        content = content.push(sparkline);
    }

    content.into()
}

// ── Helpers ──

fn rfmt<T: std::fmt::Display>(r: &Reading<T>, f: impl FnOnce(&T) -> String) -> String {
    match r {
        Reading::Value(v) => f(v),
        Reading::Unavailable { .. } => "—".into(),
    }
}

fn rfmt_opt<T: std::fmt::Display>(r: &Reading<T>, f: impl FnOnce(&T) -> String) -> String {
    rfmt(r, f)
}

fn rstr<T: std::fmt::Display>(r: &Reading<T>) -> String {
    match r {
        Reading::Value(v) => format!("{v:.2}"),
        Reading::Unavailable { .. } => "—".into(),
    }
}

fn stat_box(label: String, value: String, color: Color) -> Element<'static, Message> {
    container(
        column![
            text(label).size(10).color(theme::TEXT_DIM),
            text(value).size(14).color(color),
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
