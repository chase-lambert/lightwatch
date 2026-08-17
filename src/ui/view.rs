use super::graph::{ChartId, MultiChart, SeriesData};
use super::graph_geom::{AxisKind, DrawWindow};
use super::prefs::{self, SectionId, SectionVisibility};
use super::theme;
use crate::collect::proc::{self as proc_collect, KillOutcome};
use crate::model::*;
use crate::sample::latest::{Latest, Published};
use crate::sample::worker::Sampler;
use iced::alignment::Horizontal;
use iced::widget::text::Wrapping;
use iced::widget::{
    Canvas, Space, button, column, container, mouse_area, responsive, row, scrollable, text,
    text_input, tooltip,
};
use iced::{Alignment, Color, Element, Length, Size, Subscription, Theme};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Layout constants — chart mins + panel chrome estimates for flex vs scroll
// ---------------------------------------------------------------------------

const MIN_CPU_CHART: f32 = 100.0;
const MIN_MEM_CHART: f32 = 80.0;
const MIN_GPU_CHART: f32 = 60.0;

/// Padding (6×2) + internal column spacing budget inside a panel card.
const PANEL_FRAME: f32 = 20.0;
const PANEL_HEADER: f32 = 20.0;
const MEM_STATS_H: f32 = 36.0;
const GPU_STATS_H: f32 = 36.0;
/// Per-row budget for the 4-column CPU legend (chip + line-leading), conservative
/// so flex is not chosen when the legend alone would overflow the estimate.
const LEGEND_ROW_H: f32 = 20.0;
/// `column(items).spacing(2)` between legend chips in each column.
const LEGEND_COL_SPACING: f32 = 2.0;
const SECTION_GAP: f32 = 4.0;
const DISPLAY_INTERVAL: Duration = Duration::from_millis(100);
const STORAGE_MOUNT_WIDTH: f32 = 100.0;
const STORAGE_BAR_MAX_WIDTH: f32 = 140.0;
const STORAGE_USED_WIDTH: f32 = 72.0;
const STORAGE_SEPARATOR_WIDTH: f32 = 8.0;

/// GSM-like: every *expanded* section gets an equal share of leftover height.
const WEIGHT_SECTION: u16 = 1;
/// Collapsed section is header-only.
const COLLAPSED_HEADER_H: f32 = 28.0;
/// Inputs for the pure flex-vs-scroll decision (section region height only).
/// Sections always occupy a header row; only expanded ones need chart mins.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutPlan {
    pub cpu_expanded: bool,
    pub memory_expanded: bool,
    pub gpu_expanded: usize,
    pub gpu_collapsed: usize,
    /// Logical cores for CPU legend row estimate (when CPU expanded).
    pub cpu_cores: usize,
}

#[derive(Clone, Copy)]
struct ChartView {
    window_secs: f64,
    window_end_ns: u64,
    interval_ns: u64,
    flex: bool,
    animate: bool,
}

impl ChartView {
    fn window(self) -> DrawWindow {
        DrawWindow {
            sample_interval_ns: self.interval_ns,
            window_secs: self.window_secs,
            window_end_ns: self.window_end_ns,
        }
    }
}

impl LayoutPlan {
    /// Minimum height of the section stack when expanded charts are at floors.
    pub fn min_content_height(&self) -> f32 {
        let mut h = 0.0;
        let mut n = 0usize;

        // CPU always present as a header.
        n += 1;
        if self.cpu_expanded {
            let rows = self.cpu_cores.div_ceil(4).max(1) as f32;
            let legend_h = rows * LEGEND_ROW_H + LEGEND_COL_SPACING * (rows - 1.0).max(0.0);
            h += PANEL_FRAME + PANEL_HEADER + MIN_CPU_CHART + legend_h;
        } else {
            h += COLLAPSED_HEADER_H;
        }

        n += 1;
        if self.memory_expanded {
            h += PANEL_FRAME + PANEL_HEADER + MEM_STATS_H + MIN_MEM_CHART;
        } else {
            h += COLLAPSED_HEADER_H;
        }

        for _ in 0..self.gpu_expanded {
            h += PANEL_FRAME + PANEL_HEADER + GPU_STATS_H + MIN_GPU_CHART;
            n += 1;
        }
        for _ in 0..self.gpu_collapsed {
            h += COLLAPSED_HEADER_H;
            n += 1;
        }

        if n > 1 {
            h += SECTION_GAP * (n as f32 - 1.0);
        }
        h
    }
}

