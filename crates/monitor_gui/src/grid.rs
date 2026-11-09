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

use monitor_core::model::{
    CameraSource, ConnectionState, FrigateOrigin, Osd, OsdItem, StreamKind, TileAspect,
};
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
    /// The picture of this tile is already on the screen, scanned out of the
    /// activity's own surface underneath this window - so this tile must leave
    /// its area unpainted and let the compositor show it.
    ///
    /// Only ever true for the magnified viewport or a recording, and only
    /// while the decoder really is presenting it. A one-cell grid is not one:
    /// it has one cell but draws its own picture. See
    /// `XgViewApp::picture_on_layer`.
    pub on_video_layer: bool,
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
    /// The moment the picture is of, in seconds since the epoch, when it comes
    /// from a recording. `None` for a live stream, whose picture is of now.
    pub clock: Option<f64>,
    /// How the picture is fitted into the tile.
    pub aspect: TileAspect,
    /// Draw the picture rotated 90° clockwise. Used in portrait single-view
    /// so a landscape feed fills the upright screen sideways.
    pub rotated: bool,
    /// What the tile shows of its picture, where it is the magnified one.
    ///
    /// Every other tile shows all of it. See [`PictureZoom`].
    pub picture_zoom: Option<PictureZoom>,
    /// What this tile is showing of a recording, where it is showing one.
    ///
    /// `Some` is also the answer to "is this a recording": a recording that is
    /// being seeked reports a state a live stream reports when it has nothing
    /// to show, and it has something - the window being left is still on
    /// screen. See [`holds_last_picture`].
    pub recording: Option<Recording<'a>>,
}

/// What a tile's corners say about the recording it is showing.
///
/// Three facts, one of them a place and two of them what the picture is doing,
/// and they are one value because they are one answer: a tile is showing a
/// recording, and this is what there is to say about it. Held apart, `paused`
/// could be set on a tile that is not a recording, which is a state that means
/// nothing.
#[derive(Clone, Copy)]
pub struct Recording<'a> {
    /// Where the segments are being read from.
    ///
    /// The camera's stream address is not it: a recording comes off the server
    /// that holds it, and the two are different answers to the same corner.
    pub origin: &'a FrigateOrigin,
    /// The picture is not moving - held by the viewer, or out of range.
    pub paused: bool,
    /// The rate it is playing at, where it is not the normal one.
    ///
    /// `None` at `1x`: a corner that always says `1x` spends its width on the
    /// default, and the corner next to it - the frame rate - only needs
    /// explaining when it is multiplied.
    pub speed: Option<&'static str>,
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
    /// Where the pointer was when the tile was clicked, if it was.
    ///
    /// The magnified viewport's one tile covers the whole wall, and the bar it
    /// carries is drawn by the application rather than as a widget of its own:
    /// this is the position that tells a press on a control from one on the
    /// picture. See [`crate::tilebar`].
    pub click_pos: Option<egui::Pos2>,
    /// Where the pointer was on a double click.
    ///
    /// Read to keep a double click in the bar's activation band from unwinding
    /// the magnification: a press there is the bar being asked for.
    pub double_click_pos: Option<egui::Pos2>,
    /// Where the pointer is over the tile, if it is over it at all.
    pub hover_pos: Option<egui::Pos2>,
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
    // Whether the picture on screen is being held while a stream is on its way:
    // a switch, a seek, or a channel that was parked and is being opened again.
    let holding = holds_last_picture(state, tile.recording.is_some(), tile.video.is_some());
    match state {
        // A recording that has played to its end keeps the last picture it
        // drew, exactly as a stalled live stream does: there is nothing to
        // report and nothing to cover it with.
        ConnectionState::Streaming | ConnectionState::Stalled | ConnectionState::Finished => {
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
            if holding {
                // A recording being seeked: the picture is the window being
                // left, and the next one is a request away.
                live_canvas(&painter, body, tile, tier);
            } else {
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
        } else if holding && matches!(state, ConnectionState::Connecting | ConnectionState::Reconnecting) {
            // The same wait without a stream to name: a channel that was parked
            // and is being opened again holds the last picture it had, and says
            // nothing about it - a still picture with no reason given, which
            // reads as a camera that has stopped rather than one being fetched.
            let width = (rect.width() - theme::space::L).clamp(120.0, 430.0);
            let band = Rect::from_center_size(rect.center(), vec2(width, 34.0));
            painter.rect_filled(band, CornerRadius::ZERO, Color32::from_black_alpha(205));
            spinner(&painter, band.left_center() + vec2(22.0, 0.0), 10.0, tile.time, theme::FOCUS);
            painter.text(
                band.left_center() + vec2(42.0, 0.0),
                Align2::LEFT_CENTER,
                monitor_i18n::tr(state.label()),
                FontId::proportional(13.0),
                theme::FOCUS,
            );
        }
    }

    // The on-screen display, in the corners the settings panel chose. It goes
    // over whatever the state left on the tile, so that a stream that failed
    // still says which camera it is. When the picture is turned sideways the
    // OSD corners trade places, but the text still lives on the whole tile.
    //
    // Not while the picture is scanned out of a surface of its own: those
    // corners are this wall's, drawn where the *tile* is, and the picture is
    // narrower than the tile as soon as it is not stretched. The wall is under
    // the picture, so the copy used to be covered by it and never seen - with
    // the picture fitted it shows in the bands, twice over, beside the copy the
    // overlay draws in the picture's own corners. See `draw_overlay_only`,
    // which is that copy.
    if !tile.on_video_layer {
        osd(&painter, body, tile);
    }

    if tile.focused {
        ring(&painter, rect);
    }

    response
}

