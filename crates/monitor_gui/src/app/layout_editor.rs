//! The editor for a viewer-drawn grid. It opens over the layout picker and
//! holds a *preview* grid: nothing reaches the wall or the configuration until
//! Confirm. Back and the cross drop the whole thing, the same contract the
//! camera reorder list uses.
//!
//! One page does both jobs. The row and column buttons describe the baseline;
//! until a cell is merged the grid is just that baseline. Merging neighbours
//! whose shared edge is the same length, and splitting a cell straight back
//! into baseline unit squares, happen on the preview below those buttons. No
//! mode switch is needed: the buttons stay put and the preview is always
//! editable. Every operation rechecks that the units are still tiled exactly
//! once, so a stray press cannot leave a hole in the wall.

use super::*;
use monitor_core::layout::{Cell, CustomGrid, MAX_CUSTOM_DIMENSION};

/// How many unit columns two cells share. A vertical neighbour with no
/// overlap is not above or below this one, even if its row is adjacent.
fn overlap_columns(a: &Cell, b: &Cell) -> u16 {
    let left = u16::from(a.col.max(b.col));
    let right = u16::from(a.col + a.col_span).min(u16::from(b.col) + u16::from(b.col_span));
    right.saturating_sub(left)
}

/// What the editor is showing while it is open.
#[derive(Debug, Clone, Copy)]
pub(super) struct EditorState {
    /// Which of the four picker slots is being edited.
    pub(super) slot: usize,
    /// The custom slot's own control in the picker, to put the ring back on
    /// after Confirm. `None` when the editor was opened by a pointer.
    pub(super) slot_focus: Option<egui::Id>,
    /// The grid under construction. Only the first `count` cells are real.
    pub(super) grid: CustomGrid,
    /// Cell under the focus on the preview. Re-sorted after each edit, so
    /// this is an index, never an identity a camera could rely on.
    pub(super) selected: usize,
    /// First half of a merge pair. Set by the first Enter on a cell, then the
    /// viewer walks to a neighbour and presses Enter again. Keeping it
    /// explicit means a single press can never merge something by accident.
    pub(super) anchor: Option<usize>,
}

impl EditorState {
    /// Editing an existing slot loads its shape; a new slot starts from a
    /// small uniform grid rather than an empty preview.
    pub(super) fn open(slot: usize, existing: Option<CustomGrid>) -> Self {
        let grid = existing.unwrap_or_else(|| CustomGrid::uniform(2, 2));
        EditorState { slot, slot_focus: None, grid, selected: 0, anchor: None }
    }

    /// Rebuild from plain dimensions. The selection is clamped to the new
    /// grid and any half-made pair is forgotten.
    pub(super) fn set_dimensions(&mut self, cols: u8, rows: u8) {
        self.grid = CustomGrid::uniform(cols, rows);
        self.selected = self.selected.min(self.grid.cells().len().saturating_sub(1));
        self.anchor = None;
    }

