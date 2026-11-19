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

/// Every row of the editor starts at this same inset from the panel edge.
/// The row/column labels, hint, preview and buttons all share it, so their
/// left edges line up whatever the width of the text in any one row.
const INSET: f32 = theme::space::XS;

/// The preview has to hold seven numbered cells a side. The first pass at
/// 260x180 gave ~37x26pt cells, which the screenshot showed was roomy; 238x134
/// keeps the preview near 16:9 while still leaving ~19pt per row for the 13pt
/// digits. The modal follows the preview so the hint beside it keeps its own
/// readable column.
const EDITOR_WIDTH: f32 = 420.0;
const PREVIEW_W: f32 = 238.0;
const PREVIEW_H: f32 = 134.0;

/// Exact width egui gives a text button: the Button-font text plus both pads.
/// `horizontal_centered` only centres vertically, so a leading space built
/// from the real button widths is what actually centres the row.
pub(super) fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    let galley = ui.fonts(|fonts| {
        fonts.layout_no_wrap(
            text.to_owned(),
            egui::TextStyle::Button.resolve(ui.style()),
            theme::TEXT,
        )
    });
    galley.rect.width() + ui.spacing().button_padding.x * 2.0
}


/// How many unit columns two cells share. A vertical neighbour with no
/// overlap is not above or below this one, even if its row is adjacent.
fn overlap_columns(a: &Cell, b: &Cell) -> u16 {
    let left = u16::from(a.col.max(b.col));
    let right = u16::from(a.col + a.col_span).min(u16::from(b.col) + u16::from(b.col_span));
    right.saturating_sub(left)
}

/// The cell under the pointer at release. A drag that ends outside the
/// preview, or in a gap between cells, has nowhere legal to land.
fn cell_under(
    area: egui::Rect,
    layout: GridLayout,
    cells: &[Cell],
    pos: Option<egui::Pos2>,
) -> Option<usize> {
    let pos = pos?;
    if !area.contains(pos) {
        return None;
    }
    cells.iter().enumerate().find_map(|(index, _)| {
        grid::tile_rect(area, layout, index, 2.0).contains(pos).then_some(index)
    })
}

/// The unit rectangle spanned by the two ends of a drag. Every cell wholly
/// inside it would be merged on release, so the whole set is highlighted
/// rather than just the endpoints.
fn drag_bounds(cells: &[Cell], from: usize, to: usize) -> Option<(u16, u16, u16, u16)> {
    let (Some(a), Some(b)) = (cells.get(from), cells.get(to)) else {
        return None;
    };
    Some((
        a.col.min(b.col).into(),
        a.row.min(b.row).into(),
        (u16::from(a.col) + u16::from(a.col_span))
            .max(u16::from(b.col) + u16::from(b.col_span)),
        (u16::from(a.row) + u16::from(a.row_span))
            .max(u16::from(b.row) + u16::from(b.row_span)),
    ))
}

/// The cell the keyboard ring was on before an edit. Its index can vanish in
/// the re-sort, so the ring has to be put back on whatever cell now covers
/// the same unit after a pointer merge.
fn focused_cell(cells: &[Cell], focused: Option<egui::Id>) -> Option<(usize, Cell)> {
    let focused = focused?;
    cells.iter().enumerate().find_map(|(index, cell)| {
        (focused == egui::Id::new(("layout-editor-cell", index))).then_some((index, *cell))
    })
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
    /// Set when the Delete button was pressed: the caller clears the slot
    /// instead of writing the preview into it.
    pub(super) delete: bool,
    /// First half of a merge pair. Set by the first Enter on a cell, then the
    /// viewer walks to a neighbour and presses Enter again. Keeping it
    /// explicit means a single press can never merge something by accident.
    pub(super) anchor: Option<usize>,
    /// Cell a pointer drag began on, and the cell it is over now. Both are
    /// cleared on release; neither is written to the configuration.
    pub(super) drag_from: Option<usize>,
    pub(super) drag_to: Option<usize>,
}

