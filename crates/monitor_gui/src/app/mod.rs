//! Application state and the egui render loop.
//!
//! The UI thread never touches a socket or a decoder: it drains the channel
//! event queues (oneshot per frame), recomputes the decoding schedule and paints
//! the grid. Everything else happens on the tokio runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::{Align, Align2, CornerRadius, FontId, Id, Key, Layout, Modifiers, Rect, RichText, Sense, StrokeKind, vec2};
use tokio::runtime::Handle;

use monitor_core::autostart::{self, AutostartStatus};
use monitor_core::power::{self, PowerStatus};
use monitor_core::reboot;
use monitor_core::config::{AppConfig, Orientation};
use monitor_core::layout::{GridLayout, NavigateOutcome};
use monitor_core::model::{infer_sub_stream, ConnectionState, OsdItem, StreamKind, TileAspect};
use monitor_core::pipeline::{CallFailure, ChannelManager, StreamEvent, StreamStats};
use monitor_core::scheduler::Scheduler;
use monitor_core::talk::{TalkRoute, ToneFailure};
use monitor_core::{CameraSource, Direction};

use crate::dialogs::{self, BackgroundEvent, CameraDraft, DiscoveryUi, Tab};
use self::file_picker::FilePicker;
use crate::fonts;
use crate::grid::{self, PictureZoom, Tile, TileActions};
use crate::icons::{self, Icon};
use crate::controls;
use crate::nav::{Axis, Dir, Exit, Kind, Nav, ScopeDef};
use crate::theme;
use crate::tilebar;
use crate::video::{VideoRenderer, VideoSurface};
use crate::window::{self, OnTop};
use crate::RunOptions;

/// Debounce applied before writing the config file.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// How long the wall stays off the top after a link is opened, so that the
/// window the browser or the mail client opens comes up in front of it.
///
/// A clock rather than an event: nothing tells us when another program's window
/// appears, and while the viewer is reading that page they are not looking at
/// the wall anyway.
const LINK_TAKEOVER: Duration = Duration::from_secs(5);

/// Repaint interval while at least one channel is showing video.
///
/// 25 Hz, which is what the sub-streams of a wall are coded at: repainting
/// faster only drew the same picture again. It is the cheapest of the costs
/// `priv/docs/grid-cpu-cost.md` counts - a sixth of the whole frame's
/// tessellation and composition, for a number.
const LIVE_REPAINT: Duration = Duration::from_millis(40);

/// Width the toolbar is laid out for at its full size.
///
/// A television and the desktop window are at least this wide; a phone in
/// landscape is narrower, and the row has to stay one row - a second one would
/// push the grid down, and the grid is what the viewer is for. Below this the
/// bar's text is scaled down until it fits.
const TOOLBAR_REFERENCE_WIDTH: f32 = 960.0;

/// Smallest the toolbar is allowed to shrink to, as a fraction of its text size.
const TOOLBAR_MIN_SCALE: f32 = 0.6;

/// How long the question the wall asks on the first Back stands.
///
/// Back sits beside the arrows a remote is steered with, and a program that
/// left on one unmeant press cannot be brought back by the press that made the
/// mistake. The first press therefore asks, and only a second one inside this
/// window leaves. It also bounds how long the question is drawn for.
const EXIT_WINDOW: Duration = Duration::from_secs(2);

/// How long the focus ring stands after the last input.
///
/// Its own clock, and deliberately not the bars': those fade after
/// `chrome_idle_secs`, which is a setting the viewer chooses, and the ring is a
/// cursor rather than a part of them - it stands on the wall while the viewer is
/// using it and no longer. The two meet at the two ends of the bars' life and
/// nowhere else: they arm the ring as they appear and take it with them as they
/// go, and a wall driven with the bars away keeps the ring it is aimed with. See
/// [`XgViewApp::ring_follows_bars`].
const RING_IDLE: Duration = Duration::from_secs(8);

/// Whether a focus the navigation layer does not own is still the magnified
/// viewport's own, and must be left where the viewer put it.
///
/// The seat and a recording's strip both draw a widget and both keep their
/// arrows, but neither is a member of that layer. Surrendering the seat at the
/// end of every frame rebuilt it with a default event filter, so the lock
/// `tilebar::draw` puts on it never survived to the next frame's `begin_pass`;
/// egui then walked the focus off in whichever direction the arrow pointed.
/// egui reads the *raw* events at the start of a frame, so consuming a key
/// later cannot stop that walk - only the filter can.
fn viewport_keeps_focus(
    focused: Id,
    open: bool,
    timeline_shown: bool,
    timeline_id: Option<Id>,
) -> bool {
    // Only while the bar is up. Once it is hidden the seat's widget is not
    // drawn, and keeping a focus on an id that no widget owns leaves the wall
    // without its keys for the frames it takes egui to notice.
    open
        && (focused == tilebar::seat()
            || (timeline_shown && timeline_id.is_some() && Some(focused) == timeline_id))
}

/// How long a pointer press may last and still be a tap, in seconds.
///
/// A finger rests on the glass while the viewer reads the picture, and a press
/// that lingers is not a press on anything: the magnified viewport's bar is put
/// away by a tap, not by every press that happens to end over the picture.
const TAP_SECS: f64 = 0.3;

/// How far a pointer press may slide and still be a tap, in points.
///
/// Tighter than egui's own click distance (6 points): a grip that slid while it
/// pressed is a gesture in the making - the viewer reaching for a pan - and the
/// picture treats it as one, so it must not also press the bar away.
const TAP_POINTS: f32 = 4.0;

/// Width of the settings side panel, and the narrowest it may be dragged to.
///
/// A phone in landscape is barely wider than the panel at its default, which is
/// why it is a side panel and not a window: it can be dragged narrow, and on a
/// narrow screen the viewer mostly has it closed.
const SETTINGS_WIDTH: f32 = 360.0;
const SETTINGS_MIN_WIDTH: f32 = 300.0;

/// The project's page on GitHub, shown in the About tab.
const REPOSITORY_URL: &str = "https://github.com/xhbl/xgview";

/// How far a direction press scrolls the settings body when it has nowhere left
/// to move the focus, as a fraction of the window's height.
///
/// The body is scrolled by moving the focus onto a control - see [`crate::nav`]
/// - which leaves whatever follows the last control out of a remote's reach:
/// the adb line, the table of keys. The press that walks off the end of the body
/// moves the body instead, by a little less than a screenful so the viewer keeps
/// the context of what has just left. See `body_scroll`.
const REMOTE_SCROLL_FRACTION: f32 = 0.6;

/// Side of the edit / remove buttons of a camera row, in points.
///
/// Smaller than a toolbar button: a row is read as a line of text, and a
/// control the size of the bar's would dwarf the name it sits beside.
const ROW_ICON: f32 = 24.0;

/// Auto-hide state of the top and bottom bars.
///
/// A wall left running is nothing but pictures, so in full screen the bars get
/// out of the way on their own. They come back on the first sign of a viewer,
/// and the settings panel and the device window hold them on screen: hiding
/// the bar of a panel that was just opened would strand the panel.
#[derive(Debug, Clone, Copy)]
struct Chrome {
    visible: bool,
    /// When input was last seen, which is what the fade is timed from.
    last_input: Instant,
    /// Set by a request for the picture and nothing else - entering full screen
    /// - and consumed by the next `chrome_visible` call, which takes the bars
    /// away at once. The click that asks for full screen is itself input, so
    /// without this it would be read as a request to see the bars.
    hide_now: bool,
    /// Whether the viewer asked for the bars this frame. Read before the keys
    /// are handed out, because the application takes the ones it acts on out of
    /// the queue. See [`XgViewApp::viewer_active`] for what counts as asking.
    input_seen: bool,
    /// Where the two bars were the last time they were drawn.
    ///
    /// Kept while they are hidden, because that is when it is needed: a press
    /// in the band a bar sits in is the pointer's way of asking for it back,
    /// and while the bars are gone the wall covers that band - so the last
    /// rectangle they were drawn in is the only description of where it is.
    /// `None` until they have been drawn once.
    bars: Option<(Rect, Rect)>,
    /// Swallow the primary gesture that revealed Clean View's controls,
    /// including its release on a later frame.
    consume_primary: bool,
    /// Back reveals controls before it can unwind the wall this frame.
    reveal_back: bool,
    /// An interaction ending starts a fresh countdown even without intervening frames.
    clean_held: bool,
    /// The reveal click must not count as the first half of a tile double-click.
    reveal_double_click_until: f64,
}

/// The bar an arrow walked off the wall for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bar {
    Toolbar,
    Status,
}

/// The picture of one channel currently on the GPU, with the frame it came from.
pub struct ChannelTexture {
    /// Sequence number of the frame currently uploaded.
    sequence: u64,
    /// The moment that frame is of, when it came from a recording.
    at: Option<f64>,
    surface: VideoSurface,
}

/// Runtime information of one channel, fed by the pipeline events.
#[derive(Debug, Clone)]
pub struct ChannelUi {
    pub state: ConnectionState,
    pub detail: String,
    /// Stream currently decoded.
    pub stream: Option<StreamKind>,
    pub codec: Option<String>,
    /// True while this channel's picture is decoded on the GPU.
    pub hardware: bool,
    /// True once the stream has proved to carry an audio track this build can
    /// decode. The magnified bar's sound switch is offered on it.
    pub audio: bool,
    /// Whether the stream has said anything about its audio yet.
    ///
    /// `audio` above is the answer, and before it arrives it is false - but
    /// false means "not proved", not "there is none". The two are told apart
    /// here, because the bar's sound switch is a place in the row and a place
    /// that is taken and given back moves every control after it: a switch that
    /// appeared once the stream had spoken and vanished while it had not would
    /// have the whole row jumping under the remote's ring.
    pub audio_known: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The size the **Main** stream last reported, kept after the channel goes
    /// back to Sub. A session names its size in its first sample, before any
    /// picture, so while Main is opening the wall can already put the Sub
    /// frame it is holding into Main's box. `width`/`height` above describe
    /// whichever stream is arriving now and revert to 640x480 in the grid;
    /// this one does not.
    pub main_size: Option<(u32, u32)>,
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub total_frames: u64,
    /// Stream decoded before the current switch. While it is set the viewport
    /// keeps the last frame of that stream instead of showing a black frame.
    pub switching_from: Option<StreamKind>,
    /// Which sample of the pipeline's statistics was applied last. The numbers
    /// are read from a store rather than delivered, so this is what keeps a
    /// repaint from applying the same one again.
    stats_sequence: u64,
}

impl Default for ChannelUi {
    fn default() -> Self {
        Self {
            state: ConnectionState::Idle,
            detail: String::new(),
            stream: None,
            codec: None,
            hardware: false,
            audio: false,
            audio_known: false,
            width: None,
            height: None,
            main_size: None,
            fps: 0.0,
            bitrate_kbps: 0.0,
            total_frames: 0,
            switching_from: None,
            stats_sequence: 0,
        }
    }
}

impl ChannelUi {
    fn apply(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::State { state, detail, .. } => {
                self.state = state;
                self.detail = detail;
                if matches!(
                    state,
                    ConnectionState::Reconnecting | ConnectionState::Connecting | ConnectionState::Failed
                ) {
                    self.fps = 0.0;
                    self.bitrate_kbps = 0.0;
                    self.total_frames = 0;
                }
                // A channel that has given up will never deliver the picture the
                // switch is waiting for, and the band that says "switching to X,
                // holding the last frame" would then sit over the failure panel
                // for as long as the failure lasts - the one thing on the tile
                // that still claims something is coming. Only the sample of a
                // session that produced a picture clears it otherwise, and a
                // session that was refused never produces one. See
                // `ChannelUi::apply_stats`.
                if state == ConnectionState::Failed {
                    self.switching_from = None;
                }
            }
            StreamEvent::Transition { from, to, .. } => {
                self.switching_from = from.filter(|from| *from != to);
                self.stream = Some(to);
                self.fps = 0.0;
                self.bitrate_kbps = 0.0;
                self.total_frames = 0;
            }
            // How this camera would be talked to, which is not this channel's
            // state: the interface takes it before an event reaches a channel,
            // and caches it by address. See `App::talk_route`.
            StreamEvent::Talk { .. } => {}
            // Likewise a failed call, which ends the call rather than saying
            // anything about the channel: see `App::poll_events`.
            StreamEvent::CallFailed { .. } => {}
        }
    }

    /// Takes one sample of the pipeline's statistics, if it is a new one.
    ///
    /// The numbers describe the stream as it is now, so the newest sample is
    /// the whole of what there is to know: applying it twice says nothing the
    /// first application did not.
    fn apply_stats(&mut self, sequence: u64, stats: &StreamStats) {
        if sequence == self.stats_sequence {
            return;
        }
        self.stats_sequence = sequence;
        self.stream = Some(stats.stream);
        if stats.codec.is_some() {
            self.codec = stats.codec.clone();
        }
        self.hardware = stats.hardware;
        self.audio = stats.audio;
        // The stream has now said what its audio is, which is not the same as
        // saying there is any: the switch is offered on a stream that has
        // spoken and greyed on one that has not. See `audio_known`.
        self.audio_known = true;
        if stats.width.is_some() {
            self.width = stats.width;
        }
        if stats.height.is_some() {
            self.height = stats.height;
        }
        // A Main session's first sample carries the size from the stream's
        // description, before its first picture. Keep it for the Sub frame
        // held while that picture is on its way; see `main_size`.
        if stats.stream == StreamKind::Main {
            if let (Some(width), Some(height)) = (stats.width, stats.height) {
                self.main_size = Some((width, height));
            }
        }
        self.fps = stats.fps;
        self.bitrate_kbps = stats.bitrate_kbps;
        self.total_frames = stats.total_frames;
        // The new stream delivered data: the held frame can be dropped.
        if stats.total_frames > 0 {
            self.switching_from = None;
        }
    }
}

/// One page of the grid, either the visible one or the one sliding out.
#[derive(Debug, Clone)]
struct PageView {
    layout: GridLayout,
    /// Global index of this page's first slot. An empty cell is numbered from
    /// here, so the numbers keep counting across pages instead of starting
    /// over at 01 on every page.
    first: usize,
    cells: Vec<Option<usize>>,
}

/// Pagination / swipe animation state.
#[derive(Debug, Clone, Default)]
struct Slide {
    /// Displacement of the visible page, in page widths (0 = settled).
    offset: f32,
    /// `+1` when the page entered from the right, `-1` from the left.
    dir: f32,
    /// Page sliding out.
    outgoing: Option<PageView>,
    /// Live finger / mouse displacement, in points.
    drag: f32,
    dragging: bool,
}

impl Slide {
    fn start(&mut self, dir: f32, outgoing: Option<PageView>) {
        self.offset = dir;
        self.dir = dir;
        self.outgoing = outgoing;
        self.drag = 0.0;
    }

    fn is_animating(&self) -> bool {
        self.offset != 0.0 || self.drag != 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Error,
}

#[derive(Debug, Clone)]
struct Toast {
    text: String,
    kind: ToastKind,
    at: Instant,
    /// Middle of the screen rather than the usual place above the status bar.
    ///
    /// The magnified bar's fit switch has no word left to read the new value
    /// on, so it says it in the one place a viewer looking at the picture
    /// already is; the news a channel reports stays where it has always been.
    center: bool,
}

/// Page of the settings side panel.
///
/// The panel used to be one long column. Six sections of it are read by
/// different people - the wall's layout, the camera list, the stream policy,
/// the machine it runs on, the hours it blanks itself, and the version - and
/// scrolling past five of them to reach the sixth is what the tabs remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    Display,
    Cameras,
    Streams,
    Blackout,
    System,
    About,
}

impl SettingsTab {
    /// How many there are.
    ///
    /// Named so that anything holding one entry per tab - the strip's ids, for
    /// one - is sized from the list itself and cannot fall behind it. It did:
    /// a sixth tab read past the end of a five-entry array and took the whole
    /// panel down on the frame it was opened.
    const COUNT: usize = 6;

    const ALL: [Self; Self::COUNT] =
        [Self::Display, Self::Cameras, Self::Streams, Self::Blackout, Self::System, Self::About];

    fn label(self) -> &'static str {
        match self {
            Self::Display => "settings-tab-display",
            Self::Cameras => "settings-tab-cameras",
            Self::Streams => "settings-tab-streams",
            Self::System => "settings-tab-system",
            Self::Blackout => "settings-tab-blackout",
            Self::About => "settings-tab-about",
        }
    }
}

/// Who answers a direction press.
///
/// egui walks the focusable widgets of the bars, the settings panel and the
/// windows with the same four directions a remote control sends, and it does so
/// before the application runs; the wall is not made of widgets and answers
/// them here. Exactly one of the two is in charge of a given press, and which
/// one is decided by where the remote's focus was when the frame began - since
/// that is what the viewer can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// The wall: the arrows move the channel focus, Enter magnifies.
    Grid,
    /// One of the widgets in the bars, the settings panel or a window.
    Controls,
}

/// What one of the magnified bar's controls does.
///
/// The bar is a list of controls in walk order, and a press - by the remote's
/// confirm key or by a finger - answers the one it landed on. Naming the act
/// rather than the place keeps the two apart: the quarter turn is only on the
/// bar upright, so an index would mean one thing in one orientation and another
/// in the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TileBarAction {
    /// Turn the picture a quarter turn, or back.
    Rotate,
    /// Move the picture fit on to the next one.
    Fit,
    /// Turn the sound on or off.
    Sound,
    /// Start or end the talk-back.
    Mic,
    /// End the call the bar has taken over for.
    Hangup,
    /// Play this camera's recordings in the viewport.
    Playback,
    /// Back over the recording by ten seconds.
    StepBack,
    /// The same, forwards.
    StepForward,
    /// Hold the picture, or let it go.
    PlayPause,
    /// Step to the event before this moment.
    PrevEvent,
    /// Step to the event after it.
    NextEvent,
    /// More detail on the timeline.
    ZoomIn,
    /// The whole of it.
    ZoomOut,
    /// Play faster, or back to normal.
    Speed,
    /// Show the days, or go back to the stretch of one.
    Date,
    /// Save the recording being watched to a file.
    Download,
    /// Leave the recording and go back to the camera.
    Live,
    /// The place a sound switch would stand in, on a stream with no audio
    /// track.
    ///
    /// The switch is the one control that comes and goes with the picture, and
    /// a control that comes and goes takes every control after it with it. The
    /// place is kept, and drawn as a grey speaker rather than left empty, so
    /// the row does not shift under the remote's ring and the viewer can see
    /// what is missing. It answers nothing and takes no press. See
    /// `tilebar::Control::Silent`.
    NoSound,
    /// The mark a strip carries while the box answers a question about a
    /// recording. It is a place in the row, not a control: nothing presses it,
    /// nothing focuses it, and nothing on the strip can be pressed beside it.
    Waiting,
}

/// The control the ring is anchored to, the picture it belongs to, and when the
/// anchor was set.
///
/// The anchor is what holds the ring on a strip whose row is being replaced:
/// it names the control - the one that undoes the switch - rather than the
/// place the control stood in, because the place is not the same twice. It is
/// given a time as well, because a press that asked for something the box
/// never answered would otherwise leave an anchor behind for good, and an
/// anchor that outlives its question puts the ring on a control the viewer
/// was never shown.
#[derive(Debug, Clone, Copy)]
struct RingAnchor {
    /// The control the ring belongs on.
    action: TileBarAction,
    /// The picture the anchor was asked for: another picture is another ring.
    channel: usize,
    /// When the anchor was set.
    since: Instant,
}

/// How long the ring's anchor waits for the row it names.
///
/// As long as a box may take to answer a question about a recording, and then
/// a little: an anchor that gave up early would put the ring nowhere on a slow
/// answer, and one that never gave up would be an anchor with no question
/// behind it.
const ANCHOR_WAIT: Duration = Duration::from_secs(30);

/// Detects the frame a panel appears on.
///
/// The remote's focus starts on the wall, and a panel that opens without taking
/// it is unreachable: the arrows would go on walking channels behind it. This
/// is what hands it over, once, as the panel appears.
#[derive(Debug, Default)]
struct Handoff {
    open: bool,
}

impl Handoff {
    /// True on the single frame `open` goes from false to true.
    fn entering(&mut self, open: bool) -> bool {
        let entering = open && !self.open;
        self.open = open;
        entering
    }

    /// Hands the focus to the control a window has just put up, on the one frame
    /// it opens, and says whether it opened.
    ///
    /// `fresh_caret` is for a form whose fields carry the same id on every
    /// visit. egui keeps the caret in the *persisted* state that id carries, so
    /// a field that has never been focused starts the caret wherever the
    /// previous visit left it - in the middle of the next camera's name.
    /// Dropping the state hands the choice back to egui, which puts it at the
    /// end.
    ///
    /// The order is the reason this is one function: the state has to be dropped
    /// before the focus arrives, and a window that has a payload has no other
    /// place to say so.
    fn takes_focus(
        &mut self,
        ctx: &egui::Context,
        open: bool,
        anchor: Option<Id>,
        fresh_caret: bool,
    ) -> bool {
        if !self.entering(open) {
            return false;
        }
        if let Some(id) = anchor {
            if fresh_caret {
                egui::TextEdit::store_state(ctx, id, Default::default());
            }
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
        true
    }
}

/// What every modal window in this program carries.
///
/// The four things a window needs and nothing else: the payload it is showing -
/// or `None`, which is a window that is not up - the frame it appeared on, the
/// control it opens on, and the one it gives the focus back to when it closes.
/// They were four fields per window, written out seven times and told apart by a
/// suffix; five of the seven windows are on this type now.
///
/// The payload is what makes one type serve both kinds of window: a picker has
/// nothing to carry and holds `()`, while the camera form holds the draft being
/// edited. Whether a window is up is not a field of its own, so it cannot say
/// one thing while the payload says another.
#[derive(Debug)]
struct Modal<T> {
    /// What the window is showing, or `None` while it is closed.
    payload: Option<T>,
    /// Watched for the frame it appears on, so the remote's focus lands on its
    /// first control instead of walking the wall behind it.
    handoff: Handoff,
    /// First control of the window, to be handed the focus as it opens.
    anchor: Option<Id>,
    /// The button that opened it, to give it back its focus when it closes.
    /// `None` when it was opened with a pointer: a press moves no focus anywhere
    /// else in this program, so the window closes and the ring stays where it
    /// was.
    ///
    /// A window that may still be put back copies it, because it has to hand it
    /// on to the next frame; whoever gives the focus back takes it, so the modal
    /// stops pointing at a button. One closed by the thing it was asking about -
    /// a camera row that is no longer there - has nothing to give the focus back
    /// to, and keeps it until the next `open` replaces it.
    from: Option<Id>,
}

impl<T> Default for Modal<T> {
    fn default() -> Self {
        Self { payload: None, handoff: Handoff::default(), anchor: None, from: None }
    }
}

/// What a window's body decided about itself.
///
/// The two questions are separate: whether the window stays up, and whether what
/// it did changed the configuration. A third decides what happens to the focus,
/// because one of them - the question about a camera that has just been removed -
/// closes with nothing left to give it back to.
#[derive(Debug, Clone, Copy)]
struct Answer {
    /// Whether the window stays up for another frame.
    keep: bool,
    /// Whether what it did changed the configuration, which the caller is told.
    changed: bool,
    /// Whether the focus goes back to the button the window was opened from.
    restore: bool,
}

impl Answer {
    /// Stay up, having changed nothing.
    const STAY: Answer = Answer { keep: true, changed: false, restore: false };
    /// Stay up, having changed the configuration.
    const STAY_CHANGED: Answer = Answer { keep: true, changed: true, restore: false };
    /// Close, and give the focus back.
    const CLOSE: Answer = Answer { keep: false, changed: false, restore: true };
    /// Close, give the focus back, and say the configuration changed.
    const CLOSE_CHANGED: Answer = Answer { keep: false, changed: true, restore: true };
    /// Close without giving the focus back: either what it asked about is gone -
    /// the camera row that was just removed - or something else is about to take
    /// the screen, as the reboot this window asks for does.
    const CLOSE_LEAVING_THE_FOCUS: Answer = Answer { keep: false, changed: true, restore: false };
}

impl<T> Modal<T> {
    /// Whether the window is up.
    fn is_open(&self) -> bool {
        self.payload.is_some()
    }

    /// Opens the window on `payload`, replacing whatever it was showing.
    fn open(&mut self, payload: T) {
        self.payload = Some(payload);
    }

    /// Closes the window, and drops the control it had put the focus on.
    fn close(&mut self) {
        self.payload = None;
        self.anchor = None;
    }

    /// Closes the window and hands back the control the focus came from.
    ///
    /// One call rather than two, because the two have an order: the focus has to
    /// be taken before the window is closed, or the modal has already stopped
    /// remembering where it came from. See [`Modal::from`].
    fn close_restoring(&mut self) -> Option<Id> {
        self.payload = None;
        self.anchor = None;
        self.from.take()
    }

    /// Takes the payload out for the frame it is being drawn on.
    ///
    /// The window is drawn with the payload in hand and the rest of the
    /// application borrowed mutably beside it, which only works if the two are
    /// separate - so the window takes it, and puts it back with `open` unless it
    /// closed.
    fn take(&mut self) -> Option<T> {
        self.payload.take()
    }

    /// Hands the focus to the window on the frame it opens, and says whether it
    /// opened. `fresh_caret` drops the caret state of a form whose fields carry
    /// the same ids on every visit - see [`Handoff::takes_focus`].
    fn hand_off(&mut self, ctx: &egui::Context, fresh_caret: bool) -> bool {
        self.hand_off_when(ctx, self.payload.is_some(), fresh_caret)
    }

    /// [`hand_off`](Self::hand_off) for a window whose rows are drawn a frame
    /// after it opens, so the anchor does not exist on the opening frame.
    ///
    /// The hand-off happens once, on the frame `open` rises. A window opened by
    /// a control in the panel - the file picker is - puts up its first row only
    /// on the following frame, and spending that one chance on the frame it
    /// opened would leave the keyboard on the wall behind it.
    fn hand_off_when(&mut self, ctx: &egui::Context, open: bool, fresh_caret: bool) -> bool {
        self.handoff.takes_focus(ctx, open, self.anchor, fresh_caret)
    }
}

/// A camera the settings panel is editing, and the form bound to it.
#[derive(Debug)]
struct CameraEdit {
    /// Position of the entry in `AppConfig::cameras`.
    ///
    /// The slot, rather than the identifier: a camera may sit in more than one
    /// group, as an entry of its own, and entries created by an import can share
    /// an identifier. An identifier-only lookup would then find the entry in
    /// whichever group comes first in the file rather than the one the list the
    /// edit came from shows.
    slot: usize,
    draft: CameraDraft,
}

/// A camera the settings panel is asking about removing.
#[derive(Debug)]
struct RemoveConfirm {
    slot: usize,
    name: String,
}

/// What a passphrase prompt is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SealJob {
    /// Sealing the configuration for the trip: the passphrase is asked for
    /// twice, so a mistyped one cannot put the configuration out of reach.
    Export,
    /// Opening a sealed configuration: asked once, and a wrong one is tried
    /// again in place rather than closing the prompt.
    Import,
}

/// The passphrase prompt, while it is up.
///
/// One window for both directions: what an export and an import need is the
/// same entry, and the differences - a second field, a different action - are
/// small enough to read off `job` here rather than to keep two windows in step.
#[derive(Debug)]
struct Passphrase {
    job: SealJob,
    /// The passphrase, and - on an export - its confirmation.
    first: String,
    again: String,
    /// Whether the entry is shown rather than dotted, for checking a long one.
    reveal: bool,
    /// Why the last attempt did not go through, drawn in place.
    error: Option<String>,
    /// Whether the passphrase is being stretched right now, on a thread of its
    /// own. The window stays up and its buttons go away until the answer is in.
    working: bool,
    /// The sealed document an import is opening, and where it came from.
    sealed: Option<String>,
    /// The file the document came from, where the platform names one - the
    /// desktop's dialogs do, Android's picker hands over the text alone.
    origin: Option<PathBuf>,
}

impl Passphrase {
    /// The prompt an export opens on.
    fn for_export() -> Self {
        Self {
            job: SealJob::Export,
            first: String::new(),
            again: String::new(),
            reveal: false,
            error: None,
            working: false,
            sealed: None,
            origin: None,
        }
    }

    /// The prompt a sealed document opens on.
    fn for_import(sealed: String, origin: Option<PathBuf>) -> Self {
        Self {
            job: SealJob::Import,
            first: String::new(),
            again: String::new(),
            reveal: false,
            error: None,
            working: false,
            sealed: Some(sealed),
            origin,
        }
    }

    fn title(&self) -> String {
        monitor_i18n::tr(match self.job {
            SealJob::Export => "prompt-seal-title",
            SealJob::Import => "prompt-open-title",
        })
    }

    fn hint(&self) -> String {
        monitor_i18n::tr(match self.job {
            SealJob::Export => "prompt-seal-hint",
            SealJob::Import => "prompt-open-hint",
        })
    }

    fn action(&self) -> String {
        monitor_i18n::tr(match self.job {
            SealJob::Export => "prompt-seal-action",
            SealJob::Import => "prompt-open-action",
        })
    }
}