/// Prefer FillPortion flex when the section region is tall enough.
pub fn use_flex(available_h: f32, plan: &LayoutPlan) -> bool {
    available_h >= plan.min_content_height()
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Top-level dashboard tab (GSM-style shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppTab {
    Resources,
    Processes,
    Health,
}

/// UI state held by the application.
pub struct Lightwatch {
    latest: Arc<Latest>,
    notify_rx: mpsc::Receiver<()>,
    pending_config: Arc<Mutex<Option<HistoryConfig>>>,
    selected_process_control: Arc<Mutex<Option<ProcessId>>>,
    published: Option<Arc<Published>>,
    last_seen_gen: u64,
    /// Boottime-domain timestamp for chart motion, derived from the scheduled
    /// display deadline rather than callback delivery time.
    chart_now_ns: Option<u64>,
    chart_inputs: ChartInputs,
    profile_chart_inputs: ProfileChartInputs,
    config: HistoryConfig,
    selected_preset: HistoryPreset,
    presets: Vec<HistoryPreset>,
    visibility: SectionVisibility,
    active_tab: AppTab,
    /// Process search query (UI-only).
    process_query: String,
    process_sort: ProcessSortKey,
    /// When true, larger values first for the active numeric column.
    process_sort_desc: bool,
    /// The row selected in the process table by a single click.
    selected_process: Option<ProcessId>,
    /// The process currently open in the double-click detail page.
    profiled_process: Option<ProcessId>,
    /// Soft status under End Process (signal sent / denied / gone).
    process_status: Option<String>,
}

struct GpuChartInputs {
    pci_id: Arc<str>,
    content_signature: u64,
    series: Vec<SeriesData>,
}

#[derive(Default)]
struct ChartInputs {
    generation: u64,
    cpu_ids: Vec<CoreId>,
    cpu_legend_labels: Vec<String>,
    cpu_count_label: String,
    cpu_content_signature: u64,
    cpu_series: Vec<SeriesData>,
    memory_content_signature: u64,
    memory_series: Vec<SeriesData>,
    gpu_series: Vec<GpuChartInputs>,
}

#[derive(Default)]
struct ProfileChartInputs {
    generation: u64,
    cpu_signature: u64,
    cpu: Vec<SeriesData>,
    memory_signature: u64,
    memory: Vec<SeriesData>,
    memory_max_bytes: f32,
    io_signature: u64,
    io: Vec<SeriesData>,
    io_max_bytes_per_second: f32,
}

impl ProfileChartInputs {
    fn from_published(generation: u64, published: &Published) -> Self {
        let Some(profile) = &published.process_profile else {
            return Self::default();
        };
        let history = &profile.history;
        let cpu = vec![SeriesData {
            points: Arc::from(history.cpu.points()),
            color: theme::ACCENT_CPU,
            max_value: 100.0,
            fill: true,
            line_alpha: None,
        }];
        let memory = vec![SeriesData {
            points: Arc::from(history.rss_anon.points()),
            color: theme::ACCENT_MEM,
            max_value: series_max(&history.rss_anon).max(1.0),
            fill: true,
            line_alpha: None,
        }];
        let io_max = series_max(&history.disk_read)
            .max(series_max(&history.disk_write))
            .max(1.0);
        let io = vec![
            SeriesData {
                points: Arc::from(history.disk_read.points()),
                color: theme::ACCENT_CPU,
                max_value: io_max,
                fill: false,
                line_alpha: None,
            },
            SeriesData {
                points: Arc::from(history.disk_write.points()),
                color: theme::ACCENT_SWAP,
                max_value: io_max,
                fill: false,
                line_alpha: None,
            },
        ];
        Self {
            generation,
            cpu_signature: chart_content_signature(&cpu, [0]),
            memory_signature: chart_content_signature(&memory, [0]),
            memory_max_bytes: memory[0].max_value * 1024.0,
            io_signature: chart_content_signature(&io, [0, 1]),
            io_max_bytes_per_second: io_max,
            cpu,
            memory,
            io,
        }
    }
}

fn series_max(ring: &Ring) -> f32 {
    ring.points()
        .into_iter()
        .filter_map(|point| point.value)
        .fold(0.0, f32::max)
        * 1.1
}

impl ChartInputs {
    fn from_published(generation: u64, published: &Published) -> Self {
        let snap = &published.snapshot;
        let hist = &published.history;

        let cpu_ids = hist
            .cpu_per_core
            .iter()
            .map(|(core_id, _)| *core_id)
            .collect::<Vec<_>>();
        let cpu_series: Vec<SeriesData> = hist
            .cpu_per_core
            .iter()
            .map(|(core_id, ring)| SeriesData {
                points: Arc::from(ring.points()),
                color: theme::core_color(core_id.0),
                max_value: 100.0,
                fill: false,
                line_alpha: Some(0.80),
            })
            .collect();
        let cpu_content_signature =
            chart_content_signature(&cpu_series, cpu_ids.iter().map(|id| u64::from(id.0)));
        let cpu_legend_labels = cpu_ids
            .iter()
            .map(|core_id| {
                let percent = snap
                    .cpu
                    .per_core_percent
                    .iter()
                    .find(|reading| reading.id == *core_id)
                    .and_then(|reading| match &reading.value {
                        Reading::Value(value) => Some(format!("{value:3.0}%")),
                        Reading::Unavailable { .. } => None,
                    })
                    .unwrap_or_else(|| "  —".to_owned());
                format!("{} {percent}", core_id.label())
            })
            .collect();
        let cpu_count_label = if snap.cpu.core_hidden > 0 {
            format!(
                "{} core(s) (+{} hidden)",
                cpu_ids.len(),
                snap.cpu.core_hidden
            )
        } else {
            format!("{} core(s)", cpu_ids.len())
        };

        let mut memory_series = vec![SeriesData {
            points: Arc::from(hist.mem_used.points()),
            color: theme::ACCENT_MEM,
            max_value: (snap.memory.total_kb as f32).max(1.0),
            fill: true,
            line_alpha: None,
        }];
        if let Reading::Value(swap_total) = snap.memory.swap_total_kb
            && swap_total > 0
        {
            memory_series.push(SeriesData {
                points: Arc::from(hist.swap_used.points()),
                color: theme::ACCENT_SWAP,
                max_value: swap_total as f32,
                fill: false,
                line_alpha: None,
            });
        }
        let memory_content_signature =
            chart_content_signature(&memory_series, 0..memory_series.len() as u64);

        let gpu_series = hist
            .gpu_series
            .iter()
            .map(|gpu| {
                let series = vec![SeriesData {
                    points: Arc::from(gpu.util.points()),
                    color: theme::ACCENT_GPU,
                    max_value: 100.0,
                    fill: true,
                    line_alpha: None,
                }];
                GpuChartInputs {
                    pci_id: Arc::from(gpu.pci_id.as_str()),
                    content_signature: chart_content_signature(&series, [0]),
                    series,
                }
            })
            .collect();

        Self {
            generation,
            cpu_ids,
            cpu_legend_labels,
            cpu_count_label,
            cpu_content_signature,
            cpu_series,
            memory_content_signature,
            memory_series,
            gpu_series,
        }
    }

    fn gpu(&self, pci_id: &str) -> Option<&GpuChartInputs> {
        self.gpu_series
            .iter()
            .find(|gpu| gpu.pci_id.as_ref() == pci_id)
    }
}

/// Fingerprint the generation-scoped inputs that can alter canonical paths
/// without inspecting every sample. The publication generation remains the
/// sole epoch for point content; this covers membership, scale, and style.
fn chart_content_signature(
    series: &[SeriesData],
    membership: impl IntoIterator<Item = u64>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    series.len().hash(&mut hasher);
    for member in membership {
        member.hash(&mut hasher);
    }
    for item in series {
        item.points.len().hash(&mut hasher);
        item.max_value.to_bits().hash(&mut hasher);
        item.color.r.to_bits().hash(&mut hasher);
        item.color.g.to_bits().hash(&mut hasher);
        item.color.b.to_bits().hash(&mut hasher);
        item.color.a.to_bits().hash(&mut hasher);
        item.fill.hash(&mut hasher);
        item.line_alpha.map(f32::to_bits).hash(&mut hasher);
    }
    hasher.finish()
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
    DisplayTick(Instant),
    SelectPreset(HistoryPreset),
    ToggleSection(SectionId),
    SelectTab(AppTab),
    ProcessQueryChanged(String),
    SortProcesses(ProcessSortKey),
    SelectProcess(ProcessId),
    OpenProcessProfile(ProcessId),
    EndProcess,
    BackToProcesses,
}

/// Boot function for the iced application.
pub fn boot(config: HistoryConfig) -> (Lightwatch, iced::Task<Message>) {
    let latest = Arc::new(Latest::new());
    let (notify_tx, notify_rx) = mpsc::sync_channel::<()>(1);
    let pending_config = Arc::new(Mutex::new(None::<HistoryConfig>));
    let selected_process_control = Arc::new(Mutex::new(None::<ProcessId>));

    let s_latest = Arc::clone(&latest);
    let s_notify = notify_tx;
    let s_config = config.clone();
    let s_pending = Arc::clone(&pending_config);
    let s_selected = Arc::clone(&selected_process_control);
    std::thread::spawn(move || {
        let mut sampler = Sampler::new(s_config, s_latest, s_notify, s_pending, s_selected);
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
        selected_process_control,
        published: None,
        last_seen_gen: 0,
        chart_now_ns: None,
        chart_inputs: ChartInputs::default(),
        profile_chart_inputs: ProfileChartInputs::default(),
        config,
        selected_preset: initial_preset,
        presets: HistoryPreset::all(),
        visibility: prefs::load_ui_prefs(),
        active_tab: AppTab::Resources,
        process_query: String::new(),
        process_sort: ProcessSortKey::Memory,
        process_sort_desc: true,
        selected_process: None,
        profiled_process: None,
        process_status: None,
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
        Message::DisplayTick(deadline) => {
            app.chart_now_ns = Some(chart_boottime_ns(
                deadline,
                Instant::now(),
                crate::clock_boottime_ns(),
            ));
            while app.notify_rx.try_recv().is_ok() {}
            if let Some((g, pubd)) = app.latest.pull_if_newer(app.last_seen_gen) {
                app.last_seen_gen = g;
                app.chart_inputs = ChartInputs::from_published(g, &pubd);
                app.profile_chart_inputs = ProfileChartInputs::from_published(g, &pubd);
                app.published = Some(pubd);
            }
            iced::Task::none()
        }
        Message::SelectPreset(preset) => {
            // Validate BEFORE mutating selected_preset.
            let interval_ms = app.config.interval.as_millis() as u64;
            let window_secs = preset.window_secs();
            if let Ok(new_config) = HistoryConfig::validate(interval_ms, window_secs) {
                app.selected_preset = preset;
                *app.pending_config.lock().unwrap() = Some(new_config.clone());
                app.config = new_config;
            }
            iced::Task::none()
        }
        Message::ToggleSection(id) => {
            app.visibility.toggle(&id);
            // Always best-effort save after a toggle (no early-return skip).
            prefs::save_ui_prefs(&app.visibility);
            iced::Task::none()
        }
        Message::SelectTab(tab) => {
            app.active_tab = tab;
            iced::Task::none()
        }
        Message::ProcessQueryChanged(q) => {
            app.process_query = q;
            iced::Task::none()
        }
        Message::SortProcesses(key) => {
            if app.process_sort == key {
                app.process_sort_desc = !app.process_sort_desc;
            } else {
                app.process_sort = key;
                // Deliberate: numeric columns start descending (hog-first);
                // Name/PID start ascending (dictionary / id order).
                app.process_sort_desc = !matches!(key, ProcessSortKey::Name | ProcessSortKey::Pid);
            }
            iced::Task::none()
        }
        Message::SelectProcess(id) => {
            app.selected_process = Some(id);
            app.process_status = None;
            iced::Task::none()
        }
        Message::OpenProcessProfile(id) => {
            app.selected_process = Some(id);
            set_profile_target(
                &mut app.profiled_process,
                &app.selected_process_control,
                Some(id),
            );
            app.process_status = None;
            iced::Task::none()
        }
        Message::EndProcess => {
            if let Some(id) = app.profiled_process.or(app.selected_process) {
                let outcome = proc_collect::end_process(Path::new("/proc"), id);
                app.process_status = Some(kill_status_text(&outcome));
            }
            iced::Task::none()
        }
        Message::BackToProcesses => {
            set_profile_target(
                &mut app.profiled_process,
                &app.selected_process_control,
                None,
            );
            app.process_status = None;
            iced::Task::none()
        }
    }
}

fn set_profile_target(
    profiled: &mut Option<ProcessId>,
    control: &Mutex<Option<ProcessId>>,
    next: Option<ProcessId>,
) {
    *profiled = next;
    *control.lock().unwrap() = next;
}

fn kill_status_text(outcome: &KillOutcome) -> String {
    match outcome {
        KillOutcome::SignalSent => "signal sent (SIGTERM)".into(),
        KillOutcome::SignalSentToRoot { root_pid } => {
            format!("signal sent (SIGTERM) to app root pid {root_pid}")
        }
        KillOutcome::Gone => "process gone".into(),
        KillOutcome::IdentityMismatch => "process identity changed — not signalled".into(),
        KillOutcome::PermissionDenied => "permission denied".into(),
        KillOutcome::Failed(s) => format!("failed: {s}"),
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
    let processes = match app.active_tab {
        AppTab::Processes if app.profiled_process.is_none() => Some(visible_processes(
            &snap.processes,
            &app.process_query,
            app.process_sort,
            app.process_sort_desc,
        )),
        _ => None,
    };
    // One chrome row: tabs left, tab-specific control (presets / search) right.
    let chrome = tab_chrome(app, processes.as_ref().map(|visible| visible.match_count));

    let body: Element<'_, Message> = match app.active_tab {
        AppTab::Resources => resources_body(app, snap),
        AppTab::Processes => processes_body(app, snap, processes.as_ref()),
        AppTab::Health => health_body(snap),
    };

    // Extra air between the tab chrome and the first panel (CPU / table / health).
    column![chrome, body]
        .spacing(10)
        .padding(iced::Padding {
            top: 8.0,
            right: 8.0,
            bottom: 6.0,
            left: 8.0,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Tabs on the left; trailing chrome (history presets or process search) on the right.
fn tab_chrome<'a>(app: &'a Lightwatch, process_match_count: Option<usize>) -> Element<'a, Message> {
    let tabs = tab_buttons(app.active_tab);
    let trailing: Element<'a, Message> = match app.active_tab {
        AppTab::Resources => history_presets(app),
        AppTab::Processes => {
            if let Some(match_count) = process_match_count {
                process_search_trailing(app, match_count)
            } else {
                Space::new().width(Length::Shrink).into()
            }
        }
        AppTab::Health => Space::new().width(Length::Shrink).into(),
    };

    row![tabs, Space::new().width(Length::Fill), trailing]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding([0, 2])
        .width(Length::Fill)
        .into()
}

fn tab_buttons(active: AppTab) -> Element<'static, Message> {
    let tabs = [
        (AppTab::Resources, "Resources"),
        (AppTab::Processes, "Processes"),
        (AppTab::Health, "Health"),
    ];
    let buttons: Vec<Element<Message>> = tabs
        .iter()
        .map(|(tab, label)| {
            let selected = *tab == active;
            let label = text(*label).size(12).color(if selected {
                Color::WHITE
            } else {
                theme::TEXT_DIM
            });
            let mut btn = button(label).padding([4, 10]);
            if selected {
                btn = btn.style(iced::widget::button::primary);
            } else {
                btn = btn.style(iced::widget::button::text);
            }
            btn.on_press(Message::SelectTab(*tab)).into()
        })
        .collect();
    row(buttons).spacing(4).align_y(Alignment::Center).into()
}

fn history_presets(app: &Lightwatch) -> Element<'_, Message> {
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
            let mut btn = button(label).padding([4, 8]);
            if is_selected {
                btn = btn.style(iced::widget::button::primary);
            }
            btn.on_press(Message::SelectPreset(*p)).into()
        })
        .collect();
    row(buttons).spacing(4).align_y(Alignment::Center).into()
}

