//! The shapes the controls carry, drawn rather than typed.
//!
//! egui's bundled fonts include an icon set, but nothing in the build guarantees
//! the coverage of the glyph asked for, and a missing one is drawn as a box -
//! which on a television is worse than carrying no icon at all. These are a
//! handful of shapes, drawn with the same painter the tiles use, so they are the
//! same on every target.
//!
//! Each icon is defined inside a unit square and mapped onto whatever rectangle
//! it is asked for, which is what lets one definition serve the bar and anything
//! else that needs it.

use egui::{
    Color32, CornerRadius, Id, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2, vec2,
};

use crate::theme;

/// A shape a control can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Add something: two strokes crossing.
    Plus,
    /// Settings: a cogwheel.
    Gear,
    /// Enter full screen: four corners opening outwards.
    Expand,
    /// Leave full screen: four corners closing inwards.
    Collapse,
    /// Back to the wall: four tiles.
    Grid,
    /// Edit something: a pencil.
    Edit,
    /// Remove something: a bin.
    Trash,
    /// Move something up a list: a chevron pointing up.
    Up,
    /// Move something down a list: a chevron pointing down.
    Down,
    /// Move something left: a chevron pointing left.
    Left,
    /// Move something right: a chevron pointing right.
    Right,
    /// Close something: two strokes crossed.
    Close,
    /// Turn upright: a tall viewport.
    Portrait,
    /// Turn on its side: a widescreen viewport.
    Landscape,
    /// Adjust the picture fit: a frame with arrows pointing in and out.
    AspectRatio,
    /// Sound on: a speaker with waves leaving it.
    Sound,
    /// Sound off: a speaker with a cross where the waves were.
    Mute,
    /// Talk-back idle: a microphone on its stand.
    Mic,
    /// End the call: a handset put down, the shape a phone's hang-up key carries.
    Hangup,
    /// Turn the magnified picture a quarter turn: an arrow curving round a
    /// square, the shape a phone means by "rotate".
    Rotate,
    /// The way into a camera's recordings: a screen with the play mark in it -
    /// something to open, rather than a clock to read.
    History,
    /// Start playing: a solid triangle.
    Play,
    /// Hold the picture: two solid bars.
    Pause,
    /// Back over the recording by ten seconds: a ring open at the left, a
    /// double arrowhead at its top pointing back, and the number it steps by
    /// inside it.
    StepBack,
    /// The same, forwards.
    StepForward,
    /// The event before this moment: a ring open at the lower left, a
    /// corner-shaped arrowhead at its upper left, and an exclamation mark inside
    /// it - the shape a viewer of a camera app already knows as "step to what
    /// happened".
    EventBack,
    /// The event after this moment: the same, mirrored, pointing forward.
    EventForward,
    /// How much of the recording the timeline shows: two open rings facing each
    /// other with a double-headed arrow through them, which is what "wider or
    /// narrower" looks like without a word.
    Zoom,
    /// Back to the camera as it is now: a video camera, body and lens - the
    /// picture itself, since that is what the button switches to.
    Live,
}

/// Side of the square an icon button occupies, in points.
///
/// Sized as a touch target rather than as a glyph: the bar is used with a finger
/// on a tablet, and this is about the smallest thing a finger hits reliably.
const BUTTON: f32 = 30.0;

/// Fraction of the button the drawing itself takes.
const GLYPH: f32 = 0.5;

/// The line weight a glyph of a given extent is drawn with.
///
/// A glyph keeps the same weight relative to its own size whatever the button,
/// so that a small copy in a form is not drawn with the heavy lines the toolbar
/// can afford: at a 10 point glyph the toolbars 1.6 would close the gap between
/// the two arcs of the speaker and turn the shape into a blob.
fn stroke_width(extent: f32) -> f32 {
    (extent * 0.1).clamp(1.0, 1.6)
}

/// An icon button: a square carrying `icon`, with `tooltip` under the pointer.
///
/// The icon is the whole label, which is why the tooltip is not optional - it is
/// the only place a viewer who does not recognise the shape can read the word.
/// `selected` paints the button as held down, the state the bar uses for the
/// panels that are open.
pub fn button(ui: &mut Ui, icon: Icon, selected: bool, tooltip: &str) -> Response {
    button_sized(ui, icon, selected, false, tooltip, BUTTON)
}

