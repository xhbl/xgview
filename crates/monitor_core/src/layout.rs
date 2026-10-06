use serde::{Deserialize, Serialize};

/// Supported grid layouts.
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
}

impl Default for GridLayout {
    fn default() -> Self {
        GridLayout::G2x2
    }
}

impl GridLayout {
    /// All layouts, ordered from the densest detail to the densest grid.
    pub const ALL: [GridLayout; 4] = [
        GridLayout::G1x1,
        GridLayout::G2x2,
        GridLayout::G3x3,
        GridLayout::G4x4,
    ];

    pub fn cols(self) -> usize {
        match self {
            GridLayout::G1x1 => 1,
            GridLayout::G2x2 => 2,
            GridLayout::G3x3 => 3,
            GridLayout::G4x4 => 4,
        }
    }

    pub fn rows(self) -> usize {
        self.cols()
    }

    /// Number of visible viewports in one page.
    pub fn capacity(self) -> usize {
        self.cols() * self.rows()
    }

    pub fn label(self) -> &'static str {
        match self {
            GridLayout::G1x1 => "1x1",
            GridLayout::G2x2 => "2x2",
            GridLayout::G3x3 => "3x3",
            GridLayout::G4x4 => "4x4",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        let normalized = label.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|layout| layout.label() == normalized)
    }

    /// Next denser layout, wrapping around at 4x4.
    pub fn next(self) -> Self {
        match self {
            GridLayout::G1x1 => GridLayout::G2x2,
            GridLayout::G2x2 => GridLayout::G3x3,
            GridLayout::G3x3 => GridLayout::G4x4,
            GridLayout::G4x4 => GridLayout::G1x1,
        }
    }

    /// Previous layout, wrapping around at 1x1.
    pub fn prev(self) -> Self {
        match self {
            GridLayout::G1x1 => GridLayout::G4x4,
            GridLayout::G2x2 => GridLayout::G1x1,
            GridLayout::G3x3 => GridLayout::G2x2,
            GridLayout::G4x4 => GridLayout::G3x3,
        }
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

/// Pure navigation math: computes the new focus (global channel index) and page
/// for a DPAD move, turning the page when the focus leaves the current page.
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
    let cols = layout.cols();
    let rows = layout.rows();
    let pages = page_count(total, capacity);
    let page = page.min(pages - 1);

    // Normalise the focus so that it always points inside `page`.
    let focus = match focus {
        Some(focus) if page_of(focus, capacity) == page => focus,
        _ => page * capacity,
    };
    let cell = focus - page * capacity;
    let col = cell % cols;
    let row = cell / cols;

    let visible_len = |page: usize| -> usize {
        let start = page * capacity;
        (start + capacity).min(total).saturating_sub(start)
    };
    let clamp_cell = |page: usize, cell: usize| -> usize {
        let len = visible_len(page);
        if len == 0 {
            0
        } else {
            cell.min(len - 1)
        }
    };

    match dir {
        Direction::Up => {
            if row > 0 {
                NavigateOutcome::Moved { focus: focus - cols }
            } else if page == 0 {
                NavigateOutcome::Blocked
            } else {
                let previous = page - 1;
                let cell = clamp_cell(previous, (rows - 1) * cols + col);
                NavigateOutcome::PageTurned { page: previous, focus: previous * capacity + cell }
            }
        }
        Direction::Down => {
            if row + 1 < rows && cell + cols < visible_len(page) {
                NavigateOutcome::Moved { focus: focus + cols }
            } else if page + 1 >= pages {
                NavigateOutcome::Blocked
            } else {
                let next = page + 1;
                let cell = clamp_cell(next, col);
                NavigateOutcome::PageTurned { page: next, focus: next * capacity + cell }
            }
        }
        Direction::Left => {
            if col > 0 {
                NavigateOutcome::Moved { focus: focus - 1 }
            } else if page == 0 {
                NavigateOutcome::Blocked
            } else {
                let previous = page - 1;
                let cell = clamp_cell(previous, row * cols + (cols - 1));
                NavigateOutcome::PageTurned { page: previous, focus: previous * capacity + cell }
            }
        }
        Direction::Right => {
            if col + 1 < cols && cell + 1 < visible_len(page) {
                NavigateOutcome::Moved { focus: focus + 1 }
            } else if page + 1 >= pages {
                NavigateOutcome::Blocked
            } else {
                let next = page + 1;
                let cell = clamp_cell(next, row * cols);
                NavigateOutcome::PageTurned { page: next, focus: next * capacity + cell }
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
}
