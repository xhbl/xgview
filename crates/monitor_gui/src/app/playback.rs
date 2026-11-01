//! A camera's recordings, playing in the magnified viewport.
//!
//! The camera keeps its slot: its name, its on-screen display, its place on the
//! wall, its settings. Only the picture comes from somewhere else - a channel of
//! its own, one past the last camera, which the scheduler never names and so
//! never opens, parks or reopens.

use super::*;

use std::time::{SystemTime, UNIX_EPOCH};

/// One camera's recordings, playing in the magnified viewport.
pub(super) struct Playback {
    /// The channel whose picture this is - the camera's own slot, which keeps
    /// its name, its display and its place on the wall.
    slot: usize,
    /// The channel the recording plays on. See [`XgViewApp::playback_index`].
    pub(super) index: usize,
}

impl XgViewApp {
    /// The channel a recording plays on: one past the last camera.
    ///
    /// Every camera's channels are `0..count`, and the scheduler only ever
    /// names one of those. The index after the last is therefore a channel no
    /// camera can claim, so a recording needs no reservation anywhere else -
    /// and a camera list that changes length is the one thing that moves it.
    pub(super) fn playback_index(&self) -> usize {
        // The channel count, not the file's: a channel index is a position in
        // the cameras the wall is showing, and a disabled one or another
        // group's would leave the reservation short of the last channel.
        self.config.enabled_count()
    }

    /// The camera the magnified viewport is showing.
    ///
    /// A channel index is a position in the wall's own list, so it is read back
    /// into an entry through the same mapping everything else uses.
    pub(super) fn magnified_camera(&self) -> Option<&CameraSource> {
        let channel = self.scheduler.zoom()?;
        let slot = *self.config.active_slots().get(channel)?;
        self.config.cameras.get(slot)
    }

    /// The channel whose picture a slot is showing.
    ///
    /// The slot itself, except while a recording plays on the magnified one.
    pub(super) fn view_index(&self, slot: usize) -> usize {
        match &self.playback {
            Some(playback) if playback.slot == slot => playback.index,
            _ => slot,
        }
    }

    /// Whether a recording is playing on this slot.
    pub(super) fn is_playing_back(&self, slot: usize) -> bool {
        matches!(&self.playback, Some(playback) if playback.slot == slot)
    }

    /// Plays the last minute of a camera's recordings in the magnified
    /// viewport.
    ///
    /// A minute ending now is the stretch that is certain to exist: the segment
    /// list is not read yet, so this is what a viewer means by "what just
    /// happened" rather than a window built around an event.
    pub(super) fn enter_playback(&mut self) {
        let Some(channel) = self.scheduler.zoom() else {
            return;
        };
        let Some((origin, camera_id, credentials)) = self.magnified_camera().and_then(|camera| {
            let origin = camera.frigate.clone()?;
            let credentials = camera
                .mse_username
                .clone()
                .map(|user| (user, camera.mse_password.clone().unwrap_or_default()));
            Some((origin, camera.id.clone(), credentials))
        }) else {
            return;
        };
        let index = self.playback_index();
        let to = epoch_now();
        let from = to - 60;
        let uri = format!(
            "{}/api/{}/start/{from}/end/{to}/clip.mp4",
            origin.base_url.trim_end_matches('/'),
            origin.camera
        );
        tracing::debug!(target: "xgview::app", channel, index, %uri, "playing a recording");
        self.manager.play_clip(index, uri, camera_id, credentials);
        self.playback = Some(Playback { slot: channel, index });
    }

    /// Puts the recording away and goes back to the camera's own stream.
    ///
    /// The channel is stopped rather than left running: a recording that plays
    /// on unseen is a request to the server and a decoder held for nothing.
    pub(super) fn leave_playback(&mut self) {
        let Some(playback) = self.playback.take() else {
            return;
        };
        self.manager.stop_clip(playback.index);
        self.channels.remove(&playback.index);
        self.textures.remove(&playback.index);
        self.video.release(playback.index);
        tracing::debug!(
            target: "xgview::app",
            index = playback.index,
            "the recording is put away"
        );
    }
}

/// Seconds since the epoch.
fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}