    /// The first Enter on a cell marks it; the second, on a neighbour, merges
    /// them. Pressing it on the anchor again clears the mark. Returns true
    /// when the grid actually changed.
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
                let first = self.grid.cells()[anchor];
                let second = self.grid.cells()[self.selected];
                let survivor = self.grid.merge(anchor, self.selected);
                self.anchor = None;
                if let Some(grid) = survivor {
                    self.grid = grid;
                    // The merged cell takes the pair's common corner. Follow
                    // it through the re-sort so the ring stays on the result.
                    self.select_at(first.row.min(second.row), first.col.min(second.col));
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Which cell the Split button acts on. An anchor the viewer just picked
    /// wins: walking down to the button changes the focus cell, but the split
    /// should still undo the shape they selected. Fall back to the last focus
    /// when there is no anchor, and to a cell that can actually be split.
    pub(super) fn split_target(&self) -> Option<usize> {
        [self.anchor, Some(self.selected)]
            .into_iter()
            .flatten()
            .find(|&index| self.grid.can_split(index))
    }

    /// Break the cell under the focus straight back into the baseline's unit
    /// squares. A 3x1 cell takes one press, not repeated halving; any pending
    /// merge anchor is dropped so its old index cannot point at a new cell.
    pub(super) fn split(&mut self, index: usize) -> bool {
        let Some(cell) = self.grid.cells().get(index).copied() else {
            return false;
        };
        if let Some(grid) = self.grid.split(index) {
            self.grid = grid;
            self.anchor = None;
            self.select_at(cell.row, cell.col);
            true
        } else {
            false
        }
    }

    /// The cell above or below that shares the most of this cell's width and
    /// sits closest to it. The row scopes handle Left/Right and the exits at
    /// the ends of the grid; a vertical press needs the geometry, because a
    /// wide cell can cover units belonging to several baseline rows.
    pub(super) fn vertical_neighbor(&self, index: usize, dir: Dir) -> Option<usize> {
        if !matches!(dir, Dir::Up | Dir::Down) {
            return None;
        }
        let cells = self.grid.cells();
        let a = *cells.get(index)?;
        let mut best: Option<(usize, u16, u16)> = None; // (index, gap, overlap)
        for (position, b) in cells.iter().enumerate() {
            let (gap, overlap) = if dir == Dir::Up {
                let end = u16::from(b.row) + u16::from(b.row_span);
                if end > u16::from(a.row) {
                    continue;
                }
                (u16::from(a.row) - end, overlap_columns(&a, b))
            } else {
                let start = u16::from(b.row);
                let a_end = u16::from(a.row) + u16::from(a.row_span);
                if start < a_end {
                    continue;
                }
                (start - a_end, overlap_columns(&a, b))
            };
            if overlap == 0 {
                continue;
            }
            let better = best.is_none_or(|(at, best_gap, best_overlap)| {
                gap < best_gap
                    || (gap == best_gap && (overlap > best_overlap
                        || (overlap == best_overlap && position < at)))
            });
            if better {
                best = Some((position, gap, overlap));
            }
        }
        best.map(|(position, _, _)| position)
    }

    /// A cell is identified by its top-left unit, which survives the fill-order
    /// re-sort after an edit. Keep the selection on the shape the press made.
    fn select_at(&mut self, row: u8, col: u8) {
        self.selected = self
            .grid
            .cells()
            .iter()
            .position(|cell| cell.row == row && cell.col == col)
            .unwrap_or(0);
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
        assert_eq!(state.anchor, None);
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
        assert!(state.split(0));
        assert_eq!(state.grid, before);
    }

    #[test]
    fn the_selection_follows_the_shape_the_press_made() {
        let mut state = EditorState::open(0, None);
        state.set_dimensions(3, 2);
        state.enter();
        state.selected = 1;
        assert!(state.enter()); // the 2x1 cell sorts back to seat zero
        assert_eq!(state.selected, 0);
        assert_eq!(state.grid.cells()[state.selected].col_span, 2);
        assert!(state.split(0));
        assert_eq!(state.selected, 0);
        assert_eq!(state.grid.cells()[state.selected].col_span, 1);
    }

    #[test]
    fn vertical_walk_picks_the_nearest_overlapping_cell() {
        let mut state = EditorState::open(0, None);
        state.set_dimensions(3, 3);
        assert_eq!(state.vertical_neighbor(0, Dir::Down), Some(3));
        assert_eq!(state.vertical_neighbor(3, Dir::Up), Some(0));
        assert_eq!(state.vertical_neighbor(4, Dir::Up), Some(1));
        assert_eq!(state.vertical_neighbor(6, Dir::Down), None);

        // A wide top cell sits above two bottom units; the nearer column wins.
        state.enter();
        state.selected = 1;
        assert!(state.enter());
        assert_eq!(state.grid.cells()[state.selected].col_span, 2);
        assert_eq!(state.vertical_neighbor(state.selected, Dir::Down), Some(2));
    }

    #[test]
    fn split_acts_on_the_anchor_even_after_walking_to_the_button() {
        let mut state = EditorState::open(0, None);
        state.enter(); // anchor on the 2x1 made below
        state.selected = 1;
        assert!(state.enter()); // merge the top pair into a wide cell
        assert_eq!(state.anchor, None);
        state.enter(); // anchor that wide cell
        state.selected = 2; // walking down to the button visits a unit cell
        assert_eq!(state.split_target(), Some(0));
        assert!(state.split(state.split_target().unwrap()));
        assert_eq!(state.grid, CustomGrid::uniform(2, 2));
    }

    #[test]
    fn a_wide_cell_can_absorb_a_different_sized_neighbour() {
        let mut state = EditorState::open(0, None);
        state.set_dimensions(3, 2);
        state.enter();
        state.selected = 1;
        assert!(state.enter()); // 2x1 across the top left
        state.selected = 0;
        state.enter();
        state.selected = 1;
        assert!(state.enter()); // the 1x1 at top right joins it
        assert_eq!(state.grid.count, 4);
        assert_eq!(state.grid.display_label(), "4");
        assert!(state.split(0));
        assert_eq!(state.grid, CustomGrid::uniform(3, 2));
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

                // The remote enters on the first row button, not on the cross.
                app.layout_editor.anchor = None;
                ui.add_space(theme::space::S);
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
                            if app.layout_editor.anchor.is_none() && is_rows && value == 1 {
                                app.layout_editor.anchor = Some(response.id);
                            }
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

                ui.add_space(theme::space::XS);
                let hint = if let Some(anchor) = payload.anchor {
                    monitor_i18n::tr_args(
                        "layout-editor-merge-picked",
                        &[("cell", (anchor + 1).into())],
                    )
                } else {
                    monitor_i18n::tr("layout-editor-merge-hint")
                };
                ui.label(RichText::new(hint).small().color(theme::ACCENT));

                ui.add_space(theme::space::S);
                let preview_w = ui.available_width().min(220.0);
                let area = ui.allocate_space(vec2(preview_w, 110.0)).1;
                let layout = GridLayout::Custom(payload.grid);
                grid::layout_thumbnail(ui.painter(), area, layout);
                // egui turns Enter/Space into a click on a focused widget
                // whose Sense includes click, so `response.clicked()` is the
                // one press from both a pointer and a remote. Reading the keys
                // again here would take the same press twice: set the anchor,
                // then immediately clear it. They are only used to tell which
                // way the press came, so a merge can move the ring or not.
                let focused = ctx.memory(|memory| memory.focused());
                // One scope per baseline row: Left/Right walk the cells that
                // start in that row, Up/Down cross to the same seat in the
                // row above or below. A snapshot keeps the borrow of `payload`
                // clear while a press edits the grid mid-walk.
                let cells = payload.grid.cells().to_vec();
                let mut changed = false;
                for (row, scope) in LAYOUT_EDITOR_GRID_ROWS.iter().enumerate() {
                    app.nav.open(scope);
                    for (index, cell) in cells.iter().enumerate() {
                        if usize::from(cell.row) != row {
                            continue;
                        }
                        let rect = grid::tile_rect(area, layout, index, 2.0);
                        let id = egui::Id::new(("layout-editor-cell", index));
                        let response = ui.interact(rect, id, egui::Sense::click());
                        if response.has_focus() {
                            payload.selected = index;
                        }
                        if response.clicked() {
                            payload.selected = index;
                            changed = payload.enter();
                        }
                        let by_key = changed
                            && focused == Some(response.id)
                            && ctx.input(|input| {
                                input::CONFIRM_KEYS.iter().any(|key| input.key_pressed(*key))
                            });
                        app.nav.item(&response);
                        if changed {
                            // The merge re-sorted the cells. The surviving
                            // shape is at `selected`; a remote needs the ring
                            // put on it rather than on the index that just
                            // went away. A pointer press moves no ring.
                            if by_key {
                                app.restore_focus =
                                    Some(egui::Id::new(("layout-editor-cell", payload.selected)));
                            }
                            break;
                        }

                        // A hand-drawn control has no widget visuals of its
                        // own: `interact_selectable` is what turns a focus
                        // ring into a visible stroke, the same way the
                        // picker's thumbnails do.
                        let anchored = payload.anchor == Some(index);
                        let visuals = ui.style().interact_selectable(&response, anchored);
                        if ui.is_rect_visible(rect) {
                            let painter = ui.painter();
                            let corner = egui::CornerRadius::same(theme::radius::S);
                        let fill = if anchored {
                            theme::ACCENT.gamma_multiply(0.55)
                        } else {
                            visuals.weak_bg_fill
                        };
                        painter.rect_filled(rect, corner, fill);
                        if anchored {
                            // The cyan ring means "the remote is here"; the
                            // anchor needs a different signal, or the first
                            // Enter looks like nothing happened. Accent fill
                            // plus a bright, heavier frame does that.
                            painter.rect_stroke(
                                rect,
                                corner,
                                egui::Stroke::new(2.5_f32, theme::TEXT_BRIGHT),
                                egui::StrokeKind::Inside,
                            );
                        } else if visuals.bg_stroke.width > 0.0 {
                            painter.rect_stroke(rect, corner, visuals.bg_stroke, egui::StrokeKind::Inside);
                        }
                            painter.text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                (index + 1).to_string(),
                                egui::FontId::proportional(13.0),
                                visuals.text_color(),
                            );
                        }
                    }
                    app.nav.close();
                    if changed {
                        break;
                    }
                }

                ui.add_space(theme::space::S);
                app.nav.open("layout-editor-actions");
                ui.horizontal(|ui| {
                    let split = app
                        .nav
                        .tracked(ui.button(monitor_i18n::tr("layout-editor-split")));
                    if split.clicked() {
                        if let Some(target) = payload.split_target() {
                            payload.split(target);
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