/// An icon button of a given side, for a row where a control the size of the
/// toolbar's would dwarf the text beside it.
///
/// `muted` draws the shape in the dim colour, for a button that is on screen
/// but has nothing to do this frame - the end of a list, where a move would go
/// nowhere. It stays focusable, so that the walk across the row does not shift
/// under the viewer when the button loses its purpose.
pub fn button_sized(
    ui: &mut Ui,
    icon: Icon,
    selected: bool,
    muted: bool,
    tooltip: &str,
    side: f32,
) -> Response {
    button_sized_glyph(ui, icon, selected, muted, tooltip, side, GLYPH)
}

/// [`button_sized`] whose drawing takes `glyph` of the button rather than the
/// usual [`GLYPH`].
///
/// A row that sizes its buttons to the text height leaves a square only as tall
/// as a line, and the usual half of it is a shape that looks lost beside the
/// words. A larger share fills that square without raising the row.
pub fn button_sized_glyph(
    ui: &mut Ui,
    icon: Icon,
    selected: bool,
    muted: bool,
    tooltip: &str,
    side: f32,
    glyph: f32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    paint_button(ui, rect, &response, icon, selected, muted, side, glyph, false, None);
    hover_text(response, tooltip)
}

/// [`button_sized`] carrying an explicit [`Id`].
///
/// egui's automatic ids are seeded from the widget's place among its siblings,
/// so they change the moment a row moves. A control that is itself rearranged -
/// a move button in the camera list - has to keep its id, or the keyboard focus
/// is dropped as soon as its row changes place. The id is the caller's, derived
/// from what the control belongs to rather than from where it is drawn.
pub fn button_at(
    ui: &mut Ui,
    id: Id,
    icon: Icon,
    selected: bool,
    muted: bool,
    tooltip: &str,
    side: f32,
) -> Response {
    // The space is allocated first - it is what lays the row out - and the
    // interaction is then claimed under the caller's id.
    let (_auto, rect) = ui.allocate_space(Vec2::splat(side));
    button_in(ui, rect, id, icon, selected, muted, tooltip, false)
}

/// A button drawn in a rectangle the caller has placed.
///
/// The magnified viewport's bar lays its controls out in the picture's own
/// frame, so their rectangles are known before egui's layout runs; everything
/// else about the button - the interaction, the frame, the focus ring - is
/// egui's own. `turned` draws the glyph with the picture, see [`paint_turned`].
pub fn button_in(
    ui: &mut Ui,
    rect: Rect,
    id: Id,
    icon: Icon,
    selected: bool,
    muted: bool,
    tooltip: &str,
    turned: bool,
) -> Response {
    // A muted button is inert: drawn so the row keeps its shape, but nothing
    // can land on it - the pointer cannot click it and the keyboard walk cannot
    // focus it, because there is nothing it could do. `Sense::hover` keeps it
    // out of both, while `Sense::click` would put it in the focus order.
    let sense = if muted { Sense::hover() } else { Sense::click() };
    let response = ui.interact(rect, id, sense);
    paint_button(ui, rect, &response, icon, selected, muted, rect.width(), GLYPH, turned, None);
    hover_text(response, tooltip)
}

/// The red control a call ends with: [`button_in`] painted in the colour the
/// bars use for danger.
///
/// The frame is the warning red rather than the widget's fill, so the one
/// control a call has cannot be mistaken for another; the lit look a press and
/// the focus give it are still egui's.
pub fn danger_in(
    ui: &mut Ui,
    rect: Rect,
    id: Id,
    icon: Icon,
    tooltip: &str,
    turned: bool,
) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    paint_button(ui, rect, &response, icon, false, false, rect.width(), GLYPH, turned, Some(theme::ERROR));
    hover_text(response, tooltip)
}

/// [`Response::on_hover_text`], with an empty tooltip meaning none: the bar's
/// controls carry no word of their own, and an empty box would still be drawn.
fn hover_text(response: Response, tooltip: &str) -> Response {
    if tooltip.is_empty() {
        response
    } else {
        response.on_hover_text(tooltip)
    }
}

