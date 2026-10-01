//! Grid geometry and tile painting.
//!
//! The grid is drawn directly with an [`egui::Painter`]: every cell is a tile
//! showing the camera name, the stream currently decoded, the connection state
//! and the newest decoded picture, letterboxed into the tile. The same code
//! paints the outgoing page during a swipe, which is why painting and
//! interaction are decoupled: [`paint`] never mutates the application state, it
//! only reports what the user did.

use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Response, Sense, Stroke, StrokeKind, Ui, vec2};

use monitor_core::model::{CameraSource, ConnectionState, StreamKind};
use monitor_core::GridLayout;

use crate::app::ChannelUi;
use crate::theme;
use crate::video::VideoSurface;

/// Gap between two tiles, in points.
pub const GAP: f32 = 6.0;
/// Height of the tile header strip, in points.
pub const HEADER_HEIGHT: f32 = 26.0;

/// Channels visible on a page: one entry per grid cell (the vector length is
/// always the layout capacity, `None` marks a cell past the last channel).
pub fn page_cells(layout: GridLayout, page: usize, total: usize) -> Vec<Option<usize>> {
    let capacity = layout.capacity();
    let start = page * capacity;
    (0..capacity)
        .map(|slot| {
            let index = start + slot;
            (index < total).then_some(index)
        })
        .collect()
}

/// Rectangle of one grid cell inside `area`.
pub fn tile_rect(area: Rect, layout: GridLayout, cell: usize, gap: f32) -> Rect {
    let cols = layout.cols() as f32;
    let rows = layout.rows() as f32;
    let width = ((area.width() - gap * (cols - 1.0)) / cols).max(1.0);
    let height = ((area.height() - gap * (rows - 1.0)) / rows).max(1.0);
    let col = (cell % layout.cols()) as f32;
    let row = (cell / layout.cols()) as f32;
    Rect::from_min_size(
        egui::pos2(area.left() + col * (width + gap), area.top() + row * (height + gap)),
        vec2(width, height),
    )
}

/// Everything needed to paint one tile.
pub struct Tile<'a> {
    pub id: Id,
    /// Global channel index.
    pub index: Option<usize>,
    pub camera: Option<&'a CameraSource>,
    pub channel: Option<&'a ChannelUi>,
    /// Newest decoded picture of the channel, `None` until the first frame.
    pub video: Option<VideoSurface>,
    /// Draw the focus ring.
    pub focused: bool,
    /// Accept clicks and drags.
    pub interactive: bool,
    /// Dim the tile (used for the page that slides out).
    pub dim: bool,
    pub show_stats: bool,
    pub time: f64,
}

/// Interaction reported by [`paint`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TileActions {
    pub focus: Option<usize>,
    pub zoom: Option<usize>,
    pub drag_started: bool,
    pub drag_delta: f32,
    pub drag_stopped: bool,
}