/// Where a sealed export is written once its text is in.
#[derive(Debug)]
enum ExportTarget {
    /// The file the picker chose, which every desktop writes the same way, and
    /// Android too when the viewer walks to a folder it may write.
    File(PathBuf),
    /// Android: the name to write into the public `Download/xgview` folder,
    /// through the activity. See [`XgViewApp::export_to_download`].
    #[cfg(target_os = "android")]
    Download(&'static str),
}

/// A passphrase being stretched, or a sealed document being opened, on a thread
/// of its own.
///
/// Stretching one is hundreds of thousands of rounds of arithmetic: nothing on
/// this side of a second on the desktop, and the better part of one on a
/// television - and the wall has to keep drawing while it runs, because a frame
/// that does not is an application the system is entitled to call dead. So it
/// happens away from the render loop and the answer is taken by
/// [`XgViewApp::poll_sealing`], one job at a time: the prompt that starts one
/// stays up until it has been answered.
#[derive(Debug)]
enum Sealing {
    /// A sealed document being opened for the prompt that is up for it, and the
    /// file it came from.
    Open {
        result: Receiver<Result<AppConfig, ()>>,
        origin: PathBuf,
    },
    /// The configuration being sealed for an export, which is written out as
    /// soon as the text is in.
    Export {
        result: Receiver<Result<String, String>>,
        target: ExportTarget,
    },
}

/// What the file dialog is open for, while it is.
///
/// One dialog object serves both directions, so this is how the answer it gives
/// is known to be an export - with the passphrase it is to be sealed under - or
/// an import. See [`XgViewApp::ask_for_file`].
#[derive(Debug)]
enum FileJob {
    /// Writing the configuration for another machine, sealed under this
    /// passphrase, or in clear when there is none.
    Export(Option<String>),
    /// Reading a configuration in - sealed, or in clear from an older export.
    Import,
}

/// Android: the press that is waiting on the storage the public volume needs.
///
/// Two different things are asked for, with the same shape of wait: the runtime
/// storage permission on Android 9, which is a dialog answered over the app, and
/// "all files access" from Android 11 on, which is a switch in the system
/// settings the viewer has to leave the app to flip. Neither answer is visible
/// on this side - one arrives in Java, the other in a screen that reports
/// nothing - so the press is remembered here and the question is asked again
/// every frame until it comes. See [`XgViewApp::poll_storage_pending`]. Only
/// compiled on Android, where alone the questions exist.
#[cfg(target_os = "android")]
struct PendingStorage {
    /// The press that asked, and what it does once the answer is `yes`.
    job: StorageJob,
    /// When it was asked, so one never answered can be given up on.
    asked: Instant,
    /// How many times the activity had come back when it was asked. One more
    /// than this means the viewer has been away and returned - the only signal
    /// the system-settings switch gives.
    resumes: u64,
}

/// The press, as a wait: which half of the configuration pair it was, and which
/// question it is waiting on.
#[cfg(target_os = "android")]
#[derive(Debug)]
enum StorageJob {
    /// Open the import window, on the export folder the Android 9 storage
    /// permission makes readable.
    Import,
    /// The same window, waiting instead on "all files access" - the grant that
    /// lets it list the configurations other applications put on the volume.
    ImportAllFiles,
    /// Write the configuration into `Download/xgview`, sealed under this
    /// passphrase when there is one.
    Export(Option<String>),
}

/// Translation key of what the status line calls a grid layout.
///
/// [`GridLayout::label`] is the button's own text: right on the bar, where the
/// neighbouring buttons are the context, and too terse on its own in a line of
/// status.
fn layout_hint(layout: GridLayout) -> &'static str {
    match layout {
        GridLayout::G1x1 => "toolbar-layout-1x1",
        GridLayout::G2x2 => "toolbar-layout-2x2",
        GridLayout::G3x3 => "toolbar-layout-3x3",
        GridLayout::G4x4 => "toolbar-layout-4x4",
        GridLayout::N2 => "toolbar-layout-2",
        GridLayout::N3 => "toolbar-layout-3",
        GridLayout::N5 => "toolbar-layout-5",
        GridLayout::N6 => "toolbar-layout-6",
        GridLayout::N7 => "toolbar-layout-7",
        GridLayout::N8 => "toolbar-layout-8",
        GridLayout::N10 => "toolbar-layout-10",
        GridLayout::N12 => "toolbar-layout-12",
        GridLayout::N13 => "toolbar-layout-13",
        GridLayout::P1 => "toolbar-layout-p1",
        GridLayout::P2 => "toolbar-layout-p2",
        GridLayout::P3 => "toolbar-layout-p3",
        GridLayout::P4 => "toolbar-layout-p4",
    }
}

/// The navigation scopes of the layout picker's three rows, top to bottom.
///
/// The picker lays its nine thumbnails out three to a row; each row is a scope
/// of its own, and the vertical arrows cross between them at the same column.
/// See the `Nav::new` call in [`XgViewApp::new`].
const LAYOUT_PICKER_ROWS: [&str; 3] =
    ["layout-picker-row-0", "layout-picker-row-1", "layout-picker-row-2"];

/// Keyboard / DPAD keys consumed during one frame.
#[derive(Debug, Clone, Copy, Default)]
struct Keys {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    enter: bool,
    /// Whether this frame's Enter / Space was a press of its own.
    ///
    /// [`egui::InputState::consume_key`] matches key-repeat events as well, which
    /// is what a held key sends: fine for walking a selection, wrong for opening
    /// a bar or pressing its control, which a held OK would otherwise do again
    /// and again. Read before the event is consumed.
    enter_fresh: bool,
    back: bool,
    page_next: bool,
    page_prev: bool,
    settings: bool,
    devices: bool,
    fullscreen: bool,
    layout: [bool; 10],
}

/// The viewer application.
pub struct XgViewApp {
    config: AppConfig,
    /// Last configuration written to disk, used to avoid useless writes.
    persisted: AppConfig,
    config_path: PathBuf,
    scheduler: Scheduler,
    manager: ChannelManager,
    handle: Handle,
    channels: HashMap<usize, ChannelUi>,
    /// The audio devices this machine offered when the viewer started.
    ///
    /// Read once, before the sink opens, and never again: opening a device makes
    /// it exclusive, which takes it out of the platform's own list - so a list
    /// read while the sound is playing is missing exactly the device in use, and
    /// one read after every change would move the choice under the remote. A
    /// device plugged in later is picked up by the next start.
    audio_outputs: Vec<crate::audio::AudioDevice>,
    audio_inputs: Vec<crate::audio::AudioDevice>,
    /// Whether a camera can be talked to, keyed by the address a call would be
    /// placed to.
    ///
    /// A camera's capability rather than a channel's: the picture's session
    /// carries no back channel, so the answer comes from a probe of its own and
    /// belongs to the address, not to the slot the camera is shown in.
    talk_route: HashMap<String, TalkRoute>,
    /// Decoded pictures currently on the GPU, keyed by channel index.
    textures: HashMap<usize, ChannelTexture>,
    /// The moment last reported as an arrival for a channel whose picture comes
    /// from a surface, keyed the same way. Such a session publishes its moment
    /// on every repaint without handing back a frame, and only a *change* in it
    /// is an arrival. See [`Self::upload_frames`].
    surface_moments: HashMap<usize, Option<f64>>,
    /// The camera whose picture the activity's surface is answering, and
    /// whether this camera's first frame has reached it yet.
    ///
    /// The surface keeps its last frame, so a step to another camera must not
    /// leave the wall standing aside for a frame of the camera being left.
    /// See [`Self::track_surface_picture`] and [`Self::picture_on_layer`].
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    surface_picture_owner: Option<String>,
    surface_awaiting_first: bool,
    /// Whether the wall must go on painting the picture this frame even though
    /// the surface has been asked to show it. The activity moves its view on
    /// the UI thread, a frame after the request; standing aside in that same
    /// frame leaves one frame where neither the wall nor the view has a
    /// picture. See [`Self::picture_on_layer`].
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    picture_layer_held: bool,
    /// Uploads the decoded planes and turns them into those pictures.
    video: VideoRenderer,
    /// The machine's memory, processor load and operating system, read while the
    /// About page is open; see `monitor_core::vitals`.
    vitals: monitor_core::vitals::Vitals,
    /// When this viewer started, for the About page's uptime.
    started: Instant,
    /// Measures the frame, when `XGVIEW_FRAME_MS` asks it to.
    ///
    /// `None` is the usual state: a frame that stutters is a question, and this
    /// is the answer to it, not something the viewer pays for by default. See
    /// [`crate::frame`].
    frame_clock: Option<crate::frame::FrameClock>,
    events_tx: Sender<BackgroundEvent>,
    events_rx: Receiver<BackgroundEvent>,
    /// What the file window was opened for, and `None` while it is closed.
    /// Both directions use the viewer's own picker, which is the only file
    /// window a remote can walk. See [`Self::ask_for_file`].
    file_job: Option<FileJob>,
    /// The import picker: a listing a remote control can walk. It is what an
    /// import opens everywhere, and what an export opens on a desktop - the
    /// typed name a save needs is a field the window grows at its foot. See
    /// [`file_picker::FilePicker`].
    file_picker: Modal<FilePicker>,
    /// What the picker answered, carried out of it: the window's payload is
    /// dropped as it closes, so the answer cannot travel in it.
    file_picker_picked: Option<PathBuf>,
    /// Whether the picker was backed out of rather than answered.
    file_picker_cancelled: bool,
    /// Android: the half of the pair that asked for the storage a public folder
    /// needs, and is waiting for it before it goes ahead - the import window is
    /// not put up, the export is not written. `None` when nothing is waiting.
    #[cfg(target_os = "android")]
    storage_pending: Option<PendingStorage>,
    /// Android: whether this run has already sent the viewer to the system
    /// screen that grants "all files access", which is where an import of a
    /// configuration copied on from another machine begins.
    ///
    /// Once, so that a viewer who does not want that grant can still browse -
    /// and import - what this app itself wrote, instead of being pushed into
    /// the system settings on every press. The screen is offered again after a
    /// restart.
    #[cfg(target_os = "android")]
    all_files_asked: bool,
    /// Whether the file dialog had the keyboard when this frame's keys were
    /// handed out.
    ///
    /// Not the same question as [`Self::file_dialog_open`]: the dialog closes
    /// itself inside [`Self::poll_file_dialog`], which runs before the keys are
    /// routed, so a Back that has just closed it is still in the queue when the
    /// wall reads it - and would take the settings panel down behind it as
    /// well.
    file_dialog_keys: bool,
    /// Whether this frame's poll took an answer from the file dialog.
    ///
    /// The panel is still drawn under the dialog, and a touch screen sends the
    /// press and the release in different frames: the press lands on the
    /// dialog's button, and the release arrives after [`Self::poll_file_dialog`]
    /// has closed the dialog and put the panel back under the finger. Read
    /// twice, that one press answers the dialog and presses the control behind
    /// it - which is the control the dialog was opened from. A request that
    /// arrives in the frame the answer did is the same press, and is dropped.
    file_dialog_answered: bool,
    /// The control the file dialog was opened from, to be given the focus back
    /// when it closes - the way a window's `from` does for the others.
    file_dialog_anchor: Option<Id>,
    /// Start-on-boot state as the desktop start-up section reports it; the
    /// Android section shows the system's own state instead and never reads it.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    autostart: AutostartStatus,
    /// What the platform is holding awake, and why it might be holding nothing.
    /// Kept so the panel can say whether the switches took.
    power: PowerStatus,
    /// The blank the schedule asks for, and the mark that wanders over it.
    blackout: crate::blackout::BlackoutState,
    /// Whether the wall was blank on the last frame, so that going into and out
    /// of it is acted on once rather than on every frame of it.
    blackout_on: bool,
    decoder: &'static str,
    hardware_decoder: bool,
    /// Whether hardware and software decoding can be chosen between here. Left
    /// false, the setting is shown for what it is - a setting that does not
    /// apply - rather than offered and ignored.
    decoder_selectable: bool,
    show_settings: bool,
    /// Whether the layout picker is open: the panel that offers the layouts
    /// that have no button of their own.
    layout_picker: Modal<()>,
    /// Tab the settings panel is on.
    settings_tab: SettingsTab,
    /// First control of the settings panel, so the panel can be handed the
    /// remote's focus the moment it opens.
    settings_anchor: Option<Id>,
    /// The panel and the device window, watched for the frame they appear on.
    settings_handoff: Handoff,
    /// Whether the portrait aspect picker is open: the modal that lists the
    /// cameras on the current page and lets the remote cycle each one's fit.
    portrait_aspect_picker: Modal<()>,
    /// The combined-info editor: the modal that edits one of the four slots.
    combined_info_picker: Modal<()>,
    /// Which slot (0-3) the editor is on.
    combined_info_picker_slot: usize,
    /// A scroll asked for by a direction press that had nowhere left to move the
    /// focus inside the settings body, in points.
    ///
    /// Set from `handle_keys` and spent as the body is drawn, which is the only
    /// place egui reads a scroll request from. See [`REMOTE_SCROLL_FRACTION`].
    body_scroll: Option<f32>,
    dialog_handoff: Handoff,
    discovery: DiscoveryUi,
    slide: Slide,
    needs_sync: bool,
    dirty: bool,
    dirty_since: Option<Instant>,
    /// A write that failed leaves the change in memory and holds the next
    /// automatic attempt until this moment. The state file is written on every
    /// edit, so a directory that has gone read-only would otherwise be asked
    /// once a frame; and the change has to stay dirty, because a wall is
    /// usually switched off at the plug. See [`XgViewApp::save_now`].
    save_blocked_until: Option<Instant>,
    toast: Option<Toast>,
    /// When Back was last pressed on the wall with nothing left to close. The
    /// next Back inside [`EXIT_WINDOW`] leaves the program.
    exit_armed: Option<Instant>,
    fullscreen: bool,
    /// Whether the start-up full screen still has to be applied, on the first
    /// frame where the window knows its monitor.
    ///
    /// Doing it on the viewport builder instead leaves a window-sized full
    /// screen on Windows; see `native_options` in `lib.rs`.
    fullscreen_pending: bool,
    /// Whether the window still has to be given a windowed size, having been
    /// created full screen with none asked for; see `set_fullscreen`.
    windowed_size_pending: bool,
    /// What the window's place among the others is doing, apart from the
    /// setting; see [`crate::window`].
    on_top: OnTop,
    /// A second launch asked this window to come forward, and the focus still
    /// has to be sent: it goes a frame after the restore that precedes it. See
    /// [`crate::instance`].
    come_forward: bool,
    /// Whether the top and bottom bars are on screen right now.
    chrome: Chrome,
    /// The controls of the top bar with the rectangle each was drawn in, left to
    /// right, as of the last frame the bar was on screen.
    toolbar_items: Vec<(Id, Rect)>,
    /// The page-turn buttons as of the last frame the bar was on screen, so the
    /// focus can bounce from one to the other when a turn reaches the edge and
    /// the button under the focus disappears.
    page_prev_id: Option<Id>,
    page_next_id: Option<Id>,
    /// The direction of the last page turn this frame, so the frame-end focus
    /// check can bounce to the other button when the turn reaches the edge.
    last_turn: Option<bool>,
    /// The author control of the status bar, as of the last frame that bar was
    /// on screen - the one control below the wall, which the arrow off the
    /// bottom of the wall lands on.
    status_author: Option<Id>,
    /// Horizontal centre of the focused tile, which is where the arrow that
    /// walks off the top of the wall comes back down from.
    grid_focus_x: Option<f32>,
    /// When the focus ring was last armed by an input of the viewer's, and
    /// `None` until it ever was.
    ///
    /// `None` is what keeps the ring off a wall nobody has touched yet: a fresh
    /// run shows the picture, not a cursor standing on it. See
    /// [`XgViewApp::ring_shown`].
    ring_seen: Option<Instant>,
    /// Whether this frame aimed at the wall's cursor - a key that walks it, or a
    /// click on a tile.
    ///
    /// A change of the wall's cursor - the page or the focused channel - without
    /// the viewer aiming takes that ring down, because the wall moved it under
    /// them and the ring would be standing on the wrong tile. The one case that
    /// is not that is an arrow walking off the edge of the wall, which is aiming
    /// and turns the page itself - so the cursor is compared against this. See
    /// [`XgViewApp::ring_follows_cursor`].
    ring_aimed: bool,
    /// Where the pointer went down, and when, while a press is in flight.
    ///
    /// egui forgets a press's start time on the frame it ends, so the release is
    /// read against this instead: see [`XgViewApp::tap_release`].
    tap_press: Option<(egui::Pos2, f64)>,
    /// Whether a second finger was down during the press in flight.
    ///
    /// A pinch is not a tap: the touches are gone by the frame the release is
    /// read on, so the fact has to be remembered while it is happening.
    tap_multi: bool,
    /// When the strip the last tap asked for may be put up.
    ///
    /// A tap opens the strip, and two taps in quick succession are the way back
    /// to the grid. The first of those two must not put the strip on screen on
    /// its way to the second, so the asking is held here for egui's own
    /// double-click window and dropped if the second tap arrives.
    tap_open_at: Option<Instant>,
    /// What the magnified viewport shows of its picture: the whole of it, or the
    /// window a pinch and a drag have left it at.
    ///
    /// It belongs to the viewport rather than to the channel, and it is reset
    /// wherever the viewport becomes another one - a magnify, a step to the next
    /// channel, or the way back to the grid - because a window is a part of one
    /// picture and means nothing on the next. See [`PictureZoom`].
    picture_zoom: PictureZoom,
    /// Where the magnified picture is drawn, in wall points, while a surface of
    /// its own is showing it - the rectangle the wall leaves to the picture's
    /// view.
    ///
    /// `None` whenever the wall draws the picture itself, which is every frame
    /// on a platform without that surface: there is no other view to place.
    /// Recomputed in `draw_grid` from the same answers the tile is drawn with,
    /// so that the two cannot disagree about where the picture is - it is what
    /// the fit switch changes. The corners and the bar are laid out against the
    /// tile, not against this: they belong to the wall's frame, and the wall is
    /// what reads the presses on them.
    picture_rect: Option<egui::Rect>,
    /// How far the page is from settled, in wall points - the distance the
    /// picture's view is translated by while a page slides.
    ///
    /// A `SurfaceView` is placed by a layout pass, and a page's travel changes
    /// every frame of an animation; re-laying it out per frame is both slow and
    /// a frame behind, which leaves the live picture sitting where its page used
    /// to be - beside a copy of itself. A translation is applied by the
    /// compositor with no layout at all, so the view follows the page exactly.
    /// See `picture_slide` and `android::set_video_shift`.
    picture_slide: f32,
    /// The rectangle the magnified tile is painted in, in wall points - which is
    /// the rectangle its ring goes on.
    ///
    /// Kept for the overlay, on the same terms as [`Self::picture_rect`]: the
    /// ring is drawn by the wall, and the wall is *under* the picture, so its
    /// top and bottom edges are covered by the picture's view wherever that view
    /// is narrower than the tile - which is every picture the fit switch has
    /// letterboxed, and the ring then reads as three sides and two stubs instead
    /// of a rectangle. The copy that can be seen is the overlay's. Recomputed in
    /// `draw_grid` from the same rectangle the tile is painted in, so the two
    /// cannot disagree - including while a channel step slides the tile.
    magnified_rect: Option<egui::Rect>,
    /// The band the pinned strip stands in, in wall points, as the frame drew
    /// it - or `None` where the strip has no band and floats over the picture.
    ///
    /// Kept for the overlay, which paints the band's floor and the seam above it
    /// over a picture the surface is showing. It is handed the tile the page has
    /// slid to rather than the wall, and a band measured on that tile travels
    /// with the page: the band is the wall's frame and stands still. See
    /// `split_panel` and [`Self::strip_band`].
    strip_band: Option<egui::Rect>,
    /// When the magnification that is up lets the rest of the page go, or
    /// `None` when nothing is magnified.
    ///
    /// A magnification parks the other channels, which is right for watching
    /// one camera and wrong for looking at it: the sessions of a whole page are
    /// torn down and opened again on the way in and out, and on a box that
    /// cannot afford nine of them at once that is seconds of blank tiles. So
    /// the page is held for [`paging::ZOOM_PARK_GRACE`] first, and a look and a
    /// step back costs one stream switch. See [`XgViewApp::settle_zoom_park`].
    zoom_park_at: Option<Instant>,
    /// The floating bar inside the magnified viewport.
    ///
    /// A press in the picture's top or bottom band brings it up, and it is
    /// answered by the wall rather than by egui's focus: it must not wake the
    /// top and bottom bars, which take their height from the wall. See
    /// [`crate::tilebar`].
    tile_bar: tilebar::Bar,
    /// The state the bar's sound control draws, and the switch the sink follows.
    sound_on: bool,
    /// The recording playing in the magnified viewport, when one is. See
    /// [`crate::app::playback`].
    playback: Option<playback::Playback>,
    /// A clip being saved, and the short memory of the last one.
    ///
    /// Deliberately *beside* `playback`, not inside it: leaving playback takes
    /// that whole value apart, while a download keeps running on its own task.
    /// Coming back - even on another camera - has to find the same state. See
    /// [`download::DownloadState`].
    download: DownloadState,
    /// The panel that asks how much of the recording to save. It belongs to the
    /// playback context and is closed on the way out of it; a download already
    /// confirmed does not, and keeps running on the state above.
    download_panel: Modal<DownloadChoice>,
    /// Whether the save panel was up at the end of the last frame.
    ///
    /// The recording's bar comes back on the frame that panel goes away, and on
    /// that frame only: a bar put up whenever the panel is closed would stand
    /// again the moment Back put it down, leaving the viewer unable to dismiss
    /// it at all. See [`XgViewApp::download_panel_window`].
    download_panel_was_open: bool,
    /// Whether the microphone is switched on, the bar's talk control driving it.
    ///
    /// The bar is a toggle rather than a push-to-talk button: a remote control
    /// has no way to hold one down, and one rule serves the pointer, the keys
    /// and the remote alike - see `tile_bar_activate`.
    talking: bool,
    /// The channel the microphone was switched on for. Moving the magnification
    /// somewhere else stops it, so a viewer never talks to the camera they left.
    talk_index: Option<usize>,
    /// The microphone, opened on the first talk and kept for the window's life.
    /// `None` when this machine has no input device, or the platform refused it.
    mic: Option<Arc<crate::mic::Mic>>,
    /// The audio sink, `None` when this machine has no output device. The core
    /// only ever sees it through the audio hub, as an `AudioOutput`.
    audio: Option<Arc<crate::audio::Sink>>,
    /// Who answered the last frame's direction presses.
    owner: Owner,
    /// The focus the viewer was looking at as the last frame ended.
    ///
    /// `owner` is read from this same focus, and both belong to the frame that
    /// has just been drawn: egui gives a Back/Escape press its own answer at the
    /// start of the *next* frame and drops the focus before the application sees
    /// the key. Asking the focus on the press's own frame therefore says "none"
    /// even when a ring was on the strip, and a rule that used it let the first
    /// Back out of a magnified picture that still had a ring. What the press has
    /// to answer is the state it was aimed at - the one this field kept. See
    /// [`XgViewApp::keys_off_the_grid`].
    focus_at_end: Option<Id>,
    /// Whether the recording's strip was drawn this frame.
    ///
    /// It is the one focus that the navigation layer does not own: the strip is a
    /// row rather than a control and reads its own keys, so the end of the frame
    /// has to know it has a widget before it can leave that focus alone. See the
    /// surrender in [`Self::end_frame`].
    timeline_shown: bool,
    /// Explicit directional navigation for the windows that opted into it.
    ///
    /// Keeps egui's geometric walk off the arrows of the registered controls;
    /// see [`crate::nav`]. Empty until a window registers its controls, and the
    /// wall and the bars keep egui's own walk.
    nav: Nav,
    /// The control a form handed the focus to at the end of the last frame, to
    /// be scrolled into view on this one: it had already been drawn when the
    /// focus reached it. See [`Nav::next_in_scope`].
    reveal_next: Option<Id>,
    /// Frames left in which a confirm key is discarded.
    ///
    /// The Enter or Space that opened a window is still in flight for a frame
    /// or two - the soft keyboard the window raises may carry one of its own -
    /// and it would confirm or activate the control the window has just put
    /// under the focus, before the viewer has read the form. Both keys have to
    /// be taken: egui turns either into a click on a focused widget.
    swallow_confirm: u8,
    /// A control to hand the focus back to as the next frame begins.
    ///
    /// A window gives the focus back to the button it was opened from, and
    /// that button is behind the window: asking for it on the frame the window
    /// closes would be a frame too late, once the panel has had its say. The
    /// request is kept and applied at the start of the next frame instead.
    restore_focus: Option<Id>,
    /// The control the ring is anchored to, and the channel it belongs to.
    ///
    /// A switch of the row under the ring replaces the controls the ring was
    /// on, and the id it holds names nothing once they are gone: egui drops
    /// the focus with the widget, and the keyboard falls back to the picture.
    /// The anchor is the answer - the control that undoes the switch - asked
    /// of the row as it is drawn, not of the row that was there before: a row
    /// that is a different length puts it somewhere else, and a row that is
    /// still being drawn puts it nowhere for a frame. So the ring is put on it
    /// again every frame while the anchor holds, and it cannot be lost by a
    /// row that changed under it. See
    /// [`XgViewApp::ring_follows_the_switch`] and
    /// [`XgViewApp::place_the_anchored_ring`].
    ring_anchor: Option<RingAnchor>,
    /// The channel whose recording the box has been asked for, and when.
    ///
    /// The answer is a request away, and the wait is a state of the picture:
    /// until it lands the strip carries nothing but a waiting mark, because a
    /// live row would still offer the microphone, a second way into the
    /// recording and a turn of a picture that is about to be replaced. It is
    /// also the record that decides which answers are still about what is on
    /// the screen - a second press, a viewer who has left, and a box that took
    /// too long all leave answers that name a question no longer asked. The
    /// wait has an end, so a box that never answers does not hold the strip
    /// for good. See [`XgViewApp::playback_is_asking`].
    playback_asking: Option<(usize, Instant)>,
    /// Whether a direction press asked for the ring on the pinned strip, to be
    /// put there as the next frame begins.
    ///
    /// A press that lands on nothing focused is answered by putting the ring on
    /// the strip's first control - but not on the frame the press arrives in.
    /// egui reads the arrows before any of this runs and spends them at the end
    /// of that same frame, walking the focus out of whatever holds it by
    /// geometry; a widget given the focus this frame has no lock yet - the lock
    /// takes hold only on a widget that had it last frame - and the ring was
    /// walked straight off the control it landed on. Upright, the strip's second
    /// and third lines sit under its first, so on a recording the ring left the
    /// first control for the fit button below it: a press that meant "give me
    /// the ring" left it on the fifth control, which is what a viewer saw. No
    /// direction is latched on the frame after a press, so the request waits for
    /// that one. See [`XgViewApp::claim_the_strip`].
    strip_claim: bool,
    /// Whether an import takes only the camera list, leaving the viewer's own
    /// settings - layout, discovery, reconnect - as they are. Ticked by
    /// default: taking the cameras alone is the import that cannot surprise a
    /// machine that is already installed.
    import_cameras_only: bool,
    /// Whether an export is sealed with a passphrase before it is written.
    /// Ticked by default: a configuration carries the cameras' credentials, so
    /// a copy that travels is sealed unless the viewer says otherwise.
    /// Unticking it writes the JSON in clear, for a file that is going no
    /// further than a machine the viewer trusts. Like the import box beside it,
    /// this is a preference for the session and is not saved.
    encrypt_export: bool,
    /// The passphrase prompt, while it is up: what seals an export, and what
    /// opens a sealed one. See [`Passphrase`].
    passphrase: Modal<Passphrase>,
    /// The seal or open running on a thread of its own, if any. See [`Sealing`].
    sealing: Option<Sealing>,
    /// The test tone a camera's form asked for, while it is being played.
    ///
    /// It runs on the runtime rather than in the frame: a tone is a second and a
    /// half of socket work, and the window it was asked from stays open and
    /// responsive underneath it.
    talk_test: Option<Receiver<Result<(), ToneFailure>>>,
    /// The Android start-on-boot state - whether the app may draw over other
    /// apps, and whether it is the home app - and when it was last read. Read
    /// through `crate::android`, throttled: it only changes while the viewer is
    /// away in the system settings.
    #[cfg(target_os = "android")]
    android_boot: (bool, bool),
    #[cfg(target_os = "android")]
    android_boot_at: f64,
    /// Android: the text field the viewer has confirmed for editing, which is
    /// what raises the soft keyboard.
    ///
    /// The focus alone does not raise it: on a remote, walking a form full of
    /// fields would put the keyboard up at every stop and cut the walk short.
    /// It comes up when a field is confirmed with Enter or a touch, and goes
    /// away on Back or when the focus leaves. `None` while nothing is being
    /// edited. See `handle_keys`.
    #[cfg(target_os = "android")]
    editing: Option<Id>,
    /// The camera the settings panel is editing, while its window is up.
    camera_edit: Modal<CameraEdit>,
    /// Camera waiting for the remove button to be confirmed.
    ///
    /// Asked for in a window, so that a press on a bin icon - a shape, one
    /// press away from its neighbour - cannot drop a camera on its own, and so
    /// that the question cannot be walked past and left open.
    remove_confirm: Modal<RemoveConfirm>,
    /// A system action (exit or reboot) the settings panel is asking about.
    ///
    /// Asked for in a window, so that a press on a button beside its neighbour
    /// cannot leave the program or restart the machine on its own.
    system_confirm: Modal<()>,
    /// Whether the group-name field is to be given the focus as it is drawn:
    /// set when a group has just been added, so its name is typed over at once.
    focus_group_name: bool,
    /// The group whose removal is being confirmed, by index.
    group_remove: Option<usize>,
    /// The camera order being arranged while the Cameras tab is in its reorder
    /// mode.
    ///
    /// `None` is the ordinary list, drawn from the configuration. `Some` holds
    /// the identifiers in the order the viewer has put them in; the
    /// configuration is not written until the order is confirmed, so the wall
    /// and the channels are undisturbed while the list is rearranged.
    reorder: Option<Vec<String>>,
    /// The button the reorder list opens on - the down arrow of its first row,
    /// the only one of the pair with somewhere to go - to hand it the focus once
    /// the list has been drawn. The press that opens the mode happens a frame
    /// before there is anything to focus.
    reorder_anchor: Option<Id>,
    /// The button that opens the reorder mode, to hand the focus back to as it
    /// closes. Given a fixed [`Id`], because the control it belongs to only
    /// exists on the frames the ordinary list is drawn.
    reorder_trigger_anchor: Option<Id>,
    /// Set as the mode opens, cleared once its first move button has the focus.
    reorder_focus: bool,
    /// What each control is called, for the status line.
    ///
    /// The bar is made of shapes, and a remote control has no pointer to hover
    /// for a tooltip, so the status line is where a viewer reads what the shape
    /// under the focus does. Rebuilt every frame: a name is only worth showing
    /// while the control it belongs to is on screen.
    focus_names: HashMap<Id, &'static str>,
    /// Name of the focused control, resolved one frame behind.
    ///
    /// The status bar is drawn before the panels that fill the names in, so this
    /// is read at the end of a frame and shown at the start of the next.
    focused_name: Option<&'static str>,
    from_autostart: bool,
    /// Which way the wall stands. A device that cannot stand upright is kept
    /// landscape whatever the configuration asks for.
    orientation: Orientation,
    /// Whether this device can stand upright at all: a touch screen that is not
    /// a television. When false the orientation button is never drawn and the
    /// wall stays landscape.
    supports_portrait: bool,
    /// A supervisor is watching this process, so Back must not end the program:
    /// the wall is meant to stay up, and the only way out is the exit button.
    supervised: bool,
    time: f64,
}

mod bars;
mod cameras;
mod chrome;
mod download;
mod file_picker;
mod input;
mod paging;
mod persist;
#[cfg(target_os = "android")]
pub use persist::public_download_dir;
pub use persist::EXPORT_SUBFOLDER;
mod playback;
use playback::Source;
use download::{DownloadChoice, DownloadState};
mod pickers;
mod settings;
mod streams;
mod wall;
mod windows;

