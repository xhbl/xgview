//! Grid geometry and tile painting.
//!
//! The grid is drawn directly with an [`egui::Painter`]: every cell is a tile
//! showing the camera name, the stream currently decoded, the connection state
//! and the newest decoded picture, letterboxed into the tile. The same code
//! paints the outgoing page during a swipe, which is why painting and
//! interaction are decoupled: [`paint`] never mutates the application state, it
//! only reports what the user did.

use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, vec2};

use monitor_core::model::{CameraSource, ConnectionState, StreamKind, TileAspect};
use monitor_core::GridLayout;

use crate::app::ChannelUi;
use crate::theme;
use crate::video::VideoSurface;

/// Width of a grid line, in points: the room the tiles leave between each other
/// and around the wall.
///
/// The wall is painted in [`crate::theme::GRID_LINE`] and the tiles leave this
/// much of it showing, rather than each tile drawing a border of its own: two
/// neighbours' borders would double up in the middle and leave the outer ring
/// half as wide as the crosses.
pub const LINE: f32 = 1.0;

/// How much of a tile's furniture fits.
///
/// A 4x4 grid on a television leaves each tile a couple of hundred points; the
/// same tile on a phone in landscape leaves it about a hundred. What is drawn
/// over a picture - a stream that failed and why, the decoder's numbers - is
/// dropped rather than shrunk when it would be too small to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// The picture and its state only.
    Minimal,
    /// Room for the reason a stream failed.
    Compact,
    /// Everything the overlay is asked for.
    Full,
}

impl Tier {
    fn of(size: Vec2) -> Self {
        if size.y < 150.0 || size.x < 220.0 {
            Self::Minimal
        } else if size.y < 260.0 || size.x < 380.0 {
            Self::Compact
        } else {
            Self::Full
        }
    }
}

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
    /// How the picture is fitted into the tile.
    pub aspect: TileAspect,
}

/// Interaction reported by [`paint`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TileActions {
    pub focus: Option<usize>,
    pub zoom: Option<usize>,
    /// The aspect button of this channel was pressed.
    pub cycle_aspect: Option<usize>,
    pub drag_started: bool,
    pub drag_delta: f32,
    pub drag_stopped: bool,
}

