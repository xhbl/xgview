# Desktop camera workspace

On desktop, the wall toolbar includes three controls after the page arrows:

- **New window (+):** opens another native window, initially centered over the main window. Move it to another display as desired.
- **Pin:** enabled by default. Drag the wall to turn pages. Disable the pin to drag a camera tile onto another occupied tile and swap their positions. Empty cells are not drop targets. Escape cancels a drag.
- **Save (disk):** enables local workspace persistence. Disable it and confirm deletion to forget the saved workspace without closing the current windows.

Each window selects a page independently. Layouts belong to pages, not windows. Changing a page's capacity redistributes cameras in sequence across all later pages; empty cells remain at the end. Multiple windows can show the same page. Additional windows have layout, paging and fullscreen controls (including F11). Camera context menus reuse the main wall's menu; some editing dialogs are displayed in the main window.

The local sidecar (for example `xgvconfig.workspace.json` for `xgvconfig.json`) beside the application configuration stores page layouts, camera ordering, additional window geometry, page selection and fullscreen state. It is not part of the portable configuration export. Main window startup geometry retains its existing behavior. Each configuration has its own sidecar. Switching groups closes secondary windows and resets the workspace to the incoming group’s original view. Changing the camera count resets camera order to the configured sequence, while retaining page layouts. The experimental shared `workspace.json` from earlier development builds is not loaded automatically. Ordinary camera settings remain in the application configuration.

On Windows, a native cursor helper supports dragging between windows and checking monitor availability. An unavailable display is intended to cause a secondary window to return near the main window, reduced in size and out of fullscreen. Physical multi-monitor arrangements, hot unplug, mixed DPI and non-Windows platforms have not been tested. On other desktop platforms, cross-window dragging uses egui events and is unverified.

## Validation checklist

Run `cargo test --release --locked -p monitor_core -p monitor_gui` in a configured build environment. Manually check page resizing in both directions, camera order and count, shared pages, closing windows, swipe animation and cancellation, occupied-cell swaps within and between windows, rejected empty/outside drops, fullscreen, saving/restoring, deletion confirmation and configuration export. Test group switching and camera additions/removals separately. Multi-monitor hot unplug, mixed DPI, Linux, macOS and Android regression checks remain outstanding.
