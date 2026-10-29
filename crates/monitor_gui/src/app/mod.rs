//! Application state and the egui render loop.
//!
//! The UI thread never touches a socket or a decoder: it drains the channel
//! event queues (oneshot per frame), recomputes the decoding schedule and paints
//! the grid. Everything else happens on the tokio runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(target_os = "android"))]
use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::{Align, Align2, CornerRadius, FontId, Id, Key, Layout, Modifiers, Rect, RichText, Sense, StrokeKind, vec2};
#[cfg(not(target_os = "android"))]
use egui_file_dialog::{DialogState, FileDialog, FileDialogLabels};
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
const LIVE_REPAINT: Duration = Duration::from_millis(33);

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
    pub width: Option<u32>,
    pub height: Option<u32>,
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
            width: None,
            height: None,
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
        if stats.width.is_some() {
            self.width = stats.width;
        }
        if stats.height.is_some() {
            self.height = stats.height;
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
}

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
    /// `None` when it was opened with a pointer, which moves no focus.
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
    /// The edit button that opened the window, to give it back its focus.
    ///
    /// `None` when the window was opened with a pointer: a press does not move
    /// the focus anywhere else in this program, so it must not appear to move it
    /// here either - the window closes and the ring stays where it was.
    from: Option<Id>,
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
    /// The desktop: the file its dialog chose.
    #[cfg(not(target_os = "android"))]
    File(PathBuf),
    /// Android: the name to write into the public `Download/xgview` folder.
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
    /// A sealed document being opened for the prompt that is up for it.
    Open {
        result: Receiver<Result<AppConfig, ()>>,
        origin: Option<PathBuf>,
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

/// The file dialog the export / import opens, with this program's filters and
/// wording.
///
/// Built once and kept for the life of the viewer: it remembers the directory
/// it was last left in, and its labels are read from the language pack here
/// rather than on every frame. See [`XgViewApp::ask_for_file`] for why it is
/// drawn by egui rather than by the desktop.
#[cfg(not(target_os = "android"))]
fn new_file_dialog() -> FileDialog {
    let configurations = monitor_i18n::tr("file-dialog-filter-config");
    FileDialog::new()
        .labels(file_dialog_labels())
        .add_file_filter_extensions(&configurations, vec!["seal", "json"])
        .default_file_filter(&configurations)
        .default_size([760.0, 480.0])
        .min_size([520.0, 340.0])
}

/// What the file dialog's own controls say, in the viewer's language.
///
/// Every label it draws is named here; the two it never draws - the titles of
/// the folder and multiple-selection modes, which this program does not open -
/// are left at the default by `..Default`.
#[cfg(not(target_os = "android"))]
fn file_dialog_labels() -> FileDialogLabels {
    FileDialogLabels {
        title_select_file: monitor_i18n::tr("file-dialog-title-open"),
        title_save_file: monitor_i18n::tr("file-dialog-title-save"),
        cancel: monitor_i18n::tr("file-dialog-cancel"),
        overwrite: monitor_i18n::tr("file-dialog-overwrite"),
        reload: monitor_i18n::tr("file-dialog-reload"),
        working_directory: monitor_i18n::tr("file-dialog-working-directory"),
        show_hidden: monitor_i18n::tr("file-dialog-show-hidden"),
        show_system_files: monitor_i18n::tr("file-dialog-show-system"),
        heading_pinned: monitor_i18n::tr("file-dialog-heading-pinned"),
        heading_places: monitor_i18n::tr("file-dialog-heading-places"),
        heading_devices: monitor_i18n::tr("file-dialog-heading-devices"),
        heading_removable_devices: monitor_i18n::tr("file-dialog-heading-removable"),
        home_dir: monitor_i18n::tr("file-dialog-home"),
        desktop_dir: monitor_i18n::tr("file-dialog-desktop"),
        documents_dir: monitor_i18n::tr("file-dialog-documents"),
        downloads_dir: monitor_i18n::tr("file-dialog-downloads"),
        audio_dir: monitor_i18n::tr("file-dialog-audio"),
        pictures_dir: monitor_i18n::tr("file-dialog-pictures"),
        videos_dir: monitor_i18n::tr("file-dialog-videos"),
        pin_folder: monitor_i18n::tr("file-dialog-pin"),
        unpin_folder: monitor_i18n::tr("file-dialog-unpin"),
        rename_pinned_folder: monitor_i18n::tr("file-dialog-rename"),
        selected_directory: monitor_i18n::tr("file-dialog-selected-directory"),
        selected_file: monitor_i18n::tr("file-dialog-selected-file"),
        selected_items: monitor_i18n::tr("file-dialog-selected-items"),
        file_name: monitor_i18n::tr("file-dialog-file-name"),
        file_filter_all_files: monitor_i18n::tr("file-dialog-all-files"),
        save_extension_any: monitor_i18n::tr("file-dialog-any-extension"),
        open_button: monitor_i18n::tr("file-dialog-open"),
        save_button: monitor_i18n::tr("file-dialog-save"),
        cancel_button: monitor_i18n::tr("file-dialog-cancel-button"),
        overwrite_file_modal_text: monitor_i18n::tr("file-dialog-overwrite-text"),
        err_empty_folder_name: monitor_i18n::tr("file-dialog-err-empty-folder"),
        err_empty_file_name: monitor_i18n::tr("file-dialog-err-empty-file"),
        err_directory_exists: monitor_i18n::tr("file-dialog-err-directory-exists"),
        err_file_exists: monitor_i18n::tr("file-dialog-err-file-exists"),
        ..Default::default()
    }
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
    /// The file dialog the export / import opens, and what it was opened for.
    ///
    /// Drawn by egui inside this window rather than by the desktop; `file_job`
    /// is what the answer is for, and `None` while the dialog is closed. Kept
    /// between uses so the directory it was last left in is kept. See
    /// [`Self::ask_for_file`].
    #[cfg(not(target_os = "android"))]
    file_dialog: FileDialog,
    #[cfg(not(target_os = "android"))]
    file_job: Option<FileJob>,
    /// Whether the file dialog had the keyboard when this frame's keys were
    /// handed out.
    ///
    /// Not the same question as [`Self::file_dialog_open`]: the dialog closes
    /// itself inside [`Self::poll_file_dialog`], which runs before the keys are
    /// routed, so a Back that has just closed it is still in the queue when the
    /// wall reads it - and would take the settings panel down behind it as
    /// well.
    file_dialog_keys: bool,
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
    /// The floating bar inside the magnified viewport.
    ///
    /// A press in the picture's top or bottom band brings it up, and it is
    /// answered by the wall rather than by egui's focus: it must not wake the
    /// top and bottom bars, which take their height from the wall. See
    /// [`crate::tilebar`].
    tile_bar: tilebar::Bar,
    /// The state the bar's sound control draws, and the switch the sink follows.
    sound_on: bool,
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
    /// Frames left in which an Enter is discarded.
    ///
    /// The Enter that opened a window is still in flight for a frame or two -
    /// the soft keyboard the window raises may carry one of its own - and it
    /// would confirm or activate the control the window has just put under the
    /// focus, before the viewer has read the form.
    swallow_enter: u8,
    /// A control to hand the focus back to as the next frame begins.
    ///
    /// A window gives the focus back to the button it was opened from, and
    /// that button is behind the window: asking for it on the frame the window
    /// closes would be a frame too late, once the panel has had its say. The
    /// request is kept and applied at the start of the next frame instead.
    restore_focus: Option<Id>,
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
    passphrase: Option<Passphrase>,
    /// First control of the prompt, to hand it the focus as it opens.
    passphrase_anchor: Option<Id>,
    /// The seal or open running on a thread of its own, if any. See [`Sealing`].
    sealing: Option<Sealing>,
    /// The test tone a camera's form asked for, while it is being played.
    ///
    /// It runs on the runtime rather than in the frame: a tone is a second and a
    /// half of socket work, and the window it was asked from stays open and
    /// responsive underneath it.
    talk_test: Option<Receiver<Result<(), ToneFailure>>>,
    /// Watches the prompt for the frame it appears on.
    passphrase_handoff: Handoff,
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
    camera_edit: Option<CameraEdit>,
    /// First control of the edit window, to hand it the focus as it opens.
    camera_edit_anchor: Option<Id>,
    /// Watches the edit window for the frame it appears on.
    camera_edit_handoff: Handoff,
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
mod input;
mod paging;
mod persist;
mod pickers;
mod settings;
mod streams;
mod wall;
mod windows;

impl XgViewApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
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

        theme::install(&cc.egui_ctx);
        fonts::install_for(&cc.egui_ctx, &monitor_i18n::current_language());

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
        let view = config.group();
        let start_layout =
            if orientation.is_portrait() { view.portrait_layout } else { view.layout };
        let scheduler = Scheduler::new(start_layout, view.page, view.focus, config.enabled_count());
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
        let (events_tx, events_rx) = unbounded();
        let capabilities = monitor_codec::capabilities();

        // eframe is built with the wgpu backend, so it always offers the render
        // state the plane uploads go through.
        let render_state =
            cc.wgpu_render_state.as_ref().expect("eframe is built with the wgpu backend");
        // The Windows selector falls back to WARP rather than leaving the
        // machine unable to draw (see `select_adapter`), and the wall says so:
        // a frame rate that low would otherwise be taken for the cameras'.
        #[cfg(windows)]
        let software_renderer = render_state.adapter.get_info().device_type == wgpu::DeviceType::Cpu;
        let video = VideoRenderer::new(render_state);

        let scopes = [
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
            // The magnified viewport's bar: a row across the wall, a column down
            // a turned picture. No exits - a strip has nothing above or below it
            // to walk to, and Back is the way out.
            ("tile-bar-row", ScopeDef::new(Axis::Row)),
            ("tile-bar-column", ScopeDef::new(Axis::Column)),
        ];

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
            video,
            vitals: monitor_core::vitals::Vitals::new(),
            started: Instant::now(),
            frame_clock: crate::frame::FrameClock::from_env(),
            events_tx,
            events_rx,
            #[cfg(not(target_os = "android"))]
            file_dialog: new_file_dialog(),
            #[cfg(not(target_os = "android"))]
            file_job: None,
            file_dialog_keys: false,
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
            tile_bar: tilebar::Bar::new(),
            sound_on,
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
            swallow_enter: 0,
            restore_focus: None,
            import_cameras_only: true,
            encrypt_export: true,
            passphrase: None,
            passphrase_anchor: None,
            sealing: None,
            talk_test: None,
            passphrase_handoff: Handoff::default(),
            #[cfg(target_os = "android")]
            android_boot: (false, false),
            #[cfg(target_os = "android")]
            android_boot_at: f64::NEG_INFINITY,
            #[cfg(target_os = "android")]
            editing: None,
            camera_edit: None,
            camera_edit_anchor: None,
            camera_edit_handoff: Handoff::default(),
            remove_confirm: Modal::default(),
            system_confirm: Modal::default(),
            focus_group_name: false,
            group_remove: None,
            reorder: None,
            reorder_anchor: None,
            reorder_trigger_anchor: None,
            reorder_focus: false,
            owner: Owner::Grid,
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
        if self.passphrase.is_some() {
            self.passphrase_anchor
        } else if self.remove_confirm.is_open() {
            self.remove_confirm.anchor
        } else if self.system_confirm.is_open() {
            self.system_confirm.anchor
        } else if self.camera_edit.is_some() {
            self.camera_edit_anchor
        } else if self.discovery.open {
            self.discovery.focus_anchor
        } else if self.layout_picker.is_open() {
            self.layout_picker.anchor
        } else if self.portrait_aspect_picker.is_open() {
            self.portrait_aspect_picker.anchor
        } else if self.combined_info_picker.is_open() {
            self.combined_info_picker.anchor
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
            let Some(frame) = self.manager.frames().latest(index) else {
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
            self.textures.insert(index, ChannelTexture { sequence: frame.sequence, surface });
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
        match &job {
            FileJob::Export(passphrase) => {
                // The name says which of the two the file is: the sealed export
                // carries a `.seal` and the one written in clear a `.json`, and
                // both are what the dialog's filter accepts.
                self.file_dialog.config_mut().default_file_name = if passphrase.is_some() {
                    monitor_core::EXPORT_FILE_NAME
                } else {
                    monitor_core::CONFIG_FILE_NAME
                }
                .to_owned();
                self.file_dialog.save_file();
            }
            FileJob::Import => self.file_dialog.pick_file(),
        }
        self.file_job = Some(job);
    }

    fn poll_events(&mut self) {
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
            self.channels.entry(index).or_default().apply(event);
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

    /// What the export / import pair does. The desktop picks files with the
    /// system's dialogs; Android has the fixed export folder and the system's
    /// document picker, and says so. See [`Self::ask_for_file`].
    #[cfg(not(target_os = "android"))]
    fn import_hint(&self) -> String {
        monitor_i18n::tr("about-import-hint")
    }

    /// Says what an import of `count` cameras did, naming the file it came from
    /// where the platform names one.
    ///
    /// The desktop's dialogs hand over a path, so the toast points at the file;
    /// Android's picker hands over the text alone, and the wording says so.
    #[cfg(not(target_os = "android"))]
    fn say_imported(&mut self, count: usize, origin: Option<PathBuf>) {
        let path = origin.map(|path| path.display().to_string()).unwrap_or_default();
        self.flash(
            monitor_i18n::tr_args("toast-imported", &[("count", count.into()), ("path", path.into())]),
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

impl eframe::App for XgViewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let start = self.begin_frame(ctx);
        self.pump_background(ctx);
        self.collect_events(ctx);
        let focus_before = self.route_keys(ctx);
        self.draw_chrome(ctx);
        self.draw_wall(ctx);
        self.end_frame(ctx, start, focus_before);
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
        // The Enter that opened a window is still in flight for a frame or two:
        // taken out before any control is drawn, so it cannot land on the one
        // the window has just put under the focus.
        if self.swallow_enter > 0 {
            self.swallow_enter -= 1;
            ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
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
        if self.settings_handoff.entering(self.show_settings) {
            if let Some(id) = self.settings_anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.layout_picker.handoff.entering(self.layout_picker.is_open()) {
            if let Some(id) = self.layout_picker.anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.portrait_aspect_picker.handoff.entering(self.portrait_aspect_picker.is_open()) {
            if let Some(id) = self.portrait_aspect_picker.anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.combined_info_picker.handoff.entering(self.combined_info_picker.is_open()) {
            if let Some(id) = self.combined_info_picker.anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.dialog_handoff.entering(self.discovery.open) {
            if let Some(id) = self.discovery.focus_anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.camera_edit_handoff.entering(self.camera_edit.is_some()) {
            self.swallow_enter = 10;
            if let Some(id) = self.camera_edit_anchor {
                // The caret of a text field that has never been focused starts
                // after its last character - but egui keeps the cursor in the
                // *persisted* state the field's id carries, and this form's ids
                // are the same for every camera. Left alone, the caret would sit
                // wherever the previous camera's editing left it: in the middle
                // of the next camera's name. Dropping the state hands the choice
                // back to egui, which puts it at the end.
                egui::TextEdit::store_state(ctx, id, Default::default());
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.remove_confirm.handoff.entering(self.remove_confirm.is_open()) {
            self.swallow_enter = 10;
            if let Some(id) = self.remove_confirm.anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.system_confirm.handoff.entering(self.system_confirm.is_open()) {
            self.swallow_enter = 10;
            if let Some(id) = self.system_confirm.anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.passphrase_handoff.entering(self.passphrase.is_some()) {
            self.swallow_enter = 10;
            if let Some(id) = self.passphrase_anchor {
                // The caret starts after the last character, and the field's id
                // is the same on every visit: dropping the persisted state hands
                // the choice back to egui. See the edit window for the same.
                egui::TextEdit::store_state(ctx, id, Default::default());
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
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
            if let Some(focused) = ctx.memory(|memory| memory.focused()) {
                if !self.nav.owns_now(focused) {
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
        if self.tile_bar.is_open() && self.panel_anchor().is_none() {
            // A focus on one of the bar's ids is the bar's, whether or not that
            // control is drawn this frame: the microphone a call turns into the
            // handset is one, and the handset it becomes is another. Anything
            // else goes back to the seat, where it draws nothing until an arrow
            // walks it off.
            let focused = ctx.memory(|memory| memory.focused());
            if !focused.is_some_and(tilebar::owns) {
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
            if self.talking || self.tile_bar_pinned() {
                self.tile_bar.touch(Instant::now());
            } else if left.is_zero() {
                self.tile_bar.hide();
            } else {
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
        #[cfg(not(target_os = "android"))]
        self.poll_file_dialog(ctx);

        // Anything the soft keyboard typed has to be in the input queue before
        // a widget is drawn: a text field reads the events of the frame it is
        // drawn in, so a keystroke handed over later would be a frame behind.
        #[cfg(target_os = "android")]
        crate::keyboard::drain(ctx);

        // The document an import picked, if the picker has answered since the
        // last frame. Taken before anything is drawn, so the wall it changes is
        // drawn from the new configuration on this frame.
        #[cfg(target_os = "android")]
        self.poll_picked_config();

        if ctx.input(|input| input.viewport().close_requested()) {
            self.save_now();
            self.manager.shutdown();
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
        // Read before the keys are handed out: `handle_keys` takes the ones the
        // application acts on out of the queue, and the bars still have to know
        // a viewer was there. What counts as asking for them is the whole of
        // `viewer_active`.
        self.chrome.input_seen = self.viewer_active(ctx);
        // And the press in flight is tracked before anything is drawn: the
        // release that closes the magnified viewport's bar is read against it.
        self.track_tap(ctx);
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

        self.poll_events();
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
        let bars = self.chrome_visible();
        if bars {
            let toolbar = egui::TopBottomPanel::top("xgview-toolbar").show(ctx, |ui| self.toolbar(ui));
            let status = egui::TopBottomPanel::bottom("xgview-status").show(ctx, |ui| self.status_bar(ui));
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
        if bands_hidden && !self.scheduler.is_zoomed() {
            if let Some((top, bottom)) = self.chrome.bars {
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
            .frame(egui::Frame::default().fill(theme::BACKGROUND))
            .show(ctx, |ui| {
                let area = ui.available_rect_before_wrap();
                self.draw_grid(ui);
                // The floating page arrows sit at the very edges the magnified
                // viewport's bar is docked to, and are drawn after the wall - so
                // they would lie on top of it. The bar is the surface in use
                // while it is up, and the bars' own page buttons are still
                // there; the arrows come back with the picture once it goes.
                if self.is_portrait() && self.chrome.visible && !self.tile_bar.is_open() {
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
fn drag_frame(input: &egui::InputState, area: Rect) -> Option<egui::Vec2> {
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
            Key::Num4,
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
}
