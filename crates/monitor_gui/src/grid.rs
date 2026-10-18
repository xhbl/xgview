//! Grid geometry and tile painting.
//!
//! The grid is drawn directly with an [`egui::Painter`]: every cell is a tile
//! showing the camera name, the stream currently decoded, the connection state
//! and the newest decoded picture, letterboxed into the tile. The same code
//! paints the outgoing page during a swipe, which is why painting and
//! interaction are decoupled: [`paint`] never mutates the application state, it
//! only reports what the user did.

use std::sync::Arc;

use egui::epaint::{Mesh, Shape, TextShape, Vertex};
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
///
/// The layout divides `area` into `grid_cols() x grid_rows()` equal units with
/// `gap` between neighbours; a cell covers its span of them, so a large cell of
/// a mixed layout is drawn across every unit it merged. For the square layouts
/// every span is one and this is the plain uniform grid it always was.
pub fn tile_rect(area: Rect, layout: GridLayout, cell: usize, gap: f32) -> Rect {
    let cell = layout.cells()[cell];
    let cols = layout.grid_cols() as f32;
    let rows = layout.grid_rows() as f32;
    let unit_w = ((area.width() - gap * (cols - 1.0)) / cols).max(1.0);
    let unit_h = ((area.height() - gap * (rows - 1.0)) / rows).max(1.0);
    let col = cell.col as f32;
    let row = cell.row as f32;
    let span_x = cell.col_span as f32;
    let span_y = cell.row_span as f32;
    Rect::from_min_size(
        egui::pos2(area.left() + col * (unit_w + gap), area.top() + row * (unit_h + gap)),
        vec2(unit_w * span_x + gap * (span_x - 1.0), unit_h * span_y + gap * (span_y - 1.0)),
    )
}

/// Paints a small picture of a layout: the wall's own grid geometry, shrunk
/// into `rect`, one rectangle per viewport. Used by the layout picker.
pub fn layout_thumbnail(painter: &egui::Painter, rect: Rect, layout: GridLayout) {
    painter.rect_filled(rect, CornerRadius::ZERO, theme::CANVAS);
    for cell in 0..layout.capacity() {
        let cell_rect = tile_rect(rect.shrink(1.0), layout, cell, 1.0);
        painter.rect_stroke(
            cell_rect,
            CornerRadius::ZERO,
            Stroke::new(1.0_f32, theme::GRID_LINE),
            StrokeKind::Inside,
        );
    }
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
    /// The four combined-info slots, each an ordered list of atomic items.
    pub combined: [&'a [OsdItem]; 4],
    pub time: f64,
    /// How the picture is fitted into the tile.
    pub aspect: TileAspect,
    /// Draw the picture rotated 90° clockwise. Used in portrait single-view
    /// so a landscape feed fills the upright screen sideways.
    pub rotated: bool,
    /// What the tile shows of its picture, where it is the magnified one.
    ///
    /// Every other tile shows all of it. See [`PictureZoom`].
    pub picture_zoom: Option<PictureZoom>,
}

/// What the tile shows of its picture, where it is the magnified one.
///
/// The magnified view is one camera drawn into one tile, and inspecting a detail
/// of it means showing *less* of the picture over the same area rather than
/// growing the tile: the tile shows a window onto the source, and it shows that
/// window whole. `1.0` is the picture the tile has always shown - the same
/// texture, the same rectangle, the same letterboxing - and pushing in only
/// moves the window and shrinks it. So the scale is a measure of what is *not*
/// shown: there is no second texture, no re-layout, and nothing to do on the
/// streams behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PictureZoom {
    /// How much of the source is shown: `1.0` is all of it, `2.0` is half its
    /// width and half its height.
    pub scale: f32,
    /// Centre of the window, in the picture's own coordinates, both axes `0..1`:
    /// `0.5` is the middle of the picture.
    pub center: Vec2,
}

/// The most the picture may be pushed in.
///
/// A sub stream magnified eight times is a handful of source pixels stretched
/// over the screen, and a trackpad's own wheel travels a long way in one
/// gesture: past this the picture is only blur, and the limit is what keeps a
/// gesture from reaching it by accident.
pub const PICTURE_ZOOM_MAX: f32 = 8.0;

impl PictureZoom {
    /// The whole picture, which is how a magnified view starts and what a
    /// magnified viewport goes back to.
    pub const FULL: Self = Self { scale: 1.0, center: Vec2::new(0.5, 0.5) };

