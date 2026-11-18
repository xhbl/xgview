# Incoming Video Bitrate Display

## Overview

XGView displays incoming video traffic statistics in the
About interface.

The feature provides:

- Current incoming video bitrate.
- Average incoming video bitrate over a 60-second window.

These values help operators observe the network traffic
associated with video reception.

## Implementation

Primary implementation:

- `crates/monitor_gui/src/app/mod.rs`

The application collects incoming traffic measurements and
maintains a rolling history for calculating the average.

## Current Bitrate

The current bitrate represents the most recent incoming
video traffic measurement.

It is an observed traffic rate, not the configured bitrate
of an RTSP camera.

## Sixty-Second Average

The application calculates an average using recent traffic
samples within a 60-second window.

Old samples are excluded as the measurement window advances.

The history is reset after a sufficiently long measurement
gap to avoid presenting stale data as current activity.

## Missing Measurements

A bitrate that has not yet been measured must not be
presented as a valid numeric measurement.

The interface may display a placeholder until sufficient
measurement information becomes available.

## Automated Tests

Relevant tests include:

- `network_traffic_averages_samples`
- `network_traffic_keeps_sixty_second_window`
- `network_traffic_resets_after_long_gap`
- `network_traffic_ignores_early_and_invalid_samples`
- `a_rate_nobody_has_measured_is_a_dash`

These tests cover sample averaging, history expiration,
measurement gaps, invalid samples, and unavailable values.

## Manual Verification

1. Launch XGView without an active video stream.
2. Open the About interface.
3. Inspect the traffic statistics.
4. Start receiving video from an RTSP camera.
5. Verify that the current bitrate responds to traffic.
6. Observe the average over approximately 60 seconds.
7. Stop receiving video.
8. Verify that the displayed values respond appropriately.
9. Resume reception after a long pause.
10. Confirm that stale samples do not distort the average.

## Limitations

The displayed bitrate is an application traffic statistic.
It is not a network-interface throughput measurement.

The 60-second average depends on the availability and
timing of incoming traffic samples.

## Related Commits

- `467908d` — show incoming video bitrate and 60-second average.