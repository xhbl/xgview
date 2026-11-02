//! A camera's recordings, playing in the magnified viewport.
//!
//! The camera keeps its slot: its name, its on-screen display, its place on the
//! wall, its settings. Only the picture comes from somewhere else - a channel of
//! its own, one past the last camera, which the scheduler never names and so
//! never opens, parks or reopens.

use super::*;

use std::time::{SystemTime, UNIX_EPOCH};

use monitor_core::config::FrigateConfig;
use monitor_core::discovery::frigate::{FrigateClient, Segment, stretches, window_at};
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
    /// The stretch the timeline is drawn across - the last few minutes that
    /// were read, which is not the window.
    ///
    /// It has to be. Playback begins at the window's first second, so a timeline
    /// drawn across the window alone would put the cursor at its left edge after
    /// every seek: the picture would have moved and the cursor would look as
    /// though it had jumped back to where it started.
    pub(super) span: (i64, i64),
    /// What the camera recorded around it. Read once when playback opens, and
    /// kept: the timeline draws the stretches that exist, and asking again for
    /// every repaint would be a request a frame.
    pub(super) segments: Vec<Segment>,
    /// The address the window is a suffix of - `…/api/<camera>` - kept so a
    /// scrub can ask for another stretch without the camera being looked up
    /// again, or having moved on the wall in between.
    pub(super) uri_base: String,
    /// The login that address takes. Frigate's own, not the restream's.
    pub(super) credentials: Option<(String, String)>,
    /// The moment under the viewer's finger, while there is one. The cursor
    /// follows it and the picture stays where it was, because nothing has been
    /// asked for yet.
    pub(super) scrub: Option<i64>,
    /// The moment a seek asked for, until the picture arrives there.
    ///
    /// A seek opens a new window, and what is on screen until its first frame
    /// arrives is still the window being left - whose moment is the one the
    /// drag began from. Drawing the cursor from that frame would put it back
    /// where the drag started, and it would only reach the place the viewer
    /// pointed at a request later.
    pub(super) pending: Option<i64>,
    /// Whether a finger is on the timeline, and where it has got to.
    ///
    /// Kept here rather than read from egui's drag state: what the viewer is
    /// doing is pressing on a strip, and that is a fact about the pointer and
    /// the row - not about whether a widget was the one egui decided to hand the
    /// gesture to.
    pub(super) scrubbing: bool,
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
        segments: Vec<Segment>,
    ) {
        let index = self.playback_index();
        let (from, to) = window;
        let uri_base = format!(
            "{}/api/{}",
            origin.base_url.trim_end_matches('/'),
            origin.camera
        );
        let uri = format!("{uri_base}/start/{from}/end/{to}/clip.mp4");
        tracing::debug!(target: "xgview::app", channel, index, %uri, "playing a recording");
        self.manager.play_clip(index, uri, camera_id.clone(), credentials.clone(), window);
        let span = extent(&segments).unwrap_or((from, to));
        self.playback = Some(Playback {
            slot: channel,
            index,
            camera: camera_id,
            span,
            segments,
            uri_base,
            credentials,
            scrub: None,
            // The window's first frame is the moment the cursor starts from,
            // and it is a request away; until then the slot still shows the
            // camera's own picture, which has no moment at all.
            pending: Some(from),
            scrubbing: false,
        });
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

impl XgViewApp {
    /// Draws what the recording holds across the timeline's row.
    ///
    /// The row is in the picture's own frame, so it is read as one axis: a
    /// moment is its distance along the window, and a stretch is the band
    /// between two of them. Which of the row's sides that axis runs along is
    /// the row's own shape - across a picture as it arrives, down one drawn
    /// turned.
    pub(super) fn draw_timeline(&self, ui: &egui::Ui, row: Rect) {
        let Some(playback) = &self.playback else {
            return;
        };
        let (start, end) = playback.span;
        let span = (end - start).max(1) as f32;
        let horizontal = row.width() > row.height();
        let middle = row.center();
        let along = |at: i64| {
            let t = ((at - start) as f32 / span).clamp(0.0, 1.0);
            if horizontal {
                row.left() + row.width() * t
            } else {
                row.top() + row.height() * t
            }
        };
        let band = |from: f32, to: f32, thick: f32| {
            let to = to.max(from + 1.0);
            if horizontal {
                Rect::from_min_max(
                    egui::pos2(from, middle.y - thick / 2.0),
                    egui::pos2(to, middle.y + thick / 2.0),
                )
            } else {
                Rect::from_min_max(
                    egui::pos2(middle.x - thick / 2.0, from),
                    egui::pos2(middle.x + thick / 2.0, to),
                )
            }
        };

        let painter = ui.painter();
        // The stretches that were recorded, in the accent the wall uses for a
        // picture that is arriving. Merged first: the recorder's own file
        // boundaries are fractions of a second apart, and drawing them would put
        // a notch every ten seconds along a recording that is continuous.
        for segment in &stretches(&playback.segments) {
            if segment.end <= start || segment.start >= end {
                continue;
            }
            let from = along(segment.start.max(start));
            let to = along(segment.end.min(end));
            painter.rect_filled(band(from, to, 6.0), CornerRadius::same(2), theme::ACCENT.gamma_multiply(0.55));
        }
        // Where the picture has got to. Its moment comes from the frame itself,
        // so a stalled stream leaves the cursor where the picture is rather
        // than where a clock thinks it should be - and a seek that is on its
        // way leaves it where the finger was. See [`Playback::pending`].
        let position = cursor(
            playback.scrub,
            playback.pending,
            self.view_texture(playback.slot).and_then(|entry| entry.at),
        );
        if let Some(at) = position {
            let x = along(at.round() as i64);
            painter.rect_filled(band(x - 1.5, x + 1.5, 18.0), CornerRadius::same(1), theme::FOCUS);
        }
    }
}

impl XgViewApp {
    /// Answers a finger on the timeline.
    ///
    /// Pressing holds the picture where it is - a recording is paced by its own
    /// timestamps, so that is the only way to stop it - and the cursor follows
    /// the finger with nothing asked of the server. Letting go asks for the
    /// stretch under the finger and plays it from there.
    pub(super) fn timeline_input(&mut self, ui: &egui::Ui, row: Rect) {
        let Some(playback) = &self.playback else {
            return;
        };
        let (index, start, end) = (playback.index, playback.span.0, playback.span.1);
        let span = (end - start).max(1) as f32;
        let horizontal = row.width() > row.height();
        let moment = |pos: egui::Pos2| -> i64 {
            let along = if horizontal {
                (pos.x - row.left()) / row.width()
            } else {
                (pos.y - row.top()) / row.height()
            };
            start + (along.clamp(0.0, 1.0) * span) as i64
        };

        // Claimed so the wall behind does not take the press for a page swipe.
        // What the viewer is doing is read from the pointer rather than from
        // this answer: whether egui hands the gesture to this widget or to the
        // picture under it is not something a timeline should depend on.
        let _ = ui.interact(row, Id::new(("xgview-timeline", index)), Sense::click_and_drag());
        let (down, pos) = ui.ctx().input(|input| (input.pointer.any_down(), input.pointer.interact_pos()));
        let scrubbing = self.playback.as_ref().is_some_and(|playback| playback.scrubbing);

        if !scrubbing {
            // A press that starts on the strip holds the picture where it is and
            // puts the cursor under the finger. One that starts elsewhere is the
            // picture's, and does nothing here.
            if let (true, Some(pos)) = (down, pos) {
                if row.contains(pos) {
                    self.manager.hold_clip(index, true);
                    if let Some(playback) = self.playback.as_mut() {
                        playback.scrubbing = true;
                        playback.scrub = Some(moment(pos));
                    }
                }
            }
            return;
        }

        match (down, pos) {
            // The finger may leave the strip; the cursor follows it to the end
            // rather than stopping where the strip does.
            (true, Some(pos)) => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.scrub = Some(moment(pos));
                }
            }
            _ => self.end_scrub(),
        }
    }

    /// Lets the picture go and plays from wherever the finger was left.
    fn end_scrub(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        playback.scrubbing = false;
        let (at, index) = (playback.scrub.take(), playback.index);
        match at {
            // The hold is not let go of first: a session resumed for a moment
            // publishes a frame of the window that is being left, and that
            // frame is the one that would put the cursor back. The window the
            // seek asks for replaces the session, so the hold ends with it.
            Some(at) => self.seek(at),
            // Nothing under the finger: the recording goes on where it was
            // held.
            None => self.manager.hold_clip(index, false),
        }
    }

    /// Plays from the nearest moment that has a recording.
    ///
    /// A scrub that lands in a hole is taken to the edge of the nearest stretch
    /// rather than refused: the viewer is pointing at a place on a timeline, and
    /// the answer "there is nothing there, here is the closest there is" is one
    /// they can act on.
    fn seek(&mut self, at: i64) {
        let Some(playback) = &self.playback else {
            return;
        };
        let index = playback.index;
        // Neither of these can happen while the list is the one the window was
        // built from - and if it does, saying so is better than a picture that
        // silently carries on where it was, which reads as a drag that was
        // refused.
        let Some(window) = nearest(&stretches(&playback.segments), at)
            .and_then(|target| window_at(&playback.segments, target, WINDOW))
        else {
            self.manager.hold_clip(index, false);
            self.flash(monitor_i18n::tr("toast-no-recording"), ToastKind::Error);
            return;
        };
        let (camera, uri_base) = (playback.camera.clone(), playback.uri_base.clone());
        let credentials = playback.credentials.clone();
        let (from, to) = window;
        let uri = format!("{uri_base}/start/{from}/end/{to}/clip.mp4");
        tracing::debug!(target: "xgview::app", index, %uri, "playing from a scrub");
        // A new channel on the same index: the old one is replaced, which is
        // what "play from here" means. It starts unpaused, so the hold the
        // press began ends with it.
        self.manager.play_clip(index, uri, camera, credentials, window);
        // The cursor stays where the finger left it until the picture gets
        // there. The moment asked for is held inside the window that will
        // play, so a scrub past the end of a stretch - taken to that stretch's
        // edge, and played from the start of the next one - does not wait for a
        // moment the new window never reaches.
        if let Some(playback) = self.playback.as_mut() {
            playback.scrub = None;
            playback.scrubbing = false;
            playback.pending = Some(at.clamp(from, to));
        }
    }

    /// Lets the cursor follow the picture again, once the picture has reached
    /// the moment a seek asked for.
    ///
    /// Called with every frame of every channel, because a frame is the only
    /// thing that says where the picture has got to. A frame that is not the
    /// recording's says nothing about it.
    pub(super) fn picture_arrived(&mut self, index: usize, at: Option<f64>) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        if playback.index == index && playback.pending.is_some_and(|pending| caught_up(at, pending)) {
            playback.pending = None;
        }
    }
}