    /// Whether the picture has been pushed in at all.
    pub fn magnified(self) -> bool {
        self.scale > 1.0
    }

    /// The source window this shows, in the picture's own coordinates.
    pub fn window(self) -> Rect {
        Rect::from_center_size(self.center.to_pos2(), Vec2::splat(1.0 / self.scale))
    }

    /// The window a pinch asked for: the picture spread by `delta` about `at`.
    ///
    /// `at` is where the fingers are, and `dest` where the picture is drawn, so
    /// that the point of the picture under them stays under them: the gesture
    /// zooms towards what it is aimed at rather than towards the middle of the
    /// tile. A `delta` of exactly `1.0` leaves the window where it is - a frame
    /// in which the fingers only moved - and the window is kept inside the
    /// picture, because there is nothing outside it to show.
    pub fn zoomed(self, delta: f32, at: egui::Pos2, dest: Rect) -> Self {
        let scale = (self.scale * delta).clamp(1.0, PICTURE_ZOOM_MAX);
        let window = self.window();
        // Where the aim falls in the picture as it is now, through where it
        // falls in the tile: a gesture made past the edge of the picture is
        // aimed at the edge of it.
        let aim = ((at - dest.min) / dest.size()).clamp(Vec2::ZERO, Vec2::splat(1.0));
        let under = window.min.to_vec2() + aim * window.size();
        let half = Vec2::splat(0.5 / scale);
        let center = (under + (Vec2::splat(0.5) - aim) / scale).clamp(half, Vec2::splat(1.0) - half);
        Self { scale, center }
    }

    /// The window a drag asked for: the picture follows the hands.
    ///
    /// `delta` is how far the pointer moved this frame and `dest` where the
    /// picture is drawn, so the window moves by the same distance *of the
    /// picture* rather than of the tile - at a magnification, a short drag
    /// crosses a lot of picture. Both ends of it are a stop, like the ends of a
    /// magnified viewport's channel step.
    pub fn panned(self, delta: Vec2, dest: Rect) -> Self {
        let scale = self.scale;
        let half = Vec2::splat(0.5 / scale);
        let center = (self.center - delta / (dest.size() * scale)).clamp(half, Vec2::splat(1.0) - half);
        Self { scale, center }
    }
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
        ConnectionState::Streaming | ConnectionState::Stalled => {
            live_canvas(&painter, body, tile, tier)
        }
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
                monitor_i18n::tr_args("grid-holding-last", &[("tag", monitor_i18n::tr(from.tag()).into())]),
                FontId::proportional(11.0),
                theme::TEXT_DIM,
            );
        }
    }

    // The on-screen display, in the corners the settings panel chose. It goes
    // over whatever the state left on the tile, so that a stream that failed
    // still says which camera it is. When the picture is turned sideways the
    // OSD corners trade places, but the text still lives on the whole tile.
    osd(&painter, body, tile);

    if tile.focused {
        ring(&painter, rect);
    }

    response
}