fn process_search_trailing(app: &Lightwatch, match_count: usize) -> Element<'_, Message> {
    let count_label = if app.process_query.trim().is_empty() {
        format!("{match_count} processes")
    } else {
        format!("{match_count} match(es)")
    };
    let search = text_input("Search name or pid…", &app.process_query)
        .on_input(Message::ProcessQueryChanged)
        .padding(5)
        .size(12)
        .width(Length::Fixed(220.0));

    row![text(count_label).size(11).color(theme::TEXT_DIM), search,]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

fn resources_body<'a>(app: &'a Lightwatch, snap: &'a Snapshot) -> Element<'a, Message> {
    let window_secs = app.config.window.as_secs_f64();
    // Two-interval diagnostic look-ahead: chart "now" lags wall clock by two
    // sample intervals so the next two real samples sit off-screen right and
    // scroll in with immutable spline geometry (no re-fitting at reveal).
    let interval_ns = app.config.interval.as_nanos() as u64;
    let delay_ns = interval_ns.saturating_mul(2);
    let chart_now_ns = app
        .chart_now_ns
        .expect("published snapshots are installed only on display ticks");
    let window_end_ns = chart_now_ns.saturating_sub(delay_ns);

    let gpu_expanded = snap
        .gpus
        .iter()
        .filter(|g| app.visibility.is_gpu_visible(&g.pci_id))
        .count();
    let gpu_collapsed = snap.gpus.len().saturating_sub(gpu_expanded);
    let any_expanded = app.visibility.show_cpu || app.visibility.show_memory || gpu_expanded > 0;
    let animate = resource_chart_animation_active(app.active_tab, true, any_expanded);
    let layout_plan = LayoutPlan {
        cpu_expanded: app.visibility.show_cpu,
        memory_expanded: app.visibility.show_memory,
        gpu_expanded,
        gpu_collapsed,
        cpu_cores: app.chart_inputs.cpu_ids.len(),
    };

    let vis = app.visibility.clone();
    let chart_inputs = &app.chart_inputs;
    responsive(move |size: Size| {
        let flex = use_flex(size.height, &layout_plan);
        build_sections(
            snap,
            chart_inputs,
            &vis,
            ChartView {
                window_secs,
                window_end_ns,
                interval_ns,
                flex,
                animate,
            },
        )
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn resource_chart_animation_active(
    active_tab: AppTab,
    has_publication: bool,
    any_expanded: bool,
) -> bool {
    active_tab == AppTab::Resources && has_publication && any_expanded
}

fn health_body(snap: &Snapshot) -> Element<'static, Message> {
    let storage = health_storage_panel(&snap.health);
    let battery = health_battery_panel(&snap.health);
    // A little air under Battery so the last card isn't hard against the window edge.
    scrollable(
        column![storage, battery, Space::new().height(10.0)]
            .spacing(SECTION_GAP)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: 0.0,
                right: 0.0,
                bottom: 4.0,
                left: 0.0,
            }),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn health_storage_panel(health: &crate::model::HealthSnapshot) -> Element<'static, Message> {
    let mut body_items: Vec<Element<'static, Message>> = Vec::new();

    match &health.mounts {
        Reading::Value(mounts) if !mounts.is_empty() => {
            for m in mounts {
                body_items.push(mount_row(m));
            }
        }
        Reading::Value(_) => {
            body_items.push(
                text("No data mounts")
                    .size(12)
                    .color(theme::TEXT_DIM)
                    .into(),
            );
        }
        Reading::Unavailable { .. } => {
            // Quiet: enumeration failed — omit mount list noise.
        }
    }

    match &health.drives {
        Reading::Value(drives) if !drives.is_empty() => {
            if !body_items.is_empty() {
                body_items.push(Space::new().height(6).into());
            }
            for d in drives {
                body_items.push(drive_row(d));
            }
        }
        _ => {}
    }

    if body_items.is_empty() {
        body_items.push(text("—").size(12).color(theme::TEXT_DIM).into());
    }

    let header = row![section_label("Storage")]
        .align_y(Alignment::Center)
        .width(Length::Fill);
    let body = column(body_items).spacing(4).width(Length::Fill);
    panel(header.into(), Some(body.into()), true, false)
}

fn health_battery_panel(health: &crate::model::HealthSnapshot) -> Element<'static, Message> {
    let mut body_items: Vec<Element<'static, Message>> = Vec::new();

    match &health.batteries {
        Reading::Value(bats) if !bats.is_empty() => {
            for b in bats {
                body_items.push(battery_row(b));
            }
        }
        Reading::Value(_) => {
            body_items.push(text("No batteries").size(12).color(theme::TEXT_DIM).into());
        }
        Reading::Unavailable { .. } => {
            body_items.push(text("—").size(12).color(theme::TEXT_DIM).into());
        }
    }

    let header = row![section_label("Battery")]
        .align_y(Alignment::Center)
        .width(Length::Fill);
    // Slightly looser than Storage so pack + peripherals breathe;
    // extra top padding separates the "Battery" title from the first row.
    let body = column(body_items)
        .spacing(8)
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });
    panel(header.into(), Some(body.into()), true, false)
}

