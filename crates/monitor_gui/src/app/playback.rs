//! A camera's recordings, playing in the magnified viewport.
//!
//! The camera keeps its slot: its name, its on-screen display, its place on the
//! wall, its settings. Only the picture comes from somewhere else - a channel of
//! its own, one past the last camera, which the scheduler never names and so
//! never opens, parks or reopens.

use super::*;

use std::time::{SystemTime, UNIX_EPOCH};

use monitor_core::config::FrigateConfig;
use monitor_core::discovery::frigate::{FrigateClient, Segment, VodPlaylist, stretches, window_at};
use monitor_core::model::FrigateOrigin;

/// One camera's recordings, playing in the magnified viewport.
pub(super) struct Playback {
    /// The channel whose picture this is - the camera's own slot, which keeps
    /// its name, its display and its place on the wall.
    slot: usize,
    /// The channel the recording plays on. See [`XgViewApp::playback_index`].
    pub(super) index: usize,
    /// Whose recording it is and where it is served from. The wall can change
    /// the camera behind a slot without changing how many there are - another
    /// group - so the slot alone does not say whose recording this is.
    pub(super) source: Source,
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
    /// The stretch the playlist on this channel covers, when it plays one.
    ///
    /// A moment inside it is another fragment of the session already running -
    /// one request, no decoder to build. A moment outside it is another range,
    /// which is another playlist and another session. `None` when the box serves
    /// no playlist and the stretch is one `clip.mp4`, where every seek is
    /// another window.
    pub(super) vod: Option<(i64, i64)>,
    /// How many seconds the timeline is drawn across. One of [`ZOOMS`].
    pub(super) zoom: i64,
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
    /// A range read while the finger was still on the timeline.
    ///
    /// A drag that leaves the range the session holds needs another playlist,
    /// and asking for it once the finger is lifted puts the request between the
    /// viewer and the picture. The finger says where it is going seconds before
    /// it stops, so the range is read while it is still there and the answer is
    /// waiting when it lifts. See [`XgViewApp::read_ahead`].
    pub(super) ahead: Option<Ahead>,
    /// The moment a seek is waiting to play, while the range it needs is still
    /// being read.
    pub(super) awaiting: Option<i64>,
}

/// A range read before it was needed, and the answer to it.
pub(super) struct Ahead {
    /// The range asked for.
    pub(super) window: (i64, i64),
    /// The fragments it was cut into, once the box has answered - `None` while
    /// the answer is still on its way, and `Some(None)` for a box that serves no
    /// playlist for that range.
    pub(super) answer: Option<Option<VodPlaylist>>,
}

/// Whose recording is playing, and where it is served from.
///
/// The three travel together because nothing about a recording can be asked for
/// without all of them, and none can be looked up again when it is needed: a
/// seek happens long after the recording opened, and the wall may have moved
/// another camera into that slot by then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Source {
    /// The camera's own name, for the events the channel emits.
    pub(super) camera: String,
    /// Where its server answers, and what it calls that camera.
    pub(super) origin: FrigateOrigin,
    /// The login that server takes. Frigate's own, not the restream's.
    pub(super) credentials: Option<(String, String)>,
}