/// Draws the four corners of a tile's on-screen display.
fn osd(painter: &egui::Painter, body: Rect, tile: &Tile<'_>) {
    let ctx = painter.ctx();
    let ppp = ctx.pixels_per_point();
    let screen_min = ctx.screen_rect().size().min_elem() * ppp;
    let metrics = OsdMetrics::of(body, ppp, screen_min);
    // A sideways picture turns its corners with it: a 90° clockwise turn
    // maps left-top to right-top, right-top to right-bottom, and so on.
    let corners = if tile.rotated {
        [
            (tile.osd.top_left, Align2::RIGHT_TOP),
            (tile.osd.top_right, Align2::RIGHT_BOTTOM),
            (tile.osd.bottom_left, Align2::LEFT_TOP),
            (tile.osd.bottom_right, Align2::LEFT_BOTTOM),
        ]
    } else {
        [
            (tile.osd.top_left, Align2::LEFT_TOP),
            (tile.osd.top_right, Align2::RIGHT_TOP),
            (tile.osd.bottom_left, Align2::LEFT_BOTTOM),
            (tile.osd.bottom_right, Align2::RIGHT_BOTTOM),
        ]
    };
    for (item, align) in corners {
        let lines = osd_lines(item, tile);
        if !lines.is_empty() {
            let scale = match item {
                OsdItem::Name | OsdItem::NumberName => NAME_SCALE,
                _ => MEASURE_SCALE,
            };
            if tile.rotated {
                osd_corner_rotated(painter, body, align, tile.dim, &lines, &metrics, scale);
            } else {
                osd_corner(painter, body, align, tile.dim, &lines, &metrics, scale);
            }
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
    fn of(body: Rect, ppp: f32, screen_min: f32) -> Self {
        let min = body.width().min(body.height()) * ppp;
        let f = (screen_min / 1080.0).max(1.0);
        Self {
            font: (min * 0.045).clamp(7.0 * f, 28.0 * f) / ppp,
            inset: (min * 0.0125).clamp(1.0 * f, 10.0 * f) / ppp,
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
    let hardware = if channel.is_some_and(|channel| channel.hardware) {
        monitor_i18n::tr("decode-hw")
    } else {
        monitor_i18n::tr("decode-sw")
    };
    let codec = || OsdLine {
        dot: None,
        text: format!(
            "{} {:.0}kbps",
            codec_name(channel),
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
    // The connection the channel pulls over, then the address it is pulling -
    // the mode the switch is on, and the url that follows it. The address is
    // masked the way the settings row masks it, so a password or a stream key
    // never reaches the picture, and shortened to the room a corner has.
    let transport = || OsdLine {
        dot: None,
        text: format!(
            "{} {}",
            camera.connection_label(),
            theme::truncate(
                &camera.masked_uri(channel.and_then(|channel| channel.stream).unwrap_or(StreamKind::Main)),
                34
            )
        ),
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
                Some(ConnectionState::Stalled) => theme::WARN,
                _ => theme::TEXT_DIM,
            }),
            text: channel
                .and_then(|channel| channel.stream)
                .map(|stream| monitor_i18n::tr(stream.tag()).to_string())
                .unwrap_or_else(|| "—".to_string()),
        }],
        OsdItem::Codec => vec![codec()],
        OsdItem::Fps => vec![fps()],
        OsdItem::Format => vec![format()],
        OsdItem::Transport => vec![transport()],
        OsdItem::DateTime => {
            let Some(now) = monitor_core::blackout::local_now() else {
                return Vec::new();
            };
            vec![OsdLine {
                dot: None,
                text: format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    now.year, now.month, now.day, now.hour, now.minute, now.second
                ),
            }]
        }
        OsdItem::Combined1 | OsdItem::Combined2 | OsdItem::Combined3 | OsdItem::Combined4 => {
            let slot = item.combined_slot().unwrap();
            let mut lines = Vec::new();
            for &atom in tile.combined[slot] {
                if atom.combined_slot().is_some() || atom == OsdItem::Off {
                    continue;
                }
                lines.extend(osd_lines(atom, tile));
            }
            lines
        }
    }
}

/// The codec a tile names, uppercased.
///
/// The name is the camera's - it arrives in the stream's own description - and
/// so is its case, which is why it is normalised here: `h264` in one corner and
/// `H265` in the next would read as two conventions rather than one. Until the
/// stream has described itself there is nothing to name, and a dash says so,
/// the way the stream item does.
fn codec_name(channel: Option<&ChannelUi>) -> String {
    channel
        .and_then(|channel| channel.codec.as_deref())
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "—".to_string())
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

/// [`outlined_text`] for a picture turned sideways: the text is rotated `angle`
/// radians clockwise, drawn via [`TextShape`] because [`Painter::text`] does not
/// expose the `angle` field.
fn outlined_rotated_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    angle: f32,
    text: &str,
    font: FontId,
    color: Color32,
) {
    let edge = Color32::from_black_alpha(190);
    for offset in [vec2(-1.0, 0.0), vec2(1.0, 0.0), vec2(0.0, -1.0), vec2(0.0, 1.0)] {
        let galley = painter.layout_no_wrap(text.to_string(), font.clone(), edge);
        painter.add(TextShape::new(pos + offset, galley, edge).with_angle(angle));
    }
    let galley = painter.layout_no_wrap(text.to_string(), font, color);
    painter.add(TextShape::new(pos, galley, color).with_angle(angle));
}

