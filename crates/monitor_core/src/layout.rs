use serde::{Deserialize, Serialize};

/// One viewport of a layout, in grid units.
///
/// Every layout divides the wall into `grid_cols() x grid_rows()` equal units;
/// a cell covers `col_span x row_span` of them, starting at `(col, row)`. The
/// square layouts have only 1x1 cells; the mixed ones merge a few units into
/// their large viewports. See [`GridLayout::cells`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub col: u8,
    pub row: u8,
    pub col_span: u8,
    pub row_span: u8,
}

/// Shorthand for building the tables below.
const fn c(col: u8, row: u8, col_span: u8, row_span: u8) -> Cell {
    Cell { col, row, col_span, row_span }
}

/// The four square grids the wall has always had, kept under their own names.
const CELLS_1X1: &[Cell] = &[c(0, 0, 1, 1)];
const CELLS_2X2: &[Cell] = &[c(0, 0, 1, 1), c(1, 0, 1, 1), c(0, 1, 1, 1), c(1, 1, 1, 1)];
const CELLS_3X3: &[Cell] = &[
    c(0, 0, 1, 1),
    c(1, 0, 1, 1),
    c(2, 0, 1, 1),
    c(0, 1, 1, 1),
    c(1, 1, 1, 1),
    c(2, 1, 1, 1),
    c(0, 2, 1, 1),
    c(1, 2, 1, 1),
    c(2, 2, 1, 1),
];
const CELLS_4X4: &[Cell] = &[
    c(0, 0, 1, 1),
    c(1, 0, 1, 1),
    c(2, 0, 1, 1),
    c(3, 0, 1, 1),
    c(0, 1, 1, 1),
    c(1, 1, 1, 1),
    c(2, 1, 1, 1),
    c(3, 1, 1, 1),
    c(0, 2, 1, 1),
    c(1, 2, 1, 1),
    c(2, 2, 1, 1),
    c(3, 2, 1, 1),
    c(0, 3, 1, 1),
    c(1, 3, 1, 1),
    c(2, 3, 1, 1),
    c(3, 3, 1, 1),
];

/// 2 cells, a 2x1 grid split left/right. Each cell is 8:9.
const CELLS_N2: &[Cell] = &[c(0, 0, 1, 1), c(1, 0, 1, 1)];
/// 3 cells on a 3x2 grid: one tall cell on the left, two stacked on the right.
const CELLS_N3: &[Cell] = &[c(0, 0, 2, 2), c(2, 0, 1, 1), c(2, 1, 1, 1)];
/// 5 cells on a 6x5 grid: two across the top, three across the bottom.
const CELLS_N5: &[Cell] = &[
    c(0, 0, 3, 3),
    c(3, 0, 3, 3),
    c(0, 3, 2, 2),
    c(2, 3, 2, 2),
    c(4, 3, 2, 2),
];
/// 6 cells on a 3x3 grid: one 2x2 in the corner, five 1x1 around it.
const CELLS_N6: &[Cell] = &[
    c(0, 0, 2, 2),
    c(2, 0, 1, 1),
    c(2, 1, 1, 1),
    c(0, 2, 1, 1),
    c(1, 2, 1, 1),
    c(2, 2, 1, 1),
];
/// 7 cells on a 4x4 grid: three 2x2 and four 1x1.
const CELLS_N7: &[Cell] = &[
    c(0, 0, 2, 2),
    c(2, 0, 2, 2),
    c(0, 2, 2, 2),
    c(2, 2, 1, 1),
    c(3, 2, 1, 1),
    c(2, 3, 1, 1),
    c(3, 3, 1, 1),
];
/// 8 cells on a 4x4 grid: one 3x3, a right column and a bottom row of 1x1.
const CELLS_N8: &[Cell] = &[
    c(0, 0, 3, 3),
    c(3, 0, 1, 1),
    c(3, 1, 1, 1),
    c(3, 2, 1, 1),
    c(3, 3, 1, 1),
    c(0, 3, 1, 1),
    c(1, 3, 1, 1),
    c(2, 3, 1, 1),
];
/// 10 cells on a 4x4 grid: two 2x2 across the top and eight 1x1 below.
const CELLS_N10: &[Cell] = &[
    c(0, 0, 2, 2),
    c(2, 0, 2, 2),
    c(0, 2, 1, 1),
    c(1, 2, 1, 1),
    c(2, 2, 1, 1),
    c(3, 2, 1, 1),
    c(0, 3, 1, 1),
    c(1, 3, 1, 1),
    c(2, 3, 1, 1),
    c(3, 3, 1, 1),
];
/// 12 cells on a 4x3 grid, all equal. Each cell is 4:3, not 16:9.
const CELLS_N12: &[Cell] = &[
    c(0, 0, 1, 1),
    c(1, 0, 1, 1),
    c(2, 0, 1, 1),
    c(3, 0, 1, 1),
    c(0, 1, 1, 1),
    c(1, 1, 1, 1),
    c(2, 1, 1, 1),
    c(3, 1, 1, 1),
    c(0, 2, 1, 1),
    c(1, 2, 1, 1),
    c(2, 2, 1, 1),
    c(3, 2, 1, 1),
];
/// 13 cells on a 4x4 grid: one 2x2 in the corner and twelve 1x1.
const CELLS_N13: &[Cell] = &[
    c(0, 0, 2, 2),
    c(2, 0, 1, 1),
    c(3, 0, 1, 1),
    c(2, 1, 1, 1),
    c(3, 1, 1, 1),
    c(0, 2, 1, 1),
    c(1, 2, 1, 1),
    c(2, 2, 1, 1),
    c(3, 2, 1, 1),
    c(0, 3, 1, 1),
    c(1, 3, 1, 1),
    c(2, 3, 1, 1),
    c(3, 3, 1, 1),
];

