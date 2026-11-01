//! A camera's recordings, playing in the magnified viewport.
//!
//! The camera keeps its slot: its name, its on-screen display, its place on the
//! wall, its settings. Only the picture comes from somewhere else - a channel of
//! its own, one past the last camera, which the scheduler never names and so
//! never opens, parks or reopens.

use super::*;

use std::time::{SystemTime, UNIX_EPOCH};

use monitor_core::config::FrigateConfig;
use monitor_core::discovery::frigate::{FrigateClient, window_at};
use monitor_core::model::FrigateOrigin;

/// One camera's recordings, playing in the magnified viewport.
pub(super) struct Playback {
    /// The channel whose picture this is - the camera's own slot, which keeps
    /// its name, its display and its place on the wall.
    slot: usize,
    /// The channel the recording plays on. See [`XgViewApp::playback_index`].
    pub(super) index: usize,
    /// The camera it belongs to. The wall can change the camera behind a slot
    /// without changing how many there are - another group - so the slot alone
    /// does not say whose recording this is.
    pub(super) camera: String,
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

    /// The channel a slot's picture is drawn from.
    ///
    /// The playback channel's once it exists - and the camera's own until then.
    /// A recording's first frame is a request away, and a viewport that went
    /// blank for the length of it would look like the picture had failed rather
    /// than like something was being fetched.
    pub(super) fn view_channel(&self, slot: usize) -> Option<&ChannelUi> {
        let view = self.view_index(slot);
        self.channels.get(&view).or_else(|| self.channels.get(&slot))
    }

    /// The texture a slot's picture is drawn from. See [`Self::view_channel`].
    pub(super) fn view_texture(&self, slot: usize) -> Option<&ChannelTexture> {
        let view = self.view_index(slot);
        self.textures.get(&view).or_else(|| self.textures.get(&slot))
    }

    /// Whether the recording on this slot still belongs to the camera behind
    /// it. A wall that changed length moves the reservation; one that changed
    /// group leaves it where it is and puts a different camera in front of it.
    pub(super) fn playback_is_stale(&self, index: usize) -> bool {
        let Some(playback) = &self.playback else {
            return false;
        };
        if playback.index != index {
            return true;
        }
        let cameras = self.config.active_cameras();
        cameras.get(playback.slot).map(|camera| camera.id.as_str()) != Some(playback.camera.as_str())
    }

    /// Whether a recording is playing on this slot.
    pub(super) fn is_playing_back(&self, slot: usize) -> bool {
        matches!(&self.playback, Some(playback) if playback.slot == slot)
    }

    /// Asks a camera's server what there is to play, and plays it.
    ///
    /// The window is read from the segment list rather than guessed: a window
    /// is only playable where a segment covers it, and it has to begin where a
    /// segment begins - a clip cut mid-segment opens on whatever frame is there,
    /// and the moment each picture is of is counted from the window's start. So
    /// this is a request, and what it answers arrives as an event.
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
        start_window(
            &self.handle,
            &self.events_tx,
            channel,
            camera_id,
            origin,
            credentials,
            epoch_now(),
        );
    }

    /// Opens a playback channel on the stretch that was read.
    ///
    /// Separate from [`Self::enter_playback`] because it is what the answer to
    /// the question arrives at, a request later.
    pub(super) fn start_playback(
        &mut self,
        channel: usize,
        camera_id: String,
        origin: FrigateOrigin,
        credentials: Option<(String, String)>,
        window: (i64, i64),
    ) {
        let index = self.playback_index();
        let (from, to) = window;
        let uri = format!(
            "{}/api/{}/start/{from}/end/{to}/clip.mp4",
            origin.base_url.trim_end_matches('/'),
            origin.camera
        );
        tracing::debug!(target: "xgview::app", channel, index, %uri, "playing a recording");
        self.manager.play_clip(index, uri, camera_id.clone(), credentials, window);
        self.playback = Some(Playback { slot: channel, index, camera: camera_id });
        // The speaker belongs to the channel whose picture is on screen, and
        // that has just become the recording's.
        self.apply_audio();
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
        // And back to the camera's own sound, which never stopped playing.
        self.apply_audio();
        tracing::debug!(
            target: "xgview::app",
            index = playback.index,
            "the recording is put away"
        );
    }
}

/// How far back the segment list is read when a playback channel is opened.
///
/// The window itself is a minute; the list is read over an hour so that a
/// camera whose recording is not continuous still has a segment to align to.
const LOOKBACK: i64 = 3600;

/// How much of a recording a channel is given at once.
const WINDOW: i64 = 60;

/// Reads what a camera has recorded and sends back the stretch to play.
///
/// The address is the camera's own rather than the import's configuration: the
/// server may have been edited since, and the camera carries where it answers.
fn start_window(
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    channel: usize,
    camera_id: String,
    origin: FrigateOrigin,
    credentials: Option<(String, String)>,
    now: i64,
) {
    let sender = events.clone();
    handle.spawn(async move {
        let (user, pass) = credentials.clone().unwrap_or_default();
        let config = FrigateConfig::at(&origin.base_url, &user, &pass);
        let window = match FrigateClient::new(config) {
            Ok(mut client) => client
                .segments(&origin.camera, now - LOOKBACK, now)
                .await
                .ok()
                .and_then(|segments| window_at(&segments, now - WINDOW, WINDOW)),
            Err(_) => None,
        };
        let _ = sender.send(BackgroundEvent::PlaybackReady {
            channel,
            camera_id,
            origin,
            credentials,
            window,
        });
    });
}

/// Seconds since the epoch.
fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}