/// Draws a button's frame and glyph; the allocation is the caller's, so that
/// the automatic and the explicit id forms share one painting. `glyph` is the
/// share of the button the drawing takes, see [`button_sized_glyph`]; `turned`
/// draws the glyph with the picture, and `fill` overrides the frame's colour for
/// the one button that is a warning rather than a control.
#[allow(clippy::too_many_arguments)]
fn paint_button(
    ui: &Ui,
    rect: Rect,
    response: &Response,
    icon: Icon,
    selected: bool,
    muted: bool,
    side: f32,
    glyph: f32,
    turned: bool,
    fill: Option<Color32>,
) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let visuals = ui.style().interact_selectable(response, selected);
    let corner = CornerRadius::same(theme::radius::M);
    let frame = rect.expand(visuals.expansion);
    ui.painter().rect_filled(frame, corner, fill.unwrap_or(visuals.weak_bg_fill));
    if fill.is_some() && (response.hovered() || response.is_pointer_button_down_on()) {
        // A filled button still answers the pointer: the frame's colour is the
        // button's own, so the light a hover and a press give it is laid over it.
        ui.painter().rect_filled(frame, corner, Color32::from_white_alpha(18));
    }
    if visuals.bg_stroke.width > 0.0 {
        // The focus ring arrives this way: egui paints `widgets.active` for
        // whatever has the keyboard focus, and that state carries a stroke.
        ui.painter().rect_stroke(frame, corner, visuals.bg_stroke, StrokeKind::Inside);
    }
    // The five that carry a number, a mark or two heads need a little more of
    // the button than a plain shape, because their drawing fills less of its own
    // square (a ring, not a full-width stroke) and the digits need room. A
    // fifth more brings them to the same visual size as the rest.
    let detail = matches!(
        icon,
        Icon::StepBack | Icon::StepForward | Icon::EventBack | Icon::EventForward | Icon::Zoom
    );
    let extent = side * if detail { glyph * 1.15 } else { glyph };
    let glyph_rect = Rect::from_center_size(rect.center(), Vec2::splat(extent));
    // The theme sets `override_text_color` so that every label draws in the
    // project's text colour; egui's text widgets take it too, so an icon that
    // sits beside a text button has to as well, or it reads as a dimmer kind of
    // button. `muted` is the one case that is deliberately dim.
    let color = if fill.is_some() {
        Color32::WHITE
    } else if muted {
        theme::TEXT_DIM
    } else {
        ui.visuals()
            .override_text_color
            .unwrap_or_else(|| visuals.text_color())
    };
    paint_turned(ui.painter(), glyph_rect, icon, Stroke::new(stroke_width(extent), color), turned);
}

/// Draws `icon` filling `rect`.
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, stroke: Stroke) {
    paint_turned(painter, rect, icon, stroke, false);
}

/// A point of the unit square placed on `rect`, turned with the picture when
/// `turned`.
fn unit_at(rect: Rect, turned: bool, x: f32, y: f32) -> Pos2 {
    if turned {
        Pos2::new(rect.right() - y * rect.height(), rect.top() + x * rect.width())
    } else {
        Pos2::new(rect.left() + x * rect.width(), rect.top() + y * rect.height())
    }
}

/// The unit square's \((x_0, y_0) - (x_1, y_1)\) placed on `rect`, turned with
/// the picture when `turned`.
///
/// The two corners are normalized rather than taken as minimum and maximum: a
/// quarter turn swaps the direction of both axes, so the corner that was the
/// square's top-left is the screen's top-right once it is turned, and
/// `Rect::from_min_max` would build an inverted, empty rectangle. That is how
/// the microphone lost its capsule and the picture fit its frame once the bar
/// turned with a portrait wall.
fn unit_rect(rect: Rect, turned: bool, x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect::from_two_pos(unit_at(rect, turned, x0, y0), unit_at(rect, turned, x1, y1))
}

