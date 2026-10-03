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
/// Hint / placeholder text of an empty field.
///
/// A third tier below [`TEXT_DIM`] on purpose: an empty field shows its hint
/// where a value would be, and on a television a hint only 40% dimmer than the
/// text is read as a value that is already in the box.
pub const TEXT_HINT: Color32 = Color32::from_rgb(92, 101, 116);

/// Fill of a control the remote control is on.
///
/// Deliberately not the accent. Two different things have to be told apart at a
/// glance from a couch, and they can happen at once:
///
/// * a control that is *on* - the layout in use, a panel that is open - which
///   carries the accent fill, and
/// * the control the remote control is *on*, which carries this fill and the
///   cyan ring.
///
/// Sharing one colour between them is what made a focused button look switched
/// on, and a switched-on button look focused.
pub const FOCUS_FILL: Color32 = Color32::from_rgb(56, 66, 82);

/// Spacing scale, in points.
///
/// Every gap in the interface comes from these four values. A layout built
/// from loose numbers - `10.0` here, `7.0` there - is what makes a screen look
/// assembled rather than drawn, and it is also what makes changing the density
/// later impossible.
pub mod space {
    /// Inside one control: the gap between an icon and its label, a chip's pad.
    pub const XS: f32 = 4.0;
    /// Between two controls of the same group.
    pub const S: f32 = 8.0;
    /// Between two groups of controls.
    pub const M: f32 = 12.0;
    /// Between two regions, and along a panel's inner edge.
    pub const L: f32 = 16.0;
}

/// Corner radii, in points.
pub mod radius {
    /// Chips, badges, the strip behind an overlay.
    pub const S: u8 = 4;
    /// Tiles, buttons, fields.
    pub const M: u8 = 6;
    /// Windows and panels.
    pub const L: u8 = 10;
}

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
    // The stroke of a selected control is its *text*, not a border: egui hands
    // `selection.stroke` to the widget as `fg_stroke`. The focus ring is a
    // different thing and comes from `widgets.active` below.
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::from_rgb(228, 242, 255));
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, TILE_BORDER);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(31, 36, 45);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 52, 64);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.6));
    // A remote control acts on whatever has the keyboard focus, so that is the
    // one state that must be unmistakable from a couch. egui paints
    // `widgets.active` for a widget that has focus - `WidgetVisuals::style`
    // treats `has_focus` like a held pointer button - which is therefore where
    // the ring goes. Its fill stays neutral so that it cannot be mistaken for
    // the accent of a control that is on; see [`FOCUS_FILL`].
    visuals.widgets.active.bg_fill = FOCUS_FILL;
    visuals.widgets.active.weak_bg_fill = FOCUS_FILL;
    visuals.widgets.active.bg_stroke = Stroke::new(FOCUS_WIDTH, FOCUS);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(space::S, space::S);
    style.spacing.button_padding = egui::vec2(space::M, space::XS + 2.0);
    style.spacing.slider_width = 140.0;
    ctx.set_style(style);
}

/// A text field's hint, in [`TEXT_HINT`].
///
/// The colour has to be put on the text itself: a plain `&str` hint is laid
/// out with [`egui::Visuals::override_text_color`] baked in - the normal text
/// colour - and the weaker colour [`egui::TextEdit`] would otherwise ask for
/// is then ignored, because a galley that already carries a colour is painted
/// as it is.
pub fn hint(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(TEXT_HINT)
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
