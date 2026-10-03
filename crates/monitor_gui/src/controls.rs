//! Wrappers for the controls the navigation layer cannot take as they come.
//!
//! On a remote Up and Down always move the focus, so a value control cannot be
//! stepped with them. Left and Right are the value's instead, but egui's
//! [`egui::DragValue`] steps with Up and Down, so its sideways presses have to
//! be turned into a step by us. The navigation layer carries the press (it is
//! the one that sees the arrows), and this applies it as the widget is drawn.
//!
//! A fold is the other way round: it is a stop like any other, but it is drawn
//! in two pieces and painted as if it were not a control at all. See [`fold`].

use std::ops::RangeInclusive;

use egui::emath::Numeric;
use egui::{Response, Ui};

use crate::nav::{Kind, Nav};

/// A [`egui::DragValue`] registered with the navigation layer.
///
/// Up and Down walk the form; Left and Right change the value by `step`. The
/// value is written straight to `value`, so a caller that tracks edits with
/// [`Response::changed`] still sees them.
///
/// The step is applied *before* the widget is drawn. A focused `DragValue`
/// enters its keyboard-edit mode on focus, and in that mode it shows a text
/// buffer stored under its id — not the value behind the reference — so
/// changing the value after the widget has run would not be visible until the
/// focus leaves. Applying the step first and clearing that buffer makes the
/// `DragValue` regenerate the text from the new value, so the change is seen
/// in the same frame.
pub fn drag_value<T: Numeric>(
    nav: &mut Nav,
    ui: &mut Ui,
    value: &mut T,
    range: RangeInclusive<T>,
    step: f64,
    suffix: &str,
) -> Response {
    let id = ui.next_auto_id();
    let steps = f64::from(nav.value_step());
    let stepped = steps != 0.0 && ui.memory(|mem| mem.has_focus(id));
    if stepped {
        *value = T::from_f64((*value).to_f64() + steps * step);
        ui.data_mut(|data| data.remove_temp::<String>(id));
    }
    let mut response = ui.add(egui::DragValue::new(value).range(range).suffix(suffix));
    nav.item_kind(Kind::DragValue, &response);
    if stepped {
        response.mark_changed();
    }
    response
}

/// A [`egui::CollapsingHeader`] registered with the navigation layer.
///
/// It is a stop of the form like any other control - Up and Down reach it, and
/// Enter or a click folds it - which takes two things egui does not do by
/// itself:
///
/// * Its seat is taken *before* it is drawn. A scope's order is the order its
///   controls registered in, and a header belongs ahead of the controls it
///   hides; but a `CollapsingHeader` hands back a response only once its body
///   has been shown too, so the seat is remembered and the header registered
///   into it afterwards.
/// * The header is painted with the focus ring by hand. egui paints a header
///   nothing at all unless its frame is asked for, so a viewer would have no way
///   of seeing that the remote is on it.
pub fn fold<R>(
    nav: &mut Nav,
    ui: &mut Ui,
    label: &str,
    default_open: bool,
    body: impl FnOnce(&mut Ui, &mut Nav) -> R,
) -> egui::collapsing_header::CollapsingResponse<R> {
    let seat = nav.seat();
    let response = egui::CollapsingHeader::new(label)
        .default_open(default_open)
        .show(ui, |ui| body(ui, nav));
    nav.item_at(seat, response.header_response.id);

    let header = &response.header_response;
    if header.has_focus() {
        let visuals = ui.style().visuals.widgets.active;
        ui.painter().rect_stroke(
            header.rect.expand(visuals.expansion),
            visuals.corner_radius,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
    }
    nav.reveal(header);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{Axis, Dir, ScopeDef};

    /// A fold is drawn in two pieces - its header, and the body it hides - and
    /// its response only arrives once both have been drawn. In the walk, though,
    /// the header comes *before* what it hides: that is what the seat is for.
    #[test]
    fn the_fold_walks_before_the_controls_it_hides() {
        let ctx = egui::Context::default();
        let mut nav = Nav::new(&[("body", ScopeDef::new(Axis::Column))]);
        let mut ids = Vec::new();

        for _ in 0..2 {
            nav.begin();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 300.0))),
                ..Default::default()
            };
            let mut inside = Vec::new();
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    nav.open("body");
                    let above = nav.tracked(ui.button("above"));
                    let fold = fold(&mut nav, ui, "A fold", true, |ui, nav| {
                        inside.push(nav.tracked(ui.button("inside one")).id);
                        inside.push(nav.tracked(ui.button("inside two")).id);
                    });
                    let below = nav.tracked(ui.button("below"));
                    nav.close();
                    if ids.is_empty() {
                        let inside = std::mem::take(&mut inside);
                        ids = [vec![above.id, fold.header_response.id], inside, vec![below.id]].concat();
                    }
                });
            });
            nav.finish(&ctx);
        }

        let (above, header, first, second, below) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
        assert_eq!(nav.step(above, Dir::Down), Some(header), "the fold's header comes first");
        assert_eq!(nav.step(header, Dir::Down), Some(first), "then the controls it hides");
        assert_eq!(nav.step(first, Dir::Down), Some(second));
        assert_eq!(nav.step(second, Dir::Down), Some(below), "and then what follows the fold");
        assert_eq!(nav.step(header, Dir::Up), Some(above));
    }
}