impl EditorState {
    /// Editing an existing slot loads its shape; a new slot starts from the
    /// default 5x5 uniform grid rather than an empty preview.
    pub(super) fn open(slot: usize, existing: Option<CustomGrid>) -> Self {
        let grid = existing.unwrap_or_else(|| CustomGrid::uniform(5, 5));
        EditorState {
            slot,
            slot_focus: None,
            grid,
            selected: 0,
            anchor: None,
            delete: false,
            drag_from: None,
            drag_to: None,
        }
    }

    /// Rebuild from plain dimensions. The selection is clamped to the new
    /// grid and any half-made pair is forgotten.
    pub(super) fn set_dimensions(&mut self, cols: u8, rows: u8) {
        self.grid = CustomGrid::uniform(cols, rows);
        self.selected = self.selected.min(self.grid.cells().len().saturating_sub(1));
        self.anchor = None;
        self.drag_from = None;
        self.drag_to = None;
    }

    /// The first Enter on a cell marks it; the second, on a neighbour, merges
    /// them. Pressing it on the anchor again clears the mark. Returns true
    /// when the grid actually changed.
    pub(super) fn enter(&mut self) -> bool {
        self.drag_from = None;
        self.drag_to = None;
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

    /// Finish a pointer drag from one cell to another. The rectangle has to
    /// be tiled exactly by the cells inside it; an illegal landing changes
    /// nothing. The survivor is followed through the re-sort so a later
    /// split or merge still acts on the shape under the pointer.
    pub(super) fn merge_drag(&mut self, to: usize) -> bool {
        let Some(from) = self.drag_from.take() else {
            return false;
        };
        self.drag_to = None;
        let first = self.grid.cells()[from];
        let second = self.grid.cells()[to];
        if let Some(grid) = self.grid.merge_rect(from, to) {
            self.grid = grid;
            self.select_at(first.row.min(second.row), first.col.min(second.col));
            // A pointer drag leaves the ring where it was. Marking the new
            // cell as picked gives it the same visible selection the second
            // Enter press gives, so Split is lit on a cell the viewer can
            // actually see is selected.
            self.anchor = Some(self.selected);
            true
        } else {
            false
        }
    }

    /// The Split button's target during a frame. An anchor is an explicit
    /// pick and survives the ring walking away. Without one, the only valid
    /// target is the cell the ring is on right now: a cell that merely used
    /// to have the ring must not keep Split lit after the ring has left.
    pub(super) fn split_target_for(&self, focused: Option<usize>) -> Option<usize> {
        [self.anchor, focused]
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
            self.drag_from = None;
            self.drag_to = None;
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
    fn a_touch_release_finishes_with_the_last_highlighted_cell() {
        let source = include_str!("layout_editor.rs");
        let guarded = format!("if let Some({}) = {}", "pointer", "pointer");
        assert!(source.contains(&guarded), "a lifted finger may lose latest_pos");
        let finish = format!("{}.{}(landing)", "payload", "merge_drag");
        assert!(
            source.matches(&finish).count() >= 2,
            "the widget release and the frame-end release both merge"
        );
    }

    #[test]
    fn the_preview_room_fits_seven_numbered_cells() {
        let cell_w = PREVIEW_W / f32::from(MAX_CUSTOM_DIMENSION);
        let cell_h = PREVIEW_H / f32::from(MAX_CUSTOM_DIMENSION);
        assert!(
            cell_w >= 24.0 && cell_h >= 17.0,
            "13pt digits need their own box"
        );
        assert!(
            cell_w <= 40.0 && cell_h <= 24.0,
            "the cells should stay compact at 7x7"
        );
        assert!(
            (PREVIEW_W / PREVIEW_H - 16.0 / 9.0).abs() < 0.08,
            "the preview stays close to 16:9"
        );
        assert!(
            EDITOR_WIDTH > PREVIEW_W + theme::space::M * 3.0,
            "the hint keeps a column beside the preview"
        );
    }

    #[test]
    fn a_new_slot_starts_from_the_default_grid() {
        let state = EditorState::open(2, None);
        assert_eq!(state.slot, 2);
        assert_eq!(state.grid.cells().len(), 25);
        assert_eq!(state.grid.display_label(), "5x5");
        assert_eq!(state.anchor, None);
        assert_eq!(state.drag_from, None);
        assert_eq!(state.drag_to, None);
        assert!(!state.delete);
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
        state.set_dimensions(2, 2);
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
    fn split_follows_the_ring_only_while_it_is_on_the_cell() {
        let merged = CustomGrid::uniform(2, 2).merge(0, 1).unwrap();
        let mut state = EditorState::open(0, Some(merged));
        state.selected = 0;
        assert_eq!(state.split_target_for(Some(0)), Some(0));
        assert_eq!(state.split_target_for(None), None, "the ring left the cell");
        state.anchor = Some(0);
        assert_eq!(state.split_target_for(None), Some(0), "an explicit pick stays");
    }

    #[test]
    fn split_is_only_available_while_a_cell_can_be_broken_up() {
        let mut state = EditorState::open(0, None);
        assert_eq!(state.split_target_for(None), None);
        state.enter();
        state.selected = 1;
        assert!(state.enter());
        assert_eq!(state.split_target_for(Some(0)), Some(0));
        assert!(state.split(state.split_target_for(Some(0)).unwrap()));
        assert_eq!(state.split_target_for(None), None);
    }

    #[test]
    fn a_focused_cell_is_found_before_the_grid_is_re_sorted() {
        let grid = CustomGrid::uniform(2, 2);
        let (index, cell) = focused_cell(
            grid.cells(),
            Some(egui::Id::new(("layout-editor-cell", 1))),
        )
        .unwrap();
        assert_eq!(index, 1);
        assert_eq!((cell.row, cell.col), (0, 1));
        assert_eq!(focused_cell(grid.cells(), None), None);
        assert_eq!(
            focused_cell(grid.cells(), Some(egui::Id::new("layout-editor-cell"))),
            None
        );
    }

    #[test]
    fn the_drag_highlight_covers_the_whole_rectangle() {
        let grid = CustomGrid::uniform(3, 3);
        assert_eq!(drag_bounds(grid.cells(), 0, 8), Some((0, 0, 3, 3)));
        assert_eq!(drag_bounds(grid.cells(), 1, 5), Some((1, 0, 3, 2)));
        assert_eq!(drag_bounds(grid.cells(), 0, 9), None);
    }

    #[test]
    fn a_pointer_drag_merges_the_rectangle_it_covers() {
        let mut state = EditorState::open(0, None);
        state.set_dimensions(2, 2);
        state.drag_from = Some(0);
        state.drag_to = Some(3);
        assert!(state.merge_drag(3));
        assert_eq!(state.grid.count, 1);
        assert_eq!(
            state.grid.cells()[0],
            monitor_core::layout::Cell { col: 0, row: 0, col_span: 2, row_span: 2 }
        );
        assert_eq!(state.drag_from, None);
        assert_eq!(state.drag_to, None);
        assert_eq!(state.anchor, Some(state.selected));

        let merged = state.grid;
        state.drag_from = Some(0);
        assert!(!state.merge_drag(0), "a drag inside one cell changes nothing");
        assert_eq!(state.grid, merged);
        assert_eq!(state.drag_from, None);
        assert_eq!(state.anchor, Some(state.selected));
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
        state.set_dimensions(2, 2);
        state.enter(); // anchor on the 2x1 made below
        state.selected = 1;
        assert!(state.enter()); // merge the top pair into a wide cell
        assert_eq!(state.anchor, None);
        state.enter(); // anchor that wide cell
        state.selected = 2; // walking down to the button visits a unit cell
        assert_eq!(state.split_target_for(None), Some(0));
        assert!(state.split(state.split_target_for(None).unwrap()));
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
    /// Returns the finished state on Confirm or Delete, so the caller can
    /// write the preview to the slot or clear it. Back and the cross return
    /// `None` and change nothing.
    pub(super) fn draw_layout_editor(
        &mut self,
        ctx: &egui::Context,
    ) -> (Answer, Option<EditorState>) {
        let mut answer = Answer::STAY;
        let mut confirmed = false;
        let mut back = false;
                let mut finished: Option<EditorState> = None;
                // egui gives up the focus the moment a control is drawn grey,
                // so the greyed Split button cannot tell us afterwards that
                // the ring was on it. Remember it before anything is drawn.
                let focus_before = ctx.memory(|memory| memory.focused());

                self.modal_frame(
            ctx,
            "xgview-layout-editor",
            EDITOR_WIDTH,
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
                // One fixed slot for both labels: "Columns" is wider than
                // "Rows", so a hard-coded width lets the digit columns drift
                // apart. Measure the translated text at the same size egui
                // will draw it, then force that width on both rows.
                // Match the digits: those are Button text, whose font can be
                // larger than Body. Using the row height as a font size made
                // the labels visibly bigger than the buttons.
                let label_font = egui::TextStyle::Button.resolve(ui.style());
                let label_w = ui.fonts(|fonts| {
                    ["layout-editor-rows", "layout-editor-columns"]
                        .iter()
                        .map(|key| {
                            fonts
                                .layout_no_wrap(
                                    monitor_i18n::tr(key).to_string(),
                                    label_font.clone(),
                                    theme::TEXT,
                                )
                                .rect
                                .width()
                        })
                        .fold(0.0_f32, f32::max)
                }) + theme::space::M;
                // The remote enters on the hint, an invisible focus target,
                // so opening the editor shows no ring until a key moves it.
                ui.add_space(theme::space::S);
                for (key, scope, is_rows) in [
                    ("layout-editor-rows", "layout-editor-rows", true),
                    ("layout-editor-columns", "layout-editor-columns", false),
                ] {
                    app.nav.open(scope);
                    ui.horizontal(|ui| {
                        ui.add_space(INSET);
                        // Reserve the whole slot before drawing into it: a
                        // child label only reports the width of its own text,
                        // so the shorter "Rows" let the digits slide left.
                        let row_h = ui.text_style_height(&egui::TextStyle::Button)
                            + ui.spacing().button_padding.y * 2.0;
                        let slot = ui
                            .allocate_exact_size(vec2(label_w, row_h), egui::Sense::hover())
                            .0;
                        ui.painter().text(
                            slot.left_center(),
                            egui::Align2::LEFT_CENTER,
                            monitor_i18n::tr(key),
                            label_font.clone(),
                            theme::TEXT,
                        );
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

                let hint = if let Some(anchor) = payload.anchor {
                    // A merged cell has a second way out besides merging:
                    // say so while it is the one under the ring.
                    let key = if payload.grid.can_split(anchor) {
                        "layout-editor-merge-picked-splittable"
                    } else {
                        "layout-editor-merge-picked"
                    };
                    monitor_i18n::tr_args(key, &[("cell", (anchor + 1).into())])
                } else {
                    monitor_i18n::tr("layout-editor-merge-hint")
                };
                ui.add_space(theme::space::M);
                let mut area = egui::Rect::NOTHING;
                let mut hint_rect = egui::Rect::NOTHING;
                ui.horizontal(|ui| {
                    ui.add_space(INSET);
                    area = ui
                        .allocate_exact_size(vec2(PREVIEW_W, PREVIEW_H), egui::Sense::hover())
                        .0;
                    ui.add_space(theme::space::M);
                    // The hint sits beside the preview in a fixed-height
                    // block. Its wording changes when a cell is picked; with
                    // its own column the rows below no longer jump as it
                    // re-wraps.
                    let hint_w = ui.available_width();
                    ui.allocate_ui_with_layout(
                        vec2(hint_w, PREVIEW_H),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| {
                            let label = ui.add(
                                egui::Label::new(
                                    RichText::new(hint).small().color(theme::ACCENT),
                                )
                                .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                            hint_rect = label.rect;
                        },
                    );
                });
                // Like the portrait-aspect panel: the hint is a focus target
                // with no visuals of its own. The window hands the ring here
                // and the viewer sees nothing until an arrow moves it on.
                let hint_focus = ui.interact(
                    hint_rect,
                    ui.id().with("layout-editor-hint-focus"),
                    egui::Sense::focusable_noninteractive(),
                );
                app.nav.open("layout-editor-entry");
                app.nav.item(&hint_focus);
                app.nav.close();
                app.layout_editor.anchor = Some(hint_focus.id);
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
                let focused_cell = focused_cell(&cells, focused);
                let pointer = ctx.input(|input| input.pointer.latest_pos());
                let drag_region = payload
                    .drag_from
                    .zip(payload.drag_to)
                    .and_then(|(from, to)| drag_bounds(&cells, from, to));
                let mut changed = false;
                for (row, scope) in LAYOUT_EDITOR_GRID_ROWS.iter().enumerate() {
                    app.nav.open(scope);
                    for (index, cell) in cells.iter().enumerate() {
                        if usize::from(cell.row) != row {
                            continue;
                        }
                        let rect = grid::tile_rect(area, layout, index, 2.0);
                        let id = egui::Id::new(("layout-editor-cell", index));
                        let response = ui.interact(rect, id, egui::Sense::click_and_drag());
                        if response.has_focus() {
                            payload.selected = index;
                        }
                        // A press starts a rectangle drag. The cells under
                        // the start and the pointer are marked while it is
                        // held; only the release edits the grid.
                        if response.drag_started() {
                            payload.drag_from = Some(index);
                            payload.drag_to = Some(index);
                            payload.selected = index;
                            payload.anchor = None;
                        }
                        if payload.drag_from == Some(index) {
                            // A touch release can clear `latest_pos` on the
                            // frame the finger lifts. Keep the last cell the
                            // drag highlighted instead of falling back to the
                            // start cell, which would cancel the merge.
                            if let Some(pointer) = pointer {
                                payload.drag_to = cell_under(area, layout, &cells, Some(pointer));
                            }
                            if response.drag_stopped() {
                                let landing = payload.drag_to.unwrap_or(index);
                                changed = payload.merge_drag(landing);
                            }
                        }
                        let dragged = response.drag_started()
                            || response.dragged()
                            || response.drag_stopped();
                        let confirm_by_key = focused == Some(response.id)
                            && ctx.input(|input| {
                                input::CONFIRM_KEYS.iter().any(|key| input.key_pressed(*key))
                            });
                        if response.clicked() && !dragged {
                            payload.selected = index;
                            changed = payload.enter();
                            if changed && !confirm_by_key {
                                // A pointer press moves no ring, so mark the
                                // survivor as picked to show what Split acts
                                // on. The keyboard already gets the ring.
                                payload.anchor = Some(payload.selected);
                            }
                        }
                        app.nav.item(&response);
                        if changed {
                            // An edit re-sorts the cells, so the index the
                            // ring held can name another shape or none at
                            // all. Put it back on the cell that now covers
                            // the same unit, whether the merge came from a
                            // key or from a pointer. With no ring in the
                            // grid a pointer press creates none.
                            if let Some((_, was)) = focused_cell {
                                let target = payload
                                    .grid
                                    .cells()
                                    .iter()
                                    .position(|cell| {
                                        cell.row <= was.row
                                            && cell.col <= was.col
                                            && u16::from(cell.row)
                                                + u16::from(cell.row_span)
                                                >= u16::from(was.row) + 1
                                            && u16::from(cell.col)
                                                + u16::from(cell.col_span)
                                                >= u16::from(was.col) + 1
                                    })
                                    .unwrap_or(payload.selected);
                                app.restore_focus =
                                    Some(egui::Id::new(("layout-editor-cell", target)));
                            }
                            break;
                        }

                        // A hand-drawn control has no widget visuals of its
                        // own: `interact_selectable` is what turns a focus
                        // ring into a visible stroke, the same way the
                        // picker's thumbnails do.
                        let in_drag = drag_region.is_some_and(|(left, top, right, bottom)| {
                            let (col, row) = (u16::from(cell.col), u16::from(cell.row));
                            col >= left
                                && col + u16::from(cell.col_span) <= right
                                && row >= top
                                && row + u16::from(cell.row_span) <= bottom
                        });
                        let anchored = payload.anchor == Some(index) || in_drag;
                        let visuals = ui.style().interact_selectable(&response, anchored);
                        if ui.is_rect_visible(rect) {
                            let painter = ui.painter();
                            let corner = egui::CornerRadius::same(theme::radius::S);
                        // The thumbnail's white grid lines live under these
                        // cells. Fill only the one state that needs a fill,
                        // and keep it inside the line so the full outline
                        // stays visible.
                        if anchored {
                            painter.rect_filled(rect.shrink(1.0), corner, theme::ACCENT.gamma_multiply(0.55));
                        } else if response.hovered() || response.has_focus() {
                            painter.rect_filled(rect.shrink(1.0), corner, visuals.weak_bg_fill);
                        }
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
                // A drag released off the preview never reaches a cell's
                // `drag_released`. A touch release also often arrives without
                // any widget reporting `drag_stopped`, so finish the drag here
                // with the last highlighted cell before dropping the marks.
                if payload.drag_from.is_some() && !ctx.input(|input| input.pointer.primary_down()) {
                    let landing = payload.drag_to.unwrap_or_else(|| payload.drag_from.unwrap());
                    payload.merge_drag(landing);
                }

                ui.add_space(theme::space::S);
                // `horizontal_centered` centres vertically, not horizontally,
                // so pad by the measured width of the three buttons instead.
                // This keeps the row centred without claiming extra height.
                app.nav.open("layout-editor-actions");
                ui.horizontal(|ui| {
                    let labels = [
                        monitor_i18n::tr("layout-editor-split"),
                        monitor_i18n::tr("ok"),
                        monitor_i18n::tr("layout-editor-delete"),
                    ];
                    let used = labels.iter().map(|text| button_width(ui, text)).sum::<f32>()
                        + 2.0 * ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - used) * 0.5).max(0.0));
                    let split_target =
                        payload.split_target_for(focused_cell.as_ref().map(|(index, _)| *index));
                    let can_split = split_target.is_some();
                    // A disabled control must not stay in the ring: the
                    // remote would land on a grey button with no focus ring
                    // and think the focus was lost. Skip tracking it; if the
                    // ring was on it when the cell lost focus, OK takes it.
                    let mut split_greyed_with_ring = false;
                    let split = ui.add_enabled(
                        can_split,
                        egui::Button::new(labels[0].clone()),
                    );
                    if can_split {
                        let split = app.nav.tracked(split);
                        if split.clicked() {
                            if let Some(target) = split_target {
                                // Only move a ring that was actually here:
                                // a pointer press with the ring elsewhere
                                // leaves it where the viewer put it.
                                let ring_was_on_split = focus_before == Some(split.id);
                                let ring_was_on_target = focus_before
                                    == Some(egui::Id::new(("layout-editor-cell", target)));
                                payload.split(target);
                                if ring_was_on_split || ring_was_on_target {
                                    // Splitting greys this button out. Put the
                                    // ring on the first unit it just made,
                                    // not on the row of dimension buttons.
                                    app.restore_focus = Some(egui::Id::new((
                                        "layout-editor-cell",
                                        payload.selected,
                                    )));
                                    if ring_was_on_split {
                                        // The Enter that pressed Split must not
                                        // also set an anchor on the new cell.
                                        app.swallow_confirm = 2;
                                    }
                                }
                            }
                        }
                    } else if focus_before == Some(split.id) {
                        // Greying a control surrenders egui's focus, so asking
                        // from `has_focus()` here is too late. The ring came
                        // here from a cell that was not anchored; OK is the
                        // next control in the row and takes it over.
                        split_greyed_with_ring = true;
                    }
                    let ok = app.nav.tracked(ui.button(labels[1].clone()));
                    if split_greyed_with_ring {
                        app.restore_focus = Some(ok.id);
                        app.swallow_confirm = 2;
                    }
                    if ok.clicked() && payload.valid() {
                        confirmed = true;
                        finished = Some(*payload);
                    }
                    // A slot that is on the wall cannot be removed under the
                    // viewer: switch to another layout first. The greyed
                    // button is skipped by the walk, and a ring already on it
                    // is handed to OK.
                    let can_delete = !matches!(
                        app.scheduler.layout(),
                        GridLayout::Custom(current)
                            if Some(current)
                                == app.config.group().custom_layouts[payload.slot.min(3)]
                    );
                    let delete = ui.add_enabled(can_delete, egui::Button::new(labels[2].clone()));
                    if can_delete {
                        let delete = app.nav.tracked(delete);
                        if delete.clicked() {
                            payload.delete = true;
                            confirmed = true;
                            finished = Some(*payload);
                        }
                    } else if focus_before == Some(delete.id) {
                        app.restore_focus = Some(ok.id);
                        app.swallow_confirm = 2;
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
