//! Whether each Frigate camera is recording at this moment.
//!
//! A red square beside the live dot says the camera is being recorded. Frigate
//! writes its recordings in segments of about ten seconds, and a segment only
//! appears once the recorder has finished it, so the square is a short-lag
//! answer to "is it recording *now*" rather than the exact instant. A camera
//! with no Frigate origin has no answer at all and draws no square; one whose
//! query failed keeps its last answer, or shows the grey square if it never
//! had one.
//!
//! No state here is persisted: the next launch asks again. The polling task is
//! one shot per tick, like the playback reads in [`super::playback`], so there
//! is no long-lived loop to stop when a camera is added or removed.

use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::Sender;
use tokio::runtime::Handle;

use monitor_core::config::FrigateConfig;
use monitor_core::discovery::frigate::{FrigateClient, Segment};
use monitor_core::model::FrigateOrigin;

use crate::dialogs::BackgroundEvent;

/// How far back the segment list is read, in seconds.
///
/// A little wider than the active window below so that a segment whose start
/// is just outside the window is still returned and can be judged by its end.
pub(super) const LOOKBACK: i64 = 25;

/// A segment ending within this many seconds of now counts as recording.
///
/// The recorder writes about every ten seconds and a segment is visible only
/// once written, so a live camera's newest segment ends a few seconds ago.
/// Fifteen seconds covers that lag without holding the red square for long
/// after a motion-only camera stops.
pub(super) const ACTIVE: i64 = 15;

/// Whether the segment list says the camera is recording at `now`.
///
/// True when any returned segment reaches into the active window. A segment
/// that ended before `now - ACTIVE` is an old recording, not a live one. The
/// window the list was read with is [`LOOKBACK`], wider than `ACTIVE`, so a
/// segment that started earlier but is still being written is included.
pub(super) fn is_recording(segments: &[Segment], now: i64) -> bool {
    segments.iter().any(|segment| segment.end >= now - ACTIVE)
}

/// The wall clock, in whole seconds since the epoch.
pub(super) fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Ask one Frigate camera for its newest recording segment and send the answer.
///
/// `active` is `None` when the box could not be asked, which tells the receiver
/// to keep whatever it last knew. The camera id, not its label, keys the answer
/// so a camera in two slots is only asked twice and both agree.
pub(super) fn poll_one(
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    camera_id: String,
    origin: FrigateOrigin,
    credentials: Option<(String, String)>,
    now: i64,
) {
    let sender = events.clone();
    handle.spawn(async move {
        let (user, pass) = credentials.unwrap_or_default();
        let config = FrigateConfig::at(&origin.base_url, &user, &pass);
        let segments = match FrigateClient::new(config) {
            Ok(mut client) => match client.segments(&origin.camera, now - LOOKBACK, now).await {
                Ok(segments) => Some(segments),
                Err(err) => {
                    tracing::debug!(
                        target: "xgview::app",
                        camera = %origin.camera,
                        %err,
                        "the recording-state read failed"
                    );
                    None
                }
            },
            Err(err) => {
                tracing::debug!(
                    target: "xgview::app",
                    camera = %origin.camera,
                    %err,
                    "the recording-state client failed"
                );
                None
            }
        };
        let active = segments.map(|segments| is_recording(&segments, now));
        let _ = sender.send(BackgroundEvent::RecordingStatus { camera_id, active });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: i64, end: i64) -> Segment {
        Segment { start, end }
    }

    #[test]
    fn a_recent_segment_means_recording() {
        let now = 1000;
        assert!(is_recording(&[segment(985, 996)], now), "ended 4s ago");
        assert!(is_recording(&[segment(970, 985)], now), "ended 15s ago, edge");
        assert!(is_recording(&[segment(960, 999)], now), "still being written");
    }

    #[test]
    fn an_old_or_empty_list_is_not_recording() {
        let now = 1000;
        assert!(!is_recording(&[], now), "nothing recorded in the window");
        assert!(!is_recording(&[segment(950, 984)], now), "ended 16s ago");
        assert!(!is_recording(&[segment(900, 910)], now), "a finished recording");
    }

    #[test]
    fn any_recent_segment_counts_not_only_the_newest() {
        let now = 1000;
        let segments = [segment(900, 910), segment(970, 995)];
        assert!(is_recording(&segments, now));
    }
}
