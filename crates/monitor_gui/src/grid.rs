//! Grid geometry and tile painting.
//!
//! The grid is drawn directly with an [`egui::Painter`]: every cell is a tile
//! showing the camera name, the stream currently decoded, the connection state
//! and the newest decoded picture, letterboxed into the tile. The same code
//! paints the outgoing page during a swipe, which is why painting and
//! interaction are decoupled: [`paint`] never mutates the application state, it
//! only reports what the user did.

use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, vec2};

use monitor_core::model::{CameraSource, ConnectionState, Osd, OsdItem, StreamKind, TileAspect};
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
    /// What each corner of the tile shows.
    pub osd: Osd,
    pub time: f64,
    /// How the picture is fitted into the tile.
    pub aspect: TileAspect,
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
    // so a tile paints no border of its own and keeps no inset. The only thing
    // over the picture is the on-screen display the settings panel asked for.
    if tile.camera.is_none() {
        painter.rect_filled(rect, CornerRadius::ZERO, dim(theme::TILE_EMPTY));
        if tile.focused {
            ring(&painter, rect);
        }
        return response;
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
            painter.text(body.center() + vec2(0.0, 12.0), Align2::CENTER_CENTER, monitor_i18n::tr("grid-stream-failed"), FontId::proportional(14.0), theme::ERROR);
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
            painter.text(body.center(), Align2::CENTER_CENTER, monitor_i18n::tr("grid-suspended"), FontId::proportional(13.0), dim(theme::TEXT_DIM));
        }
        ConnectionState::Idle | ConnectionState::Connecting | ConnectionState::Reconnecting => {
            painter.rect_filled(body, CornerRadius::ZERO, dim(theme::CANVAS_PENDING));
            spinner(&painter, body.center() - vec2(0.0, 16.0), 12.0, tile.time, theme::ACCENT);
            painter.text(body.center() + vec2(0.0, 14.0), Align2::CENTER_CENTER, monitor_i18n::tr(state.label()), FontId::proportional(14.0), theme::TEXT);
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
                monitor_i18n::tr_args("grid-switching-to", &[("stream", monitor_i18n::tr(target.label()).into())]),
                FontId::proportional(13.0),
                theme::FOCUS,
            );
            painter.text(
                band.left_center() + vec2(42.0, 10.0),
                Align2::LEFT_CENTER,
                monitor_i18n::tr_args("grid-holding-last", &[("tag", from.tag().into())]),
                FontId::proportional(11.0),
                theme::TEXT_DIM,
            );
        }
    }

    // The on-screen display, in the corners the settings panel chose. It goes
    // over whatever the state left on the tile, so that a stream that failed
    // still says which camera it is.
    osd(&painter, body, tile);

    if tile.focused {
        ring(&painter, rect);
    }

    response
}

/// Draws the four corners of a tile's on-screen display.
fn osd(painter: &egui::Painter, body: Rect, tile: &Tile<'_>) {
    let metrics = OsdMetrics::of(body);
    for (item, align) in [
        (tile.osd.top_left, Align2::LEFT_TOP),
        (tile.osd.top_right, Align2::RIGHT_TOP),
        (tile.osd.bottom_left, Align2::LEFT_BOTTOM),
        (tile.osd.bottom_right, Align2::RIGHT_BOTTOM),
    ] {
        let lines = osd_lines(item, tile);
        if !lines.is_empty() {
            // A camera's name is what a viewer looks for first, while the
            // measures around it are a detail: the two are not set at the same
            // size.
            let scale = match item {
                OsdItem::Name | OsdItem::NumberName => NAME_SCALE,
                _ => MEASURE_SCALE,
            };
            osd_corner(painter, body, align, tile.dim, &lines, &metrics, scale);
        }
    }
}

/// Text scale of a name, against the size the tile itself asks for.
const NAME_SCALE: f32 = 1.2;
/// Text scale of everything that is a measurement rather than a name.
const MEASURE_SCALE: f32 = 0.8;

/// Sizes of a tile's on-screen display, taken from the tile itself.
///
/// A wall is watched at every scale, from a single channel filling a television
/// to sixteen cells on a phone: one size of text cannot serve both, and the
/// display is the first thing to overrun a small cell. Everything scales with
/// the tile's shorter side, capped at the size it has on a full tile so that a
/// large screen does not end up with text across half the picture.
struct OsdMetrics {
    font: f32,
    /// Distance kept from the tile's edge.
    inset: f32,
}

impl OsdMetrics {
    fn of(body: Rect) -> Self {
        let min = body.width().min(body.height());
        Self {
            font: (min * 0.045).clamp(7.0, 12.0),
            inset: (min * 0.0125).clamp(1.0, 4.0),
        }
    }
}

/// One line of a tile's on-screen display.
struct OsdLine {
    /// A dot drawn before the text, in a colour of its own.
    dot: Option<Color32>,
    text: String,
}

