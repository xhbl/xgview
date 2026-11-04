//! A camera's recordings, playing in the magnified viewport.
//!
//! The camera keeps its slot: its name, its on-screen display, its place on the
//! wall, its settings. Only the picture comes from somewhere else - a channel of
//! its own, one past the last camera, which the scheduler never names and so
//! never opens, parks or reopens.

use super::*;

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tilebar;
use monitor_core::blackout::{LocalTime, local_at};
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
    /// Where playback opened, which is what the cursor shows before the first
    /// frame has arrived to say where the picture really is.
    pub(super) opened: i64,
    /// How fast the picture plays: one, two or four times. The session is told;
    /// the sound is not, so anything but one is silent. See
    /// [`XgViewApp::cycle_speed`].
    pub(super) speed: f32,
    /// Where a drag on the strip began: how far along it the finger went down,
    /// and the moment under the cursor then. What moves is the strip, so this is
    /// the anchor a drag is measured from rather than a place the cursor goes.
    pub(super) drag: Option<(f32, i64)>,
    /// How far a pinch over the strip has spread, as a ratio that starts at one.
    /// A pinch arrives as a factor per frame, so a step of the ladder is taken
    /// when the product of them has doubled or halved.
    pub(super) pinch: f32,
    /// An arrow held down on the timeline, and since when. A held key walks
    /// further per repeat the longer it is held - see
    /// [`XgViewApp::walk_timeline`].
    pub(super) held: Option<(bool, std::time::Instant)>,
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
    /// What the strip is showing. See [`Timeline`].
    pub(super) timeline: Timeline,
    /// The days the calendar shows, oldest first, once the box has answered.
    /// Empty until it has.
    pub(super) days: Vec<Day>,
    /// How far the calendar is scrolled along, in points from its first day.
    ///
    /// A calendar is twenty-one days and a phone shows eight of them, so the
    /// rest have to be reachable: the row slides under the finger, and what is
    /// off its ends is simply not drawn.
    pub(super) days_scroll: f32,
    /// The finger that is sliding the calendar: where it went down, and what the
    /// scroll was then. A press that moves is a scroll and one that does not is
    /// a pick, so a press alone picks nothing.
    pub(super) days_drag: Option<(f32, f32)>,
    /// Whether the calendar should bring the day the picture is on into view.
    /// Set when it is shown, and cleared once it has been.
    pub(super) days_follow: bool,
    /// Whether the day list is being read.
    pub(super) reading_days: bool,
    /// The moment on a picked day the strip should move to, once that day's
    /// segment list has been read. It is the same time of day as the moment on
    /// screen, not the day's first moment. See [`XgViewApp::pick_day`].
    pub(super) to_day: Option<i64>,
    /// The moment a seek is waiting to play, while the range it needs is still
    /// being read.
    pub(super) awaiting: Option<i64>,
    /// A moment a seek is waiting to reach, while the *segment list* is being
    /// read to hold it. A drag or a jump can go anywhere the recording does, so
    /// the list is read on the way rather than being the limit.
    pub(super) to_seek: Option<i64>,
    /// Which way a jump was asked for, while the events are being read to find
    /// one. `true` is forwards.
    pub(super) to_jump: Option<bool>,
    /// The stretch of segment list being read for a strip wider than what was
    /// held. See [`XgViewApp::read_older`].
    pub(super) reading_older: Option<(i64, i64)>,
    /// The earliest moment the camera answered "nothing recorded" for, once a
    /// strip has been drawn that far back. The strip stops there.
    pub(super) oldest: Option<i64>,
    /// Whether the picture is held. The session is told, and stops reading; the
    /// bar draws the control as the one that lets it go.
    pub(super) paused: bool,
    /// When the camera's events began and ended, in the stretch that was read.
    /// Empty until the box has answered, and empty for a camera with none.
    pub(super) events: Vec<(i64, i64)>,
    /// Whether the event list is being read.
    pub(super) reading_events: bool,
}

/// What the timeline is showing.
///
/// Two projections of the same recording: a stretch of it, where a moment is a
/// distance along the strip, and the days it was recorded on, where a day is a
/// band and the strip is a calendar. They are the plan's six widths and its
/// lowest level - the one that answers "which night was that" rather than "what
/// time was that".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Timeline {
    /// Seconds across the strip.
    Span,
    /// One band per day, the newest at the end.
    Days,
}

