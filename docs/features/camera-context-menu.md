# Camera Context Menu

## Overview

XGView provides a right-click context menu for camera tiles
on the video wall.

The menu offers two actions:

- Edit camera.
- Delete camera.

These actions reuse the existing camera editing and removal
confirmation interfaces.

## Implementation

Primary implementation:

- `crates/monitor_gui/src/app/wall.rs`

Related configuration logic:

- `crates/monitor_core/src/config.rs`

The context menu is attached to interactive camera tiles.

## Camera Index Mapping

The video wall displays cameras from the active group,
excluding disabled cameras.

Consequently, the visible camera index is not necessarily
the same as its position in the configuration file.

The application uses `AppConfig::active_slots()` to map
visible camera indices to configuration slots.

For example, if the configuration contains:

- Slot 0: Camera A, enabled.
- Slot 1: Camera B, disabled.
- Slot 2: Camera C, enabled.

The visible camera indices are:

- Index 0 -> configuration slot 0.
- Index 1 -> configuration slot 2.

Editing or deleting Camera C must therefore operate on
configuration slot 2, not slot 1.

## Editing

Selecting Edit opens the existing camera editor with
the selected camera's configuration.

Changes are handled by the normal camera editing workflow.

## Deletion

Selecting Delete opens the existing removal confirmation
dialog.

The camera is not removed merely by opening the context menu
or selecting the Delete action.

## Automated Tests

Relevant existing configuration test:

- `a_place_on_the_wall_is_a_slot_in_the_file`

This test verifies active-slot mapping for disabled cameras
and different camera groups.

The current test coverage does not directly simulate
right-click interaction with the video wall.

## Manual Verification

1. Add at least three cameras.
2. Disable the second camera.
3. Right-click the third camera on the video wall.
4. Select Edit and verify that the correct camera opens.
5. Close the editor without saving.
6. Right-click the same camera and select Delete.
7. Verify that a confirmation dialog appears.
8. Cancel the deletion and confirm that the camera remains.
9. Repeat the test with another active camera group.
10. Repeat on a subsequent page of a multi-page layout.

## Limitations

The context menu requires an interactive camera tile and
a pointing device that supports secondary-click input.

## Related Commits

- `e10b705` — add right-click edit and delete actions.
- `4369b1d` — allow context menu while settings are open.