/// What one corner of a tile shows, in the order its lines are drawn.
fn osd_lines(item: OsdItem, tile: &Tile<'_>) -> Vec<OsdLine> {
    let Some(camera) = tile.camera else {
        return Vec::new();
    };
    let channel = tile.channel;
    let hardware = if channel.is_some_and(|channel| channel.hardware) { "HW" } else { "SW" };
    let link = || OsdLine {
        dot: None,
        text: format!(
            "{} {:.0}kbps",
            camera.transport.as_str(),
            channel.map(|channel| channel.bitrate_kbps).unwrap_or(0.0)
        ),
    };
    let fps = || OsdLine {
        dot: None,
        text: format!("{} {:.2}fps", hardware, channel.map(|channel| channel.fps).unwrap_or(0.0)),
    };
    let format = || OsdLine {
        dot: None,
        text: match channel.and_then(|channel| channel.width.zip(channel.height)) {
            Some((width, height)) => format!("{}x{} {}", width, height, monitor_i18n::tr(tile.aspect.short_label())),
            None => format!("?x? {}", monitor_i18n::tr(tile.aspect.short_label())),
        },
    };
    match item {
        OsdItem::Off => Vec::new(),
        OsdItem::Name => vec![OsdLine { dot: None, text: camera.short_label(24) }],
        OsdItem::NumberName => vec![OsdLine {
            dot: None,
            text: match tile.index {
                Some(index) => format!("{} {}", index + 1, camera.short_label(22)),
                None => camera.short_label(24),
            },
        }],
        OsdItem::Stream => vec![OsdLine {
            // The dot is the picture moving - the one question a wall of still
            // images cannot answer from the images themselves.
            dot: Some(match channel.map(|channel| channel.state) {
                Some(ConnectionState::Streaming) => theme::LIVE,
                _ => theme::TEXT_DIM,
            }),
            text: channel
                .and_then(|channel| channel.stream)
                .map(|stream| stream.tag().to_string())
                .unwrap_or_else(|| "—".to_string()),
        }],
        OsdItem::Link => vec![link()],
        OsdItem::Fps => vec![fps()],
        OsdItem::Format => vec![format()],
        OsdItem::Detail => vec![link(), fps(), format()],
    }
}

/// Draws one corner's lines, growing away from the corner they belong to.
fn osd_corner(painter: &egui::Painter, body: Rect, align: Align2, dim: bool, lines: &[OsdLine], m: &OsdMetrics, scale: f32) {
    let size = m.font * scale;
    let font = FontId::monospace(size);
    // Leading and padding follow the text they belong to, so a scaled line
    // keeps the proportions of an unscaled one.
    let line = size * 1.25;
    let pad = vec2(size * 0.35, size * 0.22);
    let color = if dim { theme::TEXT.gamma_multiply(0.5) } else { theme::TEXT };
    let left = matches!(align, Align2::LEFT_TOP | Align2::LEFT_BOTTOM);
    let top = matches!(align, Align2::LEFT_TOP | Align2::RIGHT_TOP);
    let anchor = if left { Align2::LEFT_TOP } else { Align2::RIGHT_TOP };
    let corner = egui::pos2(
        if left { body.left() + m.inset } else { body.right() - m.inset },
        if top { body.top() + m.inset } else { body.bottom() - m.inset },
    );

    // The backdrop has to be the size of the text it sits behind, and how wide
    // a line is only the font knows: every line is laid out once to measure it,
    // and the room a leading dot takes is added to whichever corner has one.
    let dot_lead = if lines.iter().any(|line| line.dot.is_some()) { size * 1.15 } else { 0.0 };
    let text_width = lines
        .iter()
        .map(|line| painter.layout_no_wrap(line.text.clone(), font.clone(), color).size().x)
        .fold(0.0_f32, f32::max);
    let width = text_width + dot_lead;
    let height = lines.len() as f32 * line;

    // Anchored at its corner: the block hangs below the top edge or above the
    // bottom one, so the corner keeps its place whatever the tile measures.
    let (x0, y0) = if left {
        (corner.x, if top { corner.y } else { corner.y - height })
    } else {
        (corner.x - width, if top { corner.y } else { corner.y - height })
    };
    let block = Rect::from_min_size(egui::pos2(x0, y0), vec2(width, height));
    let backdrop = if dim { theme::OSD_BACKDROP.gamma_multiply(0.5) } else { theme::OSD_BACKDROP };
    painter.rect_filled(block.expand2(pad).intersect(body), CornerRadius::same(theme::radius::S), backdrop);

    for (row, entry) in lines.iter().enumerate() {
        // The lines of a bottom corner stack upwards from the edge, so the
        // first one keeps the corner and the rest grow into the picture.
        let y = if top { corner.y + row as f32 * line } else { corner.y - (row as f32 + 1.0) * line };
        let mut x = corner.x;
        if let Some(dot) = entry.dot {
            let lead = if left { dot_lead } else { -dot_lead };
            painter.circle_filled(
                egui::pos2(x + lead * 0.4, y + line * 0.5),
                (size * 0.3).max(2.0),
                dot,
            );
            x += lead;
        }
        outlined_text(painter, egui::pos2(x, y), anchor, &entry.text, font.clone(), color);
    }
}

/// Text over a picture, with a dark edge so it stays readable on anything.
fn outlined_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    align: Align2,
    text: &str,
    font: FontId,
    color: Color32,
) {
    let edge = Color32::from_black_alpha(190);
    for offset in [vec2(-1.0, 0.0), vec2(1.0, 0.0), vec2(0.0, -1.0), vec2(0.0, 1.0)] {
        painter.text(pos + offset, align, text, font.clone(), edge);
    }
    painter.text(pos, align, text, font, color);
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
        monitor_i18n::tr("grid-waiting-video"),
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