/// Draws `icon` filling `rect`, turned a quarter turn clockwise when `turned`.
///
/// The turn is the one the wall takes for a portrait picture: the unit square's
/// left edge becomes the screen's top edge. A bar that turns with the picture
/// carries its icons turned the same way.
pub fn paint_turned(painter: &Painter, rect: Rect, icon: Icon, stroke: Stroke, turned: bool) {
    let at = |x: f32, y: f32| unit_at(rect, turned, x, y);
    let square = |x0: f32, y0: f32, x1: f32, y1: f32| unit_rect(rect, turned, x0, y0, x1, y1);
    let line = |points: &[Pos2]| Shape::line(points.to_vec(), stroke);
    // The speaker both sound icons carry, so the two read as one control.
    let speaker = || {
        line(&[
            at(0.10, 0.38),
            at(0.30, 0.38),
            at(0.52, 0.16),
            at(0.52, 0.84),
            at(0.30, 0.62),
            at(0.10, 0.62),
            at(0.10, 0.38),
        ])
    };

    match icon {
        Icon::Plus => {
            painter.add(line(&[at(0.5, 0.05), at(0.5, 0.95)]));
            painter.add(line(&[at(0.05, 0.5), at(0.95, 0.5)]));
        }
        Icon::Grid => {
            for (x, y) in [(0.04, 0.04), (0.54, 0.04), (0.04, 0.54), (0.54, 0.54)] {
                let tile = square(x, y, x + 0.42, y + 0.42);
                painter.rect_stroke(tile, CornerRadius::same(1), stroke, StrokeKind::Inside);
            }
        }
        Icon::Expand => {
            painter.add(line(&[at(0.06, 0.40), at(0.06, 0.06), at(0.40, 0.06)]));
            painter.add(line(&[at(0.60, 0.06), at(0.94, 0.06), at(0.94, 0.40)]));
            painter.add(line(&[at(0.94, 0.60), at(0.94, 0.94), at(0.60, 0.94)]));
            painter.add(line(&[at(0.40, 0.94), at(0.06, 0.94), at(0.06, 0.60)]));
        }
        Icon::Collapse => {
            painter.add(line(&[at(0.06, 0.40), at(0.40, 0.40), at(0.40, 0.06)]));
            painter.add(line(&[at(0.60, 0.06), at(0.60, 0.40), at(0.94, 0.40)]));
            painter.add(line(&[at(0.94, 0.60), at(0.60, 0.60), at(0.60, 0.94)]));
            painter.add(line(&[at(0.40, 0.94), at(0.40, 0.60), at(0.06, 0.60)]));
        }
        Icon::Gear => {
            // A ring, a hub and eight teeth: enough to read as a cogwheel at the
            // size of a button, and nothing more is legible at that size anyway.
            let centre = rect.center();
            let ring = rect.width() * 0.32;
            painter.circle_stroke(centre, ring, stroke);
            painter.circle_stroke(centre, rect.width() * 0.10, stroke);
            for step in 0..8 {
                let angle = step as f32 * std::f32::consts::TAU / 8.0;
                let (sin, cos) = angle.sin_cos();
                let inward = centre + vec2(cos, sin) * ring;
                let outward = centre + vec2(cos, sin) * rect.width() * 0.50;
                painter.line_segment([inward, outward], stroke);
            }
        }
        Icon::Edit => {
            // A pencil: a barrel lying at 45 degrees, and the tip that writes.
            let near_top = at(0.26, 0.74);
            let near_bottom = at(0.18, 0.66);
            let far_bottom = at(0.68, 0.16);
            let far_top = at(0.76, 0.24);
            painter.add(line(&[near_bottom, far_bottom, far_top, near_top, near_bottom]));
            painter.add(line(&[near_bottom, at(0.10, 0.90)]));
            painter.add(line(&[near_top, at(0.10, 0.90)]));
        }
        Icon::Trash => {
            // A bin: the lid, the handle above it, and the body below.
            painter.add(line(&[at(0.14, 0.28), at(0.86, 0.28)]));
            painter.add(line(&[at(0.40, 0.28), at(0.40, 0.17), at(0.60, 0.17), at(0.60, 0.28)]));
            painter.add(line(&[at(0.24, 0.28), at(0.30, 0.88), at(0.70, 0.88), at(0.76, 0.28)]));
        }
        Icon::Up => {
            // A chevron: one stroke, so that it reads at the size of a row.
            painter.add(line(&[at(0.22, 0.62), at(0.50, 0.34), at(0.78, 0.62)]));
        }
        Icon::Down => {
            painter.add(line(&[at(0.22, 0.38), at(0.50, 0.66), at(0.78, 0.38)]));
        }
        Icon::Left => {
            painter.add(line(&[at(0.72, 0.06), at(0.28, 0.50), at(0.72, 0.94)]));
        }
        Icon::Right => {
            painter.add(line(&[at(0.28, 0.06), at(0.72, 0.50), at(0.28, 0.94)]));
        }
        Icon::Close => {
            painter.add(line(&[at(0.16, 0.16), at(0.84, 0.84)]));
            painter.add(line(&[at(0.84, 0.16), at(0.16, 0.84)]));
        }
        Icon::Portrait => {
            // A tall viewport, 9:16 - the shape the wall takes once the button
            // is pressed.
            let body = square(0.247, 0.05, 0.753, 0.95);
            painter.rect_stroke(body, CornerRadius::same(1), stroke, StrokeKind::Inside);
        }
        Icon::Landscape => {
            // A wide viewport, 16:9.
            let body = square(0.05, 0.247, 0.95, 0.753);
            painter.rect_stroke(body, CornerRadius::same(1), stroke, StrokeKind::Inside);
        }
        Icon::AspectRatio => {
            painter.rect_stroke(square(0.05, 0.20, 0.95, 0.80), CornerRadius::same(1), stroke, StrokeKind::Inside);
            painter.add(line(&[at(0.27, 0.52), at(0.27, 0.40), at(0.41, 0.40)]));
            painter.add(line(&[at(0.73, 0.48), at(0.73, 0.60), at(0.59, 0.60)]));
        }
        Icon::Play => {
            let colour = stroke.color;
            painter.add(Shape::convex_polygon(
                vec![at(0.30, 0.12), at(0.30, 0.88), at(0.84, 0.50)],
                colour,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            let colour = stroke.color;
            for (x0, x1) in [(0.28, 0.42), (0.58, 0.72)] {
                painter.rect_filled(square(x0, 0.14, x1, 0.86), CornerRadius::ZERO, colour);
            }
        }
        Icon::StepBack | Icon::StepForward => {
            // A ring open on the side the arrow has come from, a double
            // arrowhead riding its top, and the number inside - the shape the
            // camera apps draw for "ten seconds". The ring starts level with its
            // centre, on the left going back and on the right going forward, and
            // runs three quarters of the way round to the arrowheads.
            let forward = matches!(icon, Icon::StepForward);
            let heavy = stroke;
            let (centre, radius) = ((0.5_f32, 0.57_f32), 0.375_f32);
            // How far the inner arrowhead's point sits from the top of the ring:
            // the ring runs on to that point, so the shaft reaches the head.
            let lead = 0.0656_f32;
            let overrun = (lead / radius).asin().to_degrees();
            let (sign, from, to) = if forward {
                (1.0_f32, 0.0_f32, 270.0 + overrun)
            } else {
                (-1.0, 180.0, -90.0 - overrun)
            };
            painter.add(circle_arc(rect, turned, centre, radius, from, to, heavy));
            let y = centre.1 - (radius * radius - lead * lead).sqrt();
            for offset in [lead, lead + 0.1556] {
                painter.add(chevron(rect, turned, (centre.0 + sign * offset, y), (sign, 0.0), 0.208, heavy));
            }
            let [one, zero] = ten(rect, turned, Stroke::new(stroke.width * 0.85, stroke.color));
            painter.add(one);
            painter.add(zero);
        }
        Icon::EventBack | Icon::EventForward => {
            // The same ring, but open at the bottom corner and ended by a
            // single corner-shaped head - two bars meeting at a right angle,
            // pointing down and outwards - with an exclamation mark inside.
            let forward = matches!(icon, Icon::EventForward);
            let heavy = stroke;
            let colour = stroke.color;
            let extent = rect.width();
            let radius = 0.42_f32;
            let side = if forward { 1.0_f32 } else { -1.0 };
            let (from, to) = if forward { (45.0_f32, 333.0_f32) } else { (135.0, -153.0) };
            painter.add(circle_arc(rect, turned, (0.5, 0.5), radius, from, to, heavy));
            // The head sits a little past the end of the ring, at the side, and
            // always points diagonally down and outwards; its bars run up and
            // inwards along the ring's own edge.
            let tip = (0.5 + side * radius, 0.329);
            let diagonal = std::f32::consts::FRAC_1_SQRT_2;
            painter.add(chevron(rect, turned, tip, (side * diagonal, diagonal), 0.205, heavy));
            // The mark, drawn rather than typed: a stem that tapers from a broad
            // rounded top to a narrower rounded foot, and a round dot.
            let mut stem: Vec<Pos2> = Vec::with_capacity(18);
            for step in 0..=8 {
                let angle = (180.0 + 180.0 * step as f32 / 8.0).to_radians();
                stem.push(at(0.5 + 0.082 * angle.cos(), 0.341 + 0.082 * angle.sin()));
            }
            for step in 0..=8 {
                let angle = (180.0 * step as f32 / 8.0).to_radians();
                stem.push(at(0.5 + 0.059 * angle.cos(), 0.483 + 0.059 * angle.sin()));
            }
            painter.add(Shape::convex_polygon(stem, colour, Stroke::NONE));
            painter.circle_filled(at(0.5, 0.67), extent * 0.084, colour);
        }
        Icon::Live => {
            // A video camera: the body, and the lens flaring out of its front.
            // The button switches the view to the camera as it is now, so the
            // camera itself is the picture - nothing inside it to say it is
            // recording.
            painter.rect_stroke(
                square(0.05, 0.20, 0.63, 0.80),
                CornerRadius::same(2),
                stroke,
                StrokeKind::Inside,
            );
            painter.add(Shape::closed_line(
                vec![at(0.63, 0.44), at(0.93, 0.25), at(0.93, 0.75), at(0.63, 0.56)],
                stroke,
            ));
        }
        Icon::Zoom => {
            // Two open rings facing each other - a link that can be pulled
            // apart - and a double-headed arrow through them. The rings and
            // the arrow share the weight of every other icon's line.
            let heavy = stroke;
            let light = stroke;
            painter.add(circle_arc(rect, turned, (0.265, 0.5), 0.195, 45.0, 315.0, heavy));
            painter.add(circle_arc(rect, turned, (0.735, 0.5), 0.195, 225.0, 495.0, heavy));
            painter.add(Shape::line(vec![at(0.195, 0.5), at(0.805, 0.5)], light));
            painter.add(chevron(rect, turned, (0.195, 0.5), (-1.0, 0.0), 0.125, light));
            painter.add(chevron(rect, turned, (0.805, 0.5), (1.0, 0.0), 0.125, light));
        }
        Icon::Sound => {
            painter.add(speaker());
            painter.add(line(&[at(0.64, 0.38), at(0.74, 0.50), at(0.64, 0.62)]));
            painter.add(line(&[at(0.78, 0.26), at(0.90, 0.50), at(0.78, 0.74)]));
        }
        Icon::Mute => {
            painter.add(speaker());
            painter.add(line(&[at(0.62, 0.32), at(0.84, 0.68)]));
            painter.add(line(&[at(0.84, 0.32), at(0.62, 0.68)]));
        }
        Icon::Mic => {
            // The capsule, its yoke and its stand.
            let capsule = square(0.36, 0.06, 0.64, 0.56);
            painter.rect_stroke(capsule, CornerRadius::same(6), stroke, StrokeKind::Inside);
            painter.add(line(&[at(0.22, 0.44), at(0.22, 0.56), at(0.50, 0.74), at(0.78, 0.56), at(0.78, 0.44)]));
            painter.add(line(&[at(0.50, 0.74), at(0.50, 0.92)]));
            painter.add(line(&[at(0.34, 0.92), at(0.66, 0.92)]));
        }
        Icon::History => {
            // A screen with the play mark in it: the recordings, as a picture to
            // open, rather than a clock to read. Nothing about time in it, so it
            // cannot be mistaken for a timer or a schedule, and it has no ring to
            // be confused with the buttons that step through the recording once
            // it is open.
            painter.rect_stroke(
                square(0.06, 0.18, 0.94, 0.82),
                CornerRadius::same(2),
                stroke,
                StrokeKind::Inside,
            );
            painter.add(Shape::convex_polygon(
                vec![at(0.41, 0.34), at(0.41, 0.66), at(0.67, 0.50)],
                stroke.color,
                Stroke::NONE,
            ));
        }
        Icon::Rotate => {
            // A circular arrow: an arc most of the way round, a head that says
            // which way it turns, and the picture's frame in the middle.
            let (radius, from, to) = (0.38_f32, 200.0_f32.to_radians(), 0.0_f32);
            let on_arc = |a: f32| (0.5 + radius * a.cos(), 0.5 - radius * a.sin());
            let arc: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let (x, y) = on_arc(from + (to - from) * i as f32 / 16.0);
                    at(x, y)
                })
                .collect();
            painter.add(Shape::line(arc, stroke));
            let (head_len, head_wing) = (0.13_f32, 0.09_f32);
            let (tx, ty) = on_arc(to);
            // The point a chord of the head's length falls back to, on the arc.
            let (bx, by) = on_arc(to + head_len / radius);
            let (cx, cy) = (tx - bx, ty - by);
            let norm = (cx * cx + cy * cy).sqrt();
            let (dx, dy) = (cx / norm, cy / norm);
            let (px, py) = (dy, -dx);
            painter.add(line(&[
                at(bx - px * head_wing, by - py * head_wing),
                at(tx, ty),
                at(bx + px * head_wing, by + py * head_wing),
            ]));
            let body = square(0.33, 0.33, 0.67, 0.67);
            painter.rect_stroke(body, CornerRadius::same(1), stroke, StrokeKind::Inside);
        }
        Icon::Hangup => {
            // The handset laid down, drawn as the phone's own call-end key has
            // it: an outer arc with the receiver's underside folded inside it
            // and the two cups meeting its ends. Two arcs, not one line - the
            // single line read as a bracket at the size of a button.
            use std::f32::consts::PI;
            let arc = |cx: f32, cy: f32, rx: f32, ry: f32, from: f32, to: f32| -> Vec<Pos2> {
                (0..=12)
                    .map(|i| {
                        let a = from + (to - from) * i as f32 / 12.0;
                        at(cx + rx * a.cos(), cy - ry * a.sin())
                    })
                    .collect()
            };
            let mut points = arc(0.5, 0.68, 0.42, 0.38, PI, 0.0);
            points.push(at(0.68, 0.68));
            points.extend(arc(0.5, 0.54, 0.18, 0.10, 0.0, PI));
            points.push(at(0.32, 0.68));
            painter.add(Shape::closed_line(points, stroke));
        }
    }
}

