//! A camera's recordings, playing in the magnified viewport.
//!
//! The camera keeps its slot: its name, its on-screen display, its place on the
//! wall, its settings. Only the picture comes from somewhere else - a channel of
//! its own, one past the last camera, which the scheduler never names and so
//! never opens, parks or reopens.

use super::*;

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::grid::Recording;
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
    /// An arrow held down on the timeline. See [`Held`].
    pub(super) held: Option<Held>,
    /// Which of the bar's controls the focus came from when Down took it into
    /// the strip, so Up can hand it back to the one it left rather than to the
    /// first. `None` when it came from the seat, which is not a control.
    pub(super) from_control: Option<usize>,
    /// What the camera recorded around it. Read once when playback opens, and
    /// kept: the timeline draws the stretches that exist, and asking again for
    /// every repaint would be a request a frame.
    pub(super) segments: Vec<Segment>,
    /// [`stretches`] of `segments`: the runs the strip is drawn as one band for.
    /// Kept because the strip draws it every frame, and merging a day of ten
    /// second files per frame is work that changes only when the list does.
    pub(super) stretches: Vec<Segment>,
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
    /// The moment the picture was on when the seek was made, which its own
    /// frame is of.
    ///
    /// A seek backwards leaves the session stopped on the frame it was holding,
    /// and that frame is published before the landing one - it is later than the
    /// moment asked for, so it looks like the seek has arrived and the cursor
    /// goes back to where it started from. The frame at this moment is the one
    /// being left, and it does not count as arriving. `None` when the picture
    /// has no moment yet, or when the seek goes nowhere.
    pub(super) pending_from: Option<i64>,
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
    /// How many seeks have been made since the recording opened.
    ///
    /// A range is read in the background, and a drag can ask for more than one:
    /// the answer that lands last is not necessarily the seek that was made
    /// last. Each read carries the number of the seek that asked for it, and an
    /// answer older than the newest one is dropped rather than played.
    pub(super) seek_gen: u64,
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
    /// The day the remote is on in the calendar, as a place in [`Self::days`].
    ///
    /// A calendar is a row of buttons, and the remote walks it: the arrows move
    /// this, the ring is drawn on it, and the confirm key picks it. `None` until
    /// the calendar is shown, when it starts on the day the picture is on.
    pub(super) day_cursor: Option<usize>,
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
    /// The stretch being read for the strip's *right* edge. A slot of its own,
    /// because a strip can be short on both sides at once and one slot would
    /// starve one of them. See [`XgViewApp::read_newer`].
    pub(super) reading_newer: Option<(i64, i64)>,
    /// The moment a read the box could not answer may be asked for again, on
    /// each side of the strip.
    ///
    /// A failed read leaves the stretch it was for unread, and a stretch with
    /// nothing drawn in it reads as "not recorded". Asked again the next frame
    /// would be a request every frame to a box that keeps failing, so the retry
    /// waits a few seconds. One per side, because a box that failed one read has
    /// not failed the other, and holding both back kept a side that was working
    /// from filling until the other side's retry came round. See
    /// [`XgViewApp::read_strip`].
    pub(super) retry_older: i64,
    pub(super) retry_newer: i64,
    /// The earliest moment the camera answered "nothing recorded" for, once a
    /// strip has been drawn that far back. The strip stops there.
    pub(super) oldest: Option<i64>,
    /// Whether the picture is held. The session is told, and stops reading; the
    /// bar draws the control as the one that lets it go.
    pub(super) paused: bool,
    /// When the camera's events began and ended, in the stretch that was read.
    /// Empty until the box has answered, and empty for a camera with none.
    pub(super) events: Vec<(i64, i64)>,
    /// The stretches the event list has been answered for, in order and merged.
    ///
    /// Apart from the segment list's own [`Self::covered`], which is not the same
    /// question: a strip can hold the marks of a stretch whose segments were read
    /// for another width, and the other way round. What this holds is what the
    /// box has been asked about *for events*, so a window that has already been
    /// answered for is not asked again every frame as it slides. See
    /// [`events_to_ask`].
    pub(super) events_cover: Vec<(i64, i64)>,
    /// When the box may be asked for events again after an answer it could not
    /// give. See [`READ_RETRY`].
    pub(super) retry_events: i64,
    /// Whether the event list is being read, and which window it was asked for.
    /// An answer for another window is not this list: a jump or a widened strip
    /// that asked while this read was in flight must not take it as its own.
    pub(super) reading_events: bool,
    pub(super) events_ask: Option<(i64, i64)>,
    /// A window the events had to be read for while another read was in flight.
    ///
    /// The read is one at a time, and a second ask used to be dropped: a strip
    /// widened while its events were being read cleared the old marks and was
    /// never asked again, so the half it had just uncovered had none. The ask is
    /// remembered here and made when the answer lands.
    pub(super) events_wanted: Option<(i64, i64)>,
    /// The window a waiting jump needs. See [`XgViewApp::events_arrived`].
    pub(super) jump_ask: Option<(i64, i64)>,
    /// What came of looking past the hole a recording stopped at. See
    /// [`AfterHole`].
    pub(super) after_hole: Option<AfterHole>,
    /// The last automatic hop past a hole: the moment the picture was on, and
    /// the stretch it was carried on to.
    ///
    /// Kept apart from [`Self::after_hole`] because a seek clears that one, and
    /// a hop begins with a seek - so a guard in `after_hole` is gone by the time
    /// the hop's own session stops. A session that ends without putting a frame
    /// on the screen leaves the picture's moment where it was; its stop then
    /// names the same stretch again, and the hop restarts every few tens of
    /// milliseconds. A frame that arrives clears it. See
    /// [`XgViewApp::picture_arrived`] and `hole_hop_replayed`.
    pub(super) hole_hop: Option<(i64, i64)>,
    /// The stretches of time the box has been asked about, in order and merged.
    ///
    /// The segment list is what has been *read*, not what has been recorded: a
    /// list holding the 31st and today says nothing about the days between them.
    /// So "the next stretch after this moment" is only an answer when the box
    /// was asked about the time between - which is what this remembers.
    pub(super) covered: Vec<(i64, i64)>,
}

impl Playback {
    /// Whether the picture is standing still because the box has not cut what
    /// comes next yet, rather than because the viewer held it.
    ///
    /// A wait holds the session so the picture does not run on into a stretch
    /// it cannot serve, and the hold is not the viewer's pause: nothing is
    /// drawn differently for it except that the picture is not moving, which
    /// is what the corner and the play control say. See [`AfterHole::Waiting`].
    fn waiting(&self) -> bool {
        matches!(self.after_hole, Some(AfterHole::Waiting { .. }))
    }
}

/// What a recording that stopped at a hole is doing about it.
///
/// The box serves a playlist up to the first hole and stops there - the
/// `#EXT-X-ENDLIST` lands after the last fragment before it - so the session
/// ending is not the recording ending. What comes next is the beginning of the
/// next stretch, and finding it is a read: the segment list held for the strip
/// may not reach past the hole at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterHole {
    /// A range is being read for what comes after this moment.
    ///
    /// The range is part of it because a hole can be longer than the day one
    /// read covers: the next day is asked about from where the last answer
    /// stopped, and the answer has to be recognised as that read's.
    ///
    /// `at` is when to ask again if nothing comes back at all: a read the box
    /// could not answer is not "nothing after the hole", and a look that was
    /// forgotten left the picture stopped with nothing on its way. See
    /// [`XgViewApp::read_strip`].
    Looking { stop: i64, window: (i64, i64), at: i64 },
    /// It was read, and the box has nothing past the stop **yet**: the stop is
    /// the live edge, where the recorder is still cutting its next fragment. The
    /// question is asked again shortly rather than answered as the end.
    Waiting { stop: i64, at: i64 },
    /// It was read, and the recording has nothing after it: this is the end.
    Nothing(i64),
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

/// An arrow held down on the strip.
///
/// A press steps at once; after that the step repeats on its own clock. It has
/// to: a key that is down is down every frame, so a step a frame would cross the
/// strip three times a second, and the OS repeat is not something this side can
/// see the shape of. Two moments are kept because two things are measured from
/// them - how long it has been held, which is what the step grows with, and when
/// it last stepped, which is what the repeat waits for.
#[derive(Debug, Clone, Copy)]
pub(super) struct Held {
    /// Which way it is walking. `true` is forwards.
    pub(super) forward: bool,
    /// When it went down.
    pub(super) since: std::time::Instant,
    /// When it last stepped.
    pub(super) stepped: std::time::Instant,
    /// When the key was last seen up, `None` while it is down.
    ///
    /// Some front ends give one press a frame of its own, so its down and its
    /// up arrive apart; some remotes report a held key as a run of down/up
    /// pairs. Neither gap is a new press. The hold survives a short release so
    /// that its clocks do, and [`REPEAT_DELAY`] still separates a press from a
    /// hold. See [`Held::restarted`].
    pub(super) released: Option<std::time::Instant>,
}

impl Held {
    /// Whether the arrow is down right now, rather than in the short grace
    /// after it came up. `XgViewApp::recording_stopped` lets a held arrow's own
    /// steps keep their moment, but answers a stop once the finger is off.
    fn is_down(&self) -> bool {
        self.released.is_none()
    }

    /// Whether an arrow that has gone down again begins a new press.
    ///
    /// A change of direction always does. So does a release that has outlasted
    /// [`RELEASE_GRACE`] - that is a finger that has lifted and come back. A
    /// release inside the grace is the gap between two frames of one press.
    fn restarted(&self, forward: bool, now: std::time::Instant) -> bool {
        self.forward != forward
            || self
                .released
                .is_some_and(|at| now.saturating_duration_since(at) >= RELEASE_GRACE)
    }
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
    ///
    /// The origin spells it, because the corner of the tile that says what the
    /// recording is being fetched from reads the same one. See
    /// [`FrigateOrigin::api_base`].
    fn api_base(&self) -> String {
        self.origin.api_base()
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

    /// What the corners of this slot's tile say about the recording it shows.
    ///
    /// One answer, because it is one question: the tile is showing a recording,
    /// and this is what there is to say about it. The origin is the session's
    /// own rather than the camera's - the same server, but the recording was
    /// asked of the one that answered, and a corner naming the other would be
    /// reporting something nobody checked.
    pub(super) fn recording_mark(&self, slot: usize) -> Option<Recording<'_>> {
        let playback = self.playback.as_ref().filter(|playback| playback.slot == slot)?;
        let state = self.channels.get(&playback.index).map(|channel| channel.state);
        Some(Recording {
            origin: &playback.source.origin,
            // A wait is not a hold, but it is a picture that is not moving, and
            // the corner's question is only whether it is. See
            // [`Playback::waiting`].
            paused: still(playback.paused, state) || playback.waiting(),
            // Said only where it is not the normal rate: see [`Recording`].
            speed: (playback.speed > 1.5).then(|| speed_label(playback.speed)),
        })
    }

    /// The moment a recording's picture is of, for the corner that names it.
    ///
    /// The frame's own moment once one has arrived, and the cursor's until
    /// then: a seek is a request away, and the corner saying "now" for the
    /// second it takes would be the one thing a recording never means.
    pub(super) fn playback_clock(&self, slot: usize) -> Option<f64> {
        let playback = self.playback.as_ref().filter(|playback| playback.slot == slot)?;
        let frame = self.view_texture(slot).and_then(|entry| entry.at);
        frame.or_else(|| Some(self.centre_of(playback) as f64))
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
    // Everything the answer to the opening read carries, and it is one answer:
    // a struct for it would be a struct with one caller.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_playback(
        &mut self,
        channel: usize,
        source: Source,
        window: (i64, i64),
        begin: Option<i64>,
        playlist: Option<VodPlaylist>,
        segments: Vec<Segment>,
        read: (i64, i64),
    ) {
        let index = self.playback_index();
        let from = window.0;
        let begin = begin.map(|at| begin_in(window, at));
        // The camera's own session under this slot is a decoder drawing nothing
        // from here on: the recording is what the viewport shows. Let go of it
        // before the recording's first fragment is asked for, so that the two
        // are not both pulling a 1080p picture through a box that can carry one.
        self.scheduler.set_recording(Some(channel));
        self.needs_sync = true;
        let vod = self.play_recording(index, &source, window, playlist, begin);
        let merged = stretches(&segments);
        // What opening actually heard back: without it the first drag would
        // read the strip it is already holding, and wait for the answer before
        // moving. Only what the answer holds, though - the recorder lists a
        // file a moment after closing it, so an answer at the live edge stops
        // short of the request, and the seconds it did not reach would be a
        // hole nothing asks about again. See [`answered`].
        let covered: Vec<(i64, i64)> = answered(read, &segments).into_iter().collect();
        tracing::debug!(
            target: "xgview::app",
            index,
            asked_from = read.0,
            asked_to = read.1,
            known_to = covered.last().map(|run| run.1).unwrap_or(read.0),
            files = segments.len(),
            "the recording opened"
        );
        self.playback = Some(Playback {
            slot: channel,
            index,
            source,
            opened: begin.unwrap_or(from),
            speed: 1.0,
            pinch: 1.0,
            drag: None,
            held: None,
            from_control: None,
            zoom: SPAN,
            segments,
            stretches: merged,
            vod,
            scrub: None,
            // The first frame is the moment the cursor starts from, and it is a
            // request away; until then the slot still shows the camera's own
            // picture, which has no moment at all. A playlist's range reaches
            // back before where playback begins, so it is where the picture
            // starts - not the range's - that the cursor waits for.
            pending: Some(begin.unwrap_or(from)),
            // Nothing is on screen from this recording yet: the slot shows the
            // camera's own picture, which has no moment to be left behind.
            pending_from: None,
            scrubbing: false,
            timeline: Timeline::Span,
            days: Vec::new(),
            days_scroll: 0.0,
            days_drag: None,
            days_follow: false,
            day_cursor: None,
            reading_days: false,
            to_day: None,
            ahead: None,
            seek_gen: 0,
            awaiting: None,
            to_seek: None,
            to_jump: None,
            reading_older: None,
            reading_newer: None,
            retry_older: 0,
            retry_newer: 0,
            oldest: None,
            paused: false,
            events: Vec::new(),
            events_cover: Vec::new(),
            retry_events: 0,
            reading_events: false,
            events_ask: None,
            events_wanted: None,
            jump_ask: None,
            after_hole: None,
            hole_hop: None,
            // What opening actually heard back, so that the first drag does not
            // read the strip it is already holding. See [`answered`].
            covered,
        });
        // The bar's row has changed under the focus: the camera's controls are
        // gone and the recording's are here, and the place the focus was on means
        // something else in this row. It goes to the seat, which draws nothing
        // until an arrow picks a control. See [`Self::leave_playback`].
        self.restore_focus = Some(tilebar::seat());
        // A new recording starts at normal speed whatever the last one was
        // played at, and says so: the channel's control outlives the request
        // that is open - see `Supervisor::play` - so the speed it is holding is
        // the previous recording's until this one names its own.
        self.manager.set_rate(index, 1.0);
        // The speaker belongs to the channel whose picture is on screen, and
        // that has just become the recording's.
        self.apply_audio();
        // The events of the stretch the timeline shows, so that the two marks
        // either side of the cursor are known before anyone asks to step to one.
        self.read_events(index);
        // And the days the camera recorded on, which the calendar draws from.
        // Read here rather than when the calendar is first shown: until the
        // answer arrives the date row has nothing to draw, so a viewer who
        // presses the date control would watch an empty row for as long as the
        // box takes over `/api/recordings/summary`. Reading it while the
        // picture is already playing costs nothing anyone can see, and the
        // answer is in hand long before the press. See [`Self::read_days`].
        self.read_days(index);
    }

