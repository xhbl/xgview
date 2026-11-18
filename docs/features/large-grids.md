# Large Camera Grids (5x5 and 6x6)

## Overview

XGView supports two additional uniform camera layouts:

- 5x5: up to 25 cameras per page.
- 6x6: up to 36 cameras per page.

These layouts extend the existing grid system without changing
the behavior of previously supported layouts.

## Implementation

The layouts are defined in `monitor_core` and integrated into
the desktop GUI layout picker.

Primary implementation files:

- `crates/monitor_core/src/layout.rs`
- `crates/monitor_gui/src/app/mod.rs`

## Pagination

Camera indices remain global across pages.

For example, in a 5x5 layout:

- Page 1 contains camera indices 0 through 24.
- Page 2 starts with camera index 25.

An incomplete final page may contain empty cells.

## Navigation

The layouts participate in the existing grid navigation and
pagination mechanisms.

Navigation must preserve the current page and camera indices
according to the shared layout model.

## Automated Tests

Relevant tests include:

- `capacities_match_layouts`
- `every_layout_tiles_its_grid_exactly`
- `computes_pages`
- `page_bounds_are_clamped`
- `the_layout_picker_walks_its_three_rows`
- `cells_of_every_layout_paint_inside_the_wall_without_overlap`
- `a_square_layout_is_still_the_uniform_grid`

These tests cover layout capacities, geometry, pagination,
picker navigation, and tile placement.

## Manual Verification

1. Open the layout picker.
2. Select the 5x5 layout.
3. Confirm that the wall displays 25 cells.
4. Select the 6x6 layout.
5. Confirm that the wall displays 36 cells.
6. Add more cameras than one page can display.
7. Navigate between pages.
8. Verify that camera ordering remains consistent.
9. Return to a previously supported layout.

## Limitations

Displaying 25 or 36 simultaneous video streams may increase
CPU, GPU, memory, and network utilization.

Actual performance depends on the hardware, stream resolution,
codec, bitrate, and decoding configuration.

## Related Commits

- `3a807fd` — add 5x5 and 6x6 camera grids.
- `9a5b685` — update layout picker navigation tests.
- `33edf4e` — correct layout hints and test capacities.