/// [`osd_corner`] for a picture turned sideways. The text is rotated 90°
/// clockwise and the lines stack horizontally instead of vertically. A
/// rotated line at `pos` occupies `(pos.x - text_h, pos.y)` to
/// `(pos.x, pos.y + text_w)` on screen — it reads top-to-bottom, extending
/// left from `pos.x` and down from `pos.y`.
fn osd_corner_rotated(
    painter: &egui::Painter,
    body: Rect,
    align: Align2,
    dim: bool,
    lines: &[OsdLine],
    m: &OsdMetrics,
    scale: f32,
) {
    let size = m.font * scale;
    let font = FontId::monospace(size);
    let line = size * 1.25;
    let pad = vec2(size * 0.35, size * 0.22);
    let color = if dim { theme::TEXT.gamma_multiply(0.5) } else { theme::TEXT };
    let left = matches!(align, Align2::LEFT_TOP | Align2::LEFT_BOTTOM);
    let top = matches!(align, Align2::LEFT_TOP | Align2::RIGHT_TOP);
    let corner = egui::pos2(
        if left { body.left() + m.inset } else { body.right() - m.inset },
        if top { body.top() + m.inset } else { body.bottom() - m.inset },
    );

    let dot_lead = if lines.iter().any(|line| line.dot.is_some()) { size * 1.15 } else { 0.0 };
    let galleys: Vec<_> = lines
        .iter()
        .map(|entry| painter.layout_no_wrap(entry.text.clone(), font.clone(), color))
        .collect();
    let max_text_width = galleys.iter().map(|g| g.size().x).fold(0.0_f32, f32::max);

    // Rotated 90° CW: the original block's width and height trade places.
    let block_w = lines.len() as f32 * line;
    let block_h = max_text_width + dot_lead;
    let (x0, y0) = if left {
        (corner.x, if top { corner.y } else { corner.y - block_h })
    } else {
        (corner.x - block_w, if top { corner.y } else { corner.y - block_h })
    };
    let block = Rect::from_min_size(egui::pos2(x0, y0), vec2(block_w, block_h));
    let backdrop = if dim { theme::OSD_BACKDROP.gamma_multiply(0.5) } else { theme::OSD_BACKDROP };
    painter.rect_filled(block.expand2(pad).intersect(body), CornerRadius::same(theme::radius::S), backdrop);

    let angle = std::f32::consts::FRAC_PI_2;
    for (row, entry) in lines.iter().enumerate() {
        let text_w = galleys[row].size().x;
        let text_h = galleys[row].size().y;

        // Lines stack horizontally away from the corner.  A rotated line at
        // `pos` extends left to `pos.x - text_h` and down to `pos.y + text_w`,
        // so left corners offset by `text_h` to put the left edge at the corner.
        let pos = if left {
            egui::pos2(corner.x + row as f32 * line + text_h, if top { corner.y } else { corner.y - text_w })
        } else {
            egui::pos2(corner.x - row as f32 * line, if top { corner.y } else { corner.y - text_w })
        };

        // A leading dot sits between the corner and the text; the text shifts
        // away from the corner to make room for it.
        let pos = if entry.dot.is_some() {
            egui::pos2(pos.x, pos.y + if top { dot_lead } else { -dot_lead })
        } else {
            pos
        };
        if let Some(dot) = entry.dot {
            let dot_x = pos.x - text_h * 0.5;
            let dot_y = if top { corner.y + dot_lead * 0.5 } else { corner.y - dot_lead * 0.5 };
            painter.circle_filled(egui::pos2(dot_x, dot_y), (size * 0.3).max(2.0), dot);
        }

        outlined_rotated_text(painter, pos, angle, &entry.text, font.clone(), color);
    }
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
    // [`fitted`] for the shapes, and why the default distorts nothing. What of
    // the picture is shown depends on whether this is the magnified view and how
    // far it has been pushed in: every other tile shows it whole.
    let size = surface.size;
    if size.x > 0.0 && size.y > 0.0 {
        let whole = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let window = tile.picture_zoom.map_or(whole, PictureZoom::window);
        let dest = picture_area(size, rect, tile.aspect, tile.rotated);
        if tile.rotated {
            turned_canvas(painter, dest, window, surface);
        } else {
            painter.image(surface.id, dest, window, Color32::WHITE);
        }
    }
}