    /// Holds the picture, lets it go, or keeps it going.
    ///
    /// A hold is the session's own pause: it stops reading, so nothing is
    /// decoded and nothing is published, and the clock owes no time for the
    /// wait. A range that ran out has no session left to hold, so the same
    /// press - which the control draws as **play**, see
    /// [`Self::playback_still`] - carries the recording on past the hole it
    /// stopped at, and starts it again only when there is nothing past it. A
    /// picture waiting for the recorder to cut its next fragment is the third
    /// case: the press asks the box again rather than starting anything over.
    pub(super) fn toggle_play(&mut self) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // A press while the box has not cut what comes next is "try again now",
        // not "start over": the moment on screen is the one the playlist does
        // not reach yet, and the branch below would take the picture back to
        // where playback opened - a minute, or another day. Asked again at the
        // same moment, which either finds the fragment that has been cut since
        // or waits for it again. See [`Playback::waiting`].
        if playback.waiting() {
            let at = self.centre_of(playback);
            // Where a press that finds nothing past the stop starts it again,
            // worked out before the borrow goes: the answer to the probe arrives
            // a request later, and this one has to be ready when it does not
            // come back with anything. See [`restart_from`].
            let restart = restart_from(&playback.segments, playback.opened, at);
            // The press is "keep going": whatever the viewer had held is let go
            // of, so the session the retry starts plays rather than starting
            // held.
            if let Some(playback) = self.playback.as_mut() {
                playback.paused = false;
            }
            // Asked of the playlist rather than through a seek: the stop is
            // where the picture already is, and a seek to it is a segment list
            // read that the last answer has already given. One request, and the
            // picture plays as soon as the fragments are there. See
            // [`Self::look_at_edge`].
            if self.look_at_edge(at) {
                tracing::debug!(target: "xgview::app", at, "the recording is asked for again");
                return;
            }
            // Nothing past the stop, and nothing being written there either: the
            // press starts the recording again rather than doing nothing. See
            // [`at_live_edge`].
            self.move_to(restart);
            tracing::debug!(target: "xgview::app", at = restart, "the recording starts again");
            return;
        }
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // A range that ran out has nothing to hold, and the control that offers
        // a press offers it as **play**. What the press means is "keep going":
        // the recording carries on at the next stretch, past the hole it
        // stopped at, or at the file the recorder has just cut when the stop is
        // the live edge. With nothing there it plays again from where the viewer
        // was watching. It never waits for an answer: a press that returns
        // without the picture moving is a press that did nothing. See
        // [`AfterHole`] and [`restart_from`].
        if self.recording_ended() {
            let stop = self.centre_of(playback);
            let next = next_stretch(&playback.segments, &playback.covered, stop);
            let carries = carries_on(&playback.segments, stop);
            let opened = playback.opened;
            let restart = restart_from(&playback.segments, opened, stop);
            // The press is "keep going" whether the picture was held or not, and
            // whatever the answer turns out to be it is played: a stop is the
            // last frame there was, not a moment the viewer chose to hold. A
            // press that left the hold on found nothing to let go of, and the
            // recording it carried on into started held - which is a press that
            // does nothing at all. See [`AfterHole`].
            if let Some(playback) = self.playback.as_mut() {
                playback.paused = false;
            }
            if let Some(next) = next {
                self.move_to(next);
                tracing::debug!(target: "xgview::app", at = next, "the recording carries on");
                return;
            }
            if let Some(carries) = carries {
                self.move_to(carries);
                tracing::debug!(target: "xgview::app", at = carries, "the recording carries on at the live edge");
                return;
            }
            // Nothing in the list carries on from the stop, so the press plays
            // the recording again. Waiting for a look past the stop is what a
            // press must *not* do: an answer still on its way, or a stop the
            // recorder has not cut its next fragment past yet, leaves the picture
            // exactly where it was, and a press that does nothing looks broken.
            // A stop that really is the live edge carries on by itself, when the
            // next fragment is cut - see [`Self::look_at_edge`] - and a viewer
            // who presses play before that has asked to watch, not to wait.
            self.move_to(restart);
            tracing::debug!(target: "xgview::app", at = restart, "the recording starts again");
            return;
        }
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        playback.paused = !playback.paused;
        let (index, paused) = (playback.index, playback.paused);
        self.manager.hold_clip(index, paused);
        tracing::debug!(target: "xgview::app", index, paused, "playback held or let go");
    }

    /// The recording on this channel reached a hole, and carries on past it if
    /// there is anything there.
    ///
    /// The box serves a playlist up to the first hole and stops: `#EXT-X-ENDLIST`
    /// lands after the last fragment before it. So a session that finished is
    /// not a recording that ended - the picture stops at the edge of a stretch
    /// and the viewer expects the next one, not a frozen frame.
    ///
    /// `nothing_there` is the one stop that is not that: the box answered
    /// "nothing recorded at that moment", so the session never showed a picture
    /// at all. What is on screen belongs to an older session and its stretch is
    /// not the one that stopped, so there is no hole to carry on past - carrying
    /// on from the picture's own moment is a jump to the next recording *after
    /// it*, which was seventeen hours in the report that found this, and it
    /// happened the instant the seek was refused. See [`Self::seek`].
    pub(super) fn recording_stopped(&mut self, index: usize, nothing_there: bool) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.index != index {
            return;
        }
        // A finger on the timeline is choosing where the picture goes, and a
        // continue would take it somewhere else under the hand.
        if playback.scrubbing {
            return;
        }
        // A segment read is on its way for a moment the viewer asked for: when
        // it lands the seek goes through and its own session reports a hole.
        // Nothing else holds this back - a *playlist* on its way, or a moment
        // asked for that has not arrived, is not a reason to leave the picture
        // stopped for ever.
        if playback.to_seek.is_some() {
            return;
        }
        // An arrow held down is a run of steps, each opening a session of its
        // own. The stops of the ones a later step replaced arrive out of order,
        // and this one is not the picture reaching a hole: clearing `pending`
        // would put the cursor back on the frame the step left - which across a
        // hole is still a frame from the stretch before it. Then `next_stretch`
        // answers with the same stretch every time, and `step_landing` measures
        // every step from that same old frame. The strip shuttles between two
        // days instead of walking on, and can never reach the later ones. Let
        // the newest step's moment stand, and let its own session answer.
        //
        // A key that has come up is the other case, including the short grace
        // that follows it: that one stop does belong to the picture, and the
        // recording carries on past its hole as it always did.
        if playback.held.is_some_and(|held| held.is_down()) {
            return;
        }
        // A stop that names the same frame and the same next stretch as the last
        // hop is that hop replaying itself: the session it started ended without
        // putting a frame on the screen, so the picture's moment never moved.
        // Neither the cursor nor the picture is touched again - this is the loop
        // that restarted a session every 20-60 ms and held the strip shuttling
        // between two days, on quick repeated presses as much as on a hold. A
        // frame that arrived, or a different stretch, is a different hop.
        let replay = {
            let Some(playback) = self.playback.as_ref() else {
                return;
            };
            let stop = self.pictured(index).unwrap_or(playback.opened);
            hole_hop_replayed(
                playback.hole_hop,
                stop,
                next_stretch(&playback.segments, &playback.covered, stop),
            )
        };
        if replay {
            return;
        }
        // The moment a seek was waiting for is not coming: the session that
        // would have taken the picture there is the one that has just stopped.
        // What is on screen is the stretch being left, and the moment to act on
        // is the one the picture is of.
        if let Some(playback) = self.playback.as_mut() {
            playback.pending = None;
        }
        if nothing_there {
            return;
        }
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let stop = self.centre_of(playback);
        // Looked past this moment already: the recording is over, and nothing
        // is asked of the box again.
        if playback.after_hole == Some(AfterHole::Nothing(stop)) {
            return;
        }
        // A look is in flight, and this stop is the one it was made for.
        if matches!(playback.after_hole, Some(AfterHole::Looking { .. })) {
            return;
        }
        let next = next_stretch(&playback.segments, &playback.covered, stop);
        let carries = carries_on(&playback.segments, stop);
        if let Some(next) = next {
            if let Some(playback) = self.playback.as_mut() {
                playback.hole_hop = Some((stop, next));
            }
            tracing::debug!(target: "xgview::app", at = next, "the recording carries on past a hole");
            self.move_to(next);
            return;
        }
        // The recording goes on from here: the recorder wrote while the picture
        // played, and the playlist ended at the last fragment it had served.
        // The rest of the file it stopped in is what comes next, so the picture
        // resumes at the stop itself. See [`carries_on`].
        if let Some(next) = carries {
            tracing::debug!(target: "xgview::app", at = next, "the recording carries on at the live edge");
            self.move_to(next);
            return;
        }
        // A stop at the edge of what has been cut is asked about again shortly:
        // the next fragment is being written, and "nothing yet" is not the end.
        // The question goes to the playlist rather than to the file list, because
        // the fragments of the file being written are already being served while
        // that file is listed only when it is closed. See [`AfterHole::Waiting`].
        if stop >= epoch_now() - LIVE_EDGE {
            self.look_at_edge(stop);
        } else {
            self.look_past_hole(stop);
        }
    }

    /// Asks the box for the fragments past a stop, and plays them if they are
    /// there.
    ///
    /// A playlist rather than a segment list: what the picture needs is the next
    /// fragment, and the box serves fragments of the file it is still writing
    /// while listing that file only when it closes it - so a wait on the list is
    /// a wait for a close, ten seconds at the shortest and a minute on a box
    /// configured for long files, while the fragments are already there.
    ///
    /// The answer comes back as a seek's own: [`Self::start_seek`] plays it when
    /// it reaches the stop, and waits again when it does not, so one wait costs
    /// one playlist a poll. See [`stops_short`].
    ///
    /// `false` when there is nothing to ask for: the stop is past the end of a
    /// recording that is no longer being written, and waiting there is a request
    /// every couple of seconds for ever. See [`at_live_edge`].
    fn look_at_edge(&mut self, stop: i64) -> bool {
        let Some(playback) = self.playback.as_ref() else {
            return false;
        };
        let now = epoch_now();
        let newest = listed_to(&playback.segments, now);
        let index = playback.index;
        let source = playback.source.clone();
        let gen = playback.seek_gen;
        let from = edge_from(&playback.segments, stop);
        // A camera that has stopped recording has nothing past the stop and
        // never will: the stop is past the end of the recording rather than at
        // the edge of what has been written, and the wait concludes instead of
        // asking again. It is the same conclusion a stop well behind the clock
        // gets, reached by looking at the box rather than at the clock. See
        // [`at_live_edge`].
        if !at_live_edge(stop, now, newest) {
            tracing::debug!(
                target: "xgview::app",
                stop,
                newest,
                "the recording has nothing past the stop"
            );
            if let Some(playback) = self.playback.as_mut() {
                playback.after_hole = Some(AfterHole::Nothing(stop));
            }
            return false;
        }
        // Armed before the request: an answer that never arrives - a box that
        // could not be asked - is asked for again rather than leaving the picture
        // stopped with nothing on its way. A start clears it. See
        // [`Self::start_seek`] and [`Self::read_strip`].
        if let Some(playback) = self.playback.as_mut() {
            playback.after_hole = Some(AfterHole::Waiting { stop, at: now + LIVE_LOOK });
        }
        tracing::debug!(
            target: "xgview::app",
            index,
            stop,
            from,
            "asking for the fragments past the stop"
        );
        start_playlist(
            &self.handle,
            &self.events_tx,
            index,
            source,
            (from, stop + LIVE_SPAN),
            stop,
            Some(gen),
        );
        true
    }

    /// Reads the segment list past a hole, looking for where the recording
    /// carries on.
    ///
    /// The list held for the strip may end at the hole itself - it was read
    /// around the moment being watched - so the answer that says whether there
    /// is anything after it is a read of its own.
    fn look_past_hole(&mut self, stop: i64) {
        self.look_from(stop, stop, stop + HOLE_LOOK);
    }

    /// Reads one day's segment list, looking for what comes after `stop`.
    ///
    /// From `from`, which is the stop itself the first time and the end of the
    /// last answer after that: a hole longer than a day is looked past a day at
    /// a time. See [`HOLE_LIMIT`].
    fn look_from(&mut self, stop: i64, from: i64, to: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let index = playback.index;
        let source = playback.source.clone();
        if let Some(playback) = self.playback.as_mut() {
            playback.after_hole = Some(AfterHole::Looking {
                stop,
                window: (from, to),
                at: epoch_now() + READ_RETRY,
            });
        }
        tracing::debug!(target: "xgview::app", stop, from, to, "looking past a hole");
        // From the stop itself the first time: the segments the answer holds are
        // the ones that begin after it, and a read that began a second later
        // would leave a second of time nobody has asked about - which is what
        // the next question tests for. See [`covered_between`].
        start_segments(&self.handle, &self.events_tx, index, source, from, to);
    }

    /// Whether the picture on the strip is standing still - held, or out of
    /// range.
    ///
    /// The same answer the tile's corner gives (see [`still`]), for the control
    /// that draws the mark on the bar: one rule, so the two cannot disagree
    /// about whether the picture is moving.
    pub(super) fn playback_still(&self) -> bool {
        let Some(playback) = self.playback.as_ref() else {
            return false;
        };
        still(playback.paused, self.channels.get(&playback.index).map(|channel| channel.state))
            || playback.waiting()
    }

    /// Whether the recording has played to the end of its range.
    fn recording_ended(&self) -> bool {
        let Some(playback) = self.playback.as_ref() else {
            return false;
        };
        self.channels.get(&playback.index).map(|channel| channel.state)
            == Some(ConnectionState::Finished)
    }

    /// Moves the picture by `by` seconds.
    ///
    /// Held, the step is the whole of what happens: a seek while the session is
    /// paused lands on one frame and stays there - the session publishes the
    /// sample it lands on and goes back to holding. Playing, it is a jump and
    /// the recording carries on from there.
    pub(super) fn step_playback(&mut self, by: i64) {
        self.step_by(by);
    }

    /// Takes the step `by` seconds, or reads for one when there is nowhere to step.
    ///
    /// The two arrows that walk the recording come here: the strip's own keys
    /// ([`Self::walk_timeline`]) and the bar's step buttons. A step is a walk,
    /// and a hole is a stretch the recording does not have: the next picture in
    /// the direction of the step is what a press asks for. A drag is the other
    /// way round - a finger over a hole means the nearest picture - and that is
    /// [`nearest`], which is why this cannot live there.
    ///
    /// A step back that is already at the oldest picture the list holds is not a
    /// step: what is behind it has not been read yet, and the answer to the press
    /// is a read of its own ([`Self::read_before_the_list`]). Without it the
    /// strip's left edge was the end of the recording as far as a key could tell,
    /// and holding the key there did nothing at all.
    fn step_by(&mut self, by: i64) {
        match self.step_landing(by) {
            Some(at) => self.move_to(at),
            // Only a step back has nowhere to go: a step forward is never
            // clamped to where it started, because what is past the last file is
            // a recording the box has not listed yet. See [`Self::step_landing`].
            None if by < 0 => {
                if let Some(index) = self.playback.as_ref().map(|playback| playback.index) {
                    self.read_before_the_list(index);
                }
            }
            None => {}
        }
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
            // Which list the jump is waiting for, so that an answer for another
            // window is not mistaken for it. See [`Self::events_arrived`].
            playback.jump_ask = Some((from, to));
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
            .or_else(|| self.pictured(playback.index))
            .unwrap_or(playback.opened)
    }

    /// The moment the picture is actually on, ignoring the cursor.
    ///
    /// What a seek leaves behind is the frame on screen, not where the cursor
    /// is showing: a finger on the strip or a seek still on its way puts the
    /// cursor somewhere else. See [`Playback::pending_from`].
    fn pictured(&self, index: usize) -> Option<i64> {
        // The moment rather than the picture: a decoder given a surface renders
        // into it and hands nothing back, so the store is told what the picture
        // is of even when there is no frame to draw. Reading the frame's own
        // moment instead left the cursor at the moment the recording opened,
        // for as long as it played.
        self.manager.frames().moment(index).map(|at| at as i64)
    }

    /// The stretch the strip is drawn across: the width it is showing, centred
    /// on the cursor.
    fn window_of(&self, playback: &Playback) -> (i64, i64) {
        let centre = self.centre_of(playback);
        let half = playback.zoom / 2;
        (centre - half, centre + half)
    }

    /// Reads the events of the stretch the timeline shows.
    ///
    /// A deliberate read - a recording opened, a width chosen, a day picked - so
    /// it is not held back by [`LIVE_SLICE`]: whatever is uncovered is asked for
    /// at once. What the box has already answered for is not asked again, so the
    /// press that lands on the window the strip is already showing costs nothing.
    /// See [`Self::read_strip`], which is the same read every frame with the wait
    /// on it, and [`Self::read_events_at`] for a moment the cursor is not on yet.
    fn read_events(&mut self, index: usize) {
        let Some(centre) = self
            .playback
            .as_ref()
            .map(|playback| self.centre_of(playback))
        else {
            return;
        };
        self.read_events_at(index, centre, 1);
    }

    /// The same read, for a moment the cursor is not on yet.
    ///
    /// A drag's end is the case: the strip is about to be centred on the moment
    /// the finger left, and the picture has not moved there yet - so `centre_of`
    /// still answers with the old one, and the marks read for it would be the
    /// marks of the stretch being left.
    fn read_events_at(&mut self, index: usize, centre: i64, least: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let ask = events_to_ask(
            &playback.events_cover,
            centre,
            playback.zoom,
            epoch_now(),
            least,
        );
        if let Some((from, to)) = ask {
            self.read_events_between(index, from, to);
        }
    }

    /// Reads the events of a stretch. One at a time: a second ask while the
    /// first is in flight would be two answers for one list - so it is
    /// remembered instead, and made when the answer lands. Dropping it left a
    /// strip widened while a read was in flight with no marks on the half that
    /// was just uncovered.
    fn read_events_between(&mut self, index: usize, from: i64, to: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.reading_events {
            if let Some(playback) = self.playback.as_mut() {
                playback.events_wanted = Some((from, to));
            }
            return;
        }
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, from, to, "reading the events");
        if let Some(playback) = self.playback.as_mut() {
            playback.reading_events = true;
            playback.events_ask = Some((from, to));
            playback.events_wanted = None;
        }
        start_events(&self.handle, &self.events_tx, index, source, from, to);
    }

    /// Takes the events of a stretch. A box that could not be asked leaves the
    /// list empty, which is a timeline with no marks rather than an error.
    pub(super) fn events_arrived(
        &mut self,
        index: usize,
        camera_id: String,
        from: i64,
        to: i64,
        events: Option<Vec<(i64, i64)>>,
    ) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        if playback.index != index || playback.source.camera != camera_id {
            return;
        }
        // The answer is the list only when it answers the read that is in
        // flight. Another window's answer is dropped rather than drawn: the
        // list is for one window, and the strip it is drawn on has moved on.
        if playback.events_ask != Some((from, to)) {
            tracing::debug!(target: "xgview::app", from, to, "dropping an answer for another window");
            return;
        }
        playback.events_ask = None;
        playback.reading_events = false;
        // A read for a jump reaches past the strip - a day either way, from the
        // moment on screen - and what it finds is added to the events the strip is
        // drawing rather than replacing them. Replaced, a press at the last event
        // emptied the strip: the answer for the day ahead held nothing, so the
        // marks of the stretch being watched went with it. A read of the strip's
        // own is still the list, because it is asked for the window it draws. See
        // [`Self::jump_event`].
        let jumping = playback.jump_ask == Some((from, to));
        let found = events.as_ref().is_some_and(|events| !events.is_empty());
        if let Some(events) = events {
            if jumping {
                playback.events.extend(events);
                playback.events.sort_by_key(|event| event.0);
                playback.events.dedup_by_key(|event| event.0);
            } else {
                playback.events = events;
            }
            // What the box has now been asked about. Every ask covers the window
            // the strip draws and a window either side of it, so a list that
            // replaces the last one still holds everything on screen - which is
            // what makes replacing safe here and what the cover is for.
            cover(&mut playback.events_cover, from, to);
        } else {
            // The box could not answer: the marks are the ones that were there,
            // and the read is not made again on the next frame. See
            // [`READ_RETRY`].
            playback.retry_events = epoch_now() + READ_RETRY;
        }
        // A jump was waiting for this list: it is made now, but only when this
        // is the list it asked for. A press that had to be remembered while
        // another read was in flight is not spent on that one - its own read
        // comes after. `to_jump` is cleared first, so a second answer that still
        // holds nothing says so instead of reading on.
        //
        // Whether it is made is what *this answer* held, not what the strip holds:
        // the strip's own marks are always there, and a press with nothing ahead
        // would otherwise jump to the event it is already on, for ever.
        let mut jumped = None;
        if playback.jump_ask == Some((from, to)) {
            let forward = playback.to_jump.take();
            playback.jump_ask = None;
            if let Some(forward) = forward.filter(|_| found) {
                jumped = Some(forward);
            }
        }
        // What was asked for while this read was in flight - a widened strip, or
        // the jump above - is asked for now.
        let wanted = playback.events_wanted.take();
        let jump_next = playback.to_jump.and(playback.jump_ask);
        // What was asked for while this read was in flight may already be
        // answered by it: the strip asks every frame, so the window it remembered
        // is often the window this answer covered. Asking again for a range the
        // cover now holds is a request for marks already on screen.
        let next = wanted
            .or(jump_next)
            .filter(|(from, to)| uncovered_width(&playback.events_cover, *from, *to) > 0);
        if let Some(forward) = jumped {
            self.jump_event(forward);
        }
        if let Some((from, to)) = next {
            self.read_events_between(index, from, to);
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
        // The picture this seek is leaving, read before the session is replaced:
        // the frames of the one being left are what it publishes until the new
        // one has something to show, and they are told apart from its own by
        // being nearer this moment than the one asked for. See
        // [`Self::picture_arrived`].
        let from = self.pictured(index);
        // A finger on the timeline is choosing a moment, and an answer that lands
        // under it is not the finger's: taking the drag's cursor away is a strip
        // that jumps back under the hand - which is what a probe answering
        // mid-drag looked like, a couple of seconds after the wait armed. The
        // drag's own seek is made when the finger is lifted, and that seek
        // numbers itself, so this answer is dropped rather than kept. See
        // [`Self::end_scrub`] and [`Self::timeline_input`].
        if playback.scrubbing {
            tracing::debug!(
                target: "xgview::app",
                index,
                at,
                "a seek under a finger is dropped"
            );
            return;
        }
        // The range a playlist was asked for reaches back before the moment, so
        // the picture begins inside it. A box that serves no playlist gets a
        // `clip.mp4`, which begins where it is asked for and no earlier.
        //
        // What the moment is held to is the playlist, not the range's end: the box
        // serves the file it is still writing before it lists it, so the last
        // second of the segment list is inside the playlist. Held to the range's
        // end instead (`begin_in`), a drag to the very end of the band began a
        // second behind where the finger stopped - the picture came back a second
        // and then went on, which is the jump this fixed. A moment the playlist
        // does not reach is what [`stops_short`] is for, and it waits there rather
        // than starting a session.
        //
        // The playlist's own end, and not the list's: the segment list ceils a
        // file's end, so the last second it names can be a second the box has no
        // fragment for. Beginning there is a session that never shows a picture -
        // the box answers "nothing recorded at that moment" - which is a drag that
        // springs back to where it started rather than the nearest picture. A
        // drag to the last second of a stretch is exactly that moment.
        //
        // Not at the live edge, though. There the playlist stops short because the
        // recorder has not cut the next fragment yet, and clamping the moment to it
        // is a drag to the end of the band that lands half a minute earlier - the
        // fragments behind the finger are still to come, and the wait below is the
        // answer. See [`at_live_edge`].
        let now = epoch_now();
        let newest = listed_to(&playback.segments, now);
        let live = at_live_edge(at, now, newest);
        let (window, begin) = match &playlist {
            Some(playlist) => (window, Some(served_begin(playlist, window, at, live))),
            None => match window_at(&playback.segments, at, WINDOW) {
                Some(window) => (window, None),
                // Nothing to play there and no session started: the hold a
                // press took has to be let go of, or the picture stops for ever -
                // and so does the moment it was waiting for, or the cursor waits
                // for a picture that is not coming. The list has no file ending
                // after the moment, which is a moment past everything it holds.
                None => {
                    self.release_hold(index);
                    if let Some(playback) = self.playback.as_mut() {
                        playback.to_seek = None;
                        playback.pending = None;
                    }
                    return;
                }
            },
        };
        let source = playback.source.clone();
        let (speed, paused) = (playback.speed, playback.paused);
        // A playlist holds only the fragments the box has served when its range
        // is read, and one asked for across the live edge can stop before the
        // moment the seek wants to begin at. Starting a session on it would end
        // where it began, and the stop would ask for the same moment again -
        // one request after another. The fragments after it are still being
        // written; the picture waits for them. See [`stops_short`].
        if let (Some(playlist), Some(begin)) = (&playlist, begin) {
            if stops_short(playlist, begin) {
                let wait = after_stop(begin, now, newest);
                // A stop near the clock is the recorder having not cut the next
                // fragment yet, and is asked about again shortly. Well behind it
                // is the edge of the recording the seek landed on, where there
                // is nothing to wait for: the fragments that are there are
                // played, and the stop carries the picture past the hole. See
                // [`AfterHole::Waiting`].
                if let AfterHole::Waiting { .. } = wait {
                    tracing::debug!(
                        target: "xgview::app",
                        index,
                        at = begin,
                        served = playlist.starts_at(playlist.fragments.len()),
                        "the playlist stops where it starts"
                    );
                    // The picture the seek is leaving is held while the box
                    // cuts what comes next, rather than carried on with: the
                    // viewer is watching a moment this session cannot reach.
                    if self.has_session(index) {
                        self.manager.hold_clip(index, true);
                    }
                    if let Some(playback) = self.playback.as_mut() {
                        playback.ahead = None;
                        playback.awaiting = None;
                        playback.scrub = None;
                        playback.scrubbing = false;
                        playback.pending = Some(begin);
                        // The picture the wait is leaving is what it keeps
                        // showing, so its frames are not an arrival either.
                        playback.pending_from = from.filter(|from| *from != begin);
                        playback.after_hole = Some(wait);
                    }
                    return;
                }
            }
        }
        let vod = self.play_recording(index, &source, window, playlist, begin);
        // A new session starts at normal speed, so the rate the viewer chose is
        // put back on it: the control says "2x" and the picture has to agree.
        self.manager.set_rate(index, speed);
        // And it starts playing, so a picture the viewer had held is held
        // again: a drag says where to look, not that it should start.
        if paused {
            self.manager.hold_clip(index, true);
        }
        if let Some(playback) = self.playback.as_mut() {
            playback.scrub = None;
            playback.scrubbing = false;
            playback.vod = vod;
            // The seek this drag was going to make has been made.
            playback.ahead = None;
            playback.awaiting = None;
            // And whatever the picture was waiting for is this: a session started
            // is a wait over, and one left armed would have the strip ask for the
            // same fragments again - a playlist, a session and a stop, one after
            // another. See [`AfterHole`].
            playback.after_hole = None;
            // The moment the picture begins at, held to the stretch that will
            // play: a scrub taken to the edge of the nearest recording does not
            // wait for a moment this playlist never reaches.
            let begin = begin.unwrap_or(window.0);
            playback.pending = Some(begin);
            playback.pending_from = from.filter(|from| *from != begin);
        }
    }

    /// Whether an answer to a range read belongs to the newest seek.
    ///
    /// A seek made while another range is still being read is the one that
    /// counts: playing the older answer would put the picture at a moment the
    /// viewer has already left.
    pub(super) fn seek_is_current(&self, gen: u64) -> bool {
        self.playback.as_ref().is_some_and(|playback| playback.seek_gen == gen)
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
        // The bar's row is not the row it was: the recording's controls are gone
        // and the camera's are back. The focus was on a place in the old row, and
        // the place means something else now - so it goes to the seat, where the
        // first arrow picks a control by name rather than by number.
        self.restore_focus = Some(tilebar::seat());
        self.manager.stop_clip(playback.index);
        self.channels.remove(&playback.index);
        self.textures.remove(&playback.index);
        self.video.release(playback.index);
        // The camera's own session is wanted again: the viewport shows the
        // camera once more, and it has had no decoder for as long as the
        // recording was up.
        self.scheduler.set_recording(None);
        self.needs_sync = true;
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
        // The remote is on the strip: the ring is the one this application draws
        // for "the keys are here", and it is **taken from the style rather than
        // named**, so that it is the same ring the bar's controls are given -
        // whatever visuals the platform ended up with.
        if self.strip_has_focus(ui.ctx()) {
            let ring = ui.style().visuals.widgets.active.bg_stroke;
            ui.painter().rect_stroke(
                row,
                CornerRadius::same(theme::radius::M),
                ring,
                StrokeKind::Inside,
            );
        }
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
        // The stretches that were recorded, merged when the list last changed:
        // the recorder's own file boundaries are fractions of a second apart,
        // and drawing them would put a notch every ten seconds along a recording
        // that is continuous.
        //
        // The last of them is drawn as far as the newest moment the recording is
        // known to hold, which is also where a drag stops - one answer for the
        // drawing and the dragging, so the cursor cannot stand on empty track. The
        // file the recorder is writing is listed only once it is closed, so without
        // it the band would stop a file short of the picture. See
        // [`Self::recorded_to`].
        //
        // One rectangle and not the stretch plus a piece stuck on its end, which is
        // what this was: two rounded ends meeting are two caps, and a cap is a
        // quarter of a circle curving away from the join - so a band that is one
        // stretch with a few seconds added to it reads as two pieces with a gap
        // between them, at exactly the place the viewer is looking (the end of the
        // recording).
        let newest = self.recorded_to(playback);
        let stretches = &playback.stretches;
        for (index, segment) in stretches.iter().enumerate() {
            let stop = if index + 1 == stretches.len() {
                segment.end.max(newest.unwrap_or(segment.end))
            } else {
                segment.end
            };
            if stop <= start || segment.start >= end {
                continue;
            }
            let from = along(segment.start.max(start));
            let to = along(stop.min(end));
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
    /// The day the picture is on is the one that looks chosen, and **the day the
    /// remote is on is the one that is ringed**: picking a date is a move to
    /// another one, and the date already showing is the way back to the strip.
    /// With the ring on the cursor the two are told apart - the cursor is where a
    /// press would go, the brighter button is where the picture already is.
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
        let here = playback
            .days
            .iter()
            .rposition(|day| day.start <= now)
            .map(|index| playback.days[index].start);
        // The ring is the remote's, so it is on the cursor while the remote is
        // here, and on the day the picture is on when it is not.
        let focused = self.strip_has_focus(ui.ctx());
        let cursor = focused.then_some(playback.day_cursor).flatten();
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
            // not here at all, so every one of them is filled - the one the
            // picture is on a shade brighter.
            let here_day = here == Some(day.start);
            let fill = if here_day { 0.75 } else { 0.45 };
            painter.rect_filled(band, CornerRadius::same(6), theme::ACCENT.gamma_multiply(fill));
            let ringed = match cursor {
                Some(cursor) => cursor == index,
                None => here_day,
            };
            if ringed {
                painter.rect_stroke(
                    band,
                    CornerRadius::same(6),
                    egui::Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS),
                    StrokeKind::Inside,
                );
            }
            if each >= 34.0 {
                // Turned with the picture, like the times above the strip: a
                // calendar on a picture drawn sideways runs down the screen, and
                // a label written across it would be the one thing on that side
                // of the wall that is not.
                turned_text(
                    &painter,
                    band.center(),
                    day.label.clone(),
                    DAY_SIZE,
                    theme::TEXT_BRIGHT,
                    !horizontal,
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
    /// Moves the calendar's ring a day under the arrows, and slides the row to
    /// keep it in view.
    ///
    /// A day a press, and not a day a frame: a held arrow is not repeated into a
    /// flood, and the row is as many days as the camera has recorded - a press
    /// each. The cursor is where the ring is drawn and where the confirm key
    /// picks, so it is the one thing the row has to keep on screen.
    fn move_day_cursor(&mut self, row: Rect, forward: bool) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let last = playback.days.len().saturating_sub(1);
        if playback.days.is_empty() {
            return;
        }
        let at = playback.day_cursor.unwrap_or(0);
        let at = if forward { (at + 1).min(last) } else { at.saturating_sub(1) };
        playback.day_cursor = Some(at);
        // A row that does not move with it is a ring that walks off the edge.
        let (each, total) = day_metrics(row, playback.days.len());
        let length = if row.width() > row.height() { row.width() } else { row.height() };
        playback.days_scroll = day_scroll_for(at, each, playback.days_scroll, length, total);
    }

    /// Picks the day the calendar's ring is on, for the confirm key.
    pub(super) fn pick_day_cursor(&mut self) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.timeline != Timeline::Days {
            return;
        }
        let index = playback.index;
        let day = playback
            .day_cursor
            .and_then(|cursor| playback.days.get(cursor))
            .cloned();
        if let Some(day) = day {
            self.pick_day(index, &day);
        }
    }

    /// Puts the ring on the day the picture is on, when the calendar is first
    /// shown: the remote opens where the viewer already is.
    fn start_day_cursor(&mut self) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.day_cursor.is_some() || playback.days.is_empty() {
            return;
        }
        let now = self.centre_of(playback);
        let at = playback
            .days
            .iter()
            .rposition(|day| day.start <= now)
            .unwrap_or(0);
        if let Some(playback) = self.playback.as_mut() {
            playback.day_cursor = Some(at);
        }
    }

    /// The id the strip is focused by: the row's own, claimed by
    /// [`XgViewApp::timeline_input`] and the thing the keys that enter it ask
    /// for.
    pub(super) fn timeline_id(&self) -> Option<Id> {
        self.playback
            .as_ref()
            .map(|playback| Id::new(("xgview-timeline", playback.index)))
    }

    /// Answers a finger on the timeline.
    ///
    /// Pressing holds the picture where it is - a recording is paced by its own
    /// timestamps, so that is the only way to stop it - and the cursor follows
    /// the finger with nothing asked of the server. Letting go asks for the
    /// stretch under the finger and plays it from there.
    pub(super) fn timeline_input(&mut self, ui: &egui::Ui, row: Rect) {
        // The calendar's ring is the remote's, and it opens on the day the picture
        // is on - before any arrow can move it, which is why this is the first
        // thing the frame does.
        if self
            .playback
            .as_ref()
            .is_some_and(|playback| playback.timeline == Timeline::Days)
        {
            self.start_day_cursor();
        }
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
        let id = self.timeline_id().unwrap_or_else(|| Id::new("xgview-timeline"));
        let response = ui.interact(row, id, Sense::click_and_drag());
        // egui walks the focus itself when an arrow is pressed and the focused
        // widget does not claim that arrow: the widget in that direction gets
        // the focus instead, which is the opposite of what a viewer walking the
        // strip is asking for - one press and the strip is gone. The navigation
        // layer locks the arrows for its own controls (see `Nav::finish`); the
        // strip is not one of them, so it locks them here.
        if response.has_focus() {
            ui.ctx().memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            });
        }
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
            && tilebar::down_enters_strip(ui.memory(|memory| memory.focused()))
        {
            // Where it came from, so Up can put it back there rather than at the
            // head of the row: a viewer who walked to the sixth control to get
            // here does not want to walk back from the first.
            let from = ui.memory(|memory| memory.focused()).and_then(tilebar::index_of);
            if let Some(playback) = self.playback.as_mut() {
                playback.from_control = from;
            }
            response.request_focus();
        }
        if response.has_focus() {
            if up_key {
                // Back to the control it left, or to the first when it came from
                // the seat - which is not a control and cannot take it back.
                let back = self
                    .playback
                    .as_ref()
                    .and_then(|playback| playback.from_control)
                    .unwrap_or(0);
                ui.memory_mut(|memory| memory.request_focus(tilebar::control(back)));
            } else if days {
                // The calendar is a row of buttons and the remote walks it: the
                // arrows move the ring a day at a time, and the row slides to
                // keep it in view. See [`Self::move_day_cursor`].
                if left_press {
                    self.move_day_cursor(row, false);
                } else if right_press {
                    self.move_day_cursor(row, true);
                }
            } else if left || right || left_press || right_press {
                // A front end that drains every queued event into one frame can
                // deliver a press's down and up together. egui has then taken
                // the key out of `keys_down` before the strip reads it, so
                // `key_down` says nothing while `key_pressed` says the press
                // came. Either way this is one call: `fresh` and the repeat
                // clock are what decide whether it steps.
                self.walk_timeline(right || right_press);
            } else if let Some(playback) = self.playback.as_mut() {
                // Not the end of the hold yet: a release that is part of one
                // press is only a gap between two frames, and throwing the
                // hold away here would make its next down a fresh press. It
                // goes when the grace has run out instead.
                let now = std::time::Instant::now();
                let lapsed = playback.held.as_ref().is_some_and(|held| {
                    held.released
                        .is_some_and(|at| now.saturating_duration_since(at) >= RELEASE_GRACE)
                });
                if lapsed {
                    playback.held = None;
                } else if let Some(held) = playback.held.as_mut() {
                    held.released.get_or_insert(now);
                }
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
                        // **No focus**: a finger or a mouse on the strip is
                        // asking for the time, not for the arrows - the ring is
                        // the remote's, and a press does not put it here. The
                        // keyboard enters by Down, which is a different line and
                        // a different intention.
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
                    // nothing on its left. The same on the right: a drag towards
                    // later moments runs off the end of what has been read, and a
                    // strip that only ever grows to the left has a right half
                    // that is always a hole.
                    self.read_older(index);
                    self.read_newer(index);
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
        // one it shows now, the picture not having moved yet.
        self.read_events_at(index, at, 1);
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
        // The marks are kept: an event is a stretch of time, and a strip of
        // another width draws the same ones - wider, or fewer of them, but not
        // different. Cleared, every change of width blanked the strip until the
        // answer landed. What the wider window reaches beyond them is read below.
        let index = playback.index;
        tracing::debug!(target: "xgview::app", zoom, "the timeline changed width");
        self.read_events(index);
        // Both halves: a wider strip reaches further either way, and only the
        // left one used to be read - so widening the strip left its right half
        // empty until something else moved the cursor.
        self.read_older(index);
        self.read_newer(index);
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
                // And the remote starts there too: the ring opens on the date
                // the picture is already showing.
                playback.day_cursor = None;
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

    /// Plays faster, or back to normal: one, two, four, eight, one.
    ///
    /// Eight is where the ladder ends. The session divides the wall clock by the
    /// rate, so the picture advances eight seconds per second - and the sound is
    /// left out of anything but one, because there is nothing here that could
    /// resample it.
    pub(super) fn cycle_speed(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let speed = next_speed(playback.speed);
        playback.speed = speed;
        let index = playback.index;
        self.manager.set_rate(index, speed);
        tracing::debug!(target: "xgview::app", index, speed, "playback rate");
    }

    /// The word the speed control carries: `1×`, `2×`, `4×` or `8×`.
    pub(super) fn playback_speed_label(&self) -> &'static str {
        speed_label(self.playback.as_ref().map_or(1.0, |playback| playback.speed))
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
    /// "a little further" at every level. **A press steps at once and a held key
    /// repeats on its own clock**, which is what [`hold_step`] answers: a key
    /// that is down is down every frame, and a step a frame would cross the strip
    /// three times a second.
    pub(super) fn walk_timeline(&mut self, forward: bool) {
        let now = std::time::Instant::now();
        let Some(playback) = self.playback.as_mut() else {
            return;
        };
        let base = (playback.zoom / 20).max(1);
        // A press - or a change of direction - steps at once, and starts both
        // clocks: the hold's, which the step grows with, and the repeat's. A
        // release inside [`RELEASE_GRACE`] is the same press, so neither clock
        // moves and the repeat goes on waiting on the one it already had.
        let fresh = playback
            .held
            .map_or(true, |held| held.restarted(forward, now));
        if fresh {
            playback.held = Some(Held {
                forward,
                since: now,
                stepped: now,
                released: None,
            });
        } else if let Some(held) = playback.held.as_mut() {
            held.released = None;
        }
        let Some(held) = playback.held else {
            return;
        };
        let (step, repeat) = hold_step(base, now.saturating_duration_since(held.since));
        // A key that is down is the same key every frame, so the repeat cannot
        // begin until the key has been held longer than a finger holds one: a
        // press of 160 ms read as two steps, which is a step that looks like it
        // went twice as far. Nothing repeats before [`REPEAT_DELAY`].
        if !fresh
            && (now.saturating_duration_since(held.since) < REPEAT_DELAY
                || now.saturating_duration_since(held.stepped) < repeat)
        {
            return;
        }
        if let Some(playback) = self.playback.as_mut() {
            if let Some(held) = playback.held.as_mut() {
                held.stepped = now;
            }
        }
        self.step_by(if forward { step } else { -step });
    }

    /// Where a step of `by` seconds lands, taken across a hole.
    ///
    /// The landing [`Self::step_by`] takes, once the list has said where the
    /// recording goes and where it stops. A hole is a stretch the recording does not
    /// have, and what a step asks for is the next picture in the direction it
    /// went - not the edge it came from, which is what [`nearest`] answers and
    /// what a finger wants. See [`across_hole`].
    ///
    /// `None` when there is nothing to do: no recording open, or a step back that
    /// is already as far back as the list goes. The second is not the same as a
    /// step of nothing - a held key repeats, and a landing where the picture
    /// already is would be a seek to the same fragment ten times a second.
    fn step_landing(&self, by: i64) -> Option<i64> {
        let playback = self.playback.as_ref()?;
        let centre = self.centre_of(playback);
        let at = across_hole(&playback.segments, centre + by, by > 0);
        // A step back stops at the oldest picture the list holds. A moment before
        // it is a read that can only answer "nothing there" - the box has said so,
        // or the strip's left edge has not been read yet - and the cursor would
        // come back to where it started, so the same press would do it again for
        // as long as the key was held. A step forward is not held this way: what
        // is past the last file is a recording the box has not listed yet, and
        // the read is the right answer.
        //
        // The list's first file is a provisional edge, not the recording's: it is
        // where what has been read begins. A step that stops here is answered with
        // a read by [`Self::step_by`], and the edge moves back when one finds
        // more.
        let at = if by < 0 {
            playback.segments.first().map_or(at, |first| at.max(first.start))
        } else {
            at
        };
        (at != centre).then_some(at)
    }

    /// Takes the picture to a moment, keeping the hold it was under.
    fn move_to(&mut self, at: i64) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let (index, paused) = (playback.index, playback.paused);
        let at = self.clamp_move(at);
        self.seek(at);
        // The strip is drawn a whole width around the cursor, so the list has to
        // reach half a width behind it - and it is the same obligation whether the
        // cursor was moved by a finger, by an arrow or by an event. Asked for
        // here, which is where all three pass; without it a held arrow walked the
        // strip off the end of what had been read and left a band of nothing on
        // the left, which only a drag ever filled. See [`Self::read_older`].
        //
        // And the same on the right: nothing else reads forward, so without it
        // the strip runs out at the cursor and every drag towards later moments
        // ends in a request for a moment nobody has asked the box about.
        self.read_older(index);
        self.read_newer(index);
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
    /// The far end is where the band ends - see [`Self::recorded_to`] - so that
    /// what a drag does and what the strip draws are the same thing: the cursor
    /// never stands on empty track. It used to be `now`, which is a moment the
    /// recording may not have reached yet, and a drag could leave the cursor a
    /// fragment to the right of the band with nothing under it.
    ///
    /// The near end is the oldest moment the camera *has* - `oldest`, once a read
    /// has said where the recording begins - and not the first segment of the
    /// list: the list is what has been asked for, and a drag that stopped at the
    /// end of it would be a strip that could not be pushed back into a history
    /// nobody had read yet. Until a read has said where the beginning is, there
    /// is no bound at all, and the reads follow the finger.
    ///
    /// This is the **finger's** bound. A step, a jump to an event and the recording
    /// carrying on past a hole are [`Self::clamp_move`]'s, which is `now`: what a
    /// hand points at should play at once, but what the viewer asks for by name may
    /// be a moment the box is about to serve.
    fn clamp_centre(&self, at: i64) -> i64 {
        let Some(playback) = self.playback.as_ref() else {
            return at;
        };
        let newest = self.recorded_to(playback).unwrap_or_else(epoch_now);
        clamp_to(at, playback.oldest, newest)
    }

    /// A moment kept inside the recording, for a move no finger made: a step, a jump
    /// to an event, or the recording carrying on past a hole.
    ///
    /// The far end is **now** - a recording has nothing in the future - and not the
    /// band's own edge, which is what a drag is held to. Those two are different
    /// questions: a hand on the strip should not be able to point at a moment that
    /// cannot be played, but a step or a jump is a decision about where the picture
    /// goes, and a moment the box has not cut yet is a wait, not a refusal. Held to
    /// the band, the picture could not cross a hole at all: the recording carried on
    /// past one, the moment it was carried on to was pulled back to where it had
    /// stopped, and a session restarted there every half second.
    fn clamp_move(&self, at: i64) -> i64 {
        let Some(playback) = self.playback.as_ref() else {
            return at;
        };
        clamp_to(at, playback.oldest, epoch_now())
    }

    /// The newest moment the strip draws as recorded - and therefore where a drag
    /// stops.
    ///
    /// Three things say it, and the newest of them wins: the last file the box has
    /// listed, the end of the playlist being played, and the moment the picture is
    /// on. Each is counted only while it is within [`LIVE_EDGE`] of the listed one -
    /// a picture a day away from the last stretch is another day, and a band drawn
    /// across the hole between them would be a lie.
    ///
    /// It is the **recorded** end and not the end that can be played from at once.
    /// That was tried (`min` of the listed file and the served end) and it is a wall:
    /// when the picture reaches the end of its playlist - which is every time the
    /// recording is about to carry on - the two are the same moment, and the cursor
    /// cannot be dragged forward at all. The wait a drag to the recorded end costs is
    /// the box cutting the fragment, and a wait the viewer can point at is better
    /// than a stretch they cannot.
    ///
    /// `None` when the list holds nothing: there is no band, and a bound taken from
    /// it would pin the cursor to a strip that has not been read.
    fn recorded_to(&self, playback: &Playback) -> Option<i64> {
        let listed = playback.stretches.last()?.end;
        let served = playback.vod.map_or(listed, |(_, end)| end);
        let pictured = self.pictured(playback.index).unwrap_or(listed);
        let newest = listed.max(served).max(pictured);
        Some(if newest - listed <= LIVE_EDGE { newest } else { listed })
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

    /// Asks the box which days it recorded on, once per recording: a day does not
    /// appear or disappear while the viewer is looking at it.
    ///
    /// Read when playback opens rather than when the calendar is first shown -
    /// see [`Self::start_playback`] - and again from [`Self::toggle_calendar`]
    /// only if that first read has not answered: a box that was slow enough for
    /// the viewer to reach the date control first still gets asked.
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

    /// Reads the strip's window from its left edge up to the cursor.
    ///
    /// The window, not "the gap between the left edge and the first recording":
    /// the list is a union of everything ever read, so a day looked at earlier
    /// leaves a hole between it and this one, and measured from the recordings
    /// alone that hole looked like a stretch the list already held. The strip
    /// then kept its band of nothing while the events, read for the window
    /// itself, were drawn across it.
    ///
    /// Asked for a slice at a time, and only where the box has not been asked
    /// about: a day at once is a megabyte and seconds of strip drawn empty at
    /// the edge that was just uncovered. See [`READ_CHUNK`] and
    /// [`read_slice_back`].
    ///
    /// Each slice is the one *beside the band already drawn*, so the band grows
    /// out towards the edge the drag uncovered. Read the other way - a slice at
    /// the edge, then the next one inwards - and two bands meet in the middle,
    /// which reads as two strips rather than one.
    fn read_older(&mut self, index: usize) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // One at a time: the answer widens the strip, which may then want more.
        if playback.reading_older.is_some() {
            return;
        }
        // A read that came back short of what it asked for waits before it is
        // asked again. The strip's own frame makes that check; a drag calls this
        // once per cursor position and does not, and forty segment reads a second
        // is a box too busy to cut the fragment the picture is waiting for. See
        // [`READ_RETRY`].
        if epoch_now() < playback.retry_older {
            return;
        }
        let (start, _) = self.window_of(playback);
        let centre = self.centre_of(playback);
        // Back no further than the moment the camera answered "nothing" for.
        let from = playback.oldest.map_or(start, |oldest| start.max(oldest));
        // A finger on the strip asks for a slice at a time, so the band follows
        // it; with the finger up the stretch it left behind is read in one
        // request, which the box answers in about as long as a slice. See
        // [`READ_CHUNK`] and [`READ_WHOLE`].
        let width = if playback.scrubbing { READ_CHUNK } else { READ_WHOLE };
        let Some((from, to)) = read_slice_back(&playback.covered, from, centre, width) else {
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

    /// Reads the stretch before the oldest file the list holds.
    ///
    /// What a step that has nowhere to go is answered with, and not something
    /// [`Self::read_older`] can do: that one works inside the strip's window, and
    /// the window is centred on the cursor - which is exactly where a step back
    /// has got stuck. The window has to move before `read_older` can reach
    /// further, and the window moves with the cursor, so nothing moves. At the
    /// second-narrowest width the window reaches five minutes back and the band
    /// never grows again.
    ///
    /// Asked from the list's own edge instead, a day at a time, and never
    /// *including* that edge: an answer that held the list's first file would be
    /// read as "the recording begins here" by [`Self::segments_arrived`], which is
    /// how a strip declares its left edge to be the beginning of history. What is
    /// behind the list is unknown, and an empty answer for a day of it says only
    /// that this day held nothing.
    fn read_before_the_list(&mut self, index: usize) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // One at a time: the answer extends the strip, which may then want more.
        if playback.reading_older.is_some() {
            return;
        }
        let Some(first) = playback.segments.first().map(|segment| segment.start) else {
            return;
        };
        // Where the camera answered "nothing" for, there is nothing to ask about.
        if playback.oldest.is_some_and(|oldest| oldest >= first) {
            return;
        }
        // The walk is over ground already asked about, so the wait a *failed*
        // read takes is not for this one: an empty day here is progress.
        if epoch_now() < playback.retry_older {
            return;
        }
        // One day of it, and only the day next to what has been asked about: the
        // slice is the one *beside* the band, so the request is a day wide however
        // far the walk has gone. Asking from the list's edge every time - which is
        // what this did - is a request that grows with the walk, and by its end
        // eight days of files in one answer. See [`read_slice_back`].
        //
        // The floor is how far a step will look. A hole wider than a week is not
        // crossed by an arrow - the calendar is for days - and without a bound the
        // walk keeps asking for days the camera never had, for as long as the key
        // is held. `oldest` is left alone either way: this is where a *step*
        // stops, not where the recording begins, and a read from anywhere else can
        // still go further back.
        let floor = first - STEP_HISTORY - READ_BEFORE;
        let Some((from, to)) = read_slice_back(&playback.covered, floor, first - 1, READ_BEFORE)
        else {
            return;
        };
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, from, to, "reading the stretch before the list");
        // Marked before the task is spawned: an answer that arrives before the
        // mark would clear a field that had not been set yet.
        if let Some(playback) = self.playback.as_mut() {
            playback.reading_older = Some((from, to));
        }
        start_segments(&self.handle, &self.events_tx, index, source, from, to);
    }

    /// Reads the strip's window from the cursor to its right edge.
    ///
    /// The strip is drawn a whole width around the cursor, so the list has to
    /// reach the right edge - and nothing else reads forward: `read_older` only
    /// goes back, and a session reads the range it plays. A strip whose right
    /// half was never read runs out at the cursor, and a seek past that edge is
    /// a request for a moment nobody has asked the box about - which is the
    /// request that comes back "nothing recorded at that moment".
    ///
    /// No further than the newest moment there is: a recording has nothing in
    /// the future, and asking for it is a request the box answers with an empty
    /// list every time.
    ///
    /// A read of its own rather than sharing [`Self::read_older`]'s slot: a
    /// strip can be short on both sides at once, and one slot would starve one
    /// of them.
    fn read_newer(&mut self, index: usize) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        if playback.reading_newer.is_some() {
            return;
        }
        // See [`Self::read_older`]: a drag asks once per cursor position, and the
        // answer that just said the box has nothing new is not worth asking again
        // on the next frame.
        if epoch_now() < playback.retry_newer {
            return;
        }
        let (_, end) = self.window_of(playback);
        let centre = self.centre_of(playback);
        let now = epoch_now();
        let to = end.min(now);
        let width = if playback.scrubbing { READ_CHUNK } else { READ_WHOLE };
        let Some((from, to)) = read_slice(&playback.covered, centre.min(to), to, width) else {
            return;
        };
        // The newest recording arrives a second at a time, and what is left of
        // it is the sliver at the edge of the strip's right half - which is
        // drawn to the moment the picture is on anyway. Asked for as it comes,
        // that sliver is a request every second for one second of recording; it
        // waits until a few seconds of it have accumulated. Only at that edge:
        // a short stretch further back is a hole to fill, not the live tip.
        // See [`LIVE_SLICE`].
        if to >= now && to - from < LIVE_SLICE {
            return;
        }
        let source = playback.source.clone();
        tracing::debug!(target: "xgview::app", index, from, to, "reading newer segments");
        if let Some(playback) = self.playback.as_mut() {
            playback.reading_newer = Some((from, to));
        }
        start_segments(&self.handle, &self.events_tx, index, source, from, to);
    }

    /// Keeps the strip's segment list as wide as the strip is drawn.
    ///
    /// The reads that follow a drag or a widened strip ask for one slice and
    /// then ask again when the answer lands, which is fast for as long as the
    /// box keeps answering. A read that comes back with nothing, or that the
    /// box could not answer at all, ends that chain - and the stretch stayed
    /// blank until the viewer moved the strip again. That is the band of
    /// nothing the event marks were drawn across: the marks are read for the
    /// window itself, on their own request, so they arrived while the band
    /// did not.
    ///
    /// So the window is asked for here as well, every frame. Both reads ask
    /// only for a stretch the window shows and the box has not already
    /// answered for, and only one at a time per side, so a strip that is whole
    /// costs nothing but the lookup. A read that failed waits [`READ_RETRY`]
    /// first, so a box that keeps failing is not asked once a frame.
    pub(super) fn read_strip(&mut self, ctx: &egui::Context) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        // The calendar is another picture of the recording rather than a width
        // of the strip, and the window it would be read for is not what is on
        // screen: nothing is asked for while it is shown.
        if playback.timeline == Timeline::Days {
            return;
        }
        let index = playback.index;
        // Each side has its own retry: a box that could not answer one read has
        // not failed the other, and holding both back kept a side that was
        // working from filling until the other side's retry came round.
        let now = epoch_now();
        let wait_older = now < playback.retry_older;
        let wait_newer = now < playback.retry_newer;
        let wait_events = now < playback.retry_events;
        let (centre, zoom) = (self.centre_of(playback), playback.zoom);
        if !wait_older {
            self.read_older(index);
        }
        if !wait_newer {
            self.read_newer(index);
        }
        // The marks of the window, and this is the one place that keeps them up:
        // the strip slides under a cursor that does not move, so playing forward
        // uncovers new time on the right without anything the viewer did - and a
        // stretch the picture carries on into after a hole is a window that
        // moved a night. Neither asks for events, and neither used to get any:
        // the list was the one read when the recording was opened, so the band
        // grew and the marks did not. See [`events_to_ask`].
        if !wait_events {
            // How much new time is worth a request: a second at the clock, and a
            // fifth of an hour on a strip a day wide - where a second is a
            // hundredth of a pixel and a mark on it is not there to be seen. A
            // deliberate read has no such wait. See [`Self::read_events_at`].
            self.read_events_at(index, centre, LIVE_SLICE.max(zoom / 64));
        }
        if wait_older || wait_newer || wait_events {
            // The strip is short and the box is not to be asked yet. A held
            // picture draws no frames of its own, so one is asked for, or the
            // blank stays for as long as the viewer leaves the picture held.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        // A stop at the live edge is asked about again once the recorder has had
        // time to cut its next fragment: "nothing yet" is not the recording
        // being over, and nothing else will ask. A look past a hole that the box
        // could not answer is asked for again the same way - forgotten, it left
        // the picture stopped with nothing on its way and no way to ask. See
        // [`AfterHole`].
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let after = playback.after_hole;
        let due = match after {
            Some(AfterHole::Waiting { at, .. } | AfterHole::Looking { at, .. }) => at,
            _ => return,
        };
        if now < due {
            ctx.request_repaint_after(std::time::Duration::from_secs((due - now) as u64));
            return;
        }
        match after {
            Some(AfterHole::Waiting { stop, .. }) => {
                self.look_at_edge(stop);
            }
            Some(AfterHole::Looking { stop, window, .. }) => self.look_from(stop, window.0, window.1),
            _ => {}
        }
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
        // Only the slot this answer was asked for: a strip can have a read out
        // on both sides, and clearing both let the side that was still waiting
        // be asked for again - the same megabyte of segments, once per side that
        // answered. See [`Self::read_older`] and [`Self::read_newer`].
        // Which side this answer is for, before the slots are cleared: a failed
        // read is remembered on its own side only.
        let was_older = playback.reading_older == Some((from, to));
        let was_newer = playback.reading_newer == Some((from, to));
        if was_older {
            playback.reading_older = None;
        }
        if was_newer {
            playback.reading_newer = None;
        }
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
            //
            // A look past a hole is not answered either: it is asked for again
            // once the retry has waited, rather than read as "nothing after the
            // hole" - and rather than forgotten, which left the picture stopped
            // with nothing on its way and nothing left to ask. See
            // [`AfterHole::Looking`] and [`Self::read_strip`].
            if let Some(stop) = look_past(&playback.after_hole, from, to) {
                playback.after_hole = Some(AfterHole::Looking {
                    stop,
                    window: (from, to),
                    at: epoch_now() + READ_RETRY,
                });
            }
            // Asked again once the retry has waited, rather than only when the
            // viewer happens to move the strip: the stretch this was for is
            // drawn as nothing until it is answered, and a band of nothing in
            // the middle of the recording is what the marks - read separately
            // and answered - were drawn across. See [`Self::read_strip`].
            //
            // A read asked for *before* the list is the exception: it is walking
            // back through history a day at a time, and an empty day is progress,
            // not a box that is behind - the next day is a different question.
            // Waited on, five seconds a day is a strip that crawls back through a
            // night. See [`Self::read_before_the_list`].
            let retry_at = epoch_now() + READ_RETRY;
            let before_the_list =
                playback.segments.first().is_some_and(|first| to < first.start);
            if was_older && !before_the_list {
                playback.retry_older = retry_at;
            }
            if was_newer {
                playback.retry_newer = retry_at;
            }
            // A hold taken for a seek that this read was to answer is let go of
            // too: a read that could not be made must not leave the picture
            // held for ever.
            if playback.to_seek.is_some() {
                playback.to_seek = None;
                playback.pending = None;
                self.release_hold(index);
            }
            return;
        };
        // The box answered for this range: whether it held anything or not, the
        // time in it is now known - and "asked, nothing there" is exactly what a
        // hole is. See [`covered_between`]. Not at the live edge, though: there
        // the box lists the files it has closed, and the one it is writing is
        // still to come. An answer that stops short of the request there - one
        // file behind, or nothing at all - is the recording not being cut yet.
        // Marking that tail known draws a gap beside the cursor that nothing
        // ever asks about again, and asking again the next frame is a request
        // every frame to a box already behind: the strip waits a few seconds
        // instead. See [`LIVE_EDGE`] and [`READ_RETRY`].
        let answered_to = segments
            .iter()
            .map(|segment| segment.end)
            .max()
            .unwrap_or(from);
        let now = epoch_now();
        let at_edge = to >= now - LIVE_EDGE;
        let short = answered_to < to;
        if from < now {
            let asked_to = to.min(now);
            let mark_to = if at_edge {
                answered_to.min(asked_to).max(from)
            } else {
                asked_to
            };
            cover(&mut playback.covered, from, mark_to);
            if short && at_edge {
                let retry_at = now + READ_RETRY;
                if was_older {
                    playback.retry_older = retry_at;
                }
                if was_newer {
                    playback.retry_newer = retry_at;
                }
            }
        }
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
            // The hold a press took goes with it, or the picture stays stopped
            // for ever - but only a *seek*'s, since a read of the strip's own
            // comes back empty while a finger is still on the timeline.
            let seeking = playback.to_seek.take().is_some();
            // Only that seek's own read. An empty answer to a read of the strip's
            // own says nothing about the moment a seek is waiting for - at the
            // live edge the strip asks for the next few seconds every couple of
            // seconds and is told "nothing yet" every time - and clearing it here
            // took the cursor back to the picture in the middle of the wait: the
            // strip went back a few seconds, and forward again when the answer it
            // was actually waiting for arrived. See [`Playback::pending`].
            if seeking {
                playback.pending = None;
            }
            // A look past a hole that found nothing. A day that held nothing is
            // a hole longer than a day: the next day is asked about the same
            // way, up to the limit where "after the hole" stops meaning
            // anything. Otherwise this is the end of the recording, and a press
            // from here starts it again.
            if let Some(stop) = look_past(&playback.after_hole, from, to) {
                // A look that found nothing - or could not be answered - is the
                // end only when the recorder has stopped writing there: at the
                // live edge it is a fragment that has not been cut yet. See
                // [`at_live_edge`] and [`AfterHole::Waiting`].
                let now = epoch_now();
                let newest = listed_to(&playback.segments, now);
                playback.after_hole = Some(after_stop(stop, now, newest));
                if keep_looking(stop, from, to) {
                    self.look_from(stop, to, to + HOLE_LOOK);
                    return;
                }
            }
            if picked.is_some() {
                // A day with nothing on it: the calendar stays where it is and
                // says so, rather than reading as a press that missed.
                self.flash(monitor_i18n::tr("toast-no-recording"), ToastKind::Error);
            }
            if seeking {
                self.release_hold(index);
            }
            return;
        }
        // An answer runs on from where the list ends: a newer one after the last
        // file, an older one before the first. Each is put where it belongs
        // rather than sorted in - and the older half is the direction a drag
        // into history fills - so a day of files is not re-sorted for every
        // slice that lands. Only an answer that overlaps what is held, the same
        // stretch read twice, is sorted.
        //
        // The answer's own order is checked rather than assumed: the list is
        // searched and drawn in order, and one answer out of order would leave
        // it that way.
        let in_order = segments.windows(2).all(|pair| pair[0].start <= pair[1].start);
        let joins_before = in_order
            && playback
                .segments
                .first()
                .zip(segments.last())
                .is_some_and(|(first, last)| last.start < first.start);
        if joins_before {
            playback.segments.splice(..0, segments);
        } else {
            let ordered = in_order
                && playback
                    .segments
                    .last()
                    .zip(segments.first())
                    .map_or(true, |(last, first)| last.start < first.start);
            playback.segments.extend(segments);
            if !ordered {
                playback.segments.sort_by_key(|segment| segment.start);
            }
        }
        playback.segments.dedup_by_key(|segment| segment.start);
        playback.stretches = stretches(&playback.segments);
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
                // Through `move_to`, so a picture the viewer had held is held
                // again: a seek made while paused shows the frame it lands on
                // rather than the pause being let go of with it.
                self.move_to(at);
                return;
            }
            // The moment was asked about and there is nothing at it: a hole. A
            // drag over one means "the closest picture", which is what
            // [`nearest`] answers and what the finger expects - the cursor going
            // back to where it started instead reads as a drag that was refused.
            // A step never lands in one (see [`across_hole`]), so this is a
            // finger's answer or a jump's.
            if covered_between(&playback.covered, at, at + 1) {
                playback.to_seek = None;
                playback.pending = None;
                let closest = nearest(&playback.stretches, at).filter(|target| *target != at);
                // The hold a press took is let go of, or the picture stays
                // stopped for ever - unless the picture is being taken somewhere,
                // which holds it again by itself.
                if let Some(target) = closest {
                    self.move_to(target);
                } else {
                    self.release_hold(index);
                }
                return;
            }
            // Nobody has asked about it yet - the read that has just landed was
            // another's. Asked now, or a seek whose read was turned away for
            // being one too many waits for an answer that is never coming.
            //
            // Not at the live edge, though. There the file the moment falls in
            // is still being written, so every answer says the same thing, and
            // asking the moment each one lands is a two-hour segment list -
            // seventeen of them a second, for as long as the file stays open.
            // The wait asks again in a couple of seconds instead, and it asks
            // the playlist rather than the file list, which is where the
            // fragments of that file already are. See [`AfterHole::Waiting`] and
            // [`XgViewApp::look_at_edge`].
            let now = epoch_now();
            let newest = listed_to(&playback.segments, now);
            if at_live_edge(at, now, newest) {
                playback.to_seek = None;
                playback.after_hole = Some(after_stop(at, now, newest));
                return;
            }
            // Past the end of the recording, with nothing being written there:
            // not a stretch to keep reading around, but a moment that does not
            // exist - a cursor dragged to the right of everything a camera
            // stopped recording an hour ago. It goes back to the picture, and the
            // hold a drag took is let go of, the same way a moment the box
            // answered nothing about does. See [`at_live_edge`].
            if at > newest {
                playback.to_seek = None;
                playback.pending = None;
                self.release_hold(index);
                return;
            }
            self.read_around(index, at);
            return;
        }
        // A look past a hole was waiting for this list: it says where the
        // recording carries on, if it does.
        if let Some(stop) = look_past(&playback.after_hole, from, to) {
            let next = next_stretch(&playback.segments, &playback.covered, stop);
            // The file after the stop is listed: the recorder wrote on while the
            // picture played, and at the live edge the answer is not "what comes
            // next", it is "more of what was already playing". See [`carries_on`].
            let carries = carries_on(&playback.segments, stop);
            playback.after_hole = match (next, carries) {
                (Some(_), _) | (None, Some(_)) => None,
                (None, None) => {
                    let now = epoch_now();
                    Some(after_stop(stop, now, listed_to(&playback.segments, now)))
                }
            };
            if let Some(next) = next {
                tracing::debug!(target: "xgview::app", at = next, "the recording carries on past a hole");
                self.move_to(next);
                return;
            }
            if let Some(carries) = carries {
                tracing::debug!(target: "xgview::app", at = carries, "the recording carries on at the live edge");
                self.move_to(carries);
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
            if let Some(target) = nearest(&playback.stretches, at) {
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
        // A wider strip may want more than this one answered, on either side.
        self.read_older(index);
        self.read_newer(index);
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
        start_playlist(&self.handle, &self.events_tx, index, source, window, at, None);
        if let Some(playback) = self.playback.as_mut() {
            playback.ahead = Some(Ahead { window, answer: None });
        }
    }

    /// Plays a moment the running session's own playlist holds, as another
    /// fragment of that session.
    ///
    /// The decoder, the parser and the login carry on, and the fragments in
    /// between are never asked for - which is what makes a step across a
    /// recording one request rather than another stream. The playlist reaches
    /// past the files the box has listed: the file being written is served while
    /// it is being written and listed only when it is closed, so at the live edge
    /// a moment the segment list calls past its end is still inside the session.
    /// See [`covered`].
    ///
    /// A session that reached the end of its range has nothing left to jump in -
    /// its task is gone - so that seek is another range. See [`Self::has_session`].
    fn jump_inside(&mut self, index: usize, target: i64, from: Option<i64>) -> bool {
        let Some(playback) = self.playback.as_ref() else {
            return false;
        };
        if !covered(playback.vod, target) || !self.has_session(index) {
            return false;
        }
        tracing::debug!(target: "xgview::app", index, at = target, "playing another fragment");
        // Asked for before the hold is let go of, so that the session wakes to
        // the jump rather than to the frame it was stopped on: a resumed session
        // publishes the sample it was waiting for, and that sample is the moment
        // the drag began from.
        self.manager.seek_fragment(index, target);
        self.manager.hold_clip(index, false);
        if let Some(playback) = self.playback.as_mut() {
            playback.scrub = None;
            playback.scrubbing = false;
            playback.pending = Some(target);
            playback.pending_from = from.filter(|from| *from != target);
            playback.paused = false;
        }
        true
    }

    /// Plays from the nearest moment that has a recording.
    ///
    /// A scrub that lands in a hole is taken to the edge of the nearest stretch
    /// rather than refused: the viewer is pointing at a place on a timeline, and
    /// the answer "there is nothing there, here is the closest there is" is one
    /// they can act on.
    fn seek(&mut self, at: i64) {
        // A look past a hole belonged to the moment the picture stopped at, and
        // the picture is being taken somewhere else: whatever it would have said
        // is about a place nobody is going. And the seek is numbered: the range
        // it may need is read in the background, and a second seek can be made
        // while the first is still being answered - only the newest one may
        // start a session, or a slower answer for an older moment would replace
        // the picture with where the viewer was.
        let gen = match self.playback.as_mut() {
            Some(playback) => {
                playback.after_hole = None;
                // Whatever the last seek left waiting is about a place this one
                // is leaving: an answer that lands on it would pull the picture
                // back to where the viewer was.
                playback.to_seek = None;
                playback.awaiting = None;
                playback.pending = None;
                playback.seek_gen += 1;
                playback.seek_gen
            }
            None => return,
        };
        let Some(playback) = &self.playback else {
            return;
        };
        let index = playback.index;
        // This seek opens a new session, and its first picture must be reported
        // as an arrival even when it is dated exactly where the last session
        // stopped - which is what happens at the live edge, where the next
        // stretch begins at the moment the previous one ended. The surface
        // path reports a moment only when it changes, so the old one would
        // otherwise swallow that first picture, leaving `pending` set and the
        // strip pinned to a moment the picture has long since played past.
        self.surface_moments.remove(&index);
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
        // The frame the picture is on is what this seek is leaving. The session
        // publishes it once more before the landing frame, and it is later than
        // the moment asked for - taken as arriving, it is the cursor going back
        // to where it started. See [`Playback::pending_from`].
        let from = self.pictured(index);
        if reach.is_some_and(|(first, last)| at < first || at > last) {
            // A moment the playlist already holds is a jump inside the session
            // that is running, even though the list says it is past the end:
            // the box serves the file it is still writing and lists it only when
            // it closes it, so a step forward at the live edge lands in that
            // file. Read as "past the end" it became a segment read, then
            // another, for as long as the file was open. See
            // [`Self::jump_inside`].
            if self.jump_inside(index, at, from) {
                return;
            }
            self.read_around(index, at);
            if let Some(playback) = self.playback.as_mut() {
                playback.to_seek = Some(at);
                // The cursor waits where the finger left it, as it does for a
                // seek whose playlist is on its way: what is on screen is still
                // the stretch being left.
                playback.pending = Some(at);
                playback.pending_from = from.filter(|from| *from != at);
            }
            return;
        }
        // Inside the list's span is not the same as known: a list holding the
        // 31st and today spans four days it has never asked about, and the
        // nearest stretch in it is a jump nobody asked for. A moment the box has
        // not been asked about is read around first, and comes back through
        // `to_seek` like any other. See [`covered_between`].
        //
        // A moment a file in the list covers is the exception: the list is what
        // the recording holds, so that moment has been answered already. The
        // very end of a file counts - the band reaches it, and it is where a
        // seek to the end of the strip lands. Asked about instead, it is a read
        // that can never say yes: at the live edge a read only marks what the
        // box listed, which ends exactly there, so every answer asks again and
        // the seek never arrives. See [`Self::segments_arrived`].
        let recorded = playback
            .segments
            .iter()
            .any(|segment| segment.start <= at && at <= segment.end);
        if !recorded && !covered_between(&playback.covered, at, at + 1) {
            self.read_around(index, at);
            if let Some(playback) = self.playback.as_mut() {
                playback.to_seek = Some(at);
                playback.pending = Some(at);
                playback.pending_from = from.filter(|from| *from != at);
            }
            return;
        }
        // Neither of these can happen while the list is the one the window was
        // built from - and if it does, saying so is better than a picture that
        // silently carries on where it was, which reads as a drag that was
        // refused.
        let Some(target) = nearest(&playback.stretches, at) else {
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
        // gone - so that seek is another range too. See [`Self::jump_inside`].
        if self.jump_inside(index, target, from) {
            return;
        }
        // Read again: a jump ends the borrow the answer above was taken from.
        let Some(playback) = &self.playback else {
            return;
        };

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
                        playback.pending_from = from.filter(|from| *from != target);
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
            playback.pending_from = from.filter(|from| *from != target);
        }
        tracing::debug!(target: "xgview::app", index, at = target, "another stretch for a scrub");
        start_playlist(&self.handle, &self.events_tx, index, source, window, target, Some(gen));
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
        if playback.index == index {
            if let Some(pending) = playback.pending {
                // The frames that are not an arrival: the picture the seek left
                // behind, and the ones the session it is leaving publishes while
                // the new one is being read. A seek out of a range leaves the
                // old session running until the answer lands, and its frames
                // keep coming - and `caught_up` alone counts every one of them
                // as an arrival when the seek went *backwards*, because they are
                // all later than the moment asked for. The cursor then went to
                // the old picture and the step was made again from there: a
                // strip that jittered between two days while a key was held.
                //
                // Which picture a frame belongs to is not something a frame
                // says, but it is where the frame is: the old session's are at
                // the moment the seek left, the new one's at the moment it asked
                // for. Nearer the former is not an arrival.
                let left_behind = playback.pending_from.is_some_and(|from| {
                    at.is_some_and(|at| belongs_to_the_old_picture(from, pending, at))
                });
                if !left_behind && caught_up(at, pending) {
                    playback.pending = None;
                    playback.pending_from = None;
                    // A frame really arrived, so the picture's moment has
                    // moved on. The hop that brought it here is settled, and a
                    // later stop is about a different frame. See
                    // `Playback::hole_hop`.
                    playback.hole_hop = None;
                }
            }
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

/// How much of a strip's missing segment list is asked for in one request
/// while a finger is on it.
///
/// The edge is uncovered the moment the finger moves it, and an edge with
/// nothing drawn in it reads as "not recorded" until the answer lands, so the
/// band grows towards the edge in steps rather than waiting for the whole
/// stretch. What a step costs is mostly its round trip: measured against the
/// box on 2026-11-06, one hour came back in 57 ms, six in 66 and twelve in 82,
/// while the answer itself grows with the width - 90, 432 and 866 kilobytes.
/// Six hours is therefore the step that fills the widest strip in two requests
/// a side instead of six, at a quarter of the bytes a day would carry. Each
/// slice is taken against the band already drawn, so the band grows out towards
/// the edge rather than being joined from both ends. See [`read_slice`] and
/// [`read_slice_back`].
const READ_CHUNK: i64 = 21_600;

/// How much of a strip's missing segment list is asked for in one request once
/// the finger is lifted.
///
/// Nothing is following the edge then, so the slices are only requests standing
/// between the viewer and a filled strip, and the box answers a wide one in
/// barely more time than a narrow one: six hours in 66 ms, twelve in 82. Twelve
/// hours is half of the widest strip - [`ZOOMS`] ends at a day - so each side
/// of it is one request rather than two, carrying the same bytes a pair of six
/// hour slices would, sixteen milliseconds later. No wider is any use:
/// [`read_slice`] stops at the window's edge, so the rest would never be asked
/// for.
const READ_WHOLE: i64 = 43_200;

/// How far back one read reaches when it is walking back through history.
///
/// The strip's own widths stop at a day, and a read asked for from the list's
/// edge is not filling a strip - it is looking for where the recording goes on,
/// and the answer may be days away. A day at a time is the same request the
/// widest strip makes, and a day of files is a few thousand rows: the box
/// answered a day of this camera's history in 100-200 ms.
const READ_BEFORE: i64 = 86_400;

/// How far back a step will walk looking for where the recording goes on.
///
/// A step back that reaches the list's edge asks for the day before it, and keeps
/// asking a day at a time while there is nothing there - a camera off for a night
/// is a hole an arrow should cross. A week is as far as that is worth doing: a
/// hole wider than a week is a day the calendar is for, and an unbounded walk is
/// a request every few hundred milliseconds for as long as a key is held, at a
/// camera that has already said no.
const STEP_HISTORY: i64 = 7 * 86_400;

/// How long a read the box could not answer waits before it is asked again.
///
/// Long enough that a box refusing every request is asked a handful of times a
/// minute rather than once a frame, and short enough that a strip left blank by
/// a lost connection fills itself in while the viewer is still looking at it.
const READ_RETRY: i64 = 5;

/// How much of the newest recording a strip waits for before it asks the box.
///
/// The recording is written a second at a time, so what is left to read at the
/// live edge is a sliver that grows by a second. Asked for as it comes, that is
/// a request every second for one second of recording, for as long as the strip
/// is open. It waits until a few seconds have accumulated instead; the sliver is
/// at the edge of the strip's right half, which is drawn no further than the
/// moment the picture is on.
const LIVE_SLICE: i64 = 10;

/// How far past a hole the segment list is read, looking for where the
/// recording carries on.
///
/// A day. Most holes are a minute - a camera that dropped its stream and came
/// back - and the ones that are not are usually a night, which is the length
/// that has to be covered for "it stopped" to mean "the recording is over".
/// The read only happens when the list already held does not reach past the
/// hole, which is the list read around the moment being watched and reaches an
/// hour or so; every small hole is answered from that, with no request at all.
const HOLE_LOOK: i64 = 24 * 3600;

/// How near the present a stop has to be to be the live edge.
///
/// A playlist ends at the last fragment the box had when its range was read,
/// and the recorder cuts a file about every ten seconds: a stop a minute or two
/// behind the clock is the edge of what has been written, not the end of the
/// recording. The window is loose because the device's clock and the server's
/// are not the same clock.
const LIVE_EDGE: i64 = 300;

/// How long the picture waits at the live edge before asking again.
///
/// Asking the moment it stops is answered "nothing" - the next fragment is still
/// being cut, and a cut is about every ten seconds. Two is short enough that a
/// picture resumes within a cut of the one before without the viewer noticing it
/// had stopped, and long enough not to ask every frame.
const LIVE_LOOK: i64 = 2;

/// How much of a playlist has to be ahead of the moment playback begins at, in
/// seconds, for a session to be started there.
///
/// A moment is a whole second and a fragment is not, so a playlist that ends
/// within a second of the moment still holds the frame the picture stopped on.
/// Playing that frame again is a stop asking for the same moment, one request
/// after another. See [`stops_short`].
const EDGE_PLAY: i64 = 2;

/// The seam between two files the recorder cut one after the other.
///
/// The box dates a file's end and the next file's start a second apart even
/// when nothing was missed, and the strip merges that to draw one band. A stop
/// this close to the file after it is where a playlist ran out of what it was
/// asked for rather than where the recording has a hole, so the picture carries
/// on there. Anything wider is a stretch the recording really does not have,
/// which a read past the hole answers. See [`carries_on`].
const SEAM: i64 = 3;

/// How far past the live edge one ask reaches.
///
/// The width of the playlist asked for when the picture waits for the fragments
/// past a stop: a few minutes, which is the strip's own width at the closer
/// levels and far more than the next fragment needs. It is also the furthest back
/// a range at the edge may begin, so that the range still holds the stop - see
/// [`edge_from`].
const LIVE_SPAN: i64 = WINDOW;

/// How far past a hole the looking goes, a day at a time.
///
/// A month. Each day is one request, and a day that holds nothing is two bytes
/// of answer - so a camera that was off for three weeks is twenty-odd small
/// requests and then one that finds it, rather than one enormous one. Past this
/// the recording is not "after a hole" any more, it is somewhere else in the
/// archive, and the calendar is what finds it.
const HOLE_LIMIT: i64 = 30 * 24 * 3600;

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
pub(super) fn step_zoom(zoom: i64, finer: bool) -> Option<i64> {
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

/// The next rate the speed control asks for: one, two, four, eight, and back to
/// one. Eight is the end of the ladder.
fn next_speed(speed: f32) -> f32 {
    if speed < 1.5 {
        2.0
    } else if speed < 3.0 {
        4.0
    } else if speed < 6.0 {
        8.0
    } else {
        1.0
    }
}

/// Whether a recording's picture is standing still.
///
/// Two things stop it and the corner says one thing about both: the viewer held
/// it, or the range ran out. Which of the two it is, the timeline says - the
/// cursor is at the end for one of them and wherever it was left for the other -
/// and the corner's question is only whether the picture is moving.
fn still(paused: bool, state: Option<ConnectionState>) -> bool {
    paused || state == Some(ConnectionState::Finished)
}

/// The word a rate is written as, on the control that carries it.
///
/// A picture of a fast-forward cannot say how fast it is, so this one control
/// carries a word rather than a glyph. It has to agree with [`next_speed`]: the
/// number the viewer reads is the one the next press will leave behind.
fn speed_label(speed: f32) -> &'static str {
    if speed > 6.0 {
        "8×"
    } else if speed > 3.0 {
        "4×"
    } else if speed > 1.5 {
        "2×"
    } else {
        "1×"
    }
}

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

/// How far a held arrow steps, and how soon it steps again.
///
/// A press steps at once; this is what the repeats after it do. Both halves move
/// with the hold: the step grows, so a long hold covers ground, and the repeat
/// shortens, so it covers it sooner. A twentieth of the strip every 160 ms is a
/// third of a strip a second, which is fine walking; a fifth of it every 100 ms
/// is two strips a second, which is crossing a day's recording in a breath.
fn hold_step(base: i64, held: std::time::Duration) -> (i64, std::time::Duration) {
    let held = held.as_secs_f32();
    if held > 1.5 {
        (base * 4, REPEAT_FAST)
    } else if held > 0.6 {
        (base * 2, REPEAT_MID)
    } else {
        (base, REPEAT_SLOW)
    }
}

/// How long a key must be down before it repeats at all.
///
/// A press is read as "down" on every frame until it is released, and a finger
/// holds a key for 130-175 ms - long enough to cross [`REPEAT_SLOW`] and be
/// taken for a second step. The delay is what separates a press from a hold:
/// below it, one press is one step however long the finger takes to lift.
const REPEAT_DELAY: std::time::Duration = std::time::Duration::from_millis(400);
/// How long a hold outlives its release.
///
/// A front end that gives each input event a frame of its own reports a press
/// as a down frame followed by an up frame, and a remote that repeats by
/// sending down/up pairs reports a hold the same way. Reading every gap as the
/// end of the hold would make each pair a fresh press - one that steps at once
/// and never reaches [`REPEAT_DELAY`], so a held arrow walks a step per event
/// instead of a step per repeat. Short enough that two deliberate presses are
/// still two presses.
const RELEASE_GRACE: std::time::Duration = std::time::Duration::from_millis(120);
/// How often a held arrow steps: at first, after half a second, and after one
/// and a half.
const REPEAT_SLOW: std::time::Duration = std::time::Duration::from_millis(160);
/// See [`REPEAT_SLOW`].
const REPEAT_MID: std::time::Duration = std::time::Duration::from_millis(120);
/// See [`REPEAT_SLOW`].
const REPEAT_FAST: std::time::Duration = std::time::Duration::from_millis(100);

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

/// Where the calendar has to be slid to so that `index`'s button is in the row -
/// and where it already is, when it is.
///
/// The cursor is the ring and the confirm key's target, so a row that does not
/// follow it is a ring that walks off the edge and a press that picks something
/// nobody can see.
fn day_scroll_for(index: usize, each: f32, scroll: f32, length: f32, total: f32) -> f32 {
    let start = index as f32 * each;
    let scroll = if start < scroll {
        start
    } else if start + each > scroll + length {
        start + each - length
    } else {
        scroll
    };
    day_scroll_to(scroll, total, length)
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

/// Where a range asked for at the live edge begins.
///
/// A range that begins on a segment's own boundary is served straight out of the
/// files the box has; one that begins anywhere else is cut, every fragment of it,
/// at 671-1034 ms a time. A stop is a fragment's end and rarely a segment's, and
/// past the last file listed there is no segment to fall back into - so the range
/// begins at that file's own start, which is a boundary.
///
/// It never begins so far back that the range stops reaching the stop: a playlist
/// read for a range that ends before the moment is a wait that never ends. See
/// [`range_from`] and [`XgViewApp::look_at_edge`].
fn edge_from(segments: &[Segment], stop: i64) -> i64 {
    let from = segments
        .last()
        .filter(|last| last.end <= stop)
        .map_or_else(|| range_from(segments, stop, stop), |last| last.start);
    from.max(stop - LIVE_SPAN)
}

/// Where a session on `playlist` begins, held to what the box will actually serve.
///
/// The moment asked for, and no later than the last second the playlist holds: a
/// playlist's fragments end a fraction of a second before the moment the segment
/// list ceils the file's end to, so the second the list names can be one the box
/// has no fragment for. Beginning there is a session that shows nothing at all -
/// the box answers "nothing recorded at that moment" - which is a drag to the end
/// of a stretch springing back to where it started.
///
/// `live` is the exception, and it is why this cannot be a plain clamp: at the live
/// edge a playlist stops short because the recorder has not cut the next fragment
/// yet, and the fragments behind the moment are still to come. Held back to the
/// playlist's end there, a drag to the end of the band lands half a minute earlier,
/// and then plays forward through the half minute it was denied. The wait
/// [`stops_short`] arms is the answer there, not a shorter request.
///
/// Never before the range's own start: a playlist asked for from `window.0` holds
/// nothing earlier, and a begin outside it is a wait for a picture it never
/// reaches.
fn served_begin(playlist: &VodPlaylist, window: (i64, i64), at: i64, live: bool) -> i64 {
    if live {
        return at.max(window.0);
    }
    let served = playlist.starts_at(playlist.fragments.len()) - 1;
    at.min(served).max(window.0)
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

/// Whether a playlist stops before the moment playback would begin at, or so
/// little after it that starting a session there is a replay and not a play.
///
/// The box serves a playlist only as far as it has fragments when the range is
/// read, so one asked for across the live edge can end at - or before - the
/// moment the seek wants. A session started on it ends where it began, and the
/// stop then asks for that same moment again: a playlist, a session and a stop,
/// one after another, for as long as the fragments are not cut. The moment is
/// counted in whole seconds and the fragments in fractions, so the last frame
/// of a playlist reads as a moment it still holds - which is the same loop a
/// second longer. A couple of seconds is the least that is worth a session. The
/// fragments after it are on their way, so the picture waits for them instead.
/// See [`AfterHole::Waiting`].
fn stops_short(playlist: &VodPlaylist, begin: i64) -> bool {
    playlist.starts_at(playlist.fragments.len()) < begin + EDGE_PLAY
}

/// What an answer to `read` lets the strip mark as asked about.
///
/// Only what the box listed, and never past the range asked for. The recorder
/// lists a file a moment after it closes it, so an answer at the live edge
/// stops short of the request - and marking the whole request instead would
/// draw the seconds it did not reach as a hole nothing asks about again: the
/// next read begins at that mark, and a file that starts before it is behind
/// it for good. See [`XgViewApp::start_playback`] and [`LIVE_EDGE`].
fn answered(read: (i64, i64), segments: &[Segment]) -> Option<(i64, i64)> {
    let to = segments
        .iter()
        .map(|segment| segment.end)
        .max()
        .unwrap_or(read.0)
        .min(read.1);
    (to > read.0).then_some((read.0, to))
}

/// Remembers that the box was asked about `from..to`, merging with what is
/// already known.
fn cover(covered: &mut Vec<(i64, i64)>, from: i64, to: i64) {
    if from >= to {
        return;
    }
    let mut from = from;
    let mut to = to;
    covered.retain(|(start, end)| {
        // Overlapping or touching: this read fills the gap between them.
        if *end < from || *start > to {
            return true;
        }
        from = from.min(*start);
        to = to.max(*end);
        false
    });
    covered.push((from, to));
    covered.sort_by_key(|(start, _)| *start);
}

/// Whether a day that held nothing is followed by another day of looking.
///
/// A day that held nothing is a hole longer than a day, and the next day is
/// asked about from where the answer stopped. The chain ends at [`HOLE_LIMIT`],
/// where the recording is somewhere else in the archive rather than after a
/// hole.
fn keep_looking(stop: i64, from: i64, to: i64) -> bool {
    to - from >= HOLE_LOOK && to < stop + HOLE_LIMIT
}

/// Whether the box has been asked about all of `from..to`.
///
/// A stretch that begins after `stop` is the next recording only if the time
/// between the two was asked about: a list that holds the 31st and today, with
/// nothing read in between, would otherwise offer today as "what comes next".
fn covered_between(covered: &[(i64, i64)], from: i64, to: i64) -> bool {
    covered.iter().any(|(start, end)| *start <= from && to <= *end)
}

/// How much of `from..to` the cover does not hold, at its widest gap.
///
/// The runs are sorted and merged, so the gaps are walked in one pass: the one
/// before the first run, the ones between runs, and the one after the last.
fn uncovered_width(covered: &[(i64, i64)], from: i64, to: i64) -> i64 {
    let mut widest = 0;
    let mut at = from;
    for (start, stop) in covered {
        if *stop <= at {
            continue;
        }
        if *start >= to {
            break;
        }
        if *start > at {
            widest = widest.max((*start).min(to) - at);
        }
        at = at.max(*stop);
    }
    widest.max(to - at)
}

/// What the event list should be asked for, or `None` when it already holds it.
///
/// A window either side of the one drawn, and never past the clock: the strip
/// slides under a cursor that does not move, so a read that covered only what is
/// drawn would be asked again a second later. Every ask covers the whole drawn
/// window, and that is what lets an answer *replace* the list rather than add to
/// it: a list that is its own window's marks cannot lose one that is still on
/// screen.
///
/// `least` is how much new time is worth a request. A deliberate read passes 1 -
/// anything uncovered at all - and the read made every frame passes
/// [`LIVE_SLICE`], so a window sliding a second at a time is not a request a
/// frame. That wait scales with the window on a wide strip, where a second is a
/// hundredth of a pixel: a mark that arrives a quarter of an hour late at a day's
/// width is a mark nobody could have seen arriving, and asking for two days of
/// events every ten seconds is a box asked for nothing. See
/// [`XgViewApp::read_strip`].
fn events_to_ask(
    covered: &[(i64, i64)],
    centre: i64,
    zoom: i64,
    now: i64,
    least: i64,
) -> Option<(i64, i64)> {
    let from = centre - zoom;
    let to = (centre + zoom).min(now);
    if from >= to {
        return None;
    }
    (uncovered_width(covered, from, to) >= least).then_some((from, to))
}

/// The next slice of `from..to` the box has not been asked about, and no wider
/// than `width`.
///
/// Read from `from` outwards: that is the band already drawn - the cursor, for
/// the strip's right half - so the strip grows from it towards the edge rather
/// than leaving a band of nothing beside the cursor until the answer lands.
///
/// Stepping over what has been asked about is what lets a stretch with a hole
/// in the middle be read at all: the read walks along the gap rather than
/// stopping at the first recording it meets.
fn read_slice(covered: &[(i64, i64)], from: i64, to: i64, width: i64) -> Option<(i64, i64)> {
    let mut at = from;
    while at < to {
        // Step over a stretch the box has already answered for, whole: a hole
        // is not asked about again, and the end of a covered run is asked for
        // by the next read rather than half of it here.
        if let Some((_, end)) = covered.iter().find(|(start, end)| *start <= at && at < *end) {
            at = *end;
            continue;
        }
        let next = covered
            .iter()
            .filter(|(start, _)| *start > at)
            .map(|(start, _)| *start)
            .min()
            .unwrap_or(to)
            .min(to);
        return Some((at, (at + width).min(next)));
    }
    None
}

/// The next slice of `from..to` the box has not been asked about, taken from
/// `to` back towards `from`, and no wider than `width`.
///
/// The mirror of [`read_slice`] for the strip's left side, where the band
/// already drawn is on the `to` side: the slice is the one *next to that band*,
/// so the band grows out towards the edge the drag uncovered rather than a new
/// band appearing at the edge and the two meeting in the middle. Meeting in the
/// middle is what reads as two strips rather than one.
///
/// Walked from `to` back, so what is answered for is stepped over on the way:
/// a stretch read earlier is a boundary of the gap behind it, not something to
/// be discovered by walking the whole span again.
fn read_slice_back(covered: &[(i64, i64)], from: i64, to: i64, width: i64) -> Option<(i64, i64)> {
    let mut end = to;
    while end > from {
        // The band already drawn: walk back to where it begins, and the gap
        // beyond it is the one to read. A band that ends exactly at `end` is
        // the same case - there is no gap against it.
        if let Some((start, _)) = covered.iter().find(|(start, stop)| *start < end && end <= *stop) {
            end = (*start).max(from);
            continue;
        }
        let gap_start = covered
            .iter()
            .filter(|(_, stop)| *stop <= end)
            .map(|(_, stop)| *stop)
            .max()
            .unwrap_or(from)
            .max(from);
        return Some(((end - width).max(gap_start), end));
    }
    None
}

/// The moment a look past a hole was made from, when this answer is that look's.
///
/// A segment read is answered for the range it asked about, so the range is what
/// tells one read from another - the same test the calendar's picked day uses.
fn look_past(after_hole: &Option<AfterHole>, from: i64, to: i64) -> Option<i64> {
    match *after_hole {
        Some(AfterHole::Looking { stop, window, .. }) if window == (from, to) => Some(stop),
        _ => None,
    }
}

/// Where a press at a stop starts the recording again.
///
/// The moment playback opened, while the picture is still in the same stretch of
/// recording: a viewer who watched from there to the end wants it again from
/// there. Once the viewer has walked to another stretch, that moment is a
/// journey away - another day, as often as not - and "again" means the beginning
/// of the stretch they are in rather than a jump back to where they started
/// watching hours ago.
fn restart_from(segments: &[Segment], opened: i64, stop: i64) -> i64 {
    let stretch = stretches(segments)
        .into_iter()
        .find(|stretch| stretch.start <= stop && stop <= stretch.end);
    match stretch {
        // Where it opened, while that is a moment the recording still has ahead
        // of it. Opening at the very stop - which is where a recording that has
        // just stopped is opened, see [`entry_moment`] - would start the picture
        // again on the last moment there is, and the press would run out again
        // before it was seen to have played at all.
        Some(stretch) if stretch.start <= opened && opened < stop => opened,
        Some(stretch) => stretch.start,
        None => opened,
    }
}

/// Where a recording carries on, after a hole the picture stopped at.
///
/// A stretch that begins at or before `stop` is the one the picture is at the
/// end of; what comes next is the first that begins after it **and** whose
/// distance from it has been asked about. The list is what has been read, not
/// what was recorded, so a stretch further down it is not the next one until the
/// box has said there is nothing in between - see [`covered_between`]. `None`
/// otherwise, which is either the end of the recording or the end of what has
/// been read, and only a read tells the two apart. See
/// [`XgViewApp::look_past_hole`].
fn next_stretch(segments: &[Segment], covered: &[(i64, i64)], stop: i64) -> Option<i64> {
    stretches(segments)
        .into_iter()
        .map(|stretch| stretch.start)
        .find(|start| *start > stop && covered_between(covered, stop, *start))
}

/// Whether a stop is the last automatic hop past a hole replaying itself.
///
/// The picture's moment is the frame it stopped on, and `next` is the stretch
/// that frame would be carried on to. A session that ended without showing a
/// frame leaves both unchanged, so making the same hop again would only start
/// another session that ends the same way. Once the picture arrives somewhere
/// new, `Playback::hole_hop` is cleared and a fresh hop is possible.
fn hole_hop_replayed(hop: Option<(i64, i64)>, stop: i64, next: Option<i64>) -> bool {
    next.is_some_and(|next| hop == Some((stop, next)))
}

/// The moment the recording carries on from a stop, when the recording goes on
/// there.
///
/// A stop at the live edge is not one the list knows nothing after: the recorder
/// wrote on while the picture played, and the file it is writing is still
/// listed. The box serves a playlist only as far as it had fragments when the
/// range was read, and the last of them ends partway through that file - so the
/// stop is *inside* a file that is not over, and what comes next is the rest of
/// it. The picture therefore carries on at the stop itself rather than at the
/// file after it: leaping to the next file's start would skip the part of this
/// one that is still being cut, and would put the picture nearer the live edge
/// than the box can serve, where the next session ends where it began.
///
/// A stop on a file's last second is the seam between two files, not a remainder
/// of one: [`Segment`] rounds an end up, so a picture on the last frame reads as
/// a second short of it. There the next file's own start is the moment. A stop
/// with nothing within [`SEAM`] of it is a hole the recording really has, and
/// a read past the hole is what answers that. See [`XgViewApp::look_at_edge`].
fn carries_on(segments: &[Segment], stop: i64) -> Option<i64> {
    if let Some(inside) = segments
        .iter()
        .find(|segment| segment.start <= stop && stop < segment.end)
    {
        if inside.end - stop > 1 {
            return Some(stop);
        }
    }
    segments
        .iter()
        .find(|segment| segment.start >= stop)
        .map(|segment| segment.start)
        .filter(|start| start - stop <= SEAM)
}

/// Whether a stop is the live edge of a recording that is still being written.
///
/// Two facts, and both are needed. The stop is near the clock - that is what the
/// wait is about. And the newest moment the box has listed is near the clock
/// too: a camera that stopped recording an hour ago leaves its newest file an
/// hour behind, and a seek to "now" is then past the end of the recording rather
/// than at the edge of it. Waited on, that is a playlist every couple of seconds
/// for as long as the picture is left there, and a picture that never moves.
///
/// The window is the same loose [`LIVE_EDGE`] as the clock's: the device's clock
/// and the server's are not the same clock, and a recorder that pauses for a
/// minute is still recording.
fn at_live_edge(stop: i64, now: i64, newest: i64) -> bool {
    stop >= now - LIVE_EDGE && newest >= now - LIVE_EDGE
}

/// The newest moment the box has listed: where the last file it closed ends.
///
/// A list that holds nothing says nothing about the box, so the clock stands in
/// for it - the recording may be one the reads have not caught up with.
fn listed_to(segments: &[Segment], now: i64) -> i64 {
    segments.last().map_or(now, |last| last.end)
}

/// What a stop is, once its list has been read and reaches no further.
///
/// A stop at the live edge is waited on: the recorder is still cutting the next
/// fragment, and asking again shortly is what keeps the picture going. Anything
/// else is the end of the recording, and there is nothing to wait for. See
/// [`at_live_edge`] and [`AfterHole::Waiting`].
fn after_stop(stop: i64, now: i64, newest: i64) -> AfterHole {
    if at_live_edge(stop, now, newest) {
        AfterHole::Waiting { stop, at: now + LIVE_LOOK }
    } else {
        AfterHole::Nothing(stop)
    }
}

/// The moment closest to `at` that a stretch covers.
fn nearest(stretches: &[Segment], at: i64) -> Option<i64> {
    stretches
        .iter()
        .map(|stretch| at.clamp(stretch.start, stretch.end))
        .min_by_key(|moment| (*moment - at).abs())
}

/// A moment a step landed on, taken across a hole rather than back to the edge it is
/// nearest.
///
/// A step is a walk through the recording, and a hole is a stretch it does not have:
/// the next picture in the direction of the step is what a press asks for. The
/// nearest edge is the one the step came from, so a hole - a camera off for a night,
/// the last file of one day to the first of the next - is a wall the arrows cannot
/// walk over: every press landed back where it started, and the same press again
/// started from there. A drag is the other way round, and keeps [`nearest`]: a finger
/// over a hole means "the closest picture".
///
/// A moment past the end of the list is left alone, and so is one before its start:
/// that is not a hole but a stretch nobody has read, and the read a step waits for is
/// the right answer - jumping to the far side of it would skip a recording that is
/// there. Only a moment the list reaches past is a hole, and the list is the merged
/// record of everything the box has said.
///
/// [`Segment::end`] is where a file stops, not a moment inside it: the list floors a file's
/// start and ceils its end, so `end` can be up to a second past the last fragment the box
/// will serve - and a seek to it comes back "nothing recorded at that moment", which the
/// interface reads as a hole. A step back therefore lands on `end - 1`, which is inside
/// the file by construction. A step forward is safe already: `start` is floored, so it is
/// a moment the file holds.
fn across_hole(segments: &[Segment], at: i64, forward: bool) -> i64 {
    if segments.iter().any(|segment| segment.start <= at && at <= segment.end) {
        return at;
    }
    let side = if forward {
        segments.iter().filter(|segment| segment.start > at).map(|segment| segment.start).min()
    } else {
        segments.iter().filter(|segment| segment.end < at).map(|segment| segment.end - 1).max()
    };
    side.unwrap_or(at)
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

/// Whether a frame is one the session being left is still publishing, rather than
/// the picture the seek went to.
///
/// A seek out of a range leaves the old session running until the answer lands,
/// and a frame is the only thing that says where the picture has got to - but a
/// frame carries no name. Where it is says it instead: the old session's frames
/// are at the moment the seek left, the new one's at the moment it asked for.
///
/// `caught_up` alone is not enough, and that is what made a held arrow jitter
/// between two days: a seek *backwards* is caught up by every frame of the
/// session it is leaving, because they are all later than the moment asked for.
fn belongs_to_the_old_picture(from: i64, pending: i64, at: f64) -> bool {
    (at - from as f64).abs() < (at - pending as f64).abs()
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
            read: (now - LOOKBACK, now),
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
    gen: Option<u64>,
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
            gen,
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
            from,
            to,
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
        let started = std::time::Instant::now();
        let (user, pass) = source.credentials.clone().unwrap_or_default();
        let config = FrigateConfig::at(&source.origin.base_url, &user, &pass);
        // The days are counted in the viewer's zone: the box answers in UTC
        // otherwise, and a recording made in the evening of one day is the next
        // day to a UTC box - so the calendar starts a day late. See
        // `blackout::zone_name`.
        let zone = monitor_core::blackout::zone_name();
        // A read that fails is a calendar with nothing to draw rather than a
        // camera that recorded nothing, and the two look the same on screen -
        // so it is said here, where a run that keeps missing its days can be
        // told apart from a box that has none.
        let days = match FrigateClient::new(config) {
            Ok(mut client) => match client.recorded_days(&source.origin.camera, zone.as_deref()).await
            {
                Ok(days) => Some(days),
                Err(err) => {
                    tracing::debug!(
                        target: "xgview::app",
                        index,
                        %err,
                        "the recorded days could not be read"
                    );
                    None
                }
            },
            Err(_) => None,
        };
        // What the date row costs before it can be drawn. It is normally tens of
        // milliseconds, and a box that takes a second or more over it is the one
        // thing that can make the calendar feel slow even though it is read
        // before the control is pressed. See [`XgViewApp::start_playback`].
        tracing::debug!(
            target: "xgview::app",
            index,
            ms = started.elapsed().as_millis() as u64,
            days = days.as_ref().map(|days| days.len()).unwrap_or(0),
            "the recorded days were read"
        );
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
            Ok(mut client) => match client.segments(&source.origin.camera, from, to).await {
                Ok(segments) => Some(segments),
                Err(err) => {
                    tracing::debug!(
                        target: "xgview::app",
                        camera = %source.origin.camera,
                        from,
                        to,
                        %err,
                        "the segment read failed"
                    );
                    None
                }
            },
            Err(err) => {
                tracing::debug!(target: "xgview::app", camera = %source.origin.camera, %err, "the segment read failed");
                None
            }
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
    use monitor_core::discovery::frigate::Fragment;

    /// A hold is not a new press just because a front end reported its down
    /// and its up in different frames: the short release keeps the clocks, so
    /// the repeat delay still separates a press from a hold.
    #[test]
    fn a_short_release_does_not_restart_a_hold() {
        let since = std::time::Instant::now();
        let held = |forward, released| Held { forward, since, stepped: since, released };

        let down = held(true, None);
        assert!(down.is_down(), "the arrow is down");
        assert!(!down.restarted(true, since), "the key is still down");
        assert!(down.restarted(false, since), "the other arrow is a new press");

        let released = held(true, Some(since));
        assert!(!released.is_down(), "a released arrow is no longer held");
        assert!(
            !released.restarted(true, since + RELEASE_GRACE / 2),
            "a gap between two frames of one press"
        );
        assert!(
            released.restarted(true, since + RELEASE_GRACE),
            "a finger that lifted and came back"
        );
        assert!(
            released.restarted(false, since + RELEASE_GRACE / 2),
            "a change of direction never waits for the grace"
        );
    }

    /// A hop that has just been made from one frame to one stretch is not made
    /// again from the same pair: a session that never put a frame on the screen
    /// would otherwise name that pair for ever, restarting every few tens of
    /// milliseconds. A frame that moved, or a different stretch, is a new hop.
    #[test]
    fn a_hole_hop_is_not_replayed_from_the_same_frame() {
        let hop = Some((100, 400));
        assert!(hole_hop_replayed(hop, 100, Some(400)), "the same stop");
        assert!(!hole_hop_replayed(hop, 100, None), "nothing to hop to");
        assert!(!hole_hop_replayed(None, 100, Some(400)), "no hop yet");
        assert!(!hole_hop_replayed(hop, 101, Some(400)), "the picture moved on");
        assert!(!hole_hop_replayed(hop, 100, Some(500)), "a different stretch");
    }

    /// A recording that stopped at a hole carries on at the next stretch.
    ///
    /// The box ends the playlist at the hole rather than serving what is past
    /// it, so the session finishing is not the recording ending: the moment
    /// after the stop is inside a stretch the picture is at the end of, and what
    /// comes next is the stretch after it.
    #[test]
    fn a_recording_carries_on_past_a_hole() {
        let segments = [
            Segment { start: 100, end: 110 },
            // A second between files: one stretch.
            Segment { start: 111, end: 120 },
            // A hole, and another stretch.
            Segment { start: 400, end: 410 },
            Segment { start: 411, end: 420 },
        ];
        // The whole span has been asked about, so the hole in it is a hole.
        let covered = [(90, 420)];
        assert_eq!(next_stretch(&segments, &covered, 120), Some(400), "past the hole");
        assert_eq!(next_stretch(&segments, &covered, 105), Some(400), "and from inside it");
        assert_eq!(next_stretch(&segments, &covered, 420), None, "the last stretch is the last");
        assert_eq!(next_stretch(&segments, &covered, 90), Some(100), "a stop before it all");
        assert_eq!(next_stretch(&[], &covered, 90), None, "and no list at all");
    }

    /// A list that holds two far-apart stretches says nothing about the time
    /// between them.
    ///
    /// The list is what has been *read*: a viewer who opened playback today and
    /// scrubbed back to the 31st leaves a list with both, and nothing in between.
    /// Offering today as "what comes next" is the bug this is for - the time
    /// between the two has to be asked about first.
    #[test]
    fn a_sparse_list_does_not_say_what_comes_next() {
        let segments = [
            Segment { start: 100, end: 120 },
            // Three days later, and nothing in the list between them.
            Segment { start: 259_300, end: 259_320 },
        ];
        assert_eq!(
            next_stretch(&segments, &[(100, 130)], 120),
            None,
            "the time between them was never asked about"
        );
        assert_eq!(
            next_stretch(&segments, &[(100, 259_400)], 120),
            Some(259_300),
            "and once it has been, the answer is the far stretch"
        );
    }

    /// A stop carries on when the list holds the file that follows it, and only
    /// across the seam between two files cut one after the other - never across
    /// a hole that the strip merely draws as one band.
    #[test]
    fn the_file_after_a_stop_carries_the_recording_on() {
        // Nothing was cut after the stop.
        assert_eq!(carries_on(&[Segment { start: 100, end: 120 }], 120), None);
        // The next file, cut where the one before ended.
        let seam = [
            Segment { start: 100, end: 120 },
            Segment { start: 121, end: 140 },
        ];
        assert_eq!(carries_on(&seam, 120), Some(121));
        // The file cut where the one before ended begins at the stop itself.
        let flush = [
            Segment { start: 100, end: 120 },
            Segment { start: 120, end: 140 },
        ];
        assert_eq!(carries_on(&flush, 120), Some(120));
        // A hole wider than a seam is not a carry-on: a playlist ends there, and
        // a seek into it would end where it began. See [`SEAM`].
        let hole = [
            Segment { start: 100, end: 120 },
            Segment { start: 140, end: 160 },
        ];
        assert_eq!(carries_on(&hole, 120), None, "a hole is not a seam");
        assert_eq!(carries_on(&[], 120), None);
    }

    /// A stop *inside* a file carries on at the stop itself.
    ///
    /// The playlist is served as far as the box has fragments when its range is
    /// read, and the last of them ends partway through the file still being
    /// written: the picture plays to it and the session ends with the recording
    /// continuing. The rest of that file is what is being cut, so the picture
    /// resumes at the stop rather than at the file after it - leaping on would
    /// skip the part of this file that is still coming, and would put the
    /// picture nearer the live edge than the box can serve.
    #[test]
    fn a_stop_inside_a_file_carries_on_at_the_stop() {
        let continuous = [
            Segment { start: 100, end: 120 },
            Segment { start: 121, end: 140 },
        ];
        assert_eq!(carries_on(&continuous, 115), Some(115), "the rest of the file");
        // A file's last second is its seam, not a remainder: the end is rounded
        // up, so a picture on the last frame reads as a second short of it.
        let flush = [
            Segment { start: 100, end: 119 },
            Segment { start: 119, end: 140 },
        ];
        assert_eq!(carries_on(&flush, 118), Some(119), "at the file's own end");
        // The file is not over: waiting for the next one would throw away the
        // part of this one the box has already listed. See [`AfterHole::Waiting`].
        assert_eq!(carries_on(&[Segment { start: 100, end: 120 }], 115), Some(115));
        // A hole after the file stopped in is a hole, not a seam.
        let hole = [
            Segment { start: 100, end: 120 },
            Segment { start: 140, end: 160 },
        ];
        assert_eq!(carries_on(&hole, 118), Some(118), "the rest of the file");
        assert_eq!(carries_on(&hole, 119), None, "the last second is its end");
        assert_eq!(carries_on(&hole, 120), None, "a hole is not a seam");
    }

    /// A playlist that stops before the moment asked for - or less than a
    /// second past it - holds nothing worth playing.
    ///
    /// The box serves fragments only as far as it has cut them, so a playlist
    /// read across the live edge can end at the moment a seek wants to begin at.
    /// A session started on it ends where it began and asks for that moment
    /// again, one request after another. See [`stops_short`].
    #[test]
    fn a_playlist_that_stops_short_holds_nothing_to_play() {
        let playlist = VodPlaylist {
            start: 1000,
            fragments: vec![
                Fragment { name: "a".into(), duration: 10.0 },
                Fragment { name: "b".into(), duration: 10.0 },
            ],
            ..VodPlaylist::default()
        };
        // It is served through 1020.
        assert!(stops_short(&playlist, 1020), "it ends where playback begins");
        assert!(stops_short(&playlist, 1030), "it stops before it");
        // The last second of it is the frame the picture stopped on, counted in
        // whole seconds: playing it is the same stop again.
        assert!(stops_short(&playlist, 1019), "a second of it is not a play");
        assert!(!stops_short(&playlist, 1018), "two seconds of it are ahead");
        let empty = VodPlaylist { start: 1000, ..VodPlaylist::default() };
        assert!(stops_short(&empty, 1000), "nothing served at all");
    }

    /// A session begins at the moment asked for, and never at a second the
    /// playlist has no fragment for.
    ///
    /// The segment list ceils a file's end and the playlist's fragments do not
    /// reach it: a drag to the last second of a stretch began a session at that
    /// second, the box answered "nothing recorded at that moment", and the cursor
    /// sprang back to where the drag started instead of landing on the nearest
    /// picture. See [`served_begin`].
    #[test]
    fn a_session_begins_where_the_playlist_reaches() {
        let playlist = VodPlaylist {
            start: 1000,
            fragments: vec![
                Fragment { name: "a".into(), duration: 10.0 },
                Fragment { name: "b".into(), duration: 10.0 },
            ],
            ..VodPlaylist::default()
        };
        let window = (1000, 1020);
        // Inside it, the moment asked for is the moment played.
        assert_eq!(served_begin(&playlist, window, 1005, false), 1005);
        // Its last second is the one the fragments do not reach.
        assert_eq!(served_begin(&playlist, window, 1020, false), 1019);
        assert_eq!(served_begin(&playlist, window, 1030, false), 1019, "past the end");
        // Never before the range.
        assert_eq!(served_begin(&playlist, window, 900, false), 1000);
        // At the live edge the playlist is short because the fragments are still
        // being cut: the moment asked for is played, and `stops_short` arms the
        // wait. Held back to the playlist's end, a drag to the end of the band
        // landed half a minute earlier and then played forward through it.
        assert_eq!(served_begin(&playlist, window, 1030, true), 1030);
        assert_eq!(served_begin(&playlist, window, 900, true), 1000, "still inside");
    }

    /// A range asked for at the live edge begins on a file's own boundary, and
    /// never so far back that it stops reaching the stop.
    ///
    /// The box cuts every fragment of a range that does not begin on a boundary,
    /// at 671-1034 ms a time; a stop is a fragment's end and rarely a segment's,
    /// and past the last file listed there is no segment to fall back into. See
    /// [`edge_from`].
    #[test]
    fn a_range_at_the_edge_begins_on_a_file_boundary() {
        let segments = [
            Segment { start: 100, end: 110 },
            Segment { start: 111, end: 120 },
            Segment { start: 121, end: 130 },
        ];
        // Inside the list: the start of the file the stop falls in.
        assert_eq!(edge_from(&segments, 125), 121, "the file the stop is in");
        // Past the list: the last file's own start, which is a boundary.
        assert_eq!(edge_from(&segments, 140), 121, "the last file listed");
        // Never further back than the range reaches, or the playlist would end
        // before the moment asked for and the wait would never end.
        let far = 121 + LIVE_SPAN + 60;
        assert_eq!(edge_from(&segments, far), far - LIVE_SPAN, "a range wide enough");
        // Nothing listed at all: the stop is the only moment there is.
        assert_eq!(edge_from(&[], 500), 500);
    }

    /// A stop the list reaches no further than is waited on at the live edge,
    /// and taken for the end of the recording when it is well behind the clock.
    #[test]
    fn a_stop_near_the_clock_is_waited_on() {
        let now = 10_000;
        let stop = now - 60;
        let writing = now - 10;
        assert_eq!(
            after_stop(stop, now, writing),
            AfterHole::Waiting { stop, at: now + LIVE_LOOK }
        );
        // A stop well behind the clock is the end even while the box writes.
        let old = now - LIVE_EDGE - 1;
        assert_eq!(after_stop(old, now, writing), AfterHole::Nothing(old));
        // And the clock is not the only thing that says "still recording": a box
        // whose newest file is an hour old has stopped, and a stop at "now" is
        // then past the end of the recording rather than at the edge of it.
        // Waited on, that is a playlist every couple of seconds for ever, and a
        // picture that never moves. See [`at_live_edge`].
        let stopped = now - 3600;
        assert_eq!(after_stop(now, now, stopped), AfterHole::Nothing(now));
        assert_eq!(after_stop(stop, now, stopped), AfterHole::Nothing(stop));
        // A list that holds nothing says nothing about the box: the clock stands
        // in for it rather than the stop being taken for the end. See
        // [`listed_to`].
        assert_eq!(listed_to(&[], now), now);
        let segments = [Segment { start: 100, end: 120 }];
        assert_eq!(listed_to(&segments, now), 120);
    }

    /// A hole longer than a day is looked past a day at a time, and the chain
    /// stops at the limit rather than walking the archive.
    #[test]
    fn a_hole_longer_than_a_day_is_looked_past_a_day_at_a_time() {
        let day = HOLE_LOOK;
        // The first day held nothing: the next one is asked about.
        assert!(keep_looking(1_000, 1_000, 1_000 + day));
        // The last day of the chain: no further.
        assert!(!keep_looking(1_000, 1_000 + HOLE_LIMIT - day, 1_000 + HOLE_LIMIT));
        // A read narrower than a day is the last of its chain, whatever it found.
        assert!(!keep_looking(1_000, 1_000, 1_000 + 600));
    }

    /// A look's answer is recognised by the range it asked about, so a chain of
    /// them - and the reads the strip makes at the same time - do not answer for
    /// each other.
    #[test]
    fn a_look_is_recognised_by_its_range() {
        let looking = Some(AfterHole::Looking { stop: 1_000, window: (1_000, 2_000), at: 0 });
        assert_eq!(look_past(&looking, 1_000, 2_000), Some(1_000));
        assert_eq!(look_past(&looking, 2_000, 3_000), None, "another day's read");
        assert_eq!(look_past(&looking, 900, 1_100), None, "a read around the cursor");
        assert_eq!(look_past(&Some(AfterHole::Nothing(1_000)), 1_000, 2_000), None);
    }

    /// What has been asked about is kept merged and in order, so a gap between
    /// two reads is a gap and a read that fills it is not.
    #[test]
    fn what_has_been_asked_about_is_merged() {
        let mut covered: Vec<(i64, i64)> = Vec::new();
        cover(&mut covered, 100, 200);
        cover(&mut covered, 300, 400);
        assert_eq!(covered, vec![(100, 200), (300, 400)]);
        cover(&mut covered, 200, 300);
        assert_eq!(covered, vec![(100, 400)], "a read that fills the gap joins them");
        cover(&mut covered, 500, 600);
        cover(&mut covered, 400, 500);
        assert_eq!(covered, vec![(100, 600)]);
        assert!(covered_between(&covered, 150, 550));
        assert!(!covered_between(&covered, 150, 650), "past what was read");
        assert!(!covered_between(&covered, 50, 550), "and before it");
    }

    /// The widest stretch of a window nothing has been asked about.
    ///
    /// It is what decides whether a window that has slid is worth a request, so
    /// the ends and the hole in the middle all count. See [`uncovered_width`].
    #[test]
    fn the_uncovered_part_of_a_window_is_what_is_left_of_it() {
        assert_eq!(uncovered_width(&[], 0, 100), 100);
        assert_eq!(uncovered_width(&[(0, 100)], 0, 100), 0);
        assert_eq!(uncovered_width(&[(0, 60)], 0, 100), 40, "after it");
        assert_eq!(uncovered_width(&[(40, 100)], 0, 100), 40, "before it");
        assert_eq!(uncovered_width(&[(0, 40), (60, 100)], 0, 100), 20, "between");
        assert_eq!(uncovered_width(&[(-50, -10), (10, 50)], -100, 100), 50);
        assert_eq!(uncovered_width(&[(-500, -400)], 0, 100), 100, "none of it in the window");
        // Runs are merged and sorted, so one that begins before the window is
        // still the one that covers its start.
        assert_eq!(uncovered_width(&[(-100, 30), (30, 80)], 0, 100), 20);
    }

    /// What the event list is asked for, and when it is not asked for at all.
    ///
    /// The strip slides under a cursor that does not move, so the window is
    /// always partly new. Every ask is the window and a window either side of it,
    /// which is what makes an answer that replaces the list safe; a window that
    /// has slid by a second is a sliver rather than a request, and one held to
    /// the clock is never asked past it. See [`events_to_ask`].
    #[test]
    fn the_events_of_a_window_are_asked_for_a_window_either_side() {
        let now = 100_000;
        assert_eq!(events_to_ask(&[], 10_000, 600, now, 1), Some((9_400, 10_600)));
        assert_eq!(
            events_to_ask(&[], now - 60, 600, now, 1),
            Some((now - 660, now)),
            "held to the clock, not past it"
        );
        // Answered for: nothing to ask, whatever the wait.
        let covered = [(9_400, 10_600)];
        assert_eq!(events_to_ask(&covered, 10_000, 600, now, 1), None);
        assert_eq!(events_to_ask(&covered, 10_000, 600, now, LIVE_SLICE), None);
        // A window that has slid by a second is a sliver, not a request - and a
        // deliberate read takes the sliver anyway.
        assert_eq!(events_to_ask(&covered, 10_001, 600, now, LIVE_SLICE), None);
        assert_eq!(
            events_to_ask(&covered, 10_001, 600, now, 1),
            Some((9_401, 10_601))
        );
        // Slid by more than the wait, it is worth one.
        assert_eq!(
            events_to_ask(&covered, 10_020, 600, now, LIVE_SLICE),
            Some((9_420, 10_620))
        );
    }

    /// Opening marks the stretch the box answered for, not the hour it asked
    /// about.
    ///
    /// The last file a recorder cut is listed a moment after it closes, so a
    /// read at the live edge can come back without it. Marking the whole
    /// request as asked draws that file as a hole that no later read asks
    /// about again, because the next read begins at the mark.
    #[test]
    fn opening_marks_what_the_box_listed_and_no_more() {
        let read = (1000, 4600);
        let listed = [
            Segment { start: 4300, end: 4360 },
            Segment { start: 4370, end: 4400 },
        ];
        assert_eq!(answered(read, &listed), Some((1000, 4400)), "up to the last file listed");
        assert_eq!(
            answered((1000, 4405), &listed),
            Some((1000, 4400)),
            "and the file that closed too late to be listed stays unasked"
        );
        assert_eq!(answered(read, &[]), None, "nothing listed, nothing known");
        assert_eq!(
            answered(read, &[Segment { start: 4550, end: 4610 }]),
            Some((1000, 4600)),
            "never past the range asked for"
        );
    }

    /// A press at a stop starts again from where the viewer was watching.
    ///
    /// The moment playback opened is the answer while the picture is still in
    /// the same stretch of recording. A viewer who walked to another stretch
    /// would otherwise be carried back to where they opened - another day, when
    /// the stretch they are in is one they scrubbed to - so the beginning of
    /// that stretch is what "again" means there.
    #[test]
    fn a_press_at_a_stop_starts_again_where_the_viewer_is() {
        let segments = [
            Segment { start: 100, end: 200 },
            Segment { start: 1000, end: 1100 },
        ];
        // Opened and stopped inside the same stretch: from where it opened.
        assert_eq!(restart_from(&segments, 120, 195), 120);
        // Opened far away, stopped in the second stretch: its beginning.
        assert_eq!(restart_from(&segments, 150, 1095), 1000);
        // A stop the list does not cover falls back to where it opened.
        assert_eq!(restart_from(&segments, 150, 5000), 150);
    }

    /// A recording that ran out of range is not moving, and the corner says so
    /// the same way it says a hold: the mark is about the picture, and the
    /// timeline is where the reason lives.
    #[test]
    fn a_recording_that_ran_out_is_still() {
        assert!(!still(false, Some(ConnectionState::Streaming)), "playing");
        assert!(still(true, Some(ConnectionState::Streaming)), "held by the viewer");
        assert!(still(false, Some(ConnectionState::Finished)), "or out of range");
        assert!(still(true, Some(ConnectionState::Finished)));
        // A channel that has not reported yet is not the end of anything, and
        // one that is connecting is a seek on its way.
        assert!(!still(false, None));
        assert!(!still(false, Some(ConnectionState::Connecting)));
        // A failed read is a picture that stopped for a reason the tile names
        // elsewhere - it is not the range ending.
        assert!(!still(false, Some(ConnectionState::Failed)));
    }

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

    /// A held arrow walks further and steps sooner the longer it is held - which
    /// is what makes a hold accelerate rather than merely stride, and what keeps
    /// a key that is down every frame from crossing the strip three times a
    /// second.
    #[test]
    fn a_held_arrow_walks_further_and_steps_sooner() {
        let at = |millis: u64| hold_step(100, std::time::Duration::from_millis(millis));
        assert_eq!(at(0), (100, REPEAT_SLOW), "a press is one step");
        assert_eq!(at(600), (100, REPEAT_SLOW), "and half a second is not yet a hold");
        assert_eq!(at(700), (200, REPEAT_MID), "then twice the ground, sooner");
        assert_eq!(at(1600), (400, REPEAT_FAST), "and four times, sooner still");
        assert!(REPEAT_FAST < REPEAT_MID && REPEAT_MID < REPEAT_SLOW);
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

    /// The strip's focus as the application does it: a button above, the bar's
    /// seat beside it, the strip below, Down moving in, Left walking it - and the
    /// two rules the end of a frame has for a focus that is not the bar's own.
    ///
    /// **That is what took it**: the strip was entered and handed to the seat in
    /// the same frame - the button's ring went, nothing had the keys - and the
    /// next arrow walked off the seat onto the first control, which is exactly
    /// "the focus disappears from the button, and left or right brings it back".
    #[test]
    fn the_strip_keeps_the_focus_after_it_is_entered() {
        let (button, seat, strip) = (Id::new("button"), Id::new("seat"), Id::new("strip"));
        let walk = |keep_strip: bool| {
            let ctx = egui::Context::default();
            let mut seen = Vec::new();
            for frame in 0..6 {
                let key = match frame {
                    1 => Some(egui::Key::ArrowDown),
                    3 | 4 => Some(egui::Key::ArrowLeft),
                    _ => None,
                };
                let events = key
                    .map(|key| {
                        vec![egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        }]
                    })
                    .unwrap_or_default();
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 600.0))),
                    events,
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let top = ui.max_rect().min;
                        let button_rect = Rect::from_min_size(top, vec2(100.0, 40.0));
                        let strip_rect =
                            Rect::from_min_size(top + vec2(0.0, 60.0), vec2(600.0, 50.0));
                        let _ = ui.interact(button_rect, button, Sense::click());
                        let strip_response = ui.interact(strip_rect, strip, Sense::click_and_drag());
                        if frame == 0 {
                            ctx.memory_mut(|memory| memory.request_focus(button));
                        }
                        // The bar's own rule: the first arrow steps off the seat
                        // onto the first control.
                        if ctx.memory(|memory| memory.focused()) == Some(seat) && key.is_some() {
                            ctx.memory_mut(|memory| memory.request_focus(button));
                        }
                        // The strip's: Down enters it, and a focused strip locks
                        // the arrows. See `timeline_input`.
                        if frame == 1 && ctx.memory(|memory| memory.focused()) == Some(button) {
                            strip_response.request_focus();
                        }
                        if strip_response.has_focus() {
                            ctx.memory_mut(|memory| {
                                memory.set_focus_lock_filter(
                                    strip,
                                    egui::EventFilter {
                                        horizontal_arrows: true,
                                        vertical_arrows: true,
                                        ..Default::default()
                                    },
                                );
                            });
                        }
                    });
                    // The end of the frame, in the order the application has it:
                    // a focus the layer does not own is surrendered, and one that
                    // is not the bar's own goes back to the seat.
                    let kept = ctx.memory(|memory| memory.focused());
                    let strip_kept = keep_strip && kept == Some(strip);
                    if let Some(focused) = kept {
                        if focused != button && !strip_kept {
                            ctx.memory_mut(|memory| memory.surrender_focus(focused));
                        }
                    }
                    if !strip_kept {
                        let focused = ctx.memory(|memory| memory.focused());
                        if focused != Some(button) && focused != Some(seat) {
                            ctx.memory_mut(|memory| memory.request_focus(seat));
                        }
                    }
                });
                seen.push(ctx.memory(|memory| memory.focused()));
            }
            seen
        };

        // As it was: entered and given up in one frame - the seat draws nothing,
        // so the ring on the button simply goes - and the next arrows walk the
        // seat onto the first control.
        let taken = walk(false);
        assert_eq!(taken[1], Some(seat), "the strip is entered and given up at once");
        assert_eq!(taken[4], Some(button), "and the arrows walk the seat onto the button");

        // Kept: Down enters, and the arrows stay where they are put.
        let kept = walk(true);
        assert_eq!(kept[1], Some(strip), "Down takes the focus into the strip");
        assert_eq!(kept[2], Some(strip), "and the frame after keeps it");
        assert_eq!(kept[3], Some(strip), "a Left does not take it back");
        assert_eq!(kept[4], Some(strip), "nor does a second Left");
    }

    /// egui walks the focus itself when an arrow is pressed: the widget in that
    /// direction takes it, unless the focused one claims the arrow. That is what
    /// took the strip's focus away the moment a viewer walked it - one press and
    /// the strip was gone - and what the lock in `timeline_input` is for.
    #[test]
    fn egui_walks_the_focus_on_an_arrow_unless_the_widget_claims_it() {
        // Two widgets, one above the other, with an arrow pressed down the page.
        let walk = |lock: bool| {
            let ctx = egui::Context::default();
            let (above, below) = (Id::new("above"), Id::new("below"));
            for frame in 0..4 {
                let events = if frame == 2 {
                    vec![egui::Event::Key {
                        key: egui::Key::ArrowDown,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }]
                } else {
                    Vec::new()
                };
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 600.0))),
                    events,
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let top = ui.max_rect().min;
                        let above_rect = Rect::from_min_size(top, vec2(100.0, 20.0));
                        let below_rect =
                            Rect::from_min_size(top + vec2(0.0, 100.0), vec2(100.0, 20.0));
                        let _ = ui.interact(above_rect, above, Sense::click());
                        let _ = ui.interact(below_rect, below, Sense::click());
                        if frame == 0 {
                            ctx.memory_mut(|memory| memory.request_focus(above));
                        }
                        if lock && ctx.memory(|memory| memory.has_focus(above)) {
                            ctx.memory_mut(|memory| {
                                memory.set_focus_lock_filter(
                                    above,
                                    egui::EventFilter {
                                        horizontal_arrows: true,
                                        vertical_arrows: true,
                                        ..Default::default()
                                    },
                                );
                            });
                        }
                    });
                });
            }
            ctx.memory(|memory| memory.focused())
        };
        assert_eq!(walk(false), Some(Id::new("below")), "the arrow walks the focus away");
        assert_eq!(walk(true), Some(Id::new("above")), "and a widget that claims it stays");
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

    /// The calendar's row follows its ring: a day the remote walks to is a day
    /// the row slides to show, and one already in view leaves the row alone.
    #[test]
    fn the_calendar_follows_its_ring() {
        // Three days of fifty points across a row that shows two.
        let (each, total, length) = (50.0, 150.0, 100.0);
        assert_eq!(day_scroll_for(1, each, 0.0, length, total), 0.0, "already in view");
        assert_eq!(day_scroll_for(2, each, 0.0, length, total), 50.0, "the third is slid in");
        assert_eq!(day_scroll_for(0, each, 50.0, length, total), 0.0, "and the first back");
        // A row that holds them all never moves.
        assert_eq!(day_scroll_for(2, each, 0.0, 210.0, total), 0.0);
    }

    /// The speed control's ladder and its word are the same ladder: the number
    /// the viewer reads is the one the next press leaves behind, and eight is the
    /// end of it.
    #[test]
    fn the_speed_ladder_and_its_word_agree() {
        let mut speed = 1.0;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(speed_label(speed));
            speed = next_speed(speed);
        }
        assert_eq!(seen, ["1×", "2×", "4×", "8×", "1×"], "one, two, four, eight, one");
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

    /// A stretch missing from the list is read a slice at a time, outwards from
    /// the band already drawn: the strip grows toward the edge the drag
    /// uncovered instead of holding a band of nothing until a day of segments
    /// has arrived.
    #[test]
    fn a_missing_stretch_is_read_a_slice_at_a_time() {
        // Nothing read: the slice starts where the band is, at `from`.
        assert_eq!(
            read_slice(&[], -86_400, 0, READ_CHUNK),
            Some((-86_400, -86_400 + READ_CHUNK)),
            "the read begins at the band, not at the edge"
        );
        // A stretch no wider than a slice is one request, not two.
        assert_eq!(read_slice(&[], -600, 0, READ_CHUNK), Some((-600, 0)));
        // Asked about: nothing to read, and nothing asked for again.
        assert_eq!(read_slice(&[(-600, 0)], -600, 0, READ_CHUNK), None);
        // A hole in the middle is stepped over rather than stopping the read:
        // what has been answered for is skipped, and the read goes on past it.
        assert_eq!(
            read_slice(&[(-86_400, -80_000)], -86_400, 0, READ_CHUNK),
            Some((-80_000, -80_000 + READ_CHUNK))
        );
    }

    /// The strip's left side fills from the band that is already there, out
    /// towards the edge the drag uncovered: two bands meeting in the middle
    /// read as two strips rather than one.
    #[test]
    fn a_missing_stretch_is_read_from_the_band_outwards() {
        // Nothing read: the slice still ends against the cursor, where the band
        // is, and runs left from it.
        assert_eq!(
            read_slice_back(&[], -86_400, 0, READ_CHUNK),
            Some((-READ_CHUNK, 0)),
            "the read starts at the band, not at the edge"
        );
        // A stretch no wider than a slice is one request, not two.
        assert_eq!(read_slice_back(&[], -600, 0, READ_CHUNK), Some((-600, 0)));
        // Asked about: nothing to read, and nothing asked for again.
        assert_eq!(read_slice_back(&[(-600, 0)], -600, 0, READ_CHUNK), None);
        // The slice is taken beside the band already drawn, however much unread
        // time lies beyond it.
        assert_eq!(
            read_slice_back(&[(-600, 0)], -86_400, 0, READ_CHUNK),
            Some((-600 - READ_CHUNK, -600)),
            "the band grows outwards from what is drawn"
        );
        // Once that has landed, the next slice is the one further out.
        assert_eq!(
            read_slice_back(&[(-600 - READ_CHUNK, 0)], -86_400, 0, READ_CHUNK),
            Some((-600 - 2 * READ_CHUNK, -600 - READ_CHUNK))
        );
        // A stretch read earlier is stepped over: the gap behind it is read too.
        assert_eq!(
            read_slice_back(&[(-86_400, -80_000), (-600, 0)], -86_400, 0, READ_CHUNK),
            Some((-600 - READ_CHUNK, -600))
        );
        // With the finger up, the stretch that is left is one read, not a
        // chain of slices: twelve hours of it are asked for at once.
        assert_eq!(
            read_slice_back(&[(-600, 0)], -600 - READ_WHOLE, 0, READ_WHOLE),
            Some((-600 - READ_WHOLE, -600))
        );
        // And a gap wider than that is still a bounded request, not the whole
        // of it: the answer grows with the width.
        assert_eq!(
            read_slice_back(&[(-600, 0)], -86_400, 0, READ_WHOLE),
            Some((-600 - READ_WHOLE, -600))
        );
    }

    /// A step that lands in a hole crosses it, both ways.
    ///
    /// The nearest edge is the one the step came from, so a hole was a wall the
    /// arrows could not walk over: the last file of one day to the first of the next
    /// is a night wide, and every press landed back where it started. A moment past
    /// the end of the list is not a hole, though - it is a stretch nobody has read,
    /// and the step waits for the read. See [`across_hole`].
    #[test]
    fn a_step_crosses_a_hole() {
        let segments = [
            Segment { start: 100, end: 200 },
            Segment { start: 20_000, end: 20_100 },
        ];
        // Inside a stretch, the step is the step - the very end of one included.
        assert_eq!(across_hole(&segments, 150, true), 150);
        assert_eq!(across_hole(&segments, 20_050, false), 20_050);
        assert_eq!(across_hole(&segments, 200, false), 200, "the end of a file");
        assert_eq!(across_hole(&segments, 20_000, true), 20_000, "the start of one");
        // In the hole: the next picture that way, not the edge it came from. Back
        // is a second inside the file, because its listed end is ceiled and the
        // box will not serve that second. See [`across_hole`].
        assert_eq!(across_hole(&segments, 1_000, true), 20_000, "forward");
        assert_eq!(across_hole(&segments, 1_000, false), 199, "back");
        // Nothing that way: where the step landed, and the read is what answers it.
        assert_eq!(across_hole(&segments, 50, false), 50, "before the first");
        assert_eq!(across_hole(&segments, 20_200, true), 20_200, "after the last");
    }

    /// The frames of the session a seek is leaving are not the arrival it waits
    /// for, and only where they are tells them apart.
    ///
    /// A seek backwards is "caught up" by every one of them, because they are
    /// all later than the moment asked for: the cursor went to the old picture
    /// and the step was made again from there, which a held arrow showed as a
    /// strip jittering between two days. See [`belongs_to_the_old_picture`].
    #[test]
    fn a_frame_of_the_session_being_left_is_not_an_arrival() {
        // Stepping back from 11/4 to 11/2, with the old session still playing.
        let (from, pending) = (1793805072, 1793599973);
        assert!(belongs_to_the_old_picture(from, pending, 1793805072.4));
        assert!(belongs_to_the_old_picture(from, pending, 1793805073.0), "it plays on");
        // The new session's own first frame, a fragment before the moment asked
        // for, is the arrival.
        assert!(!belongs_to_the_old_picture(from, pending, 1793599960.0));
        assert!(!belongs_to_the_old_picture(from, pending, 1793599973.0));
        // A step of a second is still told apart.
        let (from, pending) = (1_000, 999);
        assert!(belongs_to_the_old_picture(from, pending, 1_000.0));
        assert!(!belongs_to_the_old_picture(from, pending, 998.5));
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