/// Paints one cell and reports the user interaction.
pub fn paint(ui: &mut Ui, rect: Rect, tile: &Tile<'_>) -> Response {
    let sense = if tile.interactive { Sense::click_and_drag() } else { Sense::hover() };
    let response = ui.interact(rect, tile.id, sense);
    let painter = ui.painter_at(rect);
    let radius = CornerRadius::same(6);
    let dim = |color: Color32| if tile.dim { color.gamma_multiply(0.5) } else { color };

    painter.rect_filled(rect, radius, dim(theme::TILE_BORDER));

    let Some(camera) = tile.camera else {
        painter.rect_filled(rect.shrink(1.0), CornerRadius::same(5), dim(theme::TILE_EMPTY));
        if tile.focused {
            ring(&painter, rect, radius);
        }
        return response;
    };

    let canvas = rect.shrink(1.0);
    let header_height = HEADER_HEIGHT.min(canvas.height() * 0.3);
    let header = Rect::from_min_size(canvas.min, vec2(canvas.width(), header_height));
    let body = Rect::from_min_max(egui::pos2(canvas.left(), header.bottom()), canvas.max);
    let tiny = canvas.height() < 120.0;

    painter.rect_filled(canvas, CornerRadius::same(5), dim(theme::TILE_CANVAS));
    painter.rect_filled(header, CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 }, dim(theme::TILE_HEADER));

    let label = match tile.index {
        Some(index) => format!("{:>2}  {}", index + 1, camera.short_label(24)),
        None => camera.short_label(24),
    };
    painter.text(
        header.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        dim(theme::TEXT),
    );

    let stream = tile.channel.and_then(|channel| channel.stream);
    if let Some(stream) = stream {
        let color = match stream {
            StreamKind::Main => theme::WARN,
            StreamKind::Sub => theme::ACCENT,
        };
        painter.text(
            header.right_center() - vec2(8.0, 0.0),
            Align2::RIGHT_CENTER,
            format!("[{}]", stream.tag()),
            FontId::monospace(11.0),
            dim(color),
        );
    }

    // Body: either video placeholder, spinner or error panel.
    let state = tile.channel.map(|channel| channel.state).unwrap_or(ConnectionState::Idle);
    let detail = tile.channel.map(|channel| channel.detail.as_str()).unwrap_or("");
    match state {
        ConnectionState::Streaming => live_canvas(&painter, body, tile),
        ConnectionState::Failed => {
            painter.rect_filled(body, CornerRadius::same(4), dim(theme::CANVAS_FAILED));
            painter.text(body.center() - vec2(0.0, 16.0), Align2::CENTER_CENTER, "!", FontId::proportional(26.0), theme::ERROR);
            painter.text(body.center() + vec2(0.0, 12.0), Align2::CENTER_CENTER, "Stream failed", FontId::proportional(14.0), theme::ERROR);
            if !detail.is_empty() && !tiny {
                painter.text(
                    body.center() + vec2(0.0, 32.0),
                    Align2::CENTER_CENTER,
                    theme::truncate(detail, 46),
                    FontId::proportional(11.0),
                    theme::TEXT_DIM,
                );
            }
        }
        ConnectionState::Suspended => {
            painter.rect_filled(body, CornerRadius::same(4), dim(theme::CANVAS));
            painter.text(body.center(), Align2::CENTER_CENTER, "suspended", FontId::proportional(13.0), dim(theme::TEXT_DIM));
        }
        ConnectionState::Idle | ConnectionState::Connecting | ConnectionState::Reconnecting => {
            painter.rect_filled(body, CornerRadius::same(4), dim(theme::CANVAS_PENDING));
            spinner(&painter, body.center() - vec2(0.0, 16.0), 12.0, tile.time, theme::ACCENT);
            painter.text(body.center() + vec2(0.0, 14.0), Align2::CENTER_CENTER, state.label(), FontId::proportional(14.0), theme::TEXT);
            if !detail.is_empty() && !tiny {
                painter.text(
                    body.center() + vec2(0.0, 34.0),
                    Align2::CENTER_CENTER,
                    theme::truncate(detail, 52),
                    FontId::proportional(11.0),
                    theme::TEXT_DIM,
                );
            }
        }
    }

    // Stream switch overlay: the last frame of the previous stream is held until
    // the new stream delivers its first frames, so the viewport never shows a
    // black or green frame.
    if let Some(channel) = tile.channel {
        if let Some(from) = channel.switching_from {
            let width = (rect.width() - 16.0).clamp(120.0, 430.0);
            let band = Rect::from_center_size(rect.center(), vec2(width, 54.0));
            painter.rect_filled(band, CornerRadius::same(6), Color32::from_black_alpha(205));
            spinner(&painter, band.left_center() + vec2(22.0, 0.0), 10.0, tile.time, theme::FOCUS);
            let target = channel.stream.unwrap_or(StreamKind::Main);
            painter.text(
                band.left_center() + vec2(42.0, -10.0),
                Align2::LEFT_CENTER,
                format!("Switching to {} stream…", target.label()),
                FontId::proportional(13.0),
                theme::FOCUS,
            );
            painter.text(
                band.left_center() + vec2(42.0, 10.0),
                Align2::LEFT_CENTER,
                format!("holding last {} frame", from.tag()),
                FontId::proportional(11.0),
                theme::TEXT_DIM,
            );
        }
    }

    if tile.show_stats {
        if let Some(channel) = tile.channel {
            if !tiny {
                painter.text(
                    body.right_bottom() - vec2(8.0, 6.0),
                    Align2::RIGHT_BOTTOM,
                    format!("{:.1} fps  {:.0} kb/s", channel.fps, channel.bitrate_kbps),
                    FontId::monospace(11.0),
                    theme::TEXT_DIM.gamma_multiply(0.9),
                );
            }
        }
    }

    if tile.focused {
        ring(&painter, rect, radius);
    }

    response
}