/// How much of a recording the timeline shows at once.
///
/// Ten minutes. An hour across a strip this wide is three seconds to a pixel,
/// which is too coarse to aim a scrub with; the levels that let it be changed
/// are the next thing the timeline learns.
const SPAN: i64 = 600;

/// What the timeline is drawn across: the last few minutes that were read.
fn extent(segments: &[Segment]) -> Option<(i64, i64)> {
    let first = segments.first()?;
    let last = segments.last()?;
    if last.end <= first.start {
        return None;
    }
    Some((last.end.saturating_sub(SPAN).max(first.start), last.end))
}

/// The moment closest to `at` that a stretch covers.
fn nearest(stretches: &[Segment], at: i64) -> Option<i64> {
    stretches
        .iter()
        .map(|stretch| at.clamp(stretch.start, stretch.end))
        .min_by_key(|moment| (*moment - at).abs())
}

/// What the cursor shows: the moment under the finger while there is one, then
/// the moment asked for, and only then the picture's own.
///
/// The order is what keeps a seek from looking like a refusal. A press holds the
/// picture where it is, so the frame on screen still says the moment the drag
/// began from until the new window's first frame arrives - reading it back would
/// put the cursor at the place the drag started, and only a request later at the
/// place the viewer pointed at.
fn cursor(scrub: Option<i64>, pending: Option<i64>, picture: Option<f64>) -> Option<f64> {
    scrub.or(pending).map(|at| at as f64).or(picture)
}