impl XgViewApp {
    /// Draws one window: takes its payload, hands it to `body`, and puts it back
    /// or closes it on the answer.
    ///
    /// A modal rather than a panel, for all seven of them: centred, and its
    /// backdrop swallows the presses that would otherwise reach the wall behind
    /// it. A window is left only by its own buttons, or by Back.
    ///
    /// `body` is handed the application too, because most of these windows do
    /// more than draw - one saves a camera, one starts a reboot, one leaves a
    /// passphrase stretching on a thread. What they all do *around* that is the
    /// same, and it is here: take the payload, draw it in a modal of the given
    /// width, then put it back or close it and give the focus back.
    /// The life every window shares: take its payload, draw it once, then put it
    /// back, close it, or close it and hand the focus back.
    ///
    /// How a window is *drawn* is not one of those things. Nearly all of them
    /// are an `egui::Modal`; the save panel over a picture turned a quarter turn
    /// is painted by hand, because egui has no rotated layer. What both need
    /// around the drawing is exactly this, and a window that took its payload
    /// twice would flicker. See [`Self::modal_frame`] and `download`'s turned
    /// panel.
    fn modal_lifecycle<T>(
        &mut self,
        slot: impl Fn(&mut Self) -> &mut Modal<T>,
        draw: impl FnOnce(&mut Self, &mut T) -> Answer,
    ) -> Answer {
        let Some(mut payload) = slot(self).take() else {
            return Answer::STAY;
        };
        let answer = draw(self, &mut payload);
        if answer.keep {
            slot(self).open(payload);
        } else if answer.restore {
            let from = slot(self).close_restoring();
            self.restore_focus = from;
        } else {
            slot(self).close();
        }
        answer
    }

    fn modal_frame<T>(
        &mut self,
        ctx: &egui::Context,
        id: &str,
        width: f32,
        slot: impl Fn(&mut Self) -> &mut Modal<T>,
        body: impl FnOnce(&mut Self, &mut egui::Ui, &mut T) -> Answer,
    ) -> Answer {
        self.modal_lifecycle(slot, |app, payload| {
            let mut answer = Answer::STAY;
            egui::Modal::new(Id::new(id)).show(ctx, |ui| {
                ui.set_width(width);
                answer = body(app, ui, payload);
            });
            answer
        })
    }

    /// Builds the wall.
    ///
    /// It takes the two things it needs from a renderer rather than eframe's
    /// `CreationContext`: the egui context the interface is drawn in, and the
    /// wgpu state the pictures are uploaded through. eframe hands both over on
    /// the desktop; on Android the surface comes from a `SurfaceView` and this
    /// is the whole of what that path has to reproduce. `eframe::Frame` is not
    /// used at all.
    pub fn new(
        egui_ctx: &egui::Context,
        render_state: &egui_wgpu::RenderState,
        options: RunOptions,
        config_path: PathBuf,
        handle: Handle,
    ) -> Self {
        let RunOptions { config, fullscreen, extra_lang_dir, .. } = options;
        let mut config = config;

        // Where this flag comes from is per platform, and reading it is what
        // decides between the two start-up toasts. On the desktop it is the
        // command line, and the value in `RunOptions` is the one the start-on-
        // boot registration put there. On Android there is no command line: the
        // launch intent says whether the boot receiver started us, so the
        // activity is asked once it is up, and the value in `RunOptions` is a
        // placeholder that is not read at all.
        #[cfg(target_os = "android")]
        let from_autostart = crate::android::from_autostart();
        #[cfg(not(target_os = "android"))]
        let from_autostart = options.from_autostart;

        // A supervisor's child reads its mark from the environment, which the
        // entry point has already turned into this flag. Android supervises
        // itself and passes `false`.
        let supervised = options.supervised;

        // The interface language is chosen before the first frame is drawn:
        // English is embedded, and a pack beside the executable or under the
        // configuration directory is what the setting can switch to.
        let mut language_dirs = vec![monitor_core::config::config_dir().join("langs")];
        if let Some(dir) = extra_lang_dir.as_ref() {
            language_dirs.push(dir.clone());
        }
        monitor_i18n::init(&config.language, &language_dirs);

        theme::install(egui_ctx);
        fonts::install_for(egui_ctx, &monitor_i18n::current_language());

        let autostart = autostart::status();
        if autostart.supported {
            // The registry is the source of truth for the start-on-boot state.
            config.autostart = autostart.enabled;
        }
        // Android's two halves are Java's, and the bridge to the activity lives
        // in this crate rather than in `monitor_core`: hand it over before
        // anything asks whether the platform can hold anything at all.
        #[cfg(target_os = "android")]
        power::install_android(crate::android::set_keep_awake);
        // Before the first frame, and from this thread: a Windows execution
        // state is per-thread, and this is the thread that stays alive. A
        // platform with no mechanism is not an error to report here - the
        // panel greys the switches out and says so.
        if power::is_supported() {
            if let Err(err) = power::apply(config.prevent_sleep, config.keep_screen_on) {
                tracing::warn!(target: "xgview::power", %err, "the machine cannot be kept awake");
            }
        }
        let power = power::status();
        // Said out loud once, because a machine that no longer sleeps has
        // nothing else pointing at the program that asked it not to.
        tracing::info!(
            target: "xgview::power",
            supported = power.supported,
            prevent_sleep = config.prevent_sleep,
            keep_screen_on = config.keep_screen_on,
            mechanism = power.mechanism,
            applied = ?power.applied,
            "power management at start-up"
        );
        let discovery = DiscoveryUi::new(&config.discovery);
        #[cfg(target_os = "android")]
        let supports_portrait = crate::android::supports_portrait();
        #[cfg(not(target_os = "android"))]
        let supports_portrait = false;
        // A remembered portrait is honoured only where the device can stand
        // upright; anywhere else the wall starts landscape and stays there.
        let orientation = if supports_portrait {
            config.android_orientation
        } else {
            Orientation::Landscape
        };
        let (start_layout, start_page, start_focus) = config.group().view(orientation.is_portrait());
        let scheduler =
            Scheduler::new(start_layout, start_page, start_focus, config.enabled_count());
        let manager = ChannelManager::spawn(
            &handle,
            config.reconnect.clone(),
            Duration::from_millis(config.handshake_timeout_ms),
            config.prefer_hardware_decode,
        );
        // The devices are listed before anything opens one: a device that is
        // open is exclusive, and the platform stops listing it - which would
        // leave the settings panel offering everything except the one it is
        // playing through.
        let audio_outputs = crate::audio::output_devices();
        let audio_inputs = crate::mic::input_devices();
        // The audio sink, opened once. A machine without an output device leaves
        // `None` here, and the sound switch has nothing to drive - not a failure:
        // the picture carries on. The device is the one the configuration names,
        // or the system's default when it names none.
        let audio = crate::audio::Sink::open(config.audio_output.as_deref());
        if let Some(sink) = &audio {
            sink.set_volume(config.volume);
            manager.audio().set_output(Some(sink.clone()));
        }
        let sound_on = config.sound_on;
        // A process killed mid-download cannot clean up after itself. Nothing
        // but this start can tell an orphan `.part` from one still being
        // written, and nothing ever reads a `.part` as a video, so old ones go.
        download::cleanup_on_start();
        let (events_tx, events_rx) = unbounded();
        let capabilities = monitor_codec::capabilities();

        // The state the plane uploads go through: eframe's on the desktop, the
        // one built beside the `SurfaceView`'s surface on Android.
        // The Windows selector falls back to WARP rather than leaving the
        // machine unable to draw (see `select_adapter`), and the wall says so:
        // a frame rate that low would otherwise be taken for the cameras'.
        #[cfg(windows)]
        let software_renderer = render_state.adapter.get_info().device_type == wgpu::DeviceType::Cpu;
        let video = VideoRenderer::new(render_state);

        let mut scopes: Vec<(&'static str, ScopeDef)> = vec![
            ("dialog-tabs", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope("dialog-body"))),
            ("dialog-body", ScopeDef::new(Axis::Column).scrolling().exit(Dir::Up, Exit::Scope("dialog-tabs"))),
            ("settings-tabs", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope("settings-body"))),
            ("settings-body", ScopeDef::new(Axis::Column).scrolling().exit(Dir::Up, Exit::Scope("settings-tabs"))),
            ("toolbar", ScopeDef::new(Axis::Row)),
            // The layout picker is a 3x3 grid of thumbnails: each row is a
            // scope of its own, and Down / Up cross between them at the same
            // column. The sideways arrows walk a row, so a press at either
            // end is spent rather than wrapping.
            ("layout-picker-row-0", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::ScopeAt("layout-picker-row-1"))),
            ("layout-picker-row-1", ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::ScopeAt("layout-picker-row-0")).exit(Dir::Down, Exit::ScopeAt("layout-picker-row-2"))),
            ("layout-picker-row-2", ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::ScopeAt("layout-picker-row-1"))),
            ("status", ScopeDef::new(Axis::Row)),
            ("edit-body", ScopeDef::new(Axis::Column)),
            ("confirm-body", ScopeDef::new(Axis::Column)),
            ("passphrase-body", ScopeDef::new(Axis::Column)),
            ("juanz-row", ScopeDef::new(Axis::Row)),
            ("aspect-picker", ScopeDef::new(Axis::Column)),
            // The magnified viewport's bar: one row, whichever way the picture
            // is drawn. A turned strip runs down the screen but is still walked
            // left-to-right, so there is no second scope for it. No exits - a
            // strip has nothing beyond its ends to walk to, and Back is the way
            // out.
            ("tile-bar-row", ScopeDef::new(Axis::Row)),
            // The save panel: chosen lengths and the one button under them. Its
            // scopes are declared with the panel that walks them; see
            // `download::scopes`.
        ];
        // The picker's own scopes are declared with the window that walks them;
        // see `file_picker::scopes`. The save panel's, likewise, beside the
        // panel: see `download::scopes`.
        scopes.extend(file_picker::scopes());
        scopes.extend(download::scopes());

        let mut app = Self {
            persisted: config.clone(),
            config,
            config_path,
            scheduler,
            manager,
            handle,
            channels: HashMap::new(),
            audio_outputs,
            audio_inputs,
            talk_route: HashMap::new(),
            textures: HashMap::new(),
            surface_moments: HashMap::new(),
            surface_picture_owner: None,
            surface_awaiting_first: false,
            picture_layer_held: false,
            video,
            vitals: monitor_core::vitals::Vitals::new(),
            started: Instant::now(),
            frame_clock: crate::frame::FrameClock::from_env(),
            events_tx,
            events_rx,
            file_job: None,
            file_picker: Modal::default(),
            file_picker_picked: None,
            file_picker_cancelled: false,
            #[cfg(target_os = "android")]
            storage_pending: None,
            #[cfg(target_os = "android")]
            all_files_asked: false,
            file_dialog_keys: false,
            file_dialog_answered: false,
            file_dialog_anchor: None,
            autostart,
            power,
            decoder: capabilities.backend,
            hardware_decoder: capabilities.hardware,
            decoder_selectable: capabilities.selectable,
            show_settings: false,
            layout_picker: Modal::default(),
            settings_tab: SettingsTab::default(),
            settings_anchor: None,
            settings_handoff: Handoff::default(),
            portrait_aspect_picker: Modal::default(),
            combined_info_picker: Modal::default(),
            combined_info_picker_slot: 0,
            body_scroll: None,
            dialog_handoff: Handoff::default(),
            discovery,
            blackout: crate::blackout::BlackoutState::new(),
            blackout_on: false,
            slide: Slide::default(),
            needs_sync: true,
            dirty: false,
            dirty_since: None,
            save_blocked_until: None,
            toast: None,
            exit_armed: None,
            fullscreen,
            fullscreen_pending: fullscreen,
            windowed_size_pending: fullscreen && crate::FULLSCREEN_ON_THE_BUILDER,
            on_top: OnTop::default(),
            come_forward: false,
            chrome: Chrome {
                visible: true,
                last_input: Instant::now(),
                hide_now: false,
                input_seen: false,
                bars: None,
                consume_primary: false,
                reveal_back: false,
                clean_held: false,
                reveal_double_click_until: f64::NEG_INFINITY,
            },
            toolbar_items: Vec::new(),
            page_prev_id: None,
            page_next_id: None,
            last_turn: None,
            status_author: None,
            grid_focus_x: None,
            ring_seen: None,
            ring_aimed: false,
            tap_press: None,
            tap_multi: false,
            tap_open_at: None,
            picture_zoom: PictureZoom::FULL,
            picture_rect: None,
            picture_slide: 0.0,
            magnified_rect: None,
            strip_band: None,
            zoom_park_at: None,
            tile_bar: tilebar::Bar::new(),
            sound_on,
            playback: None,
            download: DownloadState::default(),
            download_panel: Modal::default(),
            download_panel_was_open: false,
            talking: false,
            talk_index: None,
            mic: None,
            audio,
            // The "Add devices" window, the settings panel and the top bar are
            // on this layer: each is a row (of tabs, or of controls) over a
            // column, which is the shape egui's walk handles worst. The wall
            // keeps its own navigation. The two bodies are the scopes that
            // scroll - a tab strip is always in view, and a scope that asked to
            // be revealed from outside a scroll area would leave the request
            // for the next one in the pass to consume.
            nav: Nav::new(&scopes),
            reveal_next: None,
            swallow_confirm: 0,
            restore_focus: None,
            ring_anchor: None,
            playback_asking: None,
            strip_claim: false,
            import_cameras_only: true,
            encrypt_export: true,
            passphrase: Modal::default(),
            sealing: None,
            talk_test: None,
            #[cfg(target_os = "android")]
            android_boot: (false, false),
            #[cfg(target_os = "android")]
            android_boot_at: f64::NEG_INFINITY,
            #[cfg(target_os = "android")]
            editing: None,
            camera_edit: Modal::default(),
            remove_confirm: Modal::default(),
            system_confirm: Modal::default(),
            focus_group_name: false,
            group_remove: None,
            reorder: None,
            reorder_anchor: None,
            reorder_trigger_anchor: None,
            reorder_focus: false,
            owner: Owner::Grid,
            focus_at_end: None,
            timeline_shown: false,
            focus_names: HashMap::new(),
            focused_name: None,
            from_autostart,
            orientation,
            supports_portrait,
            supervised,
            time: 0.0,
        };
        app.sync();
        let message = match (app.from_autostart, app.supervised) {
            (true, true) => monitor_i18n::tr_args(
                "toast-started-autostart-supervised",
                &[("version", env!("CARGO_PKG_VERSION").into()), ("decoder", app.decoder.into())],
            ),
            (true, false) => monitor_i18n::tr_args(
                "toast-started-autostart",
                &[("version", env!("CARGO_PKG_VERSION").into()), ("decoder", app.decoder.into())],
            ),
            (false, true) => monitor_i18n::tr_args(
                "toast-started-supervised",
                &[("version", env!("CARGO_PKG_VERSION").into()), ("decoder", app.decoder.into())],
            ),
            (false, false) => monitor_i18n::tr_args(
                "toast-started",
                &[("version", env!("CARGO_PKG_VERSION").into()), ("decoder", app.decoder.into())],
            ),
        };
        app.flash(message, ToastKind::Info);
        #[cfg(windows)]
        if software_renderer {
            app.flash(monitor_i18n::tr("toast-software-renderer"), ToastKind::Error);
        }
        app
    }

    // ---------------------------------------------------------------- helpers

    /// The control an open panel falls back to when nothing has the focus.
    ///
    /// The windows come first - only one of them is ever up - then the reorder
    /// list, then the settings panel; `None` when nothing is open, where the
    /// wall answers for itself. Read at the end of a frame to keep a panel from
    /// being left without a focus, and at the start of one to give the focus
    /// back the moment a key says the viewer is on the keyboard again.
    fn panel_anchor(&self) -> Option<Id> {
        if self.passphrase.is_open() {
            self.passphrase.anchor
        } else if self.remove_confirm.is_open() {
            self.remove_confirm.anchor
        } else if self.system_confirm.is_open() {
            self.system_confirm.anchor
        } else if self.camera_edit.is_open() {
            self.camera_edit.anchor
        } else if self.discovery.open {
            self.discovery.focus_anchor
        } else if self.layout_picker.is_open() {
            self.layout_picker.anchor
        } else if self.portrait_aspect_picker.is_open() {
            self.portrait_aspect_picker.anchor
        } else if self.combined_info_picker.is_open() {
            self.combined_info_picker.anchor
        } else if self.download_panel.is_open() {
            // The save panel is opened from the recording's bar, and the bar is
            // still on screen behind it - `tile_bar.is_open()` is true the whole
            // time. The rule at the end of the frame that puts a stray focus
            // back on the bar's seat does so only while this answers `None`, so
            // leaving the panel out of here took the focus out of it on every
            // frame: the arrows walked nothing, and Enter pressed the control
            // the ring was on behind the window, which reopened the panel and
            // threw away its anchor. See `Handoff`.
            self.download_panel.anchor
        } else if self.reorder.is_some() {
            self.reorder_anchor
        } else if self.show_settings {
            self.settings_anchor
        } else {
            None
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.dirty_since = Some(Instant::now());
    }

    /// Says what a control is called, for the status line. See [`Self::focus_names`].
    fn name(&mut self, response: &egui::Response, name: &'static str) {
        self.focus_names.insert(response.id, name);
    }

    /// Whether something is floating over the picture rather than beside it.
    ///
    /// The surface path cares about the difference. A panel takes layout space,
    /// so the wall shrinks and the picture shrinks with it, and the panel is on
    /// screen whatever the picture is doing. A window or a modal takes none: it
    /// is drawn over the picture's rectangle, and the picture is on a surface
    /// above the wall, so it is covered whole - not drawn where nobody can see
    /// it, but unpressable, because the presses go to the picture's view.
    /// While one of those is up the surface path is suspended, the codec hands
    /// its frames back, and the wall draws everything.
    ///
    /// The settings panel is *not* in this list, on purpose. It is a
    /// `SidePanel` and already moves the picture out of its way, and suspending
    /// costs a codec reopen at each end - a price worth paying for a dialog
    /// that covers the picture, not for one that sits beside it.
    pub fn floating_layer_open(&self, ctx: &egui::Context) -> bool {
        if self.discovery.open
            || self.layout_picker.is_open()
            || self.portrait_aspect_picker.is_open()
            || self.combined_info_picker.is_open()
            || self.passphrase.is_open()
            || self.camera_edit.is_open()
            || self.remove_confirm.is_open()
            || self.system_confirm.is_open()
        {
            return true;
        }
        // The soft keyboard's box is pinned to the top of the screen rather
        // than following its field, so a field in a panel can still have its
        // box over the picture. Only while it is actually up: `editing` is the
        // field the viewer confirmed for editing, and the mirror is what the
        // box is drawn from. See `input_overlay`.
        #[cfg(target_os = "android")]
        {
            if self.editing.is_some()
                && ctx
                    .data(|data| data.get_temp::<dialogs::Mirror>(Id::new(dialogs::MIRROR)))
                    .is_some_and(|mirror| self.editing == Some(mirror.id))
            {
                return true;
            }
        }
        #[cfg(not(target_os = "android"))]
        let _ = ctx;
        false
    }

    /// Uploads the pictures decoded since the last repaint.
    ///
    /// Only the newest frame of a channel is uploaded: the [`FrameStore`]
    /// already dropped the ones the repaint missed, so a slow frame costs
    /// sharpness, never latency. The upload happens on the UI thread because
    /// that is the only thread owning the graphics context, but it is one write
    /// per plane per channel, and the conversion passes of the whole grid are
    /// submitted together.
    ///
    /// [`FrameStore`]: monitor_core::pipeline::FrameStore
    fn upload_frames(&mut self) {
        for index in self.channels.keys().copied().collect::<Vec<_>>() {
            // A slot whose picture is coming from somewhere else - a recording
            // playing over the camera - is decoded but never drawn, and a
            // texture of it would be three megabytes a frame uploaded for
            // nobody. Its last one stays where it is, which is what the
            // viewport falls back to when the recording has nothing yet.
            if self.view_index(index) != index {
                self.surface_moments.remove(&index);
                continue;
            }
            let Some(frame) = self.manager.frames().latest(index) else {
                // No picture to upload, and a moment all the same: a session
                // that renders into a surface publishes what its picture is of
                // and hands nothing back. A seek waiting for it still ends on
                // it - see `Playback::pending`.
                let arrived = self.manager.frames().moment(index);
                // That moment is published again on *every* repaint, whether
                // the picture moved or not. Reporting each one re-runs the
                // arrival test against the frame the seek left: while steps
                // come faster than pictures, every new `pending` is answered
                // by that same old frame, the cursor goes back to where the
                // step began, and the strip jitters between the two. One
                // report per moment is enough - the next moment is the picture
                // that actually arrived.
                if self.surface_moments.get(&index) != Some(&arrived) {
                    self.surface_moments.insert(index, arrived);
                    self.picture_arrived(index, arrived);
                }
                continue;
            };
            let current = self.textures.get(&index).map(|entry| entry.sequence);
            if current == Some(frame.sequence) {
                continue;
            }
            self.video.upload(index, &frame);
            let Some(surface) = self.video.surface(index) else {
                continue;
            };
            self.textures.insert(
                index,
                ChannelTexture { sequence: frame.sequence, at: frame.at, surface },
            );
            // The moment the picture is of is what says where it has got to,
            // and it is what ends a seek's wait for the cursor to be let go of.
            // Read from the store rather than from the frame, because a picture
            // that goes to a surface has a moment and no frame at all. See
            // `Playback::pending`.
            let arrived = self.manager.frames().moment(index);
            self.surface_moments.insert(index, arrived);
            self.picture_arrived(index, arrived);
        }
        // A recording plays on a channel of its own, past the wall's slots, so
        // the loop above never reaches it: it is not one of `self.channels` and
        // its slot is not its index. It hands nothing back either - the codec
        // draws into its own surface - so the moment it publishes is the only
        // word on where the picture has got to. Without this the seek that
        // opened the recording never cleared, and the cursor and the clock
        // stayed on the moment it opened for as long as it played.
        let recording = self.playback.as_ref().map(|playback| playback.index);
        if let Some(index) = recording {
            let arrived = self.manager.frames().moment(index);
            if self.surface_moments.get(&index) != Some(&arrived) {
                self.surface_moments.insert(index, arrived);
                self.picture_arrived(index, arrived);
            }
        }
        self.video.flush();
    }

    /// Opens the file dialog for what the viewer pressed.
    ///
    /// The dialog is [`crate::app`]'s own: egui draws it inside this window, so
    /// there is no window of the desktop's to come up in front of the wall, no
    /// session for it to be started in, and nothing it can be left waiting on.
    /// What it answers is picked up by [`Self::poll_file_dialog`], which draws
    /// it while it is up.
    #[cfg(not(target_os = "android"))]
    fn ask_for_file(&mut self, job: FileJob) {
        // The press that answered the dialog, read again by the control it was
        // opened from. See [`Self::file_dialog_answered`].
        if self.file_dialog_answered {
            return;
        }
        match job {
            // Both directions use the viewer's own window. The native dialogs
            // it replaces are the two screens a remote could not reach at all,
            // and one window means one set of keys to learn. The name it starts
            // with says which of the two files it is: a sealed export carries a
            // `.seal` and one written in clear a `.json`, and the listing takes
            // both.
            FileJob::Export(passphrase) => self.export_with_picker(passphrase),
            FileJob::Import => self.import_with_picker(),
        }
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        for event in self.manager.poll() {
            // A camera's answer, not a channel's: it is kept by address so that
            // a camera shown in two slots, or magnified twice, is asked once.
            if let StreamEvent::Talk { uri, route, .. } = &event {
                self.talk_route.insert(uri.clone(), *route);
                continue;
            }
            // A call that did not come up ends the call the interface thought it
            // had started: the bar goes back and the reason is said once.
            if let StreamEvent::CallFailed { reason, .. } = &event {
                let key = match reason {
                    CallFailure::NoBackChannel => "toast-talk-unavailable",
                    CallFailure::Unreachable => "toast-talk-failed",
                };
                self.flash(monitor_i18n::tr(key), ToastKind::Error);
                self.set_talking(false);
                continue;
            }
            let index = event.index();
            // A recording that reached a hole ends its session, and the picture
            // carries on at the next stretch: the box stops the playlist there
            // rather than serving what is past it. See
            // [`XgViewApp::recording_stopped`].
            //
            // A session that ends *with a reason* is the other kind of stop: the
            // box answered "nothing recorded at that moment", and no picture was
            // ever shown. The pipeline reports it as a finish so that a hole is
            // not drawn as a broken camera (see `pipeline::channel`), and the
            // reason is the only thing that tells the two apart - a clean finish
            // carries none.
            let (stopped, nothing_there) = match &event {
                StreamEvent::State { state: ConnectionState::Finished, detail, .. } => {
                    (true, !detail.is_empty())
                }
                _ => (false, false),
            };
            self.channels.entry(index).or_default().apply(event);
            if stopped {
                self.recording_stopped(index, nothing_there);
            }
        }

        while let Ok(event) = self.events_rx.try_recv() {
            let mut touched_config = false;
            let mut toast: Option<(String, ToastKind)> = None;
            match &event {
                BackgroundEvent::Imported(camera) => {
                    self.config.upsert_camera((**camera).clone());
                    touched_config = true;
                    toast = Some((
                        monitor_i18n::tr_args("toast-camera-added", &[("name", camera.name.clone().into())]),
                        ToastKind::Info,
                    ));
                }
                BackgroundEvent::ImportFailed { name, error } => {
                    toast = Some((
                        monitor_i18n::tr_args(
                            "toast-camera-import-failed",
                            &[("name", name.clone().into()), ("error", error.to_string().into())],
                        ),
                        ToastKind::Error,
                    ));
                }
                BackgroundEvent::SynologyDone(cameras) => {
                    // The cameras are listed in the dialog for the viewer to
                    // pick from, not imported on arrival - see `synology_tab`.
                    toast = Some((
                        monitor_i18n::tr_args(
                            "toast-synology-found",
                            &[("count", cameras.len().into())],
                        ),
                        ToastKind::Info,
                    ));
                }
                BackgroundEvent::SynologyFailed(error) => {
                    toast = Some((
                        monitor_i18n::tr_args("toast-synology-failed", &[("error", error.to_string().into())]),
                        ToastKind::Error,
                    ));
                }
                BackgroundEvent::FrigateDone(import) => {
                    // Listed in the dialog for the viewer to pick from, not
                    // imported on arrival - see `frigate_tab`.
                    toast = Some((
                        monitor_i18n::tr_args(
                            "toast-frigate-found",
                            &[("count", (import.cameras.len() + import.leftovers.len()).into())],
                        ),
                        ToastKind::Info,
                    ));
                }
                BackgroundEvent::FrigateFailed(error) => {
                    toast = Some((
                        monitor_i18n::tr_args("toast-frigate-failed", &[("error", error.to_string().into())]),
                        ToastKind::Error,
                    ));
                }
                BackgroundEvent::Go2rtcDone(import) => {
                    toast = Some((
                        monitor_i18n::tr_args("toast-go2rtc-found", &[("count", import.cameras.len().into())]),
                        ToastKind::Info,
                    ));
                }
                BackgroundEvent::PlaybackReady { channel, camera_id, origin, credentials, window, begin, playlist, segments, read } => {
                    // Only the answer to the question this picture is still
                    // waiting for belongs to it. A second press, an answer that
                    // arrived after the viewer left the picture, and one that
                    // came after the wait was given up are answers to a
                    // question that is no longer on the screen; the strip has
                    // gone back to its controls and an answer landing now would
                    // take the picture away from whoever is watching it.
                    // `playback_is_asking` owns the wait's clock and the
                    // picture it belongs to; the channel is the last half of
                    // "this answer is for that question".
                    let waiting = self.playback_is_asking()
                        && self.playback_asking.is_some_and(|(asked, _)| asked == *channel);
                    if waiting {
                        self.playback_asking = None;
                        match window {
                            Some(window) => self.start_playback(
                                ctx,
                                *channel,
                                Source {
                                    camera: camera_id.clone(),
                                    origin: origin.clone(),
                                    credentials: credentials.clone(),
                                },
                                *window,
                                *begin,
                                playlist.clone(),
                                segments.clone(),
                                *read,
                            ),
                            // The camera is there and its server answered; there is
                            // simply nothing recorded to play. Said once, and the
                            // viewport stays where it was. The bar's clock is in
                            // charge again - the question has been answered, and the
                            // answer was that there is nothing to see. The anchor
                            // goes with it: the row it named is the row that is
                            // being drawn, and it carries no way back into a
                            // recording that is not there.
                            None => {
                                self.tile_bar.release(Instant::now());
                                self.ring_anchor = None;
                                // No recording to open, so no ring to carry
                                // into one: the seat the wait held is the
                                // picture's to have back, the same as when an
                                // answer opens a recording without a ring.
                                self.release_the_waiting_seat(ctx);
                                self.flash(
                                    monitor_i18n::tr("toast-no-recording"),
                                    ToastKind::Error,
                                )
                            }
                        }
                    } else {
                        // The answer came, so that question is over even when
                        // the viewer has left the picture it was asked of:
                        // otherwise stepping back to the same channel would
                        // find a strip still waiting for an answer it has
                        // already had. An answer for another channel is not
                        // this question's, and leaves the newer one alone.
                        if self.playback_asking.is_some_and(|(asked, _)| asked == *channel) {
                            self.playback_asking = None;
                        }
                        tracing::debug!(
                            target: "xgview::app",
                            channel,
                            "a recording answered a question no longer asked"
                        );
                    }
                }
                BackgroundEvent::PlaylistReady { index, camera_id, window, at, playlist, gen } => {
                    match *gen {
                        // Read ahead of the finger: kept until a seek wants it.
                        None => {
                            self.range_arrived(*index, camera_id.clone(), *window, playlist.clone());
                        }
                        // A seek's own answer, and only the newest seek's is
                        // played: an older one is a moment already left behind.
                        Some(gen) if self.seek_is_current(gen) => {
                            self.start_seek(*index, camera_id.clone(), *window, *at, playlist.clone());
                        }
                        Some(_) => {}
                    }
                }
                BackgroundEvent::EventsReady { index, camera_id, from, to, events } => {
                    self.events_arrived(*index, camera_id.clone(), *from, *to, events.clone());
                }
                BackgroundEvent::DaysReady { index, camera_id, days } => {
                    self.days_arrived(*index, camera_id.clone(), days.clone());
                }
                BackgroundEvent::SegmentsReady { index, camera_id, from, to, segments } => {
                    self.segments_arrived(
                        *index,
                        camera_id.clone(),
                        *from,
                        *to,
                        segments.clone(),
                    );
                }
                BackgroundEvent::Go2rtcFailed(error) => {
                    toast = Some((
                        monitor_i18n::tr_args("toast-go2rtc-failed", &[("error", error.to_string().into())]),
                        ToastKind::Error,
                    ));
                }
                BackgroundEvent::DiscoveryDone(report) => {
                    // The devices are listed in the dialog for the viewer to
                    // pick from, not imported on arrival - see `onvif_tab`.
                    toast = Some((
                        monitor_i18n::tr_args(
                            "toast-discovery-found",
                            &[
                                ("devices", report.devices.len().into()),
                                ("ports", report.port_hits.len().into()),
                            ],
                        ),
                        ToastKind::Info,
                    ));
                }
                BackgroundEvent::DiscoveryFailed(error) => {
                    toast = Some((
                        monitor_i18n::tr_args("toast-discovery-failed", &[("error", error.to_string().into())]),
                        ToastKind::Error,
                    ));
                }
                BackgroundEvent::DownloadProgress { id, progress } => {
                    // Drained on every frame, not only while playback is open:
                    // the clip keeps arriving after the viewer leaves it.
                    self.download.note_progress(*id, *progress);
                }
                BackgroundEvent::DownloadDone { id, path } => {
                    if self.download.finish_done(*id, path.clone()) {
                        tracing::info!(target: "xgview::app", path = %path, "the clip was saved");
                        toast = Some((
                            monitor_i18n::tr_args("toast-download-done", &[("path", path.clone().into())]),
                            ToastKind::Info,
                        ));
                    }
                }
                BackgroundEvent::DownloadFailed { id, reason } => {
                    if self.download.finish_failed(*id, reason.clone()) {
                        tracing::warn!(target: "xgview::app", reason = %reason, "the clip could not be saved");
                        toast = Some((
                            monitor_i18n::tr_args(
                                "toast-download-failed",
                                &[("error", reason.clone().into())],
                            ),
                            ToastKind::Error,
                        ));
                    }
                }
                _ => {}
            }
            self.discovery.apply(event);
            if touched_config {
                self.needs_sync = true;
                self.mark_dirty();
            }
            if let Some((text, kind)) = toast {
                self.flash(text, kind);
            }
        }

        // The periodic numbers are kept rather than delivered, so they are read
        // here rather than drained: a repaint that never happened - the app in
        // the background, the window covered - leaves one sample per channel
        // waiting, not a queue of them. Read after the events, so that a sample
        // taken by the session that has just ended cannot be applied over the
        // state its ending set.
        let stats = self.manager.stats().clone();
        for (index, channel) in self.channels.iter_mut() {
            if let Some(entry) = stats.latest(*index) {
                channel.apply_stats(entry.sequence, &entry.stats);
            }
        }
    }

    // ---------------------------------------------------------------- update

    fn advance_animations(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|input| input.stable_dt).clamp(0.0, 0.05);
        if !self.slide.dragging && self.slide.drag != 0.0 {
            self.slide.drag *= 1.0 - (dt * 16.0).min(1.0);
            if self.slide.drag.abs() < 0.5 {
                self.slide.drag = 0.0;
            }
        }
        if self.slide.offset != 0.0 {
            self.slide.offset -= self.slide.offset * (dt * 11.0).min(1.0);
            if self.slide.offset.abs() < 0.002 {
                self.slide.offset = 0.0;
                self.slide.outgoing = None;
            }
        }
        self.blackout.advance(dt);
    }

    /// Settles whether the wall is blank on this frame, and keeps the rest of
    /// the program out of the way while it is.
    fn update_blackout(&mut self, ctx: &egui::Context) {
        let mut on = self.blackout.evaluate(&self.config.blackout, self.time);
        if on && self.blackout.notice_input(ctx, self.time) {
            // Input asks for the wall back, and how long it stays back is the
            // schedule's own setting: the wall is not simply left visible, and
            // it is not taken away again a second later either.
            on = false;
        }
        // The streams follow the blank exactly. It is asked of the scheduler
        // rather than done to the channels, so they are parked the way a page
        // turn parks them, and the same diff brings back what the grid is
        // showing once the blank ends.
        if self.scheduler.set_suspended(on) {
            self.needs_sync = true;
        }
        if self.blackout_on != on {
            self.blackout_on = on;
            tracing::info!(
                target: "xgview::blackout",
                blank = on,
                channels_parked = self.scheduler.is_suspended(),
                "the wall's blank changed"
            );
            self.apply_power_for_blackout();
        }
        if on {
            // The magnified viewport's bar is part of the picture the blank
            // covers, and a control that cannot be seen must not be the one the
            // next press lands on.
            self.tile_bar.hide();
            // Nothing behind the blank may act on this frame's input: a click
            // aimed at a black screen must not press a button that cannot be
            // seen, and the pointer should not be there to aim with either.
            ctx.input_mut(|input| input.events.clear());
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
    }

    /// Asks for what the settings imply, and for what the blank needs on top.
    ///
    /// A blank wall is a reason to keep the display awake that the viewer did
    /// not choose, and it has to outrank the setting: a screen that goes black
    /// and is then powered down is the state this feature exists to avoid. What
    /// the viewer chose is left alone - the blank borrows the request, it does
    /// not change it.
    fn apply_power_for_blackout(&mut self) {
        if !power::is_supported() {
            return;
        }
        let blackout = self.blackout_on;
        let prevent_sleep = self.config.prevent_sleep || blackout;
        let keep_screen_on = self.config.keep_screen_on || blackout;
        if let Err(err) = power::apply(prevent_sleep, keep_screen_on) {
            tracing::warn!(
                target: "xgview::power",
                %err,
                blackout,
                "cannot hold the machine awake for the blank"
            );
        }
        self.power = power::status();
    }

    fn needs_animation(&self) -> bool {
        // The mark breathes, so a blank wall is never still.
        if self.blackout_on {
            return true;
        }
        if self.slide.is_animating() || self.discovery.running || self.discovery.synology_busy {
            return true;
        }
        // A channel that is live keeps repainting: the pictures arrive on the
        // pipeline threads and only the repaint loop turns them into textures.
        if self.channels.values().any(|channel| channel.state.is_live()) {
            return true;
        }
        let live = self.scheduler.current().map(|schedule| schedule.live().count()).unwrap_or(0);
        self.channels.values().filter(|channel| channel.state.is_live()).count() < live
    }

    // --------------------------------------------------------------- drawing

    /// Applies the two power switches and records what the platform did with
    /// them.
    ///
    /// Returns whether the configuration changed - which is to say whether the
    /// panel has something to save. A request the machine refused still changed
    /// the setting the viewer asked for; what went wrong is reported through
    /// `power.error`, under the switches.
    fn apply_power(&mut self, prevent_sleep: bool, keep_screen_on: bool) -> bool {
        self.config.prevent_sleep = prevent_sleep;
        self.config.keep_screen_on = keep_screen_on;
        if let Err(err) = power::apply(prevent_sleep, keep_screen_on) {
            self.flash(
                monitor_i18n::tr_args("settings-power-failed", &[("error", err.to_string().into())]),
                ToastKind::Error,
            );
        }
        // Read back rather than assumed: the request may have been refused, and
        // what the panel says is held should be what is held.
        self.power = power::status();
        true
    }

    /// What the export / import pair does. The desktop picks files with its own
    /// window; Android writes its export into a fixed public folder and walks
    /// that same folder for an import, and says so. See [`Self::ask_for_file`].
    #[cfg(not(target_os = "android"))]
    fn import_hint(&self) -> String {
        monitor_i18n::tr("about-import-hint")
    }

    /// Says what an import of `count` cameras did, naming the file it came
    /// from: the dialog this platform opens hands over a path, so there is
    /// always one to name.
    #[cfg(not(target_os = "android"))]
    fn say_imported(&mut self, count: usize, origin: &Path) {
        self.flash(
            monitor_i18n::tr_args(
                "toast-imported",
                &[("count", count.into()), ("path", origin.display().to_string().into())],
            ),
            ToastKind::Info,
        );
    }

}