/// Paints one cell and reports the user interaction.
///
/// The second value of the pair is `true` when the tile's aspect button was
/// pressed this frame - it is a control of its own, on top of the tile, and is
/// reported apart from the tile's own response.
pub fn paint(ui: &mut Ui, rect: Rect, tile: &Tile<'_>) -> (Response, bool) {
    // Clickable and draggable, but deliberately *not* focusable. egui walks the
    // focusable widgets with the four directions on its own, and a tile is not
    // one of them: on the wall it is a channel that moves, not a widget, and
    // leaving the focusable bit on would put every tile in egui's walk and set
    // the two navigations against each other.
    let sense = if tile.interactive { Sense::CLICK | Sense::DRAG } else { Sense::hover() };
    let response = ui.interact(rect, tile.id, sense);
    let painter = ui.painter_at(rect);
    let dim = |color: Color32| if tile.dim { color.gamma_multiply(0.5) } else { color };

    // What a tile is, is its picture, drawn edge to edge: the line between two
    // tiles is the wall showing through the room the grid leaves around them,
    // so a tile paints no border of its own and keeps no inset. The name, the
    // stream and the decoder's numbers are read elsewhere, and whatever the
    // wall draws over a picture is asked for with `show_stats`.
    if tile.camera.is_none() {
        painter.rect_filled(rect, CornerRadius::ZERO, dim(theme::TILE_EMPTY));
        if tile.focused {
            ring(&painter, rect);
        }
        return (response, false);
    }

    let body = rect;
    let tier = Tier::of(body.size());
    painter.rect_filled(body, CornerRadius::ZERO, dim(theme::TILE_CANVAS));

    // Body: either video placeholder, spinner or error panel.
    let state = tile.channel.map(|channel| channel.state).unwrap_or(ConnectionState::Idle);
    let detail = tile.channel.map(|channel| channel.detail.as_str()).unwrap_or("");
    match state {
        ConnectionState::Streaming => live_canvas(&painter, body, tile, tier),
        ConnectionState::Failed => {
            painter.rect_filled(body, CornerRadius::ZERO, dim(theme::CANVAS_FAILED));
            painter.text(body.center() - vec2(0.0, 16.0), Align2::CENTER_CENTER, "!", FontId::proportional(26.0), theme::ERROR);
            painter.text(body.center() + vec2(0.0, 12.0), Align2::CENTER_CENTER, "Stream failed", FontId::proportional(14.0), theme::ERROR);
            if tier != Tier::Minimal && !detail.is_empty() {
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
            painter.rect_filled(body, CornerRadius::ZERO, dim(theme::CANVAS));
            painter.text(body.center(), Align2::CENTER_CENTER, "suspended", FontId::proportional(13.0), dim(theme::TEXT_DIM));
        }
        ConnectionState::Idle | ConnectionState::Connecting | ConnectionState::Reconnecting => {
            painter.rect_filled(body, CornerRadius::ZERO, dim(theme::CANVAS_PENDING));
            spinner(&painter, body.center() - vec2(0.0, 16.0), 12.0, tile.time, theme::ACCENT);
            painter.text(body.center() + vec2(0.0, 14.0), Align2::CENTER_CENTER, state.label(), FontId::proportional(14.0), theme::TEXT);
            if tier != Tier::Minimal && !detail.is_empty() {
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
            let width = (rect.width() - theme::space::L).clamp(120.0, 430.0);
            let band = Rect::from_center_size(rect.center(), vec2(width, 54.0));
            painter.rect_filled(band, CornerRadius::ZERO, Color32::from_black_alpha(205));
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

    // The decoder's own numbers, drawn over the picture and only when the
    // viewer asked for them: a wall is watched, not read, and a small tile has
    // no room for a line nobody is looking at. See `Tile::show_stats`.
    if tile.show_stats && tier != Tier::Minimal {
        if let Some(channel) = tile.channel {
            painter.text(
                body.right_bottom() - vec2(theme::space::S, 6.0),
                Align2::RIGHT_BOTTOM,
                format!("{:.1} fps  {:.0} kb/s", channel.fps, channel.bitrate_kbps),
                FontId::monospace(11.0),
                theme::TEXT_DIM.gamma_multiply(0.9),
            );
        }
    }

    // The aspect button, over the picture: a press on it cycles the tile's
    // display mode rather than touching the channel.
    let cycle_aspect = aspect_chip(ui, body, tile.id, tile.aspect, tile.interactive);

    if tile.focused {
        ring(&painter, rect);
    }

    (response, cycle_aspect)
}

/// Paints the newest decoded pictures of a live channel, or a placeholder while
/// the first one is on its way.
fn live_canvas(painter: &egui::Painter, rect: Rect, tile: &Tile<'_>, tier: Tier) {
    painter.rect_filled(rect, CornerRadius::ZERO, theme::CANVAS);

    let Some(surface) = tile.video else {
        waiting_for_video(painter, rect, tier);
        return;
    };

    // Where the picture lands depends on the tile's display mode: see
    // [`fitted`] for the shapes, and why the default distorts nothing.
    let size = surface.size;
    if size.x > 0.0 && size.y > 0.0 {
        let dest = fitted(size, rect, tile.aspect);
        painter.image(
            surface.id,
            dest,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

/// Where a picture lands inside its tile, for the tile's display mode.
///
/// `Original` keeps the shape the stream sends - nothing is distorted, and the
/// bands left over are the price of a wall whose cameras do not all agree on a
/// shape. `Stretch` fills the tile whole. A fixed ratio gives the picture that
/// shape, stretched into it wherever the two disagree.
///
/// `source` is the decoded picture's size; the caller checks it is not empty.
fn fitted(source: Vec2, tile: Rect, aspect: TileAspect) -> Rect {
    match aspect.ratio() {
        Some(ratio) => {
            let (width, height) = if tile.width() / tile.height() > ratio {
                (tile.height() * ratio, tile.height())
            } else {
                (tile.width(), tile.width() / ratio)
            };
            Rect::from_center_size(tile.center(), vec2(width, height))
        }
        None if aspect == TileAspect::Stretch => tile,
        None => {
            let scale = (tile.width() / source.x).min(tile.height() / source.y);
            Rect::from_center_size(tile.center(), source * scale)
        }
    }
}

/// The aspect button: the mode this tile is drawn in, and the press that moves
/// it to the next one.
///
/// It is painted like the rest of the wall - a shape over the picture - and its
/// interaction is registered after the tile's own, so a press on it cycles the
/// mode instead of touching the channel. Returns `true` when it was pressed.
fn aspect_chip(ui: &mut Ui, body: Rect, id: Id, aspect: TileAspect, interactive: bool) -> bool {
    let galley = ui
        .painter()
        .layout_no_wrap(aspect.label().to_owned(), FontId::monospace(11.0), Color32::PLACEHOLDER);
    let size = galley.size() + vec2(2.0 * theme::space::S, 6.0);
    let corner = body.left_bottom() + vec2(theme::space::S, -theme::space::S - size.y);
    // A tile can be shorter than the chip: keep it inside the picture.
    let chip = Rect::from_min_size(egui::pos2(corner.x, corner.y.max(body.top() + theme::space::S)), size);
    {
        let painter = ui.painter();
        painter.rect_filled(chip, CornerRadius::ZERO, Color32::from_black_alpha(150));
        painter.galley(chip.center() - galley.size() * 0.5, galley, theme::TEXT_DIM);
    }
    if !interactive {
        return false;
    }
    ui.interact(chip, Id::new((id, "aspect")), Sense::click()).clicked()
}

/// Placeholder shown between `PLAY` and the first decoded picture.
///
/// Cameras answer `PLAY` first and only then send an IDR preceded by its
/// parameter sets, which on a low bandwidth link can take a moment; a static
/// icon is a truthful state, unlike an empty black tile.
fn waiting_for_video(painter: &egui::Painter, rect: Rect, tier: Tier) {
    let scale = (rect.width().min(rect.height()) * 0.22).clamp(16.0, 64.0);
    let icon = Rect::from_center_size(rect.center() - vec2(0.0, scale * 0.18), vec2(scale, scale * 0.72));
    painter.rect_stroke(icon, CornerRadius::ZERO, Stroke::new(2.0_f32, theme::TEXT_DIM.gamma_multiply(0.4)), StrokeKind::Inside);
    painter.circle_filled(icon.center(), scale * 0.18, theme::TEXT_DIM.gamma_multiply(0.3));

    if tier == Tier::Minimal {
        return;
    }
    painter.text(
        rect.center() + vec2(0.0, scale * 0.72),
        Align2::CENTER_CENTER,
        "waiting for video",
        FontId::monospace(12.0),
        theme::TEXT_DIM,
    );
}

fn ring(painter: &egui::Painter, rect: Rect) {
    painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS), StrokeKind::Inside);
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