/// Whether the picture has reached the moment a seek asked for.
///
/// A seek's window begins where the segment it was cut from begins - up to a
/// segment before the moment asked for, because that is where the keyframe is -
/// so its first frames are behind that moment. The cursor waits for them to get
/// there rather than stepping back the moment they arrive.
fn caught_up(picture: Option<f64>, pending: i64) -> bool {
    picture.is_some_and(|at| at >= pending as f64)
}

/// How far back the segment list is read when a playback channel is opened.
///
/// The window itself is a minute; the list is read over an hour so that a
/// camera whose recording is not continuous still has a segment to align to.
const LOOKBACK: i64 = 3600;

/// How much of a recording a channel is given at once.
///
/// Ten minutes, so the seam that a new window makes - a request, a decoder
/// reopening - comes once in ten minutes rather than once a minute. What is
/// asked for and where playback begins are two different questions: the window
/// runs from the moment asked for, and only what was recorded fills it.
const WINDOW: i64 = 600;

/// How far back opening a recording begins, when nothing else says.
///
/// A minute, which is what a viewer means by "what just happened". The window
/// asked for is longer, but a recording ends at the present, so a minute back is
/// all there is to play.
const ENTRY_BACK: i64 = 60;

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
        let segments = match FrigateClient::new(config) {
            Ok(mut client) => client
                .segments(&origin.camera, now - LOOKBACK, now)
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let window = window_at(&segments, now - ENTRY_BACK, WINDOW);
        let _ = sender.send(BackgroundEvent::PlaybackReady {
            channel,
            camera_id,
            origin,
            credentials,
            window,
            segments,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The timeline is drawn across everything that was read, which is wider
    /// than the window: drawn across the window alone it would put the cursor at
    /// its left edge after every seek, and the picture would have moved while
    /// the cursor looked as though it had gone back.
    #[test]
    fn the_timeline_spans_what_was_read() {
        let segments = [Segment { start: 100, end: 110 }, Segment { start: 110, end: 120 }];
        assert_eq!(extent(&segments), Some((100, 120)));
        assert_eq!(extent(&[]), None);
        // A list that ends before it starts has no span to draw.
        assert_eq!(extent(&[Segment { start: 120, end: 100 }]), None);
        // An hour of recording is shown ten minutes at a time, from its end.
        assert_eq!(extent(&[Segment { start: 0, end: 3600 }]), Some((3000, 3600)));
        // And a recording shorter than that is shown whole.
        assert_eq!(extent(&[Segment { start: 3500, end: 3600 }]), Some((3500, 3600)));
    }

    /// A scrub that lands between two stretches goes to the nearer edge rather
    /// than being refused.
    #[test]
    fn a_scrub_between_stretches_goes_to_the_nearest_edge() {
        let stretches = [Segment { start: 100, end: 110 }, Segment { start: 200, end: 210 }];
        assert_eq!(nearest(&stretches, 105), Some(105), "inside a stretch is itself");
        assert_eq!(nearest(&stretches, 150), Some(110), "nearer the one before");
        assert_eq!(nearest(&stretches, 190), Some(200), "nearer the one after");
        assert_eq!(nearest(&stretches, 90), Some(100), "before all of them");
        assert_eq!(nearest(&stretches, 900), Some(210), "after all of them");
        assert_eq!(nearest(&[], 100), None);
    }

    /// A seek's cursor stays where the finger left it until the picture gets
    /// there. Reading the moment off the frame on screen instead would put it
    /// back at the place the drag started from.
    #[test]
    fn the_cursor_waits_for_the_picture_to_catch_up() {
        // A finger on the timeline is the whole of the answer.
        assert_eq!(cursor(Some(120), Some(110), Some(100.0)), Some(120.0));
        // Once it is lifted, the moment asked for - not the frame's, which is
        // still the window being left.
        assert_eq!(cursor(None, Some(110), Some(100.0)), Some(110.0));
        assert_eq!(cursor(None, Some(110), None), Some(110.0));
        // And with the seek over, the picture's own.
        assert_eq!(cursor(None, None, Some(100.0)), Some(100.0));
        assert_eq!(cursor(None, None, None), None);

        // The window's first frames are behind the moment asked for, and are
        // not what ends the wait.
        assert!(!caught_up(Some(100.0), 110));
        assert!(!caught_up(None, 110), "nothing has been decoded yet");
        assert!(caught_up(Some(110.0), 110));
        assert!(caught_up(Some(110.4), 110));
    }
}