/// Draws the four corners of a tile's on-screen display.
///
/// Public because the magnified viewport is painted twice: by the wall, which is
/// under the picture, and by the strip over it. The corners are the same answer
/// either way, and this is the one place that knows what they say.
pub(crate) fn osd(painter: &egui::Painter, body: Rect, tile: &Tile<'_>) {
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
    /// The mark drawn before the text, in a colour of its own.
    mark: Option<OsdMark>,
    text: String,
}

/// The mark that leads a line of a tile's on-screen display.
///
/// One place, two questions. A dot is the channel's state - whether the picture
/// is moving is the one thing a wall of still images cannot say about itself -
/// and a triangle is a recording being played, which is not a state of the
/// stream at all but a different kind of picture. The bars are that recording
/// held still: the same picture, doing the other thing.
#[derive(Clone, Copy)]
enum OsdMark {
    Dot(Color32),
    Play(Color32),
    Pause(Color32),
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
    let codec = || OsdLine { mark: None, text: format!("{} {}", codec_name(channel), kbps_text(channel)) };
    let fps = || OsdLine { mark: None, text: format!("{} {}", hardware, fps_text(channel)) };
    let format = || OsdLine {
        mark: None,
        text: match channel.and_then(|channel| channel.width.zip(channel.height)) {
            Some((width, height)) => format!("{}x{} {}", width, height, monitor_i18n::tr(tile.aspect.label())),
            None => format!("?x? {}", monitor_i18n::tr(tile.aspect.label())),
        },
    };
    // The connection the channel pulls over, then the address it is pulling -
    // the mode the switch is on, and the url that follows it. The address is
    // masked the way the settings row masks it, so a password or a stream key
    // never reaches the picture, and shortened to the room a corner has.
    //
    // A recording is pulled from somewhere else, and this is the corner that
    // says so: the transport is the web the server answers on - `https` reads
    // `HTTPS`, because the port a viewer logged in against is not the same one
    // as the open one - and the address is that server's api for this camera,
    // without its scheme, since the word before it is already the scheme.
    let transport = || OsdLine {
        mark: None,
        text: match tile.recording {
            Some(recording) => recording_address(recording.origin),
            None => format!(
                "{} {}",
                camera.connection_label(),
                theme::truncate(
                    &camera.masked_uri(channel.and_then(|channel| channel.stream).unwrap_or(StreamKind::Main)),
                    34
                )
            ),
        },
    };
    match item {
        OsdItem::Off => Vec::new(),
        OsdItem::Name => vec![OsdLine { mark: None, text: camera.short_label(24) }],
        OsdItem::NumberName => vec![OsdLine {
            mark: None,
            text: match tile.index {
                Some(index) => format!("{} {}", index + 1, camera.short_label(22)),
                None => camera.short_label(24),
            },
        }],
        OsdItem::Stream => vec![stream_line(tile.recording.as_ref(), channel)],
        OsdItem::Codec => vec![codec()],
        OsdItem::Fps => vec![fps()],
        OsdItem::Format => vec![format()],
        OsdItem::Transport => vec![transport()],
        OsdItem::DateTime => {
            // A recording is drawn in the hours it happened in, not in these:
            // the corner says which moment the picture is, and for a live
            // stream that is now. For a recording it is the moment its frame
            // is of - or the moment being fetched, until a frame arrives, so
            // that a seek does not read as "now" for the second it takes.
            let moment = match tile.clock {
                Some(at) => monitor_core::blackout::local_at(at as i64),
                None => monitor_core::blackout::local_now(),
            };
            let Some(now) = moment else {
                return Vec::new();
            };
            vec![OsdLine {
                mark: None,
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

/// Whether a channel has measured a rate yet.
///
/// The first sample a session publishes names the codec and the size - they are
/// in the init segment, before any picture - and its rate is zero, because there
/// has been nothing to measure over. Printing that zero says the stream is not
/// moving, which is a different thing from not knowing yet, and a fresh
/// recording takes a while to get there: the login, the playlist and the first
/// fragment are all read before a picture exists, and the window that measures
/// the rate only closes two seconds after that.
///
/// So a rate is a dash until the channel has counted a frame, the way the codec
/// is a dash until the stream has described itself.
fn rate_measured(channel: Option<&ChannelUi>) -> bool {
    channel.is_some_and(|channel| channel.total_frames > 0)
}

/// The rate the stream arrives at, or a dash while there is none to report.
fn kbps_text(channel: Option<&ChannelUi>) -> String {
    match rate_measured(channel) {
        true => format!("{:.0}kbps", channel.map_or(0.0, |channel| channel.bitrate_kbps)),
        false => "—kbps".to_string(),
    }
}

/// The rate the pictures arrive at, or a dash while there is none to report.
fn fps_text(channel: Option<&ChannelUi>) -> String {
    match rate_measured(channel) {
        true => format!("{:.2}fps", channel.map_or(0.0, |channel| channel.fps)),
        false => "—fps".to_string(),
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

/// The line the stream item draws.
///
/// A dot is the picture moving - the one question a wall of still images cannot
/// answer from the images themselves - and it is coloured by the channel's
/// state. A recording is the other kind of picture and is drawn as one: a
/// triangle, the mark every player uses for "this is playing", and the word
/// that says what is playing. Which stream it was pulled as is not a question a
/// recording has an answer to - a recording is one stream, and it is the one
/// that was written down.
fn stream_line(recording: Option<&Recording<'_>>, channel: Option<&ChannelUi>) -> OsdLine {
    if let Some(recording) = recording {
        // The word does not change when the picture stops: it is still a
        // recording, and the mark is what says which of the two things it is
        // doing. The rate goes beside it, and only when it is not the normal
        // one - a corner that always says `1x` spends its width on the default.
        let mark = if recording.paused {
            OsdMark::Pause(theme::ACCENT)
        } else {
            OsdMark::Play(theme::ACCENT)
        };
        let word = monitor_i18n::tr("osd-playback");
        let text = match recording.speed {
            Some(speed) => format!("{word} {speed}"),
            None => word,
        };
        return OsdLine { mark: Some(mark), text };
    }
    OsdLine {
        mark: Some(match channel.map(|channel| channel.state) {
            Some(ConnectionState::Streaming) => OsdMark::Dot(theme::LIVE),
            Some(ConnectionState::Stalled) => OsdMark::Dot(theme::WARN),
            _ => OsdMark::Dot(theme::TEXT_DIM),
        }),
        text: channel
            .and_then(|channel| channel.stream)
            .map(|stream| monitor_i18n::tr(stream.tag()).to_string())
            .unwrap_or_else(|| "—".to_string()),
    }
}

/// The transport item's words for a recording: the web the server answers on,
/// and the api this camera's segments are read from.
///
/// The scheme is left off the address because the first word already is it, and
/// the room a corner has goes to the part that names the server instead.
fn recording_address(origin: &FrigateOrigin) -> String {
    format!(
        "{} {}",
        origin.transport_label(),
        theme::truncate(strip_scheme(&origin.masked_api_base()), 34)
    )
}

/// An address without the `scheme://` in front of it.
///
/// The transport word is drawn beside it and already names the scheme, so the
/// room a corner has is better spent on the part that says *which* server.
fn strip_scheme(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// Draws the mark that leads a line, turned by `angle` with the text it leads.
///
/// A dot is a circle and does not care which way it is turned; a triangle and a
/// pair of bars do, and on a picture drawn sideways they point along the line
/// they begin rather than across it.
///
/// The three are the same size: the play triangle is as tall as the dot is wide
/// and the pause bars are as tall as that too, so the mark does not change
/// weight when a recording is paused. What differs is how much of that box they
/// fill, which is the nature of the shapes - a circle fills all of it, a triangle
/// and two bars rather less.
fn draw_mark(painter: &egui::Painter, at: egui::Pos2, radius: f32, mark: OsdMark, angle: f32) {
    let (sin, cos) = angle.sin_cos();
    let turn = |offset: Vec2| vec2(offset.x * cos - offset.y * sin, offset.x * sin + offset.y * cos);
    match mark {
        OsdMark::Dot(colour) => {
            painter.circle_filled(at, radius, colour);
        }
        OsdMark::Play(colour) => {
            let r = radius;
            let corners = [vec2(-r * 0.85, -r), vec2(r * 0.85, 0.0), vec2(-r * 0.85, r)];
            painter.add(egui::Shape::convex_polygon(
                corners.map(|offset| at + turn(offset)).to_vec(),
                colour,
                Stroke::NONE,
            ));
        }
        OsdMark::Pause(colour) => {
            // Two bars as tall as the dot is wide, a bar and a gap apart, with
            // the pair centred on the same point the dot would be.
            let r = radius;
            let (bar, gap) = (r * 0.55, r * 0.4);
            for side in [-1.0, 1.0] {
                let centre = at + turn(vec2(side * (bar + gap) * 0.5, 0.0));
                let rect = Rect::from_center_size(centre, vec2(bar, r * 2.0));
                painter.rect_filled(rect, CornerRadius::same((bar * 0.25) as u8), colour);
            }
        }
    }
}

/// Where the ink of a line sits, measured from the top of its galley.
///
/// Not the middle of the box. A line box is the font's ascent and descent, and
/// the ink inside it is not centred in it: a hanzi fills the box, while
/// `Playback` hangs from its capitals down to a descender. A mark placed at the
/// box's middle therefore sits low beside Chinese and looks right beside
/// English - a difference only one of the two languages ever sees, which is why
/// it went unnoticed for so long.
///
/// Every glyph carries the rectangle its ink is drawn in (`uv_rect`), so this is
/// the union of them. A space has no ink and is skipped; a line with nothing
/// drawn at all has none, and the caller falls back to the middle of the box.
fn ink_centre(galley: &egui::Galley) -> Option<f32> {
    let mut ink: Option<Rect> = None;
    for row in &galley.rows {
        for glyph in &row.row.glyphs {
            if glyph.uv_rect.is_nothing() {
                continue;
            }
            let top_left = row.pos + glyph.pos.to_vec2() + glyph.uv_rect.offset;
            let rect = Rect::from_min_size(top_left, glyph.uv_rect.size);
            ink = Some(ink.map_or(rect, |ink| ink.union(rect)));
        }
    }
    ink.map(|ink| ink.center().y)
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
    // The same layout says where the ink of the line sits, which is where the
    // mark goes - see [`ink_centre`].
    let dot_lead = if lines.iter().any(|line| line.mark.is_some()) { size * 1.15 } else { 0.0 };
    let galleys: Vec<_> = lines
        .iter()
        .map(|line| painter.layout_no_wrap(line.text.clone(), font.clone(), color))
        .collect();
    let text_width = galleys.iter().map(|galley| galley.size().x).fold(0.0_f32, f32::max);
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
        if let Some(mark) = entry.mark {
            let lead = if left { dot_lead } else { -dot_lead };
            let centre = ink_centre(&galleys[row]).unwrap_or(line * 0.5);
            let at = egui::pos2(x + lead * 0.4, y + centre);
            draw_mark(painter, at, (size * 0.3).max(2.0), mark, 0.0);
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

    let dot_lead = if lines.iter().any(|line| line.mark.is_some()) { size * 1.15 } else { 0.0 };
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
        let pos = if entry.mark.is_some() {
            egui::pos2(pos.x, pos.y + if top { dot_lead } else { -dot_lead })
        } else {
            pos
        };
        if let Some(mark) = entry.mark {
            // The line's own height runs across the screen here, so the mark is
            // centred on the ink along x: `pos` is the text's top-left in its
            // turned frame, and the ink sits that far left of it.
            let centre = ink_centre(&galleys[row]).unwrap_or(text_h * 0.5);
            let dot_x = pos.x - centre;
            let dot_y = if top { corner.y + dot_lead * 0.5 } else { corner.y - dot_lead * 0.5 };
            let at = egui::pos2(dot_x, dot_y);
            draw_mark(painter, at, (size * 0.3).max(2.0), mark, std::f32::consts::FRAC_PI_2);
        }

        outlined_rotated_text(painter, pos, angle, &entry.text, font.clone(), color);
    }
}

/// Whether a channel that is not streaming keeps the picture it already has.
///
/// The one case this is for is a recording being seeked. A seek replaces the
/// channel, so the new one reports what a live stream reports when it has
/// nothing to show - and it has something: the window being left is still on
/// screen, and the next one is a request away. Covering it with the pending
/// canvas turns every drag into a black frame with a spinner on it.
///
/// Only the first attempt holds it. `Reconnecting` is an attempt that failed,
/// and a seek that fails has to say so rather than leave the viewer looking at
/// a picture that is not moving for a reason nobody named. A channel that has
/// never drawn anything has nothing to keep either, and a live stream that
/// dropped is saying something the viewer should see.
fn holds_last_picture(state: ConnectionState, recording: bool, has_picture: bool) -> bool {
    if !has_picture {
        return false;
    }
    match state {
        // A picture that is on screen and a request behind it: a recording
        // being seeked, or a channel switching streams.
        ConnectionState::Connecting => true,
        // A live stream that was cut keeps its last picture while it comes
        // back. The corner already says it is reconnecting, and a panel over
        // the picture says the same thing worse - the viewer loses the picture
        // they had in exchange for nothing. A recording is the exception: a
        // seek that fails has to say so rather than leave a still picture
        // sitting there with no explanation.
        ConnectionState::Reconnecting => !recording,
        _ => false,
    }
}

/// Paints the newest decoded pictures of a live channel, or a placeholder while
/// the first one is on its way.
fn live_canvas(painter: &egui::Painter, rect: Rect, tile: &Tile<'_>, tier: Tier) {
    // The picture is already on the screen, scanned out of the activity's own
    // surface under this window: painting the canvas colour here would cover it,
    // and painting the texture would be the very copy the layer exists to avoid.
    // What is drawn on top of this tile - its corners, its ring - still goes on
    // above the picture, which is the point of putting the layer underneath.
    if tile.on_video_layer {
        return;
    }
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

    /// A picture that is on screen stays there while the request behind it is
    /// answered - a recording being seeked, a channel switching streams, a live
    /// stream that was cut and is coming back.
    #[test]
    fn a_picture_stays_up_while_it_is_being_asked_for_again() {
        // Opening, with something already on screen.
        assert!(holds_last_picture(ConnectionState::Connecting, true, true));
        assert!(holds_last_picture(ConnectionState::Connecting, false, true));
        // Reconnecting: a live stream keeps its picture, a recording does not -
        // a seek that does not come back must say so rather than leave a still
        // picture sitting there unexplained.
        assert!(holds_last_picture(ConnectionState::Reconnecting, false, true));
        assert!(!holds_last_picture(ConnectionState::Reconnecting, true, true));
        // Nothing on screen is nothing to hold.
        assert!(!holds_last_picture(ConnectionState::Connecting, false, false));
        assert!(!holds_last_picture(ConnectionState::Reconnecting, false, false));

        // A channel that is streaming draws its picture anyway, and one that is
        // idle has been stopped rather than asked for.
        for state in [ConnectionState::Streaming, ConnectionState::Idle, ConnectionState::Failed] {
            assert!(!holds_last_picture(state, true, true), "{state:?}");
        }
    }

    /// A recording is the other kind of picture, and the corner that says
    /// whether the picture is moving says so: a triangle and a word, not a dot
    /// and a stream. Which stream a recording was written as is not a question
    /// it can answer - there is one of them, and it is the one on the disk.
    ///
    /// Held still, it is the bars - and the word stays: the picture is still a
    /// recording, and only what it is doing has changed.
    #[test]
    fn a_recording_is_drawn_as_playing_and_a_live_stream_as_moving() {
        monitor_i18n::init("en", &[]);
        let origin = FrigateOrigin {
            base_url: "http://frigate.lan:5000".to_string(),
            camera: "front_door".to_string(),
        };
        let at = |paused, speed| Recording { origin: &origin, paused, speed };

        let playing = stream_line(Some(&at(false, None)), None);
        assert!(matches!(playing.mark, Some(OsdMark::Play(_))), "a triangle, not a dot");
        assert_eq!(playing.text, "Playback");

        // Held still - by the viewer, or because the range ran out - the bars
        // go up, and the word stays: it is still a recording.
        let held = stream_line(Some(&at(true, None)), None);
        assert!(matches!(held.mark, Some(OsdMark::Pause(_))), "bars where the triangle was");
        assert_eq!(held.text, playing.text, "and the same word beside them");

        // A rate that is not the normal one is written beside the word, because
        // it is what explains the frame rate in the corner next door.
        let fast = stream_line(Some(&at(false, Some("8×"))), None);
        assert_eq!(fast.text, "Playback 8×");
        assert!(matches!(fast.mark, Some(OsdMark::Play(_))), "still playing");

        // A live channel keeps its dot, and its colour is its state.
        let mut channel = ChannelUi::default();
        channel.state = ConnectionState::Streaming;
        channel.stream = Some(StreamKind::Sub);
        let live = stream_line(None, Some(&channel));
        assert!(matches!(live.mark, Some(OsdMark::Dot(colour)) if colour == theme::LIVE));
        assert_eq!(live.text, monitor_i18n::tr("stream-tag-sub"));

        // A channel with nothing to say is a grey dot and a dash, as before.
        let idle = stream_line(None, None);
        assert!(matches!(idle.mark, Some(OsdMark::Dot(colour)) if colour == theme::TEXT_DIM));
        assert_eq!(idle.text, "—");
    }

    /// A rate nobody has measured is a dash, not a zero.
    ///
    /// The sample that names the codec is published before the first picture and
    /// carries a rate of zero, because nothing has been counted yet. On screen
    /// that zero is indistinguishable from a stream that has stopped - and a
    /// recording that has just been opened spends its first seconds reading a
    /// login, a playlist and a fragment before there is anything to count.
    #[test]
    fn a_rate_nobody_has_measured_is_a_dash() {
        let mut channel = ChannelUi::default();
        assert_eq!(kbps_text(Some(&channel)), "—kbps", "the codec is known, the rate is not");
        assert_eq!(fps_text(Some(&channel)), "—fps");

        // One frame counted and there is something to report.
        channel.total_frames = 1;
        channel.bitrate_kbps = 1800.0;
        channel.fps = 25.0;
        assert_eq!(kbps_text(Some(&channel)), "1800kbps");
        assert_eq!(fps_text(Some(&channel)), "25.00fps");

        // A held recording is not measured again, so the last number stands -
        // it is a measurement, and the corner keeps it.
        channel.fps = 0.0;
        assert_eq!(fps_text(Some(&channel)), "0.00fps", "a measured zero is a zero");

        assert_eq!(kbps_text(None), "—kbps", "and no channel at all is no measurement");
    }

    /// The mark is placed on the ink, not on the middle of the line box.
    ///
    /// The two are not the same place: a box is the font's ascent and descent,
    /// and what is drawn inside it sits where the letters put it. A dot in the
    /// middle of the box is what put the mark low beside Chinese and high
    /// beside English, and it is the reason the corners looked misaligned.
    #[test]
    fn the_mark_sits_on_the_ink_and_not_in_the_middle_of_the_box() {
        let ctx = egui::Context::default();
        let mut measured = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let lay = |text: &str| {
                ctx.fonts(|fonts| {
                    fonts.layout_no_wrap(text.to_owned(), FontId::monospace(40.0), Color32::WHITE)
                })
            };
            measured = Some((lay("Playback"), lay("   ")));
        });
        let (text, spaces) = measured.expect("a frame is laid out");

        let ink = ink_centre(&text).expect("a line of letters has ink");
        assert!(
            (ink - text.size().y * 0.5).abs() > 0.5,
            "the ink centre {ink} is not the box's middle {}",
            text.size().y * 0.5
        );
        // Nothing drawn is nothing to centre on, and the caller falls back to
        // the box: a line of spaces is what a corner must not panic on.
        assert_eq!(ink_centre(&spaces), None, "a space has no ink");
    }

    /// The transport corner names the server the segments come from, not the
    /// camera's own address: the camera is not the thing being watched.
    #[test]
    fn a_recording_names_the_server_it_is_read_from() {
        let plain = FrigateOrigin {
            base_url: "http://192.168.17.88:5000/".to_string(),
            camera: "front_door".to_string(),
        };
        assert_eq!(recording_address(&plain), "HTTP 192.168.17.88:5000/api/front_door");

        // The scheme is the transport word, so it is not repeated in the address.
        let secure = FrigateOrigin {
            base_url: "https://frigate.lan:8971".to_string(),
            camera: "gate".to_string(),
        };
        assert_eq!(recording_address(&secure), "HTTPS frigate.lan:8971/api/gate");
        assert!(!recording_address(&secure).contains("://"), "one scheme, one word");

        // And a password in the base url never reaches the picture.
        let secret = FrigateOrigin {
            base_url: "https://user:hunter2@frigate.lan:8971".to_string(),
            camera: "gate".to_string(),
        };
        assert!(!recording_address(&secret).contains("hunter2"));
    }
}