/// Draws the video rotated 90° clockwise. The source dimensions swap, a fixed
/// aspect ratio inverts, and the four corners are mapped to a [`Mesh`] so the
/// texture is sampled sideways without distorting it. `dest` is where the
/// picture was placed by [`picture_area`], and `window` the part of the source
/// the tile shows - all of it, unless the magnified view has been pushed in.
fn turned_canvas(painter: &egui::Painter, dest: Rect, window: Rect, surface: VideoSurface) {
    let uv = turned_corners(window);
    let mut mesh = Mesh::with_texture(surface.id);
    mesh.vertices = vec![
        Vertex { pos: dest.left_top(), uv: uv[0], color: Color32::WHITE },
        Vertex { pos: dest.right_top(), uv: uv[1], color: Color32::WHITE },
        Vertex { pos: dest.right_bottom(), uv: uv[2], color: Color32::WHITE },
        Vertex { pos: dest.left_bottom(), uv: uv[3], color: Color32::WHITE },
    ];
    mesh.indices = vec![0, 1, 2, 0, 2, 3];
    painter.add(Shape::Mesh(Arc::new(mesh)));
}

/// The texture coordinates the turned canvas puts on the four corners of a
/// source window, in its order: left-top, right-top, right-bottom, left-bottom.
///
/// A picture drawn sideways is sampled sideways - what the tile shows of the
/// source runs along the texture the other way - so its window turns with it:
/// the part of the picture that is further down lies further along the texture,
/// and the part further left lies further down it. The whole window is what that
/// canvas was written with, so a picture nobody has pushed in is drawn exactly
/// as it was before there was a window at all.
fn turned_corners(window: Rect) -> [egui::Pos2; 4] {
    let min = window.min;
    let max = window.max;
    [
        egui::pos2(min.y, 1.0 - min.x),
        egui::pos2(min.y, 1.0 - max.x),
        egui::pos2(max.y, 1.0 - max.x),
        egui::pos2(max.y, 1.0 - min.x),
    ]
}

/// Where a picture lands inside a tile: [`fitted`] or [`rotated_fitted`], by
/// whether the picture is drawn sideways.
///
/// The tile's own rectangle, the picture's size and the display mode it is fitted
/// by are all it needs, and it is what the painting and a gesture that pushes the
/// picture in both measure against - the gesture because it is aimed where the
/// fingers are, and the fingers are over this rectangle.
pub fn picture_area(size: Vec2, tile: Rect, aspect: TileAspect, rotated: bool) -> Rect {
    if rotated {
        rotated_fitted(size, tile, aspect)
    } else {
        fitted(size, tile, aspect)
    }
}

