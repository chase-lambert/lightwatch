use super::graph::{MultiChart, SeriesData};
use super::graph_geom::DrawWindow;
use super::prefs::{self, SectionId, SectionVisibility};
use super::theme;
use crate::collect::proc::{self as proc_collect, KillOutcome};
use crate::model::*;
use crate::sample::latest::{Latest, Published};
use crate::sample::worker::Sampler;
use iced::widget::{
    Canvas, Space, button, column, container, mouse_area, responsive, row, scrollable, text,
    text_input, tooltip,
};
use iced::{Alignment, Color, Element, Length, Size, Subscription, Theme};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

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
    published: Option<Arc<Published>>,
    last_seen_gen: u64,
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
    selected_process: Option<ProcessId>,
    /// Soft status under End Process (signal sent / denied / gone).
    process_status: Option<String>,
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
    ToggleSection(SectionId),
    SelectTab(AppTab),
    ProcessQueryChanged(String),
    SortProcesses(ProcessSortKey),
    SelectProcess(ProcessId),
    EndProcess,
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
        visibility: prefs::load_ui_prefs(),
        active_tab: AppTab::Resources,
        process_query: String::new(),
        process_sort: ProcessSortKey::Memory,
        process_sort_desc: true,
        selected_process: None,
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
        Message::SampleArrived => {
            while app.notify_rx.try_recv().is_ok() {}
            if let Some((g, pubd)) = app.latest.pull_if_newer(app.last_seen_gen) {
                app.last_seen_gen = g;
                // Clear selection if it is no longer in the *visible* table
                // (left the set, filtered out, or fell outside the display cap).
                if let Some(sel) = app.selected_process
                    && !selection_is_visible(
                        &pubd.snapshot.processes,
                        &app.process_query,
                        app.process_sort,
                        app.process_sort_desc,
                        sel,
                    )
                {
                    app.selected_process = None;
                }
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
            clear_selection_if_not_visible(app);
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
            clear_selection_if_not_visible(app);
            iced::Task::none()
        }
        Message::SelectProcess(id) => {
            app.selected_process = Some(id);
            app.process_status = None;
            iced::Task::none()
        }
        Message::EndProcess => {
            if let Some(id) = app.selected_process {
                let outcome = proc_collect::end_process(Path::new("/proc"), id);
                app.process_status = Some(kill_status_text(&outcome));
                // Clear selection when gone/mismatched, or after a successful
                // root redirect (the helper may linger briefly as a zombie view).
                if matches!(
                    outcome,
                    KillOutcome::Gone
                        | KillOutcome::IdentityMismatch
                        | KillOutcome::SignalSentToRoot { .. }
                ) {
                    app.selected_process = None;
                }
            }
            iced::Task::none()
        }
    }
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