/// An arc of the circle of `radius` about `centre`, from `from` to `to` degrees.
///
/// Degrees rather than radians because these are read off a drawing: zero is the
/// right of the square, ninety is its bottom, and an arc that runs clockwise on
/// the screen goes from a smaller number to a larger one. Any sweep is allowed,
/// including a negative angle or one past 360.
fn circle_arc(rect: Rect, turned: bool, centre: (f32, f32), radius: f32, from: f32, to: f32, stroke: Stroke) -> Shape {
    // About every eight degrees: at the size of a button that is a fraction of a
    // pixel off the true curve.
    let steps = (((to - from).abs() / 8.0).ceil() as usize).max(2);
    let points: Vec<Pos2> = (0..=steps)
        .map(|step| {
            let angle = (from + (to - from) * step as f32 / steps as f32).to_radians();
            unit_at(rect, turned, centre.0 + radius * angle.cos(), centre.1 + radius * angle.sin())
        })
        .collect();
    Shape::line(points, stroke)
}

/// An arrowhead: a V with its point at `tip`, the way the arrow travels being
/// `direction` (a unit vector), and two arms of length `arm` falling back from
/// the point at forty-five degrees either side - a right angle between them.
fn chevron(rect: Rect, turned: bool, tip: (f32, f32), direction: (f32, f32), arm: f32, stroke: Stroke) -> Shape {
    let diagonal = std::f32::consts::FRAC_1_SQRT_2;
    // Back along the shaft, and across it.
    let (back, across) = ((-direction.0, -direction.1), (-direction.1, direction.0));
    let end = |side: f32| {
        unit_at(
            rect,
            turned,
            tip.0 + (back.0 + side * across.0) * diagonal * arm,
            tip.1 + (back.1 + side * across.1) * diagonal * arm,
        )
    };
    Shape::line(vec![end(1.0), unit_at(rect, turned, tip.0, tip.1), end(-1.0)], stroke)
}