/// Paints the newest decoded pictures of a live channel, or a placeholder while
/// the first one is on its way.
fn live_canvas(painter: &egui::Painter, rect: Rect, tile: &Tile<'_>) {
    painter.rect_filled(rect, CornerRadius::same(4), theme::CANVAS);
    let channel = tile.channel;

    let Some(surface) = tile.video else {
        waiting_for_video(painter, rect, channel);
        return;
    };

    // The picture is letterboxed, never stretched: a non uniform scale would
    // distort faces and make the tile useless for an identification.
    let size = surface.size;
    if size.x > 0.0 && size.y > 0.0 {
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        let dest = Rect::from_center_size(rect.center(), size * scale);
        painter.image(
            surface.id,
            dest,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    if rect.width() > 150.0 && rect.height() > 90.0 {
        let chip = Rect::from_min_size(rect.left_top() + vec2(8.0, 8.0), vec2(54.0, 18.0));
        painter.rect_filled(chip, CornerRadius::same(3), Color32::from_black_alpha(150));
        painter.circle_filled(chip.left_center() + vec2(11.0, 0.0), 3.5, theme::LIVE);
        painter.text(chip.left_center() + vec2(21.0, 0.0), Align2::LEFT_CENTER, "LIVE", FontId::monospace(11.0), theme::LIVE);

        let label = match channel.and_then(|channel| channel.codec.as_deref()) {
            Some(codec) => format!("{}  {:.0}×{:.0}", codec.to_uppercase(), size.x, size.y),
            None => format!("{:.0}×{:.0}", size.x, size.y),
        };
        painter.text(
            rect.right_top() + vec2(-8.0, 8.0),
            Align2::RIGHT_TOP,
            label,
            FontId::monospace(11.0),
            theme::TEXT_DIM,
        );
    }
}

/// Placeholder shown between `PLAY` and the first decoded picture.
///
/// Cameras answer `PLAY` first and only then send an IDR preceded by its
/// parameter sets, which on a low bandwidth link can take a moment; a static
/// icon is a truthful state, unlike an empty black tile.
fn waiting_for_video(painter: &egui::Painter, rect: Rect, channel: Option<&ChannelUi>) {
    let scale = (rect.width().min(rect.height()) * 0.22).clamp(16.0, 64.0);
    let icon = Rect::from_center_size(rect.center() - vec2(0.0, scale * 0.18), vec2(scale, scale * 0.72));
    painter.rect_stroke(icon, CornerRadius::same(4), Stroke::new(2.0_f32, theme::TEXT_DIM.gamma_multiply(0.4)), StrokeKind::Inside);
    painter.circle_filled(icon.center(), scale * 0.18, theme::TEXT_DIM.gamma_multiply(0.3));

    painter.text(
        rect.center() + vec2(0.0, scale * 0.72),
        Align2::CENTER_CENTER,
        "waiting for video",
        FontId::monospace(12.0),
        theme::TEXT_DIM,
    );

    if rect.width() > 150.0 && rect.height() > 90.0 {
        let chip = Rect::from_min_size(rect.left_top() + vec2(8.0, 8.0), vec2(54.0, 18.0));
        painter.rect_filled(chip, CornerRadius::same(3), Color32::from_black_alpha(150));
        painter.circle_filled(chip.left_center() + vec2(11.0, 0.0), 3.5, theme::LIVE);
        painter.text(chip.left_center() + vec2(21.0, 0.0), Align2::LEFT_CENTER, "LIVE", FontId::monospace(11.0), theme::LIVE);

        if let Some(codec) = channel.and_then(|channel| channel.codec.as_deref()) {
            painter.text(
                rect.right_top() + vec2(-8.0, 8.0),
                Align2::RIGHT_TOP,
                codec.to_uppercase(),
                FontId::monospace(11.0),
                theme::TEXT_DIM,
            );
        }
    }
}

fn ring(painter: &egui::Painter, rect: Rect, radius: CornerRadius) {
    painter.rect_stroke(rect, radius, Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS), StrokeKind::Inside);
}

/// Paints a rotating arc, the loading indicator used while (re)connecting.
pub fn spinner(painter: &egui::Painter, center: egui::Pos2, radius: f32, time: f64, color: Color32) {
    const STEPS: usize = 26;
    let start = (time * 2.6) as f32;
    let points: Vec<egui::Pos2> = (0..=STEPS)
        .map(|step| {
            let t = step as f32 / STEPS as f32;
            let angle = start + t * std::f32::consts::TAU * 0.8;
            center + vec2(angle.cos(), angle.sin()) * radius
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(2.5_f32, color)));
}

/// Centered hint drawn when there is nothing to display.
pub fn empty_state(ui: &mut Ui, area: Rect, message: &str, hint: &str) {
    let painter = ui.painter_at(area);
    painter.text(area.center() - vec2(0.0, 12.0), Align2::CENTER_CENTER, message, FontId::proportional(19.0), theme::TEXT_DIM);
    painter.text(
        area.center() + vec2(0.0, 16.0),
        Align2::CENTER_CENTER,
        hint,
        FontId::proportional(13.0),
        theme::TEXT_DIM.gamma_multiply(0.75),
    );
}