fn mount_row(m: &crate::model::MountRow) -> Element<'static, Message> {
    let frac = (m.use_percent / 100.0).clamp(0.0, 1.0);
    let bar = fill_bar(frac, theme::ACCENT_MEM);
    let used = bytes_to_human(m.used_bytes);
    let available = format!("{} avail", bytes_to_human(m.available_bytes));
    row![
        container(
            text(m.mountpoint.clone())
                .size(12)
                .color(theme::TEXT)
                .wrapping(Wrapping::None),
        )
        .width(Length::Fixed(STORAGE_MOUNT_WIDTH))
        .clip(true),
        bar,
        text(used)
            .size(12)
            .color(theme::TEXT_DIM)
            .width(Length::Fixed(STORAGE_USED_WIDTH))
            .align_x(Horizontal::Right)
            .wrapping(Wrapping::None),
        text("·")
            .size(12)
            .color(theme::TEXT_DIM)
            .width(Length::Fixed(STORAGE_SEPARATOR_WIDTH))
            .align_x(Horizontal::Center),
        text(available).size(12).color(theme::TEXT_DIM),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into()
}

fn drive_row(d: &crate::model::DriveRow) -> Element<'static, Message> {
    let size = match &d.size_bytes {
        Reading::Value(b) => bytes_to_human(*b),
        Reading::Unavailable { .. } => "—".into(),
    };
    let mut parts = vec![d.model.clone(), d.kind.label().to_string(), size];
    if let Reading::Value(t) = &d.temp_celsius {
        parts.push(format!("{t:.0}°C"));
    }
    if let Reading::Value(w) = &d.wear_percent_used {
        parts.push(format!("wear {w}%"));
    }
    // Interesting SMART only.
    if let Reading::Value(c) = &d.critical_warning
        && *c != 0
    {
        parts.push(format!("warn 0x{c:02x}"));
    }
    if let Reading::Value(e) = &d.media_errors
        && *e > 0
    {
        parts.push(format!("media err {e}"));
    }
    let line = parts.join(" · ");
    text(line).size(12).color(theme::TEXT).into()
}

