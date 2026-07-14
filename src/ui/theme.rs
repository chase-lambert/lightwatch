use iced::Color;

use crate::model::history::CORE_PALETTE_LEN;

// Color palette for the lightwatch dashboard (dark theme).
pub const BG: Color = Color::from_rgb(0.08, 0.08, 0.10);
pub const SURFACE: Color = Color::from_rgb(0.12, 0.12, 0.15);
pub const TEXT: Color = Color::from_rgb(0.85, 0.85, 0.85);
pub const TEXT_DIM: Color = Color::from_rgb(0.50, 0.50, 0.55);
pub const ACCENT_CPU: Color = Color::from_rgb(0.30, 0.70, 0.95);
pub const ACCENT_MEM: Color = Color::from_rgb(0.40, 0.80, 0.40);
pub const ACCENT_SWAP: Color = Color::from_rgb(0.95, 0.55, 0.20);
pub const ACCENT_LOAD: Color = Color::from_rgb(0.85, 0.45, 0.65);
pub const ACCENT_SELF: Color = Color::from_rgb(0.70, 0.70, 0.30);
pub const ACCENT_GPU: Color = Color::from_rgb(0.35, 0.75, 0.60);
pub const ACCENT_TEMP: Color = Color::from_rgb(0.90, 0.40, 0.35);
pub const ACCENT_FREQ: Color = Color::from_rgb(0.50, 0.60, 0.90);
pub const ACCENT_WARN: Color = Color::from_rgb(0.95, 0.65, 0.15);

/// Palette of 16 distinct colors for per-core CPU series.
/// Chosen for perceptual distinctness on dark backgrounds.
pub const CPU_CORE_PALETTE: [Color; CORE_PALETTE_LEN] = [
    Color::from_rgb(0.30, 0.70, 0.95), // blue
    Color::from_rgb(0.95, 0.55, 0.20), // orange
    Color::from_rgb(0.40, 0.80, 0.40), // green
    Color::from_rgb(0.85, 0.45, 0.65), // magenta
    Color::from_rgb(0.50, 0.60, 0.90), // periwinkle
    Color::from_rgb(0.90, 0.65, 0.15), // amber
    Color::from_rgb(0.35, 0.75, 0.60), // teal
    Color::from_rgb(0.90, 0.40, 0.35), // salmon
    Color::from_rgb(0.60, 0.50, 0.90), // purple
    Color::from_rgb(0.75, 0.75, 0.30), // yellow
    Color::from_rgb(0.50, 0.85, 0.75), // mint
    Color::from_rgb(0.85, 0.60, 0.70), // rose
    Color::from_rgb(0.45, 0.70, 0.45), // forest
    Color::from_rgb(0.80, 0.75, 0.50), // sand
    Color::from_rgb(0.65, 0.50, 0.75), // amethyst
    Color::from_rgb(0.55, 0.80, 0.90), // sky
];

/// Get the color for a given core id (stable mapping).
pub fn core_color(core_id: u32) -> Color {
    CPU_CORE_PALETTE[(core_id as usize) % CPU_CORE_PALETTE.len()]
}