/// A clock field for a blackout time, reporting whether it was changed.
///
/// A `DragValue` in the shape [`controls::drag_value`] gives the other tabs -
/// the same navigation entry, the same Left/Right stepping, the same trick of
/// applying the step before the widget runs - with the clock's own formatting on
/// top: the value is a minute of the day, it reads as `HH:MM`, and typing
/// `07:00` into it is what anyone would try.
fn clock_field(nav: &mut crate::nav::Nav, ui: &mut egui::Ui, minutes: &mut u16) -> bool {
    let id = ui.next_auto_id();
    let steps = f64::from(nav.value_step());
    let mut value = f64::from(*minutes);
    if steps != 0.0 && ui.memory(|memory| memory.has_focus(id)) {
        value = (value + steps * 5.0).clamp(0.0, 1439.0);
        // The focused field is showing a text buffer of its own rather than the
        // value behind it, so the step would otherwise be invisible until the
        // focus left - the tab had to be left and come back to see it. Clearing
        // the buffer makes it regenerate the text from the new value in this
        // frame. This is the second half of the trick, not an extra: see the
        // note on `controls::drag_value`, which does the same.
        ui.data_mut(|data| data.remove_temp::<String>(id));
    }
    let response = ui.add(
        egui::DragValue::new(&mut value)
            .range(0.0..=1439.0)
            .speed(5.0)
            .custom_formatter(|value, _| {
                monitor_core::blackout::format_clock(value.round().clamp(0.0, 1439.0) as u16)
            })
            .custom_parser(|text| monitor_core::blackout::parse_clock(text).map(f64::from)),
    );
    nav.item_kind(crate::nav::Kind::DragValue, &response);
    let changed = response.changed() || value != f64::from(*minutes);
    if changed {
        *minutes = value.round().clamp(0.0, 1439.0) as u16;
    }
    changed
}

impl XgViewApp {
    /// Draws one frame.
    ///
    /// This is the whole of what a frame is, and it is an inherent method rather
    /// than the `eframe::App` one because two things drive it: eframe on the
    /// desktop, and - on Android - the loop in `crate::android_surface`, which
    /// owns a `SurfaceView`'s surface instead of an activity window.
    pub fn frame(&mut self, ctx: &egui::Context) {
        let start = self.begin_frame(ctx);
        // Before anything asks whether the picture is on its own layer: a step
        // to another camera has to be noticed in the same frame it happens.
        self.track_surface_picture();
        self.pump_background(ctx);
        self.collect_events(ctx);
        // The strip a recording draws is kept as wide as it is drawn here,
        // rather than only where the viewer moves it: a read that ended in
        // nothing or in a failure left it short until the next drag. See
        // `XgViewApp::read_strip`.
        self.read_strip(ctx);
        let focus_before = self.route_keys(ctx);
        // The ring the last switch anchored: put on the control it names, if the
        // row being drawn carries it. See [`XgViewApp::place_the_anchored_ring`].
        self.place_the_anchored_ring(ctx);
        // The picker is drawn here rather than with the file dialog at the top
        // of the frame: `route_keys` starts the navigation layer, and its rows
        // have to be registered after that or the arrows would have no listing
        // to walk. Both jobs walk the same window - an export names the file it
        // writes in the field at its foot, an import takes one off the listing -
        // so it is up whenever a job is, not only for an import. What it
        // answers is still in hand before the wall is drawn. See
        // `poll_file_dialog`.
        if self.file_job.is_some() {
            self.draw_file_picker(ctx);
        }
        self.draw_chrome(ctx);
        self.draw_wall(ctx);
        self.end_frame(ctx, start, focus_before);
    }
}

impl eframe::App for XgViewApp {
    /// The colour the frame is cleared to, on the paths that go through eframe.
    ///
    /// Transparent while a magnified picture is being scanned out of a surface
    /// under this window: wherever the wall paints nothing, the picture is what
    /// shows. Every other frame clears to the wall's own background - the colour
    /// its panels paint, rather than eframe's own default of `12, 12, 12` at
    /// alpha 180, which is a near-black that belonged to no wall.
    ///
    /// The Android loop in `crate::android_surface` does not ask this - it owns
    /// its render pass and clears to its own colour - so on a television this is
    /// the desktop's answer only, and `picture_on_layer` is false off Android, so
    /// the transparent branch cannot be reached there.
    ///
    /// **Opaque on purpose.** `native_options` builds an opaque window, so alpha
    /// is unused; a window that ever asks for `with_transparent` needs this
    /// method back at eframe's default, or the desktop becomes opaque again with
    /// nothing gained.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.picture_on_layer() {
            return [0.0, 0.0, 0.0, 0.0];
        }
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The window's X is an exit `quit()` never sees. It is read here rather
        // than in `on_exit` because that method's signature is the renderer's,
        // not this view's: with eframe's `glow` feature it takes a glow context
        // and without it takes nothing, and this crate enables glow on the
        // desktop only. `close_requested()` arrives before either goes away, so
        // one line covers both. A `.part` left behind is swept on the next
        // start; what this buys is the task being stopped while the process is
        // still whole.
        if ctx.input(|input| input.viewport().close_requested()) {
            self.download.cancel();
        }
        self.frame(ctx);
    }
}
/// What the frame's beginning read and its end will need.
///
/// The three values `update` used to carry from its first line to its last: the
/// clock the frame is measured by, whether the bars were up as it began, and
/// where the wall's cursor stood. The ring is drawn against the last two, which
/// is why they cannot simply be read at the end - by then the frame has moved
/// both.
struct FrameStart {
    /// `None` unless `XGVIEW_FRAME_MS` asks for the frame to be measured.
    clock: Option<Instant>,
    /// Whether the top and bottom bars were on screen as the frame began.
    bars_was_up: bool,
    /// The wall's cursor as the frame began: the page, and the focused channel.
    cursor_was: (usize, Option<usize>),
}

impl XgViewApp {
    /// The keys, and the focus they leave behind.
    ///
    /// The navigation layer starts its frame here - the layout it collects while
    /// the widgets are drawn below is the one the next frame's arrows read - and
    /// the keys are handed out against that layer. Where they leave the focus is
    /// read once they are done, because a value control confirms with Enter and
    /// gives the focus up as it is drawn, and the form passes it on.
    fn route_keys(&mut self, ctx: &egui::Context) -> Option<Id> {
        // The navigation layer starts its frame here: the layout it collects
        // while the widgets are drawn below becomes the one the next frame's
        // arrows read. The keys handed out just above used the previous one.
        self.nav.begin();
        // The next control of a form was chosen after it had been drawn, so the
        // scroll it asked for is armed now: the frame it is drawn in.
        if let Some(next) = self.reveal_next.take() {
            self.nav.arm_reveal(next);
        }
        // A window handed the focus back to the button it was opened from:
        // applied here, before anything this frame has a chance to move it.
        if let Some(id) = self.restore_focus.take() {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
        // The confirm key that opened a window is still in flight for a frame
        // or two: taken out before any control is drawn, so it cannot land on
        // the one the window has just put under the focus. Both spellings -
        // egui turns either into a click on a focused widget.
        if self.swallow_confirm > 0 {
            self.swallow_confirm -= 1;
            ctx.input_mut(|i| input::consume_any(i, &input::CONFIRM_KEYS));
        }
        // While the wall is blank the keys belong to it: `update_blackout` has
        // already emptied the queue, and skipping this stops a held modifier
        // from being read as a shortcut.
        if !self.blackout_on {
            self.handle_keys(ctx);
        }
        // The reorder mode belongs to the Cameras tab: leaving the tab, or the
        // panel, drops the arrangement that was being previewed. The focus is
        // not sent back - the control it would return to is leaving too.
        if self.reorder.is_some()
            && (!self.show_settings || self.settings_tab != SettingsTab::Cameras)
        {
            self.reorder = None;
            self.reorder_focus = false;
        }
        // Who holds the focus as the frame is drawn. A value control confirms
        // with Enter and gives the focus up while it is drawn, and the form
        // hands it on - see the anchor at the end of the frame.
        let focus_before = ctx.memory(|memory| memory.focused());
        self.advance_animations(ctx);
        self.settle_zoom_park();
        self.sync();
        self.upload_frames();

        // Rebuilt as the frame is drawn: a control that is gone has no name to
        // offer the status line. See `focus_names`.
        self.focus_names.clear();
        focus_before
    }

    /// Reads the state the frame is drawn against, before anything is.
    ///
    /// The blank is settled here too, and before any widget can read the input
    /// queue: what is behind the black must not answer the press that takes the
    /// wall out of it.
    fn begin_frame(&mut self, ctx: &egui::Context) -> FrameStart {
        // The strip under the magnified bar is drawn by the wall, and the end of
        // the frame needs to know whether it was: it is the one focus kept that
        // the navigation layer does not own.
        self.timeline_shown = false;
        // The frame is timed from here to the end of it, and only when
        // `XGVIEW_FRAME_MS` asks: `start` reads a clock, which is cheap, but the
        // `Option` in front of it is cheaper and the default is that nobody is
        // asking. See `crate::frame`.
        let frame_start = self.frame_clock.as_ref().map(crate::frame::FrameClock::start);
        // Portrait is for watching: the settings panel and the layout picker are
        // built for landscape and do not fit, so they are never up while the
        // wall stands upright. Close them if the wall was turned with them open.
        if self.is_portrait() {
            self.show_settings = false;
            self.layout_picker.close();
        }
        // Tell the activity which way to stand, once per change. A no-op on the
        // other platforms, which have no orientation to ask for.
        #[cfg(target_os = "android")]
        crate::android::set_orientation(self.is_portrait());
        // The activity came back to the foreground: retry every channel that is
        // waiting out a backoff, rather than let it spend the delay the
        // background grew. Android only - it is the only platform whose process
        // is suspended and resumed this way.
        #[cfg(target_os = "android")]
        if crate::android::take_resume() {
            self.manager.wake();
        }
        // Whether the bars were up as the last frame left them, read before
        // anything this frame can move them. The focus ring compares it with
        // what the frame leaves behind, at the end of the frame: full screen is
        // entered from a key, from the toolbar's own button and from the Display
        // tab's checkbox alike, and the last two do it while the frame is being
        // drawn. See `ring_follows_bars`.
        let bars_was_up = self.chrome.visible;
        // The wall's cursor as the last frame left it, read with it: the ring is
        // taken down when it moves without the viewer aiming - a page turned this
        // frame, a layout that shrinks under the focus, or an arrow that walked
        // off the edge of the wall (which is aiming, so that one keeps it). See
        // `ring_follows_cursor`.
        let cursor_was = (self.scheduler.page(), self.scheduler.focus());
        self.ring_aimed = false;
        self.time = ctx.input(|input| input.time);

        // Settled before anything else is drawn: whether the wall is blank
        // decides what the rest of the frame does, and the input that asks for
        // it back has to be out of the queue before a widget can read it - one
        // click must not both wake the wall and press whatever is behind the
        // black.
        self.update_blackout(ctx);

        // The magnified viewport's bar belongs to one picture, and to a wall on
        // which nothing else is open: anything that takes that away - the way
        // back to the grid, a step to the next channel, the blank, a panel over
        // the wall - takes the bar with it. The step and the two zooms also
        // close it where they happen; this is the net under all of them.
        //
        // A call is the exception, and the reason it is written here: the
        // microphone being open has to stay on screen, so a panel opening over
        // the wall leaves the bar where it is. It is drawn beside the panel, not
        // over it, and the focus goes with the panel - see the focus hand-off
        // below.
        //
        // What the focus is doing is not part of it. The bar takes the focus
        // itself, and an arrow, a press on the picture or a frame in which egui
        // has just dropped it would otherwise read as "something else has the
        // wall" and take the strip away mid-gesture. See `tap_release` for what
        // does take it down, and `panel_anchor` for the panels that do.
        if !self.scheduler.is_zoomed()
            || self.blackout_on
            || (self.panel_anchor().is_some() && !self.talking)
        {
            self.tile_bar.hide();
        } else if self.tile_bar_pinned() && !self.tile_bar.is_open() {
            // And the net holds it up as well as down. Upright, the strip is
            // part of the picture - pinned - rather than a layer the viewer
            // asks for, and nothing outside this frame's own reasons may leave
            // it down: a press that opened it is not what it waits for, and a
            // step at the end of the channel list that went nowhere must not
            // be able to strand it. See `tile_bar_pinned`.
            self.tile_bar.show(crate::tilebar::Dock::Bottom, Instant::now());
        }
        FrameStart { clock: frame_start, bars_was_up, cursor_was }
    }

    /// Ends the frame: the focus handover, the ring, the repaint and the report.
    ///
    /// It is the half that reads what the beginning wrote, which is why the two
    /// are methods taking a `FrameStart` rather than one long body.
    fn end_frame(&mut self, ctx: &egui::Context, start: FrameStart, focus_before: Option<Id>) {
        // A panel that appears takes the remote's focus, once: without it the
        // arrows would go on walking the wall behind it, which is the one thing
        // a viewer cannot see happening. See `Handoff`.
        self.settings_handoff
            .takes_focus(ctx, self.show_settings, self.settings_anchor, false);
        self.layout_picker.hand_off(ctx, false);
        self.portrait_aspect_picker.hand_off(ctx, false);
        self.combined_info_picker.hand_off(ctx, false);
        self.dialog_handoff
            .takes_focus(ctx, self.discovery.open, self.discovery.focus_anchor, false);
        // The four a press on a control opens. The confirm key that opened one
        // is still in flight for a frame or two, and would confirm the control
        // the window has just been given. See `swallow_confirm`.
        let mut opened = false;
        opened |= self.camera_edit.hand_off(ctx, true);
        opened |= self.remove_confirm.hand_off(ctx, false);
        opened |= self.system_confirm.hand_off(ctx, false);
        opened |= self.passphrase.hand_off(ctx, true);
        // The save panel is asked for by a control on the bar and drawn on the
        // frame after, so its first control does not exist yet when it opens.
        // Waiting for the anchor is what puts the focus inside it at all - a
        // panel whose focus stayed on the wall took no key at all, and its
        // buttons could not be reached by a remote.
        let download_ready =
            self.download_panel.is_open() && self.download_panel.anchor.is_some();
        opened |= self.download_panel.hand_off_when(ctx, download_ready, false);
        // The picker is opened from a control in the settings panel, so its
        // rows do not exist until the following frame. Wait for the first row
        // before spending the hand-off, or the focus stays on the wall and its
        // arrows go to the grid.
        let picker_ready = self.file_picker.is_open() && self.file_picker.anchor.is_some();
        opened |= self.file_picker.hand_off_when(ctx, picker_ready, false);
        if opened {
            self.swallow_confirm = 10;
        }
        // The reorder mode is opened by a press on one frame, and its list is
        // drawn on the next; the first move button is only known once it has
        // been drawn, so the hand-off waits for it here.
        if self.reorder_focus {
            if let Some(id) = self.reorder_anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
                self.reorder_focus = false;
            }
        }

        // An open panel is never left without something focused. The arrows
        // have nowhere to walk from otherwise, and a panel the remote can reach
        // when it opens but not one press later is worse than no panel at all.
        //
        // A press on a blank part of the panel drops the focus, and the frame
        // would end with nothing to walk from. The control that held it at the
        // start of the frame is put back instead - it is still on screen, so the
        // keyboard stays in the panel and the ring does not jump to the top of
        // it. The panel's anchor answers only when there was nothing to put
        // back.
        let anchor = self.panel_anchor();
        if ctx.memory(|memory| memory.focused()).is_none() && !self.file_dialog_open() {
            // A value control confirms with Enter and gives the focus up as the
            // form is drawn. The form hands it to the next control rather than
            // back to the tab strip, and only the last control falls through to
            // the anchor.
            let confirmed = ctx.input(|input| input.key_pressed(Key::Enter));
            let next = focus_before
                .filter(|_| confirmed)
                .and_then(|before| self.nav.next_in_scope(before));
            match next {
                Some(next) => {
                    ctx.memory_mut(|memory| memory.request_focus(next));
                    // It was drawn before the focus reached it: the reveal is
                    // armed at the start of the next frame.
                    self.reveal_next = Some(next);
                }
                None => {
                    let restore = focus_before.filter(|id| self.nav.owns_now(*id)).or(anchor);
                    if let Some(id) = restore {
                        ctx.memory_mut(|memory| memory.request_focus(id));
                    }
                }
            }
        }

        // Where the next frame's direction presses go, and what the status line
        // will call them. Both are read after everything has been drawn,
        // including the hand-offs above, because that is the focus the viewer is
        // looking at - and the names only exist once their controls have been.
        let focused = ctx.memory(|memory| memory.focused());
        self.owner = if focused.is_some() { Owner::Controls } else { Owner::Grid };
        self.focused_name = focused.and_then(|focused| self.focus_names.get(&focused).copied());
        // Kept with `owner`, and from the same read: the next frame's Back has
        // to be answered against the ring the viewer was looking at, not the
        // focus egui may already have dropped before its key was handed out.
        self.focus_at_end = focused;

        // A focused text field keeps the arrow keys for its caret and tells
        // `Memory` so, which is what stops the focus walk from taking them. On a
        // remote control that is backwards: the arrows are the only way across a
        // form, and a field in the middle of one would hold the focus for good.
        //
        // The field re-installs its lock every frame as it is drawn, so this has
        // to be undone *after* that - here, at the end of the frame, where the
        // value it leaves behind is what the next frame's `begin_pass` reads to
        // decide whether a press is offered to the walk at all. The keys
        // themselves are already taken away from the field in `handle_keys`, so
        // its caret stays where it is either way.
        if let Some(focused) = focused {
            if self.typing(ctx) {
                ctx.memory_mut(|memory| memory.set_focus_lock_filter(focused, egui::EventFilter::default()));
            }
        }

        // A control can be asked for the focus and then not be drawn - the
        // anchor restored on a frame a key changed the panel under it, a row
        // removed while it was held. egui does not clear a focus it has just
        // been given, so the frame ends with a focus that has no widget, and the
        // accessibility tree is built from the widgets that *were* drawn: it
        // panics on a focus that is not among them. Let such a focus go here,
        // where this frame's controls are all known.
        //
        // The file dialog is left out of it: it is drawn inside an egui window
        // of its own, and nothing in it was ever drawn under a scope here. The
        // file name field it opens on, and the search field a letter falls into
        // when no field holds the focus, would each read as a focus with no
        // widget and be dropped on the frame it was taken in - and neither
        // could be typed into. While it is up it answers for the focus it
        // holds, the way `panel_anchor` and the hand-offs above are held back
        // from it.
        if !self.file_dialog_open() {
            // Anything the nav layer does not own is dropped from the focus
            // before the bounce runs: the bounce may place the focus on a
            // button that was not in this frame's items - the page turned
            // during the frame, so the items were collected for the old page -
            // and it would be surrendered here if this ran after.  Next frame
            // the button is in the items and the focus is kept.
            //
            // A control that has just gone grey is handed on first: it takes no
            // press and it is no longer in the walk, so a focus left on it is a
            // focus with nowhere to go. See `step_off_a_greyed_control`.
            //
            // The frame's *starting* focus goes with it. egui gives the focus up
            // as soon as it draws a control grey, and a confirm key answered
            // before the bar is drawn greys its own control that same frame, so
            // the end of the frame would otherwise have nothing to hand on and
            // the ring would simply disappear.
            if self.tile_bar.is_open() {
                self.step_off_a_greyed_control(ctx, focus_before);
            }
            if let Some(focused) = ctx.memory(|memory| memory.focused()) {
                // The strip under the magnified bar and the seat the bar opens
                // on are the two focuses that are not the layer's. Both draw a
                // widget and both keep their arrows; surrendering either at the
                // end of every frame threw away the event filter it had just
                // been given, and the next `begin_pass` walked the focus off.
                if !self.nav.owns_now(focused)
                    && !viewport_keeps_focus(
                        focused,
                        self.tile_bar.is_open(),
                        self.timeline_shown,
                        self.timeline_id(),
                    )
                {
                    ctx.memory_mut(|memory| memory.surrender_focus(focused));
                }
            }

            // Bounce the focus to the other page-turn button when a turn
            // reaches the edge. egui surrenders the focus from a widget the
            // moment it is drawn disabled, so by the time this runs the focus
            // may already be gone - and the turn frame is the one that knows
            // the direction. Request the other button directly.
            if let Some(forward) = self.last_turn.take() {
                let info = self.scheduler.page_info();
                let target = if forward && !info.has_next() {
                    self.page_prev_id.filter(|_| info.has_previous())
                } else if !forward && !info.has_previous() {
                    self.page_next_id.filter(|_| info.has_next())
                } else {
                    None
                };
                if let Some(target) = target {
                    ctx.memory_mut(|memory| memory.request_focus(target));
                }
            }
        }

        // The bar is a layer of the interface, and its focus lives with its
        // controls: one that changes under the focus - the microphone a call
        // turns into the handset - leaves the remote with nothing, and the wall
        // behind the bar must not take the arrows back. A focus that is no
        // longer one of the bar's own controls goes to the bar's seat, where it
        // draws nothing until an arrow walks it. See `tilebar`.
        //
        // Not while a panel is up, though: a panel owns the keys while it is
        // open, and a call keeps the bar on screen behind it - taking the focus
        // back every frame would leave the panel's form impossible to walk.
        //
        // And not while the strip is pinned. A pinned strip is not a layer the
        // viewer opens: it is up for as long as the picture is magnified, so
        // this net ran every frame of a magnified portrait picture and put the
        // focus on the seat each time. The wall therefore never held the keys -
        // `owner` is `Controls` whenever anything is focused - and Back was
        // answered by the path that hands the keys back to the wall instead of
        // by `back`, which is the magnification's way out. A press that should
        // have left the viewport only surrendered the focus, and the next frame
        // took it back. See `tile_bar_pinned`.
        if self.tile_bar.is_open() && self.panel_anchor().is_none() && !self.tile_bar_pinned() {
            // A focus on one of the bar's ids is the bar's, whether or not that
            // control is drawn this frame: the microphone a call turns into the
            // handset is one, and the handset it becomes is another. Anything
            // else goes back to the seat, where it draws nothing until an arrow
            // walks it off.
            //
            // **The strip under the bar is the exception**: it is what Down goes
            // to and what the arrows walk, and it is not one of the bar's ids.
            // Without this it was put back on the seat every frame - the button
            // lost its ring, nothing had the keys, and the next arrow walked off
            // the seat onto the first control. See `timeline_input`.
            let focused = ctx.memory(|memory| memory.focused());
            let strip = self.timeline_shown && focused.is_some() && focused == self.timeline_id();
            if !focused.is_some_and(tilebar::owns) && !strip {
                ctx.memory_mut(|memory| memory.request_focus(tilebar::seat()));
            }
        }

        // The navigation layer ends its frame: it keeps the walk off the arrows
        // of whatever it has focused, and makes this frame's layout the one the
        // next frame's arrows read. After the field's own lock above, so that a
        // form field of a window on this layer is walked, not held.
        self.nav.finish(ctx);

        // The keyboard follows the focus: raised while a text field has it, and
        // dropped as soon as it does not, so that it never sits over the wall
        // taking keys the viewer meant for it.
        #[cfg(target_os = "android")]
        {
            let focused = ctx.memory(|memory| memory.focused());
            // The keyboard went away on its own - Back, or its hide button -
            // and the field is no longer being edited. Before the click below,
            // so that a touch on the field in the same frame confirms it again.
            if crate::keyboard::take_hidden() {
                self.editing = None;
            }
            // A touch click on a text field confirms it for editing, as Enter
            // does: the field has the focus and the caret, and the keyboard
            // follows. On a remote the pointer does not click, so this does not
            // fire for a DPAD walk - only an actual touch on the screen.
            if ctx.input(|i| i.pointer.primary_clicked())
                && focused.is_some_and(|f| Self::soft_keyboard(self.nav.kind(f)))
            {
                self.editing = focused;
            }
            // The keyboard tracks `editing`, not the focus: a field only raises
            // it once confirmed with Enter or a touch, and keeps it until Back
            // or a focus change clears that confirmation. Walking a form with
            // the arrows never puts the keyboard up.
            // The floating input box goes up with the keyboard, over the top of
            // the screen where the keyboard cannot reach. Its buttons may hand
            // the focus back to the field, so the focus is read again after it.
            self.input_overlay(ctx);
            let focused = ctx.memory(|memory| memory.focused());
            let wanted = focused.is_some_and(|f| self.editing == Some(f));
            // The shape of the field being edited decides the soft keyboard's
            // action key: a multi-line field must keep Enter as a newline. Read
            // from the mirror the field left for the floating box, and only for
            // the field actually being edited.
            let multiline = ctx
                .data(|data| data.get_temp::<dialogs::Mirror>(Id::new(dialogs::MIRROR)))
                .is_some_and(|mirror| self.editing == Some(mirror.id) && mirror.multiline);
            crate::keyboard::set_wanted(wanted, focused, multiline);
            if self.editing.is_none() {
                ctx.data_mut(|data| data.remove::<dialogs::Mirror>(Id::new(dialogs::MIRROR)));
            }
        }

        // The focus ring's connection with the bars, read at the end of the
        // frame and against the state the frame began in. Both halves matter:
        // full screen is entered from a key (in `handle_keys`, before anything is
        // drawn) as well as from the toolbar's own button and the Display tab's
        // checkbox (while the frame is being drawn, after the bars have been
        // asked for), so only reading it here sees every way they can come and
        // go. A toggle made during the drawing would otherwise be missed twice
        // over - the drawing already saw the old state, and the state read at
        // the start of the next frame has it gone already. One frame of delay,
        // and only for the frame the ring is drawn in. See `ring_follows_bars`.
        self.ring_seen = Self::ring_follows_bars(self.ring_seen, self.chrome.visible, start.bars_was_up, Instant::now());
        // And the wall moving its own cursor takes down a ring that is showing:
        // a page turned by a key, a button pressed by a remote or a pointer, or a
        // swipe, and a layout that re-places the focus under the viewer, all land
        // here.
        self.ring_seen = Self::ring_follows_cursor(
            self.ring_seen,
            (self.scheduler.page(), self.scheduler.focus()),
            start.cursor_was,
            self.ring_aimed,
        );

        // The call lives in the bar, and the handset is the way out of it, so a
        // call in progress makes the bar modal: a press that would put it away
        // leaves it where it is. A panel is the exception - it needs the keys,
        // and a strip lying over a form is in the way - so it takes the bar down
        // while it is up and the bar comes back when it goes.
        //
        // What ends a call is the handset, or leaving this camera: `set_target`
        // turns the microphone off for every way of leaving - the grid, another
        // group, another layout, a double press on the picture - so that half of
        // the rule is stated once, there, rather than here.
        if self.talking && !self.tile_bar.is_open() && self.panel_anchor().is_none() {
            let dock = self.tile_bar.dock();
            self.tile_bar.show(dock, Instant::now());
        }

        // The magnified viewport's bar keeps a clock of its own, and the repaint
        // is asked for against it: a wall showing no live picture is not
        // repainting on its own, and the bar still has to go when its time is up.
        //
        // A call in progress is the exception: its bar carries the one control
        // that ends it, so it stays up for as long as the microphone is open
        // rather than going down on the idle the viewer last touched it at.
        if let Some(left) = self.tile_bar.remaining(Instant::now()) {
            // A recording holds the panel up: the timeline is the only thing
            // that says where the picture is, and it is what a scrub is aimed
            // at, so it cannot be something to ask for again every eight
            // seconds. A tap still puts it away.
            if self.talking || self.tile_bar_pinned() || self.playback.is_some() {
                self.tile_bar.touch(Instant::now());
            } else if left.is_zero() {
                self.tile_bar.hide();
                // This hide happens after the frame's focus rules have run.
                // Its seat is no longer a widget, and a focus left on it
                // keeps the wall from its keys until egui notices - so take
                // the focus away here instead of waiting for that.
                self.leave_controls(ctx);
            } else {
                // A bar that is held up - a question the box has not answered
                // yet - has no time of its own to run, and the time it has left
                // is the time the hold has left. Asking for a repaint after it
                // is what brings the wall back to put the bar down when the
                // hold runs out. See `tilebar::HOLD`.
                ctx.request_repaint_after(left);
            }
        }

        let animation = self.needs_animation();
        ctx.request_repaint_after(if animation { LIVE_REPAINT } else { Duration::from_millis(500) });

        // The soft keyboard is moved by `keyboard::set_wanted` alone. egui would
        // otherwise answer its own focused `TextEdit` by asking eframe to enable
        // the window's input method, and winit toggles that through
        // `android-activity` - which hides and re-shows the keyboard around a
        // touch on the window, flickering over the field. Dropping the request
        // each frame keeps that from ever firing; the Android keyboard is not
        // egui's to move.
        #[cfg(target_os = "android")]
        ctx.output_mut(|output| output.ime = None);

        // Last, so that what the line says is what this frame actually drew:
        // the description is built only for a frame that is going to be
        // reported, and the clock is taken by name first so that reading the
        // scene does not hold a borrow of it.
        if let Some(start) = start.clock {
            let took = start.elapsed();
            if self.frame_clock.as_ref().is_some_and(|clock| clock.wants(took)) {
                let scene = self.describe_scene();
                if let Some(clock) = self.frame_clock.as_mut() {
                    clock.report(took, &scene);
                }
            }
        }
    }
}