/// 1 cell: a single full-width viewport. The smallest of the portrait set.
const CELLS_P1: &[Cell] = &[c(0, 0, 1, 1)];
/// 2 cells stacked in one column.
const CELLS_P2: &[Cell] = &[c(0, 0, 1, 1), c(0, 1, 1, 1)];
/// 3 cells stacked in one column.
const CELLS_P3: &[Cell] = &[c(0, 0, 1, 1), c(0, 1, 1, 1), c(0, 2, 1, 1)];
/// 4 cells stacked in one column.
const CELLS_P4: &[Cell] = &[
    c(0, 0, 1, 1),
    c(0, 1, 1, 1),
    c(0, 2, 1, 1),
    c(0, 3, 1, 1),
];

/// Supported wall layouts.
///
/// Four are the square, uniform grids the wall has always had - `1x1`, `2x2`,
/// `3x3`, `4x4` - and are offered as their own buttons. The rest are mixed or
/// rectangular grids, named by the number of viewports they show, and offered
/// through the layout picker. See [`GridLayout::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GridLayout {
    #[serde(rename = "1x1")]
    G1x1,
    #[serde(rename = "2x2")]
    G2x2,
    #[serde(rename = "3x3")]
    G3x3,
    #[serde(rename = "4x4")]
    G4x4,
    #[serde(rename = "2")]
    N2,
    #[serde(rename = "3")]
    N3,
    #[serde(rename = "5")]
    N5,
    #[serde(rename = "6")]
    N6,
    #[serde(rename = "7")]
    N7,
    #[serde(rename = "8")]
    N8,
    #[serde(rename = "10")]
    N10,
    #[serde(rename = "12")]
    N12,
    #[serde(rename = "13")]
    N13,
    /// Portrait: one column, one row.
    #[serde(rename = "p1")]
    P1,
    /// Portrait: one column, two rows.
    #[serde(rename = "p2")]
    P2,
    /// Portrait: one column, three rows.
    #[serde(rename = "p3")]
    P3,
    /// Portrait: one column, four rows.
    #[serde(rename = "p4")]
    P4,
}

impl Default for GridLayout {
    fn default() -> Self {
        GridLayout::G2x2
    }
}

impl GridLayout {
    /// The four square layouts, shown as buttons of their own.
    pub const STANDARD: [GridLayout; 4] =
        [GridLayout::G1x1, GridLayout::G2x2, GridLayout::G3x3, GridLayout::G4x4];

    /// The layouts chosen from the picker, in the order the picker lists them.
    pub const EXTENDED: [GridLayout; 9] = [
        GridLayout::N2,
        GridLayout::N3,
        GridLayout::N5,
        GridLayout::N6,
        GridLayout::N7,
        GridLayout::N8,
        GridLayout::N10,
        GridLayout::N12,
        GridLayout::N13,
    ];

