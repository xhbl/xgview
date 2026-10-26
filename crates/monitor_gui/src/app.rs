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
use monitor_core::pipeline::{CallFailure, ChannelManager, StreamEvent};
use monitor_core::scheduler::Scheduler;
use monitor_core::talk::TalkRoute;
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
            }
            StreamEvent::Stats { stream, codec, width, height, hardware, audio, fps, bitrate_kbps, total_frames, .. } => {
                self.stream = Some(stream);
                if codec.is_some() {
                    self.codec = codec;
                }
                self.hardware = hardware;
                self.audio = audio;
                if width.is_some() {
                    self.width = width;
                }
                if height.is_some() {
                    self.height = height;
                }
                self.fps = fps;
                self.bitrate_kbps = bitrate_kbps;
                self.total_frames = total_frames;
                // The new stream delivered data: the held frame can be dropped.
                if total_frames > 0 {
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
    /// The remove button that opened the window, to give it back its focus.
    ///
    /// `None` when the window was opened with a pointer - see
    /// [`CameraEdit::from`].
    from: Option<Id>,
}

/// A reboot the settings panel is asking confirmation for.
#[derive(Debug)]
struct SystemConfirm {
    /// The button that opened the window, to give it back its focus.
    ///
    /// `None` when the window was opened with a pointer - see
    /// [`CameraEdit::from`].
    from: Option<Id>,
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
    layout_picker: bool,
    /// Tab the settings panel is on.
    settings_tab: SettingsTab,
    /// First control of the settings panel, so the panel can be handed the
    /// remote's focus the moment it opens.
    settings_anchor: Option<Id>,
    /// The panel and the device window, watched for the frame they appear on.
    settings_handoff: Handoff,
    /// The layout picker, watched for the frame it appears on so the remote's
    /// focus lands on its first entry instead of walking the wall behind it.
    layout_picker_handoff: Handoff,
    layout_picker_anchor: Option<Id>,
    /// The button that opened the layout picker, to give it back its focus when
    /// the picker closes - the toolbar's or the settings panel's `…`.
    layout_picker_from: Option<Id>,
    /// Whether the portrait aspect picker is open: the modal that lists the
    /// cameras on the current page and lets the remote cycle each one's fit.
    portrait_aspect_picker: bool,
    portrait_aspect_picker_handoff: Handoff,
    portrait_aspect_picker_anchor: Option<Id>,
    portrait_aspect_picker_from: Option<Id>,
    /// Whether the combined-info editor is open, and which slot (0-3) it edits.
    combined_info_picker: bool,
    combined_info_picker_slot: usize,
    combined_info_picker_handoff: Handoff,
    combined_info_picker_anchor: Option<Id>,
    combined_info_picker_from: Option<Id>,
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
    remove_confirm: Option<RemoveConfirm>,
    /// First control of the remove window, to hand it the focus as it opens.
    remove_confirm_anchor: Option<Id>,
    /// Watches the remove window for the frame it appears on.
    remove_confirm_handoff: Handoff,
    /// A system action (exit or reboot) the settings panel is asking about.
    ///
    /// Asked for in a window, so that a press on a button beside its neighbour
    /// cannot leave the program or restart the machine on its own.
    system_confirm: Option<SystemConfirm>,
    /// First control of the system confirm window, to hand it the focus.
    system_confirm_anchor: Option<Id>,
    /// Watches the system confirm window for the frame it appears on.
    system_confirm_handoff: Handoff,
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
            layout_picker: false,
            settings_tab: SettingsTab::default(),
            settings_anchor: None,
            settings_handoff: Handoff::default(),
            layout_picker_handoff: Handoff::default(),
            layout_picker_anchor: None,
            layout_picker_from: None,
            portrait_aspect_picker: false,
            portrait_aspect_picker_handoff: Handoff::default(),
            portrait_aspect_picker_anchor: None,
            portrait_aspect_picker_from: None,
            combined_info_picker: false,
            combined_info_picker_slot: 0,
            combined_info_picker_handoff: Handoff::default(),
            combined_info_picker_anchor: None,
            combined_info_picker_from: None,
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
            remove_confirm: None,
            remove_confirm_anchor: None,
            remove_confirm_handoff: Handoff::default(),
            system_confirm: None,
            system_confirm_anchor: None,
            system_confirm_handoff: Handoff::default(),
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

    fn flash(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast { text: text.into(), kind, at: Instant::now(), center: false });
    }

    /// [`Self::flash`] in the middle of the screen, for news that belongs to the
    /// picture rather than to a channel - the magnified bar's fit switch.
    fn flash_centered(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast { text: text.into(), kind, at: Instant::now(), center: true });
    }

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
        } else if self.remove_confirm.is_some() {
            self.remove_confirm_anchor
        } else if self.system_confirm.is_some() {
            self.system_confirm_anchor
        } else if self.camera_edit.is_some() {
            self.camera_edit_anchor
        } else if self.discovery.open {
            self.discovery.focus_anchor
        } else if self.layout_picker {
            self.layout_picker_anchor
        } else if self.portrait_aspect_picker {
            self.portrait_aspect_picker_anchor
        } else if self.combined_info_picker {
            self.combined_info_picker_anchor
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

    /// Recomputes the decoding schedule and hands the transitions to the
    /// pipeline. Cheap and idempotent, it is only run when something changed.
    fn sync(&mut self) {
        if !self.needs_sync {
            return;
        }
        self.needs_sync = false;

        let count = self.config.enabled_count();
        if self.scheduler.channel_count() != count {
            self.scheduler.set_channel_count(count);
        }

        let cameras = self.config.active_cameras();
        let changes = self.scheduler.refresh(&cameras);
        if !changes.is_empty() {
            tracing::debug!(target: "xgview::gui", changes = changes.len(), "applying the channel schedule");
            self.manager.apply(&changes, &cameras);
        }
        // The capability question is asked after the schedule, never before:
        // magnifying reopens that channel on the main stream, and the probe has
        // to ask about the stream a call would go to. Both go down the same
        // command channel in the order they are sent, so this one finds the
        // channel already pulling it.
        if let Some(index) = self.scheduler.zoom() {
            self.manager.probe_talk(index);
        }
        self.channels.retain(|index, _| *index < count);
        // A channel beyond the grid is gone for good, and its textures with it.
        let dropped: Vec<usize> =
            self.textures.keys().copied().filter(|index| *index >= count).collect();
        for index in dropped {
            self.video.release(index);
            self.textures.remove(&index);
        }

        let layout = self.scheduler.layout();
        let page = self.scheduler.page();
        let focus = self.scheduler.focus();
        let portrait = self.orientation.is_portrait();
        let view = self.config.group_mut();
        // The grid is remembered per orientation; page and focus are shared and
        // clamped to the current grid.
        if portrait {
            view.portrait_layout = layout;
        } else {
            view.layout = layout;
        }
        view.page = page;
        view.focus = focus;
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

    fn save_now(&mut self) {
        if self.persisted == self.config {
            self.dirty = false;
            return;
        }
        match self.config.save(&self.config_path) {
            Ok(()) => {
                self.persisted = self.config.clone();
                self.dirty = false;
                self.dirty_since = None;
            }
            Err(err) => {
                self.dirty = false;
                self.dirty_since = None;
                self.flash(
                    monitor_i18n::tr_args(
                        "toast-config-save-failed",
                        &[
                            ("path", self.config_path.display().to_string().into()),
                            ("error", err.to_string().into()),
                        ],
                    ),
                    ToastKind::Error,
                );
            }
        }
    }

    fn autosave(&mut self) {
        if !self.dirty {
            return;
        }
        if self.persisted == self.config {
            self.dirty = false;
            return;
        }
        if let Some(since) = self.dirty_since {
            if since.elapsed() < SAVE_DEBOUNCE {
                return;
            }
        }
        self.save_now();
    }

    // ------------------------------------------------------------ navigation

    fn set_layout(&mut self, layout: GridLayout) {
        if self.scheduler.layout() == layout && !self.scheduler.is_zoomed() {
            return;
        }
        self.tile_bar.hide();
        self.scheduler.zoom_out();
        self.scheduler.set_layout(layout);
        self.slide = Slide::default();
        self.needs_sync = true;
        self.mark_dirty();
        self.apply_audio();
    }

    /// Whether the wall stands upright. False whenever the device cannot.
    fn is_portrait(&self) -> bool {
        self.orientation.is_portrait()
    }

    /// Scales a bar's text and spacing down on a screen narrower than the
    /// reference width. The larger screen dimension is used so portrait and
    /// landscape on the same device share the same scale.
    fn scale_bar(ui: &mut egui::Ui) {
        let size = ui.ctx().screen_rect().size();
        let width = size.x.max(size.y);
        let scale = (width / TOOLBAR_REFERENCE_WIDTH).clamp(TOOLBAR_MIN_SCALE, 1.0);
        if scale < 1.0 {
            let style = ui.style_mut();
            for font in style.text_styles.values_mut() {
                font.size *= scale;
            }
            style.spacing.button_padding.x *= scale;
            style.spacing.interact_size.x *= scale;
            style.spacing.item_spacing.x = (style.spacing.item_spacing.x * scale).max(2.0);
        }
    }

    /// Switches the wall between landscape and portrait.
    ///
    /// The grid of the orientation being left is parked first - through
    /// [`Self::sync`], which writes the scheduler's layout back to the field of
    /// the current orientation - so the other side's grid is not overwritten.
    /// The grid of the orientation being entered is loaded next. Page and focus
    /// are shared and are clamped to the new grid.
    fn set_orientation(&mut self, portrait: bool) {
        let wanted = if portrait && self.supports_portrait {
            Orientation::Portrait
        } else {
            Orientation::Landscape
        };
        if self.orientation == wanted {
            return;
        }
        self.needs_sync = true;
        self.sync();
        self.orientation = wanted;
        self.tile_bar.hide();
        let view = self.config.group().clone();
        let layout = if wanted.is_portrait() { view.portrait_layout } else { view.layout };
        self.scheduler.zoom_out();
        self.scheduler.set_layout(layout);
        self.slide = Slide::default();
        // The settings panel and the layout picker are built for landscape.
        self.show_settings = false;
        self.layout_picker = false;
        self.portrait_aspect_picker = false;
        self.combined_info_picker = false;
        self.config.android_orientation = wanted;
        self.needs_sync = true;
        self.mark_dirty();
        self.apply_audio();
    }

    /// The orientation button, shared by both bars: the shape is the mode it
    /// switches to. Drawn only where the device can stand upright.
    fn orientation_button(&mut self, ui: &mut egui::Ui, items: &mut Vec<(Id, Rect)>) {
        if !self.supports_portrait {
            return;
        }
        let (icon, key, tip) = if self.is_portrait() {
            (Icon::Landscape, "toolbar-orientation-landscape", "toolbar-orientation-landscape-tip")
        } else {
            (Icon::Portrait, "toolbar-orientation-portrait", "toolbar-orientation-portrait-tip")
        };
        let response = icons::button(ui, icon, false, &monitor_i18n::tr(tip));
        items.push((response.id, response.rect));
        self.name(&response, key);
        if response.clicked() {
            self.set_orientation(!self.is_portrait());
        }
    }

    fn turn_page(&mut self, forward: bool, keyboard: bool) -> bool {
        let previous = self.scheduler.page();
        let changed = if forward { self.scheduler.next_page() } else { self.scheduler.prev_page() };
        if changed {
            let layout = self.scheduler.layout();
            let total = self.config.enabled_count();
            let outgoing = PageView { layout, cells: grid::page_cells(layout, previous, total) };
            self.slide.start(if forward { 1.0 } else { -1.0 }, Some(outgoing));
            self.needs_sync = true;
            self.mark_dirty();
            if keyboard {
                self.last_turn = Some(forward);
            }
        }
        self.slide.drag = 0.0;
        changed
    }

    /// Walks the grid one cell, with the slide a page turn uses.
    ///
    /// Only the grid: a magnified viewport is not a grid and steps along the
    /// channel list instead. See [`Self::step_zoom`].
    fn navigate(&mut self, dir: Direction) -> NavigateOutcome {
        let previous_page = self.scheduler.page();
        let outcome = self.scheduler.navigate(dir);
        if matches!(outcome, NavigateOutcome::Blocked) {
            return outcome;
        }

        let sign = if dir == Direction::Left { -1.0 } else { 1.0 };
        let outgoing = if matches!(outcome, NavigateOutcome::PageTurned { .. }) {
            let layout = self.scheduler.layout();
            let total = self.config.enabled_count();
            Some(PageView { layout, cells: grid::page_cells(layout, previous_page, total) })
        } else {
            None
        };
        if let Some(outgoing) = outgoing {
            self.slide.start(sign, Some(outgoing));
        }
        self.needs_sync = true;
        self.mark_dirty();
        outcome
    }

    /// Moves the wall one step the way a swipe or a page key asked for: the
    /// page turn on the grid, the channel step while magnified.
    fn advance(&mut self, forward: bool, keyboard: bool) -> bool {
        if self.scheduler.is_zoomed() {
            self.step_zoom(forward)
        } else {
            self.turn_page(forward, keyboard)
        }
    }

    /// Whether the page-turn buttons have a step to take.  Magnified, the
    /// boundary is the channel list; on the grid it is the page count.
    fn can_advance(&self, forward: bool) -> bool {
        if let Some(zoom) = self.scheduler.zoom() {
            if forward {
                zoom + 1 < self.scheduler.channel_count()
            } else {
                zoom > 0
            }
        } else {
            let info = self.scheduler.page_info();
            if forward { info.has_next() } else { info.has_previous() }
        }
    }

    /// Steps the magnified viewport to the previous or next channel, with the
    /// slide a page turn uses.
    ///
    /// The magnified view is one channel, not a grid, so its step is the
    /// channel order the swipe preview already showed - see
    /// [`Scheduler::zoom_step`] - and the ends are a stop. Without this a swipe
    /// turned the grid page behind the magnification: the picture did not
    /// change, and leaving the magnification landed on a different camera.
    fn step_zoom(&mut self, forward: bool) -> bool {
        let Some(current) = self.scheduler.zoom() else {
            return false;
        };
        // The bar belongs to one picture: stepping to the next one leaves it
        // behind, the way the window a pinch left behind is.
        self.tile_bar.hide();
        if !self.scheduler.zoom_step(forward) {
            self.slide.drag = 0.0;
            return false;
        }
        let outgoing = PageView { layout: GridLayout::G1x1, cells: vec![Some(current)] };
        self.slide.start(if forward { 1.0 } else { -1.0 }, Some(outgoing));
        // The channel that slides in comes up whole, the way a magnify does: the
        // window was the last camera's, and a part of it is not a part of this
        // one.
        self.picture_zoom = PictureZoom::FULL;
        self.needs_sync = true;
        self.mark_dirty();
        // The viewport is a different camera now, and the sound and the back
        // channel follow the viewport: without this the new camera never asked
        // its session for a back channel, so its bar carried no microphone, and
        // the sound stayed on the camera that was left.
        self.apply_audio();
        self.settle_tile_bar();
        true
    }

    /// Hands the remote's focus from the wall to the top bar.
    ///
    /// The bar is where the settings panel and the device window are opened
    /// from, and a television remote has no F1: the arrow that walks off the top
    /// of the wall is the only way in. The focus lands above the tile the viewer
    /// was on rather than at one end of the row, because the row is long and the
    /// walk back to the other end is one press per control.
    fn enter_toolbar(&mut self, ctx: &egui::Context) {
        let nearest = match self.grid_focus_x {
            Some(x) => self
                .toolbar_items
                .iter()
                .min_by(|(_, left), (_, right)| {
                    (left.center().x - x).abs().total_cmp(&(right.center().x - x).abs())
                })
                .map(|(id, _)| *id),
            None => self.toolbar_items.first().map(|(id, _)| *id),
        };
        if let Some(id) = nearest {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    /// Hands the remote's focus from the wall to the author in the status bar.
    ///
    /// The status row holds the one control below the wall, so the arrow that
    /// walks off the bottom of the wall is the way onto it - the mirror of the
    /// arrow that walks off the top into the bar.
    fn enter_status(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.status_author {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    /// Hands the remote's focus back to the wall.
    fn leave_controls(&mut self, ctx: &egui::Context) {
        if let Some(id) = ctx.memory(|memory| memory.focused()) {
            ctx.memory_mut(|memory| memory.surrender_focus(id));
        }
    }

    /// Whether the top bar is what currently has the remote's focus.
    fn toolbar_has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| memory.focused())
            .is_some_and(|focused| self.toolbar_items.iter().any(|(id, _)| *id == focused))
    }

    /// Whether the author in the status bar is what currently has the remote's
    /// focus.
    fn status_has_focus(&self, ctx: &egui::Context) -> bool {
        self.status_author
            .is_some_and(|id| ctx.memory(|memory| memory.focused()) == Some(id))
    }

    /// Whether one of the two page-turn buttons is where the remote's focus is.
    ///
    /// The pair is the landscape toolbar's and the upright bar's floating arrows
    /// alike - whichever bar was up last set the two ids, so this answers for
    /// either. What it guards is the bounce a turn at the edge makes; see
    /// [`Self::turn_page`].
    fn page_button_has_focus(&self, ctx: &egui::Context) -> bool {
        Self::focus_on_page_button(
            ctx.memory(|memory| memory.focused()),
            self.page_prev_id,
            self.page_next_id,
        )
    }

    /// Whether `focused` is one of the two page-turn buttons.
    ///
    /// A page key turns the page from wherever the focus is; only a focus that
    /// stood on a page button follows the turn past the edge into the other
    /// one, because the turn is what took the button it stood on away. A tile's
    /// focus, or a focus on anything else in the bars, turns the page and stays
    /// where it was put.
    fn focus_on_page_button(focused: Option<Id>, prev: Option<Id>, next: Option<Id>) -> bool {
        focused.is_some_and(|focused| Some(focused) == prev || Some(focused) == next)
    }

    /// Hands the remote's focus from the wall to the bar an arrow walked off it
    /// for.
    ///
    /// Only into a bar that is on screen. With the bars hidden there is nothing
    /// on the far side of the wall to land on, and waking them for an arrow that
    /// walked off it is exactly the churn this is meant to stop: the bars come
    /// back for Back, for a function key, or for a press in their band, and the
    /// arrow finds them then. See [`Self::viewer_active`].
    fn hand_off(&mut self, ctx: &egui::Context, to: Bar) {
        if !self.chrome.visible {
            return;
        }
        match to {
            Bar::Toolbar => self.enter_toolbar(ctx),
            Bar::Status => self.enter_status(ctx),
        }
    }

    fn zoom_in(&mut self) {
        if self.scheduler.zoom_in() {
            // The viewport comes up whole: the window the last one was left at is
            // a part of another picture, and there is none of this one to keep.
            self.picture_zoom = PictureZoom::FULL;
            self.needs_sync = true;
            self.mark_dirty();
            self.apply_audio();
        }
        self.settle_tile_bar();
    }

    fn zoom_out(&mut self) {
        if self.scheduler.zoom_out() {
            self.needs_sync = true;
            self.mark_dirty();
            self.apply_audio();
        }
        self.settle_tile_bar();
    }

    fn zoom_to(&mut self, index: usize) {
        // A double press toggles, the way Enter does: the magnified viewport
        // goes back to the grid, and any other one is magnified in its place.
        if self.scheduler.zoom() == Some(index) {
            self.zoom_out();
        } else {
            self.scheduler.set_focus(Some(index));
            self.zoom_in();
        }
    }

    /// Points the sink at the magnified camera, or at nothing when the grid is
    /// showing, and follows the sound switch. Called whenever the zoom or the
    /// switch changes; the hub's index is the camera the listener is listening
    /// to, and the sink's active flag is whether anything comes out at all.
    ///
    /// The talk-back target moves with it: only the magnified session asks for a
    /// back channel, so naming the camera here is what opens one there - and a
    /// microphone left open on the camera the viewer just left is closed.
    ///
    /// **A call mutes the speaker, unless the platform cancels the echo.** The
    /// speaker and the microphone are one device, and a speaker playing into the
    /// microphone beside it is a loop: measured on a phone, the speaker came back
    /// through the microphone at a peak of 3003 while the room sat at 150-300,
    /// which is the howl an echo canceller exists to break. Where the platform
    /// has one and the capture came up with it - Android's `AudioRecord` on
    /// `VOICE_COMMUNICATION` with the mode in `MODE_IN_COMMUNICATION` and the
    /// speakerphone forced on, playing back through an `AudioTrack` on the same
    /// path - the camera keeps playing and the call is full duplex;
    /// [`Self::call_mutes_speaker`] is what says so. Everywhere else the loop is
    /// broken the other way round - half duplex, the way a walkie-talkie is:
    /// while the microphone is open the camera's sound is muted, and hanging up
    /// brings it back. The sound switch is left as the viewer set it. See
    /// [`Self::set_talking`].
    fn apply_audio(&mut self) {
        if let Some(sink) = &self.audio {
            sink.set_active(self.sound_on && !self.call_mutes_speaker());
            sink.set_volume(self.config.volume);
        }
        let index = self.scheduler.zoom();
        self.manager.audio().set_index(index);
        self.manager.set_talk(index);
        if self.talking && self.talk_index != index {
            self.set_talking(false);
        }
    }

    /// Whether the call in progress has to silence the speaker to keep it quiet.
    ///
    /// A call is full duplex by default: the camera's sound keeps playing while
    /// the microphone is open, which is what Android's processor does properly
    /// and what a desktop's separated speaker and microphone manage without it.
    /// A machine with the microphone beside its speaker can be made half duplex
    /// instead - the sound stops while the viewer talks - and that is the
    /// Audio tab's switch, `AppConfig::talk_mute`. See [`Self::apply_audio`].
    fn call_mutes_speaker(&self) -> bool {
        self.talking && self.config.talk_mute
    }



    /// Switches the microphone on or off.
    ///
    /// Switching it on for the first time opens the device; a machine without
    /// one says so once and then stays silent - the picture is never affected.
    /// On Android the record permission is asked for the same way: the first
    /// press puts the system dialog up and says what to do about it, and the
    /// next press finds the capture open.
    fn set_talking(&mut self, on: bool) {
        if on {
            if self.mic.is_none() {
                match crate::mic::Mic::open(
                    self.manager.talk().clone(),
                    self.config.audio_input.as_deref(),
                ) {
                    Ok(mic) => self.mic = Some(mic),
                    Err(crate::mic::MicError::Permission) => {
                        self.flash(monitor_i18n::tr("toast-microphone-permission"), ToastKind::Error);
                        return;
                    }
                    Err(crate::mic::MicError::NoDevice) => {
                        self.flash(monitor_i18n::tr("toast-no-microphone"), ToastKind::Error);
                        return;
                    }
                }
            }
        }
        self.talking = on;
        self.talk_index = on.then(|| self.scheduler.zoom()).flatten();
        self.manager.talk().set_talking(on);
        // The press is the call: a session of its own, opened while the
        // microphone is already pushing. The hub was told first, so the worker
        // that opens it finds a target and a queue to feed.
        if on {
            if let Some(index) = self.talk_index {
                self.manager.call(index);
            }
        }
        // The speaker goes quiet while the microphone is open, unless the
        // platform's canceller came with the microphone: it is the same device,
        // and without a canceller the sound it makes comes straight back in
        // through the microphone. See `apply_audio`.
        if let Some(sink) = &self.audio {
            sink.set_active(self.sound_on && !self.call_mutes_speaker());
            sink.set_volume(self.config.volume);
        }
        tracing::debug!(target: "xgview::talk", on, "the talk-back changed");
        // The bar now carries the one control that ends the call instead of the
        // microphone the press landed on, so the focus it had is gone with it;
        // the bar's seat puts the remote back on the strip without a ring, and
        // the first arrow is what reaches the handset. See `tilebar`.
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, enabled: bool) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(enabled));
        self.fullscreen = enabled;
        // A window created full screen was never given a windowed size - asking
        // for one at creation is what drops it back out of full screen, see
        // `native_options` - so the first time full screen is left it is handed
        // one rather than coming back at whatever size it happens to be.
        if !enabled && std::mem::take(&mut self.windowed_size_pending) {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                crate::WINDOWED_SIZE[0],
                crate::WINDOWED_SIZE[1],
            )));
        }
        // Leaving full screen brings the bars back on an explicit action, not
        // on a timer: the viewer is about to use them. Entering it is the
        // opposite request - the picture and nothing else - so they go at once
        // rather than after the idle delay. A configured idle of zero means the
        // bars never hide, so entering full screen leaves them up as well.
        let hide = enabled && self.config.chrome_idle_secs > 0.0;
        self.chrome.visible = !hide;
        self.chrome.hide_now = hide;
        self.chrome.last_input = Instant::now();
        // The option to stay on top is about the wall that has the screen to
        // itself, so entering and leaving full screen is when it moves.
        self.apply_window_level(ctx);
    }

    /// Asks the window manager for the window level the current state wants.
    ///
    /// Sent when something the answer is decided from moves - the option, full
    /// screen, a suspension - rather than every frame: it is a request to the
    /// window manager, and one per frame would be a repaint's worth of noise for
    /// an answer that has not changed.
    fn apply_window_level(&mut self, ctx: &egui::Context) {
        let level = window::level(
            window::supported(),
            self.config.always_on_top_fullscreen,
            self.fullscreen,
            self.on_top,
            Instant::now(),
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    }

    /// Says what the window's place among the others is doing from now on.
    ///
    /// Used around anything the viewer opened that has to come up in front of
    /// the wall; [`crate::window`] describes the two shapes its end can take.
    /// Nothing is remembered where the platform cannot hold a window above the
    /// others, since nothing can be asked of it.
    fn set_on_top(&mut self, ctx: &egui::Context, on_top: OnTop) {
        if !window::supported() {
            return;
        }
        self.on_top = on_top;
        if let OnTop::SuspendedUntil(until) = on_top {
            // Nothing else would wake the app to put the wall back when the
            // clock runs out.
            ctx.request_repaint_after(until.saturating_duration_since(Instant::now()));
        }
        self.apply_window_level(ctx);
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

    /// Draws the file dialog while it is up, and acts on what it answers.
    #[cfg(not(target_os = "android"))]
    fn poll_file_dialog(&mut self, ctx: &egui::Context) {
        // The frame it goes up, whatever held the focus hands it over. The
        // button that opened the dialog is behind it now, and the dialog reads
        // its own keys only while no widget has the focus - so an Esc still
        // belonging to that button would be read by nobody, and the first press
        // would be spent doing nothing.
        if self.file_job.is_some() && !self.file_dialog_keys {
            let held = ctx.memory(|memory| memory.focused());
            // The control the export or import was asked from is the one to go
            // back to. Both buttons remember themselves when pressed - but only
            // for a keyboard or a remote, so this is `None` when the dialog came
            // from a pointer and the focus it took is the one it hands back:
            // nothing visibly moves either way.
            self.file_dialog_anchor = self.file_dialog_anchor.or(held);
            if let Some(focused) = held {
                ctx.memory_mut(|memory| memory.surrender_focus(focused));
            }
        }
        self.file_dialog_keys = self.file_job.is_some();
        if self.file_job.is_none() {
            return;
        }
        self.file_dialog.update(ctx);
        if let Some(path) = self.file_dialog.take_picked() {
            match self.file_job.take() {
                Some(FileJob::Export(passphrase)) => self.export_to(&path, passphrase.as_deref()),
                Some(FileJob::Import) => self.import_from(path),
                None => {}
            }
        } else if matches!(
            self.file_dialog.state(),
            DialogState::Closed | DialogState::Cancelled
        ) {
            // The viewer backed out of it: nothing is imported or exported, and
            // there is nothing to say about it.
            self.file_job = None;
        }
        // It is down now, so the control behind it takes the focus back - the
        // way it would have had the dialog not been opened at all. An import
        // that turned out to want the passphrase anchors itself instead.
        if self.file_job.is_none() && self.passphrase.is_none() {
            if let Some(anchor) = self.file_dialog_anchor.take() {
                self.restore_focus = Some(anchor);
            }
        }
    }

    /// Opens what Android has instead of a file dialog: the export is written
    /// straight into the public folder, and the import hands the picker to
    /// another activity, whose answer [`Self::poll_picked_config`] takes.
    #[cfg(target_os = "android")]
    fn ask_for_file(&mut self, job: FileJob) {
        match job {
            FileJob::Export(passphrase) => self.export_config(passphrase.as_deref()),
            FileJob::Import => self.import_config(),
        }
    }

    /// Whether the file dialog is up. It owns the screen while it is: the
    /// pointer, the focus and the keyboard all belong to it.
    fn file_dialog_open(&self) -> bool {
        #[cfg(not(target_os = "android"))]
        {
            self.file_job.is_some()
        }
        #[cfg(target_os = "android")]
        {
            false
        }
    }

    /// Whether what the viewer did this frame is a request for the bars.
    ///
    /// Called before the keys are handed out: the application takes the ones it
    /// acts on out of the queue, so a key this has to answer on must be read
    /// here or not at all.
    ///
    /// There are exactly three ways to ask for the bars, and everything else is
    /// the wall:
    ///
    /// * a press in the band a bar occupies - the pointer's way in, and the only
    ///   one a finger has;
    /// * Back, which is the one key on a remote control that reads as "the
    ///   menus";
    /// * the function keys, which are what opens a panel in the first place.
    ///
    /// None of the rest of it - walking the channels, turning a page, magnifying
    /// one, picking a grid - is a request for the bars, and treating it as one is
    /// what made the picture resize under the viewer every time they touched it:
    /// the bars take their height from the wall rather than lying over it, so
    /// bringing them back for a page turn moves every tile, and then they slide
    /// away again 3.5 seconds later. See [`Self::wall_event`] for the whole of
    /// the decision, one event at a time.
    fn viewer_active(&self, ctx: &egui::Context) -> bool {
        // The magnified viewport's bar is the picture's own surface, not one of
        // the wall's bars: using it is the viewer at the picture. Waking the
        // bars for it would take the next press out of the strip's hands - and
        // move the wall under it - so none of it counts as asking for them. That
        // holds while the remote is *in* the strip: a strip that is merely up,
        // the upright default, leaves the remote on the wall, and an arrow off
        // it is still a press that asks for the bars.
        if self.tile_bar.is_open()
            && ctx.memory(|memory| memory.focused()).is_some_and(tilebar::owns)
        {
            return false;
        }
        // The remote's focus is in a bar, in the settings panel or in a window:
        // the viewer is in the menus, and everything they do belongs to them.
        if self.owner != Owner::Grid {
            return ctx.input(Self::any_input);
        }
        ctx.input(|input| {
            // While the picture is magnified and the bars are away, the band
            // along its top and bottom edge is the viewport's own bar being
            // asked for, not the menus: the two are the same part of the screen,
            // and one press cannot be both. With the bars on screen their band is
            // their own as it always was.
            let band_is_a_door = !self.scheduler.is_zoomed() || self.chrome.visible;
            input.events.iter().any(|event| {
                Self::wall_event(self.chrome.bars, self.back_is_a_door(), band_is_a_door, event)
            })
        })
    }

    /// Remembers where a press went down, so the release can be read as a tap.
    ///
    /// egui forgets a press's start time on the frame the press ends, which is
    /// exactly the frame the answer is needed on; the press is therefore kept
    /// here while it lasts. A second finger while the press is down is a pinch -
    /// or the beginning of one - and is remembered too, because the touches are
    /// gone by the release.
    fn track_tap(&mut self, ctx: &egui::Context) {
        ctx.input(|input| {
            if input.pointer.any_pressed() {
                if let Some(origin) = input.pointer.press_origin() {
                    self.tap_press = Some((origin, input.time));
                    self.tap_multi = false;
                }
            }
            if input.multi_touch().is_some() {
                self.tap_multi = true;
            }
        });
    }

    /// Where a tap ended this frame, if the press was one.
    ///
    /// A tap is a press that came up quickly, stayed where it went down, and was
    /// the only finger on the glass. A press that lingered is a thumb resting on
    /// the picture, one that slid is a pan being reached for, and a pinch is two
    /// fingers: the magnified viewport's bar goes on none of them. See
    /// [`TAP_SECS`] and [`TAP_POINTS`].
    fn tap_release(&mut self, ctx: &egui::Context) -> Option<egui::Pos2> {
        let (origin, started) = self.tap_press?;
        let released = ctx.input(|input| input.pointer.any_released());
        if !released {
            return None;
        }
        // The press is over either way, so it is never read twice.
        self.tap_press = None;
        let multi = std::mem::take(&mut self.tap_multi);
        if multi {
            return None;
        }
        ctx.input(|input| {
            let at = input.pointer.interact_pos()?;
            let quick = input.time - started <= TAP_SECS;
            let still = (at - origin).length() <= TAP_POINTS;
            (quick && still).then_some(at)
        })
    }

    /// Whether a Back, on this frame, is the viewer asking for the bars.
    ///
    /// Back unwinds one layer at a time - the magnified viewport, then whatever
    /// is open over the wall - and a Back that does that is the wall's, which is
    /// not a request for the bars. With nothing left to unwind it is the viewer
    /// asking for the menus instead, and it is the one key a remote control has
    /// that reads that way: the bars are where the settings, the device window
    /// and full screen are, and a remote has no function keys at all. See
    /// [`Self::back`], which leaves the exit question to a Back that arrives
    /// with the bars already on screen.
    fn back_is_a_door(&self) -> bool {
        !self.scheduler.is_zoomed() && !self.discovery.open && !self.show_settings
    }

    /// Whether one event, with the remote's focus on the wall, asks for the
    /// bars. The wall's whole answer, one event at a time; see
    /// [`Self::viewer_active`] for why.
    ///
    /// `back_is_a_door` is [`Self::back_is_a_door`], read once for the frame.
    ///
    /// `band_is_a_door` is whether a press in the band a bar sits in is that
    /// bar's: true on the grid, and true while magnified with the bars on
    /// screen, where the wall's own band lies below theirs. It is false only in
    /// the one state where the two bands are the same part of the screen - the
    /// magnified viewport with the bars away - and there the band is the
    /// viewport's own bar being asked for. The menus are still reachable there:
    /// `F1`/`F2`, or Back a layer at a time. See [`crate::tilebar`].
    fn wall_event(
        bars: Option<(Rect, Rect)>,
        back_is_a_door: bool,
        band_is_a_door: bool,
        event: &egui::Event,
    ) -> bool {
        match event {
            // Back is the wall's own key while there is something on the wall
            // for it to unwind - see `back` - and the viewer asking for the
            // menus when there is not.
            egui::Event::Key { key, pressed: true, .. } => {
                if matches!(*key, Key::Escape | Key::Backspace | Key::BrowserBack) {
                    back_is_a_door
                } else {
                    !Self::wall_key(*key)
                }
            }
            egui::Event::Key { pressed: false, .. } => false,
            // A press on the wall is a press on the picture - a tile to focus,
            // or a double press to magnify it - and a press in the band a bar
            // sits in is the pointer asking for that bar. A release is the tail
            // of a press that has already been answered, and never a request of
            // its own.
            //
            // Magnified with the bars away, that band belongs to the viewport's
            // own bar; see above.
            egui::Event::PointerButton { pos, pressed: true, .. } => {
                band_is_a_door && Self::in_a_bar_band(bars, *pos)
            }
            egui::Event::PointerButton { pressed: false, .. } => false,
            // A touch is placed by where it is, whatever phase it is in. A tap
            // on a touch screen arrives as a `Start` and an `End` at the same
            // place, and the end is not a second request for the bars - reading
            // it as one is what brought the bars back under every touch on the
            // wall, however carefully its start had been placed.
            egui::Event::Touch { pos, .. } => band_is_a_door && Self::in_a_bar_band(bars, *pos),
            // What a printable key arrives as *on top of* its key event, which
            // is why the grid digits are read as keys above: a character typed
            // at the wall does nothing at all - there is no field on the wall to
            // type into - and counting it would put the bars back every time the
            // viewer picked a grid with `1`, `2`, `3` or `4`.
            egui::Event::Text(_) => false,
            egui::Event::PointerMoved(_)
            | egui::Event::MouseMoved(_)
            | egui::Event::WindowFocused(_)
            | egui::Event::Screenshot { .. }
            | egui::Event::PointerGone => false,
            // A wheel and a trackpad's pinch are the viewer at the *picture*: in
            // the magnified viewport a wheel pushes it in and a drag moves it.
            // Turning one is no more a request for the bars than turning a page
            // is - the wall's own keys and a swipe are read the same way - so in
            // full screen a zoom leaves the bars to their own clock rather than
            // bringing them back, and either is answered by the picture rather
            // than by a press in the band the bars sit in.
            egui::Event::MouseWheel { .. } | egui::Event::Zoom(_) => false,
            // A dropped file, and anything an integration sends that is not named
            // here: the viewer, and none of it is something done on the wall. The
            // bars are asked for by a press in their band, by Back or by a
            // function key, and by nothing else.
            _ => true,
        }
    }

    /// Whether `input` holds anything the viewer did, as opposed to the
    /// window's own bookkeeping.
    ///
    /// Only what a person does counts. The window being resized or focused is
    /// the system, and entering full screen makes both happen; a key or a button
    /// coming back up is the tail of a press that has already counted. Reading
    /// any of it as "a viewer is here" would undo the hiding that the press
    /// itself asked for - which is exactly what entering full screen means - so
    /// the bars would come straight back.
    ///
    /// The two mouse-motion events egui sends have to be told apart, because
    /// egui sends both for one movement of the mouse:
    ///
    /// * `PointerMoved` carries a position relative to the window, so a resize
    ///   moves it under a cursor that never moved, and it cannot be trusted;
    /// * `MouseMoved` carries the raw hardware delta, which no resize produces,
    ///   and which is therefore what lets a desktop wake the bars by moving the
    ///   mouse without a change of the window's shape doing it too.
    ///
    /// This is the whole of [`Self::viewer_active`] for a viewer who is in the
    /// menus: with the focus on a control there is no wall to leave alone, and
    /// anything that keeps the control reachable keeps the bars - which the
    /// control it is part of cannot be drawn without. See
    /// [`Self::chrome_visible`].
    fn any_input(input: &egui::InputState) -> bool {
        input.pointer.any_down()
            || input.events.iter().any(|event| match event {
                egui::Event::Key { pressed, .. } => *pressed,
                egui::Event::PointerButton { pressed, .. } => *pressed,
                egui::Event::PointerMoved(_)
                | egui::Event::WindowFocused(_)
                | egui::Event::Screenshot { .. }
                | egui::Event::PointerGone => false,
                _ => true,
            })
    }

    /// Whether the keys read this frame walk the focus ring, which is what arms
    /// it.
    ///
    /// The arrows walk it - wherever it is, on the wall or on a control - and so
    /// do Back and the F1/F2 that open the bars and carry it into them. Enter and
    /// Space count only while the **wall** owns the keys (`wall_owns`): on the
    /// wall they act on what the ring is on, while on a control they activate the
    /// control - which is how a remote page button gets pressed, and that is a
    /// page turn rather than an aim. The **page keys and the layout digits are
    /// left out on purpose**: they change what the wall is showing, not where the
    /// ring is standing, and that is also what a swipe and the toolbar's page
    /// arrows do, so all three ways of turning a page behave alike. See
    /// [`Self::ring_follows_cursor`], which takes a ring that is already showing
    /// down when the wall moves its own cursor - a page turned, or a layout that
    /// re-places the focus.
    fn ring_keys(keys: &Keys, wall_owns: bool) -> bool {
        keys.left
            || keys.right
            || keys.up
            || keys.down
            || (keys.enter && wall_owns)
            || keys.back
            || keys.settings
            || keys.devices
    }

    /// Keys that act on the wall and nowhere else.
    ///
    /// Enter and Space magnify the focused viewport and unwind it again;
    /// PageUp and PageDown turn the page; the arrows move the focus, and turn
    /// the page when the focus leaves a side; the digits pick a grid, which is
    /// a shape of the wall and nothing else. The arrows that walk off the *top*
    /// and *bottom* are not the way into the bars any more either: with the bars
    /// hidden there is nothing for them to land on, and they are asked of
    /// [`Self::hand_off`], which only hands over to a bar that is on screen.
    ///
    /// Back is not here on purpose. It is the one key a remote control has that
    /// a viewer reads as "the menus" - the bars are where the settings, the
    /// device window and full screen are, and a remote has no function keys at
    /// all - so it is left to wake them, by the same rule that gives that job to
    /// F1, F2 and F11. See [`Self::viewer_active`].
    fn wall_key(key: Key) -> bool {
        matches!(
            key,
            Key::ArrowLeft
                | Key::ArrowRight
                | Key::ArrowUp
                | Key::ArrowDown
                | Key::Enter
                | Key::Space
                | Key::PageUp
                | Key::PageDown
                | Key::Num1
                | Key::Num2
                | Key::Num3
                | Key::Num4
        )
    }

    /// Whether `pos` is in the band one of the bars occupies.
    ///
    /// The bands are the ones the bars were last drawn in; see [`Chrome::bars`].
    /// Only their height is read: a bar spans the width of the window, and the
    /// rectangles can be a frame or two old - the frame the bars are drawn in to
    /// be measured, before a start in full screen takes them away again, is laid
    /// out for a window that may still be on its way to the screen it ends up
    /// on. Their height is what the viewer is aiming at either way.
    ///
    /// Before they have been drawn once there is no band at all, and a press
    /// anywhere is a press on the wall.
    fn in_a_bar_band(bars: Option<(Rect, Rect)>, pos: egui::Pos2) -> bool {
        bars.is_some_and(|(top, bottom)| {
            (top.top()..=top.bottom()).contains(&pos.y)
                || (bottom.top()..=bottom.bottom()).contains(&pos.y)
        })
    }

    /// Whether the top and bottom bars are drawn this frame.
    ///
    /// Outside full screen they always are - the window has a frame around it
    /// anyway, and the bars are where the controls live. In full screen they
    /// fade once the wall has been left alone for the configured idle time,
    /// which is the difference between a monitor and a television. The answer is
    /// kept in [`Chrome::visible`], and what the focus ring makes of it is read
    /// where it changes: the bars arm the ring as they appear and take it with
    /// them as they go, and its own clock rules in between. See
    /// [`Self::ring_follows_bars`].
    fn chrome_idle(&self) -> Duration {
        let secs = self.config.chrome_idle_secs;
        if secs <= 0.0 {
            Duration::MAX
        } else {
            Duration::from_secs_f32(secs)
        }
    }

    /// Whether the focus ring is drawn this frame.
    ///
    /// Its own clock, and only its own: armed by the keys that walk it - see
    /// [`Self::ring_keys`] - and by a click that lands on a tile, and off until
    /// the viewer makes one, which is what keeps a fresh run's wall free of a
    /// cursor. The bars have no say here; what they do to the ring they do at
    /// the moment they come and go. See [`Self::ring_follows_bars`].
    fn ring_shown(&self) -> bool {
        Self::ring_standing(self.ring_seen, Instant::now())
    }

    /// [`Self::ring_shown`], with its clock given: the rule we can test.
    fn ring_standing(armed: Option<Instant>, now: Instant) -> bool {
        armed.is_some_and(|at| now.saturating_duration_since(at) < RING_IDLE)
    }

    /// What the bars do to the ring as they come and go: they arm it as they
    /// appear, and take it away with them as they go.
    ///
    /// It is the bars' *ends* and not their state, and that is what makes the
    /// ring usable in full screen. The wall's own keys are deliberately not a
    /// request for the bars - see [`Self::viewer_active`], and the height the
    /// bars take from the wall - so a viewer driving the wall with the bars away
    /// has nothing that could bring them back, and a ring that asked whether the
    /// bars were up before it drew itself would never draw at all in the one
    /// case it exists for. Read as a gate, `chrome.visible` did exactly that.
    ///
    /// `was` is whether the bars were up as the frame *began*, and the other
    /// side of the comparison has to be the state the frame *ends* in: the bars
    /// are taken away by a key before anything is drawn (`handle_keys`) and by
    /// the toolbar's button or the Display tab's checkbox while the frame is
    /// being drawn, so a state read in the middle of the frame misses the
    /// second, and one read at the start of the next frame misses both.
    fn ring_follows_bars(seen: Option<Instant>, bars: bool, was: bool, now: Instant) -> Option<Instant> {
        match (bars, was) {
            // The bars coming up: the ring comes back with them.
            (true, false) => Some(now),
            // The bars going: they take the ring with them.
            (false, true) => None,
            // Either of them on its own: the ring keeps its own clock.
            _ => seen,
        }
    }

    /// What a change of the wall's cursor does to the ring: it takes one that is
    /// showing down.
    ///
    /// The cursor is the page together with the focused channel, and a change of
    /// either without the viewer aiming at it leaves the ring standing on the
    /// wrong tile: a page turned this frame re-places the focus into the new page
    /// (`Scheduler::normalize` keeps it on the visible one), and a layout that
    /// shrinks under the focus does the same - a 4x4's ninth channel is the third
    /// one in a 2x2 - as does a camera switched off, which clamps it. A cursor
    /// pointing at the wrong place is worse than no cursor, and the next aim
    /// lights it where it now stands. A frame that **aimed** is the exception: an
    /// arrow walking off the edge of the wall turns the page itself, and that is
    /// the viewer walking the cursor, so the ring follows it. See
    /// [`Self::ring_keys`].
    fn ring_follows_cursor(
        seen: Option<Instant>,
        cursor: (usize, Option<usize>),
        was: (usize, Option<usize>),
        aimed: bool,
    ) -> Option<Instant> {
        if cursor != was && !aimed {
            None
        } else {
            seen
        }
    }

    fn chrome_visible(&mut self) -> bool {
        // An open panel owns the bars: taking away the bar of a panel the
        // viewer just opened would strand it. The layout picker is a modal of
        // its own and counts the same way - it is opened from the toolbar, and
        // a bar that faded under it would take its own trigger away.
        if self.show_settings || self.discovery.open || self.layout_picker || self.portrait_aspect_picker || self.combined_info_picker {
            self.chrome.visible = true;
            self.chrome.hide_now = false;
            self.chrome.last_input = Instant::now();
            return true;
        }
        if !self.fullscreen {
            self.chrome.visible = true;
            self.chrome.hide_now = false;
            return true;
        }
        // No live cameras: the wall is empty, and hiding the bars would leave
        // the viewer with no way to reach the settings or the device window.
        if self.scheduler.current().and_then(|s| s.live().next()).is_none() {
            self.chrome.visible = true;
            self.chrome.hide_now = false;
            return true;
        }
        // Entering full screen asks for the picture and nothing else, and the
        // request is answered at once - except on the frames before the bars have
        // ever been drawn in this run. What they cover is only known as they are
        // laid out, and the band they leave behind is where a pointer asks for
        // them back: starting in full screen asks for the picture straight away,
        // so they are drawn once - this frame - to be measured, and taken away on
        // the next one. Waiting for a frame that draws them instead would wait
        // for ever: this is the request that keeps them from being drawn.
        if self.chrome.hide_now {
            if self.chrome.bars.is_none() {
                self.chrome.visible = true;
                return true;
            }
            self.chrome.hide_now = false;
            self.chrome.visible = false;
            return false;
        }
        if self.chrome.input_seen {
            self.chrome.last_input = Instant::now();
            self.chrome.visible = true;
        } else if self.chrome.last_input.elapsed() >= self.chrome_idle() {
            self.chrome.visible = false;
        }
        self.chrome.visible
    }

    fn back(&mut self, ctx: &egui::Context) {
        // BACK / Esc unwinds one layer at a time: the magnified viewport, then
        // whatever was opened over the wall. With nothing left to close, the
        // wall answers - and with the bars away that answer is the bars
        // themselves, which this press has already asked for; the question of
        // leaving is asked from the bars, where both it and the answer are on
        // screen. Leaving the program is not something a press reaching for the
        // menus should be able to start.
        //
        // `chrome.visible` is read here as the frame began: `chrome_visible`
        // runs after the keys have been handed out, so this is the state the
        // viewer was looking at.
        //
        // Full screen is not one of these layers any more: on a television the
        // viewer is in it from the start, so it cannot have been what the press
        // meant, and unwinding it would put an exit out of reach. F11 and the
        // settings panel's own checkbox are what toggle it.
        if self.scheduler.is_zoomed() {
            // Back unwinds the magnification a layer at a time: the bar the
            // viewport carries, and then the magnification itself. It is the one
            // key that gets out of it now - the confirm key is the bar's.
            //
            // A call is a layer of its own and Back is its way out, the same as
            // the handset: it goes before the bar, because the bar is what is
            // showing the handset.
            if self.talking {
                self.set_talking(false);
            } else if self.tile_bar.is_open() {
                self.tile_bar.hide();
            } else {
                self.zoom_out();
            }
            self.exit_armed = None;
        } else if self.discovery.open {
            self.discovery.open = false;
            self.exit_armed = None;
        } else if self.show_settings {
            self.show_settings = false;
            self.exit_armed = None;
        } else if self.chrome.visible && !self.supervised {
            self.arm_exit(ctx);
        } else {
            // Under a supervisor the wall is meant to stay up: Back is not a way
            // out of the program, only the exit button in the settings is.
            self.exit_armed = None;
        }
    }

    /// Asks before leaving the program, and leaves on the second Back.
    ///
    /// The question is drawn in the middle of the screen for [`EXIT_WINDOW`];
    /// a Back inside that window ends the program, and a Back after it has
    /// lapsed asks again rather than leaving on a press the viewer has already
    /// forgotten making.
    fn arm_exit(&mut self, ctx: &egui::Context) {
        if self.exit_armed.is_some_and(|at| at.elapsed() < EXIT_WINDOW) {
            self.quit(ctx);
            return;
        }
        self.exit_armed = Some(Instant::now());
        // The hint has to be taken down when it lapses, and a wall with no live
        // channel is not repainting on its own.
        ctx.request_repaint_after(EXIT_WINDOW);
    }

    /// Ends the program, once the viewer has confirmed it.
    ///
    /// The configuration is written and the streaming runtime stopped first:
    /// on Android the process is ended outright, and neither would otherwise
    /// happen. Ending the process, rather than only asking the viewport to
    /// close, is what Android needs - the activity's event loop cannot be built
    /// a second time in the same process, so a viewer that came back from a
    /// closed loop could never be started again. On the desktop the viewport is
    /// simply asked to close, and the close request saves and stops the runtime
    /// through the path already there.
    fn quit(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "android")]
        {
            let _ = ctx;
            self.save_now();
            self.manager.shutdown();
            // Explicitly, and before the process ends: `exit` runs no
            // destructors at all, so a `Drop` that released the wake request
            // would never be reached and the lock would outlive the program.
            if let Err(err) = power::release() {
                tracing::warn!(target: "xgview::power", %err, "cannot take the wake request back");
            }
            std::process::exit(0);
        }
        #[cfg(not(target_os = "android"))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ---------------------------------------------------------------- events

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
    }

    // ---------------------------------------------------------------- update

    /// Android: the floating input box.
    ///
    /// The soft keyboard covers the lower part of a full screen window, and a
    /// window that is full screen is not moved up out of its way. So while a
    /// field is being edited a copy of it is drawn at the top of the screen -
    /// the part that is always in sight - with the caret where the field's own
    /// is. The focus stays on the field and the keystrokes still reach it
    /// through `keyboard::drain`; this only shows what they did, and carries
    /// the buttons that act on the field: select all, copy, paste, clear, done.
    ///
    /// The buttons are for a touch screen or a pointer. A remote has neither,
    /// and while the keyboard is up its arrows belong to the keyboard.
    #[cfg(target_os = "android")]
    fn input_overlay(&mut self, ctx: &egui::Context) {
        enum Act {
            SelectAll,
            Copy,
            Paste,
            Clear,
            /// Take the text and put the keyboard away - the only way out of a
            /// multi-line field, whose input method action key is a newline.
            Done,
        }

        let Some(mirror) = ctx.data(|data| data.get_temp::<dialogs::Mirror>(Id::new(dialogs::MIRROR))) else {
            return;
        };
        if self.editing != Some(mirror.id) {
            return;
        }

        let shown: String = if mirror.password {
            "\u{2022}".repeat(mirror.text.chars().count())
        } else {
            mirror.text.clone()
        };
        let range = egui::TextEdit::load_state(ctx, mirror.id).and_then(|state| state.cursor.char_range());
        let selecting = range.is_some_and(|range| range.primary.index != range.secondary.index);
        let text = if selecting {
            // Only select-all makes a selection here, so the whole text is it.
            RichText::new(shown).size(20.0).color(theme::TEXT).background_color(theme::FOCUS_FILL)
        } else {
            let mut chars: Vec<char> = shown.chars().collect();
            let caret = range.map(|range| range.primary.index).unwrap_or(chars.len()).min(chars.len());
            chars.insert(caret, '|');
            RichText::new(chars.into_iter().collect::<String>()).size(20.0).color(theme::TEXT)
        };

        let screen = ctx.screen_rect();
        let mut act = None;
        let area = egui::Area::new(Id::new("xg-input-overlay"))
            .order(egui::Order::Tooltip)
            .fixed_pos(screen.center_top() + vec2(0.0, 12.0))
            .pivot(egui::Align2::CENTER_TOP)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .stroke(egui::Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_max_width(screen.width() * 0.8);
                        ui.label(RichText::new(&mirror.label).small().color(theme::TEXT_DIM));
                        ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap));
                        ui.horizontal(|ui| {
                            if ui.button(monitor_i18n::tr("input-select-all")).clicked() {
                                act = Some(Act::SelectAll);
                            }
                            // A password is not put on the clipboard.
                            if ui.add_enabled(!mirror.password, egui::Button::new(monitor_i18n::tr("input-copy"))).clicked() {
                                act = Some(Act::Copy);
                            }
                            if ui.button(monitor_i18n::tr("input-paste")).clicked() {
                                act = Some(Act::Paste);
                            }
                            if ui.button(monitor_i18n::tr("input-clear")).clicked() {
                                act = Some(Act::Clear);
                            }
                            if ui.button(monitor_i18n::tr("input-done")).clicked() {
                                act = Some(Act::Done);
                            }
                        });
                    });
            });

        // A press on the box is a click elsewhere to the field, which lets go
        // of the focus - and with it the keyboard, a frame later. The box is
        // not a place the viewer left the field for, so the focus is handed
        // straight back.
        let rect = area.response.rect;
        let touched = ctx.input(|input| {
            (input.pointer.any_pressed() || input.pointer.any_released())
                && input.pointer.interact_pos().is_some_and(|pos| rect.contains(pos))
        });
        if touched && ctx.memory(|memory| memory.focused()) != Some(mirror.id) {
            ctx.memory_mut(|memory| memory.request_focus(mirror.id));
        }

        if let Some(act) = act {
            match act {
                Act::SelectAll => crate::keyboard::select_all(),
                Act::Copy => crate::android::set_clipboard_text(&mirror.text),
                Act::Paste => crate::keyboard::paste(crate::android::clipboard_text()),
                Act::Clear => crate::keyboard::clear(),
                Act::Done => {
                    // Confirm the edit the way Enter would on a single-line
                    // field: drop the focus, which drops the keyboard and the
                    // floating box together, and hand it to the next control -
                    // the same move the form makes for a value control that
                    // confirms with Enter - rather than only closing the box.
                    self.editing = None;
                    ctx.memory_mut(|memory| memory.surrender_focus(mirror.id));
                    if let Some(next) = self.nav.next_in_scope(mirror.id) {
                        ctx.memory_mut(|memory| memory.request_focus(next));
                        self.reveal_next = Some(next);
                    }
                }
            }
            ctx.request_repaint();
        }
    }

    /// Whether a control of this kind takes typed text while it has the focus.
    ///
    /// A drag value is one of them: egui hands its keyboard-edit mode a
    /// `TextEdit`, and its caret keys belong to it while it is focused. A
    /// slider is not - it has no text to type into.
    fn types(kind: Kind) -> bool {
        matches!(kind, Kind::Text | Kind::DragValue)
    }

    /// Whether a control of this kind raises the Android soft keyboard.
    ///
    /// Only a text field does. A drag value can be typed into on the desktop,
    /// where a keyboard is already at hand, but on Android it is stepped by
    /// dragging or with the remote's Up / Down; raising the keyboard for it
    /// would only cover the box, and unlike a text field it leaves no copy for
    /// the floating input box to mirror.
    #[cfg(target_os = "android")]
    fn soft_keyboard(kind: Kind) -> bool {
        matches!(kind, Kind::Text)
    }

    /// Whether a text field is being typed into, which is what the soft
    /// keyboard follows.
    ///
    /// The navigation layer answers it: every text field in the application
    /// registers itself as [`Kind::Text`], and nothing else does. The obvious
    /// test - whether a [`egui::text_edit::TextEditState`] sits under the
    /// focused id - is wrong, and wrong for the rest of the process: egui keeps
    /// that state *persisted*, so an id that belonged to a field once keeps
    /// answering yes for whatever else is drawn with it later. That is how the
    /// keyboard came up on a television when the remote reached the transport
    /// button of the first camera row.
    fn typing(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| memory.focused())
            .is_some_and(|focused| Self::types(self.nav.kind(focused)))
    }

    /// The room to leave before a row of buttons to put it in the middle.
    ///
    /// egui hands a nested row the full width and lays its widgets out from the
    /// left, so a row that wants to be centred measures itself and pads its

    /// left side. The button labels are the only thing that decides the width:
    /// button padding is the theme's, and a label wider than the minimum size
    /// is the usual case here.
    fn center_offset(ui: &egui::Ui, labels: &[&str]) -> f32 {
        let padding = ui.spacing().button_padding.x * 2.0;
        let font = egui::TextStyle::Button.resolve(ui.style());
        let mut total = 0.0;
        for label in labels {
            total += ui.fonts(|fonts| {
                fonts
                    .layout_no_wrap((*label).to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                    .size()
                    .x
            }) + padding;
        }
        total += ui.spacing().item_spacing.x * labels.len().saturating_sub(1) as f32;
        ((ui.available_width() - total) * 0.5).max(0.0)
    }

    /// Takes Back out of the queue, if it was pressed.
    ///
    /// Escape on a keyboard, `BrowserBack` from an Android remote. Backspace is
    /// left alone: it belongs to a field's caret, and the alternative owner of
    /// Back only claims it when no field is being typed into.
    fn back_pressed(ctx: &egui::Context) -> bool {
        ctx.input_mut(|input| {
            let none = Modifiers::NONE;
            input.consume_key(none, Key::Escape) || input.consume_key(none, Key::BrowserBack)
        })
    }

    /// Back as a window reads it: Escape and BrowserBack always, and Backspace
    /// too while no text field is being typed into.
    ///
    /// The wall reads Back as those three keys as well, so a remote or keyboard
    /// whose Back arrives as Backspace has to be able to leave a window the same
    /// way it leaves the panel - otherwise the press falls through to the panel
    /// behind and closes that instead. A field with the focus keeps Backspace
    /// for its own text.
    fn dialog_back_pressed(&self, ctx: &egui::Context) -> bool {
        if Self::back_pressed(ctx) {
            return true;
        }
        !self.typing(ctx)
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Backspace))
    }

    /// Opens a link the way the desktop does.
    ///
    /// egui would hand the URL to eframe, which opens it through the
    /// `webbrowser` crate - and on Windows that crate always runs the *default
    /// browser*, passing the URL to it as an argument. A `mailto:` therefore
    /// reaches a browser tab instead of the mail client, because it never meets
    /// the shell that resolves a protocol to its handler. The shell is asked
    /// here instead: `ShellExecuteW` on Windows, and the desktop's opener
    /// elsewhere.
    ///
    /// Whatever answers opens a window of its own - a browser, the mail client -
    /// and a wall held on top would be sitting in front of it, so the wall is
    /// taken off the top while it happens and put back on a clock. The clock is
    /// what says when: nothing reports that another program's window has
    /// appeared.
    fn open_link(&mut self, ctx: &egui::Context, url: &str) {
        self.set_on_top(ctx, OnTop::SuspendedUntil(Instant::now() + LINK_TAKEOVER));
        #[cfg(windows)]
        {
            #[link(name = "shell32")]
            extern "system" {
                fn ShellExecuteW(
                    hwnd: *mut core::ffi::c_void,
                    operation: *const u16,
                    file: *const u16,
                    parameters: *const u16,
                    directory: *const u16,
                    show: i32,
                ) -> *mut core::ffi::c_void;
            }
            fn wide(text: &str) -> Vec<u16> {
                use std::os::windows::ffi::OsStrExt;
                std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
            }
            const SW_SHOWNORMAL: i32 = 1;
            let operation = wide("open");
            let file = wide(url);
            // Safety: two NUL terminated wide strings that outlive the call,
            // and null for the rest; the return value is only compared.
            let opened = unsafe {
                ShellExecuteW(
                    std::ptr::null_mut(),
                    operation.as_ptr(),
                    file.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    SW_SHOWNORMAL,
                )
            };
            // `ShellExecuteW` returns a value greater than 32 on success.
            if opened as isize > 32 {
                return;
            }
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(url).spawn();
            return;
        }
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
        {
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            return;
        }
        // Android has no shell to ask. egui's own opener is compiled out on this
        // target - eframe is built without its `webbrowser` feature - so the
        // activity is asked to open it with an Intent instead, which is what lets
        // a `mailto:` reach the mail app.
        #[cfg(target_os = "android")]
        crate::android::open_uri(url);
    }

    /// Whether this frame carries input that is not a Back press.
    ///
    /// Used to cancel the exit question: a viewer who presses something else has
    /// answered it by doing something else. Every key the wall answers Back to
    /// is a Back here and not an interruption - Escape, the remote's
    /// `BrowserBack`, and Backspace, which is the Back a keyboard offers and
    /// what the wall already reads as one. Movement of a pointer is not input
    /// in that sense either: a mouse nudged on a desk, or a finger resting on a
    /// touch screen, must not take the question down.
    fn input_other_than_back(ctx: &egui::Context) -> bool {
        ctx.input(|input| {
            input.events.iter().any(|event| match event {
                egui::Event::Key { key, pressed: true, .. } => {
                    !matches!(key, Key::Escape | Key::BrowserBack | Key::Backspace)
                }
                egui::Event::PointerButton { pressed: true, .. } => true,
                egui::Event::Text(_) | egui::Event::MouseWheel { .. } => true,
                _ => false,
            })
        })
    }

    /// Routes one frame of keys to whoever owns them.
    ///
    /// The four directions, Enter and Escape have two possible owners, and the
    /// choice is made by where the remote's focus was when the frame began -
    /// which is the only thing the viewer can see. A widget of the bars, the
    /// settings panel or a window owning them means egui's own focus walk does
    /// the work; it has already run by the time the application is called, and
    /// it runs on the very events read here, which is why they are *not* taken
    /// out of the queue in that case. The wall is not made of widgets, so it
    /// answers the same keys itself.
    /// Keyboard handler for the combined-info picker modal.  Called from
    /// `handle_keys` so the arrows are consumed before the nav layer or the
    /// wall can act on them.
    fn handle_combined_info_keys(&mut self, ctx: &egui::Context) {
        let none = Modifiers::NONE;
        let back = ctx.input_mut(|input| {
            input.consume_key(none, Key::Escape)
                || input.consume_key(none, Key::BrowserBack)
                || input.consume_key(none, Key::Backspace)
        });
        if back {
            self.combined_info_picker = false;
            self.restore_focus = self.combined_info_picker_from.take();
            return;
        }

        let slot = self.combined_info_picker_slot;
        let mut selected = self.config.combined_info[slot].clone();
        let focus_id = Id::new("xgview-combined-info-focus");
        let (mut focus_col, mut focus_idx): (usize, usize) =
            ctx.data(|d| d.get_temp(focus_id).unwrap_or((0, 0)));

        let available: Vec<OsdItem> = OsdItem::ATOMIC
            .iter()
            .filter(|atom| !selected.contains(atom))
            .copied()
            .collect();

        if focus_col == 0 && available.is_empty() && !selected.is_empty() {
            focus_col = 1;
            focus_idx = 0;
        }
        if focus_col == 1 && selected.is_empty() && !available.is_empty() {
            focus_col = 0;
            focus_idx = 0;
        }
        let col_len = if focus_col == 0 { available.len() } else { selected.len() };
        if focus_idx >= col_len {
            focus_idx = col_len.saturating_sub(1);
        }

        let (up, down, left, right, activate) = ctx.input_mut(|i| (
            i.consume_key(none, Key::ArrowUp),
            i.consume_key(none, Key::ArrowDown),
            i.consume_key(none, Key::ArrowLeft),
            i.consume_key(none, Key::ArrowRight),
            i.consume_key(none, Key::Enter) || i.consume_key(none, Key::Space),
        ));

        let mut changed = false;
        if up && focus_idx > 0 {
            focus_idx -= 1;
        }
        if down {
            let len = if focus_col == 0 { available.len() } else { selected.len() };
            if focus_idx + 1 < len {
                focus_idx += 1;
            }
        }
        if left && focus_col == 1 && !available.is_empty() {
            focus_col = 0;
            focus_idx = focus_idx.min(available.len() - 1);
        }
        if right && focus_col == 0 && !selected.is_empty() {
            focus_col = 1;
            focus_idx = focus_idx.min(selected.len() - 1);
        }
        if activate {
            if focus_col == 0 && focus_idx < available.len() {
                let atom = available[focus_idx];
                selected.push(atom);
                changed = true;
                if available.len() > 1 {
                    if focus_idx >= available.len() - 1 {
                        focus_idx = available.len() - 2;
                    }
                } else {
                    focus_col = 1;
                    focus_idx = selected.len() - 1;
                }
            } else if focus_col == 1 && focus_idx < selected.len() {
                selected.remove(focus_idx);
                changed = true;
                if selected.is_empty() {
                    focus_col = 0;
                    focus_idx = focus_idx.min(available.len().saturating_sub(1));
                } else if focus_idx >= selected.len() {
                    focus_idx = selected.len() - 1;
                }
            }
        }

        ctx.data_mut(|d| d.insert_temp(focus_id, (focus_col, focus_idx)));
        if changed {
            self.config.combined_info[slot] = selected;
            self.mark_dirty();
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // The file dialog owns the keyboard while it is up. Its own keybindings
        // and egui's focus walk are how a viewer moves around it, and the arrows
        // and Back taken out of the queue below would reach neither; it is left
        // to close itself, by its Cancel button or the Esc it reads.
        //
        // Read as the frame began rather than now: a Back that has just closed
        // the dialog is the dialog's to answer, and it is the same press that a
        // `file_dialog_open` read here would find still in the queue and hand to
        // the wall - closing the panel the dialog was opened from.
        if self.file_dialog_keys {
            return;
        }

        // The combined-info picker handles its own keyboard: arrows move the
        // selection within and between columns, Enter moves an item, Back
        // closes.  All consumed here so the rest of `handle_keys` — the wall,
        // the nav walk — does not also act on them.
        if self.combined_info_picker {
            self.handle_combined_info_keys(ctx);
            return;
        }

        // Android: a field's edit confirmation is dropped the moment the focus
        // leaves it - whether by an arrow, a click, or Back. Without this, a
        // field confirmed once would raise the keyboard again on re-entering
        // it, and a walk from one field to the next would carry the keyboard
        // along. `set_wanted` already gates on `editing == focused`, so a stale
        // value would not raise the keyboard on the wrong field, but clearing
        // it here keeps the state honest.
        #[cfg(target_os = "android")]
        {
            let focused = ctx.memory(|memory| memory.focused());
            if self.editing.is_some_and(|e| focused != Some(e)) {
                self.editing = None;
            }
        }

        // The question the first Back asks on the wall is answered by a second
        // Back and by nothing else. Any *input* between the two presses - a
        // direction, a click, the key that opens a panel - was the viewer doing
        // something else, and calls it off.
        //
        // Input, and not the frame: the wall repaints several times a second on
        // its own, and a rule that cancelled on a frame would take the question
        // down before it could be read.
        if self.exit_armed.is_some() && Self::input_other_than_back(ctx) {
            self.exit_armed = None;
        }

        // A window this application opened is the innermost layer: Back closes
        // it, whatever inside it has the focus. A field's caret must not hold it
        // open - Back on the first field would otherwise hand the focus back to
        // that same field, and the window could never be left.
        // A prompt whose passphrase is being stretched is the exception: it is
        // the one thing on this side of the answer that cannot be backed out of,
        // because the answer would then have nowhere to go.
        if ((self.passphrase.as_ref().is_some_and(|prompt| !prompt.working))
            || self.camera_edit.is_some()
            || self.remove_confirm.is_some()
            || self.system_confirm.is_some())
            && self.dialog_back_pressed(ctx)
        {
            if self.passphrase.take().is_some() {
                self.passphrase_anchor = None;
                // Backing out of it leaves the export or import unasked for, and
                // the button behind the prompt takes the focus again.
                self.restore_focus = self.file_dialog_anchor.take().or(self.restore_focus);
            } else if let Some(confirm) = self.remove_confirm.take() {
                self.remove_confirm_anchor = None;
                self.restore_focus = confirm.from;
            } else if let Some(confirm) = self.system_confirm.take() {
                self.system_confirm_anchor = None;
                self.restore_focus = confirm.from;
            } else if let Some(edit) = self.camera_edit.take() {
                self.camera_edit_anchor = None;
                self.restore_focus = edit.from;
            }
            return;
        }

        // A text field being typed into owns the letters, the paste and the
        // caret keys. Two kinds of key are taken away from it here.
        //
        // Back, because it is the remote control's only way out of a field: it
        // leaves the field and not the panel - one step - and the press after it
        // closes the panel.
        //
        // And the four directions, because a form with a field in the middle has
        // to be walkable past it. `TextEdit` reads its input through
        // `filtered_events`, so taking the arrows out of the queue before the
        // field is drawn keeps it from spending them on its caret. The focus
        // still moves: egui read the same events for its focus walk in
        // `begin_pass`, which happens before the keys are handed out.
        //
        // This is deliberately *not* `Context::wants_keyboard_input`, which is
        // true for any focused widget at all. The bars hand the focus to a
        // button whenever a panel opens, so asking that question would leave a
        // viewer unable to press anything - F1, F2, BACK, anything - from the
        // moment a panel appeared.
        //
        // A control on the navigation layer is the exception: its arrows are
        // answered by [`crate::nav`], a field among them included, because a
        // field in the middle of a form still has to be walkable past. Only the
        // keys that are not the arrows - the letters, Back - fall through to
        // the field.
        let nav_owns =
            ctx.memory(|memory| memory.focused()).is_some_and(|focused| self.nav.owns(focused));
        // What the focused control keeps for itself: a slider is moved with
        // Left and Right, a drag value with Up and Down, and only the other
        // axis moves the focus. A plain control keeps neither.
        let nav_kind = if nav_owns {
            ctx.memory(|memory| memory.focused()).map(|focused| self.nav.kind(focused))
        } else {
            None
        };
        // The layer already knows what kind of control it focused, so a field
        // is one that registered as a kind that takes typed text - the same
        // answer `typing` gives at the end of the frame.
        let typing = nav_kind.is_some_and(Self::types);
        if typing {
            // Android: the soft keyboard is not raised by focus alone. Enter on
            // a text field that is not yet being edited confirms it - `editing`
            // takes the focus and the keyboard comes up next frame; the Enter
            // itself is consumed so `TextEdit` does not read it as a submit. A
            // drag value is left out: it is stepped, not typed into, on Android.
            // Back on a field that is being edited drops the keyboard but keeps
            // the focus, so the viewer can read the field and re-confirm it.
            // Back on a field that is not being edited falls through to `leave`
            // below and walks the focus out, as on the desktop. A touch click is
            // handled at the end of the frame, where it can be told apart from a
            // DPAD walk.
            #[cfg(target_os = "android")]
            {
                let focused = ctx.memory(|memory| memory.focused());
                let none = Modifiers::NONE;
                if self.editing == focused {
                    if ctx.input_mut(|input| {
                        input.consume_key(none, Key::Escape)
                            || input.consume_key(none, Key::BrowserBack)
                    }) {
                        self.editing = None;
                        return;
                    }
                } else if nav_kind.is_some_and(Self::soft_keyboard)
                    && ctx.input_mut(|input| input.consume_key(none, Key::Enter))
                {
                    self.editing = focused;
                    return;
                }
            }
            // Back leaves the field; the arrows walk the form. Where a field
            // keeps its caret keys for itself - the bars and the wall - the
            // arrows are taken out of the queue here, as before; on the
            // navigation layer they are left for the layer to move the focus.
            let leave = ctx.input_mut(|input| {
                let none = Modifiers::NONE;
                let leave = input.consume_key(none, Key::Escape)
                    || input.consume_key(none, Key::BrowserBack);
                if !nav_owns {
                    for key in [Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown] {
                        input.consume_key(none, key);
                    }
                }
                leave
            });
            if leave {
                if let Some(focused) = ctx.memory(|memory| memory.focused()) {
                    ctx.memory_mut(|memory| memory.surrender_focus(focused));
                }
                return;
            }
            if !nav_owns {
                return;
            }
        }
        let grid_owns = self.owner == Owner::Grid;
        let keys = ctx.input_mut(|input| {
            let none = Modifiers::NONE;
            let mut keys = Keys::default();
            // Read before the press is consumed: `consume_key` matches key
            // repeats too, and a repeated Enter must not press the magnified
            // viewport's bar over and over. See `Keys::enter_fresh`.
            keys.enter_fresh = input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key { key: Key::Enter | Key::Space, pressed: true, repeat: false, .. }
                )
            });
            // Back is Escape on a keyboard and `BrowserBack` from an Android
            // remote: winit maps `AKEYCODE_BACK` onto that, not onto Escape, so
            // a remote that only ever sends BACK would otherwise have no way out
            // of anything.
            if grid_owns {
                keys.left = input.consume_key(none, Key::ArrowLeft);
                keys.right = input.consume_key(none, Key::ArrowRight);
                keys.up = input.consume_key(none, Key::ArrowUp);
                keys.down = input.consume_key(none, Key::ArrowDown);
                keys.enter = input.consume_key(none, Key::Enter) || input.consume_key(none, Key::Space);
                keys.back = input.consume_key(none, Key::Escape)
                    || input.consume_key(none, Key::Backspace)
                    || input.consume_key(none, Key::BrowserBack);
            } else {
                // A control of the navigation layer has its arrows taken out of
                // the queue here: the layer decides where they go, and the
                // widget must not also read them. Everything else is left in
                // the queue for egui's own walk, which ran already.
                if nav_owns {
                    // On a remote Up and Down always move the focus. The two
                    // value controls are adjusted sideways instead: the slider
                    // reads the sideways keys itself, and a drag value has them
                    // recorded on the layer for the widget to apply as it is
                    // drawn (see `controls::drag_value`), because it steps with
                    // Up / Down and those are the focus's.
                    match nav_kind {
                        Some(Kind::Slider) => {
                            keys.left = input.key_pressed(Key::ArrowLeft);
                            keys.right = input.key_pressed(Key::ArrowRight);
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                        }
                        Some(Kind::DragValue) | Some(Kind::Cycle) => {
                            // Taken so the field's caret does not move on them;
                            // the value change happens at the widget.
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                            keys.left = input.consume_key(none, Key::ArrowLeft);
                            keys.right = input.consume_key(none, Key::ArrowRight);
                        }
                        Some(Kind::Text) => {
                            // The caret is the field's, so the sideways keys are
                            // left in the queue for it. The vertical ones still
                            // walk the form.
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                        }
                        _ => {
                            // A plain control: all four arrows move the focus.
                            keys.left = input.consume_key(none, Key::ArrowLeft);
                            keys.right = input.consume_key(none, Key::ArrowRight);
                            keys.up = input.consume_key(none, Key::ArrowUp);
                            keys.down = input.consume_key(none, Key::ArrowDown);
                        }
                    }
                } else {
                    keys.left = input.key_pressed(Key::ArrowLeft);
                    keys.right = input.key_pressed(Key::ArrowRight);
                    keys.up = input.key_pressed(Key::ArrowUp);
                    keys.down = input.key_pressed(Key::ArrowDown);
                }
                keys.enter = input.key_pressed(Key::Enter) || input.key_pressed(Key::Space);
                // Backspace is the field's own when a field has the focus: it
                // deletes a character rather than unwinding a window.
                keys.back = input.key_pressed(Key::Escape)
                    || (!typing && input.key_pressed(Key::Backspace))
                    || input.key_pressed(Key::BrowserBack);
            }
            // The rest belong to the application whatever has the focus, and
            // are consumed so that nothing downstream acts on them as well.
            keys.settings = input.consume_key(none, Key::F1);
            keys.devices = input.consume_key(none, Key::F2);
            keys.fullscreen = input.consume_key(none, Key::F11);
            // The layout digits and the page keys are the exception: a field
            // being typed into keeps them. An address is made of the digits -
            // `192.168.1.4` - so consuming them changes the wall's shape under a
            // viewer who was only naming a camera. The page keys are not text,
            // but a page turning behind a dialog is the same surprise.
            if !typing {
                keys.page_prev = input.consume_key(none, Key::PageUp);
                keys.page_next = input.consume_key(none, Key::PageDown);
                keys.layout = [
                    input.consume_key(none, Key::Num1),
                    input.consume_key(none, Key::Num2),
                    input.consume_key(none, Key::Num3),
                    input.consume_key(none, Key::Num4),
                    input.consume_key(none, Key::Num5),
                    input.consume_key(none, Key::Num6),
                    input.consume_key(none, Key::Num7),
                    input.consume_key(none, Key::Num8),
                    input.consume_key(none, Key::Num9),
                    input.consume_key(none, Key::Num0),
                ];
            }
            keys
        });

        // The focus ring is armed here, before any of these keys is acted on:
        // what arms it is the keys that walk it, whatever else they end up
        // doing. See `ring_keys`.
        if Self::ring_keys(&keys, self.owner == Owner::Grid) {
            self.ring_seen = Some(Instant::now());
            self.ring_aimed = true;
        }

        // The magnified viewport's bar goes when the viewer stops using it, not
        // eight seconds after it opened: every key it answers keeps its clock
        // running. This is done here, before any of the branches below can
        // return - the bar's own keys are the navigation layer's, and that path
        // leaves this function early.
        if self.tile_bar.is_open()
            && (keys.left || keys.right || keys.up || keys.down || keys.enter || keys.back)
        {
            self.tile_bar.touch(Instant::now());
        }

        // The reorder mode owns Back: it drops the arrangement and nothing
        // else, so a press that means "never mind" cannot also close the panel
        // behind it.
        if keys.back && self.reorder.is_some() {
            self.cancel_reorder();
            return;
        }


        // Application keys first: they are how a keyboard, and whatever button
        // a remote offers beside the DPAD, reach the panels at all.
        // A dialog owns the screen while it is up: nothing behind it answers the
        // pointer, and the keys that open a panel are spent the same way, so one
        // dialog can never be stacked on another.
        let dialog_open = self.discovery.open
            || self.camera_edit.is_some()
            || self.remove_confirm.is_some()
            || self.system_confirm.is_some()
            || self.passphrase.is_some();
        // The layout picker is a modal of its own, and it owns the keys while
        // it is up: opening a panel behind it would stack two of them.
        if keys.settings && !dialog_open && !self.layout_picker && !self.portrait_aspect_picker && !self.combined_info_picker && !self.is_portrait() {
            self.show_settings = !self.show_settings;
        }
        if keys.devices && !dialog_open && !self.layout_picker && !self.portrait_aspect_picker && !self.combined_info_picker {
            self.discovery.open = !self.discovery.open;
            // Opened from the keyboard, so there is no button to hand the focus
            // back to when it closes.
            self.discovery.from = None;
        }
        if keys.fullscreen {
            let enabled = !self.fullscreen;
            self.set_fullscreen(ctx, enabled);
        }
        // A key that opens a panel, or changes the shape of the wall, is not the
        // magnified viewport's bar's: it takes the bar down before it acts, so
        // the two are never up at once. See [`crate::tilebar`].
        if keys.settings
            || keys.devices
            || keys.fullscreen
            || keys.page_prev
            || keys.page_next
            || keys.layout.iter().any(|pressed| *pressed)
        {
            self.tile_bar.hide();
        }
        // The picker is a modal: while it is up the wall's own keys - page
        // turns and the layout digits - are spent, so a press cannot change the
        // wall behind it. Back is answered below, where the panels are unwound.
        if !self.layout_picker && !self.portrait_aspect_picker && !self.combined_info_picker {
            // A page key turns the page from wherever the focus is, but the
            // bounce a turn at the edge makes - to the other page button -
            // belongs to a focus that was standing on one of those buttons: the
            // turn is what took the button it stood on away. A tile's focus, or
            // a focus anywhere else, turns the page and stays put.
            let on_page_button = self.page_button_has_focus(ctx);
            if keys.page_prev {
                self.advance(false, on_page_button);
            }
            if keys.page_next {
                self.advance(true, on_page_button);
            }
            for (slot, pressed) in keys.layout.iter().enumerate() {
                if !*pressed {
                    continue;
                }
                if self.is_portrait() {
                    // Upright, the digits pick the four stacked grids.
                    if let Some(layout) = GridLayout::PORTRAIT.get(slot) {
                        self.set_layout(*layout);
                    }
                } else {
                    // The digit is the viewport count; the last slot is `0`,
                    // which stands for the ten-cell layout. 12, 13 and 16 have
                    // no digit.
                    let digit = if slot == 9 { 0 } else { slot as u8 + 1 };
                    if let Some(layout) = GridLayout::for_digit(digit) {
                        self.set_layout(layout);
                    }
                }
            }
        }

        // A control of the navigation layer: the arrow moves by declaration -
        // the scope's order, then its exits - and not by geometry. Answered
        // before the Back handling below, so that Back still unwinds a window.
        if nav_owns {
            // A value control - a drag value, or the language button - takes
            // its step from the sideways presses recorded here; the widget
            // applies it as it is drawn this frame. Left steps down and Right
            // up, the way a slider reads them.
            if matches!(nav_kind, Some(Kind::DragValue) | Some(Kind::Cycle)) {
                if keys.left {
                    self.nav.step_value(-1);
                }
                if keys.right {
                    self.nav.step_value(1);
                }
            }
            // Up and Down always move the focus. A value control - slider, drag
            // value, text field or cycling button - answers its sideways keys
            // itself, so they never reach here; a plain control walks with all
            // four.
            let dir = match nav_kind {
                Some(Kind::Slider) | Some(Kind::DragValue) | Some(Kind::Text) | Some(Kind::Cycle) => {
                    if keys.up {
                        Some(Dir::Up)
                    } else if keys.down {
                        Some(Dir::Down)
                    } else {
                        None
                    }
                }
                _ => {
                    if keys.up {
                        Some(Dir::Up)
                    } else if keys.down {
                        Some(Dir::Down)
                    } else if keys.left {
                        Some(Dir::Left)
                    } else if keys.right {
                        Some(Dir::Right)
                    } else {
                        None
                    }
                }
            };
            if let (Some(dir), Some(focused)) = (dir, ctx.memory(|memory| memory.focused())) {
                match self.nav.step(focused, dir) {
                    Some(target) => {
                        ctx.memory_mut(|memory| memory.request_focus(target));
                    }
                    None => {
                        if dir == Dir::Down && self.nav.scope_of(focused) == Some("settings-body") {
                            // The press had nowhere to move the focus, which is
                            // the case at the end of a tab: what follows the last
                            // control - the adb line, the table of keys - is text
                            // and has no control to be walked onto. A remote has
                            // no wheel, so the press moves the body instead.
                            // Asked for here rather than applied here: only a
                            // scroll area that is being drawn reads a scroll
                            // request, and this runs before anything is drawn.
                            let points = ctx.screen_rect().height() * REMOTE_SCROLL_FRACTION;
                            self.body_scroll = Some(points);
                        }
                    }
                }
            }
        }

        if !grid_owns {
            // The magnified viewport's bar opens on its seat, which is not one
            // of its controls: the first arrow is what steps off it, onto the
            // first control - the handset while a call is up - and no arrow ever
            // walks back onto it. See `tilebar::seat`.
            if self.tile_bar.is_open()
                && ctx.memory(|memory| memory.focused()) == Some(tilebar::seat())
                && (keys.left || keys.right || keys.up || keys.down)
            {
                ctx.memory_mut(|memory| memory.request_focus(tilebar::control(0)));
            }
            // egui has walked the focus already - and dropped it, if this was
            // Escape. Back is therefore one step out of the controls and not
            // two: a panel opened over the wall closes, and otherwise the wall
            // takes the keys back.
            if keys.back {
                #[cfg(juanz)]
                if crate::juanz::close(ctx) {
                    return;
                }
                // Back unwinds one layer at a time, and the magnified viewport's
                // bar is a layer: it goes first, before the panels behind it and
                // long before the magnification. The focus goes back to the wall
                // with it, or the next press would be read by nothing at all.
                //
                // A call is a layer of its own, above the bar: Back ends it, the
                // same as the handset, and leaves the picture where it is.
                if self.talking {
                    self.set_talking(false);
                } else if self.tile_bar.is_open() {
                    self.tile_bar.hide();
                    self.leave_controls(ctx);
                } else if self.layout_picker {
                    self.layout_picker = false;
                    self.restore_focus = self.layout_picker_from.take();
                } else if self.portrait_aspect_picker {
                    self.portrait_aspect_picker = false;
                    self.restore_focus = self.portrait_aspect_picker_from.take();
                } else if self.discovery.open {
                    self.discovery.open = false;
                    self.restore_focus = self.discovery.from.take();
                } else if self.show_settings {
                    self.show_settings = false;
                } else {
                    self.leave_controls(ctx);
                }
            } else if keys.down && !self.show_settings && self.toolbar_has_focus(ctx) {
                // Below the bar there is only the wall, so the arrow that walks
                // off the bottom of it hands the keys back.
                self.leave_controls(ctx);
            } else if keys.up && self.status_has_focus(ctx) {
                // The status row is below the wall, so the arrow back to the
                // wall from it is Up, the mirror of the one that came down.
                self.leave_controls(ctx);
            }
            if keys.enter {
                let focused = ctx.memory(|memory| memory.focused());
                if focused == Some(Id::new("portrait-prev-page")) {
                    self.advance(false, true);
                } else if focused == Some(Id::new("portrait-next-page")) {
                    self.advance(true, true);
                }
            }
            return;
        }

        if self.scheduler.is_zoomed() {
            // The strip owns the keys only where the remote is in it: its
            // controls are egui's own buttons on the navigation layer, and the
            // arrows walk them. A strip that is merely up - the upright default,
            // or one a tap put there - leaves the remote on the wall, so the
            // arrows keep stepping channels and the confirm key is what walks
            // into the strip.
            let bar_owns_keys = self.tile_bar.is_open()
                && ctx.memory(|memory| memory.focused()).is_some_and(tilebar::owns);
            if !bar_owns_keys {
                // A magnified viewport is one channel, not a grid: Right and
                // Left are the next and previous channel in channel order, and
                // Up and Down have no grid to walk, so they go straight to the
                // bars.
                if keys.left {
                    self.step_zoom(false);
                }
                if keys.right {
                    self.step_zoom(true);
                }
                if keys.down {
                    self.hand_off(ctx, Bar::Status);
                }
                if keys.up {
                    self.hand_off(ctx, Bar::Toolbar);
                }
                // The confirm key brings the viewport's bar up rather than
                // unwinding the magnification: Back is the way out now, a layer
                // at a time. A held key is not a second press. The bar appears
                // without its selection drawn - the focus is put on its seat, not
                // on a control - and the first arrow is what lights it. A bar
                // that is already up is entered the same way, without being
                // shown again.
                //
                // It comes up along the bottom: the remote asks for it without
                // pointing at an edge, and the status side is where the hand and
                // the eye expect a strip of controls.
                if keys.enter && keys.enter_fresh {
                    if !self.tile_bar.is_open() {
                        self.tile_bar.show(tilebar::Dock::Bottom, Instant::now());
                    }
                    ctx.memory_mut(|memory| memory.request_focus(tilebar::seat()));
                }
            }
        } else {
            if keys.left {
                self.navigate(Direction::Left);
            }
            if keys.right {
                self.navigate(Direction::Right);
            }
            if keys.down && self.navigate(Direction::Down) == NavigateOutcome::Blocked {
                self.hand_off(ctx, Bar::Status);
            }
            if keys.up && self.navigate(Direction::Up) == NavigateOutcome::Blocked {
                self.hand_off(ctx, Bar::Toolbar);
            }
            if keys.enter {
                self.zoom_in();
            }
        }
        if keys.back {
            self.back(ctx);
        }
    }

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

    /// The toolbar as drawn while the wall stands upright: the four portrait
    /// grids, paging, full screen and the orientation button - the few things a
    /// narrow bar can hold. The settings, the add-devices button and the layout
    /// picker are left out; their panels are built for landscape and do not fit.
    fn portrait_toolbar(&mut self, ui: &mut egui::Ui) {
        Self::scale_bar(ui);
        self.nav.open("toolbar");
        let mut items: Vec<(Id, Rect)> = Vec::new();
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
            let current = self.scheduler.layout();
            for (slot, layout) in GridLayout::PORTRAIT.into_iter().enumerate() {
                let selected = current == layout && !self.scheduler.is_zoomed();
                let response = ui.selectable_label(selected, (slot + 1).to_string());
                items.push((response.id, response.rect));
                self.name(&response, layout_hint(layout));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }


            self.page_prev_id = Some(Id::new("portrait-prev-page"));
            self.page_next_id = Some(Id::new("portrait-next-page"));
            for (id_str, has) in [
                ("portrait-prev-page", self.can_advance(false)),
                ("portrait-next-page", self.can_advance(true)),
            ] {
                if !has {
                    continue;
                }
                if let Some(&(_, prev)) = items.last() {
                    let gap = prev.width() * 0.4;
                    let r = Rect::from_min_size(
                        egui::pos2(prev.right() + gap, prev.top()),
                        egui::vec2(prev.width(), prev.height()),
                    );
                    items.push((Id::new(id_str), r));
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.orientation_button(ui, &mut items);
                let fullscreen = icons::button(
                    ui,
                    if self.fullscreen { Icon::Collapse } else { Icon::Expand },
                    self.fullscreen,
                    &monitor_i18n::tr(if self.fullscreen {
                        "toolbar-fullscreen-leave-tip"
                    } else {
                        "toolbar-fullscreen-enter-tip"
                    }),
                );
                items.push((fullscreen.id, fullscreen.rect));
                self.name(
                    &fullscreen,
                    if self.fullscreen { "toolbar-fullscreen-leave" } else { "toolbar-fullscreen-enter" },
                );
                if fullscreen.clicked() {
                    let enabled = !self.fullscreen;
                    self.set_fullscreen(ui.ctx(), enabled);
                }
                if self.scheduler.is_zoomed() {
                    let back = icons::button(
                        ui,
                        Icon::Grid,
                        false,
                        &monitor_i18n::tr("toolbar-back-grid-tip"),
                    );
                    items.push((back.id, back.rect));
                    self.name(&back, "toolbar-back-grid");
                    if back.clicked() {
                        self.zoom_out();
                    }
                } else {
                    let aspect = icons::button(
                        ui,
                        Icon::AspectRatio,
                        self.portrait_aspect_picker,
                        &monitor_i18n::tr("toolbar-portrait-aspect-tip"),
                    );
                    items.push((aspect.id, aspect.rect));
                    self.name(&aspect, "toolbar-portrait-aspect");
                    if aspect.clicked() {
                        self.portrait_aspect_picker = !self.portrait_aspect_picker;
                        self.portrait_aspect_picker_from =
                            (!aspect.clicked_by(egui::PointerButton::Primary))
                                .then_some(aspect.id);
                    }
                }
            });
        });
        items.sort_by(|(_, left), (_, right)| left.center().x.total_cmp(&right.center().x));
        for (id, _) in &items {
            self.nav.item_id(*id);
        }
        self.nav.close();
        self.toolbar_items = items;
    }

    /// The status bar as drawn while the wall stands upright: the group when
    /// there is more than one, how many channels are live, the page, and the
    /// name of the control the remote is on. The layout is left out - the
    /// toolbar's own numbered buttons already say which grid is up.
    fn portrait_status_bar(&mut self, ui: &mut egui::Ui) {
        Self::scale_bar(ui);
        ui.horizontal(|ui| {
            let info = self.scheduler.page_info();
            if self.config.groups.len() > 1 {
                ui.label(RichText::new(dialogs::group_name(
                    &self.config,
                    self.config.active_index(),
                )));
                ui.separator();
            }
            let live = self.channels.values().filter(|channel| channel.state.is_live()).count();
            ui.label(
                RichText::new(monitor_i18n::tr_args(
                    "status-live",
                    &[("live", live.into()), ("total", self.config.enabled_count().into())],
                ))
                .color(if live > 0 { theme::LIVE } else { theme::TEXT_DIM }),
            );
            ui.separator();
            ui.label(monitor_i18n::tr_args(
                "status-page-plain",
                &[("page", (info.page + 1).into()), ("count", info.page_count.into())],
            ));
            if let Some(name) = self.focused_name {
                ui.separator();
                ui.label(RichText::new(monitor_i18n::tr(name)).color(theme::FOCUS));
            }
        });
    }

    fn portrait_page_buttons(&mut self, ui: &mut egui::Ui, area: Rect) {

        let r = 24.0;
        let margin = 6.0;
        let y = area.center().y;
        let fill = egui::Color32::from_black_alpha(51);
        let edge = egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(140));
        let glyph = egui::Stroke::new(2.0_f32, egui::Color32::from_white_alpha(230));
        let d = r * 0.4;
        let sides = [
            (false, "portrait-prev-page", self.can_advance(false)),
            (true, "portrait-next-page", self.can_advance(true)),
        ];
        for (forward, id, has) in sides {
            if !has {
                continue;
            }
            let x = if forward { area.max.x - margin - r } else { area.min.x + margin + r };
            let center = egui::pos2(x, y);
            let rect = Rect::from_center_size(center, egui::Vec2::splat(r * 2.0));
            let button_id = Id::new(id);
            let resp = ui.interact(rect, button_id, egui::Sense::click());
            let focused = ui.memory(|m| m.focused()) == Some(button_id);
            let p = ui.painter();
            p.circle_filled(center, r, fill);
            if focused {
                p.circle_stroke(center, r, egui::Stroke::new(theme::FOCUS_WIDTH, theme::FOCUS));
            } else {
                p.circle_stroke(center, r, edge);
            }
            let sign = if forward { 1.0 } else { -1.0 };
            let tip = egui::pos2(center.x + sign * d * 2.0 / 3.0, center.y);
            let wing_x = center.x - sign * d / 3.0;
            p.add(egui::Shape::line_segment(
                [egui::pos2(wing_x, center.y - d), tip],
                glyph,
            ));
            p.add(egui::Shape::line_segment(
                [egui::pos2(wing_x, center.y + d), tip],
                glyph,
            ));
            if resp.clicked() {
                self.advance(forward, !resp.clicked_by(egui::PointerButton::Primary));
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        if self.is_portrait() {
            self.portrait_toolbar(ui);
            return;
        }
        // The row is laid out for a television. A phone in landscape has less
        // room than that, and wrapping would cost the grid a row, so the bar
        // gives up text size instead: at the reference width and above nothing
        // changes, and below it the labels, the buttons' padding and the gaps
        // shrink together, down to the floor where they stop being readable.
        Self::scale_bar(ui);
        // The bar is one row of the navigation layer: the arrows move by
        // declaration from here on, one control per press. The controls are
        // registered after they are drawn rather than as they are, in the
        // order sorted out below - the right hand group is laid out right to
        // left, so registering it as it is drawn would walk it backwards.
        self.nav.open("toolbar");

        // Every control the remote may land on is recorded with the rectangle
        // it was drawn in: the layer needs the order, but the wall needs the
        // geometry too, to know which control is above the tile it is leaving.
        let mut items: Vec<(Id, Rect)> = Vec::new();
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
            ui.separator();

            // Layout and page are the two things a viewer changes while
            // watching, so they stay on the bar whatever its width.
            let current = self.scheduler.layout();
            for layout in GridLayout::STANDARD {
                let selected = current == layout && !self.scheduler.is_zoomed();
                let response = ui.selectable_label(selected, layout.label());
                items.push((response.id, response.rect));
                self.name(&response, layout_hint(layout));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }

            // The layouts that have no button of their own live behind one
            // button: it shows the number of the one in use, and an ellipsis
            // while the wall is on a layout that has a button of its own.
            let active = !current.is_standard() && !self.scheduler.is_zoomed();
            let more = ui.selectable_label(active || self.layout_picker, current.numeric_label().unwrap_or("…"));
            items.push((more.id, more.rect));
            self.name(&more, "toolbar-layout-more");
            if more.clicked() {
                self.layout_picker = !self.layout_picker;
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                self.layout_picker_from = (self.layout_picker
                    && !more.clicked_by(egui::PointerButton::Primary))
                .then_some(more.id);
            }

            // Only while magnified, and only there: the arrow on a tile, Enter
            // and a double click all magnify, but a touch screen has no keyboard
            // and no arrow, so this is the one way back it can offer.
            if self.scheduler.is_zoomed() {
                let back = icons::button(ui, Icon::Grid, false, &monitor_i18n::tr("toolbar-back-grid-tip"));
                items.push((back.id, back.rect));
                self.name(&back, "toolbar-back-grid");
                if back.clicked() {
                    self.zoom_out();
                }
            }

            ui.separator();
            let info = self.scheduler.page_info();
            let can_prev = self.can_advance(false);
            let can_next = self.can_advance(true);
            let previous = icons::button_sized(
                ui,
                Icon::Left,
                false,
                !can_prev,
                &monitor_i18n::tr("toolbar-prev-page"),
                30.0,
            );
            self.page_prev_id = Some(previous.id);
            if can_prev {
                items.push((previous.id, previous.rect));
                self.name(&previous, "toolbar-prev-page");
            }
            if previous.clicked() {
                self.advance(false, !previous.clicked_by(egui::PointerButton::Primary));
            }
            ui.label(monitor_i18n::tr_args("toolbar-page", &[("page", (info.page + 1).into()), ("count", info.page_count.into())]));
            let next = icons::button_sized(
                ui,
                Icon::Right,
                false,
                !can_next,
                &monitor_i18n::tr("toolbar-next-page"),
                30.0,
            );
            self.page_next_id = Some(next.id);
            if can_next {
                items.push((next.id, next.rect));
                self.name(&next, "toolbar-next-page");
            }
            if next.clicked() {
                self.advance(true, !next.clicked_by(egui::PointerButton::Primary));
            }

            // The right end of the row is the three things a viewer reaches for
            // while watching, and each of them is a shape a remote control, a
            // mouse and a finger all understand without a word of English. The
            // version ends the row, at the very edge: a touch on the outermost
            // band of a touch screen belongs to the system - a device using
            // gesture navigation keeps a back-gesture band there that no window
            // flag removes - so what sits in it must not be a control. A label
            // is not, and it holds the shape buttons left of the band.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .color(theme::TEXT_DIM),
                );
                self.orientation_button(ui, &mut items);
                let fullscreen = icons::button(
                    ui,
                    if self.fullscreen { Icon::Collapse } else { Icon::Expand },
                    self.fullscreen,
                    &monitor_i18n::tr(if self.fullscreen {
                        "toolbar-fullscreen-leave-tip"
                    } else {
                        "toolbar-fullscreen-enter-tip"
                    }),
                );
                items.push((fullscreen.id, fullscreen.rect));
                self.name(
                    &fullscreen,
                    if self.fullscreen { "toolbar-fullscreen-leave" } else { "toolbar-fullscreen-enter" },
                );
                if fullscreen.clicked() {
                    let enabled = !self.fullscreen;
                    self.set_fullscreen(ui.ctx(), enabled);
                }


                let settings = icons::button(ui, Icon::Gear, self.show_settings, &monitor_i18n::tr("toolbar-settings-tip"));
                items.push((settings.id, settings.rect));
                self.name(&settings, "toolbar-settings");
                if settings.clicked() {
                    self.show_settings = !self.show_settings;
                }

                let devices = icons::button(ui, Icon::Plus, self.discovery.open, &monitor_i18n::tr("toolbar-add-devices-tip"));
                items.push((devices.id, devices.rect));
                self.name(&devices, "toolbar-add-devices");
                if devices.clicked() {
                    self.discovery.open = !self.discovery.open;
                    // Only for a press that came from the keyboard or a remote:
                    // a pointer does not move the focus anywhere else in this
                    // program, so it must not appear to move it here either.
                    self.discovery.from = (self.discovery.open
                        && !devices.clicked_by(egui::PointerButton::Primary))
                    .then_some(devices.id);
                }
            });
        });

        // A right to left group is laid out - and so recorded - from the right
        // end backwards; the reader and the remote both want it left to right.
        items.sort_by(|(_, left), (_, right)| left.center().x.total_cmp(&right.center().x));
        // Registered in that order, which is the order of the layer's row.
        for (id, _) in &items {
            self.nav.item_id(*id);
        }
        self.nav.close();
        self.toolbar_items = items;
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        if self.is_portrait() {
            self.portrait_status_bar(ui);
            return;
        }
        // The status bar shares the toolbar's scale so the two rows read at the
        // same size on a phone in landscape, where the bar shrinks to fit.
        Self::scale_bar(ui);
        ui.horizontal(|ui| {
            // The left half of the row comes and goes - the zoom label, and the
            // name of the control the remote is on - and egui derives a widget's
            // id from how many widgets preceded it in its row. The author at the
            // other end is a control the arrows land on, so that variable half is
            // put in a scope of its own: the row then has the same shape every
            // frame and the author keeps its id. Without this the author is drawn
            // under a different id each time the name appears or goes, and the
            // focus the arrows gave it is dropped the moment it is drawn.
            ui.push_id("xgview-status-left", |ui| {
                let info = self.scheduler.page_info();
                // The group comes first, with no word in front of it, when there
                // is more than one to tell apart: everything after it - the live
                // count, the page, the focus - is about that group.
                if self.config.groups.len() > 1 {
                    ui.label(RichText::new(dialogs::group_name(
                        &self.config,
                        self.config.active_index(),
                    )));
                    ui.separator();
                }
                // How many of the configured channels are actually on the wall,
                // and how many there are to be: the one number that says at a
                // glance whether anything is missing. It reads better down here
                // than in the bar, where it was competing with the controls.
                let live = self.channels.values().filter(|channel| channel.state.is_live()).count();
                ui.label(
                    RichText::new(monitor_i18n::tr_args(
                        "status-live",
                        &[("live", live.into()), ("total", self.config.enabled_count().into())],
                    ))
                    .color(if live > 0 { theme::LIVE } else { theme::TEXT_DIM }),
                );
                ui.separator();
                ui.label(monitor_i18n::tr_args(
                    "status-page",
                    &[
                        ("layout", self.scheduler.layout().label().into()),
                        ("page", (info.page + 1).into()),
                        ("count", info.page_count.into()),
                    ],
                ));
                if self.scheduler.is_zoomed() {
                    ui.separator();
                    ui.label(RichText::new(monitor_i18n::tr("status-zoom")).color(theme::FOCUS));
                }
                ui.separator();
                match self.scheduler.focus() {
                    Some(focus) => ui.label(monitor_i18n::tr_args("status-focus", &[("index", (focus + 1).into())])),
                    None => ui.label(monitor_i18n::tr("status-no-focus")),
                };
                // What the remote control is on, for the controls that carry a
                // shape instead of a word and have no pointer to hover for a
                // tooltip.
                if let Some(name) = self.focused_name {
                    ui.separator();
                    ui.label(RichText::new(monitor_i18n::tr(name)).color(theme::FOCUS));
                }
            });
            // The author is a control of this row, so the row has to be a scope
            // of the navigation layer for the remote to walk onto it - without
            // one, `tracked` drops the id; see `Nav::push`.
            //
            // The group has an id of its own as well: the row on the left grows
            // and shrinks with what it has to say - the name of the focused
            // control appears there the moment the author takes the focus - and
            // without a salt of its own this group's widgets would move to new
            // ids with it, dropping the very focus the arrow just gave them.
            ui.push_id("xgview-status-right", |ui| {
                self.nav.open("status");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Drawn right to left, so the copyright lands at the very
                    // right and the decoder just left of it. The author is a
                    // control of its own rather than a mail link: a remote can
                    // walk onto it, and pressing it opens the About tab, which
                    // carries the address - a `mailto:` is not something a remote
                    // can follow. All three keep the top bar's own text size, and
                    // the author the default control text colour, so the bar
                    // reads as one row.
                    ui.label(
                        RichText::new(monitor_i18n::tr_args("status-copyright", &[("years", monitor_core::copyright_years().into())]))
                            .small()
                            .color(theme::TEXT_DIM),
                    );
                    let (author, _email) = monitor_core::author();
                    // A selectable label rather than a frameless button: this one
                    // draws its frame only while the pointer is on it or it holds
                    // the focus, and a focused control is painted with the focus
                    // fill and the ring - the same feedback the toolbar's own
                    // labels give. A button with no frame draws nothing at all.
                    //
                    // Its padding goes for the same reason the frame is there: a
                    // button is drawn with the button padding around its text and
                    // at least the interaction height, and both are sized for a
                    // control standing on its own - on this row they would open it
                    // up and lift the author off the line the labels either side
                    // of it sit on. With neither, the control is exactly the word,
                    // so the ring lands on the word.
                    let padding = ui.spacing().button_padding;
                    ui.spacing_mut().button_padding = egui::Vec2::ZERO;
                    let link = ui.add(
                        egui::Button::selectable(false, RichText::new(author).small())
                            .small(),
                    );
                    ui.spacing_mut().button_padding = padding;
                    let link = self.nav.tracked(link);
                    self.name(&link, "about-author-name");
                    self.status_author = Some(link.id);
                    if link.clicked() {
                        self.show_settings = true;
                        self.settings_tab = SettingsTab::About;
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(monitor_i18n::tr_args(
                            "status-decoder",
                            &[("backend", self.decoder.into())],
                        ))
                        .small()
                        .color(theme::TEXT_DIM),
                    );
                });
                self.nav.close();
            });
        });
    }

    /// The settings side panel: a tab strip that stays put, and the tab's body
    /// scrolling underneath it.
    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        // The tab strip is a row of the navigation layer: Left and Right move
        // between the tabs and select as they land - the panel follows the
        // focus the way it follows a click - and Down drops into the body.
        ui.horizontal_wrapped(|ui| {
            let mut ids = [Id::NULL; SettingsTab::COUNT];
            let mut focused = None;
            self.nav.open("settings-tabs");
            for (slot, tab) in SettingsTab::ALL.into_iter().enumerate() {
                let selected = self.settings_tab == tab;
                let label = match tab {
                    SettingsTab::Cameras => monitor_i18n::tr_args(
                        "settings-tab-cameras-count",
                        &[("count", self.config.group_camera_count(self.config.active_group).into())],
                    ),
                    other => monitor_i18n::tr(other.label()),
                };
                let response = self.nav.tracked(ui.selectable_label(selected, label));
                self.name(&response, tab.label());
                if response.clicked() {
                    self.settings_tab = tab;
                    // A press takes the focus away from whatever had it and
                    // never gives it to what was pressed, so the tab just
                    // chosen takes it here and now.
                    response.request_focus();
                }
                if response.has_focus() {
                    focused = Some(tab);
                }
                ids[slot] = response.id;
            }
            // An arrow lands on a tab without a press; the panel has to follow
            // it, or the body would keep showing the old tab under the focus.
            if let Some(tab) = focused {
                self.settings_tab = tab;
            }
            // The tab in use is where an Up out of the body comes back to.
            let slot = SettingsTab::ALL.iter().position(|tab| *tab == self.settings_tab).unwrap_or(0);
            self.settings_anchor = Some(ids[slot]);
            self.nav.set_entry("settings-tabs", ids[slot]);
            self.nav.close();
        });
        ui.separator();

        // The body is one column of the navigation layer. See the "Add
        // devices" window for why sideways is spent inside a body.
        self.nav.open("settings-body");
        let mut asked = None;
        let output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The scroll a direction press asked for when it had nowhere to
                // move the focus: it is spent here because this is the only
                // place egui reads a scroll request from - see `body_scroll`.
                if let Some(points) = self.body_scroll.take() {
                    // Negative is downwards, the sign a wheel delta carries:
                    // egui inverts the value again before adding it to the
                    // offset, so a positive one here would scroll up and be
                    // clamped away at the top. See the test in `crate::nav`.
                    ui.scroll_with_delta(egui::vec2(0.0, -points));
                    asked = Some(points);
                }
                match self.settings_tab {
                    SettingsTab::Display => self.settings_display(ui),
                    SettingsTab::Cameras => self.settings_cameras(ui),
                    SettingsTab::Streams => self.settings_streams(ui),
                    SettingsTab::System => self.settings_system(ui),
                    SettingsTab::Blackout => self.settings_blackout(ui),
                    SettingsTab::About => {
                        self.settings_about(ui);
                        false
                    }
                }
            });
        if let Some(points) = asked {
            tracing::debug!(
                target: "xgview::gui",
                points,
                offset = output.state.offset.y,
                content = output.content_size.y,
                view = output.inner_rect.height(),
                "the remote scrolled the settings body"
            );
        }
        let dirty = output.inner;
        self.nav.close();
        if dirty {
            self.mark_dirty();
        }
    }

    /// The wall itself: the grid, what the tiles show, and how the window opens.
    fn settings_display(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;


        ui.label(RichText::new(monitor_i18n::tr("settings-grid")).strong());
        ui.horizontal_wrapped(|ui| {
            for layout in GridLayout::STANDARD {
                let selected = self.scheduler.layout() == layout;
                let response = self.nav.tracked(ui.selectable_label(selected, layout.label()));
                self.name(&response, layout_hint(layout));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }
            let current = self.scheduler.layout();
            let active = !current.is_standard();
            let more = self
                .nav
                .tracked(ui.selectable_label(active || self.layout_picker, current.numeric_label().unwrap_or("…")));
            self.name(&more, "toolbar-layout-more");
            if more.clicked() {
                self.layout_picker = !self.layout_picker;
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                self.layout_picker_from = (self.layout_picker
                    && !more.clicked_by(egui::PointerButton::Primary))
                .then_some(more.id);
            }
        });
        ui.add_space(theme::space::S);
        ui.label(RichText::new(monitor_i18n::tr("settings-osd")).strong());
        ui.label(
            RichText::new(monitor_i18n::tr("settings-osd-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        let corners: [(&str, &mut OsdItem); 4] = [
            ("corner-top-left", &mut self.config.osd.top_left),
            ("corner-top-right", &mut self.config.osd.top_right),
            ("corner-bottom-left", &mut self.config.osd.bottom_left),
            ("corner-bottom-right", &mut self.config.osd.bottom_right),
        ];
        let cycle_opts = OsdItem::cycle_options(&self.config.combined_info);
        for (corner, item) in corners {
            ui.horizontal(|ui| {
                ui.label(monitor_i18n::tr(corner));
                let button = controls::cycle_button(
                    &mut self.nav,
                    ui,
                    &cycle_opts,
                    item,
                    |choice| monitor_i18n::tr(choice.label()),
                );
                self.focus_names.insert(button.id, corner);
                if button.changed() {
                    dirty = true;
                }
            });
        }
        ui.add_space(theme::space::S);
        ui.horizontal(|ui| {
            ui.label(monitor_i18n::tr("settings-combined-info"));
            for slot in 0..4 {
                let label = monitor_i18n::tr_args(
                    "settings-combined-info-slot",
                    &[("n", (slot + 1).to_string().into())],
                );
                let response = self.nav.tracked(ui.button(label));
                self.name(&response, "combined-info-button");
                if response.clicked() {
                    self.combined_info_picker = true;
                    self.combined_info_picker_slot = slot;
                    self.combined_info_picker_from =
                        (!response.clicked_by(egui::PointerButton::Primary)).then_some(response.id);
                    ui.ctx().data_mut(|d| {
                        d.insert_temp(Id::new("xgview-combined-info-focus"), (0usize, 0usize))
                    });
                }
            }
        });
        ui.add_space(theme::space::S);
        let mut fullscreen = self.fullscreen;
        let full = self.nav.tracked(ui.checkbox(&mut fullscreen, monitor_i18n::tr("settings-fullscreen")));
        if full.changed() {
            self.set_fullscreen(ui.ctx(), fullscreen);
        }
        let mut start_fullscreen = self.config.start_fullscreen;
        let start = self.nav.tracked(ui.checkbox(&mut start_fullscreen, monitor_i18n::tr("settings-start-fullscreen")));
        if start.changed() {
            self.config.start_fullscreen = start_fullscreen;
            dirty = true;
        }
        // Windows, macOS and X11 can hold a window above the others; a Wayland
        // session and Android cannot. There it is greyed out rather than left to
        // be a switch that does nothing, the way `monitor_core::power` treats the
        // switches it cannot honour. See `crate::window`.
        let on_top_supported = window::supported();
        let mut on_top = self.config.always_on_top_fullscreen;
        ui.add_enabled_ui(on_top_supported, |ui| {
            let on_top_check = self.nav.tracked(ui.checkbox(&mut on_top, monitor_i18n::tr("settings-on-top")));
            if on_top_check.changed() {
                self.config.always_on_top_fullscreen = on_top;
                self.apply_window_level(ui.ctx());
                dirty = true;
            }
        });
        if !on_top_supported {
            ui.label(RichText::new(monitor_i18n::tr("settings-on-top-unsupported")).small().color(theme::TEXT_DIM));
        }

        // Android only: whether the navigation bar keeps its strip (the wall is
        // narrower) or is hidden with the status bar (immersive). The activity is
        // told from the frame's layout; see `update`.
        #[cfg(target_os = "android")]
        {
            let mut reserve = self.config.reserve_navigation_bar;
            let response = self.nav.tracked(ui.checkbox(&mut reserve, monitor_i18n::tr("settings-reserve-navigation-bar")));
            if response.changed() {
                self.config.reserve_navigation_bar = reserve;
                dirty = true;
            }
            ui.label(
                RichText::new(monitor_i18n::tr("settings-reserve-navigation-bar-hint"))
                    .small()
                    .color(theme::TEXT_DIM),
            );
        }

        ui.add_space(theme::space::S);
        let idle = controls::slider(
            &mut self.nav,
            ui,
            &mut self.config.chrome_idle_secs,
            0.0..=10.0,
            0.5,
            "s",
            &monitor_i18n::tr("settings-chrome-idle"),
            3.5,
        );
        if idle.changed() {
            dirty = true;
        }
        ui.label(
            RichText::new(monitor_i18n::tr("settings-chrome-idle-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        ui.add_space(theme::space::S);
        if self.settings_language(ui) {
            dirty = true;
        }

        dirty
    }

    /// The interface language picker.
    ///
    /// Switching it takes effect at once: the catalogue is swapped, the fonts
    /// are reinstalled for the language's own glyphs, and a repaint draws the
    /// new text. The choice is written back to the configuration like any other
    /// setting, so the next start comes up in it.
    fn settings_language(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;
        ui.label(RichText::new(monitor_i18n::tr("settings-language")).strong());
        ui.label(
            RichText::new(monitor_i18n::tr("settings-language-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        // The languages are one cycling button, drawn by `controls::cycle_button`
        // - the same helper the OSD corners below use: the language in force is
        // shown, and a Left / Right press - or Enter, as on those buttons -
        // steps to the next one. The whole list never has to fit on the panel,
        // and the remote needs no other gesture to reach it.
        let chosen = monitor_i18n::current_language();
        let available = monitor_i18n::available();
        let is_auto = self.config.language.is_empty() || self.config.language == "auto";
        // `None` is auto, then a language by id in the order the catalogue
        // lists them - English first.
        let mut choices: Vec<Option<String>> = Vec::with_capacity(available.len() + 1);
        choices.push(None);
        choices.extend(available.iter().map(|info| Some(info.id.clone())));
        let mut choice = if is_auto { None } else { Some(chosen) };
        let auto_label = monitor_i18n::tr("settings-language-auto");
        let button = controls::cycle_button(&mut self.nav, ui, &choices, &mut choice, |choice| {
            match choice {
                None => auto_label.clone(),
                Some(id) => available
                    .iter()
                    .find(|info| info.id == *id)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| id.clone()),
            }
        });
        self.focus_names.insert(button.id, "settings-language");
        if button.changed() {
            if let Some(id) = choice {
                // Always record the explicit choice so the user leaves auto
                // mode even when the language is the same as the one
                // auto-detection picked.
                let switched = monitor_i18n::set_language(&id);
                self.config.language = id.clone();
                if switched {
                    fonts::install_for(ui.ctx(), &id);
                }
            } else {
                self.config.language = "auto".to_string();
                monitor_i18n::set_language("auto");
                fonts::install_for(ui.ctx(), &monitor_i18n::current_language());
            }
            ui.ctx().request_repaint();
            dirty = true;
        }

        dirty
    }

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

    /// The hours the wall blacks itself out, and what that does.
    fn settings_blackout(&mut self, ui: &mut egui::Ui) -> bool {
        // The tab strip already says which tab this is, so the body opens with
        // its hint - small and dim, as the other tabs open.
        ui.label(
            RichText::new(monitor_i18n::tr("settings-blackout-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::space::M);

        // The navigation layer and the schedule are taken as locals for the
        // whole body: its rows are drawn inside layout closures, and a closure
        // cannot borrow `self` while the rows are walking it.
        let nav = &mut self.nav;
        let blackout = &mut self.config.blackout;
        let enabled = blackout.enabled;
        let mut dirty = false;

        // The switch comes first because everything below it is inert while it
        // is off, and the rows are greyed rather than hidden so that what is
        // being switched off can still be read. It carries its own label, as
        // every other standalone setting does - a bare box under a heading left
        // it unclear what it switched.
        let switch = ui.checkbox(&mut blackout.enabled, monitor_i18n::tr("settings-blackout-on"));
        nav.item(&switch);
        dirty |= switch.changed();

        // The wait after an interruption. Zero is a real choice - the
        // interruption then lasts the period out - so it is a slider value
        // rather than something hidden behind a checkbox.
        let resume = controls::slider(
            nav,
            ui,
            &mut blackout.resume_after_minutes,
            0..=60,
            1.0,
            &monitor_i18n::tr("settings-blackout-minutes"),
            &monitor_i18n::tr("settings-blackout-resume"),
            1,
        );
        dirty |= resume.changed();
        // Shown whether or not the wait is zero: what zero means has to be
        // readable at the moment it is chosen.
        ui.label(
            RichText::new(monitor_i18n::tr("settings-blackout-resume-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::space::M);

        // Which period is in force, or why none can be: a viewer should be able
        // to tell a wall that is about to blank from one that will not.
        let status = if !blackout.enabled {
            monitor_i18n::tr("settings-blackout-off")
        } else if let Some(window) = monitor_core::blackout::current(blackout) {
            monitor_i18n::tr_args(
                "settings-blackout-active",
                &[("until", monitor_core::blackout::format_clock(window.end).into())],
            )
        } else if blackout.is_empty() {
            monitor_i18n::tr("settings-blackout-empty")
        } else {
            monitor_i18n::tr("settings-blackout-idle")
        };
        ui.label(RichText::new(status).small().color(theme::TEXT_DIM));
        ui.add_space(theme::space::M);

        // Borrowed only now, so that the status line above could read the whole
        // schedule: a mutable borrow of the entries would have excluded it.
        let entries = &mut blackout.entries;
        let mut remove = None;
        ui.add_enabled_ui(enabled, |ui| {
            for (index, window) in entries.iter_mut().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    let switch = ui.checkbox(&mut window.enabled, "");
                    nav.item(&switch);
                    dirty |= switch.changed();
                    // Last, because it takes the response with it.
                    switch.on_hover_text(monitor_i18n::tr("settings-blackout-enabled"));

                    ui.label(monitor_i18n::tr("settings-blackout-days"));
                    for day in 0..7u8 {
                        let on = window.days & (1 << day) != 0;
                        let label = monitor_i18n::tr(crate::blackout::DAY_KEYS[usize::from(day)]);
                        let toggle = ui.selectable_label(on, label);
                        nav.item(&toggle);
                        if toggle.clicked() {
                            window.days ^= 1 << day;
                            dirty = true;
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    dirty |= clock_field(nav, ui, &mut window.start);
                    ui.label(monitor_i18n::tr("settings-blackout-to"));
                    dirty |= clock_field(nav, ui, &mut window.end);
                    let trash = icons::button(
                        ui,
                        icons::Icon::Trash,
                        false,
                        &monitor_i18n::tr("settings-blackout-remove"),
                    );
                    nav.item(&trash);
                    if trash.clicked() {
                        remove = Some(index);
                    }
                });
                ui.add_space(8.0);
            }

            if let Some(index) = remove {
                entries.remove(index);
                dirty = true;
            }
            let add = ui.add_enabled(
                enabled,
                egui::Button::new(monitor_i18n::tr("settings-blackout-add")),
            );
            nav.item(&add);
            if add.clicked() {
                entries.push(monitor_core::config::BlackoutWindow::default());
                dirty = true;
            }
        });

        if dirty {
            // The blank is worked out from the configuration on every frame, so
            // there is nothing to hand to anything: the next frame acts on this.
            self.needs_sync = true;
        }
        dirty
    }

    /// What the box does around the viewer: it starts it at boot, and it is the
    /// machine the decoder is chosen for.
    fn settings_system(&mut self, ui: &mut egui::Ui) -> bool {
        // Only the desktop start-up section below changes anything on this tab.
        #[cfg(not(target_os = "android"))]
        let mut dirty = false;
        #[cfg(target_os = "android")]
        let mut dirty = false;

        ui.label(RichText::new(monitor_i18n::tr("settings-startup")).strong());
        // Desktop: the registration is ours to make, so it is a checkbox. On
        // Android it belongs to the manifest and the system, and the section
        // below shows the two things the device asks for instead.
        #[cfg(not(target_os = "android"))]
        {
            let supported = self.autostart.supported;
            let mut enabled = self.config.autostart;
            ui.add_enabled_ui(supported, |ui| {
                let boot = ui.checkbox(&mut enabled, monitor_i18n::tr("settings-autostart"));
                self.nav.item(&boot);
                if boot.changed() {
                    match autostart::set_enabled(enabled) {
                        Ok(()) => {
                            self.config.autostart = enabled;
                            self.autostart = autostart::status();
                            dirty = true;
                            self.flash(
                                monitor_i18n::tr(if enabled { "toast-autostart-on" } else { "toast-autostart-off" }),
                                ToastKind::Info,
                            );
                        }
                        Err(err) => {
                            self.autostart = autostart::status();
                            self.flash(
                                monitor_i18n::tr_args(
                                    "toast-autostart-failed",
                                    &[("error", err.to_string().into())],
                                ),
                                ToastKind::Error,
                            );
                        }
                    }
                }
            });
            if !supported {
                ui.label(RichText::new(monitor_i18n::tr("settings-autostart-unsupported")).small().color(theme::TEXT_DIM));
            }
            ui.label(
                RichText::new(monitor_i18n::tr_args(
                    "settings-mechanism",
                    &[("name", monitor_i18n::tr(&self.autostart.mechanism).into())],
                ))
                .small()
                .color(theme::TEXT_DIM),
            );
            ui.label(RichText::new(&self.autostart.detail).small().color(theme::TEXT_DIM));

            ui.add_space(theme::space::S);
            // One window per configuration: a second launch of this
            // configuration asks the one that is up to come forward instead of
            // opening another wall over it. It takes effect on the next launch,
            // like the start-up full screen above, and the lock follows the
            // configuration file rather than the installation - see
            // `crate::instance`.
            let mut one_instance = self.config.single_instance;
            let only = ui.checkbox(&mut one_instance, monitor_i18n::tr("settings-single-instance"));
            self.nav.item(&only);
            if only.changed() {
                self.config.single_instance = one_instance;
                dirty = true;
            }
        }
        #[cfg(target_os = "android")]
        self.android_boot_section(ui);

        ui.add_space(theme::space::L);
        ui.label(RichText::new(monitor_i18n::tr("settings-power")).strong());
        // Two switches, one request: `monitor_core::power` normalises them, so
        // the line under them says what the pair actually asks the machine for
        // rather than leaving the viewer to work out why one tick moved both.
        let power_supported = self.power.supported;
        let mut prevent_sleep = self.config.prevent_sleep;
        let mut keep_screen_on = self.config.keep_screen_on;
        ui.add_enabled_ui(power_supported, |ui| {
            let sleep = ui.checkbox(&mut prevent_sleep, monitor_i18n::tr("settings-prevent-sleep"));
            self.nav.item(&sleep);
            if sleep.changed() {
                dirty |= self.apply_power(prevent_sleep, keep_screen_on);
            }
            let screen = ui.checkbox(&mut keep_screen_on, monitor_i18n::tr("settings-keep-screen-on"));
            self.nav.item(&screen);
            if screen.changed() {
                dirty |= self.apply_power(prevent_sleep, keep_screen_on);
            }
        });
        ui.label(RichText::new(monitor_i18n::tr("settings-power-hint")).small().color(theme::TEXT_DIM));
        if !power_supported {
            ui.label(RichText::new(monitor_i18n::tr("settings-power-unsupported")).small().color(theme::TEXT_DIM));
        }
        ui.label(
            RichText::new(monitor_i18n::tr_args(
                "settings-mechanism",
                &[("name", monitor_i18n::tr(self.power.mechanism).into())],
            ))
            .small()
            .color(theme::TEXT_DIM),
        );
        if let Some(error) = &self.power.error {
            ui.label(RichText::new(error).small().color(theme::ERROR));
        }

        ui.add_space(theme::space::L);
        ui.label(RichText::new(monitor_i18n::tr("settings-keys")).strong());
        // One shortcut per row, the key in a monospace column of its own so the
        // list reads down the keys and not across a line of dots.
        egui::Grid::new("xgview-system-keys")
            .num_columns(2)
            .spacing([theme::space::L, theme::space::XS])
            .show(ui, |ui| {
                for (keys, action) in [
                    ("DPAD / arrows", "key-move-focus"),
                    ("Enter", "key-enter"),
                    ("Esc / Back", "key-back"),
                    ("Num (1-4)", "key-grid"),
                    ("PgUp / PgDn", "key-page"),
                    ("F1", "key-settings"),
                    ("F2", "key-add-devices"),
                    ("F11", "key-fullscreen"),
                ] {
                    ui.label(RichText::new(keys).monospace().strong());
                    ui.label(RichText::new(monitor_i18n::tr(action)).small().color(theme::TEXT_DIM));
                    ui.end_row();
                }
            });

        ui.add_space(theme::space::L);
        ui.label(RichText::new(monitor_i18n::tr("settings-actions")).strong());
        // Two actions that end the session. Exit leaves at once — a viewer that
        // pressed it has read the label. Reboot is behind a confirmation window:
        // a press beside a neighbour must not restart the machine on its own,
        // and it is drawn only where the process has the privilege to do it.
        let mut do_exit = false;
        let mut ask_reboot: Option<Option<Id>> = None;
        ui.horizontal(|ui| {
            let exit_label = monitor_i18n::tr("action-exit");
            let exit_btn = self.nav.tracked(ui.button(exit_label));
            if exit_btn.clicked() {
                do_exit = true;
            }
            if reboot::is_supported() {
                let reboot_label = monitor_i18n::tr("action-reboot");
                let reboot_btn = self.nav.tracked(ui.button(reboot_label));
                if reboot_btn.clicked() {
                    // Only for a press that came from the keyboard or a remote:
                    // a pointer does not move the focus anywhere else in this
                    // program, so it must not appear to move it here either.
                    ask_reboot = Some(
                        (!reboot_btn.clicked_by(egui::PointerButton::Primary))
                            .then_some(reboot_btn.id),
                    );
                }
            }
        });
        if do_exit {
            self.quit(ui.ctx());
        }
        if let Some(from) = ask_reboot {
            self.system_confirm = Some(SystemConfirm { from });
        }

        dirty
    }

    /// The Android start-on-boot section.
    ///
    /// Android starts nothing from the boot broadcast unless the system allows
    /// it, and there is nothing to toggle from here: the receiver is declared in
    /// the manifest. What the device asks for is one of two things, so both are
    /// shown with the screen that sets each, and the adb route is written out
    /// for a deployment that has a computer at hand.
    #[cfg(target_os = "android")]
    fn android_boot_section(&mut self, ui: &mut egui::Ui) {
        // The two states only change while the viewer is away in the system
        // settings, so reading them twice a second is enough and keeps the JNI
        // calls off every frame.
        if self.time - self.android_boot_at > 0.5 {
            self.android_boot = (crate::android::overlay_allowed(), crate::android::is_home_app());
            self.android_boot_at = self.time;
        }
        let (overlay, home) = self.android_boot;

        ui.label(
            RichText::new(monitor_i18n::tr("android-boot-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::space::S);

        ui.horizontal(|ui| {
            let button = ui.button(monitor_i18n::tr("android-home-button"));
            self.nav.item(&button);
            self.name(&button, "android-home-name");
            if button.clicked() {
                crate::android::open_home_settings();
                self.android_boot_at = f64::NEG_INFINITY;
            }
            ui.label(
                RichText::new(monitor_i18n::tr(if home { "android-home-yes" } else { "android-home-no" }))
                    .small()
                    .color(if home { theme::LIVE } else { theme::TEXT_DIM }),
            );
        });
        ui.horizontal(|ui| {
            let button = ui.button(monitor_i18n::tr("android-overlay-button"));
            self.nav.item(&button);
            self.name(&button, "android-overlay-name");
            if button.clicked() {
                crate::android::open_overlay_settings();
                self.android_boot_at = f64::NEG_INFINITY;
            }
            ui.label(
                RichText::new(monitor_i18n::tr(if overlay { "android-overlay-yes" } else { "android-overlay-no" }))
                    .small()
                    .color(if overlay { theme::LIVE } else { theme::TEXT_DIM }),
            );
        });

        ui.add_space(theme::space::S);
        ui.label(
            RichText::new(monitor_i18n::tr_args("android-adb-hint", &[("nl", "\n".into())]))
            .small()
            .color(theme::TEXT_DIM),
        );
    }

    /// What the viewer pulls off each camera, and what turns it into pictures.
    fn settings_streams(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        ui.label(
            RichText::new(monitor_i18n::tr("streams-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        ui.add_space(theme::space::M);
        ui.label(RichText::new(monitor_i18n::tr("settings-reconnect")).strong());
        let reconnect = &mut self.config.reconnect;
        // Borrowed beside the policy: they are two fields of the same struct,
        // and the slider that owns it sits among the retry ones.
        let handshake_timeout_ms = &mut self.config.handshake_timeout_ms;
        let first = controls::slider(&mut self.nav, ui, &mut reconnect.initial_delay_ms, 100..=10_000, 100.0, " ms", &monitor_i18n::tr("streams-first-retry"), 1_000);
        dirty |= first.changed();
        let first_frame = controls::slider(&mut self.nav, ui, &mut reconnect.first_frame_delay_ms, 1_000..=30_000, 500.0, " ms", &monitor_i18n::tr("streams-first-frame-retry"), 5_000);
        dirty |= first_frame.changed();
        let second = controls::slider(&mut self.nav, ui, &mut reconnect.max_delay_ms, 1_000..=300_000, 1000.0, " ms", &monitor_i18n::tr("streams-then-at-most"), 30_000);
        dirty |= second.changed();
        // One attempt's budget for establishing the session. It sits with the
        // retry timings because it is read at the same moment and reaches a
        // stalled channel the same way, and because a camera reached over the
        // internet can need far more of it than one on the local network.
        let handshake = controls::slider(&mut self.nav, ui, handshake_timeout_ms, 5_000..=60_000, 1_000.0, " ms", &monitor_i18n::tr("streams-handshake-timeout"), 15_000);
        dirty |= handshake.changed();
        ui.label(RichText::new(monitor_i18n::tr("streams-change-note")).small().color(theme::TEXT_DIM));
        // The shape of the curve is settled once, by whoever sized the network,
        // and never looked at again: it belongs behind a fold, not on the tab.
        // The fold is a stop of its own, ahead of the controls it hides; see
        // `controls::fold`.
        let backoff = monitor_i18n::tr("streams-backoff");
        controls::fold(&mut self.nav, ui, &backoff, false, |ui, nav| {
            let factor = controls::slider(nav, ui, &mut reconnect.multiplier, 1.0..=5.0, 0.05, "", &monitor_i18n::tr("streams-factor"), 1.8);
            dirty |= factor.changed();
            let jitter = controls::slider(nav, ui, &mut reconnect.jitter, 0.0..=1.0, 0.02, "", &monitor_i18n::tr("streams-jitter"), 0.2);
            dirty |= jitter.changed();
            let spread = controls::slider(nav, ui, &mut reconnect.spread_ms, 0..=1_000, 50.0, " ms", &monitor_i18n::tr("streams-spread"), 100);
            dirty |= spread.changed();
            let attempts = controls::slider(nav, ui, &mut reconnect.max_attempts, 0..=100, 1.0, "", &monitor_i18n::tr("streams-attempts"), 0);
            dirty |= attempts.changed();
        });

        // At this point `dirty` is only the connection sliders above. They are
        // handed to the supervisor so that the channels already running pick the
        // new values up at their next attempt instead of only the ones opened
        // after the change - a running channel holds no copy of its own to go
        // stale.
        if dirty {
            self.manager.set_reconnect_policy(self.config.reconnect.clone());
            self.manager
                .set_handshake_timeout(Duration::from_millis(self.config.handshake_timeout_ms));
        }

        ui.add_space(theme::space::M);
        ui.label(RichText::new(monitor_i18n::tr("settings-decoding")).strong());
        let mut prefer_hardware = self.config.prefer_hardware_decode;
        ui.add_enabled_ui(self.decoder_selectable, |ui| {
            let hardware = ui.checkbox(&mut prefer_hardware, monitor_i18n::tr("streams-prefer-hardware"));
            self.nav.item(&hardware);
            if hardware.changed() {
                self.config.prefer_hardware_decode = prefer_hardware;
                // A decoder is chosen when a session opens, so the change is applied
                // by reopening the channels rather than on the next connection.
                self.manager.set_hardware_preference(prefer_hardware);
                dirty = true;
                self.flash(
                    monitor_i18n::tr(if prefer_hardware {
                        "toast-decoding-gpu"
                    } else {
                        "toast-decoding-cpu"
                    }),
                    ToastKind::Info,
                );
            }
        });
        // A build with no hardware decoder at all is told apart first: the
        // "nothing to switch" wording describes a platform that decodes in
        // hardware and sees to its own fallbacks, which such a build is not.
        let summary = if !self.hardware_decoder {
            monitor_i18n::tr("streams-summary-software")
        } else if !self.decoder_selectable {
            monitor_i18n::tr("streams-summary-fixed")
        } else {
            monitor_i18n::tr("streams-summary-available")
        };
        ui.label(RichText::new(summary).small().color(theme::TEXT_DIM));

        ui.add_space(theme::space::M);
        ui.label(RichText::new(monitor_i18n::tr("settings-audio")).strong());
        // The volume slider carries the sound switch on its right, the way the
        // magnified bar does: the icon shows the same `sound_on` the bar flips,
        // so the two always agree. The reset button the shared `controls::slider`
        // would draw is left out - the switch is the row's second control.
        let (volume, mute) = ui
            .horizontal(|ui| {
                ui.label(monitor_i18n::tr("streams-volume"));
                let slider_response =
                    ui.add(egui::Slider::new(&mut self.config.volume, 0.0..=1.0).step_by(0.05));
                // The glyph is the state: a speaker when sound is on, a crossed
                // speaker when it is not - the same pair the magnified bar
                // draws. The button is as tall as a text button on this row, so
                // that the slider, the words and the switch share one baseline,
                // and the drawing takes most of that box rather than the
                // toolbar's half: a line-high square would otherwise leave the
                // shape looking like a note beside the text.
                let side = ui.text_style_height(&egui::TextStyle::Button)
                    + 2.0 * ui.spacing().button_padding.y;
                let mute = icons::button_sized_glyph(
                    ui,
                    if self.sound_on { Icon::Sound } else { Icon::Mute },
                    false,
                    false,
                    &monitor_i18n::tr("streams-sound"),
                    side,
                    0.7,
                );
                (slider_response, mute)
            })
            .inner;
        self.nav.item_kind(Kind::Slider, &volume);
        self.nav.item(&mute);
        if mute.clicked() {
            self.sound_on = !self.sound_on;
            self.config.sound_on = self.sound_on;
            self.apply_audio();
            self.mark_dirty();
            dirty = true;
        }
        if volume.changed() {
            if let Some(sink) = &self.audio {
                sink.set_volume(self.config.volume);
            }
            dirty = true;
        }
        ui.label(RichText::new(monitor_i18n::tr("streams-volume-hint")).small().color(theme::TEXT_DIM));

        ui.add_space(theme::space::S);
        let talk_mute = ui.checkbox(
            &mut self.config.talk_mute,
            monitor_i18n::tr("streams-talk-mute"),
        );
        self.nav.item(&talk_mute);
        self.name(&talk_mute, "settings-talk-mute");
        if talk_mute.changed() {
            self.apply_audio();
            self.mark_dirty();
            dirty = true;
        }
        ui.label(
            RichText::new(monitor_i18n::tr("streams-talk-mute-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        // Which device the sound leaves through, and which one the microphone
        // listens to. A machine with one sound card never needs this; a kiosk
        // with a USB dongle and a television on HDMI does, because the system's
        // default is card 0 and that is not always the one with a speaker behind
        // it. The rows are left out entirely when the platform has nothing to
        // list - Android, whose audio routing is the system's, and a machine that
        // reports no devices at all.
        dirty |= self.audio_device_rows(ui);

        dirty
    }

    /// The output and input device choices, and whether either changed.
    ///
    /// The names are the platform's own and are not translated: `plughw:CARD=PCH,DEV=3`
    /// is a fact about the machine, not a word in a language. The first entry of
    /// each list is the system's default, which is what a configuration written
    /// before this existed means.
    fn audio_device_rows(&mut self, ui: &mut egui::Ui) -> bool {
        /// The values the control cycles through: the system's default, every
        /// device the platform lists, and the configured one even when the list
        /// has lost it - a device that is open can drop out of the enumeration,
        /// and the one in use has to stay reachable to be cycled away from.
        fn options_for(
            devices: &[crate::audio::AudioDevice],
            current: Option<&str>,
        ) -> Vec<Option<String>> {
            let mut options: Vec<Option<String>> = std::iter::once(None)
                .chain(devices.iter().map(|device| Some(device.name.clone())))
                .collect();
            if let Some(current) = current {
                if !options.iter().any(|option| option.as_deref() == Some(current)) {
                    options.push(Some(current.to_string()));
                }
            }
            options
        }

        let mut changed = false;
        let outputs = self.audio_outputs.clone();
        let inputs = self.audio_inputs.clone();
        let system = monitor_i18n::tr("streams-audio-system");

        // A cycling button, not a drop-down: the popup a `ComboBox` opens is
        // egui's own and the navigation layer cannot walk into it, so a remote
        // would reach the control and then have no way to choose. Left and Right
        // - or Enter, or a click - step to the next device and wrap at the ends,
        // which is how the language picker and the OSD corners already work.
        if !outputs.is_empty() {
            ui.add_space(theme::space::S);
            ui.label(monitor_i18n::tr("streams-audio-output"));
            let mut choice = self.config.audio_output.clone();
            let options = options_for(&outputs, self.config.audio_output.as_deref());
            let button = controls::cycle_button(&mut self.nav, ui, &options, &mut choice, |choice| {
                match choice {
                    None => system.clone(),
                    Some(name) => crate::audio::device_label(name, true),
                }
            });
            self.focus_names.insert(button.id, "settings-audio-output");
            if button.changed() {
                self.config.audio_output = choice;
                self.reopen_audio_sink();
                changed = true;
            }
        }

        if !inputs.is_empty() {
            ui.add_space(theme::space::S);
            ui.label(monitor_i18n::tr("streams-audio-input"));
            let mut choice = self.config.audio_input.clone();
            let options = options_for(&inputs, self.config.audio_input.as_deref());
            let button = controls::cycle_button(&mut self.nav, ui, &options, &mut choice, |choice| {
                match choice {
                    None => system.clone(),
                    Some(name) => crate::audio::device_label(name, false),
                }
            });
            self.focus_names.insert(button.id, "settings-audio-input");
            if button.changed() {
                self.config.audio_input = choice;
                // The capture is opened on the press, so a call in progress is
                // ended here rather than left listening to the old device: the
                // microphone is reopened, on the new one, at the next press.
                if self.talking {
                    self.set_talking(false);
                }
                self.mic = None;
                changed = true;
            }
        }

        if changed {
            self.mark_dirty();
        }
        changed
    }

    /// Reopens the sink after the output device changed.
    ///
    /// The old one is dropped with the new one's arrival: a stream is not
    /// reopened in place, and the ring buffer it was draining goes with it.
    fn reopen_audio_sink(&mut self) {
        self.audio = crate::audio::Sink::open(self.config.audio_output.as_deref());
        match &self.audio {
            Some(sink) => {
                sink.set_volume(self.config.volume);
                self.manager.audio().set_output(Some(sink.clone()));
            }
            None => self.manager.audio().set_output(None),
        }
        self.apply_audio();
    }

    /// The cameras the wall shows, and the way to add more.
    fn settings_cameras(&mut self, ui: &mut egui::Ui) -> bool {
        // The reorder mode draws its own list and writes nothing back until it
        // is confirmed. See `cameras_reorder`.
        if let Some(order) = self.reorder.clone() {
            self.cameras_reorder(ui, &order);
            ui.add_space(theme::space::M);
            let mut confirm_clicked = false;
            let mut cancel_clicked = false;
            ui.horizontal(|ui| {
                let confirm = ui.button(monitor_i18n::tr("cameras-confirm-order"));
                self.nav.item(&confirm);
                self.name(&confirm, "cameras-confirm-order");
                confirm_clicked = confirm.clicked();
                let cancel = ui.button(monitor_i18n::tr("cancel"));
                self.nav.item(&cancel);
                self.name(&cancel, "cameras-cancel-order");
                cancel_clicked = cancel.clicked();
            });
            if cancel_clicked {
                self.cancel_reorder();
                return false;
            }
            if confirm_clicked {
                return self.apply_reorder();
            }
            return false;
        }
        self.cameras_list(ui)
    }

    /// Parks the view the scheduler holds into the group it belongs to: the
    /// configuration is where a group's layout, paging and focus wait while it
    /// is not on the wall.
    fn park_group_view(&mut self) {
        let (layout, page, focus) =
            (self.scheduler.layout(), self.scheduler.page(), self.scheduler.focus());
        let portrait = self.orientation.is_portrait();
        let group = self.config.group_mut();
        if portrait {
            group.portrait_layout = layout;
        } else {
            group.layout = layout;
        }
        group.page = page;
        group.focus = focus;
    }

    /// Loads the active group's view into the scheduler. The cameras follow on
    /// the next [`Self::sync`].
    ///
    /// The magnified viewport goes with the wall it was opened on: a channel
    /// index is a place in a group's camera list, so keeping it would open the
    /// new group magnified on whatever sits at that index there. A group
    /// arriving on the wall arrives whole, on the page and focus it was left at.
    fn load_group_view(&mut self) {
        self.tile_bar.hide();
        self.scheduler.zoom_out();
        let group = self.config.group().clone();
        let layout =
            if self.orientation.is_portrait() { group.portrait_layout } else { group.layout };
        self.scheduler.set_layout(layout);
        self.scheduler.set_page(group.page);
        self.scheduler.set_focus(group.focus);
        self.needs_sync = true;
        self.apply_audio();
    }

    /// Switches the wall to another camera group.
    fn switch_group(&mut self, index: usize) {
        if index == self.config.active_index() {
            return;
        }
        self.park_group_view();
        self.config.active_group = index;
        self.load_group_view();
    }

    /// The camera list as configured, with the button that opens the reorder
    /// mode at its foot. Returns `true` when the configuration changed.
    fn cameras_list(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        // Which group the wall is showing comes first, as a strip of tabs like
        // the settings tabs above, with the button that adds one at its end: the
        // buttons under it add to that group, and the list below it is that
        // group's. Renaming and removing are the uncommon half of the choice, so
        // their controls are at the foot of the page instead.
        let active = self.config.active_group;
        let labels: Vec<String> = (0..self.config.groups.len())
            .map(|index| {
                monitor_i18n::tr_args(
                    "settings-group-label",
                    &[
                        ("name", dialogs::group_name(&self.config, index).into()),
                        ("count", self.config.group_camera_count(index).into()),
                    ],
                )
            })
            .collect();
        let mut switch = None;
        let mut add = false;
        ui.horizontal_wrapped(|ui| {
            for (index, label) in labels.into_iter().enumerate() {
                let tab = ui.selectable_label(active == index, label);
                self.nav.item(&tab);
                if tab.clicked() {
                    switch = Some(index);
                }
            }
            let button = ui.button(monitor_i18n::tr("settings-group-add"));
            self.nav.item(&button);
            if button.clicked() {
                add = true;
            }
        });
        if let Some(index) = switch {
            self.switch_group(index);
            dirty = true;
        }
        if add {
            // The new group is named for the viewer to type over, and it is the
            // one on the wall - so the page under the strip is its page, and the
            // field that names it takes the focus.
            let index = self.config.add_group(monitor_i18n::tr("settings-group-new"));
            self.switch_group(index);
            self.focus_group_name = true;
            dirty = true;
        }
        ui.add_space(theme::space::S);

        ui.horizontal(|ui| {
            let devices = ui.button(monitor_i18n::tr("cameras-add-devices"));
            self.nav.item(&devices);
            if devices.clicked() {
                self.discovery.open = true;
                self.discovery.tab = Tab::Onvif;
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                self.discovery.from =
                    (!devices.clicked_by(egui::PointerButton::Primary)).then_some(devices.id);
            }
            let manual = ui.button(monitor_i18n::tr("cameras-add-manually"));
            self.nav.item(&manual);
            if manual.clicked() {
                self.discovery.open = true;
                self.discovery.tab = Tab::Manual;
                self.discovery.from =
                    (!manual.clicked_by(egui::PointerButton::Primary)).then_some(manual.id);
            }
        });
        ui.add_space(theme::space::S);

        let mut toggle: Option<(usize, bool)> = None;
        let mut ask_remove: Option<(usize, String, Option<Id>)> = None;
        let mut edit: Option<(usize, Option<Id>)> = None;
        let mut aspect_change: Option<(usize, TileAspect)> = None;
        let mut infer: Option<usize> = None;
        let mut connect_change: Option<usize> = None;
        let mut stream_change: Option<usize> = None;
        // Only the group the selector chose. The slot is a position in the whole
        // list, which is what the actions below write back through.
        let active = self.config.active_group;
        let slots: Vec<usize> = self
            .config
            .cameras
            .iter()
            .enumerate()
            .filter(|(_, camera)| usize::from(camera.group) == active)
            .map(|(slot, _)| slot)
            .collect();
        for slot in slots {
            let camera = &mut self.config.cameras[slot];
            ui.horizontal(|ui| {
                let mut camera_enabled = camera.enabled;
                let enable = ui.checkbox(&mut camera_enabled, "");
                self.nav.item(&enable);
                if enable.changed() {
                    toggle = Some((slot, camera_enabled));
                }
                ui.label(RichText::new(camera.short_label(18)).strong());
                if let Some(sub) = camera.rtsp_sub.as_deref() {
                    let inferred = sub_is_derived(camera, sub);
                    ui.label(
                        RichText::new(monitor_i18n::tr(if inferred { "cameras-sub-derived" } else { "cameras-sub-set" }))
                            .small()
                            .color(theme::ACCENT),
                    );
                } else if !camera.is_restream() && infer_sub_stream(&camera.rtsp_main).is_some() {
                    // The vendor sub-stream rules only match a camera's own
                    // address; a go2rtc or Frigate restream is a stream name, so
                    // the offer is drawn only where there is something to infer.
                    let infer_button = ui.small_button(monitor_i18n::tr("cameras-infer-sub"));
                    self.nav.item(&infer_button);
                    if infer_button.clicked() {
                        infer = Some(slot);
                    }
                }
                // The stream switch: a press moves this tile between the sub
                // stream the wall pulls on every other one and the main stream
                // the viewer pinned it to. It reads the tag the tile's corner
                // reads - see `grid`'s stream item - under the same dot, so the
                // row and the picture say the same thing, and it is drawn only
                // where there is a second address to pull: with one address both
                // of its stops are the same stream.
                if camera.has_sub() {
                    let stream_button = ui
                        .small_button(
                            RichText::new(format!("● {}", monitor_i18n::tr(camera.grid_stream().tag())))
                                .small()
                                .color(theme::ACCENT),
                        )
                        .on_hover_text(monitor_i18n::tr("cameras-stream-tip"));
                    self.nav.item(&stream_button);
                    if stream_button.clicked() {
                        stream_change = Some(slot);
                    }
                }
                // The connection switch: a press moves the camera on to the next
                // stop it offers - `AUTO/RTSP/MSE` for a camera with an MSE
                // address, `TCP/UDP` for one on RTSP alone. Its tip names what it
                // switches: the protocol where there are two to choose from, the
                // RTSP transport where there is only RTSP. A camera carried over
                // a WebSocket url has a single transport, so it has nothing to
                // switch and the button is left off.
                if !camera.is_ws_stream() {
                    let tip = if camera.has_mse() { "cameras-connect-tip" } else { "cameras-transport-tip" };
                    let connect_button = ui
                        .small_button(RichText::new(camera.connection_label()).small().color(theme::ACCENT))
                        .on_hover_text(monitor_i18n::tr(tip));
                    self.nav.item(&connect_button);
                    if connect_button.clicked() {
                        connect_change = Some(slot);
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Drawn right to left so the pair reads edit, remove;
                    // registered edit first, so the walk reaches the control
                    // that changes before the one that deletes.
                    let remove_button = icons::button_sized(ui, Icon::Trash, false, false, &monitor_i18n::tr("cameras-remove-tip"), ROW_ICON);
                    let edit_button = icons::button_sized(ui, Icon::Edit, false, false, &monitor_i18n::tr("cameras-edit-tip"), ROW_ICON);
                    self.nav.item(&edit_button);
                    self.focus_names.insert(edit_button.id, "cameras-edit-name");
                    self.nav.item(&remove_button);
                    self.focus_names.insert(remove_button.id, "cameras-remove-name");
                    if remove_button.clicked() {
                        // Only for a press that came from the keyboard or a
                        // remote: a pointer does not move the focus anywhere else
                        // in this program, so it must not appear to move it here
                        // either.
                        let from = (!remove_button.clicked_by(egui::PointerButton::Primary))
                            .then_some(remove_button.id);
                        ask_remove = Some((slot, camera.name.clone(), from));
                    }
                    if edit_button.clicked() {
                        let from = (!edit_button.clicked_by(egui::PointerButton::Primary))
                            .then_some(edit_button.id);
                        edit = Some((slot, from));
                    }
                });
            });
            // The address, the display mode and where the camera came from, on
            // a line of their own: squeezed onto the row above, they ran into
            // each other. The mode is a button - a press moves it on to the
            // next one - and the tile carries the same mode in its corner.
            ui.horizontal(|ui| {
                ui.add_space(theme::space::S);
                let aspect = camera.aspect;
                let aspect_button = ui
                    .small_button(RichText::new(monitor_i18n::tr(aspect.label())).small().color(theme::ACCENT))
                    .on_hover_text(monitor_i18n::tr("cameras-aspect-tip"));
                self.nav.item(&aspect_button);
                self.focus_names.insert(aspect_button.id, "cameras-aspect-name");
                if aspect_button.clicked() {
                    aspect_change = Some((slot, aspect.next()));
                }
                // The address follows the connection mode, so a camera on MSE
                // reads out its MSE address. Where there is a second address,
                // the tooltip names both.
                let address = theme::truncate(&camera.masked_uri(StreamKind::Main), 26);
                let address_label = ui.label(RichText::new(address).small().monospace().color(theme::TEXT_DIM));
                if let Some(mse) = camera.masked_mse_uri(StreamKind::Main) {
                    address_label.on_hover_text(format!(
                        "rtsp: {}\nmse:  {}",
                        camera.masked_rtsp_uri(StreamKind::Main),
                        mse
                    ));
                }
                ui.label(RichText::new(monitor_i18n::tr(camera.origin.label())).small().color(theme::TEXT_DIM));
            });
            ui.add_space(theme::space::XS);
        }
        if let Some((slot, aspect)) = aspect_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.aspect = aspect;
            }
            // A display choice: nothing is reopened, the tiles just paint
            // differently from this frame on.
            dirty = true;
        }
        if let Some((slot, name, from)) = ask_remove {
            self.remove_confirm = Some(RemoveConfirm { slot, name, from });
        }
        if let Some((slot, enabled)) = toggle {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.enabled = enabled;
            }
            self.needs_sync = true;
            dirty = true;
        }
        if let Some(slot) = infer {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                if camera.apply_sub_inference() {
                    self.needs_sync = true;
                    dirty = true;
                } else {
                    self.flash(monitor_i18n::tr("toast-no-sub"), ToastKind::Error);
                }
            }
        }
        if let Some((slot, from)) = edit {
            let draft = self.config.cameras.get(slot).map(CameraDraft::from_source);
            if let Some(draft) = draft {
                self.camera_edit = Some(CameraEdit { slot, draft, from });
            }
        }
        if let Some(slot) = connect_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.advance_connection();
            }
            // The connection is part of the schedule plan, so the channel is
            // reopened with the new one on the next sync.
            self.needs_sync = true;
            dirty = true;
        }
        if let Some(slot) = stream_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.advance_stream();
            }
            // The stream a tile pulls is part of the plan in the same way, so the
            // channel re-targets on the next sync - and holds its last frame
            // until the new stream delivers one; see `ScheduleChange`.
            self.needs_sync = true;
            dirty = true;
        }

        // Rearranging begins here, and changes nothing yet: the mode works on a
        // copy of the order and the configuration is written only when it is
        // confirmed. Hidden with fewer than two cameras - there is nothing to
        // arrange. The identifier is fixed, so the button that opens the mode
        // can be given the focus back when the mode closes.
        if self.config.group_camera_count(self.config.active_group) >= 2 {
            ui.add_space(theme::space::M);
            let trigger =
                ui.push_id("xgview-reorder-trigger", |ui| ui.button(monitor_i18n::tr("cameras-reorder"))).inner;
            self.nav.item(&trigger);
            self.name(&trigger, "cameras-reorder");
            self.reorder_trigger_anchor = Some(trigger.id);
            if trigger.clicked() {
                let active = self.config.active_group;
                self.reorder = Some(
                    self.config
                        .cameras
                        .iter()
                        .filter(|camera| usize::from(camera.group) == active)
                        .map(|camera| camera.id.clone())
                        .collect(),
                );
                self.reorder_focus = true;
                self.flash(monitor_i18n::tr("toast-reorder"), ToastKind::Info);
            }
        }

        // Renaming and removing a group are the uncommon half of the selector,
        // so they are down here rather than in the strip: one field and one
        // button, for the group on the wall.
        ui.add_space(theme::space::M);
        let active = self.config.active_index();
        let default_name = dialogs::group_name(&self.config, active);
        let mut remove = false;
        let mut confirmed = None;
        let mut make_default = false;
        ui.horizontal(|ui| {
            ui.label(monitor_i18n::tr("settings-group-name"));
            let field = dialogs::text_field(
                ui,
                &mut self.nav,
                &mut self.config.groups[active].name,
                &monitor_i18n::tr("settings-group-name"),
                false,
                false,
                |text| egui::TextEdit::singleline(text).hint_text(default_name).desired_width(200.0),
            );
            if field.changed() {
                dirty = true;
            }
            if std::mem::take(&mut self.focus_group_name) {
                // Just added: the focus goes to the field, so the name is typed
                // over at once rather than hunted for at the foot of the page.
                field.request_focus();
                field.scroll_to_me(Some(Align::Center));
            }
        });
        // The group's own actions, on a row of their own: the built-in group has
        // nothing to set or to remove.
        if active != 0 {
            ui.horizontal(|ui| {
                let set_default = ui.button(monitor_i18n::tr("settings-group-default-set"));
                self.nav.item(&set_default);
                if set_default.clicked() {
                    make_default = true;
                }
                let button = ui.button(monitor_i18n::tr("settings-group-remove"));
                self.nav.item(&button);
                if button.clicked() {
                    remove = true;
                }
            });
        }
        if make_default {
            // The group takes the built-in slot and stays on the wall, so the
            // view the scheduler holds is still its own; only the names moved.
            self.group_remove = None;
            if self.config.set_default_group(active) {
                dirty = true;
            }
        }
        if remove {
            self.group_remove = Some(active);
        }
        if let Some(index) = self.group_remove.filter(|index| *index == active) {
            // The question is asked where the button was, rather than in a
            // window: the group is about to go with its cameras, and a line of
            // text says so without hiding what is being lost.
            let name = dialogs::group_name(&self.config, index);
            let count = self.config.group_camera_count(index);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(monitor_i18n::tr_args(
                        "settings-group-remove-confirm",
                        &[("name", name.into()), ("count", count.into())],
                    ))
                    .small()
                    .color(theme::WARN),
                );
                let yes = ui.button(monitor_i18n::tr("settings-group-remove-yes"));
                self.nav.item(&yes);
                let no = ui.button(monitor_i18n::tr("settings-group-remove-no"));
                self.nav.item(&no);
                if yes.clicked() {
                    confirmed = Some(index);
                } else if no.clicked() {
                    self.group_remove = None;
                }
            });
        } else if self.group_remove.is_some() {
            // The page moved to another group while the question stood.
            self.group_remove = None;
        }
        if let Some(index) = confirmed {
            self.group_remove = None;
            // The view the scheduler holds belongs to the group about to go, so
            // it is parked before the list moves under it; the group the wall
            // falls back to then loads its own.
            self.park_group_view();
            if self.config.remove_group(index) {
                self.load_group_view();
                dirty = true;
            }
        }

        dirty
    }

    /// The camera list in its reorder mode.
    ///
    /// One row per camera, with up and down buttons where the ordinary list
    /// keeps its edit and remove buttons, and nothing else: the controls that
    /// change a camera are hidden while the list is being arranged. The order
    /// is the preview held in [`Self::reorder`]; it is written to the
    /// configuration only on Confirm, so the wall is untouched while the viewer
    /// arranges, and Back drops the whole thing.
    fn cameras_reorder(&mut self, ui: &mut egui::Ui, order: &[String]) {
        let last = order.len().saturating_sub(1);
        let mut moved: Option<(usize, i32, Id)> = None;
        let mut anchor = None;
        // The list is the active group's, so its labels come from that group:
        // the identifier alone would find the entry in whichever group comes
        // first in the file.
        let active = self.config.active_group;
        for (slot, id) in order.iter().enumerate() {
            let label = self
                .config
                .find_camera_in_group(id, active)
                .map(|camera| camera.short_label(24))
                .unwrap_or_default();
            let first = slot == 0;
            let bottom = slot == last;
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Drawn right to left so the pair reads up, down; registered
                    // up first, so the walk reaches it first.
                    //
                    // The identifiers are the camera's, not the row's place.
                    // egui's own are seeded from the sibling position and change
                    // when the row moves, which would drop the focus on the row
                    // the viewer was working on and hand it to the list anchor
                    // at the top. Under a fixed id the focus follows the camera
                    // through the list. See `icons::button_at`.
                    let down = icons::button_at(
                        ui,
                        Id::new(id).with("down"),
                        Icon::Down,
                        false,
                        bottom,
                        &monitor_i18n::tr("cameras-move-down"),
                        ROW_ICON,
                    );
                    let up = icons::button_at(
                        ui,
                        Id::new(id).with("up"),
                        Icon::Up,
                        false,
                        first,
                        &monitor_i18n::tr("cameras-move-up"),
                        ROW_ICON,
                    );
                    // Only the arrows with somewhere to go join the walk: a
                    // muted one is inert - see `icons::button_at` - and must not
                    // be reachable either, or the viewer would land on a button
                    // that does nothing. They are still drawn, so the row keeps
                    // its shape.
                    //
                    // egui gives focus from the keyboard, not from the pointer:
                    // a press hands the focus of whatever held it back to
                    // nothing, and the panel anchor would then take it to the
                    // first row while the pointer is still down on this button.
                    // The press takes the focus here instead, as the tab strip
                    // does after a click.
                    if !first {
                        self.nav.item(&up);
                        self.name(&up, "cameras-move-camera-up");
                        if up.is_pointer_button_down_on() {
                            up.request_focus();
                        }
                    }
                    if !bottom {
                        self.nav.item(&down);
                        self.name(&down, "cameras-move-camera-down");
                        if down.is_pointer_button_down_on() {
                            down.request_focus();
                        }
                    }
                    if first {
                        // The mode opens on the down arrow of the first row: the
                        // up arrow beside it is muted - the first camera has
                        // nowhere to go up - and a mode that opens on a dead
                        // button reads as a mode that does not work.
                        anchor = Some(down.id);
                    }
                    if up.clicked() && !first {
                        moved = Some((slot, -1, up.id));
                    }
                    if down.clicked() && !bottom {
                        moved = Some((slot, 1, down.id));
                    }
                });
            });
            ui.add_space(theme::space::XS);
        }
        self.reorder_anchor = anchor;
        let Some((slot, delta, pressed)) = moved else {
            return;
        };
        let to = (slot as i32 + delta).clamp(0, last as i32) as usize;
        let Some(order) = self.reorder.as_mut() else {
            return;
        };
        order.swap(slot, to);
        let camera = order[to].clone();
        // The button that was just pressed may have run out of room: the down
        // arrow at the foot of the list, the up arrow at the top are drawn
        // muted and do nothing. The focus goes to the arrow that still has
        // somewhere to go, so a run of presses keeps moving the camera instead
        // of landing on a dead button. Anywhere else it stays where it was.
        let follow = if to == last && delta > 0 {
            Id::new(&camera).with("up")
        } else if to == 0 && delta < 0 {
            Id::new(&camera).with("down")
        } else {
            pressed
        };
        // The focus went with the camera - to the other arrow when the pressed
        // one is spent - and the row it is on has to be brought into view. The
        // reveal is armed here and made next frame, as the button is drawn
        // again. See `reveal_next`.
        self.reveal_next = Some(follow);
        ui.ctx().memory_mut(|memory| memory.request_focus(follow));
    }

    /// Writes the arranged order back and leaves the mode. `true` when the
    /// order actually changed, so that the wall is only retargeted then.
    fn apply_reorder(&mut self) -> bool {
        let Some(order) = self.reorder.take() else {
            return false;
        };
        self.reorder_focus = false;
        self.restore_focus = self.reorder_trigger_anchor;
        if !self.config.apply_order(&order) {
            return false;
        }
        // The order is the channel order: the plan follows it on the next sync.
        self.needs_sync = true;
        self.mark_dirty();
        true
    }

    /// Leaves the reorder mode without writing anything. The configuration was
    /// never touched, so the list simply goes back to what it was.
    fn cancel_reorder(&mut self) {
        if self.reorder.take().is_none() {
            return;
        }
        self.reorder_focus = false;
        self.restore_focus = self.reorder_trigger_anchor;
    }

    /// The window one camera of the settings panel is edited in.
    ///
    /// It lays out the same form as the "Manual entry" tab - see
    /// [`dialogs::camera_fields`] - and writes back under the identifier of the
    /// entry it is editing, so a changed url repoints the camera instead of
    /// adding a second one. Returns `true` when the configuration changed.
    fn camera_edit_window(&mut self, ctx: &egui::Context) -> bool {
        // Taken for the frame, so the form and the navigation layer can be
        // borrowed apart; put back unless the window was closed or saved.
        let Some(CameraEdit { slot, mut draft, from }) = self.camera_edit.take() else {
            return false;
        };
        let mut save = false;
        let mut cancel = false;
        let mut detach = false;
        // The form edits the camera where it lives, so the heading names the
        // group it is in - when there is more than one to tell apart.
        let group = self
            .config
            .cameras
            .get(slot)
            .map(|camera| usize::from(camera.group))
            .unwrap_or(0);
        let title = dialogs::group_heading(
            &self.config,
            group,
            "camera-edit-title",
            "camera-edit-title-group",
        );
        // The form reads the entry to decide what it draws - greyed addresses
        // for an imported camera, and no "infer sub" offer on a restream. It is
        // read afresh each frame, so pressing Detach unlocks it on the spot.
        let camera = self.config.cameras.get(slot);

        // A modal: centred, and its backdrop swallows the presses that would
        // otherwise reach the panel it was opened from. It is left only by its
        // own buttons (or Back) - a press on the backdrop does nothing.
        egui::Modal::new(Id::new("xgview-edit-camera")).show(ctx, |ui| {
            ui.set_width(560.0);
            dialogs::title_bar(ui, &title, false, "");
            self.nav.open("edit-body");
            let fields = dialogs::camera_fields(ui, &mut self.nav, "edit-camera", &mut draft, camera);
            // The dialog opens on the first control of the form.
            self.camera_edit_anchor = fields.first;
            if fields.detach {
                detach = true;
            }
            if fields.inferred {
                ui.label(RichText::new(monitor_i18n::tr("camera-edit-sub-derived")).small().color(theme::LIVE));
            }
            ui.add_space(theme::space::M);
            ui.horizontal(|ui| {
                let save_label = monitor_i18n::tr("save");
                let cancel_label = monitor_i18n::tr("cancel");
                ui.add_space(Self::center_offset(ui, &[&save_label, &cancel_label]));
                if self.nav.tracked(ui.button(save_label)).clicked() {
                    save = true;
                }
                if self.nav.tracked(ui.button(cancel_label)).clicked() {
                    cancel = true;
                }
            });
            self.nav.close();
        });

        if detach {
            // The camera leaves the import's hands: its MSE address and login go
            // with the mark, so its addresses can be edited by hand without one
            // falling out of step. The window stays open, now unlocked.
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.detach();
            }
            self.needs_sync = true;
            self.camera_edit = Some(CameraEdit { slot, draft, from });
            return true;
        }

        if save {
            match draft.build() {
                Ok(source) => {
                    // Only what the form shows is written back. The entry keeps
                    // its identifier, its origin, its transport and everything
                    // else the form has no field for - the display aspect among
                    // them - so an edit to a name or a url cannot reset them.
                    match self.config.cameras.get_mut(slot) {
                        Some(camera) => camera.apply_edit(&source),
                        None => {
                            self.config.upsert_camera(source);
                        }
                    }
                    self.needs_sync = true;
                    self.camera_edit_anchor = None;
                    // The focus goes back to the button the window was opened
                    // from, not to the first control of the panel - and only
                    // when that button was reached with the keyboard or a
                    // remote. See `CameraEdit::from`.
                    self.restore_focus = from;
                    return true;
                }
                Err(error) => self.flash(error, ToastKind::Error),
            }
        }
        if !cancel {
            self.camera_edit = Some(CameraEdit { slot, draft, from });
        } else {
            self.camera_edit_anchor = None;
            self.restore_focus = from;
        }
        false
    }

    /// The window the settings panel asks about removing a camera in.
    ///
    /// A window rather than a question drawn into the row: a row that turns
    /// into its own question can be walked past and left that way, while a
    /// window has one way out. Returns `true` when the configuration changed.
    fn remove_confirm_window(&mut self, ctx: &egui::Context) -> bool {
        // Taken for the frame, like the edit window, so the layer and the
        // question can be borrowed apart.
        let Some(RemoveConfirm { slot, name, from }) = self.remove_confirm.take() else {
            return false;
        };
        let mut remove = false;
        let mut cancel = false;

        // The heading names the group the camera is leaving when there is more
        // than one to tell apart, as the edit window's names the group it is in:
        // two groups can hold a camera of the same name.
        let group = self
            .config
            .cameras
            .get(slot)
            .map(|camera| usize::from(camera.group))
            .unwrap_or(0);
        let title = dialogs::remove_title(&self.config, group, &name);

        // A modal, like the edit window: centred, and nothing behind it answers.
        // Its own buttons (or Back) are the only way out.
        egui::Modal::new(Id::new("xgview-remove-camera")).show(ctx, |ui| {
            // The question is its own column of the navigation layer, as the
            // edit window is: without it the two answers are not registered at
            // all, and the arrows are left to egui's geometric walk - which the
            // panel behind then takes away from them.
            self.nav.open("confirm-body");
            ui.set_width(380.0);
            dialogs::title_bar(ui, &title, false, "");
            ui.label(
                RichText::new(monitor_i18n::tr("remove-hint"))
                    .small()
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::space::M);
            ui.horizontal(|ui| {
                let keep_label = monitor_i18n::tr("action-keep");
                let remove_label = monitor_i18n::tr("action-remove");
                ui.add_space(Self::center_offset(ui, &[&keep_label, &remove_label]));
                let keep_button = self.nav.tracked(ui.button(keep_label));
                // The dialog opens on the answer that changes nothing.
                self.remove_confirm_anchor = Some(keep_button.id);
                if keep_button.clicked() {
                    cancel = true;
                }
                if self.nav.tracked(ui.button(RichText::new(remove_label).color(theme::ERROR))).clicked() {
                    remove = true;
                }
            });
            self.nav.close();
        });

        if remove {
            if slot < self.config.cameras.len() {
                self.config.cameras.remove(slot);
                // The row is gone, so there is no button to hand the focus
                // back to; the panel anchor picks it up.
                self.needs_sync = true;
                self.remove_confirm_anchor = None;
                return true;
            }
        }
        if !cancel {
            self.remove_confirm = Some(RemoveConfirm { slot, name, from });
        } else {
            self.remove_confirm_anchor = None;
            self.restore_focus = from;
        }
        false
    }

    /// The reboot confirmation window.
    ///
    /// A modal, like the remove window: centred, and nothing behind it
    /// answers. Its own buttons (or Back) are the only way out, and the dialog
    /// opens on the answer that changes nothing.
    fn system_confirm_window(&mut self, ctx: &egui::Context) {
        let Some(SystemConfirm { from }) = self.system_confirm.take() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;

        egui::Modal::new(Id::new("xgview-system-confirm")).show(ctx, |ui| {
            self.nav.open("confirm-body");
            ui.set_width(380.0);
            dialogs::title_bar(ui, &monitor_i18n::tr("reboot-confirm-title"), false, "");
            ui.label(
                RichText::new(monitor_i18n::tr("reboot-confirm-hint"))
                    .small()
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::space::M);
            ui.horizontal(|ui| {
                let cancel_label = monitor_i18n::tr("action-cancel");
                let reboot_label = monitor_i18n::tr("action-reboot");
                ui.add_space(Self::center_offset(ui, &[&cancel_label, &reboot_label]));
                let cancel_button = self.nav.tracked(ui.button(cancel_label));
                // The dialog opens on the answer that changes nothing.
                self.system_confirm_anchor = Some(cancel_button.id);
                if cancel_button.clicked() {
                    cancel = true;
                }
                if self
                    .nav
                    .tracked(ui.button(RichText::new(reboot_label).color(theme::ERROR)))
                    .clicked()
                {
                    confirm = true;
                }
            });
            self.nav.close();
        });

        if confirm {
            self.system_confirm_anchor = None;
            // Save first: a reboot ends the process, and the debounced save
            // would otherwise never happen.
            self.save_now();
            match reboot::reboot() {
                Ok(()) => self.quit(ctx),
                Err(err) => {
                    self.flash(
                        monitor_i18n::tr_args(
                            "toast-reboot-failed",
                            &[("error", err.to_string().into())],
                        ),
                        ToastKind::Error,
                    );
                    // The machine was not restarted, so the viewer is still
                    // here — give the button its focus back.
                    self.restore_focus = from;
                }
            }
            return;
        }
        if !cancel {
            self.system_confirm = Some(SystemConfirm { from });
        } else {
            self.system_confirm_anchor = None;
            self.restore_focus = from;
        }
    }

    /// The passphrase prompt: what seals a configuration for the trip, and what
    /// opens one that was sealed. Returns `true` when the configuration changed.
    ///
    /// Sealing asks twice, so a mistyped passphrase cannot put the configuration
    /// out of reach before it has even left. Opening asks once and, when the
    /// passphrase does not take, leaves the prompt up with that said - the
    /// document is kept here, so a second try costs nothing but the typing.
    fn passphrase_window(&mut self, ctx: &egui::Context) -> bool {
        // Taken for the frame, like the edit and remove windows, so the fields
        // and the navigation layer can be borrowed apart.
        let Some(mut prompt) = self.passphrase.take() else {
            return false;
        };
        let mut confirm = false;
        let mut leave = false;

        // A modal, like the other two: centred, and nothing behind it answers.
        // Its own buttons (or Back) are the only way out.
        egui::Modal::new(Id::new("xgview-passphrase")).show(ctx, |ui| {
            self.nav.open("passphrase-body");
            ui.set_width(440.0);
            dialogs::title_bar(ui, &prompt.title(), false, "");
            ui.label(RichText::new(prompt.hint()).small().color(theme::TEXT_DIM));
            ui.add_space(theme::space::S);

            // Read off once: the field is drawn with the entry hidden unless the
            // viewer asked to see it.
            let reveal = prompt.reveal;
            ui.label(monitor_i18n::tr("prompt-passphrase"));
            let first = dialogs::text_field(
                ui,
                &mut self.nav,
                &mut prompt.first,
                &monitor_i18n::tr("prompt-passphrase"),
                !reveal,
                false,
                |s| egui::TextEdit::singleline(s).password(!reveal).desired_width(f32::INFINITY),
            );
            // The prompt opens on this field.
            self.passphrase_anchor = Some(first.id);

            if prompt.job == SealJob::Export {
                ui.add_space(theme::space::XS);
                ui.label(monitor_i18n::tr("prompt-passphrase-again"));
                dialogs::text_field(
                    ui,
                    &mut self.nav,
                    &mut prompt.again,
                    &monitor_i18n::tr("prompt-passphrase-again"),
                    !reveal,
                    false,
                    |s| egui::TextEdit::singleline(s).password(!reveal).desired_width(f32::INFINITY),
                );
            }

            ui.add_space(theme::space::XS);
            self.nav.tracked(ui.checkbox(&mut prompt.reveal, monitor_i18n::tr("prompt-reveal")));

            if let Some(error) = &prompt.error {
                ui.add_space(theme::space::XS);
                ui.label(RichText::new(error).color(theme::ERROR));
            }

            ui.add_space(theme::space::M);
            if prompt.working {
                // The passphrase is being stretched away from this thread: the
                // window stays up and says so, and its two buttons are gone,
                // because there is nothing to press until the answer is in.
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add_space(theme::space::S);
                    ui.label(monitor_i18n::tr("prompt-open-working"));
                });
            } else {
                let action = prompt.action();
                let cancel_label = monitor_i18n::tr("cancel");
                ui.horizontal(|ui| {
                    ui.add_space(Self::center_offset(ui, &[&action, &cancel_label]));
                    if self.nav.tracked(ui.button(action)).clicked() {
                        confirm = true;
                    }
                    if self.nav.tracked(ui.button(cancel_label)).clicked() {
                        leave = true;
                    }
                });
            }
            self.nav.close();
        });

        if confirm {
            match prompt.job {
                SealJob::Export => {
                    if prompt.first.is_empty() {
                        prompt.error = Some(monitor_i18n::tr("prompt-passphrase-empty"));
                    } else if prompt.first != prompt.again {
                        prompt.error = Some(monitor_i18n::tr("prompt-passphrase-mismatch"));
                    } else {
                        // Asked for here, and drawn from the next frame on by
                        // `poll_file_dialog`.
                        self.ask_for_file(FileJob::Export(Some(prompt.first.clone())));
                        leave = true;
                    }
                }
                SealJob::Import => {
                    // Handed to a thread of its own, because stretching the
                    // passphrase is the better part of a second of arithmetic:
                    // what comes of it is taken by `poll_sealing`, and the
                    // prompt waits for it with its buttons gone.
                    let data = prompt.sealed.clone().unwrap_or_default();
                    self.spawn_open(data, prompt.first.clone(), prompt.origin.clone());
                    prompt.working = true;
                }
            }
        }

        if leave {
            self.passphrase_anchor = None;
        } else {
            self.passphrase = Some(prompt);
        }
        // Nothing here changes the configuration any more: what an opened file
        // becomes is settled by `poll_sealing`, on the frame the answer arrives.
        false
    }

    /// What the program is, where its configuration lives, and the export /
    /// import of that configuration.
    fn settings_about(&mut self, ui: &mut egui::Ui) {
        // Version and authors come from the manifest, declared once in
        // `[workspace.package]` and inherited by every crate, so the about
        // panel cannot drift from Cargo.toml. The description is translated
        // (`about-description`), with the English pack matching the manifest.
        // The name is the display name, not the crate name (this crate is
        // `monitor_gui`).
        ui.label(monitor_i18n::tr_args(
            "about-title",
            &[
                ("app", monitor_core::APP_DISPLAY_NAME.into()),
                ("description", monitor_i18n::tr("about-description").into()),
            ],
        ));
        // The author is a mailto link, with the version and the copyright years
        // either side of it so the line still reads as one sentence. The years
        // are the ones the binary was built in; see `copyright_years`.
        let (author, email) = monitor_core::author();
        ui.horizontal(|ui| {
            ui.label(monitor_i18n::tr_args("about-version-by", &[("version", env!("CARGO_PKG_VERSION").into())]));
            if email.is_empty() {
                ui.label(author);
            } else {
                // `Ui::link` rather than `hyperlink_to`: the click is opened
                // through the shell here, so a `mailto:` reaches the mail
                // client. See `open_link`.
                let link = ui.link(format!("{author} <{email}>"));
                self.nav.item(&link);
                self.name(&link, "about-author-name");
                if link.clicked() {
                    self.open_link(ui.ctx(), &monitor_core::author_mailto());
                }
            }
            ui.label(monitor_i18n::tr_args("about-copyright", &[("years", monitor_core::copyright_years().into())]));
        });
        // Where the project lives, right under the version line. A link like the
        // author's, but a wrapping label rather than `Ui::link`: the settings
        // panel is narrow and the URL is longer than one line there.
        ui.label(monitor_i18n::tr("about-repository"));
        let repository = ui.add(
            egui::Label::new(RichText::new(REPOSITORY_URL).color(theme::ACCENT))
                .wrap()
                .sense(egui::Sense::click()),
        );
        let repository = self.nav.tracked(repository);
        self.name(&repository, "about-repository");
        if repository.clicked() {
            self.open_link(ui.ctx(), REPOSITORY_URL);
        }
        #[cfg(juanz)]
        {
            let mut link: Option<String> = None;
            let juanz = crate::juanz::button(ui, &mut self.nav, &mut |url| link = Some(url.to_owned()));
            self.name(&juanz, "about-donate");
            if let Some(url) = link {
                self.open_link(ui.ctx(), &url);
            }
        }
        let decoder_mode = monitor_i18n::tr(if self.hardware_decoder {
            "about-decoder-hardware"
        } else {
            "about-decoder-software"
        });
        ui.label(monitor_i18n::tr_args(
            "about-decoder",
            &[("backend", self.decoder.into()), ("mode", decoder_mode.into())],
        ));
        ui.label(monitor_i18n::tr_args(
            "about-config",
            &[("path", self.config_path.display().to_string().into())],
        ));

        // The export / import pair. The export is sealed with a passphrase by
        // default, which the prompt asks for before the file is written;
        // unticking the box beside it writes the JSON in clear instead. The
        // desktop opens a file dialog for each, while Android writes into the
        // public Download/xgview folder and opens the system's document picker,
        // and the hint says so.
        ui.add_space(theme::space::M);
        ui.horizontal(|ui| {
            let export = ui.button(monitor_i18n::tr("about-export"));
            self.nav.item(&export);
            self.name(&export, "about-export-name");
            if export.clicked() {
                // The passphrase first: nothing is written until it has been
                // typed twice, and the file dialog that follows is opened from
                // the prompt. With the box unticked there is none to ask for,
                // and the dialog opens straight away.
                //
                // The button is remembered here, because by the time that dialog
                // is up the focus has been through the prompt on the way.
                //
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                if !export.clicked_by(egui::PointerButton::Primary) {
                    self.file_dialog_anchor = Some(export.id);
                }
                if self.encrypt_export {
                    self.passphrase = Some(Passphrase::for_export());
                } else {
                    self.ask_for_file(FileJob::Export(None));
                }
            }
            let encrypt = ui.checkbox(&mut self.encrypt_export, monitor_i18n::tr("about-encrypt-export"));
            self.nav.item(&encrypt);
            self.name(&encrypt, "about-encrypt-export-name");
        });
        ui.horizontal(|ui| {
            let import = ui.button(monitor_i18n::tr("about-import"));
            self.nav.item(&import);
            self.name(&import, "about-import-name");
            if import.clicked() {
                // A sealed document opens the passphrase prompt instead, which
                // is drawn on the wall. See `ask_for_file`.
                //
                // Remembered for a keyboard or remote press only, as the export
                // is: a pointer leaves the focus where it found it.
                if !import.clicked_by(egui::PointerButton::Primary) {
                    self.file_dialog_anchor = Some(import.id);
                }
                self.ask_for_file(FileJob::Import);
            }
            let only = ui.checkbox(&mut self.import_cameras_only, monitor_i18n::tr("about-only-cameras"));
            self.nav.item(&only);
            self.name(&only, "about-only-cameras-name");
        });
        ui.label(RichText::new(self.import_hint()).small().color(theme::TEXT_DIM));

        // The machine itself, under everything else the page says - the one
        // place a viewer looks to see what this installation is running on. The
        // numbers are re-read at most once a second and only while the page is
        // open; the page asks for the repaint that keeps them moving.
        ui.add_space(theme::space::M);
        ui.separator();
        self.vitals.refresh(Duration::from_secs(1));
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        match self.vitals.memory_percent() {
            Some(percent) if self.vitals.total_memory() > 0 => {
                let megabytes = |bytes: u64| (bytes / (1024 * 1024)) as usize;
                ui.label(monitor_i18n::tr_args(
                    "about-memory",
                    &[
                        ("used", megabytes(self.vitals.used_memory()).into()),
                        ("total", megabytes(self.vitals.total_memory()).into()),
                        ("percent", (percent as usize).into()),
                    ],
                ));
            }
            _ => {
                ui.label(monitor_i18n::tr("about-memory-unknown"));
            }
        }
        // Android does not let an application read the processor counters, so
        // the line is left out there rather than drawn as an unknown.
        #[cfg(not(target_os = "android"))]
        match self.vitals.cpu_percent() {
            Some(percent) => {
                ui.label(monitor_i18n::tr_args(
                    "about-cpu",
                    &[("percent", (percent.round() as usize).into())],
                ));
            }
            None => {
                ui.label(monitor_i18n::tr("about-cpu-unknown"));
            }
        }
        ui.label(monitor_i18n::tr_args("about-system", &[("os", self.vitals.os().into())]));
        ui.label(monitor_i18n::tr_args(
            "about-uptime",
            &[("duration", uptime_text(self.started.elapsed()).into())],
        ));
    }

    /// What the export / import pair does. The desktop picks files with the
    /// system's dialogs; Android has the fixed export folder and the system's
    /// document picker, and says so. See [`Self::ask_for_file`].
    #[cfg(not(target_os = "android"))]
    fn import_hint(&self) -> String {
        monitor_i18n::tr("about-import-hint")
    }

    /// What the export / import pair does, where the export has a fixed folder
    /// and the import opens the system's document picker.
    #[cfg(target_os = "android")]
    fn import_hint(&self) -> String {
        monitor_i18n::tr("about-import-hint-android")
    }

    /// Writes the configuration out sealed with `passphrase`, to a file the
    /// viewer has already chosen.
    ///
    /// Choosing it is the dialog [`Self::ask_for_file`] opens on another thread;
    /// this is what happens once its answer is in. Sealing is handed to a thread
    /// of its own and the file is written when the text is in - see
    /// [`Self::poll_sealing`] - while the JSON, which costs nothing to produce,
    /// goes out from here.
    #[cfg(not(target_os = "android"))]
    fn export_to(&mut self, path: &Path, passphrase: Option<&str>) {
        match passphrase {
            Some(passphrase) => {
                self.spawn_seal(passphrase.to_owned(), ExportTarget::File(path.to_path_buf()))
            }
            None => match self.config.to_json() {
                Ok(written) => self.write_export_to(path, &written),
                Err(error) => self.flash(
                    monitor_i18n::tr_args("toast-export-failed", &[("error", error.to_string().into())]),
                    ToastKind::Error,
                ),
            },
        }
    }

    /// Writes what an export came to into the file the dialog chose, and says
    /// where it went.
    #[cfg(not(target_os = "android"))]
    fn write_export_to(&mut self, path: &Path, written: &str) {
        match std::fs::write(path, written.as_bytes()) {
            Ok(()) => self.flash(
                monitor_i18n::tr_args(
                    "toast-config-exported",
                    &[("path", path.display().to_string().into())],
                ),
                ToastKind::Info,
            ),
            Err(error) => self.flash(monitor_i18n::tr_args("toast-export-failed", &[("error", error.to_string().into())]), ToastKind::Error),
        }
    }

    /// Writes the configuration out into the public `Download/xgview` folder:
    /// sealed with `passphrase`, or the JSON in clear when there is none.
    ///
    /// The sealing itself is handed to a thread of its own, and the write
    /// follows it there; see [`Self::poll_sealing`].
    #[cfg(target_os = "android")]
    fn export_config(&mut self, passphrase: Option<&str>) {
        // The name says which of the two the file is, as it does for the
        // desktop's dialog.
        let name = if passphrase.is_some() {
            monitor_core::EXPORT_FILE_NAME
        } else {
            monitor_core::CONFIG_FILE_NAME
        };
        match passphrase {
            Some(passphrase) => {
                self.spawn_seal(passphrase.to_owned(), ExportTarget::Download(name))
            }
            None => match self.config.to_json() {
                Ok(written) => self.write_download_export(name, &written),
                Err(error) => self.flash(
                    monitor_i18n::tr_args("toast-export-failed", &[("error", error.to_string().into())]),
                    ToastKind::Error,
                ),
            },
        }
    }

    /// Writes what an export came to into the public `Download/xgview` folder
    /// under `name`.
    ///
    /// The write is the activity's - `MediaStore` from Android 10 on, a plain
    /// file into the public Downloads folder before that - because a
    /// `NativeActivity` has no storage of its own the viewer can reach. See
    /// `MainActivity.writeDownloadFile`.
    #[cfg(target_os = "android")]
    fn write_download_export(&mut self, name: &str, written: &str) {
        match crate::android::write_download(name, written) {
            Ok(crate::android::ExportOutcome::Written(path)) => self.flash(
                monitor_i18n::tr_args("toast-config-exported", &[("path", path.into())]),
                ToastKind::Info,
            ),
            // Android 9 only: the storage permission has just been asked for,
            // so the export is one press away rather than failed.
            Ok(crate::android::ExportOutcome::NeedsStoragePermission) => {
                self.flash(monitor_i18n::tr("toast-export-needs-storage"), ToastKind::Error)
            }
            Err(error) => self.flash(monitor_i18n::tr_args("toast-export-failed", &[("error", error.into())]), ToastKind::Error),
        }
    }

    /// Reads a configuration file the viewer has chosen into the running one.
    ///
    /// A sealed document asks for its passphrase before anything is touched, and
    /// the prompt takes it from there; an older export - JSON, in clear - is
    /// imported on the spot, the way it always was. The file was chosen by the
    /// dialog [`Self::ask_for_file`] opens on another thread.
    #[cfg(not(target_os = "android"))]
    fn import_from(&mut self, path: PathBuf) {
        let data = match std::fs::read_to_string(&path) {
            Ok(data) => data,
            Err(error) => {
                self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.to_string().into())]), ToastKind::Error);
                return;
            }
        };
        if monitor_core::sealed::is_sealed(&data) {
            self.passphrase = Some(Passphrase::for_import(data, Some(path)));
            return;
        }
        match self.apply_import(&data) {
            Ok(count) => self.say_imported(count, Some(path)),
            Err(error) => self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.into())]), ToastKind::Error),
        }
    }

    /// Opens the system's document picker.
    ///
    /// Nothing is imported here: the picker is another activity, and its answer
    /// arrives frames later, where [`Self::poll_picked_config`] takes it. A
    /// picker the viewer backs out of is not a failure and says nothing.
    #[cfg(target_os = "android")]
    fn import_config(&mut self) {
        crate::android::pick_config_file();
    }

    /// Applies a configuration that was read from somewhere - a file the
    /// desktop picked, a document the Android picker returned, a sealed one the
    /// prompt has just opened - and reports how many cameras came with it, for
    /// the toast the caller puts up.
    fn apply_import(&mut self, data: &str) -> Result<usize, String> {
        let imported = AppConfig::from_json(data).map_err(|error| error.to_string())?;
        Ok(self.apply_import_config(imported))
    }

    /// Applies a configuration already read, and reports how many cameras came
    /// with it.
    ///
    /// The cameras replace this viewer's, and the result is written back over
    /// this installation's configuration on the next save. With the "only the
    /// cameras" box ticked nothing else is taken from it.
    fn apply_import_config(&mut self, imported: AppConfig) -> usize {
        let count = imported.cameras.len();
        self.back_up_configuration();
        if self.import_cameras_only {
            // The cameras come with the groups they live in. An entry records
            // the group it belongs to, so bringing the cameras without the
            // groups they name would leave the ones from the other groups in
            // groups this installation does not have - and never drawn.
            // Everything that is not the camera arrangement - the display, the
            // sleep and blackout settings, the scan ranges - stays this
            // viewer's own.
            self.config.cameras = imported.cameras;
            self.config.groups = imported.groups;
            self.config.active_group = imported.active_group;
            self.load_group_view();
        } else {
            // Everything is taken from the document, but the path this
            // installation reads and writes stays its own - it is a field of
            // the app, not of the configuration. The wall follows the imported
            // layout, page and focus, so the scheduler is rebuilt the way
            // start-up builds it.
            self.config = imported;
            let (layout, page, focus) = {
                let view = self.config.group();
                (view.layout, view.page, view.focus)
            };
            self.scheduler = Scheduler::new(layout, page, focus, self.config.enabled_count());
        }
        self.config.normalize();
        self.needs_sync = true;
        self.mark_dirty();
        count
    }

    /// Keeps the configuration an import is about to replace, beside it.
    ///
    /// Importing is the one thing here that can throw work away - everything
    /// else edits the configuration already loaded - so the file it was read
    /// from is copied aside first. A copy that cannot be written is reported and
    /// the import goes ahead anyway: not being able to keep the old file is no
    /// reason to refuse the new one, but it is something to hear about.
    fn back_up_configuration(&mut self) {
        let path = self.config_path.with_extension("json.bak");
        match self.config.save(&path) {
            Ok(()) => tracing::info!(
                target: "xgview::config",
                path = %path.display(),
                "kept the configuration this import replaces"
            ),
            Err(error) => self.flash(
                monitor_i18n::tr_args(
                    "toast-backup-failed",
                    &[("error", error.to_string().into())],
                ),
                ToastKind::Error,
            ),
        }
    }

    /// Takes the document the system picker returned, if one has arrived.
    ///
    /// A sealed document asks for its passphrase before anything is touched; an
    /// older export, JSON in clear, is imported on the spot.
    #[cfg(target_os = "android")]
    fn poll_picked_config(&mut self) {
        match crate::android::take_picked_config() {
            None => {}
            Some(crate::android::Picked::Cancelled) => {}
            Some(crate::android::Picked::Config(text)) => {
                if monitor_core::sealed::is_sealed(&text) {
                    self.passphrase = Some(Passphrase::for_import(text, None));
                    return;
                }
                match self.apply_import(&text) {
                    Ok(count) => self.say_imported(count, None),
                    Err(error) => self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.into())]), ToastKind::Error),
                }
            }
            Some(crate::android::Picked::Failed(error)) => {
                self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.into())]), ToastKind::Error)
            }
        }
    }

    /// Hands a sealed document and a passphrase to a thread of its own.
    ///
    /// Nothing waits for it: stretching a passphrase is hundreds of thousands of
    /// rounds, and the render loop has a wall to draw. What comes back is taken
    /// by [`Self::poll_sealing`] on a later frame.
    fn spawn_open(&mut self, data: String, passphrase: String, origin: Option<PathBuf>) {
        let (sender, result) = unbounded();
        std::thread::spawn(move || {
            // A wrong passphrase and a damaged document are one answer here, as
            // they are inside `sealed::open`: neither says anything the other
            // does not, and the prompt has one thing to say about both.
            let _ = sender.send(monitor_core::sealed::open(&data, &passphrase).map_err(|_| ()));
        });
        self.sealing = Some(Sealing::Open { result, origin });
        self.mark_dirty();
    }

    /// Hands an export's passphrase to a thread of its own, with where the text
    /// it comes to is to be written.
    fn spawn_seal(&mut self, passphrase: String, target: ExportTarget) {
        let (sender, result) = unbounded();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let _ = sender.send(
                monitor_core::sealed::seal(&config, &passphrase).map_err(|error| error.to_string()),
            );
        });
        self.sealing = Some(Sealing::Export { result, target });
        self.mark_dirty();
    }

    /// Takes what a sealing thread has come to, if it has.
    ///
    /// Run once a frame with the other answers the background jobs leave behind.
    /// The thread is left to finish by itself when the job is over - what it
    /// holds is a document and a passphrase, and there is nothing to gain by
    /// stopping it half way.
    fn poll_sealing(&mut self) {
        use crossbeam_channel::TryRecvError;

        let Some(sealing) = self.sealing.take() else {
            return;
        };
        match sealing {
            Sealing::Open { result, origin } => match result.try_recv() {
                Ok(Ok(config)) => {
                    let count = self.apply_import_config(config);
                    self.say_imported(count, origin);
                    self.passphrase = None;
                    self.passphrase_anchor = None;
                    self.restore_focus = self.file_dialog_anchor.take().or(self.restore_focus);
                    self.mark_dirty();
                }
                // A wrong passphrase and a damaged document read the same way,
                // and it is the passphrase that is worth another try: the entry
                // stays, the window stays with this said under it, and a second
                // attempt is a second field's worth of typing.
                Ok(Err(())) | Err(TryRecvError::Disconnected) => {
                    if let Some(prompt) = self.passphrase.as_mut() {
                        prompt.working = false;
                        prompt.error = Some(monitor_i18n::tr("prompt-passphrase-wrong"));
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.sealing = Some(Sealing::Open { result, origin });
                }
            },
            Sealing::Export { result, target } => match result.try_recv() {
                Ok(Ok(written)) => match target {
                    #[cfg(not(target_os = "android"))]
                    ExportTarget::File(path) => self.write_export_to(&path, &written),
                    #[cfg(target_os = "android")]
                    ExportTarget::Download(name) => self.write_download_export(name, &written),
                },
                Ok(Err(error)) => self.flash(
                    monitor_i18n::tr_args("toast-export-failed", &[("error", error.into())]),
                    ToastKind::Error,
                ),
                // The thread is gone without an answer, which the seal itself
                // has no way of doing. The file is not written, and that is what
                // is said about it.
                Err(TryRecvError::Disconnected) => {
                    self.flash(monitor_i18n::tr("toast-export-interrupted"), ToastKind::Error)
                }
                Err(TryRecvError::Empty) => {
                    self.sealing = Some(Sealing::Export { result, target });
                }
            },
        }
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

    /// Says what an import of `count` cameras did, the picked document having no
    /// path to name.
    #[cfg(target_os = "android")]
    fn say_imported(&mut self, count: usize, _origin: Option<PathBuf>) {
        self.flash(
            monitor_i18n::tr_args("toast-imported-document", &[("count", count.into())]),
            ToastKind::Info,
        );
    }

    fn current_view(&self, total: usize) -> PageView {
        match self.scheduler.zoom() {
            Some(index) if index < total => PageView { layout: GridLayout::G1x1, cells: vec![Some(index)] },
            _ => {
                let layout = self.scheduler.layout();
                PageView { layout, cells: grid::page_cells(layout, self.scheduler.page(), total) }
            }
        }
    }

    /// Page revealed by a finger drag, when it exists.
    fn neighbour_view(&self, forward: bool, total: usize) -> Option<PageView> {
        if self.scheduler.is_zoomed() {
            let focus = self.scheduler.focus()?;
            let index = if forward { focus + 1 } else { focus.checked_sub(1)? };
            return (index < total).then_some(PageView { layout: GridLayout::G1x1, cells: vec![Some(index)] });
        }
        let page = if forward { self.scheduler.page() + 1 } else { self.scheduler.page().checked_sub(1)? };
        if page >= self.scheduler.page_count() {
            return None;
        }
        let layout = self.scheduler.layout();
        Some(PageView { layout, cells: grid::page_cells(layout, page, total) })
    }

    /// Where the magnified viewport's picture is drawn this frame, if it has one.
    ///
    /// The one cell a magnified viewport has, through the same rectangle the
    /// wall draws it with - the layout, the line the grid leaves, the slide the
    /// page is drawn at - and the size of the picture the channel is showing. It
    /// is what a pinch and a drag are measured against, because both are aimed
    /// where the fingers are and the fingers are over this rectangle.
    ///
    /// A channel whose first picture has not arrived has no rectangle: what the
    /// tile shows meanwhile is a waiting icon, and an icon is nothing to push in.
    /// The fit a camera's picture takes in a tile of the view on screen.
    ///
    /// Magnified, the viewport has a fit of its own - the bar's picture-fit
    /// control edits it, and until it does the picture keeps its own ratio. A
    /// tile on the wall takes the fit its orientation gives it.
    fn tile_aspect(&self, camera: &CameraSource) -> TileAspect {
        if self.scheduler.is_zoomed() {
            camera.zoom_aspect.unwrap_or(TileAspect::Original)
        } else if self.is_portrait() {
            camera.portrait_aspect
        } else {
            camera.aspect
        }
    }

    /// Whether the magnified picture is drawn a quarter turn on an upright wall.
    ///
    /// Upright, a landscape stream used to be turned to fill the screen. It is
    /// drawn as the camera sends it now - letterboxed - until the bar's rotate
    /// control asks for the turn. The choice is the configuration's own
    /// (`zoom_rotate`), one answer for every camera, because it is the wall's
    /// orientation that makes it a question and the viewer who wants it filled.
    /// A landscape wall never turns a picture: there is nothing to fill, and the
    /// control that would ask for it is not on the bar there.
    fn magnified_rotated(&self) -> bool {
        self.is_portrait() && self.scheduler.is_zoomed() && self.config.zoom_rotate
    }

    /// Whether the magnified bar is the picture's own default: up, and staying.
    ///
    /// A portrait wall that is not turned shows the strip from the moment the
    /// picture is magnified and leaves it there. Upright, the strip lies across
    /// the bottom of a letterboxed picture and the controls - the quarter turn
    /// above all - are what the viewer is looking for; a strip on its own clock
    /// would be one more thing to ask for. Turned, the picture fills the screen
    /// and the strip would lie along its side, in the way of it: it is asked for
    /// there, exactly as in landscape. See [`Self::settle_tile_bar`].
    fn tile_bar_pinned(&self) -> bool {
        self.is_portrait() && self.scheduler.is_zoomed() && !self.magnified_rotated()
    }

    /// Gives the bar the state the picture asks for: up while pinned, away
    /// otherwise.
    ///
    /// Called where the picture changes - a magnification, a channel step, the
    /// quarter turn, the orientation - and not every frame, so a viewer who taps
    /// the strip away is not overruled by the next frame.
    fn settle_tile_bar(&mut self) {
        // The picture has changed under the question: whatever a tap was asking
        // for, it was asking of the picture that has just gone.
        self.tap_open_at = None;
        if self.tile_bar_pinned() {
            self.tile_bar.show(tilebar::Dock::Bottom, Instant::now());
        } else {
            self.tile_bar.hide();
        }
    }

    fn magnified_area(
        &self,
        cameras: &[CameraSource],
        view: &PageView,
        wall: Rect,
        dx: f32,
    ) -> Option<Rect> {
        let index = self.scheduler.zoom()?;
        let cell = view.cells.iter().position(|slot| *slot == Some(index))?;
        let rect = grid::tile_rect(wall.translate(vec2(dx, 0.0)), view.layout, cell, grid::LINE);
        let camera = cameras.get(index)?;
        let size = self.textures.get(&index)?.surface.size;
        let aspect = self.tile_aspect(camera);
        // Upright, the magnified picture is the one that may be drawn sideways -
        // the same answer the tile is drawn with. See `paint_view`.
        Some(grid::picture_area(size, rect, aspect, self.magnified_rotated()))
    }

    fn paint_view(
        &self,
        ui: &mut egui::Ui,
        area: Rect,
        cameras: &[CameraSource],
        view: &PageView,
        dx: f32,
        interactive: bool,
        tag: u8,
    ) -> TileActions {
        let mut actions = TileActions::default();
        let origin = area.translate(vec2(dx, 0.0));
        if origin.max.x < area.min.x - 4.0 || origin.min.x > area.max.x + 4.0 {
            return actions;
        }
        let focus = self.scheduler.focus();
        // Read once for the frame: the same answer for every tile, and it is the
        // frame's clock rather than each cell's turn at it.
        let ring = self.ring_shown();

        for (cell, index) in view.cells.iter().enumerate() {
            let rect = grid::tile_rect(origin, view.layout, cell, grid::LINE);
            if rect.max.x < area.min.x - 4.0 || rect.min.x > area.max.x + 4.0 {
                continue;
            }
            let tile = Tile {
                id: Id::new(("xgview-tile", tag, cell, index.unwrap_or(usize::MAX))),
                index: *index,
                camera: index.and_then(|index| cameras.get(index)),
                channel: index.and_then(|index| self.channels.get(&index)),
                video: index.and_then(|index| self.textures.get(&index)).map(|entry| entry.surface),
                // The ring is a cursor, and a wall nobody is operating is just a
                // picture: it is drawn from the last key or press on and goes
                // once the wall has been left alone for the shorter of the ring's
                // own idle and the bars'. Which bars are up does not come into
                // it. See `ring_shown`.
                focused: interactive && *index == focus && ring,
                interactive,
                dim: !interactive,
                osd: self.config.osd,
                combined: [
                    &self.config.combined_info[0],
                    &self.config.combined_info[1],
                    &self.config.combined_info[2],
                    &self.config.combined_info[3],
                ],
                time: self.time,
                aspect: index
                    .and_then(|index| cameras.get(index))
                    .map(|camera| self.tile_aspect(camera))
                    .unwrap_or_default(),
                rotated: self.magnified_rotated(),
                // The magnified viewport is the one picture that can be pushed
                // in - it is the whole wall while it stands - and it is the only
                // tile that carries the window. Nothing else is zoomable, so
                // nothing else is given one.
                picture_zoom: (index.is_some() && *index == self.scheduler.zoom())
                    .then_some(self.picture_zoom),
            };
            let response = grid::paint(ui, rect, &tile);
            if interactive {
                if response.clicked() {
                    actions.focus = *index;
                    actions.click_pos = response.interact_pointer_pos();
                }
                if response.double_clicked() {
                    actions.zoom = *index;
                    actions.double_click_pos = response.interact_pointer_pos();
                }
                if response.drag_started() {
                    actions.drag_started = true;
                }
                if response.dragged() {
                    actions.drag_delta += response.drag_delta().x;
                }
                if response.drag_stopped() {
                    actions.drag_stopped = true;
                }
                if let Some(pos) = response.hover_pos() {
                    actions.hover_pos = Some(pos);
                }
            }
        }
        actions
    }

    fn draw_grid(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        if area.width() < 24.0 || area.height() < 24.0 {
            return;
        }
        let cameras = self.config.active_cameras();
        let total = cameras.len();
        if total == 0 {
            grid::empty_state(
                ui,
                area,
                &dialogs::group_heading(
                    &self.config,
                    self.config.active_index(),
                    "wall-empty-title",
                    "wall-empty-title-group",
                ),
                &monitor_i18n::tr(if self.is_portrait() {
                    "wall-empty-hint-portrait"
                } else {
                    "wall-empty-hint"
                }),
            );
            return;
        }

        // The wall's own colour is the grid line: the tiles leave one line's
        // width around and between themselves, and it shows through. Drawing
        // it here rather than as a border on each tile is what keeps every line
        // one line wide - two neighbours would otherwise double up in the
        // middle while the outer ring stayed half as thick.
        ui.painter().rect_filled(area, egui::CornerRadius::ZERO, theme::GRID_LINE);
        let wall = area.shrink(grid::LINE);

        let view = self.current_view(total);

        // Where the focused tile is: the arrow that walks off the top of the
        // wall should come down into the bar above that tile rather than at one
        // end of the row.
        self.grid_focus_x = self.scheduler.focus().and_then(|index| {
            let cell = view.cells.iter().position(|slot| *slot == Some(index))?;
            Some(grid::tile_rect(wall, view.layout, cell, grid::LINE).center().x)
        });

        // Page sliding out, or the neighbour page revealed by a finger drag.
        if let Some(outgoing) = self.slide.outgoing.clone() {
            let dx = (self.slide.offset - self.slide.dir) * area.width() + self.slide.drag;
            self.paint_view(ui, wall, &cameras, &outgoing, dx, false, 3);
        } else if self.slide.dragging && self.slide.drag.abs() > 1.0 {
            let forward = self.slide.drag < 0.0;
            if let Some(neighbour) = self.neighbour_view(forward, total) {
                let dx = if forward { self.slide.drag + area.width() } else { self.slide.drag - area.width() };
                self.paint_view(ui, wall, &cameras, &neighbour, dx, false, 3);
            }
        }

        let dx = self.slide.offset * area.width() + self.slide.drag;
        // What the magnified viewport shows of its picture, read and moved before
        // the tiles are drawn, so that this frame draws the window the gesture
        // has just left it at rather than the one before it. The gestures are the
        // frame's own input: while the magnification stands there is one picture
        // over the whole wall, and no tile beside it to catch a drag as one. See
        // `PictureZoom`.
        let pinched = ui.ctx().input(|input| pinch_frame(input, area));
        // A wheel is the same thing to the picture as a pinch - the fingers or
        // the mouse, pushed in about the point between them - and it is read
        // separately only because egui hands it over as a *scroll*: the interface
        // scrolls elsewhere, and the magnified viewport is the one place where
        // there is nothing to scroll. See `wheel_frame`.
        let speed = ui.ctx().options(|options| options.input_options.scroll_zoom_speed);
        let wheeled = ui.ctx().input(|input| wheel_frame(input, speed, area));
        // The fingers are the wall's on the same terms as the keys: while
        // something else owns them - a panel, a window, or the black - a gesture
        // made over the wall is the panel's to answer, and the picture keeps the
        // window it has. See `handle_keys`, which runs on the same answer. The
        // magnified viewport's bar is the wall's own surface even though its
        // controls hold the focus, so a gesture over the picture still reaches
        // the picture. The bar stays through it: a pinch, a wheel and a pan leave
        // the viewer on the same picture, and only a tap or a change of picture
        // takes the strip away. See [`Self::tap_release`].
        if self.scheduler.is_zoomed()
            && !self.blackout_on
            && (self.owner == Owner::Grid || self.tile_bar.is_open())
        {
            if let Some(dest) = self.magnified_area(&cameras, &view, wall, dx) {
                // A pinch or a wheel pushes the picture in about where it is
                // aimed; a drag moves it. The first two are one gesture or the
                // other, never both - a pinch is the fingers and the wheel is
                // the mouse - and neither is a drag, because a pinch's pointer
                // follows the first finger and a wheel has no pointer of its own.
                if let Some((delta, at)) = pinched.or(wheeled) {
                    // Pushing the picture in or out is the viewer at the same
                    // picture: the strip keeps its place while it happens.
                    self.picture_zoom = self.picture_zoom.zoomed(delta, at, dest);
                } else if self.picture_zoom.magnified() {
                    // A drag on the picture moves it; one that starts on the bar
                    // is the bar's. A finger's tap carries a little movement, and
                    // taking that as a pan would move the picture under the
                    // press that was meant for the strip.
                    let drag = ui.ctx().input(|input| {
                        drag_frame(input, area).map(|delta| (delta, input.pointer.press_origin()))
                    });
                    if let Some((delta, origin)) = drag {
                        let on_bar = self.tile_bar.is_open()
                            && origin.is_some_and(|origin| {
                                in_tile_bar_strip(
                                    origin,
                                    wall,
                                    self.magnified_rotated(),
                                    self.tile_bar.dock(),
                                )
                            });
                        if !on_bar {
                            self.picture_zoom = self.picture_zoom.panned(delta, dest);
                        }
                    }
                }
            }
        } else if !self.scheduler.is_zoomed() {
            // A wall that is not magnified has no picture to push in, and the
            // window a camera was left at belongs to that camera: whichever one
            // is magnified next comes up whole.
            self.picture_zoom = PictureZoom::FULL;
        }

        let mut actions = self.paint_view(ui, wall, &cameras, &view, dx, true, 2);

        // A drag on the magnified picture is the picture moving, not the page:
        // the wall has one picture on it and nowhere to turn to, and the channel
        // after it is reached from the toolbar's arrows or the page keys, which
        // step a magnified viewport already. A pinch is not a drag either, from
        // the frame the second finger lands: the pointer follows the first one,
        // and letting this through would turn a page under the fingers. See
        // `step_zoom`.
        if self.scheduler.is_zoomed() && (pinched.is_some() || self.picture_zoom.magnified()) {
            self.slide.dragging = false;
            self.slide.drag = 0.0;
            actions.drag_started = false;
            actions.drag_delta = 0.0;
            actions.drag_stopped = false;
        }

        if actions.drag_started {
            self.slide.dragging = true;
            self.slide.drag = 0.0;
        }
        if actions.drag_delta != 0.0 {
            self.slide.drag = (self.slide.drag + actions.drag_delta).clamp(-area.width(), area.width());
        }
        if actions.drag_stopped {
            self.slide.dragging = false;
            let threshold = area.width() * 0.18;
            if self.slide.drag <= -threshold {
                self.advance(true, false);
            } else if self.slide.drag >= threshold {
                self.advance(false, false);
            }
        }
        // The magnified viewport's bar is answered and drawn before the tile's
        // own click is: a press on it, or in the band that asks for it, belongs
        // to the bar rather than to the picture behind. See [`crate::tilebar`].
        let bar_owns_press = self.tile_bar_overlay(ui, &view, wall, dx, &actions);

        if !bar_owns_press {
            if let Some(index) = actions.focus {
                // A press on the wall is the viewer pointing at what they are
                // watching, so whatever control of the bars was holding the
                // remote's focus gives it up and the arrows move channels again.
                self.leave_controls(ui.ctx());
                if self.scheduler.focus() != Some(index) {
                    self.scheduler.set_focus(Some(index));
                    self.needs_sync = true;
                    self.mark_dirty();
                }
                // A click on a tile is the pointer's way of placing the ring, and
                // the only one it has: a drag turns the page without aiming at
                // anything, and a movement places nothing. So this is where a
                // pointer arms it. See `ring_keys`.
                self.ring_seen = Some(Instant::now());
                self.ring_aimed = true;
            }
            if let Some(index) = actions.zoom {
                self.zoom_to(index);
            }
        }
    }

    /// Answers the magnified viewport's bar, draws it, and reads the tap that
    /// puts it up or away.
    ///
    /// A tap - quick, still, one finger - is the whole of the pointer's part in
    /// it: one on the picture brings the strip up along the bottom, and one on
    /// the picture while it is up takes it away. Everything else the pointer can
    /// do there - a press that lingered, a drag, a pinch - belongs to the
    /// picture, which stays as it was. A double press is still the way back to
    /// the grid: the first press opens the strip, so the second is let through
    /// to the wall and the zoom closes the strip with it.
    ///
    /// Its shape comes from [`crate::tilebar`], which lays it out in the
    /// picture's frame and maps that onto the screen, so a picture drawn turned
    /// for a portrait wall carries a turned bar.
    fn tile_bar_overlay(
        &mut self,
        ui: &mut egui::Ui,
        view: &PageView,
        wall: Rect,
        dx: f32,
        actions: &TileActions,
    ) -> bool {
        if !self.scheduler.is_zoomed() {
            return false;
        }
        // A page or a channel sliding under the viewer is not the moment to
        // bring the bar up: a press that lands during a slide belongs to the
        // slide. A bar that is already up stays drawn through the drag - it
        // belongs to the picture under it and moves with it - so only the closed
        // case is held back.
        if !self.tile_bar.is_open() && (self.slide.offset != 0.0 || self.slide.dragging) {
            return false;
        }
        let Some(index) = self.scheduler.zoom() else {
            return false;
        };
        let Some(cell) = view.cells.iter().position(|slot| *slot == Some(index)) else {
            return false;
        };
        let rect = grid::tile_rect(wall.translate(vec2(dx, 0.0)), view.layout, cell, grid::LINE);
        // A window too small to hold the strip and the room it keeps clear of
        // the edge is not given one; this is a picture down to a postage stamp.
        if rect.width() < 120.0 || rect.height() < 120.0 {
            self.tile_bar.hide();
            return false;
        }

        let rotated = self.magnified_rotated();
        // The controls are read again below, after the press, so a press that
        // changes the picture fit also redraws the bar with the set it has in the
        // same frame it changes it. Every control is a square.
        let controls = self.tile_bar_controls();
        let widths = vec![tilebar::icon_width(); controls.len()];
        let layout = tilebar::layout(rect, rotated, self.tile_bar.dock(), &widths);
        let now = Instant::now();
        let mut consumed = false;

        // A press on the picture closes the bar only when it was a tap: quick,
        // still, and the only finger down. A press that lingered is a thumb left
        // on the glass, one that slid is a pan being reached for, and a pinch is
        // two fingers - the bar stays through all of them. See `tap_release`.
        let tap = self.tap_release(ui.ctx());
        if let Some(pos) = actions.click_pos {
            let hit = layout.hit(pos);
            if self.tile_bar.is_open() {
                if matches!(hit, tilebar::Hit::Outside) {
                    // A tap on the picture takes the bar down. The bar takes this
                    // press with it, so one press outside is not also the first
                    // click of a double press on the picture - a tap that opened
                    // the bar and then closed it would otherwise put the
                    // magnification back.
                    // A tap on the picture takes the bar down - unless a call is
                    // up, which makes the bar modal: the handset is the way out
                    // of a call, and the microphone being open has to stay on
                    // screen. The press is still spent here, so it does not
                    // reach the wall behind.
                    if tap.is_some() && !self.tile_bar.ignoring(now) && !self.talking {
                        self.tile_bar.hide();
                    }
                    // A double press out here is the way back to the grid: the
                    // first press opened the strip, so the second is the wall's,
                    // and the zoom closes the strip as it goes.
                    consumed = actions.double_click_pos.is_none();
                } else {
                    // A press on the strip that did not land on a control - the
                    // padding between them - is still the bar being used.
                    self.tile_bar.touch(now);
                    consumed = true;
                }
            } else if tap.is_some() {
                // A tap anywhere on the picture asks for the strip, along the
                // bottom where a strip of controls belongs - but it is asked for
                // after egui's double-click window, because the second tap of a
                // double press is the way back to the grid and the first must not
                // flash the strip on its way there. See `tap_open_at`.
                if actions.double_click_pos.is_some() {
                    // This is the second tap: the wall's, and no strip.
                    self.tap_open_at = None;
                    consumed = false;
                } else {
                    let delay = ui
                        .ctx()
                        .options(|options| options.input_options.max_double_click_delay);
                    self.tap_open_at = Some(now + Duration::from_secs_f64(delay.max(0.0)));
                    consumed = true;
                }
            }
        }

        // The press may have opened the bar on the other edge than the one the
        // last opening used, and it may have changed the set of controls the
        // strip carries - a call starting takes all of them away: lay it out
        // again with the dock and the widths it has now, so this frame paints it
        // there rather than at the old edge and then jumping over on the next
        // one.
        let entries = self.tile_bar_entries();
        let widths = vec![tilebar::icon_width(); entries.len()];
        let layout = tilebar::layout(rect, rotated, self.tile_bar.dock(), &widths);

        // A double press in the band is the bar being asked for, not the
        // viewport being put back: its first press has already opened the bar,
        // and the second is the same gesture.
        if let Some(pos) = actions.double_click_pos {
            if !matches!(layout.hit(pos), tilebar::Hit::Outside) {
                consumed = true;
            }
        }

        if self.tile_bar.is_open() {
            // The controls are egui's own buttons, placed in the picture's own
            // frame: a press lights the one it went down on, the ring is the
            // focus the navigation layer puts there, and the confirm key reaches
            // it as a click on that focus. What is left here is the guard - the
            // gesture that opened the bar must not also press it - and the act.
            let controls: Vec<tilebar::Control> =
                entries.iter().map(|(control, _)| *control).collect();
            if let Some(index) = tilebar::draw(ui, &mut self.nav, &layout, rotated, &controls) {
                if !self.tile_bar.ignoring(now) {
                    // Was the ring on a control, or still on the seat? The
                    // difference is whether the remote is driving the bar.
                    let driven = ui
                        .ctx()
                        .memory(|memory| memory.focused())
                        .is_some_and(|id| id != tilebar::seat());
                    let talking = self.talking;
                    self.tile_bar.touch(now);
                    if let Some((_, action)) = entries.get(index) {
                        self.tile_bar_activate(*action);
                    }
                    if self.talking != talking {
                        // The microphone has become the handset and the ring goes
                        // with it: a call the remote started is hung up by the
                        // same key, without a walk. A press that started it is
                        // not aiming with a ring, and keeps the cursor on the
                        // seat. See `tilebar::seat`.
                        let target =
                            if driven { tilebar::control(0) } else { tilebar::seat() };
                        ui.ctx().memory_mut(|memory| memory.request_focus(target));
                    }
                }
            }
        }

        consumed
    }

    /// Whether the magnified camera has sound for the bar to switch.
    ///
    /// A camera that has not reported yet counts as having it: the switch shows
    /// with the picture and only goes away once the stream itself says it
    /// carries no audio track, so it never flickers in and out while a session
    /// is coming up.
    fn magnified_has_audio(&self) -> bool {
        self.scheduler
            .zoom()
            .map_or(false, |index| self.channels.get(&index).map_or(true, |channel| channel.audio))
    }

    /// Whether the magnified camera offers a talk-back media.
    ///
    /// The answer comes from a probe of the camera, not from the picture's
    /// session: that one carries no back channel and asks for none, which is
    /// what keeps a magnified tile from holding a camera's speaker open while
    /// nobody is talking. There is still no optimistic default - the microphone
    /// appears when a camera has said yes - but the yes is the camera's, so it
    /// survives the session being reopened under it.
    fn magnified_has_backchannel(&self) -> bool {
        self.magnified_stream_uri()
            .is_some_and(|uri| self.talk_route.get(&uri).is_some_and(|route| route.reachable()))
    }

    /// The address a call to the magnified camera would be placed to.
    ///
    /// The main stream, whichever one the grid is pulling: a call goes to what
    /// the viewer is looking at, and that is the address the probe asked about.
    fn magnified_stream_uri(&self) -> Option<String> {
        let index = self.scheduler.zoom()?;
        let slot = self.config.active_slots().get(index).copied()?;
        let camera = self.config.cameras.get(slot)?;
        Some(camera.stream_uri(StreamKind::Main).to_string())
    }

    /// The controls the magnified bar carries, with what each does, in walk
    /// order.
    ///
    /// A call in progress takes the bar over: it carries one control, the red
    /// handset that ends it, and nothing else can be pressed while the
    /// microphone is open. Upright, the quarter turn comes first - the one
    /// control that changes the shape of the picture before anything is fitted
    /// into it. Then the picture fit - the icon rather than the word, with the
    /// new fit named in a toast as it changes - is always there; the sound switch
    /// for a camera that has sound, the microphone for one that offers a back
    /// channel.
    fn tile_bar_entries(&self) -> Vec<(tilebar::Control, TileBarAction)> {
        if self.talking {
            return vec![(tilebar::Control::Hangup, TileBarAction::Hangup)];
        }
        let mut entries = Vec::new();
        if self.is_portrait() {
            entries.push((tilebar::Control::Icon(Icon::Rotate), TileBarAction::Rotate));
        }
        entries.push((tilebar::Control::Icon(Icon::AspectRatio), TileBarAction::Fit));
        if tilebar::SOUND && self.magnified_has_audio() {
            let icon = if self.sound_on { Icon::Sound } else { Icon::Mute };
            entries.push((tilebar::Control::Icon(icon), TileBarAction::Sound));
        }
        if self.magnified_has_backchannel() {
            entries.push((tilebar::Control::Icon(Icon::Mic), TileBarAction::Mic));
        }
        entries
    }

    /// The controls alone, which is what the layout and the paint need.
    fn tile_bar_controls(&self) -> Vec<tilebar::Control> {
        self.tile_bar_entries().into_iter().map(|(control, _)| control).collect()
    }

    /// Answers a control of the magnified bar: the handset that ends a call, the
    /// quarter turn, the picture fit, the sound switch when the camera has sound,
    /// or the microphone when it has a back channel.
    ///
    /// It is the action of the control the cursor is on for a key and of the one
    /// a finger landed on - a press acts on what it hit without moving the
    /// cursor.
    fn tile_bar_activate(&mut self, action: TileBarAction) {
        match action {
            TileBarAction::Hangup => {
                // A call is up, and its one control is the way out.
                self.set_talking(false);
            }
            TileBarAction::Rotate => {
                self.toggle_zoom_rotate();
            }
            TileBarAction::Sound => {
                self.sound_on = !self.sound_on;
                self.config.sound_on = self.sound_on;
                self.apply_audio();
                self.mark_dirty();
            }
            TileBarAction::Mic => {
                self.set_talking(!self.talking);
            }
            TileBarAction::Fit => {
                // The fit this writes is the magnified viewport's own: it is kept
                // per camera, and the wall's landscape and portrait fits - the
                // pair the settings panel and the portrait aspect picker write -
                // are not touched. Until the first press the fit is the camera's
                // own ratio, `Original`.
                //
                // The control carries no word to read the value on, so the name
                // of the fit it has just moved to is said in the middle of the
                // screen.
                let Some(index) = self.scheduler.zoom() else {
                    return;
                };
                let Some(slot) = self.config.active_slots().get(index).copied() else {
                    return;
                };
                let Some(camera) = self.config.cameras.get_mut(slot) else {
                    return;
                };
                let next = camera.zoom_aspect.unwrap_or(TileAspect::Original).next();
                camera.zoom_aspect = Some(next);
                self.mark_dirty();
                self.flash_centered(monitor_i18n::tr(next.label()), ToastKind::Info);
            }
        }
    }

    /// Turns the magnified picture a quarter turn, or back.
    ///
    /// The choice is the configuration's own, one answer for every camera: the
    /// viewer who wants an upright screen filled wants it filled for whatever
    /// they step to next, and the letterbox is not a property of a camera. Only
    /// the magnified picture is turned: the wall's tiles are drawn upright
    /// whatever this says, and a landscape wall never turns one.
    fn toggle_zoom_rotate(&mut self) {
        self.config.zoom_rotate = !self.config.zoom_rotate;
        self.mark_dirty();
        // The picture's shape is the bar's default too: turned, it goes; back
        // upright, it comes.
        self.settle_tile_bar();
    }

    /// The picker that offers the layouts with no button of their own.
    ///
    /// A modal over the wall, the way "Add devices" is: it centres itself, its
    /// backdrop swallows the presses that would otherwise land behind it, and
    /// its own Close - or Back - is the only way out. The four square layouts
    /// have buttons; this holds the rest, three to a row, each drawn as a
    /// thumbnail of its own grid with the viewport count over it.
    ///
    /// A thumbnail is a control drawn by hand, the way the toolbar's icons are:
    /// the rectangle is allocated with `Sense::click`, so it takes the focus and
    /// the focus ring the rest of the interface draws, and the picture is
    /// painted into it. The three rows are three scopes of the navigation
    /// layer, and the vertical arrows cross between them at the same column.
    fn draw_layout_picker(&mut self, ctx: &egui::Context) {
        if !self.layout_picker {
            return;
        }
        let mut chosen: Option<GridLayout> = None;
        let mut close = false;

        egui::Modal::new(Id::new("xgview-layout-picker")).show(ctx, |ui| {
            ui.set_width(360.0);
            // The way out, at the far end of the title bar: a cross rather
            // than the word, which stays as its tooltip.
            let bar = dialogs::title_bar(
                ui,
                &monitor_i18n::tr("layout-picker-title"),
                true,
                &monitor_i18n::tr("dialog-close"),
            );
            if let Some(close_button) = bar.close {
                if self.nav.tracked(close_button).clicked() {
                    close = true;
                }
            }

            // Three to a row, each row a scope of the navigation layer: the
            // sideways arrows walk a row and the vertical ones cross between
            // them at the same column, so a remote reaches all nine.
            self.layout_picker_anchor = None;
            for (row, chunk) in GridLayout::EXTENDED.chunks(3).enumerate() {
                let scope = LAYOUT_PICKER_ROWS[row];
                ui.horizontal(|ui| {
                    self.nav.open(scope);
                    for layout in chunk {
                        let selected = self.scheduler.layout() == *layout;
                        let (rect, response) =
                            ui.allocate_exact_size(vec2(108.0, 84.0), Sense::click());
                        if self.layout_picker_anchor.is_none() {
                            self.layout_picker_anchor = Some(response.id);
                        }
                        if ui.is_rect_visible(rect) {
                            let painter = ui.painter();
                            // The two states the rest of the interface draws:
                            // the layout in use carries the accent fill, and the
                            // thumbnail the remote is on carries the focus ring.
                            // Both come from the same `WidgetVisuals` a
                            // `selectable_label` would use, so the fill and the
                            // number match the toolbar's own layout buttons.
                            let visuals = ui.style().interact_selectable(&response, selected);
                            let corner = CornerRadius::same(theme::radius::S);
                            if selected || response.hovered() {
                                painter.rect_filled(rect, corner, visuals.weak_bg_fill);
                            }
                            if visuals.bg_stroke.width > 0.0 {
                                painter.rect_stroke(
                                    rect,
                                    corner,
                                    visuals.bg_stroke,
                                    StrokeKind::Inside,
                                );
                            }
                            let thumb = Rect::from_min_size(
                                rect.left_top() + vec2(6.0, 6.0),
                                vec2(rect.width() - 12.0, rect.height() - 30.0),
                            );
                            grid::layout_thumbnail(painter, thumb, *layout);
                            painter.text(
                                egui::pos2(rect.center().x, rect.bottom() - 12.0),
                                Align2::CENTER_CENTER,
                                layout.label(),
                                FontId::proportional(15.0),
                                visuals.text_color(),
                            );
                        }
                        let response = self.nav.tracked(response);
                        self.name(&response, layout_hint(*layout));
                        if response.clicked() {
                            chosen = Some(*layout);
                        }
                    }
                    self.nav.close();
                });
            }
        });

        if let Some(layout) = chosen {
            self.set_layout(layout);
            self.layout_picker = false;
            self.restore_focus = self.layout_picker_from.take();
        } else if close {
            self.layout_picker = false;
            self.restore_focus = self.layout_picker_from.take();
        }
    }

    /// The modal that lists the cameras on the current portrait page and lets
    /// the remote cycle each one's picture fit. A row per camera: its number,
    /// its name, and the aspect it is drawn at - a press on the row turns the
    /// aspect to the next of the five.
    fn draw_portrait_aspect_picker(&mut self, ctx: &egui::Context) {
        if !self.portrait_aspect_picker {
            return;
        }
        let mut close = false;
        let mut aspect_change: Option<(usize, TileAspect)> = None;

        egui::Modal::new(Id::new("xgview-portrait-aspect-picker")).show(ctx, |ui| {
            ui.set_width(300.0);
            self.portrait_aspect_picker_anchor = None;
            self.nav.open("aspect-picker");
            let bar = dialogs::title_bar(
                ui,
                &monitor_i18n::tr("portrait-aspect-picker-title"),
                true,
                &monitor_i18n::tr("dialog-close"),
            );
            if let Some(close_button) = bar.close {
                if self.nav.tracked(close_button).clicked() {
                    close = true;
                }
            }
            let hint = ui.label(
                RichText::new(monitor_i18n::tr("portrait-aspect-picker-hint"))
                    .small()
                    .color(theme::TEXT_DIM),
            );
            let hint_focus = ui.interact(
                hint.rect,
                hint.id.with("focus"),
                egui::Sense::focusable_noninteractive(),
            );
            self.nav.item(&hint_focus);
            self.portrait_aspect_picker_anchor = Some(hint_focus.id);
            ui.add_space(theme::space::M);

            let layout = self.scheduler.layout();
            let page = self.scheduler.page();
            let total = self.config.enabled_count();
            let cells = grid::page_cells(layout, page, total);
            // A cell names a place on the wall, which is a place in the active
            // group's enabled cameras and not in the file: reading the number
            // straight out of `cameras` is how this dialog came to list the
            // built-in group's cameras on any wall. The slot is what the file
            // is indexed by, so the two are crossed through `active_slots`.
            let slots = self.config.active_slots();

            for cell in cells.iter() {
                let Some(index) = cell else { continue };
                let Some(slot) = slots.get(*index).copied() else { continue };
                let Some(camera) = self.config.cameras.get(slot) else { continue };
                let name = camera.short_label(18);
                let aspect = camera.portrait_aspect;
                ui.horizontal(|ui| {
                    ui.label(format!("{}.", *index + 1));
                    ui.label(name);
                    let btn = ui
                        .small_button(
                            RichText::new(monitor_i18n::tr(aspect.label()))
                                .small()
                                .color(theme::ACCENT),
                        )
                        .on_hover_text(monitor_i18n::tr("cameras-aspect-tip"));
                    let btn = self.nav.tracked(btn);

                    self.name(&btn, "portrait-aspect-entry");
                    if btn.clicked() {
                        aspect_change = Some((slot, aspect.next()));
                    }
                });
                ui.add_space(4.0);
            }
            self.nav.close();
        });

        if let Some((slot, aspect)) = aspect_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.portrait_aspect = aspect;
            }
            self.mark_dirty();
        }
        if close {
            self.portrait_aspect_picker = false;
            self.restore_focus = self.portrait_aspect_picker_from.take();
        }
    }

    /// The modal that lets the viewer pick which atomic items a combined-info
    /// slot stacks.  Two columns: available items on the left, selected items
    /// on the right.  Keyboard is handled in `handle_combined_info_keys`;
    /// this method only draws the UI and handles mouse clicks.
    fn draw_combined_info_picker(&mut self, ctx: &egui::Context) {
        if !self.combined_info_picker {
            return;
        }
        let slot = self.combined_info_picker_slot;
        let mut selected: Vec<OsdItem> = self.config.combined_info[slot].clone();
        let mut close = false;
        let mut changed = false;
        let mut click_add: Option<usize> = None;

        let focus_id = Id::new("xgview-combined-info-focus");
        let (mut focus_col, mut focus_idx): (usize, usize) =
            ctx.data(|d| d.get_temp(focus_id).unwrap_or((0, 0)));

        egui::Modal::new(Id::new("xgview-combined-info-picker")).show(ctx, |ui| {
            ui.set_width(420.0);

            self.nav.open("combined-info-picker");

            let bar = dialogs::title_bar(
                ui,
                &monitor_i18n::tr_args("combined-info-title", &[("n", (slot + 1).to_string().into())]),
                true,
                &monitor_i18n::tr("dialog-close"),
            );
            if let Some(close_btn) = bar.close {
                if self.nav.tracked(close_btn).clicked() {
                    close = true;
                }
            }
            // The rule is the dialog's first stop; the bar's own rule runs to
            // the border, so the focus ring is kept inside the content.
            let anchor = ui.interact(
                bar.rule.shrink2(vec2(f32::from(ui.style().spacing.menu_margin.left), 0.0)),
                ui.id().with("title-anchor"),
                egui::Sense::focusable_noninteractive(),
            );
            self.nav.item(&anchor);
            self.combined_info_picker_anchor = Some(anchor.id);
            ui.add_space(4.0);

            let available: Vec<OsdItem> = OsdItem::ATOMIC
                .iter()
                .filter(|atom| !selected.contains(atom))
                .copied()
                .collect();


            let mut click_remove: Option<usize> = None;

            ui.columns(2, |columns| {
                columns[0].spacing_mut().item_spacing.y = 2.0;
                columns[0].label(
                    RichText::new(monitor_i18n::tr("combined-info-available")).small().strong(),
                );
                for (i, atom) in available.iter().enumerate() {
                    let is_focused = focus_col == 0 && focus_idx == i;
                    let response = columns[0].selectable_label(
                        is_focused,
                        RichText::new(monitor_i18n::tr(atom.label())).small(),
                    );
                    if response.clicked() {
                        click_add = Some(i);
                    }
                }

                columns[1].spacing_mut().item_spacing.y = 2.0;
                columns[1].label(
                    RichText::new(monitor_i18n::tr("combined-info-selected")).small().strong(),
                );
                for (i, atom) in selected.iter().enumerate() {
                    let is_focused = focus_col == 1 && focus_idx == i;
                    let response = columns[1].selectable_label(
                        is_focused,
                        RichText::new(monitor_i18n::tr(atom.label())).small(),
                    );
                    if response.clicked() {
                        click_remove = Some(i);
                    }
                }
            });

            if let Some(i) = click_add {
                let atom = available[i];
                selected.push(atom);
                changed = true;
                if available.len() > 1 {
                    focus_col = 0;
                    focus_idx = i.min(available.len() - 2);
                } else {
                    focus_col = 1;
                    focus_idx = selected.len() - 1;
                }
            }
            if let Some(i) = click_remove {
                selected.remove(i);
                changed = true;
                if selected.is_empty() {
                    focus_col = 0;
                    focus_idx = i.min(available.len().saturating_sub(1));
                } else {
                    focus_col = 1;
                    focus_idx = i.min(selected.len() - 1);
                }
            }

            self.nav.close();
        });

        if changed {
            ctx.data_mut(|d| d.insert_temp(focus_id, (focus_col, focus_idx)));
            self.config.combined_info[slot] = selected;
            self.mark_dirty();
        }
        if close {
            self.combined_info_picker = false;
            self.restore_focus = self.combined_info_picker_from.take();
        }
    }

    fn draw_toast(&self, ctx: &egui::Context) {
        let Some(toast) = &self.toast else {
            return;
        };
        if toast.at.elapsed() > Duration::from_secs(6) {
            return;
        }
        let color = if toast.kind == ToastKind::Error { theme::ERROR } else { theme::LIVE };
        let (anchor, offset) = if toast.center {
            (egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
        } else {
            (egui::Align2::CENTER_BOTTOM, vec2(0.0, -56.0))
        };
        egui::Area::new(Id::new("xgview-toast"))
            .anchor(anchor, offset)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    // Never wrapped: an area starts with no width of its own, and
                    // a label given one character's room stacks the rest under
                    // it - which a two-word fit name and a longer sentence both
                    // showed.
                    let label = egui::Label::new(RichText::new(&toast.text).color(color))
                        .wrap_mode(egui::TextWrapMode::Extend);
                    ui.add(label);
                });
            });
    }

    /// The question the wall asks on the first Back, in the middle of the screen.
    ///
    /// Middle, and not one of the corners the toasts use: this one is not news
    /// about a channel, it is the only thing on screen that wants an answer, and
    /// a viewer with a remote is looking at the middle.
    fn draw_exit_hint(&self, ctx: &egui::Context) {
        let Some(at) = self.exit_armed else {
            return;
        };
        if at.elapsed() >= EXIT_WINDOW {
            return;
        }
        egui::Area::new(Id::new("xgview-exit-hint"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::symmetric(20, 14))
                    .show(ui, |ui| {
                        ui.label(RichText::new(monitor_i18n::tr("exit-hint")).size(18.0).strong());
                    });
            });
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
            self.layout_picker = false;
        }
        // Tell the activity which way to stand, once per change. A no-op on the
        // other platforms, which have no orientation to ask for.
        #[cfg(target_os = "android")]
        crate::android::set_orientation(self.is_portrait());
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
        // An answer comes back with the wall drawn a frame or two after it was
        // asked for, so the frame it lands in has to come on its own.
        if self.sealing.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
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

        // A panel that appears takes the remote's focus, once: without it the
        // arrows would go on walking the wall behind it, which is the one thing
        // a viewer cannot see happening. See `Handoff`.
        if self.settings_handoff.entering(self.show_settings) {
            if let Some(id) = self.settings_anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.layout_picker_handoff.entering(self.layout_picker) {
            if let Some(id) = self.layout_picker_anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.portrait_aspect_picker_handoff.entering(self.portrait_aspect_picker) {
            if let Some(id) = self.portrait_aspect_picker_anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.combined_info_picker_handoff.entering(self.combined_info_picker) {
            if let Some(id) = self.combined_info_picker_anchor {
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
        if self.remove_confirm_handoff.entering(self.remove_confirm.is_some()) {
            self.swallow_enter = 10;
            if let Some(id) = self.remove_confirm_anchor {
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.system_confirm_handoff.entering(self.system_confirm.is_some()) {
            self.swallow_enter = 10;
            if let Some(id) = self.system_confirm_anchor {
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
        self.ring_seen = Self::ring_follows_bars(self.ring_seen, self.chrome.visible, bars_was_up, Instant::now());
        // And the wall moving its own cursor takes down a ring that is showing:
        // a page turned by a key, a button pressed by a remote or a pointer, or a
        // swipe, and a layout that re-places the focus under the viewer, all land
        // here.
        self.ring_seen = Self::ring_follows_cursor(
            self.ring_seen,
            (self.scheduler.page(), self.scheduler.focus()),
            cursor_was,
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
        if let Some(start) = frame_start {
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
        if self.layout_picker {
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
}