impl XgViewApp {
    /// The frame's background work: where the window sits among the others, the file
    /// dialog, the keyboard, and a close that was asked for.
    pub(super) fn pump_background(&mut self, ctx: &egui::Context) {
        // The start-up full screen is asked for here, on the first frame the
        // window is on its monitor, rather than on the viewport builder: asked
        // for there it is answered before the window has a monitor to be made
        // full screen on, and Windows then leaves it covering a window-sized
        // area - the same wrong geometry that toggling F11 twice repairs. See
        // `native_options` in `lib.rs`.
        if std::mem::take(&mut self.fullscreen_pending) {
            self.set_fullscreen(ctx, true);
        }

        // Another launch asked this window to come forward. The restore is a
        // command of its own, a frame before the focus: winit only raises a
        // window it does not consider minimised, and it looks at the state it
        // had before this frame, so sending the two together would drop the
        // focus. See `crate::instance`.
        if crate::instance::asked_to_come_forward() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            self.come_forward = true;
        } else if std::mem::take(&mut self.come_forward) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        // A suspension with a clock on it ends by itself, on the first frame
        // after the moment it named. The other kind is ended by the code that
        // began it, which knows when the thing it opened is over.
        if let OnTop::SuspendedUntil(until) = self.on_top {
            if Instant::now() >= until {
                self.set_on_top(ctx, OnTop::Configured);
            }
        }

        // The file dialog, while it is up: drawn and answered here, at the top
        // of the frame, so that a configuration it picked is the one the wall
        // behind is drawn from. See `ask_for_file`.
        self.poll_file_dialog(ctx);

        // Anything the soft keyboard typed has to be in the input queue before
        // a widget is drawn: a text field reads the events of the frame it is
        // drawn in, so a keystroke handed over later would be a frame behind.
        #[cfg(target_os = "android")]
        crate::keyboard::drain(ctx);

        if ctx.input(|input| input.viewport().close_requested()) {
            self.save_now();
            self.manager.shutdown();
            // The audio devices go here, on the thread that opened them and
            // while the channel workers above have just been stopped. eframe
            // does not return from `run_native` on the desktop - it calls
            // `std::process::exit(0)` - so the shutdowns after it in
            // `lib.rs::run` are dead code on this path, and the cpal streams
            // would otherwise be dropped by the loader's TLS callbacks during
            // teardown. Their `QuietDrop` guard makes that safe, but letting
            // them go here keeps the join on a healthy stack. See `audio::shutdown`.
            // Android has no cpal stream parked in a thread local, so this is
            // the desktop's close path only.
            #[cfg(not(target_os = "android"))]
            {
                crate::audio::shutdown();
                crate::mic::shutdown();
            }
            // The request outlives the viewport otherwise: on the desktop this
            // is the last moment anything of ours runs.
            if let Err(err) = power::release() {
                tracing::warn!(target: "xgview::power", %err, "cannot take the wake request back");
            }
        }
    }

    /// What the background jobs answered since the last frame, and the press in
    /// flight - both read before anything is drawn.
    ///
    /// `poll_events` is the manager's queue; this is everything that has to be
    /// in hand before a widget is drawn, which includes it.
    pub(super) fn collect_events(&mut self, ctx: &egui::Context) {
        self.collect_clean_view_input(ctx);
        // Read before the keys are handed out: `handle_keys` takes the ones the
        // application acts on out of the queue, and the bars still have to know
        // a viewer was there. What counts as asking for them is the whole of
        // `viewer_active`.
        self.chrome.input_seen = self.viewer_active(ctx);
        // And the press in flight is tracked before anything is drawn: the
        // release that closes the magnified viewport's bar is read against it.
        if !self.chrome.consume_primary {
            self.track_tap(ctx);
        }
        // A tap asks for the strip, but only once egui's double-click window has
        // passed: the second tap of a double press is the way back to the grid,
        // and the first must not put the strip on screen on its way there. See
        // `tile_bar_overlay`.
        if let Some(at) = self.tap_open_at {
            let now = Instant::now();
            if now >= at {
                self.tap_open_at = None;
                if self.scheduler.is_zoomed() && !self.tile_bar.is_open() {
                    self.tile_bar.show(tilebar::Dock::Bottom, now);
                }
            } else {
                ctx.request_repaint_after(at - now);
            }
        }

        self.poll_events(ctx);
        self.poll_sealing();
        self.poll_talk_test();
        // An answer comes back with the wall drawn a frame or two after it was
        // asked for, so the frame it lands in has to come on its own.
        if self.sealing.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// The strips the platform reserves and the two bars, then the bands they leave
    /// behind for the pointer to find them by.
    pub(super) fn draw_chrome(&mut self, ctx: &egui::Context) {
        // Android: the navigation bar keeps a strip along one edge, and which
        // edge that is belongs to the device - a landscape phone carries it
        // along a short edge, a tablet whose natural orientation is landscape
        // along the bottom one. The wall leaves exactly that strip free, so no
        // control sits under the bar. A television and an external display have
        // no navigation bar, so those insets are zero and everything stays edge
        // to edge. The notch is separate: it is reserved even with the bars
        // hidden, as a top strip upright, a bottom strip when the device is held
        // the other way up, and a side strip on its side, because a toolbar or
        // tile drawn under it cannot be used. The activity reports only the
        // notch when the bars are hidden, and the notch and the bar together
        // when they are reserved; see `MainActivity.applySystemBars`.
        #[cfg(target_os = "android")]
        let (inset_left, inset_top, inset_right, inset_bottom) = {
            crate::android::set_reserve_navigation_bar(self.config.reserve_navigation_bar);
            crate::android::insets_px()
        };
        #[cfg(not(target_os = "android"))]
        let (inset_left, inset_top, inset_right, inset_bottom) = (0_i32, 0_i32, 0_i32, 0_i32);
        let pixels_per_point = ctx.pixels_per_point();
        let inset_left = inset_left as f32 / pixels_per_point;
        let inset_top = inset_top as f32 / pixels_per_point;
        let inset_right = inset_right as f32 / pixels_per_point;
        let inset_bottom = inset_bottom as f32 / pixels_per_point;
        if inset_left >= 1.0 {
            egui::SidePanel::left("xgview-gutter-left")
                .resizable(false)
                .exact_width(inset_left)
                .frame(egui::Frame::default().fill(theme::BACKGROUND))
                .show(ctx, |_| {});
        }
        if inset_right >= 1.0 {
            egui::SidePanel::right("xgview-gutter-right")
                .resizable(false)
                .exact_width(inset_right)
                .frame(egui::Frame::default().fill(theme::BACKGROUND))
                .show(ctx, |_| {});
        }
        if inset_top >= 1.0 {
            egui::TopBottomPanel::top("xgview-gutter-top")
                .resizable(false)
                .exact_height(inset_top)
                .frame(egui::Frame::default().fill(theme::BACKGROUND))
                .show(ctx, |_| {});
        }
        if inset_bottom >= 1.0 {
            egui::TopBottomPanel::bottom("xgview-gutter-bottom")
                .resizable(false)
                .exact_height(inset_bottom)
                .frame(egui::Frame::default().fill(theme::BACKGROUND))
                .show(ctx, |_| {});
        }

        // In full screen the bars fade out when the wall is left alone; see
        // `chrome_visible`. The row is not inset on the right: the version
        // label that ends it is what keeps the touch screen's back-gesture band
        // clear of the controls; see `toolbar`.
        //
        // Whether the band of each bar has to be claimed this frame is read
        // *before* the bars are asked for, because the frame a press in a band
        // brings them back in is one of the frames that has to claim it.
        let bands_hidden = !self.chrome.visible;
        let bars = self.chrome_visible(ctx);
        if bars {
            let enabled = !self.chrome.consume_primary;
            let toolbar = egui::TopBottomPanel::top("xgview-toolbar").show(ctx, |ui| {
                if !enabled { ui.disable(); }
                self.toolbar(ui);
            });
            let status = egui::TopBottomPanel::bottom("xgview-status").show(ctx, |ui| {
                if !enabled { ui.disable(); }
                self.status_bar(ui);
            });
            // Kept past this frame, when the bars are gone: the band a bar
            // occupies is the pointer's way of asking for it back. See
            // `Chrome::bars`.
            self.chrome.bars = Some((toolbar.response.rect, status.response.rect));
        } else {
            // The bar is gone, and with it anything it could hand the remote's
            // focus to: keeping the list would point the focus at a control
            // that is not on screen.
            self.toolbar_items.clear();
            self.status_author = None;
            // A pointer the viewer cannot see is a pointer they cannot aim. The
            // bars stay away - moving the mouse is not a request for them - but
            // the cursor comes back with the first movement, so that a viewer
            // can find the band they are meant to press.
            if ctx.input(|input| input.pointer.is_moving()) {
                ctx.set_cursor_icon(egui::CursorIcon::Default);
            } else {
                ctx.set_cursor_icon(egui::CursorIcon::None);
            }
        }
        // The band each bar sat in is the pointer's way of asking for it back -
        // see `viewer_active` - and the wall is drawn underneath it, so it has
        // to be the pointer's alone: without this, the press that asks for the
        // bars would land on the tile behind it as well, and a viewer reaching
        // for the menu would also move the focus. A press is hit-tested against
        // the widgets of the frame it lands in, which is why the frame that
        // brings the bars back claims the band just like any other: by the time
        // the press is placed the bars are drawn, and the band is over them.
        //
        // Magnified, the band is the viewport's own bar's instead, and the tile
        // under it is what has to see the press: these overlays are exactly
        // where that bar is drawn, so one of them would eat the press before the
        // tile could open it. See `crate::tilebar` and `wall_event`.
        if bands_hidden && !self.config.clean_view && !self.scheduler.is_zoomed() {
            // Held to this frame's screen: the rectangles are from the frame the
            // bars were last drawn in, and a window made full screen since has
            // bands at the top and bottom of a taller screen than those - an
            // overlay over the middle of the wall eats every press on a tile
            // there. See `XgViewApp::bands`.
            if let Some((top, bottom)) = XgViewApp::bands(self.chrome.bars, ctx.screen_rect()) {
                for (id, band) in [("xgview-toolbar-band", top), ("xgview-status-band", bottom)] {
                    // The size is given, and never left to the area's own
                    // default: an area with none starts at `default_area_size`
                    // and is then constrained back onto the screen, which for
                    // the bottom band moves its top to the top of the window -
                    // an overlay over the whole wall, eating every press on a
                    // tile. See `band_overlay_area_is_the_band`.
                    egui::Area::new(Id::new(id))
                        .order(egui::Order::Middle)
                        .fixed_pos(band.min)
                        .default_size(band.size())
                        .show(ctx, |ui| {
                            ui.allocate_rect(band, egui::Sense::click());
                        });
                }
            }
        }
    }

    /// The wall, the panel over it, the windows and the pickers, and the save that
    /// ends the drawing.
    pub(super) fn draw_wall(&mut self, ctx: &egui::Context) {
        if self.show_settings {
            egui::SidePanel::right("xgview-settings")
                .default_width(SETTINGS_WIDTH)
                .min_width(SETTINGS_MIN_WIDTH)
                .show(ctx, |ui| self.settings_panel(ui));

            // The pointer's way out of the panel: a click anywhere on the wall
            // beside it. A remote control and a keyboard have Back and Escape,
            // which say "close the panel" from wherever the focus happens to
            // be; a mouse or a finger has only the gear that opened it, and a
            // control that opens something is not read as the one that closes
            // it either.
            //
            // The press is taken by a layer over the wall rather than read
            // from the input queue, so that the click which dismisses the
            // panel cannot also land on the tile under the pointer - a click
            // on the wall magnifies that channel, and closing the panel while
            // zooming into a picture behind it is not what the viewer asked
            // for. The layer stops short of the bars: they keep working while
            // the panel is open, and the gear closes it by itself.
            //
            // Its rect is what the central panel is about to be given, so it
            // covers the wall and nothing else - the panel has just been laid
            // out, and the bars before it.
            let wall = ctx.available_rect();
            let dismissed = egui::Area::new(Id::new("xgview-settings-dismiss"))
                .order(egui::Order::Middle)
                .fixed_pos(wall.min)
                .show(ctx, |ui| ui.allocate_rect(wall, egui::Sense::click()).clicked())
                .inner;
            if dismissed {
                self.show_settings = false;
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(if self.picture_on_layer() {
                // See-through, so the picture scanned out under this window
                // shows where the wall paints nothing. A one-cell grid has no
                // such picture and gets the wall's own background instead.
                egui::Color32::TRANSPARENT
            } else {
                theme::BACKGROUND
            }))
            .show(ctx, |ui| {
                if self.chrome.consume_primary { ui.disable(); }
                let area = ui.available_rect_before_wrap();
                self.draw_grid(ui);
                // The floating page arrows sit at the very edges the magnified
                // viewport's bar is docked to, and are drawn after the wall - so
                // they would lie on top of it. The bar is the surface in use
                // while it is up, and the bars' own page buttons are still
                // there; the arrows come back with the picture once it goes.
                //
                // Which is also why, with a picture scanning out of its own
                // surface, the overlay draws a second copy of them: this one is
                // under that picture. Both ask `page_arrows_shown`, and both put
                // them where `portrait_page_arrow_sides` says; only this copy
                // takes the presses.
                if self.page_arrows_shown() {
                    self.portrait_page_buttons(ui, area);
                }
            });

        if self.discovery.open {
            let handle = self.handle.clone();
            let events = self.events_tx.clone();
            if dialogs::add_devices_window(
                ctx,
                &mut self.discovery,
                &mut self.config,
                &handle,
                &events,
                &mut self.nav,
            ) {
                self.needs_sync = true;
                self.mark_dirty();
            }
            // The manual tab's test button: the dialog has no runtime, so the
            // press comes back here and is played like the edit window's.
            if std::mem::take(&mut self.discovery.talk_test) {
                let result = self.discovery.manual.build();
                match result {
                    Ok(source) => self.spawn_test_tone(source),
                    Err(error) => self.flash(error, ToastKind::Error),
                }
            }
            // The window closed itself - its cross, or Back - so the button it
            // was opened from takes the focus again.
            if !self.discovery.open {
                self.restore_focus = self.discovery.from.take();
            }
        }

        if self.camera_edit_window(ctx) {
            self.mark_dirty();
        }
        if self.remove_confirm_window(ctx) {
            self.mark_dirty();
        }
        self.system_confirm_window(ctx);
        if self.passphrase_window(ctx) {
            self.mark_dirty();
        }
        self.download_panel_window(ctx);

        self.draw_layout_picker(ctx);
        self.draw_portrait_aspect_picker(ctx);
        self.draw_combined_info_picker(ctx);
        self.draw_toast(ctx);
        self.draw_exit_hint(ctx);
        // After everything else, and on its own layer: the blank covers the
        // toolbar, the status bar and the tiles, not only the wall.
        if self.blackout_on {
            self.blackout.draw(ctx, self.time);
        }
        self.autosave();
    }
}

impl XgViewApp {
    /// What this frame was drawing, for a frame that ran long.
    ///
    /// One line, read beside the milliseconds: a stutter on the wall, a stutter
    /// with a panel over it and a stutter on a magnified picture are three
    /// different problems, and the number alone does not say which one was seen.
    fn describe_scene(&self) -> String {
        let mut what = String::from(if self.scheduler.is_zoomed() { "magnified" } else { "wall" });
        if self.show_settings {
            what.push_str(&format!(" settings={:?}", self.settings_tab));
        }
        if self.layout_picker.is_open() {
            what.push_str(" layouts");
        }
        if self.blackout_on {
            what.push_str(" blank");
        }
        if self.talking {
            what.push_str(" call");
        }
        what.push_str(&format!(
            " channels={} textures={} decoder={}",
            self.channels.len(),
            self.textures.len(),
            self.decoder
        ));
        what
    }
}

/// Whether a camera's sub stream address was guessed from its main URL.
///
/// The badge this feeds is about where the address came from, not about what it
/// looks like. An ONVIF profile that produced `…/videoSub` (Foscam's `prof1`) is
/// a real address even though it reads exactly like what the `/videoMain` ->
/// `/videoSub` rule would have derived. `sub_profile` records that a profile
/// produced it, so a camera that has one is never shown as inferred.
fn sub_is_derived(camera: &CameraSource, sub: &str) -> bool {
    camera.sub_profile.is_none()
        && sub == monitor_core::model::infer_sub_stream(&camera.rtsp_main).unwrap_or_default()
}

/// How long the viewer has been running, in the units its language names.
///
/// The seconds are always there, so the line moves while the page is open even
/// when the viewer has been up for days; every unit above them that has a value
/// is named too, and one that is left at zero between two that are not is
/// still named, so the parts always read in order.
fn uptime_text(elapsed: Duration) -> String {
    let unit = |value: u64, key: &str| monitor_i18n::tr_args(key, &[("n", (value as usize).into())]);
    let total = elapsed.as_secs();
    let (days, hours, minutes, seconds) =
        (total / 86_400, total % 86_400 / 3_600, total % 3_600 / 60, total % 60);
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(unit(days, "about-duration-days"));
    }
    if days > 0 || hours > 0 {
        parts.push(unit(hours, "about-duration-hours"));
    }
    if days > 0 || hours > 0 || minutes > 0 {
        parts.push(unit(minutes, "about-duration-minutes"));
    }
    parts.push(unit(seconds, "about-duration-seconds"));
    parts.join(" ")
}

/// This frame's pinch, as the devices reported it: how far the fingers spread
/// since the frame before - a ratio, `1.0` for unchanged - and where they are, in
/// screen points.
///
/// A touch screen counts its own fingers and says where their middle is; a
/// trackpad has no fingers to count and sends zoom events instead (a mouse's
/// zoom-modified wheel arrives as the same factor), aimed where the pointer is.
/// Both are read here, so that a gesture has one answer rather than two, and
/// neither is taken out of the queue - what the frame holds is what the devices
/// sent.
///
/// `area` is the wall: a gesture made over a panel or a bar belongs to whatever
/// is there, and the picture behind it is not what is being pushed in.
fn pinch_frame(input: &egui::InputState, area: Rect) -> Option<(f32, egui::Pos2)> {
    if let Some(touch) = input.multi_touch() {
        return area.contains(touch.center_pos).then_some((touch.zoom_delta, touch.center_pos));
    }
    let delta = input.zoom_delta();
    if delta == 1.0 {
        return None;
    }
    let at = input.pointer.hover_pos()?;
    area.contains(at).then_some((delta, at))
}

/// How much the plain wheel asked to push the picture in by this frame, if it was
/// turned - and where, in screen points.
///
/// A wheel turned with the zoom modifier - Cmd on macOS, Ctrl elsewhere - is the
/// *other* kind of wheel zoom, and [`pinch_frame`] reads it along with a
/// trackpad's pinch. This one is the plain wheel, which egui reads as a scroll,
/// because everywhere else in the interface there is something to scroll: in the
/// magnified viewport there is not, and the picture is what the wheel is over.
///
/// The factor is egui's own - `speed` is its `scroll_zoom_speed` and the delta is
/// the one it has already smoothed - so a plain wheel and a zoom-modified one
/// push the picture in by the same amount per notch, and the smoothing adds up to
/// the same total because the factors multiply. A wheel turned anywhere the
/// interface scrolls is not ours to read, which is what `area` says.
fn wheel_frame(input: &egui::InputState, speed: f32, area: Rect) -> Option<(f32, egui::Pos2)> {
    // `Modifiers::COMMAND`, which egui's own `zoom_modifier` is set to.
    if input.modifiers.command {
        return None;
    }
    let scrolled = input.smooth_scroll_delta.x + input.smooth_scroll_delta.y;
    if scrolled == 0.0 {
        return None;
    }
    let at = input.pointer.hover_pos()?;
    area.contains(at).then(|| ((speed * scrolled).exp(), at))
}

/// How far the magnified picture was dragged this frame, in points, if it was.
///
/// The pointer's own movement, rather than a tile's drag: a magnified viewport is
/// one picture over the whole wall, and there is nothing beside it to catch a
/// drag as one. A gesture made with two fingers is not a drag of its own - the
/// pointer follows the first of them - and a press that began on a bar belongs to
/// the bar, which is why where it began is what decides.
///
/// The frame the finger lands on is not a movement. `egui` measures a frame's
/// movement from wherever the pointer was *last seen*, and a touch screen has no
/// pointer between touches: the position it remembers is the last place a finger
/// was lifted from. So the first frame of every drag reports the distance between
/// two unrelated touches - the picture jumped by it, once, the moment a finger
/// touched down anywhere but where the last one left.
fn drag_frame(input: &egui::InputState, area: Rect) -> Option<egui::Vec2> {
    if input.pointer.any_pressed() {
        return None;
    }
    let delta = input.pointer.delta();
    if delta == egui::Vec2::ZERO || input.multi_touch().is_some() {
        return None;
    }
    let origin = input.pointer.press_origin()?;
    area.contains(origin).then_some(delta)
}

/// Whether a point is in the strip the magnified bar occupies along its edge.
///
/// The bar reaches [`tilebar::DEPTH`] points in from the picture edge, and its
/// activation band is shallower, so this covers both. A gesture that starts here
/// belongs to the bar, not the picture behind it: a finger's tap carries a little
/// movement, and reading that as a pan puts the bar away before it can be used.
/// Upright, the picture's top edge is the screen's right one and its bottom the
/// left, the same mapping the bar itself uses.
fn in_tile_bar_strip(pos: egui::Pos2, area: Rect, rotated: bool, dock: tilebar::Dock) -> bool {
    let depth = tilebar::DEPTH;
    match (rotated, dock) {
        (false, tilebar::Dock::Top) => pos.y <= area.top() + depth,
        (false, tilebar::Dock::Bottom) => pos.y >= area.bottom() - depth,
        (true, tilebar::Dock::Top) => pos.x >= area.right() - depth,
        (true, tilebar::Dock::Bottom) => pos.x <= area.left() + depth,
    }
}