impl Source {
    /// The address a recording of this camera is a suffix of - `…/api/<camera>`.
    fn api_base(&self) -> String {
        format!(
            "{}/api/{}",
            self.origin.base_url.trim_end_matches('/'),
            self.origin.camera
        )
    }
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
        cameras.get(playback.slot).map(|camera| camera.id.as_str())
            != Some(playback.source.camera.as_str())
    }

    /// Whether a recording is playing on this slot.
    pub(super) fn is_playing_back(&self, slot: usize) -> bool {
        matches!(&self.playback, Some(playback) if playback.slot == slot)
    }

    /// Asks a camera's server what there is to play, and plays it.
    ///
    /// The window is read from the segment list rather than guessed: a window is
    /// only playable where a segment covers it, and where it begins is what the
    /// moment each picture is of is counted from. So this is a request, and what
    /// it answers arrives as an event.
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
            Source { camera: camera_id, origin, credentials },
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
        source: Source,
        window: (i64, i64),
        begin: Option<i64>,
        playlist: Option<VodPlaylist>,
        segments: Vec<Segment>,
    ) {
        let index = self.playback_index();
        let (from, to) = window;
        let begin = begin.map(|at| begin_in(window, at));
        let vod = self.play_recording(index, &source, window, playlist, begin);
        let span = extent(&segments, SPAN, begin.unwrap_or(from)).unwrap_or((from, to));
        self.playback = Some(Playback {
            slot: channel,
            index,
            source,
            span,
            zoom: SPAN,
            segments,
            vod,
            scrub: None,
            // The first frame is the moment the cursor starts from, and it is a
            // request away; until then the slot still shows the camera's own
            // picture, which has no moment at all. A playlist's range reaches
            // back before where playback begins, so it is where the picture
            // starts - not the range's - that the cursor waits for.
            pending: Some(begin.unwrap_or(from)),
            scrubbing: false,
            ahead: None,
            awaiting: None,
        });
        // The speaker belongs to the channel whose picture is on screen, and
        // that has just become the recording's.
        self.apply_audio();
    }

    /// Plays another stretch of the recording on the channel already playing it.
    ///
    /// A seek is not another viewport: the slot, the timeline and the camera are
    /// the ones already there, and what changes is what the channel is pulling.
    /// The stretch is played from the moment asked for, which the playlist
    /// reaches by skipping the fragments before it.
    pub(super) fn start_seek(
        &mut self,
        index: usize,
        camera_id: String,
        window: (i64, i64),
        at: i64,
        playlist: Option<VodPlaylist>,
    ) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // A seek that was on its way when the viewer left - or left and came
        // back to another camera - has nowhere to land.
        if playback.index != index || playback.source.camera != camera_id {
            return;
        }
        // The range a playlist was asked for reaches back before the moment, so
        // the picture begins inside it. A box that serves no playlist gets a
        // `clip.mp4`, which begins where it is asked for and no earlier.
        let (window, begin) = match &playlist {
            Some(_) => (window, Some(begin_in(window, at))),
            None => match window_at(&playback.segments, at, WINDOW) {
                Some(window) => (window, None),
                None => return,
            },
        };
        let source = playback.source.clone();
        let vod = self.play_recording(index, &source, window, playlist, begin);
        if let Some(playback) = self.playback.as_mut() {
            playback.scrub = None;
            playback.scrubbing = false;
            playback.vod = vod;
            // The seek this drag was going to make has been made.
            playback.ahead = None;
            playback.awaiting = None;
            // The strip follows the picture: a drag into the recording's history
            // would otherwise leave the cursor pressed against its left edge,
            // with the stretch being watched somewhere off it.
            if let Some(span) = extent(&playback.segments, playback.zoom, begin.unwrap_or(window.0)) {
                playback.span = span;
            }
            // The moment the picture begins at, held to the stretch that will
            // play: a scrub taken to the edge of the nearest recording does not
            // wait for a moment this playlist never reaches.
            playback.pending = Some(begin.unwrap_or(window.0));
        }
    }

    /// Takes the answer to a range read while the finger was down.
    ///
    /// Nothing moves by itself: what is on screen is still the stretch being
    /// left, and the viewer may still have a finger on the timeline. The answer
    /// is kept, and a seek already waiting for it is made now - which is what
    /// makes the wait disappear rather than merely shorter.
    pub(super) fn range_arrived(
        &mut self,
        index: usize,
        camera_id: String,
        window: (i64, i64),
        playlist: Option<VodPlaylist>,
    ) {
        let wanted = {
            let Some(playback) = self.playback.as_mut() else {
                return;
            };
            // An answer for a recording that is no longer on screen, or for a
            // range the finger has since left, has nowhere to go.
            if playback.index != index || playback.source.camera != camera_id {
                return;
            }
            let Some(ahead) = playback.ahead.as_mut() else {
                return;
            };
            if ahead.window != window {
                return;
            }
            ahead.answer = Some(playlist.clone());
            playback.awaiting.filter(|wanted| covered(Some(window), *wanted))
        };
        let Some(wanted) = wanted else {
            return;
        };
        if let Some(playback) = self.playback.as_mut() {
            playback.ahead = None;
            playback.awaiting = None;
        }
        self.start_seek(index, camera_id, window, wanted, playlist);
    }

    /// Asks a channel to play a stretch of a recording, and says which stretch
    /// of it is a playlist.
    ///
    /// A playlist is the shape a seek wants: a fragment is a request, so another
    /// moment is another request rather than another stream. It is taken
    /// whenever the box serves one, and the old way - one `clip.mp4` for the
    /// whole stretch - is what a box without `/vod` falls back to.
    fn play_recording(
        &self,
        index: usize,
        source: &Source,
        window: (i64, i64),
        playlist: Option<VodPlaylist>,
        begin: Option<i64>,
    ) -> Option<(i64, i64)> {
        let (from, to) = window;
        match playlist {
            Some(playlist) => {
                let covered = (playlist.start, playlist.starts_at(playlist.fragments.len()));
                tracing::debug!(
                    target: "xgview::app",
                    index,
                    from,
                    to,
                    fragments = playlist.fragments.len(),
                    begin,
                    "playing a playlist"
                );
                self.manager.play_vod(
                    index,
                    source.camera.clone(),
                    playlist,
                    source.credentials.clone(),
                    begin,
                );
                Some(covered)
            }
            None => {
                let uri = format!("{}/start/{from}/end/{to}/clip.mp4", source.api_base());
                tracing::debug!(target: "xgview::app", index, %uri, "playing a recording");
                self.manager.play_clip(
                    index,
                    uri,
                    source.camera.clone(),
                    source.credentials.clone(),
                    window,
                );
                None
            }
        }
    }

    /// Whether the channel on this index still has a session to be told anything.
    ///
    /// A recording that played to the end of its range, and one that gave up,
    /// leave nothing behind: the channel is a picture and a state, and asking it
    /// to jump would set a value nothing reads. Such a seek has to be another
    /// range. A channel that has not reported a state yet is the opposite case -
    /// its session was just asked for, and reads the jump before its first
    /// sample.
    fn has_session(&self, index: usize) -> bool {
        match self.channels.get(&index) {
            None => true,
            Some(channel) => !matches!(
                channel.state,
                ConnectionState::Idle
                    | ConnectionState::Suspended
                    | ConnectionState::Failed
                    | ConnectionState::Finished
            ),
        }
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
        let response = ui.interact(row, Id::new(("xgview-timeline", index)), Sense::click_and_drag());
        // A scrub is the left button's and a touch's. The right one widens the
        // strip below, and reading it here as a press would have it start a hold
        // on the way.
        let down = ui.ctx().input(|input| input.pointer.button_down(egui::PointerButton::Primary));
        let pos = ui.ctx().input(|input| input.pointer.interact_pos());
        let scrubbing = self.playback.as_ref().is_some_and(|playback| playback.scrubbing);
        // A right press widens the strip a level: ten minutes, half an hour, an
        // hour, and back to ten minutes. Aiming a scrub is a left press, so the
        // gesture that changes what the timeline shows cannot be mistaken for
        // the one that moves the picture - and nothing else on the wall uses
        // that button.
        if response.secondary_clicked() && !scrubbing {
            self.zoom_timeline();
            return;
        }

        if !scrubbing {
            // A press that starts on the strip holds the picture where it is and
            // puts the cursor under the finger. One that starts elsewhere is the
            // picture's, and does nothing here.
            if let (true, Some(pos)) = (down, pos) {
                if row.contains(pos) {
                    self.manager.hold_clip(index, true);
                    let at = moment(pos);
                    let inside = self.playback.as_ref().is_some_and(|playback| covered(playback.vod, at));
                    if inside {
                        // A press can land far from the picture, and the fragment
                        // under it is fetched while the finger is still there.
                        self.manager.prefetch_fragment(index, at);
                    } else {
                        // A press outside the range the session holds has no
                        // fragment of it to fetch: what that seek will need is
                        // another playlist, and a tap is enough time to read it.
                        self.read_ahead(index, at);
                    }
                    if let Some(playback) = self.playback.as_mut() {
                        playback.scrubbing = true;
                        playback.scrub = Some(at);
                        // The range the last drag read is not this drag's.
                        playback.ahead = None;
                        playback.awaiting = None;
                    }
                }
            }
            return;
        }

        match (down, pos) {
            // The finger may leave the strip; the cursor follows it to the end
            // rather than stopping where the strip does.
            (true, Some(pos)) => {
                let at = moment(pos);
                let moved = self.playback.as_ref().is_some_and(|playback| playback.scrub != Some(at));
                if moved {
                    let inside = self.playback.as_ref().is_some_and(|playback| covered(playback.vod, at));
                    if inside {
                        // Where the seek will land, said while there is still
                        // time to fetch it: the session holds one fragment and
                        // fetches the one under the finger, so a drag across the
                        // strip costs no more than the fragment it stops on.
                        self.manager.prefetch_fragment(index, at);
                    } else {
                        // The finger has left the range the session holds, so
                        // that seek will need another playlist. Read it now,
                        // while there is a finger to hide the wait under.
                        self.read_ahead(index, at);
                    }
                }
                if let Some(playback) = self.playback.as_mut() {
                    playback.scrub = Some(at);
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
            // frame is the one that would put the cursor back. A seek either
            // jumps inside the session - and lets go of the hold itself, with
            // the jump already in the session's hand - or replaces it, and a
            // replaced session ends the hold with it.
            Some(at) => self.seek(at),
            // Nothing under the finger: the recording goes on where it was
            // held.
            None => self.manager.hold_clip(index, false),
        }
    }

    /// Widens the timeline a level, or takes it back to the narrowest.
    ///
    /// What is on screen does not move: the strip still ends where it ended, so
    /// a wider one reaches further back and a narrower one reaches less far. The
    /// range follows the next time one is asked for - a drag to a moment the
    /// session does not hold asks for the stretch the timeline shows.
    fn zoom_timeline(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let zoom = next_zoom(playback.zoom);
        playback.zoom = zoom;
        let anchor = playback.span.1;
        if let Some(span) = extent(&playback.segments, zoom, anchor) {
            playback.span = span;
        }
        tracing::debug!(target: "xgview::app", zoom, "the timeline changed width");
    }

    /// Reads the range a finger is over while it is still on the timeline.
    ///
    /// A drag that leaves the range the session holds will need another
    /// playlist. Asked for once the finger is lifted, that request sits between
    /// the viewer and the picture; asked for now, it is answered by then.
    /// Nothing moves: the answer waits for a seek to want it.
    ///
    /// One range at a time. A finger that keeps moving asks again only once it
    /// has left the range it already asked for, which on a ten-minute timeline
    /// is a request per ten minutes of travel rather than one per frame.
    fn read_ahead(&mut self, index: usize, at: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.ahead.as_ref().is_some_and(|ahead| covered(Some(ahead.window), at)) {
            return;
        }
        let width = playback.zoom;
        let Some(window) = window_at(
            &playback.segments,
            range_start(&playback.segments, at, width / 2),
            width,
        ) else {
            return;
        };
        let source = playback.source.clone();
        tracing::debug!(
            target: "xgview::app",
            index,
            at,
            from = window.0,
            to = window.1,
            "reading a range under the finger"
        );
        start_playlist(&self.handle, &self.events_tx, index, source, window, at, true);
        if let Some(playback) = self.playback.as_mut() {
            playback.ahead = Some(Ahead { window, answer: None });
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
        let (index, vod) = (playback.index, playback.vod);
        // Neither of these can happen while the list is the one the window was
        // built from - and if it does, saying so is better than a picture that
        // silently carries on where it was, which reads as a drag that was
        // refused.
        let Some(target) = nearest(&stretches(&playback.segments), at) else {
            self.manager.hold_clip(index, false);
            self.flash(monitor_i18n::tr("toast-no-recording"), ToastKind::Error);
            return;
        };

        // A moment the playlist covers is another fragment of the session that
        // is already running: the decoder, the parser and the login carry on,
        // and the fragments in between are never asked for. A session that
        // reached the end of its range has nothing left to jump in - its task is
        // gone - so that seek is another range too.
        if covered(vod, target) && self.has_session(index) {
            tracing::debug!(target: "xgview::app", index, at = target, "playing another fragment");
            // Asked for before the hold is let go of, so that the session wakes
            // to the jump rather than to the frame it was stopped on: a resumed
            // session publishes the sample it was waiting for, and that sample
            // is the moment the drag began from.
            self.manager.seek_fragment(index, target);
            self.manager.hold_clip(index, false);
            if let Some(playback) = self.playback.as_mut() {
                playback.scrub = None;
                playback.scrubbing = false;
                playback.pending = Some(target);
            }
            return;
        }

        // A range read while the finger was down is this seek's own request:
        // the answer is used rather than asked for again, and one still being
        // read is waited for - which is the whole point of having asked early.
        let ahead = playback
            .ahead
            .as_ref()
            .filter(|ahead| covered(Some(ahead.window), target))
            .map(|ahead| (ahead.window, ahead.answer.clone()));
        if let Some((window, answer)) = ahead {
            match answer {
                Some(playlist) => {
                    let camera_id = playback.source.camera.clone();
                    self.start_seek(index, camera_id, window, target, playlist);
                }
                // Still on its way: the seek waits for it rather than asking
                // again, and the cursor stays where the finger left it.
                None => {
                    if let Some(playback) = self.playback.as_mut() {
                        playback.awaiting = Some(target);
                        playback.scrub = None;
                        playback.scrubbing = false;
                        playback.pending = Some(target);
                    }
                }
            }
            return;
        }

        // Somewhere else: another range, which is another playlist. The hold is
        // kept while it is read - the picture stays where it is rather than
        // running on to the end of the stretch being left - and the session that
        // replaces this one starts unpaused.
        // The range is the stretch the timeline shows, at whatever width it is
        // showing it: a wider timeline reaches further back, and the range
        // reaches with it.
        let width = playback.zoom;
        let Some(window) = window_at(
            &playback.segments,
            range_start(&playback.segments, target, width / 2),
            width,
        ) else {
            self.manager.hold_clip(index, false);
            self.flash(monitor_i18n::tr("toast-no-recording"), ToastKind::Error);
            return;
        };
        let source = playback.source.clone();
        // The cursor stays where the finger left it while the range is read.
        // Left to the picture it would go back to the moment the drag began from
        // - the frame on screen is still the stretch being left - and only
        // return a request later, which reads as a drag that bounced off.
        if let Some(playback) = self.playback.as_mut() {
            playback.scrub = None;
            playback.scrubbing = false;
            playback.pending = Some(target);
        }
        tracing::debug!(target: "xgview::app", index, at = target, "another stretch for a scrub");
        start_playlist(&self.handle, &self.events_tx, index, source, window, target, false);
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

/// The widths the timeline can be shown at, narrowest first.
///
/// Ten minutes is what a scrub can be aimed at across a strip this wide - an
/// hour across it is three seconds to a pixel. But ten minutes is also all a
/// viewer can *see*, and a recording goes back an hour, so the wider levels are
/// for finding something older rather than for aiming: the strip always ends at
/// the newest moment recorded, so a wider one reaches further back and a
/// narrower one reaches less far.
const ZOOMS: [i64; 3] = [600, 1800, 3600];

/// How much of a recording the timeline shows when a recording is opened.
const SPAN: i64 = ZOOMS[0];

/// The next width up, wrapping back to the narrowest.
fn next_zoom(zoom: i64) -> i64 {
    ZOOMS.iter().copied().find(|width| *width > zoom).unwrap_or(ZOOMS[0])
}

/// What the timeline is drawn across: `zoom` seconds of what was read, ending
/// where the viewer is looking.
///
/// The end is the newest moment recorded, unless that would leave the moment
/// being watched too far behind it - which a scrub into the recording's history
/// does. Then it ends a quarter of a width past that moment, so the cursor sits
/// where there is still somewhere to scrub to on either side of it.
fn extent(segments: &[Segment], zoom: i64, anchor: i64) -> Option<(i64, i64)> {
    let first = segments.first()?;
    let last = segments.last()?;
    if last.end <= first.start {
        return None;
    }
    let end = last.end.min(anchor + zoom / 4).max(first.start + 1);
    Some((end.saturating_sub(zoom).max(first.start), end))
}

/// Whether the playlist a channel is playing can reach a moment.
///
/// The stretch it covers runs from its start to the end of its last fragment,
/// and the last moment it can play is the one before that end. A seek to the end
/// itself would be a wait for a sample the playlist does not hold, which ends
/// the session instead of moving to it - so the end belongs to the next range.
fn covered(vod: Option<(i64, i64)>, at: i64) -> bool {
    vod.is_some_and(|(start, end)| start <= at && at < end)
}

/// The range a playlist is asked for: what the timeline shows.
///
/// A range the width of the timeline and placed anywhere else would leave most
/// scrubs outside it - half of what the viewer can see would be another range
/// and another session. Widened only when the moment is outside the span, which
/// a scrub taken to the edge of a recording can produce.
fn range_for(segments: &[Segment], span: (i64, i64), at: i64) -> (i64, i64) {
    (range_from(segments, span.0.min(at), at), span.1.max(at + 1))
}

/// Where a range begins, moved back to the start of the segment it falls in.
///
/// Frigate serves a fragment straight out of the recorded file when the range
/// begins on a segment's own boundary, and cuts the whole range when it does
/// not. Measured against the box of 2026-11-02, `cam_living_room`, the same
/// ten-second fragments:
///
/// | range begins | playlist | a fragment's first byte |
/// | --- | --- | --- |
/// | on a boundary | 159 ms | 115-164 ms |
/// | in the middle of a segment | 671-1034 ms | 467-967 ms |
///
/// Every fragment of the range is cut, not only the first - so where the range
/// begins is most of what a seek costs. It was never on a boundary: the range
/// was the timeline's own span, `last.end - SPAN`, which is whatever moment the
/// recording happened to stop at.
///
/// Moved back rather than forward, so the range still holds the moment asked
/// for, and no further back than the start of the stretch that moment is in: a
/// range ends at the first hole, so one that began on the far side of a hole
/// would end before the moment it was asked for.
fn range_from(segments: &[Segment], from: i64, at: i64) -> i64 {
    let floor = stretches(segments)
        .into_iter()
        .find(|stretch| stretch.start <= at && at <= stretch.end)
        .map_or(from, |stretch| stretch.start);
    let from = from.max(floor);
    segments
        .iter()
        .rev()
        .find(|segment| segment.start <= from && segment.start >= floor)
        .map_or(from, |segment| segment.start)
}

/// Where a range asked for around a moment has to begin.
///
/// A range runs from where it starts to the first hole, so the only way to be
/// sure it holds the moment asked for is to start inside the same stretch of
/// recording: [`LEAD`] back when the recording is continuous there, and at the
/// stretch's own start when it is not. Starting earlier and trusting the range to
/// reach the moment is what a hole between the two would break - the range would
/// end before the moment, and the session would find nothing at it.
///
/// The stretch's start is a segment's start, and so is the moment `lead` back
/// lands on after [`range_from`] moves it to one - which is what keeps the box
/// from cutting the whole range. See [`range_from`].
///
/// `lead` is half the width the timeline is showing, so that the range runs as
/// far ahead of the moment as it does behind it and a scrub either way stays
/// inside it: a jump within a playlist is half a second, and one out of it is a
/// request and a session.
fn range_start(segments: &[Segment], at: i64, lead: i64) -> i64 {
    range_from(segments, at - lead, at)
}

/// Where playback begins inside a range.
///
/// The moment asked for, held to a moment the range holds. A scrub can land on
/// the very last second of a stretch - [`nearest`] takes it to that edge - and a
/// range asked for around it ends where the stretch does, so the moment can be
/// one past the range's end. Beginning at the range's own end would be a wait for
/// a picture it never reaches.
fn begin_in(window: (i64, i64), at: i64) -> i64 {
    at.clamp(window.0, window.1 - 1)
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
/// Ten minutes, whether it arrives as one `clip.mp4` or as a playlist cut from a
/// range that wide. Long enough that the seam a new range makes comes once in
/// ten minutes rather than once a minute. A connection that drops part-way
/// through used to cost this whole range of rewind, which was the other reason
/// for the width; it is resumed where it broke now, so the width is the seam's
/// alone. What is asked for and where playback begins are two different
/// questions: the range runs from the moment asked for, and only what was
/// recorded fills it.
const WINDOW: i64 = 600;

/// How far back opening a recording begins, when nothing else says.
///
/// A minute, which is what a viewer means by "what just happened". A recording
/// ends at the present, so a minute back is usually all there is to play - and
/// when it does not, [`entry_moment`] takes it to the last moment there is.
const ENTRY_BACK: i64 = 60;

/// The moment entering a recording begins at.
///
/// A minute back, or the last moment the recording holds when nothing was
/// recorded that recently. A camera that stopped recording an hour ago would
/// otherwise be opened at a moment no fragment reaches, and the channel would
/// report nothing at it rather than the last thing the camera saw.
fn entry_moment(span: Option<(i64, i64)>, at: i64) -> i64 {
    span.map_or(at, |(start, end)| at.clamp(start, end - 1))
}

/// Reads what a camera has recorded and sends back the stretch to play.
///
/// The address is the camera's own rather than the import's configuration: the
/// server may have been edited since, and the camera carries where it answers.
///
/// The playlist is read here too, in the same request's shadow: it is what the
/// channel will actually pull, and asking for it before the channel exists is
/// what keeps the first picture from being a `clip.mp4` that is then replaced.
fn start_window(
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    channel: usize,
    source: Source,
    now: i64,
) {
    let sender = events.clone();
    handle.spawn(async move {
        let (user, pass) = source.credentials.clone().unwrap_or_default();
        let config = FrigateConfig::at(&source.origin.base_url, &user, &pass);
        let mut client = FrigateClient::new(config).ok();
        let segments = match client.as_mut() {
            Some(client) => client
                .segments(&source.origin.camera, now - LOOKBACK, now)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        // Where entering begins, and the strip drawn across it. The moment is
        // the unclamped one: `entry_moment` is what holds it to the recording,
        // and holding it before the strip is chosen would draw the strip around
        // a moment it had already moved.
        let entry = now - ENTRY_BACK;
        let span = extent(&segments, SPAN, entry);
        let at = entry_moment(span, entry);
        // The playlist is asked for the stretch the timeline shows, so that a
        // scrub anywhere within it is a fragment of the session already running
        // rather than another range and another session. A box with no playlist
        // gets a window from the moment itself, which is where a `clip.mp4` is
        // cut.
        let range = span.map(|span| range_for(&segments, span, at));
        let playlist = match (range, client.as_mut()) {
            (Some(range), Some(client)) => {
                read_playlist(client, &source.origin.camera, range).await
            }
            _ => None,
        };
        let (window, begin) = match (&playlist, range) {
            (Some(_), Some(range)) => (Some(range), Some(begin_in(range, at))),
            _ => (window_at(&segments, at, WINDOW), None),
        };
        let _ = sender.send(BackgroundEvent::PlaybackReady {
            channel,
            camera_id: source.camera,
            origin: source.origin,
            credentials: source.credentials,
            window,
            begin,
            playlist,
            segments,
        });
    });
}

/// Reads the playlist for a stretch a seek asked for, and sends it back.
///
/// The seek is not held up by it: what is on screen while the range is read is
/// still the stretch being left, which is what the viewer was looking at anyway.
/// Until the answer arrives the cursor waits where the finger left it.
fn start_playlist(
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    index: usize,
    source: Source,
    window: (i64, i64),
    at: i64,
    ahead: bool,
) {
    let sender = events.clone();
    handle.spawn(async move {
        let (user, pass) = source.credentials.clone().unwrap_or_default();
        let config = FrigateConfig::at(&source.origin.base_url, &user, &pass);
        let playlist = match FrigateClient::new(config) {
            Ok(mut client) => read_playlist(&mut client, &source.origin.camera, window).await,
            Err(_) => None,
        };
        let _ = sender.send(BackgroundEvent::PlaylistReady {
            index,
            camera_id: source.camera,
            window,
            at,
            playlist,
            ahead,
        });
    });
}

/// The playlist a stretch was cut into, or `None` when the box serves none.
///
/// A box older than `/vod` answers `404`, and its recordings are played the way
/// they always were: one `clip.mp4` for the whole stretch, which a seek replaces
/// outright. That is worth a debug line rather than a toast - the recording
/// plays either way, and the only thing that changed is how a seek costs.
async fn read_playlist(
    client: &mut FrigateClient,
    camera: &str,
    window: (i64, i64),
) -> Option<VodPlaylist> {
    let (from, to) = window;
    match client.vod_playlist(camera, from, to).await {
        Ok(playlist) => Some(playlist),
        Err(err) => {
            tracing::debug!(
                target: "xgview::app",
                %camera,
                from,
                to,
                %err,
                "no playlist for this stretch, playing it whole"
            );
            None
        }
    }
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

    /// The timeline is drawn across the stretch being watched, which is wider
    /// than the window: drawn across the window alone it would put the cursor at
    /// its left edge after every seek, and the picture would have moved while
    /// the cursor looked as though it had gone back.
    #[test]
    fn the_timeline_spans_what_was_read() {
        let segments = [Segment { start: 100, end: 110 }, Segment { start: 110, end: 120 }];
        assert_eq!(extent(&segments, 600, 120), Some((100, 120)));
        assert_eq!(extent(&[], 600, 120), None);
        // A list that ends before it starts has no span to draw.
        assert_eq!(extent(&[Segment { start: 120, end: 100 }], 600, 120), None);
        // An hour of recording is shown ten minutes at a time, from its end.
        assert_eq!(extent(&[Segment { start: 0, end: 3600 }], 600, 3600), Some((3000, 3600)));
        // And a recording shorter than that is shown whole.
        assert_eq!(extent(&[Segment { start: 3500, end: 3600 }], 600, 3600), Some((3500, 3600)));
    }

    /// The strip ends at the newest moment recorded, unless the moment being
    /// watched is further back than that would reach - and the wider levels
    /// reach further back, which is what they are for.
    #[test]
    fn the_timeline_follows_what_is_being_watched() {
        let segments = run(0, 3600);
        // At the newest part of the recording the strip ends with it.
        assert_eq!(extent(&segments, 600, 3540), Some((3000, 3600)));
        // A scrub an hour back brings the strip with it, a quarter of a width
        // past the moment, so there is somewhere to scrub to on either side.
        assert_eq!(extent(&segments, 600, 1000), Some((550, 1150)));
        // The same moment on a wider strip: it reaches further back.
        assert_eq!(extent(&segments, 1800, 1000), Some((0, 1450)));
        // And the widths go ten minutes, half an hour, an hour, back to ten.
        assert_eq!(next_zoom(600), 1800);
        assert_eq!(next_zoom(1800), 3600);
        assert_eq!(next_zoom(3600), 600);
    }

    /// The range a playlist is asked for is the stretch the timeline shows, so
    /// that a scrub anywhere inside it is another fragment of the same session.
    #[test]
    fn the_playlist_range_is_what_the_timeline_shows() {
        let segments = run(900, 1800);
        let span = (1000, 1600);
        assert_eq!(range_for(&segments, span, 1100), span, "a moment inside it does not move it");
        assert_eq!(range_for(&segments, span, 1600), (1000, 1601), "the end itself is inside");
        // A moment the span does not reach - a scrub taken to the edge of the
        // recording - widens it rather than leaving the moment outside.
        assert_eq!(range_for(&segments, span, 900), (900, 1600));
        assert_eq!(range_for(&segments, span, 1700), (1000, 1701));
        // And a span that begins in the middle of a segment begins at the
        // segment's own boundary instead: that is what the box serves without
        // cutting. See [`range_from`].
        assert_eq!(range_for(&segments, (1003, 1600), 1100), (1000, 1600));
        assert_eq!(range_for(&segments, (1003, 1600), 1600), (1000, 1601));
    }

    /// A run of ten-second segments, the way the recorder writes them.
    fn run(from: i64, to: i64) -> Vec<Segment> {
        (from..to).step_by(10).map(|start| Segment { start, end: start + 10 }).collect()
    }

    /// A range asked for around a moment has to hold it, whatever the recording
    /// looks like between the two: a hole is where a range ends, so a range that
    /// starts before one never reaches the moment on its far side.
    #[test]
    fn a_range_begins_inside_the_stretch_the_moment_is_in() {
        // Continuous: half a window back, which is as far behind as it runs ahead.
        let continuous = run(0, 3600);
        let lead = SPAN / 2;
        assert_eq!(range_start(&continuous, 2000, lead), 2000 - lead);
        // A recording that does not go back that far begins at its first moment.
        assert_eq!(range_start(&continuous, 100, lead), 0);
        // A moment in the middle of a segment begins at that segment's boundary,
        // so the box serves the range rather than cutting it.
        assert_eq!(range_start(&continuous, 2007, lead), 2000 - lead);
        assert_eq!(range_start(&continuous, 2009, lead), 1700, "the lead lands on 1709");

        // A hole between the lead and the moment: the range begins where the
        // stretch the moment is in begins, which is on the moment's side of it.
        let gapped = [Segment { start: 0, end: 100 }, Segment { start: 200, end: 400 }];
        assert_eq!(range_start(&gapped, 300, lead), 200, "the stretch's own start");
        assert_eq!(range_start(&gapped, 90, lead), 0);
        // And a moment in the hole itself falls back to the lead, which
        // `window_at` lifts to the next stretch.
        assert_eq!(range_start(&gapped, 150, lead), 150 - lead);

        // What the two are for: for every moment a scrub can land on - which is
        // one a stretch covers - the range holds it.
        for at in [90, 300] {
            let (from, to) = window_at(&gapped, range_start(&gapped, at, lead), WINDOW).unwrap();
            assert!(from <= at && at < to, "{at} is not in {from}..{to}");
        }
    }

    /// Entering a recording begins a minute back, or at the last moment it holds
    /// when nothing was recorded that recently: a camera that stopped an hour ago
    /// would otherwise be opened at a moment no fragment reaches.
    #[test]
    fn entering_begins_at_the_last_moment_when_nothing_was_recorded_just_now() {
        let span = Some((1000, 1600));
        assert_eq!(entry_moment(span, 1540), 1540, "a minute back, inside the span");
        assert_eq!(entry_moment(span, 1700), 1599, "past its end: the last moment it holds");
        assert_eq!(entry_moment(span, 900), 1000, "before it: its first");
        // Nothing recorded at all: there is no stretch to be held to.
        assert_eq!(entry_moment(None, 1700), 1700);
    }

    /// A seek inside the playlist already playing is another fragment of it; a
    /// seek outside is another range, and another session.
    #[test]
    fn a_seek_inside_the_playlist_stays_in_the_session() {
        let vod = Some((100, 700));
        assert!(covered(vod, 100), "its first moment is inside it");
        assert!(covered(vod, 400));
        assert!(covered(vod, 699), "the last moment it can play");
        // The end itself is a moment the last fragment does not hold, and
        // asking for it would end the session rather than move within it.
        assert!(!covered(vod, 700));
        assert!(!covered(vod, 99), "nothing before the range was fetched");
        // A box with no playlist has no stretch to stay inside.
        assert!(!covered(None, 400));
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