/// Drop selection when it is not in the current visible slice (auth boundary
/// for End Process is the highlighted visible row).
fn clear_selection_if_not_visible(app: &mut Lightwatch) {
    let Some(sel) = app.selected_process else {
        return;
    };
    let Some(pubd) = app.published.as_ref() else {
        app.selected_process = None;
        return;
    };
    if !selection_is_visible(
        &pubd.snapshot.processes,
        &app.process_query,
        app.process_sort,
        app.process_sort_desc,
        sel,
    ) {
        app.selected_process = None;
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
    // One chrome row: tabs left, tab-specific control (presets / search) right.
    let chrome = tab_chrome(app, snap);

    let body: Element<'_, Message> = match app.active_tab {
        AppTab::Resources => resources_body(app, snap, &published.history),
        AppTab::Processes => processes_body(app, snap),
        AppTab::Health => health_body(),
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
fn tab_chrome<'a>(app: &'a Lightwatch, snap: &'a Snapshot) -> Element<'a, Message> {
    let tabs = tab_buttons(app.active_tab);
    let trailing: Element<'a, Message> = match app.active_tab {
        AppTab::Resources => history_presets(app),
        AppTab::Processes => {
            let match_count = visible_processes(
                &snap.processes,
                &app.process_query,
                app.process_sort,
                app.process_sort_desc,
            )
            .match_count;
            process_search_trailing(app, match_count)
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

    row![
        text(count_label).size(11).color(theme::TEXT_DIM),
        search,
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn resources_body<'a>(
    app: &'a Lightwatch,
    snap: &'a Snapshot,
    hist: &'a History,
) -> Element<'a, Message> {
    let window_secs = app.config.window.as_secs_f64();
    // Two-interval diagnostic look-ahead: chart "now" lags wall clock by two
    // sample intervals so the next two real samples sit off-screen right and
    // scroll in with immutable spline geometry (no re-fitting at reveal).
    let interval_ns = app.config.interval.as_nanos() as u64;
    let delay_ns = interval_ns.saturating_mul(2);
    let window_end_ns = crate::clock_boottime_ns().saturating_sub(delay_ns);

    let gpu_expanded = snap
        .gpus
        .iter()
        .filter(|g| app.visibility.is_gpu_visible(&g.pci_id))
        .count();
    let gpu_collapsed = snap.gpus.len().saturating_sub(gpu_expanded);
    let layout_plan = LayoutPlan {
        cpu_expanded: app.visibility.show_cpu,
        memory_expanded: app.visibility.show_memory,
        gpu_expanded,
        gpu_collapsed,
        cpu_cores: hist.cpu_per_core.len(),
    };

    let vis = app.visibility.clone();
    responsive(move |size: Size| {
        let flex = use_flex(size.height, &layout_plan);
        build_sections(
            snap,
            hist,
            window_secs,
            window_end_ns,
            interval_ns,
            &vis,
            flex,
        )
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn health_body() -> Element<'static, Message> {
    container(
        column![
            text("System health").size(16).color(theme::TEXT),
            text("Reserved for a later pass.")
                .size(12)
                .color(theme::TEXT_DIM),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .into()
}

// Column flex portions for the process table (sum = 100).
const COL_NAME: u16 = 28;
const COL_CPU: u16 = 12;
const COL_MEM: u16 = 14;
const COL_DREAD: u16 = 16;
const COL_DWRITE: u16 = 16;
const COL_PID: u16 = 14;

fn processes_body<'a>(app: &'a Lightwatch, snap: &'a Snapshot) -> Element<'a, Message> {
    let visible = visible_processes(
        &snap.processes,
        &app.process_query,
        app.process_sort,
        app.process_sort_desc,
    );

    // Search lives in the shared tab chrome row; body starts at the table.
    let headers = process_header_row(app.process_sort, app.process_sort_desc);

    let selected = app.selected_process;
    let body_rows: Vec<Element<Message>> = visible
        .rows
        .iter()
        .map(|r| process_row(r, selected == Some(r.id)))
        .collect();

    let list = scrollable(column(body_rows).spacing(1).width(Length::Fill))
        .height(Length::Fill)
        .width(Length::Fill);

    let end_btn = {
        let label = text("End Process").size(12);
        let mut btn = button(label).padding([6, 12]);
        // Only arm when selection is currently visible (re-checked each frame).
        let armed = app.selected_process.is_some_and(|sel| {
            visible.rows.iter().any(|r| r.id == sel)
        });
        if armed {
            btn = btn
                .style(iced::widget::button::danger)
                .on_press(Message::EndProcess);
        }
        btn
    };

    let status = text(
        app.process_status.clone().unwrap_or_else(|| {
            if let Some(sel) = app.selected_process {
                if let Some(r) = visible.rows.iter().find(|r| r.id == sel) {
                    format!(
                        "selected {} ({}) — End Process sends SIGTERM",
                        r.name, r.id.pid
                    )
                } else {
                    "select a process".into()
                }
            } else {
                "select a process".into()
            }
        }),
    )
    .size(11)
    .color(theme::TEXT_DIM);

    let footer = row![end_btn, Space::new().width(12), status]
        .spacing(4)
        .align_y(Alignment::Center)
        .padding([4, 2]);

    column![headers, list, footer]
        .spacing(4)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
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

fn process_row(row: &ProcessRow, selected: bool) -> Element<'static, Message> {
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
    let name = {
        const NAME_MAX: usize = 48;
        let mut n = row.name.clone();
        if n.chars().count() > NAME_MAX {
            n = n.chars().take(NAME_MAX - 1).collect::<String>() + "…";
        }
        n
    };

    let fg = if selected {
        Color::WHITE
    } else {
        theme::TEXT
    };
    let dim = if selected {
        Color::WHITE
    } else {
        theme::TEXT_DIM
    };

    let cells = row![
        text(name).size(12).color(fg).width(Length::FillPortion(COL_NAME)),
        text(cpu)
            .size(12)
            .color(fg)
            .width(Length::FillPortion(COL_CPU)),
        text(mem)
            .size(12)
            .color(fg)
            .width(Length::FillPortion(COL_MEM)),
        text(dread)
            .size(12)
            .color(dim)
            .width(Length::FillPortion(COL_DREAD)),
        text(dwrite)
            .size(12)
            .color(dim)
            .width(Length::FillPortion(COL_DWRITE)),
        text(row.id.pid.to_string())
            .size(12)
            .color(dim)
            .width(Length::FillPortion(COL_PID)),
    ]
    .spacing(2)
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .padding([3, 4]);

    let bg = if selected {
        theme::with_alpha(theme::ACCENT_CPU, 0.35)
    } else {
        Color::TRANSPARENT
    };

    let id = row.id;
    mouse_area(
        container(cells)
            .width(Length::Fill)
            .style(move |_t| container::Style {
                background: Some(iced::Background::Color(bg)),
                border: iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
    )
    .on_press(Message::SelectProcess(id))
    .into()
}

fn build_sections<'a>(
    snap: &'a Snapshot,
    hist: &'a History,
    window_secs: f64,
    window_end_ns: u64,
    interval_ns: u64,
    vis: &SectionVisibility,
    flex: bool,
) -> Element<'a, Message> {
    let mut sections: Vec<Element<'a, Message>> = Vec::new();

    // Headers always present (GSM); body only when expanded.
    sections.push(cpu_section(
        snap,
        hist,
        window_secs,
        window_end_ns,
        interval_ns,
        flex,
        vis.show_cpu,
    ));
    sections.push(memory_section(
        snap,
        hist,
        window_secs,
        window_end_ns,
        interval_ns,
        flex,
        vis.show_memory,
    ));
    for gpu in snap.gpus.iter() {
        let expanded = vis.is_gpu_visible(&gpu.pci_id);
        let gpu_hist = hist.gpu_series.iter().find(|gh| gh.pci_id == gpu.pci_id);
        sections.push(gpu_section(
            gpu,
            gpu_hist,
            window_secs,
            window_end_ns,
            interval_ns,
            flex,
            expanded,
        ));
    }

    let any_expanded =
        vis.show_cpu || vis.show_memory || snap.gpus.iter().any(|g| vis.is_gpu_visible(&g.pci_id));
    // Flex only when at least one body is open; otherwise headers just stack.
    let use_fill = flex && any_expanded;

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

/// Subscription function: 100ms display tick.
pub fn subscription(_app: &Lightwatch) -> Subscription<Message> {
    iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::SampleArrived)
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

fn cpu_section(
    snap: &Snapshot,
    hist: &History,
    window_secs: f64,
    window_end_ns: u64,
    interval_ns: u64,
    flex: bool,
    expanded: bool,
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
        text(format!("{}{}", core_count, hidden_text))
            .size(11)
            .color(theme::TEXT_DIM),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);
    let header = with_disclosure(expanded, SectionId::Cpu, header_rest.into());

    let body = if expanded {
        let mut chart = MultiChart::new(true);
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
            sample_interval_ns: interval_ns,
            window_secs,
            window_end_ns,
        };

        let canvas = Canvas::new(chart)
            .width(Length::Fill)
            .height(chart_height(flex, MIN_CPU_CHART));

        let cores = &hist.cpu_per_core;
        let n = cores.len();
        let per_col = n.div_ceil(4).max(1);
        let mut legend_cols: Vec<Element<Message>> = Vec::with_capacity(4);
        for col_idx in 0..4 {
            let start = col_idx * per_col;
            let end = ((col_idx + 1) * per_col).min(n);
            if start >= n {
                legend_cols.push(column![Space::new().height(1)].width(Length::Fill).into());
                continue;
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
                            Reading::Value(v) => Some(format!("{v:3.0}%")),
                            _ => None,
                        })
                        .unwrap_or_else(|| "  —".to_string());
                    let color = theme::core_color(core_id.0);
                    let label = format!("{} {}", core_id.label(), pct_str);
                    legend_chip_fixed(&label, color)
                })
                .collect();
            legend_cols.push(column(items).spacing(2).width(Length::Fill).into());
        }
        let legend = row(legend_cols).spacing(8).width(Length::Fill);
        Some(
            column![canvas, legend]
                .spacing(4)
                .height(if flex { Length::Fill } else { Length::Shrink })
                .into(),
        )
    } else {
        None
    };

    let card = panel(header, body, expanded, flex);
    container(card)
        .width(Length::Fill)
        .height(section_portion(expanded, flex))
        .into()
}

fn memory_section(
    snap: &Snapshot,
    hist: &History,
    window_secs: f64,
    window_end_ns: u64,
    interval_ns: u64,
    flex: bool,
    expanded: bool,
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

        let mut chart = MultiChart::new(false);
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
            sample_interval_ns: interval_ns,
            window_secs,
            window_end_ns,
        };

        let canvas = Canvas::new(chart)
            .width(Length::Fill)
            .height(chart_height(flex, MIN_MEM_CHART));

        Some(
            column![stats, canvas]
                .spacing(4)
                .height(if flex { Length::Fill } else { Length::Shrink })
                .into(),
        )
    } else {
        None
    };

    let card = panel(header, body, expanded, flex);
    container(card)
        .width(Length::Fill)
        .height(section_portion(expanded, flex))
        .into()
}

fn gpu_section(
    gpu: &GpuSnapshot,
    gpu_hist: Option<&GpuHistory>,
    window_secs: f64,
    window_end_ns: u64,
    interval_ns: u64,
    flex: bool,
    expanded: bool,
) -> Element<'static, Message> {
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
                sample_interval_ns: interval_ns,
                window_secs,
                window_end_ns,
            };
            let canvas = Canvas::new(chart)
                .width(Length::Fill)
                .height(chart_height(flex, MIN_GPU_CHART));
            body_col = body_col.push(canvas);
        }
        Some(
            body_col
                .height(if flex { Length::Fill } else { Length::Shrink })
                .into(),
        )
    } else {
        None
    };

    let card = panel(header, body, expanded, flex);
    container(card)
        .width(Length::Fill)
        .height(section_portion(expanded, flex))
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
fn legend_chip_fixed(label: &str, color: Color) -> Element<'static, Message> {
    let label_owned = label.to_owned();
    let swatch = container(Space::new().width(8).height(8)).style(move |_theme| container::Style {
        background: Some(iced::Background::Color(color)),
        ..Default::default()
    });
    row![
        swatch,
        Space::new().width(6),
        text(label_owned).size(11).color(theme::TEXT_DIM),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into()
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
