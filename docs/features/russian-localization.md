# Russian Localization

## Overview

XGView includes a Russian language catalog (`ru-RU`) for
the desktop user interface.

The localization also covers bulk camera import controls
and selected camera connection diagnostics.

## Implementation

Localization uses Fluent (`.ftl`) message catalogs.

Relevant files:

- `langs/ru-RU.ftl`
- `langs/zh-CN.ftl`
- `crates/monitor_i18n/langs/en.ftl`
- `crates/monitor_i18n/src/lib.rs`
- `crates/monitor_gui/src/grid.rs`
- `crates/monitor_gui/src/dialogs.rs`

The Russian catalog provides translated interface strings.
The English catalog defines the corresponding source keys.

## Camera Diagnostics

Camera connection diagnostics originate from the monitoring
core and may contain English status descriptions.

The GUI uses `localized_detail()` to translate recognized
diagnostic patterns for the currently selected language.

Examples of supported messages include:

- Connection attempts.
- Network connection errors.
- Connection timeouts.
- Retry delays.
- Stream reception status.
- Stalled streams.
- Exhausted connection attempts.

Dynamic information such as camera addresses, attempt
numbers, and durations is preserved.

Unrecognized diagnostic messages remain available in
their original form rather than being discarded.

## Language Switching

Translations are resolved using the active application
language.

Diagnostic messages are formatted for display by the GUI.

## Automated Tests

Relevant tests in `crates/monitor_gui/src/grid.rs`:

- `localized_detail_uses_active_language`
- `localized_detail_translates_connection_errors`
- `localized_detail_preserves_unknown_errors`
- `localized_detail_preserves_unknown_error_before_retry`
- `localized_detail_keeps_attempt_number`
- `localized_detail_keeps_failure_before_attempt_limit`

These tests cover Russian and Chinese translations,
language selection, dynamic values, and fallback behavior.

## Manual Verification

1. Select Russian in the application language settings.
2. Check the main interface and camera management screens.
3. Open the bulk camera import dialog.
4. Verify that its controls and messages are translated.
5. Connect to an unreachable RTSP endpoint.
6. Verify that recognized connection diagnostics appear
   in Russian.
7. Confirm that IP addresses and attempt numbers remain
   unchanged.
8. Switch to English and verify the corresponding messages.

## Limitations

The diagnostic translation layer recognizes known message
patterns. Unexpected or newly introduced diagnostics may
still appear in English until their patterns are supported.

## Related Commits

- `2dd765a` — add Russian translation.
- `8b633a7` — localize bulk camera import.
- `25f9396` — localize camera diagnostic messages.