//! The editor for a viewer-drawn grid. It opens over the layout picker and
//! holds a *preview* grid: nothing reaches the wall or the configuration until
//! Confirm. Back and the cross drop the whole thing, the same contract the
//! camera reorder list uses.
//!
//! Two stages share one grid. The first lays a plain `columns x rows` grid,
//! which is also all the old "custom grid" pull request could do. The second
//! merges equal-sized, whole-edge neighbours and splits them back. Every
//! operation rechecks that the units are still tiled exactly once, so a stray
//! press cannot leave a hole in the wall.

use super::*;
use monitor_core::layout::{CustomGrid, MAX_CUSTOM_DIMENSION};

/// What the editor is showing while it is open.
#[derive(Debug, Clone, Copy)]
pub(super) struct EditorState {
    /// Which of the four picker slots is being edited.
    pub(super) slot: usize,
    /// The grid under construction. Only the first `count` cells are real.
    pub(super) grid: CustomGrid,
    /// Which of the two stages the viewer is on.
    pub(super) stage: Stage,
    /// Cell under the focus in the merge stage. Re-sorted after each edit, so
    /// this is an index, never an identity a camera could rely on.
    pub(super) selected: usize,
    /// First half of a merge pair. Set by the first Enter, then the viewer
    /// walks to a neighbour and presses Enter again. Keeping it explicit means
    /// a single press can never merge something by accident.
    pub(super) anchor: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    RowsAndColumns,
    Merge,
}

impl EditorState {
    /// Editing an existing slot loads its shape; a new slot starts from a
    /// small uniform grid rather than an empty preview.
    pub(super) fn open(slot: usize, existing: Option<CustomGrid>) -> Self {
        let grid = existing.unwrap_or_else(|| CustomGrid::uniform(2, 2));
        EditorState { slot, grid, stage: Stage::RowsAndColumns, selected: 0, anchor: None }
    }

    /// Rebuild from plain dimensions. The merge stage keeps its selection
    /// clamped to the new grid and forgets any half-made pair.
    pub(super) fn set_dimensions(&mut self, cols: u8, rows: u8) {
        self.grid = CustomGrid::uniform(cols, rows);
        self.selected = self.selected.min(self.grid.cells().len().saturating_sub(1));
        self.anchor = None;
    }