/// The bands between `outer` and `inner`, skipping the ones with no area.
///
/// Where a picture on a surface of its own does not fill the screen, these are
/// the rectangles beside it: the wall paints nothing there (the picture's view
/// is placed at `inner` and the wall's frame is behind it), so something above
/// the picture has to. The same rectangles cover the picture's view when it is
/// scaled past that box, which is what a pinch does to it.
fn bands_around(outer: Rect, inner: Rect) -> Vec<Rect> {
    let inner = inner.intersect(outer);
    [
        Rect::from_min_max(outer.min, egui::pos2(outer.max.x, inner.min.y)),
        Rect::from_min_max(egui::pos2(outer.min.x, inner.max.y), outer.max),
        Rect::from_min_max(
            egui::pos2(outer.min.x, inner.min.y),
            egui::pos2(inner.min.x, inner.max.y),
        ),
        Rect::from_min_max(
            egui::pos2(inner.max.x, inner.min.y),
            egui::pos2(outer.max.x, inner.max.y),
        ),
    ]
    .into_iter()
    .filter(|band| band.width() > 0.5 && band.height() > 0.5)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, pos2};

    /// The band the two bars were drawn in, as `chrome.bars` holds it.
    fn bars() -> Option<(Rect, Rect)> {
        Some((
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 36.0)),
            Rect::from_min_max(pos2(0.0, 690.0), pos2(1280.0, 720.0)),
        ))
    }

    /// A window opened by a control in the panel is drawn at the top of the
    /// *next* frame. If the one-shot hand-off is spent on the opening frame,
    /// before its first row exists, the focus stays on the wall behind it and
    /// the arrows never reach the window - the file picker's failure. Waiting
    /// for the anchor keeps the hand-off for the frame it can do something.
    #[test]
    fn a_window_drawn_a_frame_late_is_not_handed_the_focus_too_soon() {
        let ctx = egui::Context::default();
        let mut modal: Modal<()> = Modal::default();
        let row = Id::new("first-row");

        modal.open(());
        assert!(!modal.hand_off_when(&ctx, false, false), "no row has been drawn yet");
        assert!(ctx.memory(|memory| memory.focused()).is_none());

        modal.anchor = Some(row);
        assert!(modal.hand_off_when(&ctx, true, false), "the frame its rows were drawn on");
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(row));
        assert!(!modal.hand_off_when(&ctx, true, false), "and only that frame");
    }

    /// The sub badge is about provenance, not shape: a Foscam's ONVIF sub stream
    /// is byte-identical to what the URL rule would derive, and must not be
    /// reported as inferred.
    #[test]
    fn a_sub_stream_a_profile_produced_is_not_reported_as_derived() {
        let mut camera = CameraSource::new("front door", "rtsp://10.0.0.1:88/videoMain");
        camera.rtsp_sub = Some("rtsp://10.0.0.1:88/videoSub".to_string());
        // The rule's own answer, with no profile behind it: derived.
        assert!(sub_is_derived(&camera, camera.rtsp_sub.as_deref().unwrap()));
        // The same address, produced by profile `prof1`: not derived.
        camera.sub_profile = Some("prof1".to_string());
        assert!(!sub_is_derived(&camera, camera.rtsp_sub.as_deref().unwrap()));
        // An address the rule would never produce is never reported as derived.
        camera.sub_profile = None;
        camera.rtsp_sub = Some("rtsp://10.0.0.1:88/low".to_string());
        assert!(!sub_is_derived(&camera, camera.rtsp_sub.as_deref().unwrap()));
    }

    /// What a viewer does while watching a wall must not bring the bars back:
    /// they take their height from the wall, so every tile would move under the
    /// press, and slide back a moment later.
    #[test]
    fn wall_keys_and_wall_presses_do_not_ask_for_the_bars() {
        for key in [
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::Enter,
            Key::Space,
            Key::PageUp,
            Key::PageDown,
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
            Key::Num0,
        ] {
            assert!(XgViewApp::wall_key(key), "{key:?} acts on the wall");
            assert!(!XgViewApp::wall_event(bars(), true, true, &press(key)), "{key:?} acts on the wall");
        }
        // A press on the picture, magnifying or not, is a press on the picture.
        assert!(!XgViewApp::in_a_bar_band(bars(), Pos2::new(640.0, 360.0)));
        assert!(!XgViewApp::wall_event(bars(), true, true, &egui::Event::PointerButton {
            pos: Pos2::new(640.0, 360.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        }));
        // The release that ends that press is not a request of its own, wherever
        // it lands: a tap arrives as a press and a release, and reading the
        // release as the viewer would put the bars back under every touch on the
        // wall.
        for pos in [Pos2::new(640.0, 360.0), Pos2::new(640.0, 12.0)] {
            assert!(
                !XgViewApp::wall_event(bars(), true, true, &egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                }),
                "a release at {pos:?} ends a press, it does not ask for anything"
            );
        }
        // A press in the band a bar sat in is the pointer asking for it back.
        assert!(XgViewApp::in_a_bar_band(bars(), Pos2::new(640.0, 12.0)));
        assert!(XgViewApp::in_a_bar_band(bars(), Pos2::new(640.0, 705.0)));
        // The band is read by height, so a rectangle measured before the window
        // reached the size it ends up on still answers for a press near its edge.
        assert!(XgViewApp::in_a_bar_band(bars(), Pos2::new(9000.0, 12.0)));
        // Before the bars have been drawn once there is no band to press, and a
        // press anywhere is a press on the wall.
        assert!(!XgViewApp::in_a_bar_band(None, Pos2::new(640.0, 12.0)));
    }

    /// Magnified with the bars away, the band along the top and bottom of the
    /// picture is that viewport's own bar being asked for, and not the menus:
    /// the two bands are the same part of the screen there, and one press cannot
    /// be both. On the grid, and magnified with the bars on screen, the band is
    /// the bars' as it always was.
    #[test]
    fn the_magnified_band_asks_for_the_viewport_bar_and_not_the_menus() {
        let band = egui::Event::PointerButton {
            pos: Pos2::new(640.0, 12.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        // `band_is_a_door` is false in the one state where the bands coincide.
        assert!(XgViewApp::wall_event(bars(), true, true, &band), "the band is the bars'");
        assert!(
            !XgViewApp::wall_event(bars(), true, false, &band),
            "magnified with the bars away, the band belongs to the viewport's bar"
        );

        // A press on the picture is the picture in either state.
        let picture = egui::Event::PointerButton {
            pos: Pos2::new(640.0, 360.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        assert!(!XgViewApp::wall_event(bars(), true, true, &picture));
        assert!(!XgViewApp::wall_event(bars(), true, false, &picture));
    }

    /// The ring is a cursor for a wall that is being operated. It is off until
    /// the viewer makes an input the first time - a fresh run shows the picture,
    /// not a cursor standing on it - and it stands for [`RING_IDLE`] after one,
    /// on its own clock rather than the bars'.
    #[test]
    fn the_ring_stands_after_an_input_and_no_longer() {
        let now = Instant::now();
        assert!(!XgViewApp::ring_standing(None, now), "nothing has been touched yet");
        // An input a moment ago: the ring stands.
        assert!(XgViewApp::ring_standing(Some(now - Duration::from_secs(1)), now));
        assert!(
            XgViewApp::ring_standing(Some(now - RING_IDLE + Duration::from_millis(100)), now),
            "a moment inside the idle"
        );
        // The idle itself - its own clock, whatever the bars' is set to - and
        // after it: gone.
        assert!(!XgViewApp::ring_standing(Some(now - RING_IDLE), now), "the idle itself");
        assert!(
            !XgViewApp::ring_standing(Some(now - RING_IDLE - Duration::from_secs(1)), now),
            "and after it"
        );
    }

    /// A window of this program owns the ring's two ends as well: its arrows
    /// are not an aim at the picture behind it, and the ring is not drawn while
    /// it is up. Both halves are needed - the first keeps a ring from being
    /// armed inside a form, the second keeps one that was already standing when
    /// the form opened from showing outside it, since a magnified picture's ring
    /// is drawn around its edge and the window does not cover that.
    #[test]
    fn a_window_of_this_program_keeps_the_ring_off_the_picture() {
        let now = Instant::now();
        let arrow_in_a_form = Keys { down: true, ..Default::default() };
        assert!(
            XgViewApp::ring_keys(&arrow_in_a_form, true),
            "on the wall the same key is an aim"
        );
        assert!(!XgViewApp::ring_armed_by(&arrow_in_a_form, true, true), "a form has it");
        assert!(XgViewApp::ring_armed_by(&arrow_in_a_form, true, false));

        let armed = Some(now - Duration::from_secs(1));
        assert!(XgViewApp::ring_visible(armed, false, now));
        assert!(!XgViewApp::ring_visible(armed, true, now), "a window covers it");
        assert!(!XgViewApp::ring_visible(None, false, now));
    }

    /// The bars are the ring's only connection, and it is at the two ends of
    /// their life: they arm it as they appear and take it with them as they go.
    /// Neither of them is a gate the ring is drawn behind - a wall driven with
    /// the bars away must keep its ring, or the one case the ring exists for is
    /// the one it cannot be drawn in.
    #[test]
    fn the_bars_arm_the_ring_as_they_appear_and_take_it_as_they_go() {
        let now = Instant::now();
        let used = Some(now - Duration::from_secs(1));
        let follows = XgViewApp::ring_follows_bars;

        // Coming up: armed, whatever the ring was doing before.
        assert_eq!(follows(used, true, false, now), Some(now), "the bars appeared");
        assert_eq!(follows(None, true, false, now), Some(now), "the bars appeared");
        // Going: taken away with them, however recently it was armed.
        assert_eq!(follows(used, false, true, now), None, "the bars went");
        assert_eq!(follows(None, false, true, now), None, "the bars went");
        // Either of them on its own: the ring keeps its own clock. In full
        // screen the bars are away most of the time and the wall is still being
        // driven, which is exactly this case.
        assert_eq!(follows(used, false, false, now), used, "the bars were away and stay away");
        assert_eq!(follows(used, true, true, now), used, "the bars were up and stay up");
        assert_eq!(follows(None, false, false, now), None, "the bars were away and stay away");
    }

    /// The picture's own surfaces are not the menus: the recording's strip and
    /// the save panel it opens. Asking for the bars from either is visible
    /// damage - they take their height from the wall, so the picture shrinks
    /// under the form the viewer is filling in.
    #[test]
    fn a_press_on_the_pictures_own_surfaces_does_not_ask_for_the_bars() {
        let control = crate::tilebar::control(0);
        let elsewhere = Id::new("a tile");
        let surface = XgViewApp::on_picture_surface;

        // The save panel: any focus at all inside it is the panel's, and a
        // frame in which it has not asked for a control yet counts too.
        assert!(surface(false, true, Some(control), false), "the save panel is up");
        assert!(surface(false, true, None, false), "the save panel, before its first focus");
        // The strip: only its own controls, or the strip itself.
        assert!(surface(true, false, Some(control), false), "a control of the strip");
        assert!(surface(true, false, None, true), "the strip has the focus");
        // A strip that is merely up leaves the remote on the wall.
        assert!(!surface(true, false, Some(elsewhere), false), "a strip that is only drawn");
        assert!(!surface(true, false, None, false), "nothing focused at all");
        // Nothing of the picture's is open: the bars are the viewer's to ask
        // for, by the keys `menus_input` and `wall_event` name.
        assert!(!surface(false, false, Some(elsewhere), false), "the menus themselves");
    }

    /// What arms the ring is a key that walks it, and never a key that changes
    /// what the wall shows: a page turn and a layout choice are the same class as
    /// a swipe and the toolbar's page arrows - they move the picture, not the
    /// ring - so all three ways of turning a page behave alike. The pointer arms
    /// it with the one action that places it, a click on a tile, which is tested
    /// where that click is handled (`paint_view`).
    #[test]
    fn the_cursor_keys_arm_the_ring_and_the_view_keys_do_not() {
        let keys = |set: fn(&mut Keys), wall_owns: bool| {
            let mut keys = Keys::default();
            set(&mut keys);
            XgViewApp::ring_keys(&keys, wall_owns)
        };
        let armed = |set: fn(&mut Keys)| keys(set, true);
        let on_a_control = |set: fn(&mut Keys)| keys(set, false);

        assert!(armed(|k| k.left = true), "the arrows walk it");
        assert!(armed(|k| k.right = true));
        assert!(armed(|k| k.up = true));
        assert!(armed(|k| k.down = true));
        assert!(armed(|k| k.enter = true), "on the wall, Enter acts on what it is on");
        assert!(armed(|k| k.back = true), "Back unwinds to where it was");
        assert!(armed(|k| k.settings = true), "F1 opens the bars and carries it in");
        assert!(armed(|k| k.devices = true), "F2 likewise");

        // Enter on a control activates the control instead - which is how a
        // remote presses a page button, and that is a page turn rather than an
        // aim. The arrows walk the ring there as much as anywhere.
        assert!(!on_a_control(|k| k.enter = true), "Enter on a control activates the control");
        assert!(on_a_control(|k| k.left = true), "the arrows walk it there too");

        assert!(!armed(|k| k.page_prev = true), "a page turn moves the picture, not the ring");
        assert!(!armed(|k| k.page_next = true), "a page turn moves the picture, not the ring");
        assert!(!armed(|k| k.layout[2] = true), "so does a layout choice");
        assert!(!armed(|k| k.fullscreen = true), "and full screen takes it away with the bars");
        assert!(!armed(|_| {}), "nothing pressed arms nothing");
    }

    /// A page key turns the page from wherever the focus is, but only a focus
    /// that was standing on a page button follows the turn at the edge into the
    /// other one - the turn is what took the button it stood on away. A tile's
    /// focus, or a focus on anything else in the bars, turns the page and stays
    /// where it was put, so a page key never hands the ring to the toolbar.
    #[test]
    fn only_a_focus_on_a_page_button_follows_a_page_key_past_the_edge() {
        let on = XgViewApp::focus_on_page_button;
        let prev = Id::new("toolbar-prev-page");
        let next = Id::new("toolbar-next-page");

        assert!(on(Some(prev), Some(prev), Some(next)), "the previous button");
        assert!(on(Some(next), Some(prev), Some(next)), "the next button");
        assert!(!on(Some(Id::new("toolbar-settings")), Some(prev), Some(next)), "another control");
        assert!(!on(Some(Id::new("tile-3")), Some(prev), Some(next)), "a tile");
        assert!(!on(None, Some(prev), Some(next)), "nothing has the focus");

        // Upright, the bar's own pair of ids answers the same way - and the
        // landscape pair, which is not what those buttons were drawn with, does
        // not.
        let upright_prev = Id::new("portrait-prev-page");
        let upright_next = Id::new("portrait-next-page");
        assert!(on(Some(upright_next), Some(upright_prev), Some(upright_next)));
        assert!(!on(Some(prev), Some(upright_prev), Some(upright_next)), "not this bar's");
    }

    /// A change of the wall's cursor takes down a ring that is showing, whichever
    /// way it was moved and whether or not the frame aimed: a page turned, a
    /// layout that re-places the focus under the viewer, or a camera switched off
    /// that clamps it. The one frame it leaves alone is the one that aimed, since
    /// an arrow walking off the edge of the wall turns the page itself and the
    /// ring follows it in.
    #[test]
    fn a_cursor_moved_for_the_viewer_takes_the_ring_down() {
        let now = Instant::now();
        let showing = Some(now - Duration::from_secs(1));
        let follows = XgViewApp::ring_follows_cursor;
        let cursor = (0, Some(3));

        assert_eq!(follows(showing, (1, Some(0)), cursor, false), None, "the page turned");
        assert_eq!(follows(showing, (0, Some(2)), cursor, false), None, "the layout re-placed the focus");
        assert_eq!(follows(showing, (0, None), cursor, false), None, "the focus was clamped away");
        assert_eq!(follows(None, (1, Some(0)), cursor, false), None, "nothing showing to take down");
        assert_eq!(follows(showing, (1, Some(0)), cursor, true), showing, "an arrow walked off the edge");
        assert_eq!(follows(showing, cursor, cursor, false), showing, "no move, no change");
        assert_eq!(follows(showing, (0, Some(2)), cursor, true), showing, "an aim keeps it wherever it went");
    }

    /// The layout picker's nine thumbnails are three rows of three, and the
    /// arrows have to reach all of them: Left / Right walk a row, Up / Down
    /// cross between the rows at the same column. This is the wiring the picker
    /// draws against.
    #[test]
    fn the_layout_picker_walks_its_three_rows() {
        let ids: Vec<Id> = (0..9).map(|slot| Id::new(("layout-picker", slot))).collect();
        let mut nav = Nav::new(&[
            ("layout-picker-row-0", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::ScopeAt("layout-picker-row-1"))),
            ("layout-picker-row-1", ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::ScopeAt("layout-picker-row-0")).exit(Dir::Down, Exit::ScopeAt("layout-picker-row-2"))),
            ("layout-picker-row-2", ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::ScopeAt("layout-picker-row-1"))),
        ]);
        let ctx = egui::Context::default();
        nav.begin();
        for (row, scope) in LAYOUT_PICKER_ROWS.iter().enumerate() {
            nav.open(scope);
            for slot in row * 3..row * 3 + 3 {
                nav.item_id(ids[slot]);
            }
            nav.close();
        }
        nav.finish(&ctx);

        // Sideways walks the row and stops at its ends.
        assert_eq!(nav.step(ids[0], Dir::Right), Some(ids[1]));
        assert_eq!(nav.step(ids[1], Dir::Right), Some(ids[2]));
        assert_eq!(nav.step(ids[2], Dir::Right), None, "the end of the row");
        assert_eq!(nav.step(ids[0], Dir::Left), None);
        // Down and Up cross between the rows, landing on the same column.
        assert_eq!(nav.step(ids[1], Dir::Down), Some(ids[4]));
        assert_eq!(nav.step(ids[4], Dir::Down), Some(ids[7]));
        assert_eq!(nav.step(ids[7], Dir::Down), None, "the last row");
        assert_eq!(nav.step(ids[4], Dir::Up), Some(ids[1]));
        assert_eq!(nav.step(ids[1], Dir::Up), None, "the first row has no exit");
        // The left and right columns keep their column too.
        assert_eq!(nav.step(ids[2], Dir::Down), Some(ids[5]));
        assert_eq!(nav.step(ids[0], Dir::Down), Some(ids[3]));
    }

    /// The picker's thumbnails are drawn inside an `egui::Modal`, and the
    /// hand-off asks for the focus on the first of them. This reproduces that
    /// shape headlessly: the focus has to land on a thumbnail and stay there,
    /// or the arrows have nothing to walk.
    #[test]
    fn a_modal_thumbnail_keeps_the_focus_the_handoff_gave_it() {
        let ctx = egui::Context::default();
        let mut nav = Nav::new(&[
            ("layout-picker-row-0", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope("layout-picker-row-1"))),
            ("layout-picker-row-1", ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::Scope("layout-picker-row-0")).exit(Dir::Down, Exit::Scope("layout-picker-row-2"))),
            ("layout-picker-row-2", ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::Scope("layout-picker-row-1"))),
        ]);
        let mut anchor: Option<Id> = None;
        let mut focused_after: Option<Id> = None;

        for frame in 0..3 {
            nav.begin();
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 720.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::Modal::new(Id::new("xgview-layout-picker")).show(ctx, |ui| {
                    ui.set_width(360.0);
                    for (row, scope) in LAYOUT_PICKER_ROWS.iter().enumerate() {
                        ui.horizontal(|ui| {
                            nav.open(scope);
                            for _ in row * 3..row * 3 + 3 {
                                let (_, response) =
                                    ui.allocate_exact_size(vec2(108.0, 84.0), Sense::click());
                                if anchor.is_none() {
                                    anchor = Some(response.id);
                                }
                                nav.tracked(response);
                            }
                            nav.close();
                        });
                    }
                });
                // The hand-off, as `update` does it: once, on the frame the
                // picker appears.
                if frame == 0 {
                    if let Some(id) = anchor {
                        ctx.memory_mut(|memory| memory.request_focus(id));
                    }
                }
                focused_after = ctx.memory(|memory| memory.focused());
            });
            nav.finish(&ctx);
        }

        let focused = focused_after.expect("a thumbnail has the focus");
        assert_eq!(Some(focused), anchor, "the focus stayed on the first thumbnail");
        assert!(nav.owns(focused), "the focused thumbnail is on the navigation layer");
    }

    /// A printable key arrives as its own event *as well as* a key event, and
    /// the grid digits are printable: read as text, choosing a grid would put
    /// the bars back - which is what the digits are not supposed to do.
    #[test]
    fn the_text_half_of_a_printable_key_does_not_ask_for_the_bars() {
        assert!(!XgViewApp::wall_event(bars(), true, true, &egui::Event::Text("3".to_owned())));
    }

    /// A press on the wall is the wall's, even when a control holds the focus.
    ///
    /// Waking the bars for it moves the wall under the very click that placed it
    /// - the bars take their height from the wall - so the press lands on another
    /// tile or on none, and the second press of a double click never reaches the
    /// first. A press in the band a bar occupies is still the pointer asking for
    /// it, and so is a key.
    #[test]
    fn a_press_on_the_wall_does_not_ask_for_the_bars() {
        let read = |events: Vec<egui::Event>| {
            let ctx = egui::Context::default();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1280.0, 720.0))),
                    events,
                    ..Default::default()
                },
                |_| {},
            );
            ctx.input(|input| XgViewApp::menus_input(input, bars()))
        };
        let press = |at: egui::Pos2| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        assert!(!read(vec![press(pos2(600.0, 360.0))]), "a tile in the middle is the wall's");
        assert!(read(vec![press(pos2(600.0, 18.0))]), "the band above is the bar's");
        assert!(read(vec![press(pos2(600.0, 705.0))]), "and so is the band below");
        assert!(
            !read(vec![egui::Event::PointerMoved(pos2(600.0, 360.0))]),
            "a movement asks for nothing"
        );
        let key = egui::Event::Key {
            key: egui::Key::F1,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert!(read(vec![key]), "a key is the menus'");
    }

    /// A band drawn on one screen is a band at the top and bottom of the next
    /// one, not a rectangle where it used to be.
    ///
    /// The rectangles are kept from the frame the bars were last drawn in, and a
    /// window made full screen since holds bands from the window it was: the
    /// bottom one lands in the middle of the screen, where it eats every press
    /// meant for a tile and wakes the bars for it.
    #[test]
    fn a_band_from_another_screen_is_not_a_band_in_this_one() {
        // Drawn when the window was 1280 x 720, asked about a 1920 x 1080 screen.
        let drawn = Some((
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 36.0)),
            Rect::from_min_max(pos2(0.0, 690.0), pos2(1280.0, 720.0)),
        ));
        let screen = Rect::from_min_max(pos2(0.0, 0.0), pos2(1920.0, 1080.0));
        let (top, bottom) = XgViewApp::bands(drawn, screen).expect("two bands");
        assert_eq!(top, Rect::from_min_max(pos2(0.0, 0.0), pos2(1920.0, 36.0)));
        assert_eq!(bottom, Rect::from_min_max(pos2(0.0, 1050.0), pos2(1920.0, 1080.0)));
        assert!(
            !XgViewApp::in_a_bar_band(Some((top, bottom)), pos2(960.0, 700.0)),
            "a press in the middle of the taller screen is the wall's"
        );
        assert!(XgViewApp::in_a_bar_band(Some((top, bottom)), pos2(960.0, 20.0)), "and the band above");
    }

    /// A tile is clicked and dragged but *not* focusable, and a double press on
    /// one is the way into the picture. That has to hold for a widget egui does
    /// not put in its focus walk - it is the whole of the wall's gesture.
    #[test]
    fn a_double_press_on_a_tile_is_a_double_press() {
        let ctx = egui::Context::default();
        let id = Id::new("a-tile");
        let at = pos2(200.0, 200.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let mut doubles = 0;
        for frame in 0..6 {
            let events = match frame {
                1 => vec![egui::Event::PointerMoved(at), button(true)],
                2 => vec![button(false)],
                3 => vec![button(true)],
                4 => vec![button(false)],
                _ => Vec::new(),
            };
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let rect = Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 400.0));
                    // The tile's own sense: clickable, draggable, not focusable.
                    let response = ui.interact(rect, id, Sense::CLICK | Sense::DRAG);
                    if response.double_clicked() {
                        doubles += 1;
                    }
                });
            });
        }
        assert_eq!(doubles, 1, "one double press, on a widget that cannot be focused");
    }

    /// A tap on a touch screen reaches egui as a `Start` and an `End` at the
    /// same place. Reading the end as a request for the bars - which is what the
    /// events an integration does not name fall through to - brought them back
    /// under every touch on the wall, however carefully the start was placed.
    #[test]
    fn both_halves_of_a_touch_are_placed_by_where_they_are() {
        for phase in [egui::TouchPhase::Start, egui::TouchPhase::End] {
            let at = |y: f32| egui::Event::Touch {
                device_id: egui::TouchDeviceId(0),
                id: egui::TouchId(0),
                phase,
                pos: Pos2::new(640.0, y),
                force: None,
            };
            assert!(!XgViewApp::wall_event(bars(), true, true, &at(360.0)), "{phase:?} on the wall");
            assert!(XgViewApp::wall_event(bars(), true, true, &at(12.0)), "{phase:?} in the toolbar band");
        }
    }

    /// The magnified picture's pinch arrives from two kinds of device: fingers
    /// that are counted and spread, and a trackpad's zoom events, which have no
    /// fingers in them at all. Both are read here, and the ratio is what the rest
    /// of the wall pushes the picture in by.
    #[test]
    fn a_pinch_is_read_from_the_fingers_and_from_a_trackpad() {
        let touch = |phase, id, x| egui::Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: egui::TouchId(id),
            phase,
            pos: pos2(x, 300.0),
            force: None,
        };
        let frame = |ctx: &egui::Context, events: Vec<egui::Event>| {
            let _ = ctx.run(egui::RawInput { events, ..Default::default() }, |_| {});
        };
        let wall = Rect::from_min_size(pos2(0.0, 40.0), vec2(1280.0, 640.0));
        let read = |ctx: &egui::Context| ctx.input(|input| pinch_frame(input, wall));

        // Two fingers land. The frame that places the pointer is the one before
        // the gesture can be measured, because the touch state reads where the
        // pointer was as the frame opened.
        let ctx = egui::Context::default();
        frame(
            &ctx,
            vec![
                touch(egui::TouchPhase::Start, 0, 400.0),
                touch(egui::TouchPhase::Start, 1, 600.0),
                egui::Event::PointerMoved(pos2(400.0, 300.0)),
            ],
        );
        assert!(read(&ctx).is_none(), "one frame is not a gesture");
        // The first frame of it measures nothing - it is the distance the rest
        // are measured against - and the two fingers' middle is where it is
        // aimed, which is the point the picture is pushed in about.
        frame(
            &ctx,
            vec![touch(egui::TouchPhase::Move, 0, 300.0), touch(egui::TouchPhase::Move, 1, 700.0)],
        );
        let (delta, at) = read(&ctx).expect("two fingers are a pinch");
        assert!((delta - 1.0).abs() < 0.01, "nothing to compare with yet: {delta}");
        assert_eq!(at, pos2(500.0, 300.0), "aimed between the fingers");
        // 200 points apart and then 250: half of the fingers' travel is the mean
        // distance from their middle.
        frame(
            &ctx,
            vec![touch(egui::TouchPhase::Move, 0, 250.0), touch(egui::TouchPhase::Move, 1, 750.0)],
        );
        let (delta, at) = read(&ctx).expect("still spreading is still a pinch");
        assert!((delta - 1.25).abs() < 0.01, "the frames' ratio: {delta}");
        assert_eq!(at, pos2(500.0, 300.0), "and it is still aimed there");
        // With the fingers gone there is no gesture to read.
        frame(
            &ctx,
            vec![touch(egui::TouchPhase::End, 0, 250.0), touch(egui::TouchPhase::End, 1, 750.0)],
        );
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(500.0, 300.0))]);
        assert!(read(&ctx).is_none(), "a frame with no fingers is not a gesture");

        // A trackpad sends no fingers: the zoom event is the gesture, aimed
        // where the pointer is.
        let ctx = egui::Context::default();
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(120.0, 400.0)), egui::Event::Zoom(1.2)]);
        let (delta, at) = read(&ctx).expect("a zoom event is a pinch");
        assert!((delta - 1.2).abs() < 0.01, "the event's own factor: {delta}");
        assert_eq!(at, pos2(120.0, 400.0), "aimed at the pointer");
        // The same event with the pointer over a bar belongs to the bar: the
        // picture behind it is not what is being pushed in.
        let band = Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 40.0));
        assert!(
            ctx.input(|input| pinch_frame(input, band)).is_none(),
            "a gesture over a bar is the bar's"
        );
    }

    /// The plain wheel pushes the picture in where the pointer is, and it is read
    /// only over the wall: a wheel turned where the interface scrolls, and the one
    /// turned with the zoom modifier - which egui folds into the frame's zoom
    /// factor and [`pinch_frame`] reads - are not the picture's to answer.
    #[test]
    fn the_plain_wheel_pushes_the_picture_in_where_the_pointer_is() {
        let wall = Rect::from_min_size(pos2(0.0, 40.0), vec2(1280.0, 640.0));
        let band = Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 40.0));
        let wheel = |delta, modifiers| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta,
            modifiers,
        };
        let frame = |ctx: &egui::Context, events: Vec<egui::Event>, modifiers: Modifiers| {
            let _ = ctx.run(egui::RawInput { events, modifiers, ..Default::default() }, |_| {});
        };
        let read = |ctx: &egui::Context, area: Rect| {
            let speed = ctx.options(|options| options.input_options.scroll_zoom_speed);
            ctx.input(|input| wheel_frame(input, speed, area))
        };

        // A notch forward, with the pointer over the wall: the picture is pushed
        // in, about where the pointer is.
        let ctx = egui::Context::default();
        frame(
            &ctx,
            vec![egui::Event::PointerMoved(pos2(900.0, 500.0)), wheel(vec2(0.0, 1.0), Modifiers::NONE)],
            Modifiers::NONE,
        );
        let (delta, at) = read(&ctx, wall).expect("a wheel over the wall is a zoom");
        assert!(delta > 1.0, "a wheel forward pushes the picture in: {delta}");
        assert_eq!(at, pos2(900.0, 500.0), "aimed at the pointer");
        // The same wheel with the pointer over a bar is the bar's.
        assert!(read(&ctx, band).is_none(), "a wheel over a bar scrolls the bar");

        // A notch back pulls the picture out again, which is how it is unwound.
        let ctx = egui::Context::default();
        frame(
            &ctx,
            vec![egui::Event::PointerMoved(pos2(900.0, 500.0)), wheel(vec2(0.0, -1.0), Modifiers::NONE)],
            Modifiers::NONE,
        );
        let (delta, _) = read(&ctx, wall).expect("a wheel back is a zoom too");
        assert!(delta < 1.0, "a wheel back pulls the picture out: {delta}");

        // Turned with the zoom modifier it is the other kind of wheel, and this
        // reader leaves it alone: `pinch_frame` has it.
        let ctx = egui::Context::default();
        frame(
            &ctx,
            vec![
                egui::Event::PointerMoved(pos2(900.0, 500.0)),
                wheel(vec2(0.0, 1.0), Modifiers::COMMAND),
            ],
            Modifiers::COMMAND,
        );
        assert!(read(&ctx, wall).is_none(), "the zoom-modified wheel is not this one");
    }

    /// A wheel and a trackpad's pinch are the viewer at the picture rather than a
    /// request for the bars: in full screen a zoom is done on the magnified
    /// viewport, and bringing the bars back over it would be the one thing the
    /// viewer did not ask for. The wall's own keys and a swipe are read the same
    /// way.
    #[test]
    fn a_wheel_does_not_ask_for_the_bars_however_it_is_turned() {
        let wheel = |modifiers| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: vec2(0.0, 1.0),
            modifiers,
        };
        assert!(!XgViewApp::wall_event(bars(), true, true, &wheel(Modifiers::NONE)), "the plain wheel");
        assert!(!XgViewApp::wall_event(bars(), true, true, &wheel(Modifiers::COMMAND)), "the zooming one");
        assert!(!XgViewApp::wall_event(bars(), true, true, &egui::Event::Zoom(1.2)), "a trackpad's pinch");
    }

    /// Moving the magnified picture is the pointer's own movement, and only from
    /// a press that began on the wall: a press that began on a bar belongs to the
    /// bar, and two fingers are a pinch - the pointer follows the first of them,
    /// so what it reports is not a drag with which to move the picture.
    #[test]
    fn only_a_press_that_began_on_the_wall_drags_the_picture() {
        let area = Rect::from_min_size(pos2(0.0, 40.0), vec2(1280.0, 640.0));
        let frame = |ctx: &egui::Context, events: Vec<egui::Event>| {
            let _ = ctx.run(egui::RawInput { events, ..Default::default() }, |_| {});
        };
        let read = |ctx: &egui::Context| ctx.input(|input| drag_frame(input, area));
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };

        let ctx = egui::Context::default();
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(640.0, 360.0))]);
        frame(&ctx, vec![button(pos2(640.0, 360.0), true)]);
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(650.0, 360.0))]);
        assert_eq!(read(&ctx), Some(vec2(10.0, 0.0)), "the press is on the wall");
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(640.0, 380.0))]);
        assert_eq!(read(&ctx), Some(vec2(-10.0, 20.0)), "and it keeps dragging");
        frame(&ctx, vec![button(pos2(640.0, 380.0), false)]);
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(700.0, 380.0))]);
        assert_eq!(read(&ctx), None, "a released pointer drags nothing");

        // A finger landing somewhere else is not travel: egui measures a frame
        // from the position the pointer was last *seen* at, and between two
        // touches that is where the last finger left the glass - so the frame a
        // finger lands on would report the distance between the two, and the
        // picture jumped by it.
        frame(&ctx, vec![button(pos2(120.0, 200.0), true)]);
        assert_eq!(read(&ctx), None, "landing is not dragging, however far away it landed");
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(130.0, 200.0))]);
        assert_eq!(read(&ctx), Some(vec2(10.0, 0.0)), "and the drag starts from where it landed");
        frame(&ctx, vec![button(pos2(130.0, 200.0), false)]);

        // A press that began on a bar, and moved over the wall, is the bar's.
        frame(&ctx, vec![button(pos2(640.0, 10.0), true)]);
        frame(&ctx, vec![egui::Event::PointerMoved(pos2(700.0, 360.0))]);
        assert_eq!(read(&ctx), None, "the press began in the toolbar band");
        frame(&ctx, vec![button(pos2(700.0, 360.0), false)]);

        // And a picture with two fingers on it is being pushed in, not moved:
        // the pointer follows the first finger, and the first finger is one of
        // them.
        let ctx = egui::Context::default();
        let touch = |phase, id, x| egui::Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: egui::TouchId(id),
            phase,
            pos: pos2(x, 300.0),
            force: None,
        };
        frame(
            &ctx,
            vec![
                touch(egui::TouchPhase::Start, 0, 400.0),
                touch(egui::TouchPhase::Start, 1, 600.0),
                egui::Event::PointerMoved(pos2(400.0, 300.0)),
            ],
        );
        frame(
            &ctx,
            vec![touch(egui::TouchPhase::Move, 0, 410.0), touch(egui::TouchPhase::Move, 1, 590.0)],
        );
        frame(&ctx, vec![button(pos2(400.0, 300.0), true), egui::Event::PointerMoved(pos2(500.0, 350.0))]);
        assert_eq!(read(&ctx), None, "a pinch is not a drag");
    }

    /// The keys that reach the menus are exactly the ones that are not the
    /// wall's - the bars are the only way to the settings, the device window and
    /// full screen, and a remote control has neither of the others, so Back
    /// belongs with them.
    #[test]
    fn menu_keys_still_ask_for_the_bars() {
        for key in [Key::F1, Key::F2, Key::F11] {
            assert!(!XgViewApp::wall_key(key), "{key:?} opens the menus");
            assert!(XgViewApp::wall_event(bars(), true, true, &press(key)), "{key:?} opens the menus");
        }
    }

    /// Back is two things, and which one it is decides whether it asks for the
    /// bars: the wall's own key while there is something on the wall for it to
    /// unwind, and the remote's door to the menus when there is not. It is the
    /// only key a remote has for that - a remote has no function keys.
    #[test]
    fn back_asks_for_the_bars_only_when_there_is_nothing_to_unwind() {
        for key in [Key::Escape, Key::BrowserBack] {
            assert!(
                XgViewApp::wall_event(bars(), true, true, &press(key)),
                "{key:?} with nothing to unwind is the door to the menus"
            );
            assert!(
                !XgViewApp::wall_event(bars(), false, true, &press(key)),
                "{key:?} with something to unwind is the wall's, and leaves the bars alone"
            );
        }
    }

    /// Moving the mouse is not a request for anything - the pointer's way in is
    /// a press in the band a bar occupies.
    #[test]
    fn moving_the_mouse_does_not_ask_for_the_bars() {
        assert!(!XgViewApp::wall_event(bars(), true, true, &egui::Event::MouseMoved(egui::vec2(4.0, 0.0))));
    }

    #[test]
    fn band_overlay_area_is_the_band() {
        // A phone in landscape is only a few hundred points tall, which is
        // where an area left to its own default size grew over the whole wall.
        let screen = Rect::from_min_max(pos2(0.0, 0.0), pos2(851.0, 393.0));
        let top = Rect::from_min_max(pos2(0.0, 0.0), pos2(851.0, 36.0));
        let bottom = Rect::from_min_max(pos2(0.0, 363.0), pos2(851.0, 393.0));
        let ctx = egui::Context::default();
        let run = |with_bands: bool| -> Vec<(String, Rect)> {
            let mut last = Vec::new();
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let _ = ctx.run(input, |ctx| {
                last.clear();
                if with_bands {
                    for (id, band) in [("xgview-toolbar-band", top), ("xgview-status-band", bottom)] {
                        let r = egui::Area::new(Id::new(id))
                            .order(egui::Order::Middle)
                            .fixed_pos(band.min)
                            .default_size(band.size())
                            .show(ctx, |ui| {
                                ui.allocate_rect(band, egui::Sense::click());
                            });
                        last.push((id.to_owned(), r.response.rect));
                    }
                }
            });
            last
        };
        for _ in 0..3 {
            run(false);
        }
        for _ in 0..3 {
            run(true);
        }
        for (id, rect) in run(true) {
            let band = if id.contains("toolbar") { top } else { bottom };
            assert_eq!(rect, band, "{id} overlay must be exactly its band");
        }
    }

    /// A key going down, as egui reports it.
    fn press(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// The bar's strip is the picture edge in its own frame: the screen's top
    /// and bottom in landscape, its right and left once the picture is turned.
    #[test]
    fn a_gesture_starting_in_the_bars_strip_is_the_bars() {
        let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 720.0));
        assert!(in_tile_bar_strip(pos2(640.0, 40.0), area, false, tilebar::Dock::Top));
        assert!(!in_tile_bar_strip(pos2(640.0, 200.0), area, false, tilebar::Dock::Top));
        assert!(in_tile_bar_strip(pos2(640.0, 680.0), area, false, tilebar::Dock::Bottom));
        assert!(!in_tile_bar_strip(pos2(640.0, 520.0), area, false, tilebar::Dock::Bottom));
        assert!(in_tile_bar_strip(pos2(1240.0, 360.0), area, true, tilebar::Dock::Top));
        assert!(!in_tile_bar_strip(pos2(1000.0, 360.0), area, true, tilebar::Dock::Top));
        assert!(in_tile_bar_strip(pos2(40.0, 360.0), area, true, tilebar::Dock::Bottom));
        assert!(!in_tile_bar_strip(pos2(280.0, 360.0), area, true, tilebar::Dock::Bottom));
    }

    /// The numbers are read from a store rather than delivered, so the same
    /// sample can be read again after a later event: applying it twice must not
    /// undo what the event set. A switch keeps the last frame of the stream it
    /// is leaving - which the numbers of that stream, applied again, would let
    /// go of.
    #[test]
    fn a_sample_read_twice_does_not_undo_a_switch() {
        let sub = StreamStats {
            index: 0,
            camera_id: "cam".to_string(),
            stream: StreamKind::Sub,
            codec: Some("H264".to_string()),
            width: Some(640),
            height: Some(360),
            hardware: false,
            audio: false,
            fps: 25.0,
            bitrate_kbps: 512.0,
            total_frames: 900,
        };
        let mut channel = ChannelUi::default();
        channel.apply_stats(7, &sub);
        assert_eq!(channel.fps, 25.0);

        channel.apply(StreamEvent::Transition {
            index: 0,
            camera_id: "cam".to_string(),
            from: Some(StreamKind::Sub),
            to: StreamKind::Main,
        });
        assert_eq!(channel.switching_from, Some(StreamKind::Sub), "the sub stream's frame is held");
        assert_eq!(channel.fps, 0.0, "a stream that has not delivered yet has no rate");

        channel.apply_stats(7, &sub);
        assert_eq!(channel.switching_from, Some(StreamKind::Sub), "the held frame is still held");
        assert_eq!(channel.fps, 0.0);
        assert_eq!(channel.stream, Some(StreamKind::Main), "and the channel is still on the new stream");
    }

    /// A Main session names its size before its first picture, and the Sub
    /// frame held while it opens needs that shape. The channel's own width and
    /// height go back to describing Sub in the grid, but the Main shape is not
    /// forgotten.
    #[test]
    fn a_main_streams_size_survives_the_return_to_sub() {
        let main = StreamStats {
            index: 0,
            camera_id: "cam".to_string(),
            stream: StreamKind::Main,
            codec: Some("H264".to_string()),
            width: Some(1920),
            height: Some(1080),
            hardware: true,
            audio: false,
            fps: 0.0,
            bitrate_kbps: 0.0,
            total_frames: 0,
        };
        let mut sub = main.clone();
        sub.stream = StreamKind::Sub;
        sub.width = Some(640);
        sub.height = Some(480);

        let mut channel = ChannelUi::default();
        channel.apply_stats(1, &sub);
        assert_eq!(channel.main_size, None, "a sub stream names no main shape");

        channel.apply_stats(2, &main);
        assert_eq!(channel.main_size, Some((1920, 1080)));

        channel.apply_stats(3, &sub);
        assert_eq!(channel.width, Some(640), "the current numbers are sub's again");
        assert_eq!(channel.main_size, Some((1920, 1080)), "but the main shape is kept");
    }

    /// The held frame is a promise that a new one is coming. A channel that has
    /// given up cannot keep it, and the band that says so would otherwise stay
    /// over the failure panel - the one thing on the tile still claiming that
    /// something is on its way.
    #[test]
    fn a_channel_that_gives_up_stops_holding_the_last_frame() {
        let mut channel = ChannelUi::default();
        channel.apply(StreamEvent::Transition {
            index: 0,
            camera_id: "cam".to_string(),
            from: Some(StreamKind::Sub),
            to: StreamKind::Main,
        });
        assert_eq!(channel.switching_from, Some(StreamKind::Sub));

        // A retry is still a wait for the picture, so the frame is held.
        channel.apply(StreamEvent::State {
            index: 0,
            camera_id: "cam".to_string(),
            state: ConnectionState::Reconnecting,
            detail: "connection refused | retry in 1000 ms".to_string(),
        });
        assert_eq!(channel.switching_from, Some(StreamKind::Sub), "a retry still expects a picture");

        // A channel that has stopped asking is not waiting for anything.
        channel.apply(StreamEvent::State {
            index: 0,
            camera_id: "cam".to_string(),
            state: ConnectionState::Failed,
            detail: "SETUP failed: 401 Unauthorized (digest authentication)".to_string(),
        });
        assert_eq!(channel.switching_from, None, "nothing is coming to replace it");
    }

    /// A picture that fills the screen leaves no bands; a letterboxed one leaves
    /// the two beside it, which are what the overlay has to paint.
    #[test]
    fn bands_beside_a_letterboxed_picture() {
        let screen = Rect::from_min_max(pos2(0.0, 0.0), pos2(800.0, 400.0));
        assert!(bands_around(screen, screen).is_empty(), "stretched: nothing beside it");

        let fitted = Rect::from_min_max(pos2(100.0, 0.0), pos2(700.0, 400.0));
        let bands = bands_around(screen, fitted);
        assert_eq!(bands.len(), 2, "one band each side, none above or below");
        assert_eq!(bands[0], Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 400.0)));
        assert_eq!(bands[1], Rect::from_min_max(pos2(700.0, 0.0), pos2(800.0, 400.0)));

        // A picture taller than the screen is the other pair of bands, and one
        // pushed in past the screen edge gives the corner rectangles back too.
        let tall = Rect::from_min_max(pos2(0.0, 50.0), pos2(800.0, 350.0));
        assert_eq!(bands_around(screen, tall).len(), 2);
        let pushed = Rect::from_min_max(pos2(-200.0, -100.0), pos2(1000.0, 500.0));
        assert!(bands_around(screen, pushed).is_empty(), "the picture covers the screen");
    }

    /// The two focuses that are not the navigation layer's, but are still the
    /// viewport's: the seat the bar opens on, and the strip under a recording.
    #[test]
    fn the_viewport_keeps_its_seat_and_its_strip() {
        let strip = Id::new("strip");
        assert!(viewport_keeps_focus(tilebar::seat(), true, true, Some(strip)));
        assert!(viewport_keeps_focus(strip, true, true, Some(strip)));
        assert!(
            !viewport_keeps_focus(tilebar::seat(), false, true, Some(strip)),
            "a closed bar must not keep a focus on its seat"
        );
        assert!(!viewport_keeps_focus(strip, true, false, Some(strip)), "no strip on screen");
        assert!(
            !viewport_keeps_focus(strip, false, true, Some(strip)),
            "a closed bar keeps neither its seat nor its strip"
        );
        assert!(!viewport_keeps_focus(Id::new("toolbar"), true, true, Some(strip)));
        assert!(!viewport_keeps_focus(Id::new("other"), true, true, None));
    }

    /// Both jobs walk the same window, so it has to be drawn for whichever one
    /// is up. The call once read `Some(FileJob::Import)`, and that is what an
    /// export ran into: the job was set, the keyboard was handed to the picker
    /// by `file_dialog_keys`, and the window it belonged to was never drawn - so
    /// the settings page answered an export press with nothing at all.
    #[test]
    fn the_picker_is_drawn_for_an_export_too() {
        let source = include_str!("mod.rs");
        let call = source.find("self.draw_file_picker(ctx);").unwrap();
        let gate = &source[call - 700..call];
        assert!(
            gate.contains("if self.file_job.is_some()"),
            "the picker is up for whatever job was asked for"
        );
        assert!(
            !gate.contains("FileJob::Import)"),
            "an import-only gate leaves an export with no window and a swallowed keyboard"
        );
    }

    /// The seat's first arrow is answered in `handle_keys`, before either the
    /// controls branch or the grid branch can return. Moved back into
    /// `keys_off_the_grid` it would run only on frames the wall does not own
    /// the keys - exactly the frames the old code missed.
    #[test]
    fn the_seats_first_arrow_is_answered_before_either_branch() {
        let source = include_str!("input.rs");
        let handle = source.find("fn handle_keys(").unwrap();
        let parsed = source.find("let keys = Self::parse_keys(").unwrap();
        let seat = source.find("tilebar::first_arrow_target(").unwrap();
        assert!(handle < parsed && parsed < seat, "the rule belongs in handle_keys, after parse_keys");
        assert_eq!(source.matches("tilebar::first_arrow_target(").count(), 1);
        let off_start = source.find("fn keys_off_the_grid(").unwrap();
        let off_end = off_start + source[off_start..].find("fn handle_keys(").unwrap();
        assert!(
            !source[off_start..off_end].contains("first_arrow_target"),
            "the rule grew a second copy behind the old branch"
        );
    }

    /// The bar answers the confirm key by hand, and egui *also* turns both
    /// Enter and Space into a fake click on the focused control as it is drawn.
    /// One press therefore has to leave the queue here in both spellings, or a
    /// toggle fires twice. Reads the source rather than driving a frame because
    /// the rule is about which branch owns the press and when the keys are
    /// taken out of the queue.
    #[test]
    fn the_confirm_key_presses_the_bar_control_the_ring_is_on() {
        let source = include_str!("input.rs");
        let handle = source.find("fn handle_keys(").unwrap();
        let rest = &source[handle..];
        let index = rest.find("focused.and_then(tilebar::index_of)").unwrap();
        assert!(
            rest[..index].contains("self.tile_bar.is_open()"),
            "the press belongs to the bar only while the bar is up"
        );
        assert!(
            rest[..index].contains("keys.enter && !self.modal_is_up()"),
            "the press belongs to the bar only while no window is over it"
        );
        let tail = &rest[index..index + 900];
        assert!(
            tail.contains("consume_any(input, &CONFIRM_KEYS)"),
            "both Enter and Space must leave the queue before the widget is drawn"
        );
        let fresh = tail.find("keys.enter_fresh").unwrap();
        let activate = tail.find("self.tile_bar_activate(action, ctx)").unwrap();
        assert!(fresh < activate, "a held confirm key must not fire the control every frame");
        assert!(
            source.contains("pub(super) const CONFIRM_KEYS: [Key; 2] = [Key::Enter, Key::Space]"),
            "Space is a confirm key too"
        );
        assert!(
            include_str!("wall.rs").contains("if !self.modal_is_up() && !self.tile_bar.ignoring(now)"),
            "the drawn bar's press path needs the same modal guard"
        );
    }

    /// The net that keeps the focus on the bar is for a bar the viewer opened.
    /// A pinned strip is up for as long as the picture is magnified, so the net
    /// ran every frame of a magnified portrait picture and put the focus on the
    /// seat each time - the wall never held the keys, and Back was answered by
    /// the layer that hands them back instead of by `back`, which is the
    /// magnification's own way out.
    #[test]
    fn the_pinned_strip_leaves_the_focus_with_the_wall() {
        let source = include_str!("mod.rs");
        let at = source
            .find("if self.tile_bar.is_open() && self.panel_anchor().is_none()")
            .unwrap();
        let head = &source[at..(at + 160).min(source.len())];
        assert!(
            head.contains("&& !self.tile_bar_pinned()"),
            "a pinned strip is not a layer and must not take the keys every frame"
        );
    }

    /// Back with the focus having walked into a pinned strip. The strip is not
    /// a layer to back out of and the ring is not the magnification either: the
    /// press takes the ring away and hands the keyboard back to the picture,
    /// which is where the viewer was before one was asked for. The next Back
    /// finds nothing focused and is `back`'s own way out. It still comes after
    /// the panels, which are layers and go first.
    #[test]
    fn back_takes_the_ring_and_leaves_the_picture_magnified() {
        let source = include_str!("input.rs");
        let start = source.find("fn keys_off_the_grid(").unwrap();
        let end = source[start..].find("fn handle_keys(").unwrap() + start;
        let rest = &source[start..end];
        assert!(
            !rest.contains("self.zoom_out()"),
            "the strip's Back is the ring's answer; the magnification is the second Back's"
        );
        let leave = rest.find("// Nothing else is a layer").expect("the ring is the last answer");
        let head = &rest[..leave];
        for panel in ["layout_picker.is_open()", "portrait_aspect_picker.is_open()", "show_settings"] {
            assert!(head.contains(panel), "{panel} is a layer and Back must close it first");
        }
    }

    /// Up on a pinned strip answers what Back answers: the ring goes and the
    /// picture keeps the keyboard. It must not leave the magnification as well -
    /// a press that both dropped the ring and left the picture would be two
    /// layers at once, and the viewer would have no way to step off the strip
    /// without losing the picture.
    #[test]
    fn up_on_a_pinned_strip_hands_the_keys_back_without_leaving() {
        let source = include_str!("input.rs");
        let start = source.find("} else if keys.up && self.tile_bar_pinned()").unwrap();
        let rest = &source[start..];
        let end = rest.find("} else if keys.up && self.status_has_focus(ctx)").unwrap();
        assert!(rest[..end].contains("self.leave_controls(ctx)"), "the ring goes with Up");
        assert!(
            !rest[..end].contains("self.zoom_out()"),
            "Up leaves the picture magnified"
        );
    }

    /// The confirm key on a pinned strip is the viewer asking for the ring.
    ///
    /// The seat draws nothing, so a press that lands there looks like a press
    /// the program never heard - which is exactly what a viewer of an upright
    /// picture saw: Enter, no ring, and the first arrow was needed to light one.
    /// A floating strip keeps the seat: it is a layer the key opens, and a
    /// viewer who opened it has not aimed at anything yet.
    #[test]
    fn the_confirm_key_lights_a_pinned_strips_first_control() {
        let source = include_str!("input.rs");
        let start = source.find("if keys.enter && keys.enter_fresh {").unwrap();
        let tail = &source[start..(start + 900).min(source.len())];
        assert!(
            tail.contains("if self.tile_bar_pinned()"),
            "a pinned strip is not a layer the key opens"
        );
        assert!(
            tail.contains("self.claim_the_strip(ctx)"),
            "the ring has to land on a control, not on the invisible seat"
        );
        assert!(
            tail.contains("memory.request_focus(tilebar::seat())"),
            "a floating strip keeps the seat, as it always has"
        );
        let helper = source.find("fn claim_the_strip(").unwrap();
        let body = &source[helper..(helper + 600).min(source.len())];
        assert!(body.contains("self.first_live_control()"), "a greyed control takes no ring");
        assert!(
            body.contains("unwrap_or_else(tilebar::seat)"),
            "an all-grey strip still needs somewhere to put the ring"
        );
    }

    /// Down on a pinned strip is the same request as the confirm key: the strip
    /// stands under the picture, so the arrow that points at it asks for the
    /// ring. It used to hand the keys to the status row, which is under the
    /// strip - a press that walked past the controls the viewer was aiming at.
    ///
    /// The arrow asks rather than does, and the ring is put down a frame later.
    /// egui latches a direction press before the frame's keys are handed out and
    /// spends it as the frame ends, walking the focus by geometry - and a widget
    /// given the focus on that frame has no lock to stop it, because a lock only
    /// takes hold on a widget that had the focus last frame. The ring therefore
    /// left the first control for whatever the layout had under it: on a
    /// recording, three lines deep, the fit button - the fifth control of
    /// thirteen. Nothing is latched on the frame after a press.
    #[test]
    fn down_asks_for_a_pinned_strips_ring_a_frame_ahead() {
        let source = include_str!("input.rs");
        let start = source.find("fn keys_of_the_magnified(").unwrap();
        let rest = &source[start..];
        let arm = rest.find("if keys.down {").unwrap();
        let tail = &rest[arm..(arm + 1500).min(rest.len())];
        assert!(tail.contains("if self.tile_bar_pinned()"), "Down points at the pinned strip");
        assert!(
            tail.contains("self.strip_claim = true"),
            "the arrow asks for the ring; egui would walk off one asked for now"
        );
        assert!(
            !tail.contains("self.claim_the_strip(ctx)"),
            "and it must not be put down on the frame egui is walking"
        );
        let held = tail.find("self.hand_off(ctx, Bar::Status)").unwrap();
        let claim = tail.find("self.strip_claim = true").unwrap();
        assert!(claim < held, "the status row is only reached when there is no pinned strip");
        // And the request is answered as the next frame begins, before any of
        // that frame's keys can be read against it.
        let asked = source.find("if std::mem::take(&mut self.strip_claim)").unwrap();
        let body = &source[asked..(asked + 300).min(source.len())];
        assert!(body.contains("self.claim_the_strip(ctx)"), "the ring is put down there");
        assert!(body.contains("self.tile_bar.is_open()"), "and only while the strip is up");
        assert!(body.contains("self.tile_bar_pinned()"), "and only where it is pinned");
    }

    /// Starting a call from the microphone replaces the whole row with the
    /// handset, so the index the ring stood on no longer names the control the
    /// viewer pressed. The ring has to go with the press rather than be dropped
    /// with the widget that used to hold it - on every wall, because the row
    /// changes the same way whether the strip lies across a landscape picture,
    /// under an upright one, or down the side of a turned one.
    #[test]
    fn the_ring_follows_the_microphone_into_the_handset() {
        let source = include_str!("input.rs");
        let at = source.find("self.tile_bar_activate(action, ctx);").unwrap();
        let tail = &source[at..(at + 700).min(source.len())];
        assert!(
            tail.contains("self.ring_follows_the_call(ctx, action)"),
            "the confirm key has to move the ring the row change took away"
        );
        let wall = include_str!("wall.rs");
        let start = wall.find("fn ring_follows_the_call(").unwrap();
        let body = &wall[start..(start + 1200).min(wall.len())];
        assert!(
            body.contains("TileBarAction::Mic | TileBarAction::Hangup"),
            "only the two ends of the call replace the row"
        );
        assert!(
            body.contains("if !self.bar_has_focus(ctx)"),
            "a press that never took the keyboard is given no ring"
        );
        assert!(
            body.contains("let turned_into = if self.talking")
                && body.contains("TileBarAction::Hangup")
                && body.contains("TileBarAction::Mic"),
            "the ring lands on the control the press turned into"
        );
        assert!(
            body.contains("memory.request_focus(tilebar::control(index))"),
            "the ring goes to a control of the new row, not to the seat"
        );
    }

    /// The picture and its recording keep the ring, and the ring walks them.
    ///
    /// The strip carries the live controls and the recording's, and the two
    /// share nothing: the row under the ring is replaced whole. Left alone, the
    /// focus names a widget that is no longer drawn, egui drops it, and the
    /// arrows change channel instead of walking the controls: the keyboard left
    /// the strip on the press that asked for the recording. The ring therefore
    /// goes to the control that undoes the switch, and only a ring that was
    /// already on the strip is moved - a viewer watching the picture asked for
    /// no cursor.
    ///
    /// Upright the strip is pinned under the picture; landscape and a turned
    /// picture float it over one. The row is replaced the same way in all
    /// three, so all three keep the ring - what a floating strip adds is a clock,
    /// not another answer. It can be put away while the box answers, and a strip
    /// that is no longer drawn has no ring to keep.
    ///
    /// The ring is held by an **anchor** rather than by the place it was put:
    /// the row is drawn again for every picture, and it is not the same length
    /// twice - a row that carries a sound switch is one longer, and the switch
    /// comes and goes with the picture. A ring put on a place is a ring on a
    /// control that the next row has drawn over, and egui drops such a focus
    /// with the widget it named. So the anchor names the control - the way back
    /// to the picture - and the ring is put on it again for as long as the
    /// anchor holds.
    #[test]
    fn the_ring_keeps_the_strip_across_the_recording() {
        let source = include_str!("input.rs");
        let at = source.find("self.tile_bar_activate(action, ctx);").unwrap();
        let tail = &source[at..(at + 1600).min(source.len())];
        assert!(
            tail.contains("self.ring_follows_the_switch(ctx, action)"),
            "a switch of the row has to keep the ring the viewer asked for"
        );
        let wall = include_str!("wall.rs");
        let start = wall.find("fn ring_follows_the_switch(").unwrap();
        let body = &wall[start..(start + 2200).min(wall.len())];
        assert!(
            body.contains("if !self.tile_bar.is_open() || !self.bar_has_focus(ctx)"),
            "any strip that is up, and only a ring that was on one of its controls"
        );
        assert!(
            !body.contains("tile_bar_pinned"),
            "a floating strip carries the same row and keeps the ring too"
        );
        assert!(
            body.contains("action: TileBarAction::Live,"),
            "opening a recording anchors the ring on the way back"
        );
        assert!(
            body.contains("action: TileBarAction::Playback,"),
            "and leaving one anchors it on the way in"
        );
        assert!(
            body.contains("self.ring_anchor = None;"),
            "any other control is the viewer's own choice, and the anchor ends"
        );
        // The anchor is put on the row as it is drawn, every frame: a row that
        // is a frame shorter moves it, and a ring that was put on a place is a
        // ring on a control that is not there.
        let at = wall.find("fn place_the_anchored_ring(").unwrap();
        let body = &wall[at..(at + 2600).min(wall.len())];
        assert!(
            body.contains("self.the_control_for(action)"),
            "the anchor is found in the row being drawn"
        );
        assert!(
            body.contains("since.elapsed() > ANCHOR_WAIT"),
            "an anchor with no question behind it gives up"
        );
        assert!(
            body.contains("!tilebar::owns(id)"),
            "a ring that has left the strip is not the one the anchor holds"
        );
        assert!(
            body.contains("memory.request_focus(id)"),
            "and the ring is put on it"
        );
        let app = include_str!("mod.rs");
        let at = app.find("self.place_the_anchored_ring(ctx);").unwrap();
        assert!(
            at > 0,
            "the ring is put on its anchor every frame, not once"
        );
        // The remote walking the strip is the viewer taking the ring back, and
        // a ring handed back to the wall is not one to keep.
        let at = source.find("if bar_owns && (keys.left || keys.right)").unwrap();
        let tail = &source[at..(at + 200).min(source.len())];
        assert!(
            tail.contains("self.ring_anchor = None"),
            "the remote walking the strip lets the anchor go"
        );
        let chrome = include_str!("chrome.rs");
        let at = chrome.find("fn leave_controls(").unwrap();
        assert!(
            chrome[at..(at + 600).min(chrome.len())].contains("self.ring_anchor = None"),
            "a ring handed back to the wall is not one to keep"
        );
    }

    /// The bar waits for the answer that is on its way to it.
    ///
    /// A floating strip has a clock of its own: eight seconds of no answer to
    /// it and it goes down. A question the box has not answered yet - a
    /// recording being opened - is an answer on its way, and the row it is
    /// about to replace is the row the viewer's ring is on. A bar that went
    /// down in the middle of the wait took the ring with it, and the keyboard
    /// went back to the picture: the arrows changed channel where the viewer
    /// was walking the controls. So the bar is held up until the answer lands,
    /// and the hold is for a while rather than for ever.
    ///
    /// The hold is the bar's clock, not a rule beside it: the time the bar has
    /// left is the time the hold has left, and the wall is asked to come back
    /// when it runs out - otherwise the bar would wait for an answer that never
    /// comes and stay up for good. The answer hands the clock back, starting it
    /// again, because a bar that has been waiting half a minute is not a bar the
    /// viewer has stopped using.
    #[test]
    fn the_bar_waits_for_the_answer_it_is_about_to_get() {
        let tilebar = include_str!("../tilebar.rs");
        assert!(
            tilebar.contains("pub fn hold(&mut self, now: Instant)"),
            "the bar can be held up"
        );
        assert!(
            tilebar.contains("pub fn held(&self, now: Instant) -> bool"),
            "and asked whether it is"
        );
        assert!(
            tilebar.contains("pub fn release(&mut self, now: Instant)"),
            "and let go when the answer lands, with its clock started again"
        );
        // The time the bar has left is the time the hold has left: that is what
        // the wall is asked to come back for.
        let at = tilebar.find("pub fn remaining(&self, now: Instant)").unwrap();
        let body = &tilebar[at..(at + 700).min(tilebar.len())];
        assert!(
            body.contains("Some(until) if now < until => Some((until - now).max(idle))"),
            "a held bar's clock is the hold's"
        );
        // The idle rule itself is unchanged: the hold is carried by the clock,
        // not by a second condition beside it.
        let app = include_str!("mod.rs");
        let at = app.find("if let Some(left) = self.tile_bar.remaining(Instant::now())").unwrap();
        let tail = &app[at..(at + 2000).min(app.len())];
        assert!(
            !tail.contains("self.tile_bar.held(Instant::now())"),
            "the hold is the bar's clock, not a rule of its own"
        );
        assert!(
            tail.contains("ctx.request_repaint_after(left)"),
            "and the wall comes back when it runs out"
        );
        // The question holds it, and the answer - a recording, or the box saying
        // there is none to play - lets it go.
        let playback = include_str!("playback.rs");
        let at = playback.find("fn enter_playback(").unwrap();
        let tail = &playback[at..(at + 3500).min(playback.len())];
        assert!(
            tail.contains("self.tile_bar.hold(now)"),
            "the question holds the bar up"
        );
        let at = playback.find("fn start_playback(").unwrap();
        let tail = &playback[at..(at + 2500).min(playback.len())];
        assert!(
            tail.contains("self.tile_bar.release(Instant::now())"),
            "the answer lets it go"
        );
        let at = app.find("BackgroundEvent::PlaybackReady").unwrap();
        let tail = &app[at..(at + 3500).min(app.len())];
        assert!(
            tail.contains("self.tile_bar.release(Instant::now())"),
            "and so does the box saying there is nothing to play"
        );
        // A press with a finger is the same question the remote asks, and it
        // gets the same answer: the ring follows the switch.
        let wall = include_str!("wall.rs");
        let at = wall.find("self.tile_bar_activate(*action, ui.ctx());").unwrap();
        let tail = &wall[at..(at + 600).min(wall.len())];
        assert!(
            tail.contains("self.ring_follows_the_switch(ui.ctx(), *action)"),
            "a finger on the strip keeps the ring too"
        );
    }

    /// The strip is one waiting mark while the box answers, and nothing on it
    /// can be pressed twice.
    ///
    /// A press that asked for a recording used to leave a live row on the
    /// screen until the box answered, and that row still offered a microphone,
    /// another way into the recording, a turn of the picture and a whole walk
    /// of controls - all of them about a picture that was about to be replaced.
    /// A call opened in that wait took the whole strip for its handset and was
    /// still there when the recording came up.
    #[test]
    fn the_strip_waits_for_the_recording_with_nothing_to_press() {
        let wall = include_str!("wall.rs");
        let at = wall.find("fn tile_bar_entries(").unwrap();
        let body = &wall[at..(at + 1200).min(wall.len())];
        assert!(
            body.contains("if self.playback_is_asking()")
                && body.contains("tilebar::Control::Waiting"),
            "the waiting strip carries one mark and no controls"
        );
        let at = wall.find("fn tile_bar_available(").unwrap();
        let body = &wall[at..(at + 500).min(wall.len())];
        assert!(
            body.contains("TileBarAction::NoSound | TileBarAction::Waiting"),
            "the mark is not a control"
        );
        // One question, and only the answer to that question is about what is
        // on the screen.
        let playback = include_str!("playback.rs");
        let at = playback.find("fn enter_playback(").unwrap();
        let body = &playback[at..(at + 3000).min(playback.len())];
        assert!(
            body.contains("if self.playback_is_asking()") && body.contains("return;"),
            "a second press is not another question"
        );
        assert!(
            body.contains("self.playback_asking = Some((channel, now))"),
            "the question is recorded with the picture it belongs to"
        );
        let app = include_str!("mod.rs");
        let at = app.find("BackgroundEvent::PlaybackReady").unwrap();
        let body = &app[at..(at + 2000).min(app.len())];
        assert!(
            body.contains("self.playback_is_asking()") && body.contains("asked == *channel"),
            "only a question still on the screen has an answer"
        );
        // The keys cannot fall through to the picture while the strip waits.
        let input = include_str!("input.rs");
        let at = input.find("if self.playback_is_asking()").unwrap();
        let body = &input[at..(at + 900).min(input.len())];
        assert!(
            body.contains("if self.playback_is_asking()")
                && body.contains("consume_any(input, &CONFIRM_KEYS)")
                && body.contains("memory.request_focus(tilebar::seat())"),
            "the arrows and the confirm key are taken, and the seat keeps the focus"
        );
        // The mark turns, and it is drawn outside the walk.
        let tilebar = include_str!("../tilebar.rs");
        let at = tilebar.find("Control::Waiting =>").unwrap();
        let body = &tilebar[at..(at + 400).min(tilebar.len())];
        assert!(
            body.contains("icons::waiting_in") && body.contains("continue;"),
            "the mark takes no press and no stop on the walk"
        );
        let icons = include_str!("../icons.rs");
        assert!(
            icons.contains("input.time") && icons.contains("circle_arc"),
            "and it turns with the clock"
        );
        // The wait is about the picture on the screen, not only a clock: the
        // paths that change the magnification do not all settle the bar, so a
        // question left behind must not put a recording on another picture.
        let at = playback.find("fn playback_is_asking(").unwrap();
        let body = &playback[at..(at + 900).min(playback.len())];
        assert!(
            body.contains("self.scheduler.zoom() == Some(channel)"),
            "a viewer who left the picture is not waiting for it"
        );
        assert_eq!(
            ANCHOR_WAIT,
            crate::tilebar::HOLD,
            "the strip and the question are held for the same wait"
        );
        // And an answer ends the question it was asked about, even when the
        // viewer has left: stepping back must not find a strip still waiting.
        let at = app.find("a recording answered a question no longer asked").unwrap();
        let head = &app[at.saturating_sub(1000)..at];
        assert!(
            head.contains("asked == *channel") && head.contains("self.playback_asking = None"),
            "an answer ends the question it was asked about"
        );
        // A call that outlived the wait is ended when the recording opens, but
        // only one that is actually up: letting go clears the ring's anchor.
        let at = playback.find("fn start_playback(").unwrap();
        let body = &playback[at..(at + 2000).min(playback.len())];
        assert!(
            body.contains("if self.talking {") && body.contains("self.set_talking(false)"),
            "the recording cannot inherit a call"
        );
    }

    /// A Back with no ring on the strip is the way out, not a press spent on
    /// a focus nothing drew.
    ///
    /// A finger on the switch between a picture and its recording used to
    /// leave the strip's seat focused on the next frame. The seat owns the
    /// keyboard without drawing a ring, so `end_frame` called the controls the
    /// owner and the first Back was spent surrendering a cursor the viewer
    /// could not see; only the second left the magnified picture.
    ///
    /// The wait for the box's answer puts the keyboard on that same seat for
    /// as long as the strip is one turning mark. A box that answers within the
    /// frame leaves none behind, and a box that takes a frame longer leaves
    /// the seat focused in the recording that opens - so a fix that only stops
    /// the seat being queued for the next frame works on the fast box and not
    /// on the slow one. Both are answered by the state the press was aimed at:
    /// the focus the last frame ended on.
    #[test]
    fn a_back_with_no_visible_ring_leaves_the_picture_at_once() {
        // A ring that is drawn is still let go by the first Back: the rule is
        // owned by `owner`, read at the end of the last frame, and not by a
        // focus egui may already have dropped on this one.
        let input = include_str!("input.rs");
        let at = input.find("fn keys_off_the_grid(").unwrap();
        let body = &input[at..(at + 9000).min(input.len())];
        let back = body.find("if keys.back {").unwrap();
        let branch = &body[back..(back + 6000).min(body.len())];
        assert!(
            branch.contains("self.leave_controls(ctx);")
                && !branch.contains("focused.is_some_and(|id| id != tilebar::seat())"),
            "a visible ring is still the first Back's business"
        );
        assert!(
            branch.contains("self.focus_at_end == Some(tilebar::seat())")
                && branch.contains("self.back(ctx);"),
            "a seat with no ring is the way out in one press"
        );
        assert!(
            input.contains("if keys.back {") && input.contains("self.back(ctx);"),
            "with no focus at all the press goes straight to back"
        );
        // The two are read together, from the frame the viewer was looking at,
        // and not from the press's own frame - egui has dropped its focus by
        // then and a ring that was on the strip would read as none.
        let app = include_str!("mod.rs");
        let at = app.find("self.owner = if focused.is_some()").unwrap();
        let tail = &app[at..(at + 800).min(app.len())];
        assert!(
            tail.contains("self.focus_at_end = focused;"),
            "the rule reads the focus the last frame ended on"
        );
        assert!(
            app.contains("focus_at_end: None,"),
            "the remembered focus starts with the app"
        );
        let playback = include_str!("playback.rs");
        assert!(
            playback.contains("let keep_ring = self.ring_anchor.is_some();")
                && playback.contains("self.restore_focus = keep_ring.then(tilebar::seat)"),
            "a switch without a ring does not put the keyboard on the seat"
        );
        assert!(
            playback.contains("if !keep_ring {")
                && playback.contains("self.release_the_waiting_seat(ctx);")
                && playback.contains("memory.surrender_focus(id)"),
            "the seat the wait left behind is given up when the answer lands"
        );
        let at = app.find("toast-no-recording").unwrap();
        assert!(
            app[at.saturating_sub(600)..at].contains("self.release_the_waiting_seat(ctx)"),
            "an answer with no recording gives the keys back too"
        );
        let wall = include_str!("wall.rs");
        assert!(
            wall.contains("if self.restore_focus == Some(tilebar::seat()) {")
                && wall.contains("self.restore_focus = None"),
            "a finger's press does not leave a seat queued for the next frame"
        );
    }

    /// Stepping off a recording with the arrows leaves the arrows with the
    /// picture.
    ///
    /// A finger on the way into a recording draws no ring. Left then steps to
    /// the camera before it, and the recording goes with the picture - but
    /// `leave_playback` used to put the strip's seat on the next frame however
    /// the recording was left. The seat draws nothing, yet the first Right on
    /// the new camera was its first arrow: it lit the first control of the bar
    /// instead of stepping the picture back. A live picture stepped the same
    /// way was never affected, because there was no recording to put away.
    #[test]
    fn stepping_past_a_recording_leaves_the_arrows_with_the_picture() {
        let playback = include_str!("playback.rs");
        let at = playback.find("fn leave_playback(").unwrap();
        let body = &playback[at..(at + 2200).min(playback.len())];
        assert!(
            body.contains("let keep_ring = self")
                && body.contains("focus_at_end")
                && body.contains("tilebar::index_of(id).is_some()"),
            "only a ring drawn on a control is worth a seat"
        );
        assert!(
            body.contains("self.restore_focus = keep_ring.then(tilebar::seat);")
                && !body.contains("self.restore_focus = Some(tilebar::seat());"),
            "leaving a recording with no ring queues no seat"
        );
        let paging = include_str!("paging.rs");
        let at = paging.find("fn step_zoom(").unwrap();
        assert!(
            paging[at..(at + 1800).min(paging.len())].contains("self.leave_playback();"),
            "a step to another camera puts its recording away"
        );
    }

    /// The strip is walked by Left and Right whichever way it lies.
    ///
    /// A turned strip runs down the screen and was given Up and Down to walk it -
    /// which took the vertical pair away from the picture underneath, and left a
    /// viewer pressing the sideways arrows with nothing moving. The quarter turn
    /// is how the picture is drawn, not a second axis for the hand: one scope,
    /// one pair of arrows.
    #[test]
    fn a_turned_strip_is_walked_by_the_sideways_arrows() {
        let source = include_str!("input.rs");
        let start = source.find("fn parse_keys(").unwrap();
        let rest = &source[start..];
        let arm = rest.find("_ if bar_owns =>").unwrap();
        let tail = &rest[arm..(arm + 1000).min(rest.len())];
        assert!(
            tail.contains("keys.left = input.consume_key(none, Key::ArrowLeft)"),
            "Left walks the strip"
        );
        assert!(
            tail.contains("keys.right = input.consume_key(none, Key::ArrowRight)"),
            "Right walks the strip"
        );
        assert!(
            !rest[..rest.find("fn handle_keys(").unwrap_or(rest.len())].contains("bar_owns &&"),
            "there is no second arm for a turned strip"
        );
        assert!(!rest.contains("bar_vertical"), "the walk has one axis, not two");
    }

    /// One scope for the strip, so that no second pair of arrows is left
    /// answering the same row of controls.
    #[test]
    fn the_strip_registers_one_scope() {
        let source = include_str!("mod.rs");
        assert!(
            !source.contains("\"tile-bar-column\""),
            "a turned strip has no scope of its own"
        );
        assert!(source.contains("(\"tile-bar-row\", ScopeDef::new(Axis::Row))"));
    }

    /// A finger on the strip must not take the keyboard with it.
    ///
    /// egui puts the focus on a widget it is clicked on, so a press on a button
    /// left the ring standing on it - and the next arrow walked the strip
    /// instead of the picture. The press is still answered; it just does not
    /// decide who reads the keyboard.
    #[test]
    fn a_finger_on_the_strip_does_not_take_the_keys() {
        let wall = include_str!("wall.rs");
        let start = wall.find("fn tile_bar_overlay(").unwrap();
        let rest = &wall[start..];
        let arm = rest.find("A finger on the strip is not the remote taking it").unwrap();
        let tail = &rest[arm..(arm + 1400).min(rest.len())];
        assert!(
            tail.contains("panel.bar.contains(pos)"),
            "the band a pinned strip stands in is the strip"
        );
        assert!(
            tail.contains("self.tile_bar_pinned() && ui.input(|input| input.pointer.any_pressed())"),
            "a floating strip is a layer and keeps the keys; and the press is the pointer's, \
             because a press on the strip never reaches the tile"
        );
        assert!(
            tail.contains("self.leave_controls(ui.ctx())"),
            "the ring goes with the finger, so the arrows go back to the picture"
        );
    }

    /// egui 0.32: a focused widget with `Sense::click()` is clicked by either
    /// Enter or Space. This is the second activation the manual bar branch has
    /// to prevent - and the reason consuming only Enter left Space looking
    /// broken. The test drives a real frame both ways.
    fn confirm_key_clicks_the_focus(key: Key, consume: bool) -> bool {
        let ctx = egui::Context::default();
        let id = Id::new("confirm-probe");
        let mut clicked = false;
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0))),
            events: vec![press(key)],
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            ctx.memory_mut(|memory| memory.request_focus(id));
            if consume {
                ctx.input_mut(|input| input::consume_any(input, &input::CONFIRM_KEYS));
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = ui.interact(
                    Rect::from_min_size(pos2(0.0, 0.0), vec2(40.0, 40.0)),
                    id,
                    Sense::click(),
                );
                clicked = response.clicked();
            });
        });
        clicked
    }

    #[test]
    fn only_a_confirm_key_left_in_flight_fake_clicks_a_widget() {
        for key in input::CONFIRM_KEYS {
            assert!(confirm_key_clicks_the_focus(key, false), "{key:?} is a fake click");
            assert!(
                !confirm_key_clicks_the_focus(key, true),
                "{key:?} must be taken out before the widget is drawn"
            );
        }
    }

    /// `||` short-circuits; `consume_any` must not. A frame carrying two
    /// synonyms has to lose both, or the leftover reaches the next layer.
    #[test]
    fn consume_any_takes_every_synonym_out_of_one_frame() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0))),
            events: vec![
                press(Key::Enter),
                press(Key::Space),
                press(Key::Escape),
                press(Key::BrowserBack),
            ],
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            ctx.input_mut(|input| {
                assert!(input::consume_any(input, &input::CONFIRM_KEYS));
                assert!(input::consume_any(input, &input::BACK_KEYS));
                for key in [Key::Enter, Key::Space, Key::Escape, Key::BrowserBack] {
                    assert!(!input.key_pressed(key), "{key:?} was left in the queue");
                }
                assert!(!input::consume_any(input, &input::CONFIRM_KEYS), "nothing left to take");
            });
        });
    }

    /// The upright page arrows are hand-drawn `Sense::click()` widgets and the
    /// application turns the page for them too. Without taking the confirm key
    /// here, egui's fake click on the focused arrow turns a second page.
    #[test]
    fn the_upright_page_arrows_own_their_confirm_key() {
        let source = include_str!("input.rs");
        let branch = source.find("if let Some(forward) = forward").unwrap();
        let tail = &source[branch..branch + 600];
        assert!(tail.contains("consume_any(input, &CONFIRM_KEYS)"));
        assert!(tail.contains("self.advance(forward, true)"));
    }

    /// A window opened by a confirm key swallows the key for a few frames, so
    /// it cannot activate the control the window has just focused. That has to
    /// cover Space as well as Enter.
    #[test]
    fn an_opening_window_swallows_both_confirm_keys() {
        let source = include_str!("mod.rs");
        let swallow = source.find("if self.swallow_confirm > 0").unwrap();
        assert!(
            source[swallow..swallow + 400]
                .contains("input::consume_any(i, &input::CONFIRM_KEYS)"),
            "the opening press must be taken in both spellings"
        );
    }

    /// eframe does not return from `run_native` on the desktop, so the only
    /// chance to let the audio devices go on a healthy stack is the frame that
    /// sees the close request. Reads the source because removing these two
    /// lines still leaves a process that exits - it just exits through the
    /// loader's TLS callbacks, where a cpal join panic aborts it.
    #[test]
    fn the_close_frame_lets_the_audio_devices_go() {
        let source = include_str!("mod.rs");
        let pump = source.find("fn pump_background(").unwrap();
        let close = pump + source[pump..]
            .find("if ctx.input(|input| input.viewport().close_requested())")
            .unwrap();
        let tail = &source[close..close + 1400];
        assert!(tail.contains("crate::audio::shutdown();"), "the output device is let go on close");
        assert!(tail.contains("crate::mic::shutdown();"), "the microphone is let go on close");
    }

    /// A bar that goes down on its idle clock does so after the frame's focus
    /// rules have run, so it must take the focus with it. Without this the
    /// seat is an id with no widget behind it until egui notices a frame or
    /// two later - the wall has no keys in the meantime.
    #[test]
    fn the_idle_bar_takes_its_focus_with_it() {
        let source = include_str!("mod.rs");
        let zero = source.find("left.is_zero()").unwrap();
        let hide = zero + source[zero..].find("self.tile_bar.hide();").unwrap();
        let tail = &source[hide..hide + 400];
        assert!(tail.contains("self.leave_controls(ctx)"), "an idle hide must surrender the focus");
    }

    /// The net under the bar holds it up as well as down. Upright and magnified
    /// the strip is pinned, and the frame's own reasons - the grid, the blank, a
    /// panel over the wall - are the only ones that take it away; a press that
    /// opened it is not what it waits for, so nothing that leaves it down by
    /// accident may survive to the next frame.
    #[test]
    fn the_frame_puts_a_pinned_bar_back_up() {
        let source = include_str!("mod.rs");
        let net = source.find("|| (self.panel_anchor().is_some() && !self.talking)").unwrap();
        let tail = &source[net..net + 900];
        assert!(
            tail.contains("self.tile_bar_pinned() && !self.tile_bar.is_open()"),
            "a pinned strip is put back rather than waited for"
        );
        assert!(tail.contains("self.tile_bar.show("), "and it is put back up");
    }

    /// The lock `tilebar::draw`, `Nav::finish` and `timeline_input` each put
    /// on whatever they have focused. egui's `begin_pass` reads the filter
    /// left at the end of the *previous* frame, and only a widget that claims
    /// both axes stops the geometric focus walk.
    fn lock_arrows(ctx: &egui::Context, id: Id) {
        ctx.memory_mut(|memory| {
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

    /// One frame of the magnified viewport with the seat's real geometry: two
    /// pixels in the bar's top-left corner, the first button down and to its
    /// right, the recording's strip below, and the wall's two bars at the
    /// screen's edges. `keep_seat` is the end-of-frame rule under test - the
    /// old one surrendered the seat every frame and asked for it again, so
    /// the lock drawn on it never reached the next `begin_pass`.
    fn focus_frame(
        ctx: &egui::Context,
        frame: usize,
        key: Option<Key>,
        keep_seat: bool,
        start: Id,
        timeline: bool,
    ) -> Option<Id> {
        let (toolbar, status, strip) =
            (Id::new("toolbar"), Id::new("status"), Id::new("xgview-timeline"));
        let (seat, first) = (tilebar::seat(), tilebar::control(0));
        let events: Vec<egui::Event> = key.map(press).into_iter().collect();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            // `handle_keys`, before anything is drawn.
            if frame == 0 {
                ctx.memory_mut(|memory| memory.request_focus(start));
            }
            // The old code answered the seat only in the branch reached when
            // the wall did *not* own the keys; a frame that owned them left
            // the focus on the seat for egui to walk by geometry. The fix
            // answers it here, before either branch can be taken.
            if keep_seat {
                if let Some(target) = tilebar::first_arrow_target(
                    true,
                    ctx.memory(|memory| memory.focused()),
                    key.is_some(),
                ) {
                    ctx.memory_mut(|memory| memory.request_focus(target));
                    // The strip reads its Down from this same queue; the press
                    // is already spent on the seat. `begin_pass` reads the raw
                    // events, so this does not stop egui's walk - the lock does.
                    ctx.input_mut(|input| {
                        for k in [Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown] {
                            input.consume_key(Modifiers::NONE, k);
                        }
                    });
                }
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                let _ = ui.interact(
                    Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 30.0)),
                    toolbar,
                    Sense::click(),
                );
                let _ = ui.interact(
                    Rect::from_min_size(pos2(0.0, 570.0), vec2(800.0, 30.0)),
                    status,
                    Sense::click(),
                );
                let seat_widget = ui.interact(
                    Rect::from_min_size(pos2(203.0, 403.0), vec2(2.0, 2.0)),
                    seat,
                    Sense::focusable_noninteractive(),
                );
                let first_widget = ui.interact(
                    Rect::from_min_size(pos2(209.0, 409.0), vec2(40.0, 40.0)),
                    first,
                    Sense::click(),
                );
                // Only a recording has a strip under the bar. A live picture
                // has nothing down there for egui to walk onto.
                let strip_widget = timeline.then(|| {
                    ui.interact(
                        Rect::from_min_size(pos2(200.0, 470.0), vec2(400.0, 50.0)),
                        strip,
                        Sense::click_and_drag(),
                    )
                });
                if seat_widget.has_focus() {
                    lock_arrows(ctx, seat);
                }
                if first_widget.has_focus() {
                    lock_arrows(ctx, first);
                }
                if strip_widget.as_ref().is_some_and(|widget| widget.has_focus()) {
                    lock_arrows(ctx, strip);
                    // Up hands the keys back to the control they came from.
                    if key == Some(Key::ArrowUp) {
                        ctx.memory_mut(|memory| memory.request_focus(first));
                    }
                }
            });
            // `end_frame`: a focus the navigation layer does not own is given
            // up, and one that is not the bar's goes back to the seat.
            if let Some(focused) = ctx.memory(|memory| memory.focused()) {
                let keeps = if keep_seat {
                    viewport_keeps_focus(focused, true, timeline, timeline.then_some(strip))
                } else {
                    timeline && focused == strip
                };
                if focused != first && !keeps {
                    ctx.memory_mut(|memory| memory.surrender_focus(focused));
                }
            }
            let focused = ctx.memory(|memory| memory.focused());
            let strip_focus = timeline && focused == Some(strip);
            if !focused.is_some_and(tilebar::owns) && !strip_focus {
                ctx.memory_mut(|memory| memory.request_focus(seat));
            }
        });
        ctx.memory(|memory| memory.focused())
    }

    fn walk_seat(key: Key, keep_seat: bool, timeline: bool) -> Vec<Option<Id>> {
        let ctx = egui::Context::default();
        (0..4)
            .map(|frame| {
                focus_frame(
                    &ctx,
                    frame,
                    (frame == 2).then_some(key),
                    keep_seat,
                    tilebar::seat(),
                    timeline,
                )
            })
            .collect()
    }

    /// The old rule, written down so the reason for the fix is not lost: a
    /// seat rebuilt every frame has a default event filter, so egui walks the
    /// focus geometrically and only Down and Right find the first button.
    #[test]
    fn a_seat_rebuilt_every_frame_is_walked_in_only_two_directions() {
        for key in [Key::ArrowDown, Key::ArrowRight] {
            assert_eq!(
                walk_seat(key, false, false)[2],
                Some(tilebar::control(0)),
                "{key:?} found the button by geometry"
            );
        }
        for key in [Key::ArrowUp, Key::ArrowLeft] {
            assert_ne!(
                walk_seat(key, false, false)[2],
                Some(tilebar::control(0)),
                "{key:?} walked off the bar entirely"
            );
        }
    }

    /// Every arrow is the same request: light the first button. One idle
    /// frame is needed for the seat's lock to reach `begin_pass`, which is
    /// also what a viewer's two separate key presses always have.
    #[test]
    fn every_arrow_from_the_seat_lights_the_first_button() {
        for timeline in [false, true] {
            for key in [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight] {
                let seen = walk_seat(key, true, timeline);
                let state = if timeline { "recording" } else { "live" };
                assert_eq!(seen[0], Some(tilebar::seat()), "{state}: opens on its seat");
                assert_eq!(
                    seen[2],
                    Some(tilebar::control(0)),
                    "{state}, {key:?}: must light the first button"
                );
                assert_eq!(
                    seen[3],
                    Some(tilebar::control(0)),
                    "{state}, {key:?}: and the ring stays put"
                );
            }
        }
    }

    /// A focused strip keeps its arrows: Up hands them back to a control,
    /// Down stays with the strip, and neither puts a ring on the wall's bars.
    #[test]
    fn a_focused_strip_keeps_its_arrows_out_of_the_walls_bars() {
        let strip = Id::new("xgview-timeline");
        for key in [Key::ArrowUp, Key::ArrowDown] {
            let ctx = egui::Context::default();
            let seen: Vec<Option<Id>> = (0..4)
                .map(|frame| {
                    focus_frame(&ctx, frame, (frame == 2).then_some(key), true, strip, true)
                })
                .collect();
            assert_ne!(seen[2], Some(Id::new("toolbar")), "{key:?} reached the toolbar");
            assert_ne!(seen[2], Some(Id::new("status")), "{key:?} reached the status row");
            let expected = if key == Key::ArrowUp {
                Some(tilebar::control(0))
            } else {
                Some(strip)
            };
            assert_eq!(seen[2], expected, "{key:?} goes where the recording says");
        }
    }

    /// What egui does on the frame a control is drawn grey: the ring the frame
    /// began with is gone by the time it ends, so the hand-off cannot read the
    /// focus that is left.
    #[test]
    fn a_control_drawn_grey_has_no_focus_left_at_the_end_of_the_frame() {
        let ctx = egui::Context::default();
        let id = tilebar::control(2);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(200.0, 100.0))),
                ..Default::default()
            },
            |ctx| {
                ctx.memory_mut(|memory| memory.request_focus(id));
                assert_eq!(ctx.memory(|memory| memory.focused()), Some(id));
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = icons::button_in(
                        ui,
                        Rect::from_min_size(pos2(0.0, 0.0), vec2(40.0, 40.0)),
                        id,
                        Icon::TimelineZoomIn,
                        false,
                        true,
                        "",
                        false,
                    );
                });
                assert!(
                    ctx.memory(|memory| memory.focused()).is_none(),
                    "egui surrenders the focus when it draws a control as grey"
                );
            },
        );
    }

    /// The frame a confirm key greys the very control the ring is on. The key is
    /// answered at the start of the frame, so the control is drawn grey and
    /// egui has given the focus up before the frame ends - but the ring was on a
    /// control when the frame began, and it has to be handed to the next live
    /// one rather than disappear onto the seat.
    #[test]
    fn a_confirm_key_that_greys_its_control_still_passes_the_ring_on() {
        let ctx = egui::Context::default();
        let from = tilebar::control(2);
        let live = [true, true, false, true, true];
        let mut seen = Vec::new();
        for frame in 0..2 {
            let pressed = frame == 0;
            let events: Vec<egui::Event> = pressed.then(|| press(Key::Enter)).into_iter().collect();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 200.0))),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    if frame == 0 {
                        ctx.memory_mut(|memory| memory.request_focus(from));
                    }
                    let focus_before = ctx.memory(|memory| memory.focused());
                    if pressed {
                        // `handle_keys` answers the confirm key before the bar
                        // is drawn, and takes both spellings out of the queue.
                        // The press takes the last step of the zoom ladder.
                        ctx.input_mut(|i| assert!(input::consume_any(i, &input::CONFIRM_KEYS)));
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        for (index, _) in live.iter().enumerate() {
                            let rect = Rect::from_min_size(
                                pos2(20.0 + index as f32 * 45.0, 20.0),
                                vec2(40.0, 40.0),
                            );
                            let sense = if index == 2 { Sense::hover() } else { Sense::click() };
                            let _ = ui.interact(rect, tilebar::control(index), sense);
                        }
                    });
                    // `end_frame`: hand off from a greyed control, then put a
                    // stray focus back on the bar's seat.
                    let focused = ctx.memory(|memory| memory.focused());
                    if frame == 0 {
                        assert!(focused.is_none(), "the grey draw gave egui's focus up");
                        assert_eq!(
                            XgViewApp::grey_handoff(focused, None, &live),
                            None,
                            "the old rule, which read only the focus left, handed on nothing"
                        );
                    }
                    if let Some(target) = XgViewApp::grey_handoff(focused, focus_before, &live) {
                        ctx.memory_mut(|memory| memory.request_focus(target));
                    }
                    let focused = ctx.memory(|memory| memory.focused());
                    if !focused.is_some_and(tilebar::owns) {
                        ctx.memory_mut(|memory| memory.request_focus(tilebar::seat()));
                    }
                    seen.push(ctx.memory(|memory| memory.focused()));
                },
            );
        }
        assert_eq!(seen[0], Some(tilebar::control(3)), "the ring went to the next live control");
        assert_eq!(seen[1], Some(tilebar::control(3)), "and stayed there on the next frame");
    }

    /// The hand-off cannot read only the focus that is left: a window over the
    /// wall owns that, and the frame a control greys has none at all.
    #[test]
    fn the_grey_handoff_knows_where_the_frame_began() {
        assert!(
            include_str!("mod.rs").contains("self.step_off_a_greyed_control(ctx, focus_before)"),
            "the frame's starting focus has to reach the hand-off"
        );
        let wall = include_str!("wall.rs");
        let guard = wall.find("fn step_off_a_greyed_control(").unwrap();
        let tail = &wall[guard..guard + 900];
        assert!(tail.contains("focus_before"), "the fallback must be there");
        assert!(tail.contains("!self.modal_is_up()"), "a window owns its own focus");
        assert!(tail.contains("self.panel_anchor().is_none()"), "and so does a panel");
    }
}