/// The number the step buttons carry, "10", drawn rather than typed.
///
/// A font's digits are a different weight from the ring they sit in, and which
/// font is a matter of the target; a one with a flag and a squared-off nought,
/// drawn with the ring's own painter, are the same bold figures everywhere and
/// turn with the picture like the rest of the icon.
fn ten(rect: Rect, turned: bool, stroke: Stroke) -> [Shape; 2] {
    let at = |x: f32, y: f32| unit_at(rect, turned, x, y);
    let one = Shape::line(vec![at(0.328, 0.525), at(0.4056, 0.478), at(0.4056, 0.714)], stroke);
    // A nought is an oval with straighter sides than an ellipse has - a
    // superellipse of exponent 2.5, which is what a bold sans-serif nought is.
    const STEPS: usize = 32;
    let nought: Vec<Pos2> = (0..STEPS)
        .map(|step| {
            let (sin, cos) = (step as f32 * std::f32::consts::TAU / STEPS as f32).sin_cos();
            at(
                0.599 + 0.086 * cos.signum() * cos.abs().powf(0.8),
                0.596 + 0.118 * sin.signum() * sin.abs().powf(0.8),
            )
        })
        .collect();
    [one, Shape::closed_line(nought, stroke)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Align, Layout};

    /// The move buttons of a reorder row carry an id derived from the camera,
    /// not from the row, so a camera that is moved keeps the same widgets - and
    /// the focus that is on one of them. egui's automatic ids are seeded from
    /// the sibling position and do not survive the move; this is the property
    /// the camera list relies on.
    #[test]
    fn an_explicit_id_survives_a_reorder() {
        fn draw(ui: &mut Ui, order: &[&str]) -> Vec<(String, Id)> {
            let mut ids = Vec::new();
            for camera in order {
                ui.horizontal(|ui| {
                    ui.label(*camera);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let down = button_at(ui, Id::new(camera).with("down"), Icon::Down, false, false, "down", 20.0);
                        let up = button_at(ui, Id::new(camera).with("up"), Icon::Up, false, false, "up", 20.0);
                        ids.push((format!("{camera}-up"), up.id));
                        ids.push((format!("{camera}-down"), down.id));
                    });
                });
                ui.add_space(4.0);
            }
            ids
        }

        let ctx = egui::Context::default();
        let run = |order: &[&str]| {
            let captured = std::cell::RefCell::new(Vec::<(String, Id)>::new());
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        captured.borrow_mut().extend(draw(ui, order));
                    });
                });
            });
            let mut ids = captured.into_inner();
            ids.sort_by(|a, b| a.0.cmp(&b.0));
            ids
        };

        assert_eq!(run(&["a", "b", "c"]), run(&["c", "a", "b"]));
    }

    /// A turned icon is still drawn inside its square and is not inverted: the
    /// rectangles the microphone's capsule and the picture fit's frame are made
    /// of have to come out with a positive size whichever way the picture is
    /// drawn, and the turn swaps their axes.
    #[test]
    fn a_turned_rectangle_is_normalized_and_stays_on_its_icon() {
        let square = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::splat(40.0));
        for turned in [false, true] {
            // The microphone's capsule: 0.28 wide and 0.50 tall in the square.
            let capsule = unit_rect(square, turned, 0.36, 0.06, 0.64, 0.56);
            assert!(capsule.width() > 0.0 && capsule.height() > 0.0, "inverted with turned={turned}");
            assert!(square.contains_rect(capsule), "outside its icon with turned={turned}");
            if turned {
                assert!(capsule.width() > capsule.height(), "the capsule lies down when turned");
            } else {
                assert!(capsule.height() > capsule.width(), "and stands up when not");
            }
        }
    }
}
