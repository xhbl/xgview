# Bulk RTSP Camera Import

## Overview

Bulk camera import allows multiple RTSP camera entries to be
created from a common address template.

It is intended for multi-channel recorders and other devices
whose channels use predictable RTSP URL patterns.

## Features

- Generate multiple camera entries from one RTSP template.
- Configure the first channel number.
- Configure the channel numbering step.
- Add cameras to the selected camera group.
- Reuse an RTSP address in different camera groups.
- Validate bulk import parameters before creating entries.

## Implementation

Primary implementation:

- `crates/monitor_gui/src/dialogs.rs`

Related localization:

- `crates/monitor_i18n/langs/en.ftl`
- `langs/ru-RU.ftl`
- `langs/zh-CN.ftl`

## Channel Numbering

The import operation generates channel numbers using the
configured first channel and step.

For example, importing four channels with:

- First channel: 1
- Step: 2

produces channel numbers:

1, 3, 5, 7

The generated RTSP URLs follow the supplied channel template.

## Camera Groups

Camera groups are independent collections.

The same RTSP camera may be present in more than one group.
Importing a camera into another group must not overwrite its
existing entry in the original group.

## Validation

Invalid bulk import parameters must be rejected before
camera entries are created.

The operation must preserve existing camera configuration
when validation fails.

## Automated Tests

Relevant tests include:

- `bulk_cameras_allow_reuse_across_groups`
- `bulk_cameras_expand_channel_templates`
- `bulk_cameras_respect_first_channel_and_step`
- `bulk_cameras_reject_invalid_parameters`

## Manual Verification

1. Open the camera management interface.
2. Start the bulk camera import operation.
3. Enter an RTSP address template.
4. Configure the first channel and numbering step.
5. Generate multiple camera entries.
6. Verify their names and RTSP addresses.
7. Repeat the operation in another camera group.
8. Confirm that entries in the original group remain unchanged.
9. Enter invalid numeric parameters.
10. Confirm that invalid input does not create camera entries.

## Limitations

Bulk import creates camera configuration entries. It does not
guarantee that the RTSP endpoints are reachable or that their
credentials are valid.

Users should verify connectivity separately.

## Related Commits

- `2ce8999` — add bulk RTSP channel import.
- `aa7e265` — allow imports across camera groups.
- `8b633a7` — localize the import interface.
- `390bb19` — preserve Backspace in numeric input fields.