/// One day of the calendar.
#[derive(Debug, Clone)]
pub(super) struct Day {
    /// Where the day begins, near enough for the strip that follows it: the
    /// first moment of it the local clock was seen at.
    pub(super) start: i64,
    /// What the day is called on the strip - `10/31`.
    pub(super) label: String,
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
        let from = window.0;
        let begin = begin.map(|at| begin_in(window, at));
        let vod = self.play_recording(index, &source, window, playlist, begin);
        self.playback = Some(Playback {
            slot: channel,
            index,
            source,
            opened: begin.unwrap_or(from),
            speed: 1.0,
            pinch: 1.0,
            drag: None,
            held: None,
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
            timeline: Timeline::Span,
            days: Vec::new(),
            days_scroll: 0.0,
            days_drag: None,
            days_follow: false,
            reading_days: false,
            to_day: None,
            ahead: None,
            awaiting: None,
            to_seek: None,
            to_jump: None,
            reading_older: None,
            oldest: None,
            paused: false,
            events: Vec::new(),
            reading_events: false,
        });
        // The speaker belongs to the channel whose picture is on screen, and
        // that has just become the recording's.
        self.apply_audio();
        // The events of the stretch the timeline shows, so that the two marks
        // either side of the cursor are known before anyone asks to step to one.
        self.read_events(index);
    }

    /// Holds the picture, or lets it go.
    ///
    /// A hold is the session's own pause: it stops reading, so nothing is
    /// decoded and nothing is published, and the clock owes no time for the wait.
    pub(super) fn toggle_play(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        playback.paused = !playback.paused;
        let (index, paused) = (playback.index, playback.paused);
        self.manager.hold_clip(index, paused);
        tracing::debug!(target: "xgview::app", index, paused, "playback held or let go");
    }

    /// Moves the picture by `by` seconds.
    ///
    /// Held, the step is the whole of what happens: a seek while the session is
    /// paused lands on one frame and stays there - the session publishes the
    /// sample it lands on and goes back to holding. Playing, it is a jump and
    /// the recording carries on from there.
    pub(super) fn step_playback(&mut self, by: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let at = self.centre_of(playback) + by;
        self.move_to(at);
    }

    /// Steps to the event before or after the moment on screen.
    ///
    /// "Before" is strictly before: an event that begins where the picture is
    /// would otherwise be the answer every time the button was pressed, and the
    /// button would do nothing. A second of slack, because an event's first
    /// frame and the moment it is named by are not the same instant.
    pub(super) fn jump_event(&mut self, forward: bool) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let index = playback.index;
        let now = self.centre_of(playback);
        let target = if forward {
            playback.events.iter().map(|event| event.0).find(|at| *at > now + 1)
        } else {
            playback.events.iter().rev().map(|event| event.0).find(|at| *at < now - 1)
        };
        if let Some(at) = target {
            tracing::debug!(target: "xgview::app", at, forward, "stepping to an event");
            // Through `move_to`, so a picture held before the press is still
            // held after it: an event is a moment to look at, not a reason to
            // start playing again.
            self.move_to(at);
            return;
        }
        // Nothing in the list - but the list is only the stretch the strip was
        // showing. A jump goes anywhere the recording does, so the events of the
        // stretch around the moment are asked for and the jump is made when they
        // arrive. Asking again for a list that already covers it would be a
        // request a press, so the second answer is the last word.
        //
        // A day of them, on the side the jump is going: an event beyond that is
        // found by the press after this one, once the recording has been read
        // that far. Anchored on the moment on screen rather than on the newest
        // recording, because the viewer may be days back in the history - and a
        // day ending at the newest would then be a list with nothing on the
        // right side of the cursor at all.
        let newest = playback.segments.last().map_or(now + LOOKBACK, |last| last.end);
        let (from, to) = if forward {
            (now, (now + DAY).min(newest))
        } else {
            (now - DAY, now)
        };
        let covered = playback
            .events
            .first()
            .zip(playback.events.last())
            .is_some_and(|(first, last)| first.0 <= from && last.1 >= to);
        if to <= from || covered || playback.to_jump.is_some() {
            tracing::debug!(target: "xgview::app", forward, "no event that way");
            return;
        }
        tracing::debug!(target: "xgview::app", forward, "reading the events for a jump");
        if let Some(playback) = self.playback.as_mut() {
            playback.to_jump = Some(forward);
        }
        self.read_events_between(index, from, to);
    }

    /// The moment under the cursor: the middle of the strip, which never moves.
    ///
    /// The frame store is the answer when there is one - it is the picture on
    /// screen, and the strip slides under the cursor as the picture plays. A
    /// seek that has not arrived yet is already there, and a finger on the strip
    /// has taken the cursor with it.
    fn centre_of(&self, playback: &Playback) -> i64 {
        // The finger first, then the seek that has not arrived, then the picture.
        // The finger has to come first or the strip does not move until it is let
        // go of: the frame store holds the picture the drag began from, and it is
        // always there while a recording plays.
        playback
            .scrub
            .or(playback.pending)
            .or_else(|| {
                self.manager
                    .frames()
                    .latest(playback.index)
                    .and_then(|frame| frame.at)
                    .map(|at| at as i64)
            })
            .unwrap_or(playback.opened)
    }

    /// The stretch the strip is drawn across: the width it is showing, centred
    /// on the cursor.
    fn window_of(&self, playback: &Playback) -> (i64, i64) {
        let centre = self.centre_of(playback);
        let half = playback.zoom / 2;
        (centre - half, centre + half)
    }

    /// Whether the picture is held, for the bar's play control to draw.
    pub(super) fn playback_paused(&self) -> bool {
        self.playback.as_ref().is_some_and(|playback| playback.paused)
    }

    /// Reads the events of the stretch the timeline shows.
    fn read_events(&mut self, index: usize) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let (from, to) = self.window_of(playback);
        self.read_events_between(index, from, to);
    }

    /// Reads the events of a stretch. One at a time: a second ask while the
    /// first is in flight would be two answers for one list.
    fn read_events_between(&mut self, index: usize, from: i64, to: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.reading_events {
            return;
        }
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, from, to, "reading the events");
        if let Some(playback) = self.playback.as_mut() {
            playback.reading_events = true;
        }
        start_events(&self.handle, &self.events_tx, index, source, from, to);
    }

    /// Takes the events of a stretch. A box that could not be asked leaves the
    /// list empty, which is a timeline with no marks rather than an error.
    pub(super) fn events_arrived(
        &mut self,
        index: usize,
        camera_id: String,
        events: Option<Vec<(i64, i64)>>,
    ) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        if playback.index != index || playback.source.camera != camera_id {
            return;
        }
        playback.reading_events = false;
        if let Some(events) = events {
            playback.events = events;
        }
        // A jump was waiting for this list: it is made now, and it does not ask
        // for the list again - `to_jump` is cleared first, so a second answer
        // that still holds nothing says so instead of reading on.
        if let Some(forward) = playback.to_jump.take() {
            if !playback.events.is_empty() {
                self.jump_event(forward);
            }
        }
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
        let speed = playback.speed;
        let vod = self.play_recording(index, &source, window, playlist, begin);
        // A new session starts at normal speed, so the rate the viewer chose is
        // put back on it: the control says "2x" and the picture has to agree.
        self.manager.set_rate(index, speed);
        if let Some(playback) = self.playback.as_mut() {
            playback.scrub = None;
            playback.scrubbing = false;
            playback.vod = vod;
            // The seek this drag was going to make has been made.
            playback.ahead = None;
            playback.awaiting = None;
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
        if let Timeline::Days = playback.timeline {
            self.draw_days(ui, row, playback);
            return;
        }
        let (start, end) = self.window_of(playback);
        let centre = self.centre_of(playback);
        let span = (end - start).max(1) as f32;
        // A turned picture carries a strip that runs down the screen, so its
        // times are written turned with it rather than on their side.
        let horizontal = row.width() > row.height();
        let along = |at: i64| {
            let t = ((at - start) as f32 / span).clamp(0.0, 1.0);
            if horizontal {
                row.left() + row.width() * t
            } else {
                row.top() + row.height() * t
            }
        };
        // Four bands in one row: the times above, the ruler under them, the
        // strip itself, and the moment under the cursor below. The cursor never
        // moves, so the strip does - which is what makes a long recording feel
        // like a long strip.
        //
        // `across` is a fraction of the way from the picture's top of the row to
        // its bottom, which is down the screen for a picture as it arrives and
        // right-to-left for one drawn turned. Every band is placed with it, so
        // the two orientations are the same drawing.
        let across = |t: f32| {
            if horizontal {
                row.top() + row.height() * t
            } else {
                row.right() - row.width() * t
            }
        };
        // The strip's centre, and the edge of it the ruler stands on: the
        // times above, the ticks, then the band itself.
        let track = across(BAND_ROW);
        let band = |from: f32, to: f32, thick: f32| {
            let to = to.max(from + 1.0);
            if horizontal {
                Rect::from_two_pos(egui::pos2(from, track - thick / 2.0), egui::pos2(to, track + thick / 2.0))
            } else {
                Rect::from_two_pos(egui::pos2(track - thick / 2.0, from), egui::pos2(track + thick / 2.0, to))
            }
        };

        let painter = ui.painter();
        // The track the recording sits in: the whole strip, in the colour of
        // nothing recorded. Drawn first and across the whole window, so a hole
        // reads as a stretch of empty track rather than as the end of the band -
        // and so does the time before the first recording and after the last,
        // where the strip runs off the ends of what exists.
        painter.rect_filled(band(along(start), along(end), BAND), CornerRadius::same(2), theme::TRACK);
        // The stretches that were recorded. Merged first: the recorder's own file
        // boundaries are fractions of a second apart, and drawing them would put
        // a notch every ten seconds along a recording that is continuous.
        for segment in &stretches(&playback.segments) {
            if segment.end <= start || segment.start >= end {
                continue;
            }
            let from = along(segment.start.max(start));
            let to = along(segment.end.min(end));
            painter.rect_filled(band(from, to, BAND), CornerRadius::same(2), theme::ACCENT.gamma_multiply(0.55));
        }
        // Where the camera noticed something: the same band in the colour the
        // wall uses for a warning, so the strip reads as "recorded, and here is
        // what happened in it" rather than as two rows of marks.
        for event in &playback.events {
            let (from, to) = (event.0.max(start), event.1.min(end));
            if to <= from {
                continue;
            }
            painter.rect_filled(
                band(along(from), along(to), BAND),
                CornerRadius::same(2),
                theme::WARN.gamma_multiply(0.85),
            );
        }
        // The ruler along the strip's upper edge, and the times over it. A tick
        // stands at every moment a label would name and a shorter one between
        // two of them, so the strip itself says where a moment falls rather
        // than the label having to be written there - and the eye can carry a
        // time from the top down to the band without guessing. On a turned
        // picture the edge is the row's right side and the ticks stand out of
        // it, and the times run down the screen turned with the picture.
        //
        // How many of them fit decides the step, so a wide screen reads a time
        // every minute where a phone reads one every ten: the same recording is
        // as finely divided as the room it is drawn in allows. And a step under
        // a minute has to name the second - which is a wider label, so the step
        // is chosen a second time at that width.
        let length = if horizontal { row.width() } else { row.height() };
        let wide = |text: &str| {
            painter
                .layout_no_wrap(text.to_owned(), egui::FontId::proportional(TIME_SIZE), theme::GRID_LINE)
                .size()
                .x
        };
        let fits = |text: &str| labels_across(length, wide(text), LABEL_GAP);
        let mut step = tick_step(playback.zoom, fits("00:00"));
        if step < 60 {
            step = tick_step(playback.zoom, fits("00:00:00"));
        }
        let seconds = step < 60;
        let minor = ruler_step(step);
        let edge = if horizontal { track - BAND / 2.0 } else { track + BAND / 2.0 };
        let out = if horizontal { -1.0 } else { 1.0 };
        let mut at = start.div_euclid(minor) * minor;
        while at < end {
            if at >= start {
                let long = at.rem_euclid(step) == 0;
                let mark = along(at);
                let tip = edge + out * if long { TICK } else { TICK_MINOR };
                let shade = if long { 0.8 } else { 0.45 };
                painter.line_segment(
                    tick_ends(horizontal, mark, edge, tip),
                    egui::Stroke::new(1.0_f32, theme::GRID_LINE.gamma_multiply(shade)),
                );
                if long {
                    if let Some(label) = clock_of(at, seconds) {
                        let spot = if horizontal {
                            egui::pos2(mark, across(TIME_ROW))
                        } else {
                            egui::pos2(across(TIME_ROW), mark)
                        };
                        turned_text(
                            painter,
                            spot,
                            label,
                            TIME_SIZE,
                            theme::GRID_LINE.gamma_multiply(0.8),
                            !horizontal,
                        );
                    }
                }
            }
            at += minor;
        }
        // The moment under the cursor, in full, where the eye already is.
        if let Some(stamp) = stamp_of(centre) {
            let spot = if horizontal {
                egui::pos2(row.center().x, across(0.83))
            } else {
                egui::pos2(across(0.83), row.center().y)
            };
            turned_text(painter, spot, stamp, 12.0, theme::TEXT_BRIGHT, !horizontal);
        }
        // The cursor: fixed in the middle, from the times down through the strip.
        let cursor = if horizontal {
            Rect::from_two_pos(
                egui::pos2(row.center().x - 1.0, across(0.25)),
                egui::pos2(row.center().x + 1.0, across(0.60)),
            )
        } else {
            Rect::from_two_pos(
                egui::pos2(across(0.25), row.center().y - 1.0),
                egui::pos2(across(0.60), row.center().y + 1.0),
            )
        };
        painter.rect_filled(cursor, CornerRadius::same(1), theme::FOCUS);
    }
}

impl XgViewApp {
    /// Draws the calendar: one button per day, filled where the camera recorded
    /// and dark where it did not, with the day's name inside it.
    ///
    /// The plan's rule is 44 points a day, which is a touch target. A row that
    /// cannot hold them all slides: the days are laid out end to end and the row
    /// is a window on them, so what is off its ends is clipped by its edge rather
    /// than drawn over what is beside it.
    ///
    /// The day the picture is on is ringed: picking a date is a move to another
    /// one, and the date already showing is the way back to the strip.
    fn draw_days(&self, ui: &egui::Ui, row: Rect, playback: &Playback) {
        let (each, total) = day_metrics(row, playback.days.len());
        if each <= 0.0 {
            return;
        }
        let horizontal = row.width() > row.height();
        let length = if horizontal { row.width() } else { row.height() };
        let scroll = day_scroll_to(playback.days_scroll, total, length);
        let painter = ui.painter().with_clip_rect(row);
        // Where the picture is, so the day it is on can be the one that looks
        // chosen: a calendar with no mark on it does not say which date the
        // viewer is already looking at.
        let now = self.centre_of(playback);
        let here = playback.days.iter().rev().find(|day| day.start <= now).map(|day| day.start);
        for (index, day) in playback.days.iter().enumerate() {
            let from = index as f32 * each - scroll + DAY_GAP / 2.0;
            let to = (from + each - DAY_GAP).max(from + 1.0);
            if to <= 0.0 || from >= length {
                continue;
            }
            let band = if horizontal {
                Rect::from_min_max(
                    egui::pos2(row.left() + from, row.top() + DAY_PAD),
                    egui::pos2(row.left() + to, row.bottom() - DAY_PAD),
                )
            } else {
                Rect::from_min_max(
                    egui::pos2(row.left() + DAY_PAD, row.top() + from),
                    egui::pos2(row.right() - DAY_PAD, row.top() + to),
                )
            };
            // A day the camera recorded on is a button; the days it did not are
            // not here at all, so every one of them is filled.
            painter.rect_filled(band, CornerRadius::same(6), theme::ACCENT.gamma_multiply(0.45));
            if here == Some(day.start) {
                painter.rect_stroke(
                    band,
                    CornerRadius::same(6),
                    egui::Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS),
                    StrokeKind::Inside,
                );
            }
            if each >= 34.0 {
                painter.text(
                    band.center(),
                    egui::Align2::CENTER_CENTER,
                    &day.label,
                    egui::FontId::proportional(DAY_SIZE),
                    theme::TEXT_BRIGHT,
                );
            }
        }
    }

    /// Brings the day the picture is on into the middle of the calendar, once,
    /// when it is first shown with a list to show.
    ///
    /// A calendar slid away from that day cannot answer "which date am I on", and
    /// the ring around the day is the answer - so showing the calendar brings it
    /// into view rather than leaving the row wherever the last look left it.
    fn follow_days(&mut self, row: Rect) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if !playback.days_follow || playback.days.is_empty() {
            return;
        }
        let (each, total) = day_metrics(row, playback.days.len());
        if each <= 0.0 {
            return;
        }
        let length = if row.width() > row.height() { row.width() } else { row.height() };
        let now = self.centre_of(playback);
        let index = playback.days.iter().rposition(|day| day.start <= now).unwrap_or(0);
        let scroll = day_scroll_to(index as f32 * each - (length - each) / 2.0, total, length);
        if let Some(playback) = self.playback.as_mut() {
            playback.days_scroll = scroll;
            playback.days_follow = false;
        }
    }
}