    /// Enter in the merge stage. The first press marks the focus cell as the
    /// first half of a pair; the second, on a neighbour, merges them. Pressing
    /// it on the anchor again clears the mark. Returns true when the grid
    /// actually changed.
    pub(super) fn enter(&mut self) -> bool {
        match self.anchor {
            None => {
                self.anchor = Some(self.selected);
                false
            }
            Some(anchor) if anchor == self.selected => {
                self.anchor = None;
                false
            }
            Some(anchor) => {
                let survivor = self.grid.merge(anchor, self.selected);
                self.anchor = None;
                if let Some(grid) = survivor {
                    self.grid = grid;
                    self.selected = self.selected.min(self.grid.cells().len().saturating_sub(1));
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Split the selected cell, if it is big enough. `horizontal` cuts it into
    /// left and right halves; otherwise it cuts top and bottom.
    pub(super) fn split(&mut self, horizontal: bool) -> bool {
        let split = self.grid.split(self.selected, horizontal);
        if let Some(grid) = split {
            self.grid = grid;
            self.selected = self.selected.min(self.grid.cells().len().saturating_sub(1));
            true
        } else {
            false
        }
    }

    pub(super) fn valid(&self) -> bool {
        self.grid.is_valid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_slot_starts_from_a_small_uniform_grid() {
        let state = EditorState::open(2, None);
        assert_eq!(state.slot, 2);
        assert_eq!(state.grid.cells().len(), 4);
        assert_eq!(state.stage, Stage::RowsAndColumns);
        assert!(state.valid());
    }

    #[test]
    fn editing_an_existing_slot_loads_its_shape() {
        let grid = CustomGrid::uniform(3, 3).merge(0, 1).unwrap();
        let state = EditorState::open(0, Some(grid));
        assert_eq!(state.grid, grid);
        assert_eq!(state.grid.count, 8);
    }

    #[test]
    fn dimension_changes_reset_the_grid_and_clamp_the_cursor() {
        let mut state = EditorState::open(0, None);
        state.selected = 3;
        state.set_dimensions(2, 1);
        assert_eq!(state.grid.cells().len(), 2);
        assert_eq!(state.selected, 1);
        assert_eq!(state.grid.display_label(), "2x1");
    }

    #[test]
    fn merging_takes_two_presses_and_only_on_a_real_neighbour() {
        let mut state = EditorState::open(0, None);
        let before = state.grid;
        assert!(!state.enter());
        assert_eq!(state.anchor, Some(0));
        assert_eq!(state.grid, before);
        state.selected = 3;
        assert!(!state.enter());
        assert_eq!(state.grid, before);
        assert!(state.anchor.is_none());
        state.selected = 0;
        assert!(!state.enter());
        state.selected = 1;
        assert!(state.enter());
        assert_eq!(state.grid.count, 3);
        assert!(state.grid.is_valid());
        state.selected = 0;
        assert!(state.split(true));
        assert_eq!(state.grid, before);
    }

    #[test]
    fn pressing_enter_on_the_anchor_clears_it() {
        let mut state = EditorState::open(0, None);
        assert!(!state.enter());
        assert_eq!(state.anchor, Some(0));
        assert!(!state.enter());
        assert!(state.anchor.is_none());
    }
}

/// The editor modal. The state lives in `app.layout_editor`'s payload while it
/// is open, which is why the body only borrows `&mut EditorState`; on Confirm
/// the caller takes the finished grid out of that payload.
///
/// Backspace stays Back here, as everywhere else in the program, so splitting
/// is a button the remote can walk onto rather than a key that would close the
/// window under the viewer.
impl XgViewApp {
    /// Returns the finished preview only on Confirm, so the caller can write
    /// it to the slot. Back and the cross return `None` and change nothing.
    pub(super) fn draw_layout_editor(
        &mut self,
        ctx: &egui::Context,
    ) -> (Answer, Option<EditorState>) {
        let mut answer = Answer::STAY;
        let mut confirmed = false;
        let mut back = false;
        let mut finished: Option<EditorState> = None;

        self.modal_frame(
            ctx,
            "xgview-layout-editor",
            460.0,
            |app| &mut app.layout_editor,
            |app, ui, payload: &mut EditorState| {
                let bar = dialogs::title_bar(
                    ui,
                    &monitor_i18n::tr("layout-editor-title"),
                    true,
                    &monitor_i18n::tr("dialog-close"),
                );
                if let Some(close) = bar.close {
                    if app.nav.tracked(close).clicked() {
                        back = true;
                    }
                }

                ui.add_space(theme::space::S);
                app.nav.open("layout-editor-tabs");
                ui.horizontal(|ui| {
                    for (stage, key) in [
                        (Stage::RowsAndColumns, "layout-editor-stage-grid"),
                        (Stage::Merge, "layout-editor-stage-merge"),
                    ] {
                        let selected = payload.stage == stage;
                        let response =
                            app.nav.tracked(ui.selectable_label(selected, monitor_i18n::tr(key)));
                        if response.clicked() {
                            payload.stage = stage;
                            payload.anchor = None;
                        }
                    }
                });
                app.nav.close();

                ui.add_space(theme::space::S);
                match payload.stage {
                    Stage::RowsAndColumns => {
                        for (key, scope, is_rows) in [
                            ("layout-editor-rows", "layout-editor-rows", true),
                            ("layout-editor-columns", "layout-editor-columns", false),
                        ] {
                            app.nav.open(scope);
                            ui.horizontal(|ui| {
                                ui.label(monitor_i18n::tr(key));
                                for value in 1..=MAX_CUSTOM_DIMENSION {
                                    let current =
                                        if is_rows { payload.grid.rows } else { payload.grid.cols };
                                    let response = app.nav.tracked(ui.selectable_label(
                                        current == value,
                                        value.to_string(),
                                    ));
                                    if response.clicked() && current != value {
                                        if is_rows {
                                            payload.set_dimensions(payload.grid.cols, value);
                                        } else {
                                            payload.set_dimensions(value, payload.grid.rows);
                                        }
                                    }
                                }
                            });
                            app.nav.close();
                        }
                    }
                    Stage::Merge => {
                        let hint = if let Some(anchor) = payload.anchor {
                            monitor_i18n::tr_args(
                                "layout-editor-merge-picked",
                                &[("cell", (anchor + 1).into())],
                            )
                        } else {
                            monitor_i18n::tr("layout-editor-merge-hint")
                        };
                        ui.label(RichText::new(hint).small().color(theme::ACCENT));
                    }
                }

                ui.add_space(theme::space::S);
                let preview_w = ui.available_width().min(220.0);
                let area = ui.allocate_space(vec2(preview_w, 110.0)).1;
                let layout = GridLayout::Custom(payload.grid);
                grid::layout_thumbnail(ui.painter(), area, layout);
                app.nav.open("layout-editor-grid");
                for index in 0..payload.grid.cells().len() {
                    let rect = grid::tile_rect(area, layout, index, 2.0);
                    let id = egui::Id::new(("layout-editor-cell", index));
                    let response = ui.interact(rect, id, egui::Sense::click());
                    if response.has_focus() {
                        payload.selected = index;
                    }
                    if response.clicked() && payload.stage == Stage::Merge {
                        payload.selected = index;
                        payload.enter();
                    }
                    if payload.anchor == Some(index) {
                        ui.painter().rect_stroke(
                            rect,
                            egui::CornerRadius::same(theme::radius::S),
                            egui::Stroke::new(2.0_f32, theme::ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        (index + 1).to_string(),
                        egui::FontId::proportional(13.0),
                        theme::TEXT_DIM,
                    );
                    app.nav.item(&response);
                }
                app.nav.close();

                ui.add_space(theme::space::S);
                app.nav.open("layout-editor-actions");
                ui.horizontal(|ui| {
                    if payload.stage == Stage::Merge {
                        let split = app
                            .nav
                            .tracked(ui.button(monitor_i18n::tr("layout-editor-split")));
                        if split.clicked() && payload.grid.can_split(payload.selected) {
                            payload.split(payload.selected % 2 == 0);
                        }
                    }
                    let ok = app.nav.tracked(ui.button(monitor_i18n::tr("ok")));
                    if ok.clicked() && payload.valid() {
                        confirmed = true;
                        finished = Some(*payload);
                    }
                    let cancel = app.nav.tracked(ui.button(monitor_i18n::tr("cancel")));
                    if cancel.clicked() {
                        back = true;
                    }
                });
                app.nav.close();

                if app.dialog_back_pressed(ctx) {
                    back = true;
                }
                answer = if confirmed || back { Answer::CLOSE } else { Answer::STAY };
                answer
            },
        );
        (answer, finished)
    }
}