/// [`fitted`] for a picture turned sideways: the source width and height
/// trade places, and a fixed ratio is inverted.
fn rotated_fitted(source: Vec2, tile: Rect, aspect: TileAspect) -> Rect {
    let rotated_source = vec2(source.y, source.x);
    match aspect.ratio() {
        Some(ratio) => {
            let ratio = 1.0 / ratio;
            let (width, height) = if tile.width() / tile.height() > ratio {
                (tile.height() * ratio, tile.height())
            } else {
                (tile.width(), tile.width() / ratio)
            };
            Rect::from_center_size(tile.center(), vec2(width, height))
        }
        None if aspect == TileAspect::Stretch => tile,
        None => {
            let scale = (tile.width() / rotated_source.x).min(tile.height() / rotated_source.y);
            Rect::from_center_size(tile.center(), rotated_source * scale)
        }
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_of_every_layout_paint_inside_the_wall_without_overlap() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(1920.0, 1080.0));
        for layout in GridLayout::ALL {
            let rects: Vec<Rect> = (0..layout.capacity())
                .map(|cell| tile_rect(area, layout, cell, LINE))
                .collect();
            for (index, a) in rects.iter().enumerate() {
                for b in &rects[index + 1..] {
                    assert!(
                        a.intersect(*b).area() < 0.01,
                        "{layout:?}: cells {index} and a later one overlap"
                    );
                }
            }
        }
    }

    #[test]
    fn a_square_layout_is_still_the_uniform_grid() {
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(1600.0, 900.0));
        let rect = tile_rect(area, GridLayout::G2x2, 3, 0.0);
        assert!((rect.width() - 800.0).abs() < 0.01);
        assert!((rect.height() - 450.0).abs() < 0.01);
        assert!((rect.left() - 800.0).abs() < 0.01);
        assert!((rect.top() - 450.0).abs() < 0.01);
    }

    /// Pushing the picture in is aimed: what the fingers are over stays under
    /// them, so the detail they went for is the detail they get.
    #[test]
    fn a_pinch_keeps_what_it_is_aimed_at_under_the_fingers() {
        let dest = Rect::from_min_size(egui::pos2(100.0, 50.0), vec2(800.0, 450.0));
        let at = egui::pos2(dest.left() + dest.width() * 0.75, dest.center().y);
        let zoomed = PictureZoom::FULL.zoomed(2.0, at, dest);
        assert!((zoomed.scale - 2.0).abs() < 0.001, "{}", zoomed.scale);
        // The window is half the picture each way, and its left edge is `0.625
        // - 0.25`, which leaves the picture's own three quarters under the hands.
        let window = zoomed.window();
        assert!((window.left() - 0.375).abs() < 0.001, "{}", window.left());
        assert!((window.width() - 0.5).abs() < 0.001);
        // The one property that matters: read through the window, the hands are
        // over the same part of the picture as they were before the pinch.
        let under = |zoom: PictureZoom| {
            let window = zoom.window();
            window.min + ((at - dest.min) / dest.size()) * window.size()
        };
        assert!(
            (under(zoomed) - under(PictureZoom::FULL)).length() < 0.001,
            "the aim moved: {:?} to {:?}",
            under(PictureZoom::FULL),
            under(zoomed)
        );
        assert!((zoomed.center.y - 0.5).abs() < 0.001, "the aim was level with the middle");
    }

    /// The window's edges are the picture's edges, and there is nowhere past
    /// them to go: a drag asks for more of the picture and is given the edge.
    #[test]
    fn the_window_never_leaves_the_picture() {
        let dest = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(800.0, 400.0));
        let middle = PictureZoom::FULL.zoomed(4.0, dest.center(), dest);
        assert!((middle.window().left() - 0.375).abs() < 0.001, "a quarter either side of the middle");
        // At four times the window is a quarter of the picture, and a drag
        // crosses that much picture per `800 * 4` points: a tile and a half of
        // dragging puts the left edge of the window on the left edge of the
        // picture, and stops there.
        let dragged = middle.panned(vec2(1200.0, 0.0), dest);
        let window = dragged.window();
        assert!(window.left().abs() < 0.001, "{}", window.left());
        assert!((window.right() - 0.25).abs() < 0.001, "{}", window.right());
        // And a drag the other way is the right and bottom edges just the same.
        let back = dragged.panned(vec2(-9999.0, -9999.0), dest);
        let window = back.window();
        assert!((window.right() - 1.0).abs() < 0.001, "{}", window.right());
        assert!((window.bottom() - 1.0).abs() < 0.001, "{}", window.bottom());
    }

    /// Closing the fingers gives the whole picture back and centres it, and
    /// opening them stops at the limit rather than keeping on.
    #[test]
    fn a_pinch_stops_at_the_whole_picture_and_at_the_limit() {
        let dest = Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(800.0, 450.0));
        let at = egui::pos2(dest.left() + 20.0, dest.top() + 30.0);
        assert_eq!(
            PictureZoom::FULL.zoomed(0.25, at, dest),
            PictureZoom::FULL,
            "a pinch closed past whole is whole, and centred"
        );
        let pushed = PictureZoom::FULL.zoomed(100.0, dest.center(), dest);
        assert!((pushed.scale - PICTURE_ZOOM_MAX).abs() < 0.001, "{}", pushed.scale);
        assert!((pushed.center - Vec2::splat(0.5)).length() < 0.001, "aimed at the middle");
    }

    /// The turned canvas's own four corners: the whole window is the set of
    /// coordinates that canvas was written with, so a picture nobody has pushed
    /// in is drawn exactly as it was.
    #[test]
    fn a_turned_window_is_sampled_where_the_picture_puts_it() {
        let whole = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        assert_eq!(
            turned_corners(whole),
            [
                egui::pos2(0.0, 1.0),
                egui::pos2(0.0, 0.0),
                egui::pos2(1.0, 0.0),
                egui::pos2(1.0, 1.0),
            ],
            "the whole picture"
        );
        // The left half of the picture: what is left of it lies further down
        // the texture, and what is above it lies further to its left.
        let left = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.5, 1.0));
        assert_eq!(
            turned_corners(left),
            [
                egui::pos2(0.0, 1.0),
                egui::pos2(0.0, 0.5),
                egui::pos2(1.0, 0.5),
                egui::pos2(1.0, 1.0),
            ]
        );
    }
}