impl XgViewApp {
    /// Slides the calendar a day under the arrows, and no further than it goes.
    ///
    /// A day a press, and not a day a frame: a held arrow is not repeated into a
    /// flood, and the row is twenty-one days long - which is a press each.
    fn slide_days(&mut self, row: Rect, forward: bool) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let (each, total) = day_metrics(row, playback.days.len());
        if each <= 0.0 {
            return;
        }
        let length = if row.width() > row.height() { row.width() } else { row.height() };
        let step = if forward { each } else { -each };
        let scroll = day_scroll_to(playback.days_scroll + step, total, length);
        if let Some(playback) = self.playback.as_mut() {
            playback.days_scroll = scroll;
        }
    }

    /// Answers a finger on the timeline.
    ///
    /// Pressing holds the picture where it is - a recording is paced by its own
    /// timestamps, so that is the only way to stop it - and the cursor follows
    /// the finger with nothing asked of the server. Letting go asks for the
    /// stretch under the finger and plays it from there.
    pub(super) fn timeline_input(&mut self, ui: &egui::Ui, row: Rect) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let index = playback.index;
        let days = matches!(playback.timeline, Timeline::Days);
        let (start, end) = self.window_of(playback);
        let centre = self.centre_of(playback);
        let span = (end - start).max(1) as f32;
        let horizontal = row.width() > row.height();
        let along = |pos: egui::Pos2| -> f32 {
            let t = if horizontal {
                (pos.x - row.left()) / row.width().max(1.0)
            } else {
                (pos.y - row.top()) / row.height().max(1.0)
            };
            t.clamp(0.0, 1.0)
        };

        // Claimed so the wall behind does not take the press for a page swipe.
        let response = ui.interact(row, Id::new(("xgview-timeline", index)), Sense::click_and_drag());
        // The strip is a slider for a remote: left and right are its own, Down
        // walks into it from the controls and Up walks back out. Those keys are
        // read here rather than through the navigation layer because the strip
        // is not one of the bar's controls - it is the thing they operate.
        let (down_key, up_key, left, right, left_press, right_press) = ui.ctx().input(|input| {
            (
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::ArrowUp),
                input.key_down(egui::Key::ArrowLeft),
                input.key_down(egui::Key::ArrowRight),
                input.key_pressed(egui::Key::ArrowLeft),
                input.key_pressed(egui::Key::ArrowRight),
            )
        });
        if !response.has_focus()
            && down_key
            && ui.memory(|memory| memory.focused()).is_some_and(tilebar::owns)
        {
            response.request_focus();
        }
        if response.has_focus() {
            if up_key {
                ui.memory_mut(|memory| memory.request_focus(tilebar::control(0)));
            } else if days {
                // The calendar slides under the arrows: a remote has no finger,
                // and a day it cannot reach is a day that is not there.
                if left_press {
                    self.slide_days(row, false);
                } else if right_press {
                    self.slide_days(row, true);
                }
            } else if left || right {
                self.walk_timeline(right);
            } else if let Some(playback) = self.playback.as_mut() {
                playback.held = None;
            }
        }

        // Two fingers spreading over the strip are the strip's, and they zoom
        // it: a pinch is a width, not a place, so nothing moves along it. The
        // picture keeps the window it has - the wall leaves this gesture to the
        // strip when it is made over the row. See [`XgViewApp::timeline_row`].
        let pinched = ui.ctx().input(|input| pinch_frame(input, row));
        if let Some((spread, _)) = pinched {
            self.pinch_timeline(spread);
        } else if let Some(playback) = self.playback.as_mut() {
            playback.pinch = 1.0;
        }

        // A scrub is the left button's and a touch's. The right one is the
        // mouse's way to the widths, and reading it here as a press would have
        // it start a drag on the way.
        let down = ui.ctx().input(|input| input.pointer.button_down(egui::PointerButton::Primary));
        let pos = ui.ctx().input(|input| input.pointer.interact_pos());
        let scrubbing = self.playback.as_ref().is_some_and(|playback| playback.scrubbing);
        if response.secondary_clicked() && !scrubbing {
            self.cycle_zoom();
            return;
        }

        // The calendar is a row of dates, not a stretch: a finger slides it, and
        // a press that goes nowhere picks the day under it. The two cannot be
        // told apart until the finger lifts, which is why a press alone picks
        // nothing and nothing is held while it is down.
        if days {
            self.follow_days(row);
            let Some(playback) = self.playback.as_ref() else {
                return;
            };
            let (each, total) = day_metrics(row, playback.days.len());
            if each <= 0.0 {
                return;
            }
            let length = if horizontal { row.width() } else { row.height() };
            if down {
                let Some(pos) = pos else {
                    return;
                };
                let at = day_along(row, pos);
                if let Some((from, scroll)) = playback.days_drag {
                    // A finger that leaves the row keeps sliding it: the row is
                    // a window on the days, and the finger is still pointing at
                    // a place in them.
                    let scroll = day_scroll_to(scroll + (from - at), total, length);
                    if let Some(playback) = self.playback.as_mut() {
                        playback.days_scroll = scroll;
                    }
                } else if row.contains(pos) {
                    if let Some(playback) = self.playback.as_mut() {
                        playback.days_drag = Some((at, playback.days_scroll));
                    }
                }
            } else {
                // Lifted, or gone: a slide is finished with, and a press that
                // did not move is the day under the finger.
                let Some((from, scroll)) = self
                    .playback
                    .as_mut()
                    .and_then(|playback| playback.days_drag.take())
                else {
                    return;
                };
                let Some(pos) = pos else {
                    return;
                };
                if (day_along(row, pos) - from).abs() > DAY_SLOP {
                    return;
                }
                let Some(playback) = self.playback.as_ref() else {
                    return;
                };
                // Cloned: picking borrows the application mutably, and the day
                // it is picking belongs to the list borrowed here.
                let picked = day_at(row, &playback.days, pos, scroll, each).cloned();
                if let Some(day) = picked {
                    self.pick_day(index, &day);
                }
            }
            return;
        }

        if !scrubbing {
            // A press that starts on the strip holds the picture and takes the
            // strip with it. One that starts elsewhere is the picture's, and
            // does nothing here. A pinch is neither: it is a width, and it was
            // answered above.
            if pinched.is_none() {
                if let (true, Some(pos)) = (down, pos) {
                    if row.contains(pos) {
                        self.manager.hold_clip(index, true);
                        response.request_focus();
                        self.fetch_for(index, centre);
                        if let Some(playback) = self.playback.as_mut() {
                            playback.scrubbing = true;
                            playback.drag = Some((along(pos), centre));
                            playback.scrub = Some(centre);
                            // The range the last drag read is not this drag's.
                            playback.ahead = None;
                            playback.awaiting = None;
                        }
                    }
                }
            }
            return;
        }

        // A pinch that began after a finger was already down is still a width,
        // not a place: the strip waits where it was until the fingers are gone.
        if pinched.is_some() {
            return;
        }
        match (down, pos) {
            // The finger may leave the strip; what it has moved keeps going to
            // the end rather than stopping where the strip does.
            (true, Some(pos)) => {
                let Some((from, anchor)) =
                    self.playback.as_ref().and_then(|playback| playback.drag)
                else {
                    return;
                };
                // Dragging the strip to the left brings later moments under the
                // cursor: the cursor is the one thing that does not move.
                let at = anchor + ((from - along(pos)) * span) as i64;
                let at = self.clamp_centre(at);
                if self.playback.as_ref().is_some_and(|playback| playback.scrub != Some(at)) {
                    self.fetch_for(index, at);
                    if let Some(playback) = self.playback.as_mut() {
                        playback.scrub = Some(at);
                    }
                    // The strip is drawn a whole width around the cursor, so the
                    // list has to reach half a width behind it - and it is asked
                    // for now, while the finger is still down, rather than when
                    // the strip has already run out of what was read and shows
                    // nothing on its left.
                    self.read_older(index);
                }
            }
            _ => self.end_scrub(),
        }
    }

    /// Lets the picture go and plays from wherever the finger was left.
    fn end_scrub(&mut self) {
        let Some((at, index)) = self.playback.as_mut().map(|playback| {
            playback.scrubbing = false;
            (playback.scrub.take(), playback.index)
        }) else {
            return;
        };
        let Some(at) = at else {
            // Nothing under the finger: the recording goes on where it was held.
            self.release_hold(index);
            return;
        };
        // Held to the recording once more: a read may have answered while the
        // finger was down and said where the beginning is, and what is read
        // below is read for where the cursor will actually be.
        let at = self.clamp_centre(at);
        // A tap that never moved the strip is not a seek: asking for the moment
        // already on screen would drop the open response and fetch the fragment
        // again for a picture that is already there.
        let centre = self.playback.as_ref().map_or(at, |playback| self.centre_of(playback));
        if (at - centre).abs() < 1 {
            self.release_hold(index);
            return;
        }
        // A drag that has taken the strip somewhere the event list does not
        // reach reads the events there: the marks on the strip and the two
        // buttons that step between them are the same list. The stretch asked
        // for is the one the strip will show once the cursor is there - not the
        // one it shows now, which is what `read_events` would ask for, the
        // picture not having moved yet.
        let zoom = self.playback.as_ref().map_or(SPAN, |playback| playback.zoom);
        let covered = self
            .playback
            .as_ref()
            .and_then(|playback| playback.events.first().zip(playback.events.last()))
            .is_some_and(|(first, last)| first.0 <= at && at <= last.1);
        if !covered {
            self.read_events_between(index, at - zoom / 2, at + zoom / 2);
        }
        // The hold is not let go of first: a session resumed for a moment
        // publishes a frame of the window that is being left, and that frame is
        // the one that would put the picture back where it was. A seek either
        // jumps inside the session - and lets go of the hold itself, with the
        // jump already in the session's hand - or replaces it, and a replaced
        // session ends the hold with it.
        //
        // Through `move_to`, so a picture paused before the finger came down is
        // still paused after it: a scrub says where to look, not that it should
        // start playing.
        self.move_to(at);
    }

    /// Lets go of the drag's hold - and keeps it, if the picture was held before
    /// the finger came down.
    ///
    /// A tap on the strip that moved nothing is not a request to play: a viewer
    /// who paused and then touched the strip meant to look at it.
    fn release_hold(&mut self, index: usize) {
        let paused = self.playback.as_ref().is_some_and(|playback| playback.paused);
        self.manager.hold_clip(index, paused);
    }

    /// More detail on the timeline, or more of it.
    ///
    /// The strip is drawn around the cursor, so a width changes what is on
    /// either side of it and not where it is: a wider level reaches further back
    /// and a narrower one reaches less far. The ladder is fixed and its ends are
    /// ends - two minutes is as close as it goes, a day as wide - because the
    /// two buttons are a pair and a press past either would have to mean "back
    /// to the other end". A right press on the strip is the one that goes round;
    /// see [`Self::cycle_zoom`].
    pub(super) fn zoom_timeline(&mut self, finer: bool) {
        let Some(zoom) = self
            .playback
            .as_ref()
            .and_then(|playback| step_zoom(playback.zoom, finer))
        else {
            return;
        };
        self.set_zoom(zoom);
    }

    /// A right press on the strip: the mouse's way to the widths.
    ///
    /// Steps finer, and starts again at the widest at the end of the ladder. The
    /// two buttons on the bar are a pair and their ends are ends - pressing "more
    /// detail" again must not mean "back to the coarsest" - but a right press has
    /// no partner, so it is the one control that goes round.
    fn cycle_zoom(&mut self) {
        let Some(zoom) = self.playback.as_ref().map(|playback| {
            step_zoom(playback.zoom, true).unwrap_or(*ZOOMS.last().expect("a ladder of widths"))
        }) else {
            return;
        };
        self.set_zoom(zoom);
    }

    /// Draws the strip at a width, and reads what that width needs.
    fn set_zoom(&mut self, zoom: i64) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        if playback.timeline == Timeline::Days {
            return;
        }
        playback.zoom = zoom;
        // A strip of a different width holds events the old list does not.
        playback.events.clear();
        let index = playback.index;
        tracing::debug!(target: "xgview::app", zoom, "the timeline changed width");
        self.read_events(index);
        self.read_older(index);
    }

    /// Shows the days, or goes back to the stretch of one.
    ///
    /// The calendar is not a width of the strip - it is a different picture of
    /// the recording - so it has its own control rather than the end of the
    /// ladder. Showing it changes nothing: the width is the one it was, the
    /// events are the ones they were, and the recording has been playing all
    /// along. Coming back is therefore the same non-change, which is what makes
    /// the button a look at the dates rather than a move.
    pub(super) fn toggle_calendar(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let index = playback.index;
        match playback.timeline {
            Timeline::Span => {
                playback.timeline = Timeline::Days;
                // Twenty-one days do not fit on a phone, so the row is slid: it
                // opens on the day the picture is on rather than on whichever
                // end the last look left it at.
                playback.days_follow = true;
                self.read_days(index);
            }
            // Back to the strip exactly as it was left: nothing to restore,
            // because nothing was changed to show the dates. A pick that is
            // still being read is dropped with it - the button that showed the
            // calendar is the same one that leaves it.
            Timeline::Days => {
                playback.timeline = Timeline::Span;
                playback.to_day = None;
            }
        }
    }

    /// Plays faster, or back to normal: one, two, four, one.
    pub(super) fn cycle_speed(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let speed = if playback.speed < 1.5 {
            2.0
        } else if playback.speed < 3.0 {
            4.0
        } else {
            1.0
        };
        playback.speed = speed;
        let index = playback.index;
        self.manager.set_rate(index, speed);
        tracing::debug!(target: "xgview::app", index, speed, "playback rate");
    }

    /// The word the speed control carries: `1×`, `2×` or `4×`.
    pub(super) fn playback_speed_label(&self) -> &'static str {
        match self.playback.as_ref().map(|playback| playback.speed) {
            Some(speed) if speed > 3.0 => "4×",
            Some(speed) if speed > 1.5 => "2×",
            _ => "1×",
        }
    }

    /// Spreads the strip, or closes it, as two fingers ask.
    ///
    /// A pinch arrives as a ratio per frame rather than as a distance, so what is
    /// remembered is the product: a step of the ladder is taken when the fingers
    /// have spread or closed by [`PINCH_STEP`], and the rest is kept for the step
    /// after.
    fn pinch_timeline(&mut self, spread: f32) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let (step, pinch) = pinch_step(playback.pinch, spread);
        playback.pinch = pinch;
        if let Some(finer) = step {
            self.zoom_timeline(finer);
        }
    }

    /// Walks the strip under the cursor, a step at a time.
    ///
    /// The step is a twentieth of what the strip is showing, so it is a few
    /// seconds at the closest width and an hour at the widest - the arrow means
    /// "a little further" at every level. Held down it accelerates: four times
    /// the step after half a second, sixteen after one and a half.
    pub(super) fn walk_timeline(&mut self, forward: bool) {
        let now = std::time::Instant::now();
        let step = {
            let Some(playback) = self.playback.as_mut() else {
                return;
            };
            let base = (playback.zoom / 20).max(1);
            match playback.held {
                Some((direction, since)) if direction == forward => {
                    let held = now.duration_since(since).as_secs_f32();
                    if held > 1.5 {
                        base * 16
                    } else if held > 0.6 {
                        base * 4
                    } else {
                        base
                    }
                }
                _ => {
                    playback.held = Some((forward, now));
                    base
                }
            }
        };
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let at = self.centre_of(playback) + if forward { step } else { -step };
        self.move_to(at);
    }

    /// Takes the picture to a moment, keeping the hold it was under.
    fn move_to(&mut self, at: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let (index, paused) = (playback.index, playback.paused);
        let at = self.clamp_centre(at);
        self.seek(at);
        if paused {
            self.manager.hold_clip(index, true);
            if let Some(playback) = self.playback.as_mut() {
                playback.paused = true;
            }
        }
    }

    /// A moment kept inside what the camera recorded: the cursor cannot be
    /// dragged off the end of the strip into nothing.
    ///
    /// The far end is the newest moment there is. The near end is the oldest
    /// moment the camera *has* - `oldest`, once a read has said where the
    /// recording begins - and not the first segment of the list: the list is
    /// what has been asked for, and a drag that stopped at the end of it would
    /// be a strip that could not be pushed back into a history nobody had read
    /// yet. Until a read has said where the beginning is, there is no bound at
    /// all, and the reads follow the finger.
    fn clamp_centre(&self, at: i64) -> i64 {
        let Some(playback) = self.playback.as_ref() else {
            return at;
        };
        let Some(last) = playback.segments.last() else {
            return at;
        };
        clamp_to(at, playback.oldest, last.end)
    }

    /// Fetches what a seek to `at` will need, while there is time to fetch it.
    fn fetch_for(&mut self, index: usize, at: i64) {
        let inside = self.playback.as_ref().is_some_and(|playback| covered(playback.vod, at));
        if inside {
            self.manager.prefetch_fragment(index, at);
        } else {
            self.read_ahead(index, at);
        }
    }

    /// Asks the box which days it recorded on, the first time the calendar is
    /// shown. One answer per recording: a day does not appear or disappear while
    /// the viewer is looking at it.
    fn read_days(&mut self, index: usize) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.reading_days || !playback.days.is_empty() {
            return;
        }
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, "reading the recorded days");
        if let Some(playback) = self.playback.as_mut() {
            playback.reading_days = true;
        }
        start_days(&self.handle, &self.events_tx, index, source);
    }

    /// Takes the days the box recorded on, and draws the calendar from them.
    pub(super) fn days_arrived(
        &mut self,
        index: usize,
        camera_id: String,
        days: Option<Vec<String>>,
    ) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        if playback.index != index || playback.source.camera != camera_id {
            return;
        }
        playback.reading_days = false;
        let Some(days) = days else {
            return;
        };
        let recorded: HashSet<String> = days.into_iter().collect();
        playback.days = calendar(&local_at, epoch_now(), &recorded);
    }

    /// Goes to a day the calendar shows.
    ///
    /// **The time of day is kept**: a date is a place in the recording, and a
    /// viewer picking one is asking for the same moment a day earlier or later -
    /// the hour, the minute, the second they were watching. Every day's start is
    /// found by the same walk through the local clock, so the distance into one
    /// day is the same time of day in another.
    ///
    /// The day the picture is already on is not a move at all: it is the way out
    /// of the calendar, and it leaves the strip showing what it was showing.
    ///
    /// Any other day is read before it is gone to - what is held is an hour
    /// around where the viewer was, and a seek can only land where the list
    /// reaches - and the strip moves there when the answer arrives. See
    /// [`Self::segments_arrived`].
    fn pick_day(&mut self, index: usize, day: &Day) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.to_day.is_some() {
            return;
        }
        let now = self.centre_of(playback);
        let Some(at) = day_target(&local_at, &playback.days, now, day) else {
            // The date already showing: back to the strip, with nothing asked of
            // the box and nothing moved.
            tracing::debug!(target: "xgview::app", index, "the day already showing");
            if let Some(playback) = self.playback.as_mut() {
                playback.timeline = Timeline::Span;
            }
            return;
        };
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, at, "going to a day");
        if let Some(playback) = self.playback.as_mut() {
            playback.to_day = Some(at);
        }
        // The whole day, and an hour before it: the strip that follows keeps the
        // width it had, and a day is not always a day. The moment asked for is
        // somewhere in it - `now - here` is at most the length of a day.
        start_segments(&self.handle, &self.events_tx, index, source, day.start - 3600, day.start + 90_000);
    }

    /// Reads the segment list around a moment the list does not reach.
    ///
    /// One stretch either side of it, which is what a recording is opened with:
    /// a seek that lands outside the list - a jump, a step, a drag let go of
    /// there - reads before it moves, and comes back through `to_seek` once the
    /// answer has merged. A read that is already in flight is not asked again.
    fn read_around(&mut self, index: usize, at: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.reading_older.is_some() {
            return;
        }
        // At least a recording's opening read either side, and at least as wide
        // as the strip: a drag across a day's strip should not be one read per
        // finger-width.
        let span = playback.zoom.max(2 * LOOKBACK);
        let (from, to) = (at - span / 2, at + span / 2);
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, at, "reading the list around a moment");
        if let Some(playback) = self.playback.as_mut() {
            playback.reading_older = Some((from, to));
        }
        start_segments(&self.handle, &self.events_tx, index, source, from, to);
    }

    /// Reads the stretch of segment list a wider strip needs. See
    /// [`older_window`].
    ///
    /// One request for the whole missing stretch rather than one per level: a
    /// day of segments is 1.7 MB and 744 ms, and asking for it a level at a time
    /// would be the same bytes in five requests.
    fn read_older(&mut self, index: usize) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // One at a time: the answer widens the strip, which may then want more.
        if playback.reading_older.is_some() {
            return;
        }
        let Some((from, to)) = older_window(
            &playback.segments,
            playback.zoom,
            self.centre_of(playback),
            playback.oldest,
        ) else {
            return;
        };
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, from, to, "reading older segments");
        // Marked before the task is spawned: an answer that arrives before the
        // mark would clear a field that had not been set yet.
        if let Some(playback) = self.playback.as_mut() {
            playback.reading_older = Some((from, to));
        }
        start_segments(&self.handle, &self.events_tx, index, source, from, to);
    }

    /// Takes the segment list read for a wider strip.
    ///
    /// Nothing moves by itself: the strip widens to what the list now reaches,
    /// and a level still wider asks for the rest. An answer that could not be
    /// read is not an answer that there is nothing there: nothing is marked, and
    /// the next width the viewer chooses asks again.
    pub(super) fn segments_arrived(
        &mut self,
        index: usize,
        camera_id: String,
        from: i64,
        to: i64,
        segments: Option<Vec<Segment>>,
    ) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        if playback.index != index || playback.source.camera != camera_id {
            return;
        }
        playback.reading_older = None;
        // The read a picked day asked for, if this is it: whether it found
        // anything or not, the picking is finished with - a day with nothing
        // recorded leaves the calendar where it is rather than waiting for an
        // answer that will never come.
        let picked = playback.to_day.filter(|at| from <= *at && *at < to);
        if picked.is_some() {
            // Finished with either way: a read that could not be made must not
            // leave the calendar waiting for a day it will never reach.
            playback.to_day = None;
        }
        let Some(segments) = segments else {
            // The camera could not be asked. Nothing is marked, so the next
            // width the viewer chooses asks again - and a failed read is not
            // read as a stretch with nothing in it.
            return;
        };
        if segments.is_empty() {
            if picked.is_none() {
                // Nothing was recorded that far back. That is where the
                // recording begins only when the read covered the beginning of
                // the list: a stretch read around a moment that is nowhere near
                // it says nothing about the gap between the two, and marking it
                // as the end would put a wall there that the viewer cannot see.
                let begins = playback
                    .segments
                    .first()
                    .map(|first| first.start)
                    .filter(|start| from <= *start && *start <= to);
                if let Some(begins) = begins {
                    playback.oldest = Some(begins);
                }
            }
            // A seek waiting for a list that came back empty is finished with:
            // the recording is not there, and the picture stays where it is
            // rather than the cursor waiting for a moment that will never come.
            playback.to_seek = None;
            playback.pending = None;
            if picked.is_some() {
                // A day with nothing on it: the calendar stays where it is and
                // says so, rather than reading as a press that missed.
                self.flash(monitor_i18n::tr("toast-no-recording"), ToastKind::Error);
            }
            return;
        }
        playback.segments.extend(segments);
        playback.segments.sort_by_key(|segment| segment.start);
        playback.segments.dedup_by_key(|segment| segment.start);
        // A seek was waiting for the list to reach it: the picture goes there
        // now, which is the same path a drag takes when the list already holds
        // it.
        if let Some(at) = playback.to_seek {
            let reached = playback
                .segments
                .iter()
                .any(|segment| segment.start <= at && at < segment.end);
            if reached {
                playback.to_seek = None;
                self.seek(at);
                return;
            }
        }
        // A day was picked, and this is the list that reaches it: the strip
        // becomes that day, and the picture goes to the moment asked for.
        if let Some(at) = picked {
            // The day's list reaches the moment asked for: the strip goes there
            // at the width it had, and the picture to that moment - or to the
            // nearest recorded one, when the recording has a hole where that
            // time of day used to be.
            if let Some(target) = nearest(&stretches(&playback.segments), at) {
                playback.timeline = Timeline::Span;
                self.seek(target);
                // The marks are the new day's, and the strip is centred on the
                // moment it went to, so half of it is the day before - which was
                // not part of this read either.
                self.read_events(index);
                self.read_older(index);
            }
            return;
        }
        // A wider strip may want more than this one answered.
        self.read_older(index);
    }

    /// Reads the range a finger is over while it is still on the timeline.
    ///
    /// A drag that leaves the range the session holds will need another
    /// playlist. Asked for once the finger is lifted, that request sits between
    /// the viewer and the picture; asked for now, it is answered by then.
    /// Nothing moves: the answer waits for a seek to want it.
    ///
    /// One range at a time. A finger that keeps moving asks again only once it
    /// has left the range it already asked for, so a sweep costs one request per
    /// window of travel rather than one per frame.
    fn read_ahead(&mut self, index: usize, at: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.ahead.as_ref().is_some_and(|ahead| covered(Some(ahead.window), at)) {
            return;
        }
        let width = RANGE_CAP;
        let Some(window) = window_at(
            &playback.segments,
            range_start(&playback.segments, at, width / 2),
            width,
        ) else {
            return;
        };
        // A window that does not hold the moment is the list's nearest stretch,
        // not the moment's own: the finger is over a stretch nobody has read
        // yet, and the segment read that follows it is what will name the
        // playlist. Asked for every frame, this would be a request a frame for a
        // range the finger has already left.
        if !covered(Some(window), at) {
            return;
        }
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
        // A moment the list does not reach is not a refusal, it is a read: a
        // drag or a jump goes anywhere the recording does, and the list is what
        // has been asked for so far. Asking for the stretch around it and coming
        // back is what makes the strip as long as the recording rather than as
        // long as the last read.
        let reach = playback
            .segments
            .first()
            .zip(playback.segments.last())
            .map(|(first, last)| (first.start, last.end));
        if reach.is_some_and(|(first, last)| at < first || at > last) {
            self.read_around(index, at);
            if let Some(playback) = self.playback.as_mut() {
                playback.to_seek = Some(at);
                // The cursor waits where the finger left it, as it does for a
                // seek whose playlist is on its way: what is on screen is still
                // the stretch being left.
                playback.pending = Some(at);
            }
            return;
        }
        // Neither of these can happen while the list is the one the window was
        // built from - and if it does, saying so is better than a picture that
        // silently carries on where it was, which reads as a drag that was
        // refused.
        let Some(target) = nearest(&stretches(&playback.segments), at) else {
            self.manager.hold_clip(index, false);
            if let Some(playback) = self.playback.as_mut() {
                playback.paused = false;
            }
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
                playback.paused = false;
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
        // The range is as wide as the box will serve, centred on where the seek
        // is going: the strip slides, and a drag that left the range every ten
        // minutes would be a session every ten minutes.
        let width = RANGE_CAP;
        let Some(window) = window_at(
            &playback.segments,
            range_start(&playback.segments, target, width / 2),
            width,
        ) else {
            self.manager.hold_clip(index, false);
            if let Some(playback) = self.playback.as_mut() {
                playback.paused = false;
            }
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
/// Five of the plan's six levels: two minutes, ten, an hour, six hours, a day.
/// Ten is what a recording opens at - what a scrub can be aimed at across a
/// strip this wide - and the wider ones are for finding something older: the
/// strip ends at the moment being watched, so a wider one reaches further back.
/// The sixth, a day per row, is the plan's calendar: a different picture rather
/// than a wider strip.
const ZOOMS: [i64; 5] = [120, 600, 3600, 21600, 86400];

/// How much of a recording the timeline shows when a recording is opened.
const SPAN: i64 = 600;

/// How wide a range a playlist is asked for: the widest the box will serve,
/// whatever the strip is showing.
///
/// The strip and the range used to be the same width, because a seek was a
/// pointer press and what could be pointed at was what one session had to hold.
/// The strip slides now, so the range is not "what can be pointed at" - it is
/// **how far a drag can go before it costs a session**, and that should be as far
/// as the box allows rather than as wide as the window happens to be. A strip
/// drawn wider than this still shows more than one range, and a drag there is
/// still a session; that is the box's limit, not the interface's.
///
/// Past this the box stops answering: measured on the box of 2026-11-02, a range
/// of 2.5 hours is served and one of 3 hours is `503`.
const RANGE_CAP: i64 = 7200;

/// The next width up or down the ladder, or `None` at an end of it.
///
/// The ends are the ends: two buttons that wrapped would make "one step more
/// detail" mean "back to the coarsest", which is the one thing a viewer pressing
/// it again does not want.
fn step_zoom(zoom: i64, finer: bool) -> Option<i64> {
    if finer {
        ZOOMS.iter().copied().rev().find(|width| *width < zoom)
    } else {
        ZOOMS.iter().copied().find(|width| *width > zoom)
    }
}

/// A day, in seconds: as far back as a jump looks for an event when the list it
/// holds has none.
const DAY: i64 = 86_400;

/// How far two fingers have to spread over the strip to move it a step of the
/// ladder. A pinch arrives as a factor per frame, and this is what those factors
/// are multiplied up to before one is taken.
const PINCH_STEP: f32 = 1.6;

/// What a frame of a pinch asks for: a step of the ladder, or nothing, and what
/// is left of the spread to carry into the next frame.
///
/// A pinch arrives as a ratio per frame rather than as a distance, so the ratios
/// multiply: the ladder moves once the fingers have spread or closed by
/// [`PINCH_STEP`] since the last step, and the rest is kept.
fn pinch_step(pinch: f32, spread: f32) -> (Option<bool>, f32) {
    let pinch = pinch * spread;
    if pinch > PINCH_STEP {
        (Some(true), pinch / PINCH_STEP)
    } else if pinch < 1.0 / PINCH_STEP {
        (Some(false), pinch * PINCH_STEP)
    } else {
        (None, pinch)
    }
}

/// How far the two step controls move the picture.
///
/// Ten seconds, the same as a double tap on a phone's video player: long enough
/// to skip a pause in what is being watched, short enough that a viewer who
/// presses it twice is still where they meant to be.
pub(super) const STEP: i64 = 10;

/// How far back the calendar will walk looking for the first day the camera
/// recorded on, in hours.
///
/// Three years. A camera whose oldest recording is older than that is a clock
/// that is wrong rather than an archive, and the walk is an hour at a time: a
/// bound is what keeps a wrong clock from walking to the beginning of time
/// inside a frame.
const CALENDAR_LIMIT: i64 = 3 * 365 * 24;

/// How far a day's button is inset from the row's edges, and how much of a gap
/// is left between two of them.
///
/// The row is `tilebar::TIMELINE_ROW` deep, so a button is that less twice the
/// inset: deep enough to read a date in, and still a row of buttons rather than
/// a bar with marks on it.
const DAY_PAD: f32 = 7.0;
/// See [`DAY_PAD`].
const DAY_GAP: f32 = 3.0;
/// The size of a day's name.
const DAY_SIZE: f32 = 10.0;

/// How far a finger may move on the calendar and still be a press rather than a
/// slide. A few points, which is what a touch screen's own slop is: below it a
/// finger that meant to press has not moved, and above it one that meant to
/// slide has.
const DAY_SLOP: f32 = 4.0;

/// The moment on a picked day whose clock reads what the moment on screen does.
///
/// The distance into the day is added as seconds, which is right on every day
/// that is 86 400 of them and an hour out on the two that are not: a clock that
/// goes back at two in the morning makes the same distance land at half past one,
/// and one that goes forward makes it land at half past three. So the clock is
/// read at the answer and the difference taken out - **once**, because an hour is
/// all a day can be out by, and because a moment a clock skipped has no moment of
/// its own to be taken to.
///
/// A platform with no clock falls back to the distance in seconds, which is what
/// a day that is a day long wants anyway.
fn same_clock(clock: &dyn Fn(i64) -> Option<LocalTime>, picked: &Day, now: i64, here: i64) -> i64 {
    let at = picked.start + (now - here);
    let seconds = |local: LocalTime| {
        local.hour as i64 * 3600 + local.minute as i64 * 60 + local.second as i64
    };
    let (Some(want), Some(got)) = (clock(now), clock(at)) else {
        return at;
    };
    at + (seconds(want) - seconds(got))
}

/// The moment a picked day is asked for: the same time of day as the moment on
/// screen.
///
/// The day that moment is in comes from the starts the calendar holds, and the
/// distance into it is carried to the picked day - see [`same_clock`], which is
/// where the hour a clock gains or loses is taken out. That works because every
/// day's start is found by the same walk through the local clock, which lands on
/// the same second of the hour each day: `now - here` is therefore a time of day
/// and not an offset from a midnight that was never found.
///
/// `None` when the two are the same day: picking the date already showing is the
/// way out of the calendar, not a seek.
fn day_target(
    clock: &dyn Fn(i64) -> Option<LocalTime>,
    days: &[Day],
    now: i64,
    picked: &Day,
) -> Option<i64> {
    let here = days
        .iter()
        .rev()
        .find(|day| day.start <= now)
        .map_or(now, |day| day.start);
    (picked.start != here).then(|| same_clock(clock, picked, now, here))
}

/// The days a calendar shows: every day the camera recorded on, oldest first,
/// ending with the day `now` falls in.
///
/// Walked an hour at a time through the local clock rather than computed,
/// because a day is not always 86400 seconds - a clock that goes forward or back
/// makes one 23 or 25 hours long - and the walk is the only thing here that
/// knows the local rules. The first moment of a day it meets is that day's
/// start, within an hour of midnight, which is as close as the strip that
/// follows it needs.
///
/// `recorded` is what the box answered: the days that hold something, named
/// `YYYY-MM-DD`. **Only those are kept**, and the walk goes back as far as the
/// earliest of them - so the calendar is as long as the recording is rather than
/// three weeks ending today, and the days a camera was switched off are not
/// buttons that lead nowhere.
fn calendar(
    clock: &dyn Fn(i64) -> Option<LocalTime>,
    now: i64,
    recorded: &HashSet<String>,
) -> Vec<Day> {
    let Some(earliest) = recorded.iter().min() else {
        // A camera that has recorded nothing has nothing to show.
        return Vec::new();
    };
    let mut out: Vec<Day> = Vec::with_capacity(recorded.len());
    let mut current: Option<(String, i64)> = None;
    let mut hour = 0;
    while hour <= CALENDAR_LIMIT {
        let at = now - hour * 3600;
        hour += 1;
        let Some(local) = clock(at) else {
            // A platform with no clock has no calendar either.
            return Vec::new();
        };
        let key = format!("{:04}-{:02}-{:02}", local.year, local.month, local.day);
        match current.as_mut() {
            // The walk goes backwards, so the oldest moment of a day it meets is
            // the last one it sees: the day's start is the sample before the
            // clock names an older day.
            Some((seen, first)) if *seen == key => *first = at,
            _ => {
                if let Some((seen, first)) = current.take() {
                    if recorded.contains(&seen) {
                        out.push(day_of(seen, first));
                    }
                }
                if key < *earliest {
                    // The walk has passed the first day the camera recorded on,
                    // and the day it is on now is not one of them: there is
                    // nothing older to name.
                    break;
                }
                current = Some((key, at));
            }
        }
    }
    if let Some((seen, first)) = current.take() {
        if recorded.contains(&seen) {
            out.push(day_of(seen, first));
        }
    }
    out.reverse();
    out
}

/// One day of the calendar, from the `YYYY-MM-DD` the walk named it by and the
/// first moment of it the walk met.
fn day_of(key: String, start: i64) -> Day {
    // `YYYY-MM-DD`, so the month and the day are where the dashes are not.
    Day { start, label: format!("{}/{}", &key[5..7], &key[8..10]) }
}

/// How wide a day's button is on this row, and how long the whole run of them is.
///
/// A row shows as many days as fit at the plan's 44 points each, so the buttons
/// fill it exactly when they do. When they do not, the run is longer than the
/// row - which is what there is to slide.
fn day_metrics(row: Rect, days: usize) -> (f32, f32) {
    let length = if row.width() > row.height() { row.width() } else { row.height() };
    let shown = days_across(length, days);
    if shown == 0 {
        return (0.0, 0.0);
    }
    let each = length / shown as f32;
    (each, each * days as f32)
}

/// Where the calendar can be slid to: from its first day to its last, and no
/// further than the row shows. A row that holds every day does not move.
fn day_scroll_to(scroll: f32, total: f32, length: f32) -> f32 {
    scroll.clamp(0.0, (total - length).max(0.0))
}

/// How far along the row a point is, in points from the first day's edge - the
/// axis the days are laid out on, which is the screen's x for a picture as it
/// arrives and its y for one drawn turned.
fn day_along(row: Rect, pos: egui::Pos2) -> f32 {
    if row.width() > row.height() {
        pos.x - row.left()
    } else {
        pos.y - row.top()
    }
}

/// The day under a point on a calendar slid by `scroll`, `each` points a day.
///
/// Held to the run of them: a point on the row's very last pixel is a fraction
/// of a day past the last one, and it is the last one the finger is over.
fn day_at(row: Rect, days: &[Day], pos: egui::Pos2, scroll: f32, each: f32) -> Option<&Day> {
    if each <= 0.0 {
        return None;
    }
    let index = ((day_along(row, pos) + scroll) / each).floor();
    if index < 0.0 {
        return None;
    }
    days.get((index as usize).min(days.len().saturating_sub(1)))
}

/// Draws `text` centred on `spot`, turned a quarter turn clockwise when
/// `turned`.
///
/// A painter's own `text` has no angle, so the galley is laid out and placed as
/// a shape - the same bargain [`icons::label_in`] makes, and the same turn: a
/// strip that runs down a turned picture carries its times down the screen
/// rather than across it.
fn turned_text(
    painter: &egui::Painter,
    spot: egui::Pos2,
    text: String,
    size: f32,
    colour: egui::Color32,
    turned: bool,
) {
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(size), colour);
    let extent = galley.size();
    // A quarter turn about `pos` puts the text's own centre at `(-h/2, w/2)`
    // from it, so the origin has to be moved back by that much for the middle of
    // the text to land on `spot`.
    let pos = if turned {
        egui::pos2(spot.x + extent.y * 0.5, spot.y - extent.x * 0.5)
    } else {
        egui::pos2(spot.x - extent.x * 0.5, spot.y - extent.y * 0.5)
    };
    painter.add(egui::epaint::TextShape {
        pos,
        galley,
        underline: egui::Stroke::NONE,
        fallback_color: colour,
        override_text_color: None,
        opacity_factor: 1.0,
        angle: if turned { std::f32::consts::FRAC_PI_2 } else { 0.0 },
    });
}

/// How far a tick stands out of the strip's upper edge, and how far the one
/// between two labels does. The row is `tilebar::TIMELINE` tall with the band
/// ten points thick across its middle, which leaves the ruler the gap under the
/// times: long enough to read against the band, short enough not to reach the
/// labels.
const TICK: f32 = 6.0;
/// See [`TICK`].
const TICK_MINOR: f32 = 3.0;

/// Where the times sit across the row, and where the band's centre does: two
/// fractions of the row's own depth, so a picture drawn turned is the same
/// drawing - see `across` in [`XgViewApp::draw_timeline`].
///
/// A row of [`tilebar::TIMELINE_ROW`] then reads as four bands, top to bottom:
/// the times, the ruler, the band, and the moment under the cursor.
const TIME_ROW: f32 = 0.115;
/// See [`TIME_ROW`].
const BAND_ROW: f32 = 0.48;
/// The band's thickness, in points. The ruler stands on its edge, so the two
/// numbers are read together.
const BAND: f32 = 10.0;
/// The size of the times along the top.
const TIME_SIZE: f32 = 9.5;
/// The space kept between two times along the top, so that two of them read as
/// two rather than as one long string.
const LABEL_GAP: f32 = 14.0;

/// The steps a time along the strip is allowed to land on: a clock's own
/// divisions, from a second to a day.
///
/// A step from this list is what makes a label readable as a time; anything
/// between them - seven seconds, two and a half minutes - reads as an arbitrary
/// cut of a recording rather than as a moment.
const STEPS: [i64; 19] = [
    1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10_800, 14_400, 21_600, 43_200,
    86_400,
];

/// The step between the times along the top of the strip: the finest clock step
/// that fits `labels` of them across `span`.
///
/// The count is the strip's own, measured from how wide a label is - a wide
/// screen reads a time every minute where a phone reads one every ten, and the
/// same recording is as finely divided as the room it is drawn in allows.
fn tick_step(span: i64, labels: i64) -> i64 {
    let want = (span / labels.max(1)).max(1);
    STEPS.iter().copied().find(|step| *step >= want).unwrap_or(86_400)
}

/// How many labels fit across a strip `length` long, each `wide` wide with
/// `gap` between two of them.
///
/// Half a label is given up at each end, because a label is centred on its
/// moment and the first and last of them hang over the ends of the strip.
fn labels_across(length: f32, wide: f32, gap: f32) -> i64 {
    (((length - wide) / (wide + gap)).floor() as i64).max(1)
}

/// The two ends of a tick: across the row from `edge` to `tip`, at `mark` along
/// it.
///
/// Both ends share the place along the strip, so the stroke is the tick's width
/// and nothing else. A pair that differed there would be a diagonal, and a
/// point-wide diagonal leans - which is exactly how the first cut of this looked
/// on screen.
fn tick_ends(horizontal: bool, mark: f32, edge: f32, tip: f32) -> [egui::Pos2; 2] {
    if horizontal {
        [egui::pos2(mark, edge), egui::pos2(mark, tip)]
    } else {
        [egui::pos2(edge, mark), egui::pos2(tip, mark)]
    }
}

/// The step between the short ticks of the ruler: the finest clock step that
/// divides a label's own, so a tick lands where no time is written and still on
/// the grid the times are on.
///
/// Dividing by five at most: a step of 300 seconds is five minutes, whose
/// divisor is a minute, and a mark every minute under a label every five is a
/// ruler rather than a hatch.
fn ruler_step(step: i64) -> i64 {
    STEPS.iter()
        .rev()
        .copied()
        .find(|small| *small < step && step % *small == 0)
        .unwrap_or(step)
}

/// The clock's name for a moment, to the second when the times are closer
/// together than a minute.
///
/// Without the seconds two labels half a minute apart read the same, and the
/// closest widths showed `14:30` twice.
fn clock_text(local: LocalTime, seconds: bool) -> String {
    if seconds {
        format!("{:02}:{:02}:{:02}", local.hour, local.minute, local.second)
    } else {
        format!("{:02}:{:02}", local.hour, local.minute)
    }
}

/// The clock's name for a moment, on the viewer's own clock.
fn clock_of(at: i64, seconds: bool) -> Option<String> {
    local_at(at).map(|local| clock_text(local, seconds))
}

/// The whole of a moment, to the second, on the viewer's own clock.
fn stamp_of(at: i64) -> Option<String> {
    local_at(at).map(|local| {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            local.year, local.month, local.day, local.hour, local.minute, local.second
        )
    })
}

/// How many of the calendar's days fit across the strip.
///
/// The plan's rule: at least 44 points each, which is a touch target. A strip
/// too short for all of them shows the newest, because a calendar ends today.
fn days_across(length: f32, days: usize) -> usize {
    if days == 0 {
        return 0;
    }
    ((length / 44.0).floor() as usize).clamp(1, days)
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

/// The stretch of segment list to read so the strip can be as wide as it is
/// drawn.
///
/// The strip is drawn from the cursor less half its width, so that is as far
/// back as the list has to reach: a strip that showed a stretch nobody had asked
/// for would be a band of nothing on its left, and no amount of dragging would
/// fill it. `None` when the list already reaches that far, which is the usual
/// case.
///
/// `oldest` is the earliest moment the camera was asked about and answered
/// nothing for: a strip reaching past it is not asked for again.
fn older_window(
    segments: &[Segment],
    zoom: i64,
    centre: i64,
    oldest: Option<i64>,
) -> Option<(i64, i64)> {
    let first = segments.first()?;
    let want = centre.saturating_sub(zoom / 2);
    let want = oldest.map_or(want, |oldest| want.max(oldest));
    if want >= first.start {
        return None;
    }
    Some((want, first.start))
}

/// A moment kept between the oldest the camera has and the newest - or under
/// the newest alone, while nothing has said where the oldest is.
///
/// The two are put in order first: a bound that crossed the other would make
/// `clamp` panic, and a panic while a frame is being drawn is a crash. It cannot
/// happen while a list only ever grows, but a strip is not worth a crash.
fn clamp_to(at: i64, oldest: Option<i64>, newest: i64) -> i64 {
    let oldest = oldest.unwrap_or(i64::MIN).min(newest);
    at.clamp(oldest, newest)
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
/// `lead` is half the widest range the box serves, so that the range runs as far
/// ahead of the moment as it does behind it and a scrub either way stays inside
/// it: a jump within a playlist is half a second, and one out of it is a request
/// and a session. It is not half the strip - the strip slides, and a range as
/// narrow as the window would be a session every time the finger crossed it.
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
///
/// An hour is where the list starts, not where it stops: a timeline wider than
/// that reads the stretch before it as it widens. See [`XgViewApp::read_older`].
const LOOKBACK: i64 = 3600;

/// How much of a recording a channel is given at once, where nothing else says.
///
/// Ten minutes: the window a `clip.mp4` is cut from, and the range a recording
/// opens with. A playlist's range is the width the timeline is showing, up to
/// [`RANGE_CAP`] - this is the floor of that, not the rule. What is asked for
/// and where playback begins are two different questions: the range runs from
/// the moment asked for, and only what was recorded fills it.
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
        // The playlist is asked for the widest stretch the box will serve,
        // centred on the moment, so that a drag either way is a fragment of the
        // session already running rather than another range and another session.
        // A box with no playlist gets a window from the moment itself, which is
        // where a `clip.mp4` is cut.
        let width = RANGE_CAP;
        let range = window_at(&segments, range_start(&segments, at, width / 2), width);
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

/// Reads the events of a stretch of recording. See [`XgViewApp::read_events`].
fn start_events(
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    index: usize,
    source: Source,
    from: i64,
    to: i64,
) {
    let sender = events.clone();
    handle.spawn(async move {
        let (user, pass) = source.credentials.clone().unwrap_or_default();
        let config = FrigateConfig::at(&source.origin.base_url, &user, &pass);
        let times = match FrigateClient::new(config) {
            Ok(mut client) => client.event_spans(&source.origin.camera, from, to).await.ok(),
            Err(_) => None,
        };
        let _ = sender.send(BackgroundEvent::EventsReady {
            index,
            camera_id: source.camera,
            events: times,
        });
    });
}

/// Asks the box which days it recorded on. See [`XgViewApp::read_days`].
///
/// Answered with `None` when the camera could not be asked at all, which the
/// calendar shows as no calendar rather than as a camera that recorded nothing.
fn start_days(handle: &Handle, events: &Sender<BackgroundEvent>, index: usize, source: Source) {
    let sender = events.clone();
    handle.spawn(async move {
        let (user, pass) = source.credentials.clone().unwrap_or_default();
        let config = FrigateConfig::at(&source.origin.base_url, &user, &pass);
        let days = match FrigateClient::new(config) {
            Ok(mut client) => client.recorded_days(&source.origin.camera).await.ok(),
            Err(_) => None,
        };
        let _ = sender.send(BackgroundEvent::DaysReady {
            index,
            camera_id: source.camera,
            days,
        });
    });
}

/// Reads the stretch of segment list a wider timeline needs.
///
/// Answered with `None` when the camera could not be asked at all, which is not
/// the same as "nothing was recorded": the strip has to be able to tell a failed
/// request from an empty stretch, or one bad read stops it widening for good.
fn start_segments(
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    index: usize,
    source: Source,
    from: i64,
    to: i64,
) {
    let sender = events.clone();
    handle.spawn(async move {
        let (user, pass) = source.credentials.clone().unwrap_or_default();
        let config = FrigateConfig::at(&source.origin.base_url, &user, &pass);
        let segments = match FrigateClient::new(config) {
            Ok(mut client) => client
                .segments(&source.origin.camera, from, to)
                .await
                .ok(),
            Err(_) => None,
        };
        let _ = sender.send(BackgroundEvent::SegmentsReady {
            index,
            camera_id: source.camera,
            from,
            to,
            segments,
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

    /// The strip is the width it is showing, centred on the cursor - and the
    /// cursor is the middle of it, which is the one thing that does not move.
    #[test]
    fn the_strip_is_drawn_around_the_cursor() {
        let window = |centre: i64, zoom: i64| {
            let half = zoom / 2;
            (centre - half, centre + half)
        };
        assert_eq!(window(1000, 600), (700, 1300));
        assert_eq!(window(1000, 86_400), (1000 - 43_200, 1000 + 43_200));
        // A moment near the start of the recording keeps its centre: the strip
        // shows the empty space before it rather than pushing the cursor off.
        assert_eq!(window(10, 600), (-290, 310));
    }

    /// The zoom buttons walk a fixed ladder and stop at its ends: two buttons
    /// that wrapped would make "one more step of detail" mean "the coarsest".
    #[test]
    fn the_zoom_buttons_walk_a_ladder_and_stop() {
        assert_eq!(step_zoom(600, true), Some(120));
        assert_eq!(step_zoom(600, false), Some(3600));
        assert_eq!(step_zoom(120, true), None, "as close as it goes");
        assert_eq!(step_zoom(86_400, false), None, "as wide as it goes");
        // And the ladder is the one the strip draws.
        assert_eq!(ZOOMS, [120, 600, 3600, 21_600, 86_400]);
    }

    /// A pinch moves the ladder one step at a time, not one step per frame: the
    /// ratios multiply, and the rest of the spread is carried.
    #[test]
    fn a_pinch_steps_the_ladder_once_it_has_spread() {
        // A little at a time adds up.
        let mut pinch = 1.0_f32;
        let mut steps = 0;
        for _ in 0..12 {
            let (step, next) = pinch_step(pinch, 1.05);
            pinch = next;
            steps += usize::from(step.is_some());
        }
        // Twelve frames of a twentieth each is 1.80 times, which is one step
        // and a little over.
        assert_eq!(steps, 1, "twelve small spreads are one step, not twelve");
        assert!(pinch > 1.0 / PINCH_STEP && pinch < PINCH_STEP, "and the rest is kept");

        // Closing the fingers is the other way.
        let (step, _) = pinch_step(1.0, 0.4);
        assert_eq!(step, Some(false));

        // A gesture that ends where it began asks for nothing.
        let (step, pinch) = pinch_step(1.0, 1.0);
        assert!(step.is_none());
        assert_eq!(pinch, 1.0);
    }

    /// The times along the top are a clock's, not an arbitrary division: the
    /// finest one that fits the labels the strip has room for.
    #[test]
    fn the_times_along_the_strip_are_a_clocks() {
        assert_eq!(tick_step(120, 6), 30);
        assert_eq!(tick_step(600, 6), 120);
        assert_eq!(tick_step(3600, 6), 600);
        assert_eq!(tick_step(86_400, 6), 14_400);
        // The same day on a wider strip is divided more finely, which is the
        // whole point of counting labels rather than assuming six.
        assert_eq!(tick_step(86_400, 24), 3600, "a day on a wide strip is an hour a label");
        assert_eq!(tick_step(86_400, 48), 1800, "and half an hour on a wider one");
    }

    /// No two times read the same. A step finer than a minute has to name the
    /// second, or the closest widths show `14:30` twice - which they did.
    #[test]
    fn no_two_times_read_the_same() {
        let clock = |unix: i64| LocalTime {
            year: 2026,
            month: 11,
            day: 3,
            weekday: 0,
            hour: (unix.div_euclid(3600) % 24) as u8,
            minute: (unix.div_euclid(60) % 60) as u8,
            second: unix.rem_euclid(60) as u8,
        };
        for zoom in ZOOMS {
            for labels in [1, 2, 3, 5, 8, 12, 24, 48] {
                let step = tick_step(zoom, labels);
                let times: Vec<String> = (0..zoom / step)
                    .map(|index| clock_text(clock(3600 * 9 + index * step), step < 60))
                    .collect();
                let mut unique = times.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(unique.len(), times.len(), "{zoom}s in {labels} labels: {times:?}");
            }
        }
    }

    /// A short tick divides a label's step and lands on the clock's own grid,
    /// and never stands more than five to a label.
    #[test]
    fn a_short_tick_divides_a_label() {
        for step in STEPS {
            let minor = ruler_step(step);
            assert_eq!(step % minor, 0, "{step}s divided into {minor}s");
            assert!(step / minor <= 5, "{step}s divided into {} ticks", step / minor);
        }
    }

    /// A label takes its own width plus a gap, and half of one is given up at
    /// each end because a label is centred on its moment.
    #[test]
    fn labels_are_counted_by_the_room_they_need() {
        assert_eq!(labels_across(600.0, 26.0, 14.0), 14);
        assert_eq!(labels_across(360.0, 26.0, 14.0), 8, "a phone reads fewer");
        assert_eq!(labels_across(30.0, 26.0, 14.0), 1, "and never none");
    }

    /// The ruler has the gap between the times and the band to itself: a long
    /// tick must end above the labels and below the band's edge, or the row
    /// reads as one smudge.
    #[test]
    fn the_ruler_stands_clear_of_the_times() {
        let row = crate::tilebar::TIMELINE_ROW;
        // Half a line of the label's own type, which is where its box ends.
        let times = row * TIME_ROW + TIME_SIZE * 0.7;
        let edge = row * BAND_ROW - BAND / 2.0;
        let ruler = edge - TICK;
        assert!(ruler > times, "the tick ends at {ruler:.1}pt, the times at {times:.1}pt");
        assert!(TICK > TICK_MINOR, "a long tick is longer than a short one");
        assert!(edge + BAND / 2.0 < row, "the band is inside the row");
    }

    /// A tick is straight: both its ends are at the same place along the strip,
    /// so the stroke is its width rather than its lean.
    #[test]
    fn a_tick_stands_straight() {
        let [foot, head] = tick_ends(true, 100.0, 20.0, 26.0);
        assert_eq!(foot.x, head.x, "across a wide row a tick is vertical");
        assert_eq!((foot.y, head.y), (20.0, 26.0), "and stands out of the edge");
        let [foot, head] = tick_ends(false, 100.0, 20.0, 26.0);
        assert_eq!(foot.y, head.y, "on a turned row it is horizontal");
        assert_eq!((foot.x, head.x), (20.0, 26.0), "and stands out of the edge");
    }

    /// The calendar is as long as the recording is: every day the box named,
    /// from the earliest of them to today, and none of the days between that it
    /// did not name.
    #[test]
    fn the_calendar_is_the_days_that_were_recorded() {
        // A clock whose day is 86400 seconds, so the walk can be counted.
        let clock = |unix: i64| {
            Some(LocalTime {
                year: 2026,
                month: 11,
                day: (unix.div_euclid(86_400) % 28 + 1) as u8,
                weekday: 0,
                hour: 0,
                minute: 0,
                second: 0,
            })
        };
        // Three days named, the oldest of them four days back, with a quiet day
        // between it and today.
        let recorded: HashSet<String> = ["2026-11-07", "2026-11-10", "2026-11-11"]
            .iter()
            .map(|day| day.to_string())
            .collect();
        let days = calendar(&clock, 86_400 * 10, &recorded);
        let labels: Vec<&str> = days.iter().map(|day| day.label.as_str()).collect();
        assert_eq!(labels, ["11/07", "11/10", "11/11"], "the quiet days are not buttons");
        // The fake clock counts its days from one, so day 7 is epoch 86400 * 6.
        let starts: Vec<i64> = days.iter().map(|day| day.start).collect();
        assert_eq!(starts, [86_400 * 6, 86_400 * 9, 86_400 * 10]);

        // A camera that has recorded nothing has no calendar, and neither has a
        // platform with no clock.
        assert!(calendar(&clock, 86_400 * 10, &HashSet::new()).is_empty());
        assert!(calendar(&|_| None, 86_400 * 10, &recorded).is_empty());
    }

    /// A clock whose every day is 86400 seconds, which is what most of them are.
    fn plain_clock(unix: i64) -> Option<LocalTime> {
        Some(LocalTime {
            year: 2026,
            month: 11,
            day: 3,
            weekday: 0,
            hour: (unix.div_euclid(3600) % 24) as u8,
            minute: (unix.div_euclid(60) % 60) as u8,
            second: unix.rem_euclid(60) as u8,
        })
    }

    /// A picked day keeps the time of day: the hour, the minute and the second
    /// the viewer was watching, on another date.
    #[test]
    fn a_picked_day_keeps_the_time_of_day() {
        // Starts a minute past midnight, which is where the walk through the
        // clock finds them - the same minute every day.
        let day = |start: i64| Day { start, label: String::new() };
        let days = vec![day(60), day(86_400 + 60), day(2 * 86_400 + 60)];
        // Half past two in the afternoon of the middle day.
        let now = 86_400 + 60 + 14 * 3600 + 30 * 60;
        assert_eq!(day_target(&plain_clock, &days, now, &days[0]), Some(60 + 14 * 3600 + 30 * 60));
        assert_eq!(
            day_target(&plain_clock, &days, now, &days[2]),
            Some(2 * 86_400 + 60 + 14 * 3600 + 30 * 60)
        );
        // The day it is already on is the way out, not a move.
        assert_eq!(day_target(&plain_clock, &days, now, &days[1]), None);
        // A moment before every start the calendar holds has no day of its own:
        // the picked day's start is the answer, carried to the clock the moment
        // reads - ten seconds past midnight, whichever day that is on.
        assert_eq!(day_target(&plain_clock, &days, 10, &days[1]), Some(86_400 + 10));
    }

    /// A day that gains an hour keeps the clock, not the distance: at the end of
    /// daylight saving the same distance into the day lands an hour early, which
    /// is what a pick on that date came back as.
    #[test]
    fn a_day_that_gains_an_hour_keeps_the_clock() {
        // 10/31 starts at zero and is a day long; 11/01 starts at 86400 and is
        // twenty-five hours long - at two in the morning its clock goes back to
        // one, so the hour from one to two happens twice; 11/02 starts where it
        // ends.
        const NOV_1: i64 = 86_400;
        const NOV_2: i64 = 86_400 + 90_000;
        let clock = |unix: i64| {
            let local = if unix < NOV_1 {
                unix
            } else if unix < NOV_2 {
                let into = unix - NOV_1;
                if into < 7200 {
                    into
                } else {
                    into - 3600
                }
            } else {
                unix - NOV_2
            };
            Some(LocalTime {
                year: 2026,
                month: 11,
                day: 1,
                weekday: 0,
                hour: (local.div_euclid(3600) % 24) as u8,
                minute: (local.div_euclid(60) % 60) as u8,
                second: local.rem_euclid(60) as u8,
            })
        };
        let day = |start: i64| Day { start, label: String::new() };
        let days = vec![day(0), day(NOV_1), day(NOV_2)];

        // Half past two in the afternoon of 10/31, carried to 11/01: the clock
        // reads 14:30 there too, which is an hour and a half past the distance
        // in seconds.
        let now = 14 * 3600 + 30 * 60;
        let at = day_target(&clock, &days, now, &days[1]).expect("another day");
        assert_eq!(clock(at).map(|local| (local.hour, local.minute)), Some((14, 30)));
        assert_eq!(at, NOV_1 + 15 * 3600 + 30 * 60, "the hour the day gained is passed over");

        // And back the other way: from 11/01 at 14:30 to 11/02, which is a day
        // long again - the hour has to be taken off rather than added.
        let back = day_target(&clock, &days, at, &days[2]).expect("another day");
        assert_eq!(clock(back).map(|local| (local.hour, local.minute)), Some((14, 30)));
        assert_eq!(back, NOV_2 + 14 * 3600 + 30 * 60);
    }

    /// A day's button is deep enough for its name: the row less the two insets
    /// has to be deeper than the type that goes in it.
    #[test]
    fn a_day_button_holds_its_name() {
        let band = crate::tilebar::TIMELINE_ROW - 2.0 * DAY_PAD;
        assert!(band > DAY_SIZE * 1.4, "a {band}pt button for {DAY_SIZE}pt type");
        assert!(DAY_GAP > 0.0, "and a gap, or two days read as one");
    }

    /// A day is a touch target: the row shows as many as fit at the plan's 44
    /// points each, and the days it cannot hold are a slide away rather than
    /// lost.
    #[test]
    fn the_calendar_shows_as_many_days_as_fit() {
        assert_eq!(days_across(1100.0, 21), 21, "room for all of them");
        assert_eq!(days_across(200.0, 21), 4);
        assert_eq!(days_across(10.0, 21), 1, "never none");
        assert_eq!(days_across(1100.0, 0), 0, "nothing to show");

        let days: Vec<Day> = (0..3)
            .map(|index| Day { start: index, label: format!("11/0{index}") })
            .collect();
        let row = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(210.0, 48.0));
        let (each, total) = day_metrics(row, days.len());
        assert_eq!((each, total), (70.0, 210.0), "three days fill the row exactly");
        let at = |x: f32, scroll: f32| day_at(row, &days, egui::pos2(x, 24.0), scroll, each);
        assert_eq!(at(10.0, 0.0).map(|day| day.start), Some(0));
        assert_eq!(at(100.0, 0.0).map(|day| day.start), Some(1));
        assert_eq!(at(205.0, 0.0).map(|day| day.start), Some(2));
        assert_eq!(day_scroll_to(30.0, total, 210.0), 0.0, "a row that holds them all does not slide");
        assert!(day_at(row, &[], egui::pos2(10.0, 24.0), 0.0, each).is_none());

        // A row too short for all of them is a window on the run: two days
        // across, and the third a slide of one day away.
        let narrow = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 48.0));
        let (each, total) = day_metrics(narrow, days.len());
        assert_eq!(each, 50.0, "two days across a hundred points");
        assert_eq!(day_scroll_to(f32::MAX, total, 100.0), 50.0, "one day's worth of slide");
        assert_eq!(day_scroll_to(-10.0, total, 100.0), 0.0, "and no further back than the first");
        let at = |x: f32| day_at(narrow, &days, egui::pos2(x, 24.0), 50.0, each);
        assert_eq!(at(10.0).map(|day| day.start), Some(1));
        assert_eq!(at(90.0).map(|day| day.start), Some(2));
    }

    /// A picked day is the whole day. The strip is anchored on the day's end,
    /// so it runs from midnight to midnight rather than from the eighteen hours
    /// before the moment to the six after it.
    #[test]
    fn a_picked_day_is_the_whole_day() {
        // A day and a bit of recording, which is what picking one reads.
        let segments = run(0, 90_000);
        let zoom = 86_400;
        assert_eq!(extent(&segments, zoom, zoom * 3 / 4), Some((0, zoom)));
        // Anchored on the moment instead, the strip would start where the list
        // does and end six hours later.
        assert_eq!(extent(&segments, zoom, 0), Some((0, zoom / 4)));
    }

    /// A seek asks for the widest range the box serves, not the width on screen:
    /// the strip slides, and one that left its range every ten minutes would be
    /// a session every ten minutes.
    #[test]
    fn a_seek_asks_for_a_range_wider_than_the_strip() {
        let segments = run(0, 86_400);
        let at = 40_000;
        let window = window_at(&segments, range_start(&segments, at, RANGE_CAP / 2), RANGE_CAP)
            .expect("a stretch");
        assert!(window.0 <= at && at < window.1, "it holds the moment");
        assert_eq!(window.1 - window.0, RANGE_CAP, "and it is as wide as the box serves");

        // A moment near the start of the recording gets what there is of it.
        let near = window_at(&segments, range_start(&segments, 60, RANGE_CAP / 2), RANGE_CAP)
            .expect("a stretch");
        assert_eq!(near.0, 0);

        // A moment before the list has no window of its own: the nearest
        // stretch comes back and it does not hold the moment, which is what
        // tells a read-ahead not to ask for a range the finger has not reached.
        let before = window_at(&segments, range_start(&segments, -3600, RANGE_CAP / 2), RANGE_CAP)
            .expect("a stretch");
        assert!(!covered(Some(before), -3600), "the nearest stretch is not the moment's own");
    }

    /// A strip wider than what has been read asks for the rest of the list, once
    /// - and stops asking once the camera has said there is nothing older.
    #[test]
    fn a_wider_strip_reads_the_list_it_needs() {
        let segments = run(0, 3600);
        // Ten minutes of a list that holds an hour: nothing to read.
        assert_eq!(older_window(&segments, 600, 3600, None), None);
        // Six hours: the strip is drawn from three hours back, and the list
        // begins at 0, so that is the stretch to read.
        assert_eq!(older_window(&segments, 21_600, 3600, None), Some((-7200, 0)));
        // A day-wide strip asks back half a day - the strip's own left edge -
        // and not a quarter of it, which left the leftmost six hours showing
        // nothing however the strip was dragged.
        assert_eq!(older_window(&segments, 86_400, 3600, None), Some((-39_600, 0)));
        // Once the camera has answered that nothing was recorded before it, the
        // same width asks for nothing.
        assert_eq!(older_window(&segments, 21_600, 3600, Some(0)), None);
        // And a list that already reaches back far enough asks for nothing.
        assert_eq!(older_window(&run(-21_600, 3600), 21_600, 3600, None), None);
    }

    /// A drag stops where the recording begins, not where the list does: what
    /// has been read is not the end of what exists, and a strip that stopped
    /// there was a band of nothing the finger could not push out of.
    #[test]
    fn a_drag_stops_at_the_oldest_history_not_at_what_was_read() {
        // The list begins at 100 and the camera has said nothing before 40: a
        // drag back to 60 is fine, and one further goes to 40, not to 100.
        assert_eq!(clamp_to(60, Some(40), 900), 60);
        assert_eq!(clamp_to(10, Some(40), 900), 40);
        // Nothing has said where the beginning is: no lower bound, so the strip
        // keeps moving and the read follows the finger.
        assert_eq!(clamp_to(10, None, 900), 10);
        // The newest moment is a wall either way.
        assert_eq!(clamp_to(1000, None, 900), 900);
        // And a bound that crossed the other is put in order rather than
        // panicking inside a frame.
        assert_eq!(clamp_to(500, Some(900), 900), 900);
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

    /// The window's first frames are behind the moment asked for, and are not
    /// what ends the wait - the cursor stays where the seek asked for it until
    /// the picture gets there.
    #[test]
    fn the_cursor_waits_for_the_picture_to_catch_up() {
        assert!(!caught_up(Some(100.0), 110));
        assert!(!caught_up(None, 110), "nothing has been decoded yet");
        assert!(caught_up(Some(110.0), 110));
        assert!(caught_up(Some(110.4), 110));
    }
}