fn battery_row(b: &crate::model::BatteryRow) -> Element<'static, Message> {
    let charge = match &b.charge_percent {
        Reading::Value(p) => format!("{p:.0}%"),
        Reading::Unavailable { .. } => match &b.capacity_level {
            Reading::Value(l) => l.clone(),
            Reading::Unavailable { .. } => "—".into(),
        },
    };

    let mut trailing = String::new();
    if b.kind == crate::model::BatteryKind::System {
        if let Reading::Value(h) = &b.health_percent {
            trailing.push_str(&format!("health {h:.0}%"));
        }
        if let Reading::Value(c) = &b.cycle_count {
            if !trailing.is_empty() {
                trailing.push_str(" · ");
            }
            trailing.push_str(&format!("{c} cycles"));
        }
    }

    let label = if b.kind == crate::model::BatteryKind::System {
        format!("{} · {}", b.id, b.label)
    } else {
        b.label.clone()
    };

    row![
        text(label)
            .size(12)
            .color(theme::TEXT)
            .width(Length::FillPortion(3)),
        text(charge)
            .size(12)
            .color(theme::TEXT)
            .width(Length::FillPortion(1)),
        text(trailing)
            .size(12)
            .color(theme::TEXT_DIM)
            .width(Length::FillPortion(3)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into()
}

/// Simple horizontal fill bar (used fraction 0..1).
fn fill_bar(fraction: f32, color: Color) -> Element<'static, Message> {
    let filled = ((fraction.clamp(0.0, 1.0) * 100.0).round() as u16).min(100);
    let empty = 100u16.saturating_sub(filled);
    let filled_w = filled.max(if fraction > 0.0 { 1 } else { 0 });
    let empty_w = if empty == 0 && filled_w < 100 {
        0
    } else {
        empty.max(if fraction < 1.0 { 1 } else { 0 })
    };

    let fill = container(Space::new().width(Length::Fill).height(Length::Fill))
        .width(Length::FillPortion(filled_w.max(1)))
        .height(Length::Fixed(8.0))
        .style(move |_t| container::Style {
            background: Some(iced::Background::Color(color)),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let rest = container(Space::new().width(Length::Fill).height(Length::Fill))
        .width(Length::FillPortion(empty_w.max(1)))
        .height(Length::Fixed(8.0))
        .style(|_t| container::Style {
            background: Some(iced::Background::Color(theme::with_alpha(
                theme::BORDER,
                0.6,
            ))),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    // Always show both tracks so layout is stable near 0% / 100%.
    let parts: Vec<Element<'static, Message>> = if filled == 0 {
        vec![rest.into()]
    } else if empty == 0 {
        vec![fill.into()]
    } else {
        vec![fill.into(), rest.into()]
    };

    container(
        row(parts)
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fixed(8.0)),
    )
    .width(Length::Fill)
    .max_width(STORAGE_BAR_MAX_WIDTH)
    .into()
}

// Column flex portions for the process table (sum = 100).
const COL_NAME: u16 = 28;
const COL_CPU: u16 = 12;
const COL_MEM: u16 = 14;
const COL_DREAD: u16 = 16;
const COL_DWRITE: u16 = 16;
const COL_PID: u16 = 14;

fn processes_body<'a>(
    app: &'a Lightwatch,
    snap: &'a Snapshot,
    visible: Option<&VisibleProcesses<'a>>,
) -> Element<'a, Message> {
    if let Some(profiled) = app.profiled_process {
        return match app
            .published
            .as_ref()
            .and_then(|published| published.process_profile.as_ref())
            .filter(|profile| profile.snapshot.id == profiled)
        {
            Some(profile) => process_profile_body(app, profile),
            None => {
                let message = if snap.processes.iter().any(|row| row.id == profiled) {
                    "Loading process profile…"
                } else {
                    "Process ended before its first profile sample."
                };
                column![
                    button("← Back").on_press(Message::BackToProcesses),
                    Space::new().height(24),
                    text(message).size(14).color(theme::TEXT_DIM)
                ]
                .spacing(8)
                .into()
            }
        };
    }

    let visible = visible.expect("process table is only built when a visible set exists");

    // Search lives in the shared tab chrome row; body starts at the table.
    let headers = process_header_row(app.process_sort, app.process_sort_desc);

    let selected = app.selected_process;
    let body_rows: Vec<Element<Message>> = visible
        .rows
        .iter()
        .map(|row| process_row(row, selected == Some(row.id)))
        .collect();

    let list = scrollable(column(body_rows).spacing(1).width(Length::Fill))
        .height(Length::Fill)
        .width(Length::Fill);

    let selected_row = selected.and_then(|id| visible.rows.iter().find(|row| row.id == id));
    let mut end = button(text("End Process").size(12)).padding([6, 12]);
    if selected_row.is_some() {
        end = end
            .style(iced::widget::button::danger)
            .on_press(Message::EndProcess);
    }
    let status = app.process_status.clone().unwrap_or_else(|| {
        selected_row.map_or_else(
            || "Select a process · double-click to inspect".into(),
            |row| {
                format!(
                    "{} ({}) selected · double-click to inspect",
                    row.name, row.id.pid
                )
            },
        )
    });
    let footer = row![
        end,
        Space::new().width(12),
        text(status).size(11).color(theme::TEXT_DIM)
    ]
    .align_y(Alignment::Center)
    .padding([4, 2]);

    column![headers, list, footer]
        .spacing(4)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn process_profile_body<'a>(
    app: &'a Lightwatch,
    profile: &'a PublishedProcessProfile,
) -> Element<'a, Message> {
    let snapshot = &profile.snapshot;
    let alive = snapshot.alive;
    let status_color = if alive {
        theme::ACCENT_GPU
    } else {
        theme::ACCENT_WARN
    };
    let mut end = button(text("End Process").size(12))
        .style(iced::widget::button::danger)
        .padding([6, 12]);
    if alive {
        end = end.on_press(Message::EndProcess);
    }
    let header = row![
        button("← Back")
            .style(iced::widget::button::text)
            .on_press(Message::BackToProcesses),
        text(snapshot.name.as_str()).size(20).color(theme::TEXT),
        text(if alive { "LIVE" } else { "ENDED" })
            .size(10)
            .color(status_color),
        Space::new().width(Length::Fill),
        text(format!("PID {}", snapshot.id.pid))
            .size(12)
            .color(theme::TEXT_DIM),
        end,
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let uid = rfmt(&snapshot.uid, |value| format!("UID {value}"));
    let summary = row![
        profile_stat("State", process_state(snapshot.state)),
        profile_stat("Age", format_duration(snapshot.age_secs)),
        profile_stat("Threads", snapshot.thread_count.to_string()),
        profile_stat("Parent", snapshot.parent_pid.to_string()),
        profile_stat("User", uid),
        profile_stat("CPU priority", nice_label(snapshot.nice)),
    ]
    .spacing(8);

    let chart_window = profile_chart_window(app);
    let cpu = rfmt(&snapshot.cpu_percent, |value| format!("{value:.1}%"));
    let cpu_time = format!(
        "{} user · {} system",
        format_duration(snapshot.user_cpu_secs as u64),
        format_duration(snapshot.system_cpu_secs as u64)
    );
    let faults = format!(
        "{} minor/s · {} major/s",
        rfmt(&snapshot.minor_faults_per_sec, |value| format!(
            "{value:.1}"
        )),
        rfmt(&snapshot.major_faults_per_sec, |value| format!(
            "{value:.1}"
        ))
    );
    let cpu_card = profile_card(
        "CPU",
        row![
            text(cpu).size(16).color(theme::ACCENT_CPU),
            text(cpu_time).size(11).color(theme::TEXT_DIM),
            Space::new().width(Length::Fill),
            text(faults).size(11).color(theme::TEXT_DIM),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into(),
        profile_chart(
            ChartId::ProcessCpu,
            app.profile_chart_inputs.generation,
            app.profile_chart_inputs.cpu_signature,
            &app.profile_chart_inputs.cpu,
            AxisKind::Percent,
            chart_window.clone(),
            alive,
        ),
    );

    let memory_primary = bytes_to_human(snapshot.rss_anon_kb.saturating_mul(1024));
    let memory_detail = format!(
        "PSS {} · Private {} · RSS {} · Peak {} · Swap {}",
        reading_bytes(&snapshot.pss_kb),
        reading_bytes(&snapshot.private_kb),
        reading_bytes(&snapshot.rss_total_kb),
        reading_bytes(&snapshot.rss_peak_kb),
        reading_bytes(&snapshot.swap_kb),
    );
    let memory_card = profile_card(
        "Memory",
        row![
            text(format!("{memory_primary} RssAnon"))
                .size(16)
                .color(theme::ACCENT_MEM),
            Space::new().width(Length::Fill),
            text(memory_detail).size(11).color(theme::TEXT_DIM),
        ]
        .align_y(Alignment::Center)
        .into(),
        profile_chart(
            ChartId::ProcessMemory,
            app.profile_chart_inputs.generation,
            app.profile_chart_inputs.memory_signature,
            &app.profile_chart_inputs.memory,
            AxisKind::Bytes {
                max_bytes: app.profile_chart_inputs.memory_max_bytes,
            },
            chart_window.clone(),
            false,
        ),
    );

    let read_rate = reading_rate(&snapshot.disk_read_bytes_per_sec);
    let write_rate = reading_rate(&snapshot.disk_write_bytes_per_sec);
    let io_card = profile_card(
        "Disk I/O",
        row![
            text(format!("Read {read_rate}"))
                .size(14)
                .color(theme::ACCENT_CPU),
            text(format!("Write {write_rate}"))
                .size(14)
                .color(theme::ACCENT_SWAP),
            Space::new().width(Length::Fill),
            text(format!(
                "Totals {} read · {} written",
                reading_bytes_raw(&snapshot.disk_read_bytes),
                reading_bytes_raw(&snapshot.disk_write_bytes)
            ))
            .size(11)
            .color(theme::TEXT_DIM),
        ]
        .spacing(14)
        .align_y(Alignment::Center)
        .into(),
        profile_chart(
            ChartId::ProcessIo,
            app.profile_chart_inputs.generation,
            app.profile_chart_inputs.io_signature,
            &app.profile_chart_inputs.io,
            AxisKind::BytesPerSecond {
                max_bytes_per_second: app.profile_chart_inputs.io_max_bytes_per_second,
            },
            chart_window,
            false,
        ),
    );

    let fd_text = match snapshot.open_fds {
        Reading::Value(value) if value.capped => format!("≥{} open files", value.count),
        Reading::Value(value) => format!("{} open files", value.count),
        Reading::Unavailable { .. } => "open files unavailable".into(),
    };
    let more = container(
        column![
            text("More").size(13).color(theme::TEXT),
            text(format!(
                "CPUs {} · {fd_text}",
                rfmt(&snapshot.cpu_affinity, Clone::clone),
            ))
            .size(11)
            .color(theme::TEXT_DIM),
            text(format!(
                "Executable  {}",
                truncate_reading(&snapshot.executable, 120)
            ))
            .size(11)
            .color(theme::TEXT_DIM),
            text(format!(
                "Command  {}",
                truncate_reading(&snapshot.command_line, 140)
            ))
            .size(11)
            .color(theme::TEXT_DIM),
            text(format!(
                "Cgroup  {}",
                truncate_reading(&snapshot.cgroup, 140)
            ))
            .size(11)
            .color(theme::TEXT_DIM),
        ]
        .spacing(5),
    )
    .padding(10)
    .width(Length::Fill)
    .style(profile_card_style);

    let signal_status: Element<'_, Message> = app
        .process_status
        .as_ref()
        .map(|value| text(value).size(11).color(theme::TEXT_DIM).into())
        .unwrap_or_else(|| Space::new().height(0).into());

    scrollable(
        column![
            header,
            summary,
            signal_status,
            cpu_card,
            memory_card,
            io_card,
            more
        ]
        .spacing(8)
        .width(Length::Fill),
    )
    .height(Length::Fill)
    .into()
}

fn process_state(state: char) -> String {
    match state {
        'R' => "Running",
        'S' => "Waiting",
        'D' => "I/O wait",
        'T' | 't' => "Stopped",
        'Z' => "Zombie",
        'I' => "Idle",
        _ => "Other",
    }
    .into()
}

fn nice_label(nice: i64) -> String {
    match nice.cmp(&0) {
        std::cmp::Ordering::Less => format!("Higher ({nice})"),
        std::cmp::Ordering::Equal => "Normal (0)".into(),
        std::cmp::Ordering::Greater => format!("Lower (+{nice})"),
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn profile_stat(label: &'static str, value: String) -> Element<'static, Message> {
    container(
        column![
            text(label).size(10).color(theme::TEXT_DIM),
            text(value).size(12).color(theme::TEXT)
        ]
        .spacing(2),
    )
    .padding([5, 8])
    .style(profile_card_style)
    .into()
}

fn profile_card<'a>(
    title: &'static str,
    summary: Element<'a, Message>,
    chart: Element<'a, Message>,
) -> Element<'a, Message> {
    container(
        column![text(title).size(13).color(theme::TEXT), summary, chart]
            .spacing(6)
            .width(Length::Fill),
    )
    .padding(10)
    .width(Length::Fill)
    .style(profile_card_style)
    .into()
}

fn profile_card_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(iced::Background::Color(theme::SURFACE)),
        border: iced::Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 3.0.into(),
        },
        ..Default::default()
    }
}

fn profile_chart<'a>(
    id: ChartId,
    generation: u64,
    signature: u64,
    series: &'a [SeriesData],
    axis: AxisKind,
    window: DrawWindow,
    animate: bool,
) -> Element<'a, Message> {
    let mut chart = MultiChart::new(id, generation, signature, series, axis, true);
    chart.window = window;
    chart.animate = animate;
    Canvas::new(chart)
        .width(Length::Fill)
        .height(Length::Fixed(110.0))
        .into()
}

fn profile_chart_window(app: &Lightwatch) -> DrawWindow {
    let interval_ns = app.config.interval.as_nanos() as u64;
    DrawWindow {
        sample_interval_ns: interval_ns,
        window_secs: app.config.window.as_secs_f64(),
        window_end_ns: app
            .chart_now_ns
            .unwrap_or_else(crate::clock_boottime_ns)
            .saturating_sub(interval_ns.saturating_mul(2)),
    }
}

fn reading_bytes(value: &Reading<u64>) -> String {
    rfmt(value, |value| bytes_to_human(value.saturating_mul(1024)))
}

fn reading_bytes_raw(value: &Reading<u64>) -> String {
    rfmt(value, |value| bytes_to_human(*value))
}

fn reading_rate(value: &Reading<f32>) -> String {
    rfmt(value, |value| {
        format!("{}/s", bytes_to_human(*value as u64))
    })
}

fn truncate_reading(value: &Reading<String>, max: usize) -> String {
    rfmt(value, |value| {
        if value.chars().count() <= max {
            value.clone()
        } else {
            value.chars().take(max - 1).collect::<String>() + "…"
        }
    })
}

fn sort_marker(active: bool, desc: bool) -> &'static str {
    if !active {
        ""
    } else if desc {
        " ▼"
    } else {
        " ▲"
    }
}

