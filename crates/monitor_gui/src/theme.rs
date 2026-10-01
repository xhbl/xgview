//! Colour palette and egui style used by the TV / desktop viewer.
//!
//! The palette is deliberately dark and high contrast: the application is meant
//! to be watched from a couch, on a large screen, in a dim living room.

use egui::{Color32, FontFamily, FontId, Stroke, TextStyle};

/// Application background.
pub const BACKGROUND: Color32 = Color32::from_rgb(10, 12, 16);
/// Panels (toolbar, settings, status bar).
pub const PANEL: Color32 = Color32::from_rgb(19, 22, 28);
/// Tile border.
pub const TILE_BORDER: Color32 = Color32::from_rgb(40, 46, 58);
/// Tile header strip.
pub const TILE_HEADER: Color32 = Color32::from_rgb(33, 39, 49);
/// Empty grid cell.
pub const TILE_EMPTY: Color32 = Color32::from_rgb(21, 24, 30);
/// Canvas of a tile that has no video yet.
pub const TILE_CANVAS: Color32 = Color32::from_rgb(14, 16, 21);
/// Canvas of a tile receiving a stream.
pub const CANVAS: Color32 = Color32::from_rgb(17, 21, 27);
/// Canvas of a channel that is connecting / reconnecting.
pub const CANVAS_PENDING: Color32 = Color32::from_rgb(14, 18, 26);
/// Canvas of a failed channel.
pub const CANVAS_FAILED: Color32 = Color32::from_rgb(30, 16, 18);
/// Video viewport of a channel that is streaming.
pub const CANVAS_LIVE: Color32 = Color32::from_rgb(20, 26, 34);

/// Focus ring of the selected viewport (bright cyan, 2 px, as required by the
/// remote control navigation specification).
pub const FOCUS: Color32 = Color32::from_rgb(0, 229, 255);
/// Focus ring width in points.
pub const FOCUS_WIDTH: f32 = 2.0;
/// Accent used by buttons and sub stream badges.
pub const ACCENT: Color32 = Color32::from_rgb(80, 190, 255);
/// Live indicator.
pub const LIVE: Color32 = Color32::from_rgb(74, 222, 128);
/// Warning (main stream badge, retries).
pub const WARN: Color32 = Color32::from_rgb(255, 183, 77);
/// Error / failed stream.
pub const ERROR: Color32 = Color32::from_rgb(255, 99, 99);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(227, 233, 240);
/// Secondary text.
pub const TEXT_DIM: Color32 = Color32::from_rgb(146, 158, 175);

/// Installs the dark theme and the larger TV friendly typography.
pub fn install(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = BACKGROUND;
    visuals.faint_bg_color = Color32::from_rgb(27, 31, 39);
    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.45);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, TILE_BORDER);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(31, 36, 45);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 52, 64);
    visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.5);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.slider_width = 140.0;
    ctx.set_style(style);
}

/// Truncates a label to `max` characters, adding an ellipsis when needed.
pub fn truncate(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