    /// All layouts: the square ones first, then the rest by viewport count.
    pub const ALL: [GridLayout; 13] = [
        GridLayout::G1x1,
        GridLayout::G2x2,
        GridLayout::G3x3,
        GridLayout::G4x4,
        GridLayout::N2,
        GridLayout::N3,
        GridLayout::N5,
        GridLayout::N6,
        GridLayout::N7,
        GridLayout::N8,
        GridLayout::N10,
        GridLayout::N12,
        GridLayout::N13,
    ];

    /// The portrait layouts, offered as their own buttons while upright: one
    /// column, one to four rows. Kept out of [`GridLayout::ALL`] so the
    /// landscape cycle and the picker never walk into them; the wall switches
    /// sets with the orientation.
    pub const PORTRAIT: [GridLayout; 4] =
        [GridLayout::P1, GridLayout::P2, GridLayout::P3, GridLayout::P4];

    /// The viewports of this layout, in the order channels fill them.
    pub fn cells(self) -> &'static [Cell] {
        match self {
            GridLayout::G1x1 => CELLS_1X1,
            GridLayout::G2x2 => CELLS_2X2,
            GridLayout::G3x3 => CELLS_3X3,
            GridLayout::G4x4 => CELLS_4X4,
            GridLayout::N2 => CELLS_N2,
            GridLayout::N3 => CELLS_N3,
            GridLayout::N5 => CELLS_N5,
            GridLayout::N6 => CELLS_N6,
            GridLayout::N7 => CELLS_N7,
            GridLayout::N8 => CELLS_N8,
            GridLayout::N10 => CELLS_N10,
            GridLayout::N12 => CELLS_N12,
            GridLayout::N13 => CELLS_N13,
            GridLayout::P1 => CELLS_P1,
            GridLayout::P2 => CELLS_P2,
            GridLayout::P3 => CELLS_P3,
            GridLayout::P4 => CELLS_P4,
        }
    }

    /// The number of grid columns the cells are laid out against.
    pub fn grid_cols(self) -> usize {
        match self {
            GridLayout::G1x1 => 1,
            GridLayout::G2x2 => 2,
            GridLayout::G3x3 => 3,
            GridLayout::G4x4 => 4,
            GridLayout::N2 => 2,
            GridLayout::N3 => 3,
            GridLayout::N5 => 6,
            GridLayout::N6 => 3,
            GridLayout::N7 => 4,
            GridLayout::N8 => 4,
            GridLayout::N10 => 4,
            GridLayout::N12 => 4,
            GridLayout::N13 => 4,
            GridLayout::P1 | GridLayout::P2 | GridLayout::P3 | GridLayout::P4 => 1,
        }
    }

    /// The number of grid rows the cells are laid out against.
    pub fn grid_rows(self) -> usize {
        match self {
            GridLayout::G1x1 => 1,
            GridLayout::G2x2 => 2,
            GridLayout::G3x3 => 3,
            GridLayout::G4x4 => 4,
            GridLayout::N2 => 1,
            GridLayout::N3 => 2,
            GridLayout::N5 => 5,
            GridLayout::N6 => 3,
            GridLayout::N7 => 4,
            GridLayout::N8 => 4,
            GridLayout::N10 => 4,
            GridLayout::N12 => 3,
            GridLayout::N13 => 4,
            GridLayout::P1 => 1,
            GridLayout::P2 => 2,
            GridLayout::P3 => 3,
            GridLayout::P4 => 4,
        }
    }

    /// Number of visible viewports in one page.
    pub fn capacity(self) -> usize {
        self.cells().len()
    }

    /// Whether this is one of the four square layouts with its own button.
    pub fn is_standard(self) -> bool {
        matches!(
            self,
            GridLayout::G1x1 | GridLayout::G2x2 | GridLayout::G3x3 | GridLayout::G4x4
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            GridLayout::G1x1 => "1x1",
            GridLayout::G2x2 => "2x2",
            GridLayout::G3x3 => "3x3",
            GridLayout::G4x4 => "4x4",
            GridLayout::N2 => "2",
            GridLayout::N3 => "3",
            GridLayout::N5 => "5",
            GridLayout::N6 => "6",
            GridLayout::N7 => "7",
            GridLayout::N8 => "8",
            GridLayout::N10 => "10",
            GridLayout::N12 => "12",
            GridLayout::N13 => "13",
            GridLayout::P1 => "1x1",
            GridLayout::P2 => "1x2",
            GridLayout::P3 => "1x3",
            GridLayout::P4 => "1x4",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        let normalized = label.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|layout| layout.label() == normalized)
    }

    /// The viewport number a picker button shows, or `None` for the layouts
    /// that have a button of their own.
    pub fn numeric_label(self) -> Option<&'static str> {
        (!self.is_standard()).then(|| self.label())
    }

    /// The layout a digit key selects: the digit is the viewport count, and
    /// `0` stands for the 10-cell layout. 12, 13 and 16 have no digit.
    pub fn for_digit(digit: u8) -> Option<GridLayout> {
        match digit {
            1 => Some(GridLayout::G1x1),
            2 => Some(GridLayout::N2),
            3 => Some(GridLayout::N3),
            4 => Some(GridLayout::G2x2),
            5 => Some(GridLayout::N5),
            6 => Some(GridLayout::N6),
            7 => Some(GridLayout::N7),
            8 => Some(GridLayout::N8),
            9 => Some(GridLayout::G3x3),
            0 => Some(GridLayout::N10),
            _ => None,
        }
    }

    /// Next denser layout, wrapping around at the last one.
    pub fn next(self) -> Self {
        let index = self.index();
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Previous layout, wrapping around at the first one.
    pub fn prev(self) -> Self {
        let index = self.index();
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// Index of the layout inside [`GridLayout::ALL`].
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|item| *item == self).unwrap_or(0)
    }
}