fn process_header_row(sort: ProcessSortKey, desc: bool) -> Element<'static, Message> {
    let cell = |key: ProcessSortKey, label: &str, portion: u16| -> Element<'static, Message> {
        let mark = sort_marker(sort == key, desc);
        let t = text(format!("{label}{mark}"))
            .size(11)
            .color(if sort == key {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            });
        button(t)
            .style(iced::widget::button::text)
            .padding(2)
            .on_press(Message::SortProcesses(key))
            .width(Length::FillPortion(portion))
            .into()
    };

    container(
        row![
            cell(ProcessSortKey::Name, "Name", COL_NAME),
            cell(ProcessSortKey::Cpu, "% CPU", COL_CPU),
            cell(ProcessSortKey::Memory, "Memory", COL_MEM),
            cell(ProcessSortKey::DiskRead, "Disk read", COL_DREAD),
            cell(ProcessSortKey::DiskWrite, "Disk write", COL_DWRITE),
            cell(ProcessSortKey::Pid, "ID", COL_PID),
        ]
        .spacing(2)
        .width(Length::Fill),
    )
    .padding([2, 4])
    .style(|_t| container::Style {
        background: Some(iced::Background::Color(theme::SURFACE)),
        border: iced::Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 2.0.into(),
        },
        ..Default::default()
    })
    .width(Length::Fill)
    .into()
}

fn process_row<'a>(row: &'a ProcessRow, selected: bool) -> Element<'a, Message> {
    let cpu = match &row.cpu_percent {
        Reading::Value(v) => format!("{v:.1}"),
        Reading::Unavailable { .. } => "—".into(),
    };
    let mem = bytes_to_human(row.mem_anon_kb.saturating_mul(1024));
    let dread = match &row.disk_read_bytes {
        Reading::Value(v) => bytes_to_human(*v),
        Reading::Unavailable { .. } => "—".into(),
    };
    let dwrite = match &row.disk_write_bytes {
        Reading::Value(v) => bytes_to_human(*v),
        Reading::Unavailable { .. } => "—".into(),
    };

    // Allow full binary names (e.g. gnome-system-monitor); still ellipsize
    // pathological long names so the row layout stays stable.
    const NAME_MAX: usize = 48;
    let name = if row.name.chars().count() > NAME_MAX {
        let mut n: String = row.name.chars().take(NAME_MAX - 1).collect();
        n.push('…');
        std::borrow::Cow::Owned(n)
    } else {
        std::borrow::Cow::Borrowed(row.name.as_str())
    };

    let foreground = if selected { Color::WHITE } else { theme::TEXT };
    let secondary = if selected {
        Color::WHITE
    } else {
        theme::TEXT_DIM
    };
    let cells = row![
        text(name)
            .size(12)
            .color(foreground)
            .width(Length::FillPortion(COL_NAME)),
        text(cpu)
            .size(12)
            .color(foreground)
            .width(Length::FillPortion(COL_CPU)),
        text(mem)
            .size(12)
            .color(foreground)
            .width(Length::FillPortion(COL_MEM)),
        text(dread)
            .size(12)
            .color(secondary)
            .width(Length::FillPortion(COL_DREAD)),
        text(dwrite)
            .size(12)
            .color(secondary)
            .width(Length::FillPortion(COL_DWRITE)),
        text(row.id.pid.to_string())
            .size(12)
            .color(secondary)
            .width(Length::FillPortion(COL_PID)),
    ]
    .spacing(2)
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .padding([3, 4]);

    let background = if selected {
        theme::with_alpha(theme::ACCENT_CPU, 0.35)
    } else {
        Color::TRANSPARENT
    };
    let id = row.id;
    mouse_area(
        container(cells)
            .width(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(background)),
                border: iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
    )
    .on_press(Message::SelectProcess(id))
    .on_double_click(Message::OpenProcessProfile(id))
    .into()
}

fn build_sections<'a>(
    snap: &'a Snapshot,
    chart_inputs: &'a ChartInputs,
    vis: &SectionVisibility,
    chart_view: ChartView,
) -> Element<'a, Message> {
    let mut sections: Vec<Element<'a, Message>> = Vec::new();
    let mut animation_driver_claimed = false;

    // Headers always present (GSM); body only when expanded. One visible
    // Canvas drives the window redraw chain; every Canvas still draws on each
    // resulting window frame.
    let cpu_view = claim_animation_driver(
        chart_view,
        vis.show_cpu && !chart_inputs.cpu_series.is_empty(),
        &mut animation_driver_claimed,
    );
    sections.push(cpu_section(snap, chart_inputs, vis.show_cpu, cpu_view));
    let memory_view = claim_animation_driver(
        chart_view,
        vis.show_memory && !chart_inputs.memory_series.is_empty(),
        &mut animation_driver_claimed,
    );
    sections.push(memory_section(
        snap,
        chart_inputs,
        vis.show_memory,
        memory_view,
    ));
    for gpu in snap.gpus.iter() {
        let expanded = vis.is_gpu_visible(&gpu.pci_id);
        let gpu_inputs = chart_inputs.gpu(&gpu.pci_id);
        let gpu_view = claim_animation_driver(
            chart_view,
            expanded && gpu_inputs.is_some_and(|inputs| !inputs.series.is_empty()),
            &mut animation_driver_claimed,
        );
        sections.push(gpu_section(
            gpu,
            gpu_inputs,
            chart_inputs.generation,
            expanded,
            gpu_view,
        ));
    }

    let any_expanded =
        vis.show_cpu || vis.show_memory || snap.gpus.iter().any(|g| vis.is_gpu_visible(&g.pci_id));
    // Flex only when at least one body is open; otherwise headers just stack.
    let use_fill = chart_view.flex && any_expanded;

    let col = column(sections)
        .spacing(SECTION_GAP)
        .width(Length::Fill)
        .height(if use_fill {
            Length::Fill
        } else {
            Length::Shrink
        });

    if use_fill {
        col.into()
    } else {
        scrollable(col)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

fn claim_animation_driver(
    mut chart_view: ChartView,
    eligible: bool,
    driver_claimed: &mut bool,
) -> ChartView {
    chart_view.animate = chart_view.animate && eligible && !*driver_claimed;
    *driver_claimed |= chart_view.animate;
    chart_view
}

/// Subscription function: 100ms display tick.
pub fn subscription(_app: &Lightwatch) -> Subscription<Message> {
    iced::time::every(DISPLAY_INTERVAL).map(Message::DisplayTick)
}

/// Convert iced's scheduled display deadline into the sample clock domain.
///
/// This is anchored afresh on every tick: `Instant` excludes suspend on Linux,
/// while `CLOCK_BOOTTIME` includes it. Subtracting only callback lateness keeps
/// ordinary X steps even without erasing a real suspend-sized jump.
fn chart_boottime_ns(deadline: Instant, now: Instant, boot_now_ns: u64) -> u64 {
    if now >= deadline {
        boot_now_ns.saturating_sub(duration_ns_u64(now.duration_since(deadline)))
    } else {
        boot_now_ns.saturating_add(duration_ns_u64(deadline.duration_since(now)))
    }
}

fn duration_ns_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Theme function
pub fn theme(_app: &Lightwatch) -> Theme {
    Theme::Dark
}

// ---------------------------------------------------------------------------
// GSM-style per-section disclosure (triangle next to section title)
// ---------------------------------------------------------------------------

/// Leading ▾ / ▸ control — toggles section body open/closed.
fn disclosure_button(expanded: bool, id: SectionId) -> Element<'static, Message> {
    let mark = if expanded { "▾" } else { "▸" };
    button(text(mark).size(14).color(theme::TEXT_DIM))
        .padding([0, 4])
        .style(iced::widget::button::text)
        .on_press(Message::ToggleSection(id))
        .into()
}

/// Prefix a section header row with the disclosure triangle.
fn with_disclosure<'a>(
    expanded: bool,
    id: SectionId,
    rest: Element<'a, Message>,
) -> Element<'a, Message> {
    row![disclosure_button(expanded, id), rest]
        .spacing(4)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
}

// ---------------------------------------------------------------------------
// panel helper — surface card with border, rounded corners, padding
// ---------------------------------------------------------------------------

fn panel<'a>(
    header: Element<'a, Message>,
    body: Option<Element<'a, Message>>,
    expanded: bool,
    flex: bool,
) -> Element<'a, Message> {
    let fill = expanded && flex;
    let inner = if let Some(body) = body {
        column![header, body]
            .spacing(4)
            .height(if fill { Length::Fill } else { Length::Shrink })
    } else {
        // Collapsed: header only.
        column![header].height(Length::Shrink)
    };
    let mut c = container(inner)
        .padding(if expanded { 6 } else { 4 })
        .style(|_theme| container::Style {
            background: Some(iced::Background::Color(theme::SURFACE)),
            border: iced::Border {
                color: theme::BORDER,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        });
    if fill {
        c = c.height(Length::Fill);
    }
    c.into()
}

