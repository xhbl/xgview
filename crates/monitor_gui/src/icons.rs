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

use egui::{CornerRadius, Id, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, vec2};

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
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    paint_button(ui, rect, &response, icon, selected, muted, side);
    response.on_hover_text(tooltip)
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
    // A muted button is inert: drawn so the row keeps its shape, but nothing
    // can land on it - the pointer cannot click it and the keyboard walk cannot
    // focus it, because there is nothing it could do. `Sense::hover` keeps it
    // out of both, while `Sense::click` would put it in the focus order.
    let sense = if muted { Sense::hover() } else { Sense::click() };
    let response = ui.interact(rect, id, sense);
    paint_button(ui, rect, &response, icon, selected, muted, side);
    response.on_hover_text(tooltip)
}

/// Draws a button's frame and glyph; the allocation is the caller's, so that
/// the automatic and the explicit id forms share one painting.
fn paint_button(
    ui: &Ui,
    rect: Rect,
    response: &Response,
    icon: Icon,
    selected: bool,
    muted: bool,
    side: f32,
) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let visuals = ui.style().interact_selectable(response, selected);
    let corner = CornerRadius::same(theme::radius::M);
    let frame = rect.expand(visuals.expansion);
    ui.painter().rect_filled(frame, corner, visuals.weak_bg_fill);
    if visuals.bg_stroke.width > 0.0 {
        // The focus ring arrives this way: egui paints `widgets.active` for
        // whatever has the keyboard focus, and that state carries a stroke.
        ui.painter().rect_stroke(frame, corner, visuals.bg_stroke, StrokeKind::Inside);
    }
    let glyph = Rect::from_center_size(rect.center(), Vec2::splat(side * GLYPH));
    let color = if muted { theme::TEXT_DIM } else { visuals.text_color() };
    paint(ui.painter(), glyph, icon, Stroke::new(1.6_f32, color));
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
    }
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
}
