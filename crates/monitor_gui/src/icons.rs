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

use egui::{CornerRadius, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, vec2};

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
}

/// Side of the square an icon button occupies, in points.
///
/// Sized as a touch target rather than as a glyph: the bar is used with a finger
/// on a tablet, and this is about the smallest thing a finger hits reliably.
const BUTTON: f32 = 30.0;

/// Fraction of the button the drawing itself takes.
const GLYPH: f32 = 0.5;

/// An icon button: a square carrying `icon`, with `tooltip` under the pointer.
///
/// The icon is the whole label, which is why the tooltip is not optional - it is
/// the only place a viewer who does not recognise the shape can read the word.
/// `selected` paints the button as held down, the state the bar uses for the
/// panels that are open.
pub fn button(ui: &mut Ui, icon: Icon, selected: bool, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(BUTTON), Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, selected);
        let corner = CornerRadius::same(theme::radius::M);
        let frame = rect.expand(visuals.expansion);
        ui.painter().rect_filled(frame, corner, visuals.weak_bg_fill);
        if visuals.bg_stroke.width > 0.0 {
            // The focus ring arrives this way: egui paints `widgets.active` for
            // whatever has the keyboard focus, and that state carries a stroke.
            ui.painter().rect_stroke(frame, corner, visuals.bg_stroke, StrokeKind::Inside);
        }
        let glyph = Rect::from_center_size(rect.center(), Vec2::splat(BUTTON * GLYPH));
        paint(ui.painter(), glyph, icon, Stroke::new(1.6_f32, visuals.text_color()));
    }
    response.on_hover_text(tooltip)
}

/// Draws `icon` filling `rect`.
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, stroke: Stroke) {
    let at = |x: f32, y: f32| Pos2::new(rect.left() + x * rect.width(), rect.top() + y * rect.height());
    let line = |points: &[Pos2]| Shape::line(points.to_vec(), stroke);

    match icon {
        Icon::Plus => {
            painter.add(line(&[at(0.5, 0.05), at(0.5, 0.95)]));
            painter.add(line(&[at(0.05, 0.5), at(0.95, 0.5)]));
        }
        Icon::Grid => {
            for (x, y) in [(0.04, 0.04), (0.54, 0.04), (0.04, 0.54), (0.54, 0.54)] {
                let tile = Rect::from_min_max(at(x, y), at(x + 0.42, y + 0.42));
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
    }
}