/// A DPAD / keyboard navigation direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub fn is_horizontal(self) -> bool {
        matches!(self, Direction::Left | Direction::Right)
    }
}

/// Describes the currently visible page of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageInfo {
    /// Zero based page index.
    pub page: usize,
    /// Total number of pages (always >= 1).
    pub page_count: usize,
    /// Viewports available on this page.
    pub capacity: usize,
    /// Total number of channels.
    pub total: usize,
    /// First channel index shown on this page.
    pub start: usize,
    /// Channel index one past the last channel shown on this page.
    pub end: usize,
}

impl PageInfo {
    /// Number of channels actually displayed on this page.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of grid cells to draw (always the full capacity of the layout).
    pub fn cells(&self) -> usize {
        self.capacity
    }

    pub fn contains(&self, index: usize) -> bool {
        index >= self.start && index < self.end
    }

    pub fn has_previous(&self) -> bool {
        self.page > 0
    }

    pub fn has_next(&self) -> bool {
        self.page + 1 < self.page_count
    }
}

/// Number of pages needed for `total` channels in the given layout.
pub fn page_count(total: usize, capacity: usize) -> usize {
    if capacity == 0 {
        return 1;
    }
    total.div_ceil(capacity).max(1)
}

/// Page containing the given channel index.
pub fn page_of(index: usize, capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    index / capacity
}

/// Builds the [`PageInfo`] of a page, clamping out of range values.
pub fn page_info(page: usize, layout: GridLayout, total: usize) -> PageInfo {
    let capacity = layout.capacity();
    let page_count = page_count(total, capacity);
    let page = page.min(page_count - 1);
    let start = page * capacity;
    let end = (start + capacity).min(total);
    PageInfo { page, page_count, capacity, total, start, end }
}

/// Result of a DPAD navigation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigateOutcome {
    /// Focus moved inside the current page.
    Moved { focus: usize },
    /// Focus crossed a page border; the page changed as well.
    PageTurned { page: usize, focus: usize },
    /// The focus is already at the outermost border.
    Blocked,
}