fn chart_height(flex: bool, min: f32) -> Length {
    if flex {
        Length::Fill
    } else {
        Length::Fixed(min)
    }
}

/// Expanded sections share equal FillPortion; collapsed are Shrink (header only).
fn section_portion(expanded: bool, flex: bool) -> Length {
    if expanded && flex {
        Length::FillPortion(WEIGHT_SECTION)
    } else {
        Length::Shrink
    }
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

fn cpu_section<'a>(
    snap: &'a Snapshot,
    chart_inputs: &'a ChartInputs,
    expanded: bool,
    chart_view: ChartView,
) -> Element<'a, Message> {
    let cpu = &snap.cpu;
    let usage = rfmt(&cpu.usage_percent, |v| format!("{v:.1}%"));
    let temp = rfmt_opt(&cpu.temp_celsius, |v| format!("{v:.1}°C"));
    let freq = rfmt_opt(&cpu.freq_mhz, |v| format!("{v:.0} MHz"));

    // GSM: ▾/▸ + title + live summary (summary stays when collapsed).
    let header_rest = row![
        section_label("CPU"),
        Space::new().width(8),
        text(usage).size(14).color(theme::ACCENT_CPU),
        Space::new().width(12),
        text(temp).size(12).color(theme::TEXT),
        Space::new().width(12),
        text(freq).size(12).color(theme::TEXT),
        Space::new().width(Length::Fill),
        text(chart_inputs.cpu_count_label.as_str())
            .size(11)
            .color(theme::TEXT_DIM),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);
    let header = with_disclosure(expanded, SectionId::Cpu, header_rest.into());

    let body = if expanded {
        let mut chart = MultiChart::new(
            ChartId::Cpu,
            chart_inputs.generation,
            chart_inputs.cpu_content_signature,
            &chart_inputs.cpu_series,
            AxisKind::Percent,
            true,
        );
        chart.window = chart_view.window();
        chart.animate = chart_view.animate;

        let canvas = Canvas::new(chart)
            .width(Length::Fill)
            .height(chart_height(chart_view.flex, MIN_CPU_CHART));

        let n = chart_inputs.cpu_ids.len();
        let per_col = n.div_ceil(4).max(1);
        let mut legend_cols: Vec<Element<Message>> = Vec::with_capacity(4);
        for col_idx in 0..4 {
            let start = col_idx * per_col;
            let end = ((col_idx + 1) * per_col).min(n);
            if start >= n {
                legend_cols.push(column![Space::new().height(1)].width(Length::Fill).into());
                continue;
            }
            let items: Vec<Element<Message>> = chart_inputs.cpu_ids[start..end]
                .iter()
                .zip(&chart_inputs.cpu_legend_labels[start..end])
                .map(|(core_id, label)| legend_chip_fixed(label, theme::core_color(core_id.0)))
                .collect();
            legend_cols.push(column(items).spacing(2).width(Length::Fill).into());
        }
        let legend = row(legend_cols).spacing(8).width(Length::Fill);
        Some(
            column![canvas, legend]
                .spacing(4)
                .height(if chart_view.flex {
                    Length::Fill
                } else {
                    Length::Shrink
                })
                .into(),
        )
    } else {
        None
    };

    let card = panel(header, body, expanded, chart_view.flex);
    container(card)
        .width(Length::Fill)
        .height(section_portion(expanded, chart_view.flex))
        .into()
}

fn memory_section<'a>(
    snap: &'a Snapshot,
    chart_inputs: &'a ChartInputs,
    expanded: bool,
    chart_view: ChartView,
) -> Element<'a, Message> {
    let mem = &snap.memory;
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

    let header_rest = row![
        section_label("Memory"),
        Space::new().width(Length::Fill),
        text(format!("Used {used}"))
            .size(12)
            .color(theme::ACCENT_MEM),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);
    let header = with_disclosure(expanded, SectionId::Memory, header_rest.into());

    let body = if expanded {
        let stats = row![
            tooltip(
                stat_box("Used", used, theme::ACCENT_MEM),
                container(text("Used = Total − Available").size(11).color(theme::TEXT))
                    .padding([4, 6])
                    .style(container::rounded_box),
                tooltip::Position::Bottom,
            ),
            Space::new().width(6),
            stat_box("Avail", avail, theme::ACCENT_MEM),
            Space::new().width(6),
            stat_box("Swap", swap, theme::ACCENT_SWAP),
            Space::new().width(6),
            stat_box("Load", load, theme::ACCENT_LOAD),
        ]
        .spacing(0);

        let mut chart = MultiChart::new(
            ChartId::Memory,
            chart_inputs.generation,
            chart_inputs.memory_content_signature,
            &chart_inputs.memory_series,
            AxisKind::Percent,
            false,
        );
        chart.window = chart_view.window();
        chart.animate = chart_view.animate;

        let canvas = Canvas::new(chart)
            .width(Length::Fill)
            .height(chart_height(chart_view.flex, MIN_MEM_CHART));

        Some(
            column![stats, canvas]
                .spacing(4)
                .height(if chart_view.flex {
                    Length::Fill
                } else {
                    Length::Shrink
                })
                .into(),
        )
    } else {
        None
    };

    let card = panel(header, body, expanded, chart_view.flex);
    container(card)
        .width(Length::Fill)
        .height(section_portion(expanded, chart_view.flex))
        .into()
}