/// The cell a direction press lands on, or `None` when the press leaves the
/// page.
///
/// Adjacency is geometric rather than arithmetic, because the mixed layouts
/// have cells of different sizes: a cell's neighbour is the one whose edge is
/// flush against it and whose other axis overlaps it. When more than one
/// qualifies - a large cell beside two small ones - the one whose overlap is
/// centred nearest is chosen. Only the first `visible` cells are candidates, so
/// the empty seats of a partial last page are never a neighbour.
fn neighbour(layout: GridLayout, cell: usize, dir: Direction, visible: usize) -> Option<usize> {
    let cells = layout.cells();
    let cur = cells[cell];
    let (cur_c0, cur_c1) = (i32::from(cur.col), i32::from(cur.col + cur.col_span));
    let (cur_r0, cur_r1) = (i32::from(cur.row), i32::from(cur.row + cur.row_span));
    let cur_col_centre = (cur_c0 + cur_c1) as f32 * 0.5;
    let cur_row_centre = (cur_r0 + cur_r1) as f32 * 0.5;

    let mut best: Option<(f32, usize)> = None;
    for (index, other) in cells.iter().enumerate().take(visible) {
        if index == cell {
            continue;
        }
        let (o_c0, o_c1) = (i32::from(other.col), i32::from(other.col + other.col_span));
        let (o_r0, o_r1) = (i32::from(other.row), i32::from(other.row + other.row_span));
        let row_overlap = o_r0.max(cur_r0) < o_r1.min(cur_r1);
        let col_overlap = o_c0.max(cur_c0) < o_c1.min(cur_c1);

        let (adjacent, distance) = match dir {
            Direction::Right => (
                o_c0 == cur_c1 && row_overlap,
                overlap_distance(o_r0, o_r1, cur_r0, cur_r1, cur_row_centre),
            ),
            Direction::Left => (
                o_c1 == cur_c0 && row_overlap,
                overlap_distance(o_r0, o_r1, cur_r0, cur_r1, cur_row_centre),
            ),
            Direction::Down => (
                o_r0 == cur_r1 && col_overlap,
                overlap_distance(o_c0, o_c1, cur_c0, cur_c1, cur_col_centre),
            ),
            Direction::Up => (
                o_r1 == cur_r0 && col_overlap,
                overlap_distance(o_c0, o_c1, cur_c0, cur_c1, cur_col_centre),
            ),
        };
        if !adjacent {
            continue;
        }
        // Keep the first of equals, so a tie falls to the lower cell index.
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, index));
        }
    }
    best.map(|(_, index)| index)
}

/// How far the centre of the overlap between two intervals is from `centre`.
fn overlap_distance(a0: i32, a1: i32, b0: i32, b1: i32, centre: f32) -> f32 {
    let low = a0.max(b0) as f32;
    let high = a1.min(b1) as f32;
    ((low + high) * 0.5 - centre).abs()
}

/// The cell a page turn enters on: the leftmost column for a forward turn, the
/// rightmost for a backward one, among the cells whose row overlaps `cell`'s.
fn page_entry(layout: GridLayout, cell: usize, at_start: bool, visible: usize) -> usize {
    let cells = layout.cells();
    let cur = cells[cell];
    let (cur_r0, cur_r1) = (i32::from(cur.row), i32::from(cur.row + cur.row_span));
    let grid_cols = layout.grid_cols() as i32;

    for (index, other) in cells.iter().enumerate().take(visible) {
        let (o_r0, o_r1) = (i32::from(other.row), i32::from(other.row + other.row_span));
        let (o_c0, o_c1) = (i32::from(other.col), i32::from(other.col + other.col_span));
        let row_overlap = o_r0.max(cur_r0) < o_r1.min(cur_r1);
        let edge = if at_start { o_c0 == 0 } else { o_c1 == grid_cols };
        if edge && row_overlap {
            return index;
        }
    }
    if at_start {
        0
    } else {
        visible.saturating_sub(1)
    }
}

/// Pure navigation math: computes the new focus (global channel index) and page
/// for a DPAD move.
///
/// The page turns with the sideways arrows: walking off the left or the right of
/// the wall continues on the neighbouring page, which is what the wall was
/// showing anyway. The vertical ones stop at the outermost row instead - a
/// [`NavigateOutcome::Blocked`] - because above and below the wall are the two
/// bars, and the wall's caller answers a blocked Up or Down by handing the keys
/// over to them.
///
/// `focus` is the currently focused global channel index.
pub fn navigate(
    layout: GridLayout,
    page: usize,
    focus: Option<usize>,
    dir: Direction,
    total: usize,
) -> NavigateOutcome {
    if total == 0 {
        return NavigateOutcome::Blocked;
    }

    let capacity = layout.capacity();
    let pages = page_count(total, capacity);
    let page = page.min(pages - 1);

    // Normalise the focus so that it always points inside `page`.
    let focus = match focus {
        Some(focus) if page_of(focus, capacity) == page => focus,
        _ => page * capacity,
    };
    let cell = focus - page * capacity;

    let visible_len = |page: usize| -> usize {
        let start = page * capacity;
        (start + capacity).min(total).saturating_sub(start)
    };
    let len = visible_len(page);
    if len == 0 {
        return NavigateOutcome::Blocked;
    }
    let cell = cell.min(len - 1);

    if let Some(target) = neighbour(layout, cell, dir, len) {
        return NavigateOutcome::Moved { focus: page * capacity + target };
    }

    match dir {
        // The vertical arrows stop at the wall; the page is turned by the
        // sideways ones, so a blocked Up or Down is how the wall hands the keys
        // to the bars above and below.
        Direction::Up | Direction::Down => NavigateOutcome::Blocked,
        Direction::Right => {
            if page + 1 >= pages {
                NavigateOutcome::Blocked
            } else {
                let next = page + 1;
                let cell = page_entry(layout, cell, true, visible_len(next));
                NavigateOutcome::PageTurned { page: next, focus: next * capacity + cell }
            }
        }
        Direction::Left => {
            if page == 0 {
                NavigateOutcome::Blocked
            } else {
                let previous = page - 1;
                let cell = page_entry(layout, cell, false, visible_len(previous));
                NavigateOutcome::PageTurned { page: previous, focus: previous * capacity + cell }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacities_match_layouts() {
        assert_eq!(GridLayout::G1x1.capacity(), 1);
        assert_eq!(GridLayout::G2x2.capacity(), 4);
        assert_eq!(GridLayout::G3x3.capacity(), 9);
        assert_eq!(GridLayout::G4x4.capacity(), 16);
        assert_eq!(GridLayout::N2.capacity(), 2);
        assert_eq!(GridLayout::N3.capacity(), 3);
        assert_eq!(GridLayout::N5.capacity(), 5);
        assert_eq!(GridLayout::N6.capacity(), 6);
        assert_eq!(GridLayout::N7.capacity(), 7);
        assert_eq!(GridLayout::N8.capacity(), 8);
        assert_eq!(GridLayout::N10.capacity(), 10);
        assert_eq!(GridLayout::N12.capacity(), 12);
        assert_eq!(GridLayout::N13.capacity(), 13);
    }

    /// The document's tables have to cover the grid exactly once: every unit in
    /// exactly one cell, no gap and no overlap. This is what proves a
    /// transcribed table is right.
    #[test]
    fn every_layout_tiles_its_grid_exactly() {
        for layout in GridLayout::ALL.into_iter().chain(GridLayout::PORTRAIT) {
            let cols = layout.grid_cols();
            let rows = layout.grid_rows();
            let mut covered = vec![0u32; cols * rows];
            for cell in layout.cells() {
                for row in cell.row..cell.row + cell.row_span {
                    for col in cell.col..cell.col + cell.col_span {
                        assert!(
                            usize::from(col) < cols && usize::from(row) < rows,
                            "{layout:?} has a cell outside its {cols}x{rows} grid"
                        );
                        covered[usize::from(row) * cols + usize::from(col)] += 1;
                    }
                }
            }
            assert!(
                covered.iter().all(|count| *count == 1),
                "{layout:?} does not tile its {cols}x{rows} grid"
            );
        }
    }

    #[test]
    fn digit_mapping_matches_viewport_counts() {
        assert_eq!(GridLayout::for_digit(1), Some(GridLayout::G1x1));
        assert_eq!(GridLayout::for_digit(4), Some(GridLayout::G2x2));
        assert_eq!(GridLayout::for_digit(9), Some(GridLayout::G3x3));
        assert_eq!(GridLayout::for_digit(0), Some(GridLayout::N10));
        for digit in 0..=9u8 {
            let layout = GridLayout::for_digit(digit).unwrap();
            let expected = if digit == 0 { 10 } else { usize::from(digit) };
            assert_eq!(layout.capacity(), expected, "digit {digit}");
        }
    }

    #[test]
    fn computes_pages() {
        assert_eq!(page_count(0, 16), 1);
        assert_eq!(page_count(16, 16), 1);
        assert_eq!(page_count(17, 16), 2);
        assert_eq!(page_count(32, 16), 2);
    }

    #[test]
    fn page_bounds_are_clamped() {
        let info = page_info(5, GridLayout::G2x2, 6);
        assert_eq!(info.page, 1);
        assert_eq!(info.page_count, 2);
        assert_eq!(info.start, 4);
        assert_eq!(info.end, 6);
        assert_eq!(info.len(), 2);
    }

    #[test]
    fn navigates_inside_page() {
        let outcome = navigate(GridLayout::G2x2, 0, Some(0), Direction::Right, 16);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 1 });
    }

    #[test]
    fn turns_page_at_right_border() {
        let outcome = navigate(GridLayout::G2x2, 0, Some(1), Direction::Right, 8);
        assert_eq!(outcome, NavigateOutcome::PageTurned { page: 1, focus: 4 });
    }

    #[test]
    fn clamps_on_partial_page() {
        // 6 channels in 2x2: second page holds indices 4 and 5 only.
        let outcome = navigate(GridLayout::G2x2, 1, Some(5), Direction::Right, 6);
        assert_eq!(outcome, NavigateOutcome::Blocked);
        let outcome = navigate(GridLayout::G2x2, 1, Some(4), Direction::Down, 6);
        assert_eq!(outcome, NavigateOutcome::Blocked);
    }

    #[test]
    fn page_turn_back_from_first_cell() {
        let outcome = navigate(GridLayout::G2x2, 1, Some(4), Direction::Left, 16);
        assert_eq!(outcome, NavigateOutcome::PageTurned { page: 0, focus: 1 });
    }

    /// The vertical arrows stop at the outermost row. Above and below the wall
    /// are the two bars, and the page is turned by the sideways arrows, so a
    /// blocked Up or Down is how the wall hands the keys over to them.
    #[test]
    fn up_and_down_stop_at_the_outermost_row() {
        // The top row of a page that is not the first: no page turn.
        let outcome = navigate(GridLayout::G2x2, 1, Some(4), Direction::Up, 16);
        assert_eq!(outcome, NavigateOutcome::Blocked);
        // The bottom row of a page that is not the last: no page turn either.
        let outcome = navigate(GridLayout::G2x2, 0, Some(2), Direction::Down, 16);
        assert_eq!(outcome, NavigateOutcome::Blocked);
        // Inside the page they still walk between rows.
        let outcome = navigate(GridLayout::G2x2, 0, Some(0), Direction::Down, 16);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 2 });
        let outcome = navigate(GridLayout::G2x2, 0, Some(2), Direction::Up, 16);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 0 });
    }

    /// A tall cell beside two stacked ones steps to the upper of the two.
    #[test]
    fn mixed_layout_neighbours_follow_geometry() {
        // N3: cell 0 is the tall left cell; its right neighbours are 1 and 2.
        let outcome = navigate(GridLayout::N3, 0, Some(0), Direction::Right, 3);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 1 });
        // The upper small cell steps left, back into the tall one.
        let outcome = navigate(GridLayout::N3, 0, Some(1), Direction::Left, 3);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 0 });
        // The small cells are stacked: the one below the other.
        let outcome = navigate(GridLayout::N3, 0, Some(1), Direction::Down, 3);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 2 });
        // The tall cell reaches the bottom row, so Down is blocked.
        let outcome = navigate(GridLayout::N3, 0, Some(0), Direction::Down, 3);
        assert_eq!(outcome, NavigateOutcome::Blocked);
    }

    /// The large cell of N5 steps down to the small cell nearest below it.
    #[test]
    fn n5_large_cell_steps_down_to_the_nearest_small_one() {
        // Top-right 3x3 cell (index 1) sits above the middle and right small
        // cells; the right one (index 4) is closer to its centre.
        let outcome = navigate(GridLayout::N5, 0, Some(1), Direction::Down, 5);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 4 });
        // The top-left cell steps down to its own column's small cell.
        let outcome = navigate(GridLayout::N5, 0, Some(0), Direction::Down, 5);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 2 });
    }

    /// N6 has no right neighbour for the small cells, so Right turns no page
    /// (there is only one) and is blocked; the corner 2x2 steps right.
    #[test]
    fn n6_corner_steps_right_into_the_small_column() {
        let outcome = navigate(GridLayout::N6, 0, Some(0), Direction::Right, 6);
        assert_eq!(outcome, NavigateOutcome::Moved { focus: 1 });
    }
}