fn gpu_section<'a>(
    gpu: &'a GpuSnapshot,
    gpu_inputs: Option<&'a GpuChartInputs>,
    generation: u64,
    expanded: bool,
    chart_view: ChartView,
) -> Element<'a, Message> {
    let util = rfmt(&gpu.util_percent, |v| format!("{v:5.1}%"));
    let vram = match (&gpu.vram_used_kb, &gpu.vram_total_kb) {
        (Reading::Value(u), Reading::Value(t)) => {
            let pct = if *t > 0 {
                *u as f64 / *t as f64 * 100.0
            } else {
                0.0
            };
            format!("{pct:3.0}% ({:4.0} MiB)", *u as f64 / 1024.0)
        }
        _ => "--".into(),
    };
    let temp = rfmt_opt(&gpu.temp_celsius, |v| format!("{v:5.1}°C"));
    let power = rfmt_opt(&gpu.power_watts, |v| format!("{v:5.1} W"));

    let title = format!("{} -- {}", gpu.name, gpu.pci_id);
    let util_summary = rfmt(&gpu.util_percent, |v| format!("{v:.0}%"));

    let header_rest = row![
        text(title).size(13).color(theme::TEXT),
        Space::new().width(Length::Fill),
        text(util_summary).size(12).color(theme::ACCENT_GPU),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);
    let header = with_disclosure(
        expanded,
        SectionId::Gpu(gpu.pci_id.clone()),
        header_rest.into(),
    );

    let body = if expanded {
        let stats = row![
            stat_box_fixed("Util", util, theme::ACCENT_GPU, 64.0),
            Space::new().width(6),
            stat_box_fixed("VRAM", vram, theme::ACCENT_GPU, 110.0),
            Space::new().width(6),
            stat_box_fixed("Temp", temp, theme::ACCENT_TEMP, 64.0),
            Space::new().width(6),
            stat_box_fixed("Power", power, theme::ACCENT_WARN, 64.0),
        ]
        .spacing(0);

        let mut body_col = column![stats].spacing(4);
        if let Some(inputs) = gpu_inputs {
            let mut chart = MultiChart::new(
                ChartId::Gpu(Arc::clone(&inputs.pci_id)),
                generation,
                inputs.content_signature,
                &inputs.series,
                AxisKind::Percent,
                false,
            );
            chart.window = chart_view.window();
            chart.animate = chart_view.animate;
            let canvas = Canvas::new(chart)
                .width(Length::Fill)
                .height(chart_height(chart_view.flex, MIN_GPU_CHART));
            body_col = body_col.push(canvas);
        }
        Some(
            body_col
                .height(if chart_view.flex {
                    Length::Fill
                } else {
                    Length::Shrink
                })
                .into(),
        )
    } else {
        None
    };

    let card = panel(header, body, expanded, chart_view.flex);
    container(card)
        .width(Length::Fill)
        .height(section_portion(expanded, chart_view.flex))
        .into()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn section_label(text_str: &str) -> Element<'static, Message> {
    text(text_str.to_owned()).size(13).color(theme::TEXT).into()
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
    .padding([2, 4])
    .style(move |_theme| container::Style {
        background: Some(iced::Background::Color(theme::with_alpha(theme::BG, 0.6))),
        border: iced::Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 3.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// Like [`stat_box`] but with a fixed outer width so digit-width changes
/// (e.g. GPU Util `0.0%` → `60.0%`) do not shift neighboring chips.
fn stat_box_fixed(
    label: &str,
    value: String,
    color: Color,
    width: f32,
) -> Element<'static, Message> {
    container(
        column![
            text(label.to_owned()).size(10).color(theme::TEXT_DIM),
            text(value).size(13).color(color),
        ]
        .spacing(1),
    )
    .padding([2, 4])
    .width(Length::Fixed(width))
    .style(move |_theme| container::Style {
        background: Some(iced::Background::Color(theme::with_alpha(theme::BG, 0.6))),
        border: iced::Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 3.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// Full-width legend chip for the 4-column CPU legend — fills its column so
/// digit-width changes in the percentage do not shove neighboring chips.
fn legend_chip_fixed<'a>(label: &'a str, color: Color) -> Element<'a, Message> {
    let swatch = container(Space::new().width(8).height(8)).style(move |_theme| container::Style {
        background: Some(iced::Background::Color(color)),
        ..Default::default()
    });
    row![
        swatch,
        Space::new().width(6),
        text(label).size(11).color(theme::TEXT_DIM),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into()
}

// ---------------------------------------------------------------------------
// Display-clock tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod process_selection_tests {
    use super::*;

    #[test]
    fn profile_control_is_sticky_replaceable_and_clearable() {
        let first = ProcessId {
            pid: 10,
            starttime: 100,
        };
        let second = ProcessId {
            pid: 11,
            starttime: 200,
        };
        let control = Mutex::new(None);
        let mut profiled = None;

        set_profile_target(&mut profiled, &control, Some(first));
        assert_eq!(profiled, Some(first));
        assert_eq!(*control.lock().unwrap(), Some(first));

        set_profile_target(&mut profiled, &control, Some(second));
        assert_eq!(profiled, Some(second));
        assert_eq!(*control.lock().unwrap(), Some(second));

        set_profile_target(&mut profiled, &control, None);
        assert_eq!(profiled, None);
        assert_eq!(*control.lock().unwrap(), None);
    }

    #[test]
    fn nice_labels_explain_relative_cpu_priority() {
        assert_eq!(nice_label(-5), "Higher (-5)");
        assert_eq!(nice_label(0), "Normal (0)");
        assert_eq!(nice_label(10), "Lower (+10)");
    }
}

#[cfg(test)]
mod display_clock_tests {
    use super::*;

    const BOOT_BASE_NS: u64 = 1_000_000_000_000;

    #[test]
    fn scheduled_deadline_converts_on_time_late_and_early() {
        let now = Instant::now();

        assert_eq!(chart_boottime_ns(now, now, BOOT_BASE_NS), BOOT_BASE_NS);
        assert_eq!(
            chart_boottime_ns(now - Duration::from_millis(37), now, BOOT_BASE_NS,),
            BOOT_BASE_NS - 37_000_000,
        );
        assert_eq!(
            chart_boottime_ns(now + Duration::from_millis(25), now, BOOT_BASE_NS,),
            BOOT_BASE_NS + 25_000_000,
        );
    }

    #[test]
    fn callback_lateness_does_not_change_regular_chart_steps() {
        let first_deadline = Instant::now();
        let first_now = first_deadline + Duration::from_millis(13);
        let first = chart_boottime_ns(first_deadline, first_now, BOOT_BASE_NS + 13_000_000);

        let second_deadline = first_deadline + DISPLAY_INTERVAL;
        let second_now = second_deadline + Duration::from_millis(47);
        let second = chart_boottime_ns(second_deadline, second_now, BOOT_BASE_NS + 147_000_000);

        assert_eq!(second - first, 100_000_000);
    }

    #[test]
    fn boottime_suspend_jump_is_preserved() {
        let first_deadline = Instant::now();
        let first_now = first_deadline + Duration::from_millis(10);
        let first = chart_boottime_ns(first_deadline, first_now, BOOT_BASE_NS + 10_000_000);

        let second_deadline = first_deadline + DISPLAY_INTERVAL;
        let second_now = second_deadline + Duration::from_millis(10);
        let second = chart_boottime_ns(second_deadline, second_now, BOOT_BASE_NS + 10_110_000_000);

        assert_eq!(second - first, 10_100_000_000);
    }

    #[test]
    fn skipped_deadlines_advance_once_by_the_full_gap() {
        let first_deadline = Instant::now();
        let first_now = first_deadline + Duration::from_millis(5);
        let first = chart_boottime_ns(first_deadline, first_now, BOOT_BASE_NS + 5_000_000);

        let next_deadline = first_deadline + Duration::from_millis(300);
        let next_now = next_deadline + Duration::from_millis(40);
        let next = chart_boottime_ns(next_deadline, next_now, BOOT_BASE_NS + 340_000_000);

        assert_eq!(next - first, 300_000_000);
    }

    #[test]
    fn compositor_animation_is_resources_only_and_requires_live_visible_work() {
        assert!(resource_chart_animation_active(
            AppTab::Resources,
            true,
            true
        ));
        assert!(!resource_chart_animation_active(
            AppTab::Processes,
            true,
            true
        ));
        assert!(!resource_chart_animation_active(
            AppTab::Resources,
            false,
            true
        ));
        assert!(!resource_chart_animation_active(
            AppTab::Resources,
            true,
            false
        ));
    }

    #[test]
    fn exactly_one_eligible_canvas_drives_window_redraws() {
        let view = ChartView {
            window_secs: 60.0,
            window_end_ns: 1_000,
            interval_ns: 1_000_000_000,
            flex: true,
            animate: true,
        };
        let mut claimed = false;

        assert!(!claim_animation_driver(view, false, &mut claimed).animate);
        assert!(claim_animation_driver(view, true, &mut claimed).animate);
        assert!(claimed);
        assert!(!claim_animation_driver(view, true, &mut claimed).animate);

        let mut disabled_claimed = false;
        let disabled = ChartView {
            animate: false,
            ..view
        };
        assert!(!claim_animation_driver(disabled, true, &mut disabled_claimed).animate);
        assert!(!disabled_claimed);
    }

    #[test]
    fn content_signature_tracks_membership_scale_and_style() {
        let points: Arc<[SamplePoint]> = Arc::from([
            SamplePoint::new(1_000_000_000, 10.0),
            SamplePoint::new(2_000_000_000, 20.0),
        ]);
        let mut series = vec![SeriesData {
            points,
            color: Color::from_rgb(0.2, 0.4, 0.6),
            max_value: 100.0,
            fill: false,
            line_alpha: Some(0.8),
        }];
        let original = chart_content_signature(&series, [0]);

        assert_ne!(original, chart_content_signature(&series, [1]));
        series[0].max_value = 200.0;
        assert_ne!(original, chart_content_signature(&series, [0]));
        series[0].max_value = 100.0;
        series[0].fill = true;
        assert_ne!(original, chart_content_signature(&series, [0]));
    }
}

// ---------------------------------------------------------------------------
// Layout tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn min_height_all_collapsed_is_headers_only() {
        let plan = LayoutPlan {
            cpu_expanded: false,
            memory_expanded: false,
            gpu_expanded: 0,
            gpu_collapsed: 0,
            cpu_cores: 16,
        };
        // CPU + Memory headers + one gap.
        let h = plan.min_content_height();
        assert!((h - (COLLAPSED_HEADER_H * 2.0 + SECTION_GAP)).abs() < 0.1);
        assert!(use_flex(h, &plan));
    }

    #[test]
    fn min_height_cpu_only_scales_with_cores() {
        let small = LayoutPlan {
            cpu_expanded: true,
            memory_expanded: false,
            gpu_expanded: 0,
            gpu_collapsed: 0,
            cpu_cores: 4,
        };
        let big = LayoutPlan {
            cpu_expanded: true,
            memory_expanded: false,
            gpu_expanded: 0,
            gpu_collapsed: 0,
            cpu_cores: 16,
        };
        assert!(big.min_content_height() > small.min_content_height());
        assert!(!use_flex(50.0, &small));
        assert!(use_flex(small.min_content_height(), &small));
    }

    #[test]
    fn min_height_full_dashboard() {
        let plan = LayoutPlan {
            cpu_expanded: true,
            memory_expanded: true,
            gpu_expanded: 2,
            gpu_collapsed: 0,
            cpu_cores: 16,
        };
        let h = plan.min_content_height();
        // Sanity: four expanded panels need a few hundred px at mins.
        assert!(h > 400.0);
        assert!(!use_flex(h - 1.0, &plan));
        assert!(use_flex(h, &plan));
    }
}
