//! Application state and the egui render loop.
//!
//! The UI thread never touches a socket or a decoder: it drains the channel
//! event queues (oneshot per frame), recomputes the decoding schedule and paints
//! the grid. Everything else happens on the tokio runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use egui::{Align, Id, Key, Layout, Modifiers, Rect, RichText, vec2};
use tokio::runtime::Handle;

use monitor_core::autostart::{self, AutostartStatus};
use monitor_core::config::AppConfig;
use monitor_core::layout::{GridLayout, NavigateOutcome};
use monitor_core::model::{ConnectionState, OsdItem, RtspTransport, StreamKind, TileAspect};
use monitor_core::pipeline::{ChannelManager, StreamEvent};
use monitor_core::scheduler::Scheduler;
use monitor_core::{CameraSource, Direction};

use crate::dialogs::{self, BackgroundEvent, CameraDraft, DiscoveryUi, Tab};
use crate::fonts;
use crate::grid::{self, Tile, TileActions};
use crate::icons::{self, Icon};
use crate::controls;
use crate::nav::{Axis, Dir, Exit, Kind, Nav, ScopeDef};
use crate::theme;
use crate::video::{VideoRenderer, VideoSurface};
use crate::RunOptions;

/// Debounce applied before writing `config.json`.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(700);

/// Repaint interval while at least one channel is showing video.
const LIVE_REPAINT: Duration = Duration::from_millis(33);

/// Width the toolbar is laid out for at its full size.
///
/// A television and the desktop window are at least this wide; a phone in
/// landscape is narrower, and the row has to stay one row - a second one would
/// push the grid down, and the grid is what the viewer is for. Below this the
/// bar's text is scaled down until it fits.
const TOOLBAR_REFERENCE_WIDTH: f32 = 1280.0;

/// Smallest the toolbar is allowed to shrink to, as a fraction of its text size.
const TOOLBAR_MIN_SCALE: f32 = 0.6;

/// Space left between the right of the toolbar and the right of the window.
///
/// A touch on the outermost band of a touch screen is the system's, not ours: a
/// device using gesture navigation keeps a back-gesture band along each edge,
/// and a control inside it never sees the tap. The navigation bar itself is a
/// different case and is hidden on Android (see `MainActivity`); this margin is
/// for the band that no window flag removes. It is on the right only, because
/// the controls that need tapping are at that end of the row - the left end is
/// the title.
const EDGE_MARGIN: i8 = 40;

/// How long the bars stay on screen in full screen after the last input.
const CHROME_IDLE: Duration = Duration::from_millis(3500);

/// How long the question the wall asks on the first Back stands.
///
/// Back sits beside the arrows a remote is steered with, and a program that
/// left on one unmeant press cannot be brought back by the press that made the
/// mistake. The first press therefore asks, and only a second one inside this
/// window leaves. It also bounds how long the question is drawn for.
const EXIT_WINDOW: Duration = Duration::from_secs(2);

/// Width of the settings side panel, and the narrowest it may be dragged to.
///
/// A phone in landscape is barely wider than the panel at its default, which is
/// why it is a side panel and not a window: it can be dragged narrow, and on a
/// narrow screen the viewer mostly has it closed.
const SETTINGS_WIDTH: f32 = 360.0;
const SETTINGS_MIN_WIDTH: f32 = 300.0;

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
    /// Whether the viewer did something this frame. Read before the keys are
    /// handed out, because the application takes the ones it acts on out of the
    /// queue. See [`XgViewApp::viewer_active`].
    input_seen: bool,
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
            }
            StreamEvent::Stats { stream, codec, width, height, hardware, fps, bitrate_kbps, total_frames, .. } => {
                self.stream = Some(stream);
                if codec.is_some() {
                    self.codec = codec;
                }
                self.hardware = hardware;
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
}

/// Page of the settings side panel.
///
/// The panel used to be one long column. Five sections of it are read by five
/// different people - the wall's layout, the camera list, the stream policy,
/// the machine it runs on, and the version - and scrolling past four of them to
/// reach the fifth is what the tabs remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    Display,
    Cameras,
    Streams,
    System,
    About,
}

impl SettingsTab {
    const ALL: [Self; 5] = [Self::Display, Self::Cameras, Self::Streams, Self::System, Self::About];

    fn label(self) -> &'static str {
        match self {
            Self::Display => "Display",
            Self::Cameras => "Cameras",
            Self::Streams => "Streams",
            Self::System => "System",
            Self::About => "About",
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
    /// Identifier of the entry being edited. The update keeps it, so that a
    /// changed url repoints the camera instead of adding a second one.
    id: String,
    draft: CameraDraft,
    /// The edit button that opened the window, to give it back its focus.
    from: Id,
}

/// A camera the settings panel is asking about removing.
#[derive(Debug)]
struct RemoveConfirm {
    id: String,
    name: String,
    /// The remove button that opened the window, to give it back its focus.
    from: Id,
}

/// What the status line calls a grid layout.
///
/// [`GridLayout::label`] is the button's own text: right on the bar, where the
/// neighbouring buttons are the context, and too terse on its own in a line of
/// status.
fn layout_hint(layout: GridLayout) -> &'static str {
    match layout {
        GridLayout::G1x1 => "Grid 1×1",
        GridLayout::G2x2 => "Grid 2×2",
        GridLayout::G3x3 => "Grid 3×3",
        GridLayout::G4x4 => "Grid 4×4",
    }
}

/// Keyboard / DPAD keys consumed during one frame.
#[derive(Debug, Clone, Copy, Default)]
struct Keys {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    enter: bool,
    back: bool,
    page_next: bool,
    page_prev: bool,
    settings: bool,
    devices: bool,
    fullscreen: bool,
    layout: [bool; 4],
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
    /// Decoded pictures currently on the GPU, keyed by channel index.
    textures: HashMap<usize, ChannelTexture>,
    /// Uploads the decoded planes and turns them into those pictures.
    video: VideoRenderer,
    events_tx: Sender<BackgroundEvent>,
    events_rx: Receiver<BackgroundEvent>,
    autostart: AutostartStatus,
    decoder: &'static str,
    hardware_decoder: bool,
    /// Whether hardware and software decoding can be chosen between here. Left
    /// false, the setting is shown for what it is - a setting that does not
    /// apply - rather than offered and ignored.
    decoder_selectable: bool,
    show_settings: bool,
    /// Tab the settings panel is on.
    settings_tab: SettingsTab,
    /// First control of the settings panel, so the panel can be handed the
    /// remote's focus the moment it opens.
    settings_anchor: Option<Id>,
    /// The panel and the device window, watched for the frame they appear on.
    settings_handoff: Handoff,
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
    /// Whether the top and bottom bars are on screen right now.
    chrome: Chrome,
    /// The controls of the top bar with the rectangle each was drawn in, left to
    /// right, as of the last frame the bar was on screen.
    toolbar_items: Vec<(Id, Rect)>,
    /// Horizontal centre of the focused tile, which is where the arrow that
    /// walks off the top of the wall comes back down from.
    grid_focus_x: Option<f32>,
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
    time: f64,
}

impl XgViewApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        options: RunOptions,
        config_path: PathBuf,
        handle: Handle,
    ) -> Self {
        fonts::install(&cc.egui_ctx);
        theme::install(&cc.egui_ctx);

        let RunOptions { config, fullscreen, from_autostart, .. } = options;
        let mut config = config;
        let autostart = autostart::status();
        if autostart.supported {
            // The registry is the source of truth for the start-on-boot state.
            config.autostart = autostart.enabled;
        }
        let discovery = DiscoveryUi::new(&config.discovery);
        let scheduler = Scheduler::new(config.layout, config.page, config.focus, config.enabled_count());
        let manager = ChannelManager::spawn(&handle, config.reconnect.clone(), config.prefer_hardware_decode);
        let (events_tx, events_rx) = unbounded();
        let capabilities = monitor_codec::capabilities();

        // eframe is built with the wgpu backend, so it always offers the render
        // state the plane uploads go through.
        let render_state =
            cc.wgpu_render_state.as_ref().expect("eframe is built with the wgpu backend");
        let video = VideoRenderer::new(render_state);

        let mut app = Self {
            persisted: config.clone(),
            config,
            config_path,
            scheduler,
            manager,
            handle,
            channels: HashMap::new(),
            textures: HashMap::new(),
            video,
            events_tx,
            events_rx,
            autostart,
            decoder: capabilities.backend,
            hardware_decoder: capabilities.hardware,
            decoder_selectable: capabilities.selectable,
            show_settings: false,
            settings_tab: SettingsTab::default(),
            settings_anchor: None,
            settings_handoff: Handoff::default(),
            dialog_handoff: Handoff::default(),
            discovery,
            slide: Slide::default(),
            needs_sync: true,
            dirty: false,
            dirty_since: None,
            toast: None,
            exit_armed: None,
            fullscreen,
            chrome: Chrome {
                visible: true,
                last_input: Instant::now(),
                hide_now: false,
                input_seen: false,
            },
            toolbar_items: Vec::new(),
            grid_focus_x: None,
            // The "Add devices" window, the settings panel and the top bar are
            // on this layer: each is a row (of tabs, or of controls) over a
            // column, which is the shape egui's walk handles worst. The wall
            // keeps its own navigation. The two bodies are the scopes that
            // scroll - a tab strip is always in view, and a scope that asked to
            // be revealed from outside a scroll area would leave the request
            // for the next one in the pass to consume.
            nav: Nav::new(&[
                ("dialog-tabs", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope("dialog-body"))),
                ("dialog-body", ScopeDef::new(Axis::Column).scrolling().exit(Dir::Up, Exit::Scope("dialog-tabs"))),
                ("settings-tabs", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope("settings-body"))),
                ("settings-body", ScopeDef::new(Axis::Column).scrolling().exit(Dir::Up, Exit::Scope("settings-tabs"))),
                ("toolbar", ScopeDef::new(Axis::Row)),
                ("edit-body", ScopeDef::new(Axis::Column)),
                ("confirm-body", ScopeDef::new(Axis::Column)),
            ]),
            reveal_next: None,
            swallow_enter: 0,
            restore_focus: None,
            camera_edit: None,
            camera_edit_anchor: None,
            camera_edit_handoff: Handoff::default(),
            remove_confirm: None,
            remove_confirm_anchor: None,
            remove_confirm_handoff: Handoff::default(),
            reorder: None,
            reorder_anchor: None,
            reorder_trigger_anchor: None,
            reorder_focus: false,
            owner: Owner::Grid,
            focus_names: HashMap::new(),
            focused_name: None,
            from_autostart,
            time: 0.0,
        };
        app.sync();
        let message = if app.from_autostart {
            format!("XGView {} started automatically ({} decoder)", env!("CARGO_PKG_VERSION"), app.decoder)
        } else {
            format!("XGView {} — {} decoder", env!("CARGO_PKG_VERSION"), app.decoder)
        };
        app.flash(message, ToastKind::Info);
        app
    }

    // ---------------------------------------------------------------- helpers

    fn flash(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast { text: text.into(), kind, at: Instant::now() });
    }

    /// The control an open panel falls back to when nothing has the focus.
    ///
    /// The windows come first - only one of them is ever up - then the reorder
    /// list, then the settings panel; `None` when nothing is open, where the
    /// wall answers for itself. Read at the end of a frame to keep a panel from
    /// being left without a focus, and at the start of one to give the focus
    /// back the moment a key says the viewer is on the keyboard again.
    fn panel_anchor(&self) -> Option<Id> {
        if self.remove_confirm.is_some() {
            self.remove_confirm_anchor
        } else if self.camera_edit.is_some() {
            self.camera_edit_anchor
        } else if self.discovery.open {
            self.discovery.focus_anchor
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
        self.channels.retain(|index, _| *index < count);
        // A channel beyond the grid is gone for good, and its textures with it.
        let dropped: Vec<usize> =
            self.textures.keys().copied().filter(|index| *index >= count).collect();
        for index in dropped {
            self.video.release(index);
            self.textures.remove(&index);
        }

        self.config.layout = self.scheduler.layout();
        self.config.page = self.scheduler.page();
        self.config.focus = self.scheduler.focus();
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
                self.flash(format!("cannot save {}: {err}", self.config_path.display()), ToastKind::Error);
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
        self.scheduler.zoom_out();
        self.scheduler.set_layout(layout);
        self.slide = Slide::default();
        self.needs_sync = true;
        self.mark_dirty();
    }

    fn turn_page(&mut self, forward: bool) -> bool {
        let previous = self.scheduler.page();
        let changed = if forward { self.scheduler.next_page() } else { self.scheduler.prev_page() };
        if changed {
            let layout = self.scheduler.layout();
            let total = self.config.enabled_count();
            let outgoing = PageView { layout, cells: grid::page_cells(layout, previous, total) };
            self.slide.start(if forward { 1.0 } else { -1.0 }, Some(outgoing));
            self.needs_sync = true;
            self.mark_dirty();
        }
        self.slide.drag = 0.0;
        changed
    }

    fn navigate(&mut self, dir: Direction) -> NavigateOutcome {
        let previous_page = self.scheduler.page();
        let previous_focus = self.scheduler.focus();
        let zoomed = self.scheduler.is_zoomed();
        let outcome = self.scheduler.navigate(dir);
        if matches!(outcome, NavigateOutcome::Blocked) {
            return outcome;
        }

        let sign = if dir == Direction::Left { -1.0 } else { 1.0 };
        let outgoing = if zoomed {
            previous_focus.map(|index| PageView { layout: GridLayout::G1x1, cells: vec![Some(index)] })
        } else if matches!(outcome, NavigateOutcome::PageTurned { .. }) {
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

    fn zoom_in(&mut self) {
        if self.scheduler.zoom_in() {
            self.needs_sync = true;
            self.mark_dirty();
        }
    }

    fn zoom_out(&mut self) {
        if self.scheduler.zoom_out() {
            self.needs_sync = true;
            self.mark_dirty();
        }
    }

    fn zoom_to(&mut self, index: usize) {
        self.scheduler.set_focus(Some(index));
        self.zoom_in();
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, enabled: bool) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(enabled));
        self.fullscreen = enabled;
        // Leaving full screen brings the bars back on an explicit action, not
        // on a timer: the viewer is about to use them. Entering it is the
        // opposite request - the picture and nothing else - so they go at once
        // rather than after the idle delay.
        self.chrome.visible = !enabled;
        self.chrome.hide_now = enabled;
        self.chrome.last_input = Instant::now();
    }

    /// Whether the viewer did something this frame.
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
    /// Called before the keys are handed out: the application takes the ones it
    /// acts on out of the queue, and the bars still have to hear about them.
    fn viewer_active(ctx: &egui::Context) -> bool {
        ctx.input(|input| {
            input.pointer.any_down()
                || input.events.iter().any(|event| match event {
                    egui::Event::Key { pressed, .. } => *pressed,
                    egui::Event::PointerButton { pressed, .. } => *pressed,
                    // The window's own geometry, and the integration's own
                    // bookkeeping, rather than anything the viewer did.
                    egui::Event::PointerMoved(_)
                    | egui::Event::WindowFocused(_)
                    | egui::Event::Screenshot { .. }
                    | egui::Event::PointerGone => false,
                    // Raw mouse motion, the wheel, a touch, a key's text, a
                    // dropped file: the viewer.
                    _ => true,
                })
        })
    }

    /// Whether the top and bottom bars are drawn this frame.
    ///
    /// Outside full screen they always are - the window has a frame around it
    /// anyway, and the bars are where the controls live. In full screen they
    /// fade once the wall has been left alone for [`CHROME_IDLE`], which is the
    /// difference between a monitor and a television.
    fn chrome_visible(&mut self) -> bool {
        // An open panel owns the bars: taking away the bar of a panel the
        // viewer just opened would strand it.
        if self.show_settings || self.discovery.open {
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
        if std::mem::take(&mut self.chrome.hide_now) {
            self.chrome.visible = false;
            return false;
        }
        if self.chrome.input_seen {
            self.chrome.last_input = Instant::now();
            self.chrome.visible = true;
        } else if self.chrome.last_input.elapsed() >= CHROME_IDLE {
            self.chrome.visible = false;
        }
        self.chrome.visible
    }

    fn back(&mut self, ctx: &egui::Context) {
        // BACK / Esc unwinds one layer at a time: the magnified viewport, then
        // whatever was opened over the wall. With nothing left to close the wall
        // itself answers, and it asks before it leaves - see `arm_exit`.
        //
        // Full screen is not one of these layers any more: on a television the
        // viewer is in it from the start, so it cannot have been what the press
        // meant, and unwinding it would put an exit out of reach. F11 and the
        // settings panel's own checkbox are what toggle it.
        if self.scheduler.is_zoomed() {
            self.zoom_out();
            self.exit_armed = None;
        } else if self.discovery.open {
            self.discovery.open = false;
            self.exit_armed = None;
        } else if self.show_settings {
            self.show_settings = false;
            self.exit_armed = None;
        } else {
            self.arm_exit(ctx);
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
            std::process::exit(0);
        }
        #[cfg(not(target_os = "android"))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ---------------------------------------------------------------- events

    fn poll_events(&mut self) {
        for event in self.manager.poll() {
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
                    toast = Some((format!("added {}", camera.name), ToastKind::Info));
                }
                BackgroundEvent::ImportFailed { name, error } => {
                    toast = Some((format!("{name}: {error}"), ToastKind::Error));
                }
                BackgroundEvent::SynologyDone(cameras) => {
                    // The cameras are listed in the dialog for the viewer to
                    // pick from, not imported on arrival - see `synology_tab`.
                    toast = Some((
                        format!("{} camera(s) found on Surveillance Station", cameras.len()),
                        ToastKind::Info,
                    ));
                }
                BackgroundEvent::SynologyFailed(error) => {
                    toast = Some((format!("Synology: {error}"), ToastKind::Error));
                }
                BackgroundEvent::DiscoveryFailed(error) => {
                    toast = Some((format!("discovery failed: {error}"), ToastKind::Error));
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

    /// Whether a text field is being typed into.
    ///
    /// egui does not answer this question directly, but a text field is the only
    /// widget that keeps a [`egui::text_edit::TextEditState`], so a focused
    /// widget with that state under its id is one.
    fn typing(ctx: &egui::Context) -> bool {
        ctx.memory(|memory| memory.focused())
            .is_some_and(|focused| egui::TextEdit::load_state(ctx, focused).is_some())
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
    fn dialog_back_pressed(ctx: &egui::Context) -> bool {
        if Self::back_pressed(ctx) {
            return true;
        }
        !Self::typing(ctx)
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Backspace))
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
    fn handle_keys(&mut self, ctx: &egui::Context) {
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
        if (self.camera_edit.is_some() || self.remove_confirm.is_some()) && Self::dialog_back_pressed(ctx) {
            if let Some(confirm) = self.remove_confirm.take() {
                self.remove_confirm_anchor = None;
                self.restore_focus = Some(confirm.from);
            } else if let Some(edit) = self.camera_edit.take() {
                self.camera_edit_anchor = None;
                self.restore_focus = Some(edit.from);
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
        let typing = Self::typing(ctx);
        if typing {
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
                        Some(Kind::DragValue) => {
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
                ];
            }
            keys
        });

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
        let dialog_open = self.discovery.open || self.camera_edit.is_some() || self.remove_confirm.is_some();
        if keys.settings && !dialog_open {
            self.show_settings = !self.show_settings;
        }
        if keys.devices && !dialog_open {
            self.discovery.open = !self.discovery.open;
        }
        if keys.fullscreen {
            let enabled = !self.fullscreen;
            self.set_fullscreen(ctx, enabled);
        }
        if keys.page_prev {
            self.turn_page(false);
        }
        if keys.page_next {
            self.turn_page(true);
        }
        for (slot, pressed) in keys.layout.iter().enumerate() {
            if *pressed {
                if let Some(layout) = GridLayout::ALL.get(slot) {
                    self.set_layout(*layout);
                }
            }
        }

        // A control of the navigation layer: the arrow moves by declaration -
        // the scope's order, then its exits - and not by geometry. Answered
        // before the Back handling below, so that Back still unwinds a window.
        if nav_owns {
            // A drag value takes its value from the sideways presses recorded
            // here; the widget applies the step as it is drawn this frame.
            // Left steps down and Right up, the way a slider reads them.
            if nav_kind == Some(Kind::DragValue) {
                if keys.left {
                    self.nav.step_value(-1);
                }
                if keys.right {
                    self.nav.step_value(1);
                }
            }
            // Up and Down always move the focus. A value control - slider, drag
            // value or text field - answers its sideways keys itself, so they
            // never reach here; a plain control walks with all four.
            let dir = match nav_kind {
                Some(Kind::Slider) | Some(Kind::DragValue) | Some(Kind::Text) => {
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
                if let Some(target) = self.nav.step(focused, dir) {
                    ctx.memory_mut(|memory| memory.request_focus(target));
                }
            }
        }

        if !grid_owns {
            // egui has walked the focus already - and dropped it, if this was
            // Escape. Back is therefore one step out of the controls and not
            // two: a panel opened over the wall closes, and otherwise the wall
            // takes the keys back.
            if keys.back {
                if self.discovery.open {
                    self.discovery.open = false;
                } else if self.show_settings {
                    self.show_settings = false;
                } else {
                    self.leave_controls(ctx);
                }
            } else if keys.down && !self.show_settings && self.toolbar_has_focus(ctx) {
                // Below the bar there is only the wall, so the arrow that walks
                // off the bottom of it hands the keys back.
                self.leave_controls(ctx);
            }
            return;
        }

        if keys.left {
            self.navigate(Direction::Left);
        }
        if keys.right {
            self.navigate(Direction::Right);
        }
        if keys.down {
            self.navigate(Direction::Down);
        }
        if keys.up && self.navigate(Direction::Up) == NavigateOutcome::Blocked {
            self.enter_toolbar(ctx);
        }
        if keys.enter {
            if self.scheduler.is_zoomed() {
                self.zoom_out();
            } else {
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
    }

    fn needs_animation(&self) -> bool {
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

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        // The row is laid out for a television. A phone in landscape has less
        // room than that, and wrapping would cost the grid a row, so the bar
        // gives up text size instead: at 1280 and above nothing changes, and
        // below it the labels, the buttons' padding and the gaps shrink
        // together, down to the floor where they stop being readable.
        let scale = (ui.available_width() / TOOLBAR_REFERENCE_WIDTH).clamp(TOOLBAR_MIN_SCALE, 1.0);
        if scale < 1.0 {
            let style = ui.style_mut();
            for font in style.text_styles.values_mut() {
                font.size *= scale;
            }
            style.spacing.button_padding.x *= scale;
            style.spacing.interact_size.x *= scale;
            style.spacing.item_spacing.x = (style.spacing.item_spacing.x * scale).max(2.0);
        }
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
        ui.horizontal(|ui| {
            ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
            ui.separator();

            // Layout and page are the two things a viewer changes while
            // watching, so they stay on the bar whatever its width.
            for layout in GridLayout::ALL {
                let selected = self.scheduler.layout() == layout && !self.scheduler.is_zoomed();
                let response = ui.selectable_label(selected, layout.label());
                items.push((response.id, response.rect));
                self.name(&response, layout_hint(layout));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }

            // Only while magnified, and only there: the arrow on a tile, Enter
            // and a double click all magnify, but a touch screen has no keyboard
            // and no arrow, so this is the one way back it can offer.
            if self.scheduler.is_zoomed() {
                let back = icons::button(ui, Icon::Grid, false, "Back to the grid (Esc / Back)");
                items.push((back.id, back.rect));
                self.name(&back, "Back to the grid");
                if back.clicked() {
                    self.zoom_out();
                }
            }

            ui.separator();
            let info = self.scheduler.page_info();
            let previous = ui.add_enabled(info.has_previous(), egui::Button::new("◀"));
            if info.has_previous() {
                items.push((previous.id, previous.rect));
                self.name(&previous, "Previous page");
            }
            if previous.clicked() {
                self.turn_page(false);
            }
            ui.label(format!("page {} / {}", info.page + 1, info.page_count));
            let next = ui.add_enabled(info.has_next(), egui::Button::new("▶"));
            if info.has_next() {
                items.push((next.id, next.rect));
                self.name(&next, "Next page");
            }
            if next.clicked() {
                self.turn_page(true);
            }

            // The right end of the row is the three things a viewer reaches for
            // while watching, and each of them is a shape a remote control, a
            // mouse and a finger all understand without a word of English.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let fullscreen = icons::button(
                    ui,
                    if self.fullscreen { Icon::Collapse } else { Icon::Expand },
                    self.fullscreen,
                    if self.fullscreen { "Leave full screen (F11)" } else { "Full screen (F11)" },
                );
                items.push((fullscreen.id, fullscreen.rect));
                self.name(
                    &fullscreen,
                    if self.fullscreen { "Leave full screen" } else { "Full screen" },
                );
                if fullscreen.clicked() {
                    let enabled = !self.fullscreen;
                    self.set_fullscreen(ui.ctx(), enabled);
                }

                let settings = icons::button(ui, Icon::Gear, self.show_settings, "Settings (F1)");
                items.push((settings.id, settings.rect));
                self.name(&settings, "Settings");
                if settings.clicked() {
                    self.show_settings = !self.show_settings;
                }

                let devices = icons::button(ui, Icon::Plus, self.discovery.open, "Add devices (F2)");
                items.push((devices.id, devices.rect));
                self.name(&devices, "Add devices");
                if devices.clicked() {
                    self.discovery.open = !self.discovery.open;
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
        ui.horizontal(|ui| {
            let info = self.scheduler.page_info();
            // How many of the configured channels are actually on the wall, and
            // how many there are to be: the one number that says at a glance
            // whether anything is missing. It reads better down here than in the
            // bar, where it was competing with the controls.
            let live = self.channels.values().filter(|channel| channel.state.is_live()).count();
            ui.label(
                RichText::new(format!("{live}/{} live", self.config.enabled_count()))
                    .color(if live > 0 { theme::LIVE } else { theme::TEXT_DIM }),
            );
            ui.separator();
            ui.label(format!("{} · page {}/{}", self.scheduler.layout().label(), info.page + 1, info.page_count));
            if self.scheduler.is_zoomed() {
                ui.separator();
                ui.label(RichText::new("1x1 zoom · main stream").color(theme::FOCUS));
            }
            ui.separator();
            match self.scheduler.focus() {
                Some(focus) => ui.label(format!("focus #{focus}")),
                None => ui.label("no focus"),
            };
            // What the remote control is on, for the controls that carry a shape
            // instead of a word and have no pointer to hover for a tooltip.
            if let Some(name) = self.focused_name {
                ui.separator();
                ui.label(RichText::new(name).color(theme::FOCUS));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("decoder: {}", self.decoder)).small().color(theme::TEXT_DIM));
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
            let mut ids = [Id::NULL; 5];
            let mut focused = None;
            self.nav.open("settings-tabs");
            for (slot, tab) in SettingsTab::ALL.into_iter().enumerate() {
                let selected = self.settings_tab == tab;
                let label = match tab {
                    SettingsTab::Cameras => format!("Cameras ({})", self.config.cameras.len()),
                    other => other.label().to_string(),
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
        let dirty = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match self.settings_tab {
                SettingsTab::Display => self.settings_display(ui),
                SettingsTab::Cameras => self.settings_cameras(ui),
                SettingsTab::Streams => self.settings_streams(ui),
                SettingsTab::System => self.settings_system(ui),
                SettingsTab::About => {
                    self.settings_about(ui);
                    false
                }
            })
            .inner;
        self.nav.close();
        if dirty {
            self.mark_dirty();
        }
    }

    /// The wall itself: the grid, what the tiles show, and how the window opens.
    fn settings_display(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        ui.label(RichText::new("Grid").strong());
        ui.horizontal_wrapped(|ui| {
            for layout in GridLayout::ALL {
                let selected = self.scheduler.layout() == layout;
                let response = self.nav.tracked(ui.selectable_label(selected, layout.label()));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }
        });
        ui.add_space(theme::space::S);
        ui.label(RichText::new("On-screen display").strong());
        ui.label(
            RichText::new("What each corner of a tile shows. Press a button to cycle.")
                .small()
                .color(theme::TEXT_DIM),
        );
        let corners: [(&str, &mut OsdItem); 4] = [
            ("Top left", &mut self.config.osd.top_left),
            ("Top right", &mut self.config.osd.top_right),
            ("Bottom left", &mut self.config.osd.bottom_left),
            ("Bottom right", &mut self.config.osd.bottom_right),
        ];
        for (corner, item) in corners {
            ui.horizontal(|ui| {
                ui.label(corner);
                let button = ui.button(item.label());
                self.nav.item(&button);
                self.focus_names.insert(button.id, corner);
                if button.clicked() {
                    *item = item.next();
                    dirty = true;
                }
            });
        }
        ui.add_space(theme::space::S);
        let mut fullscreen = self.fullscreen;
        let full = self.nav.tracked(ui.checkbox(&mut fullscreen, "Full screen (F11)"));
        if full.changed() {
            self.set_fullscreen(ui.ctx(), fullscreen);
        }
        let mut start_fullscreen = self.config.start_fullscreen;
        let start = self.nav.tracked(ui.checkbox(&mut start_fullscreen, "Open in full screen at start-up"));
        if start.changed() {
            self.config.start_fullscreen = start_fullscreen;
            dirty = true;
        }

        dirty
    }

    /// What the box does around the viewer: it starts it at boot, and it is the
    /// machine the decoder is chosen for.
    fn settings_system(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        ui.label(RichText::new("Start-up").strong());
        let supported = self.autostart.supported;
        let mut enabled = self.config.autostart;
        ui.add_enabled_ui(supported, |ui| {
            let boot = ui.checkbox(&mut enabled, "Start with the system boot");
            self.nav.item(&boot);
            if boot.changed() {
                match autostart::set_enabled(enabled) {
                    Ok(()) => {
                        self.config.autostart = enabled;
                        self.autostart = autostart::status();
                        dirty = true;
                        self.flash(
                            if enabled { "registered for start-on-boot" } else { "start-on-boot registration removed" },
                            ToastKind::Info,
                        );
                    }
                    Err(err) => {
                        self.autostart = autostart::status();
                        self.flash(format!("start-on-boot: {err}"), ToastKind::Error);
                    }
                }
            }
        });
        if !supported {
            ui.label(RichText::new("start-on-boot is not supported on this platform").small().color(theme::TEXT_DIM));
        }
        ui.label(RichText::new(format!("mechanism: {}", self.autostart.mechanism)).small().color(theme::TEXT_DIM));
        ui.label(RichText::new(&self.autostart.detail).small().color(theme::TEXT_DIM));

        dirty
    }

    /// What the viewer pulls off each camera, and what turns it into pictures.
    fn settings_streams(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        ui.label(
            RichText::new("The multi grid pulls the sub stream; 1x1 - and a magnified viewport - pull the main stream.")
                .small()
                .color(theme::TEXT_DIM),
        );

        ui.add_space(theme::space::M);
        ui.label(RichText::new("Reconnect").strong());
        let reconnect = &mut self.config.reconnect;
        ui.horizontal(|ui| {
            ui.label("first retry after");
            let first = controls::drag_value(&mut self.nav, ui, &mut reconnect.initial_delay_ms, 100..=10_000, 100.0, " ms");
            dirty |= first.changed();
            ui.label("then at most");
            let second = controls::drag_value(&mut self.nav, ui, &mut reconnect.max_delay_ms, 1_000..=300_000, 1000.0, " ms");
            dirty |= second.changed();
        });
        ui.label(RichText::new("a change applies to the connections opened afterwards").small().color(theme::TEXT_DIM));
        // The shape of the curve is settled once, by whoever sized the network,
        // and never looked at again: it belongs behind a fold, not on the tab.
        // The fold is a stop of its own, ahead of the controls it hides; see
        // `controls::fold`.
        controls::fold(&mut self.nav, ui, "Backoff shape", false, |ui, nav| {
            ui.horizontal(|ui| {
                ui.label("factor");
                let factor = controls::drag_value(nav, ui, &mut reconnect.multiplier, 1.0..=5.0, 0.05, "");
                dirty |= factor.changed();
                ui.label("jitter");
                let jitter = controls::drag_value(nav, ui, &mut reconnect.jitter, 0.0..=1.0, 0.02, "");
                dirty |= jitter.changed();
                ui.label("attempts (0 = forever)");
                let attempts = controls::drag_value(nav, ui, &mut reconnect.max_attempts, 0..=100, 1.0, "");
                dirty |= attempts.changed();
            });
        });

        ui.add_space(theme::space::M);
        ui.label(RichText::new("Decoding").strong());
        let mut prefer_hardware = self.config.prefer_hardware_decode;
        ui.add_enabled_ui(self.decoder_selectable, |ui| {
            let hardware = ui.checkbox(&mut prefer_hardware, "Prefer hardware decoding");
            self.nav.item(&hardware);
            if hardware.changed() {
                self.config.prefer_hardware_decode = prefer_hardware;
                // A decoder is chosen when a session opens, so the change is applied
                // by reopening the channels rather than on the next connection.
                self.manager.set_hardware_preference(prefer_hardware);
                dirty = true;
                self.flash(
                    if prefer_hardware {
                        "channels reopening, the GPU decodes where it can"
                    } else {
                        "channels reopening on the CPU"
                    },
                    ToastKind::Info,
                );
            }
        });
        let summary = if !self.decoder_selectable {
            "Decoding happens in hardware here and nowhere else, so there is nothing to switch."
        } else if self.hardware_decoder {
            "Pictures are decoded on the GPU where the machine offers a decoder for the stream, and on the CPU everywhere else. Each tile shows which one it got."
        } else {
            "This build has no hardware decoder: the CPU decodes, whatever this setting says."
        };
        ui.label(RichText::new(summary).small().color(theme::TEXT_DIM));

        dirty
    }

    /// The cameras the wall shows, and the way to add more.
    fn settings_cameras(&mut self, ui: &mut egui::Ui) -> bool {
        // The reorder mode draws its own list and writes nothing back until it
        // is confirmed. See `cameras_reorder`.
        if let Some(order) = self.reorder.clone() {
            self.cameras_reorder(ui, &order);
            ui.add_space(theme::space::M);
            let confirm = ui.button("Confirm order");
            self.nav.item(&confirm);
            self.name(&confirm, "Confirm order");
            if confirm.clicked() {
                return self.apply_reorder();
            }
            return false;
        }
        self.cameras_list(ui)
    }

    /// The camera list as configured, with the button that opens the reorder
    /// mode at its foot. Returns `true` when the configuration changed.
    fn cameras_list(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;

        ui.horizontal(|ui| {
            let devices = ui.button("Add devices…");
            self.nav.item(&devices);
            if devices.clicked() {
                self.discovery.open = true;
                self.discovery.tab = Tab::Onvif;
            }
            let manual = ui.button("Add manually…");
            self.nav.item(&manual);
            if manual.clicked() {
                self.discovery.open = true;
                self.discovery.tab = Tab::Manual;
            }
        });
        ui.label(RichText::new(dialogs::discovery_summary(&self.config.discovery)).small().color(theme::TEXT_DIM));
        ui.add_space(theme::space::S);

        let mut toggle: Option<(usize, bool)> = None;
        let mut ask_remove: Option<(String, String, Id)> = None;
        let mut edit: Option<(String, Id)> = None;
        let mut aspect_change: Option<(usize, TileAspect)> = None;
        let mut infer: Option<usize> = None;
        let mut transport_change: Option<(usize, RtspTransport)> = None;
        for (slot, camera) in self.config.cameras.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let mut camera_enabled = camera.enabled;
                let enable = ui.checkbox(&mut camera_enabled, "");
                self.nav.item(&enable);
                if enable.changed() {
                    toggle = Some((slot, camera_enabled));
                }
                ui.label(RichText::new(camera.short_label(18)).strong());
                if let Some(sub) = camera.rtsp_sub.as_deref() {
                    let inferred = sub == monitor_core::model::infer_sub_stream(&camera.rtsp_main).unwrap_or_default();
                    ui.label(
                        RichText::new(if inferred { "sub: derived" } else { "sub: set" })
                            .small()
                            .color(theme::ACCENT),
                    );
                } else {
                    let infer_button = ui.small_button("infer sub");
                    self.nav.item(&infer_button);
                    if infer_button.clicked() {
                        infer = Some(slot);
                    }
                }
                let transport = camera.transport;
                let transport_button = ui
                    .small_button(RichText::new(transport.as_str()).small().color(if transport.is_udp() {
                        theme::ACCENT
                    } else {
                        theme::TEXT_DIM
                    }))
                    .on_hover_text(
                        "RTSP transport for this camera. UDP bypasses a relay that damages \
                         the TCP interleaved framing.",
                    );
                self.nav.item(&transport_button);
                if transport_button.clicked() {
                    transport_change = Some((slot, transport.toggled()));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Drawn right to left so the pair reads edit, remove;
                    // registered edit first, so the walk reaches the control
                    // that changes before the one that deletes.
                    let remove_button = icons::button_sized(ui, Icon::Trash, false, false, "Remove this camera", ROW_ICON);
                    let edit_button = icons::button_sized(ui, Icon::Edit, false, false, "Edit this camera", ROW_ICON);
                    self.nav.item(&edit_button);
                    self.focus_names.insert(edit_button.id, "Edit camera");
                    self.nav.item(&remove_button);
                    self.focus_names.insert(remove_button.id, "Remove camera");
                    if remove_button.clicked() {
                        ask_remove = Some((camera.id.clone(), camera.name.clone(), remove_button.id));
                    }
                    if edit_button.clicked() {
                        edit = Some((camera.id.clone(), edit_button.id));
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
                    .small_button(RichText::new(aspect.label()).small().color(theme::ACCENT))
                    .on_hover_text("How the picture is fitted into its tile");
                self.nav.item(&aspect_button);
                self.focus_names.insert(aspect_button.id, "Display aspect");
                if aspect_button.clicked() {
                    aspect_change = Some((slot, aspect.next()));
                }
                let address = theme::truncate(&camera.masked_uri(StreamKind::Main), 26);
                ui.label(RichText::new(address).small().monospace().color(theme::TEXT_DIM));
                ui.label(RichText::new(camera.origin.label()).small().color(theme::TEXT_DIM));
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
        if let Some((id, name, from)) = ask_remove {
            self.remove_confirm = Some(RemoveConfirm { id, name, from });
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
                    self.flash("no sub stream pattern recognised", ToastKind::Error);
                }
            }
        }
        if let Some((id, from)) = edit {
            let draft = self
                .config
                .cameras
                .iter()
                .find(|camera| camera.id == id)
                .map(CameraDraft::from_source);
            if let Some(draft) = draft {
                self.camera_edit = Some(CameraEdit { id, draft, from });
            }
        }
        if let Some((slot, transport)) = transport_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.transport = transport;
            }
            // The transport is part of the schedule plan, so the channel is
            // reopened with the new one on the next sync.
            self.needs_sync = true;
            dirty = true;
        }

        // Rearranging begins here, and changes nothing yet: the mode works on a
        // copy of the order and the configuration is written only when it is
        // confirmed. Hidden with fewer than two cameras - there is nothing to
        // arrange. The identifier is fixed, so the button that opens the mode
        // can be given the focus back when the mode closes.
        if self.config.cameras.len() >= 2 {
            ui.add_space(theme::space::M);
            let trigger =
                ui.push_id("xgview-reorder-trigger", |ui| ui.button("Reorder cameras")).inner;
            self.nav.item(&trigger);
            self.name(&trigger, "Reorder cameras");
            self.reorder_trigger_anchor = Some(trigger.id);
            if trigger.clicked() {
                self.reorder =
                    Some(self.config.cameras.iter().map(|camera| camera.id.clone()).collect());
                self.reorder_focus = true;
                self.flash("Reorder: Up / Down to move · Confirm to apply · Back to cancel", ToastKind::Info);
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
        for (slot, id) in order.iter().enumerate() {
            let label = self
                .config
                .find_camera(id)
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
                        "Move down",
                        ROW_ICON,
                    );
                    let up = icons::button_at(
                        ui,
                        Id::new(id).with("up"),
                        Icon::Up,
                        false,
                        first,
                        "Move up",
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
                        self.name(&up, "Move camera up");
                        if up.is_pointer_button_down_on() {
                            up.request_focus();
                        }
                    }
                    if !bottom {
                        self.nav.item(&down);
                        self.name(&down, "Move camera down");
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
        let Some(CameraEdit { id, mut draft, from }) = self.camera_edit.take() else {
            return false;
        };
        let mut save = false;
        let mut cancel = false;

        // A modal: centred, and its backdrop swallows the presses that would
        // otherwise reach the panel it was opened from. It is left only by its
        // own buttons (or Back) - a press on the backdrop does nothing.
        egui::Modal::new(Id::new("xgview-edit-camera")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.label(RichText::new("Edit camera").heading());
            ui.add_space(theme::space::S);
            self.nav.open("edit-body");
            let fields = dialogs::camera_fields(ui, &mut self.nav, "edit-camera", &mut draft);
            // The dialog opens on the first control of the form.
            self.camera_edit_anchor = fields.first;
            if fields.inferred {
                ui.label(RichText::new("sub stream url derived from the main url").small().color(theme::LIVE));
            }
            ui.add_space(theme::space::M);
            ui.horizontal(|ui| {
                ui.add_space(Self::center_offset(ui, &["Save", "Cancel"]));
                if self.nav.tracked(ui.button("Save")).clicked() {
                    save = true;
                }
                if self.nav.tracked(ui.button("Cancel")).clicked() {
                    cancel = true;
                }
            });
            self.nav.close();
        });

        if save {
            match draft.build() {
                Ok(source) => {
                    // Only what the form shows is written back. The entry keeps
                    // its identifier, its origin, its transport and everything
                    // else the form has no field for - the display aspect among
                    // them - so an edit to a name or a url cannot reset them.
                    match self.config.cameras.iter_mut().find(|camera| camera.id == id) {
                        Some(camera) => camera.apply_edit(&source),
                        None => {
                            let mut source = source;
                            source.id = id;
                            self.config.upsert_camera(source);
                        }
                    }
                    self.needs_sync = true;
                    self.camera_edit_anchor = None;
                    // The focus goes back to the button the window was opened
                    // from, not to the first control of the panel.
                    self.restore_focus = Some(from);
                    return true;
                }
                Err(error) => self.flash(error, ToastKind::Error),
            }
        }
        if !cancel {
            self.camera_edit = Some(CameraEdit { id, draft, from });
        } else {
            self.camera_edit_anchor = None;
            self.restore_focus = Some(from);
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
        let Some(RemoveConfirm { id, name, from }) = self.remove_confirm.take() else {
            return false;
        };
        let mut remove = false;
        let mut cancel = false;

        // A modal, like the edit window: centred, and nothing behind it answers.
        // Its own buttons (or Back) are the only way out.
        egui::Modal::new(Id::new("xgview-remove-camera")).show(ctx, |ui| {
            // The question is its own column of the navigation layer, as the
            // edit window is: without it the two answers are not registered at
            // all, and the arrows are left to egui's geometric walk - which the
            // panel behind then takes away from them.
            self.nav.open("confirm-body");
            ui.set_width(380.0);
            ui.label(RichText::new(format!("Remove \"{name}\"?")).heading());
            ui.label(
                RichText::new("It leaves the wall and the configuration; the camera itself is untouched.")
                    .small()
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(theme::space::M);
            ui.horizontal(|ui| {
                ui.add_space(Self::center_offset(ui, &["Keep", "Remove"]));
                let keep_button = self.nav.tracked(ui.button("Keep"));
                // The dialog opens on the answer that changes nothing.
                self.remove_confirm_anchor = Some(keep_button.id);
                if keep_button.clicked() {
                    cancel = true;
                }
                if self.nav.tracked(ui.button(RichText::new("Remove").color(theme::ERROR))).clicked() {
                    remove = true;
                }
            });
            self.nav.close();
        });

        if remove {
            if self.config.remove_camera(&id) {
                // The row is gone, so there is no button to hand the focus
                // back to; the panel anchor picks it up.
                self.needs_sync = true;
                self.remove_confirm_anchor = None;
                return true;
            }
        }
        if !cancel {
            self.remove_confirm = Some(RemoveConfirm { id, name, from });
        } else {
            self.remove_confirm_anchor = None;
            self.restore_focus = Some(from);
        }
        false
    }

    /// Version, decoder, and where the configuration lives.
    fn settings_about(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("{} {}", monitor_core::APP_DISPLAY_NAME, env!("CARGO_PKG_VERSION")));
        ui.label(format!("author: {}", monitor_core::APP_AUTHOR));
        ui.label(format!(
            "decoder: {} ({})",
            self.decoder,
            if self.hardware_decoder { "hardware decoding available" } else { "software decoding only" }
        ));
        ui.label(format!("config: {}", self.config_path.display()));
        ui.add_space(theme::space::M);
        ui.label(RichText::new("DPAD, or the arrow keys: move · Enter: 1x1 · Esc or Back: back · PgUp / PgDn: page · F1: settings · F2: devices · F11: full screen").small().color(theme::TEXT_DIM));
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
                // The ring is a cursor, and in full screen with the bars faded
                // out there is nothing to move it with: it would be marking a
                // channel nobody is choosing between.
                focused: interactive && self.chrome.visible && *index == focus,
                interactive,
                dim: !interactive,
                osd: self.config.osd,
                time: self.time,
                aspect: index
                    .and_then(|index| cameras.get(index))
                    .map(|camera| camera.aspect)
                    .unwrap_or_default(),
            };
            let response = grid::paint(ui, rect, &tile);
            if interactive {
                if response.clicked() {
                    actions.focus = *index;
                }
                if response.double_clicked() {
                    actions.zoom = *index;
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
                "No camera configured",
                "Press F2 — or click “Add devices” — to scan the network for ONVIF cameras",
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
        let actions = self.paint_view(ui, wall, &cameras, &view, dx, true, 2);

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
                self.turn_page(true);
            } else if self.slide.drag >= threshold {
                self.turn_page(false);
            }
        }
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
        }
        if let Some(index) = actions.zoom {
            self.zoom_to(index);
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
        egui::Area::new(Id::new("xgview-toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -56.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new(&toast.text).color(color));
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
                        ui.label(RichText::new("Press BACK again to quit").size(18.0).strong());
                    });
            });
    }
}

impl eframe::App for XgViewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.time = ctx.input(|input| input.time);

        // Anything the soft keyboard typed has to be in the input queue before
        // a widget is drawn: a text field reads the events of the frame it is
        // drawn in, so a keystroke handed over later would be a frame behind.
        #[cfg(target_os = "android")]
        crate::keyboard::drain(ctx);

        if ctx.input(|input| input.viewport().close_requested()) {
            self.save_now();
            self.manager.shutdown();
        }

        // Read before the keys are handed out: `handle_keys` takes the ones the
        // application acts on out of the queue, and the bars still have to know
        // a viewer was there. See `viewer_active`.
        self.chrome.input_seen = Self::viewer_active(ctx);

        self.poll_events();
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
        self.handle_keys(ctx);
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

        // The right of a touch screen is not ours to draw controls in: see
        // `EDGE_MARGIN`. The margin is set on the panel rather than inside the
        // row so that it holds wherever the row is laid out from.
        //
        // In full screen the bars fade out when the wall is left alone; see
        // `chrome_visible`.
        if self.chrome_visible() {
            let mut margin = egui::Frame::side_top_panel(&ctx.style()).inner_margin;
            margin.right = EDGE_MARGIN;
            egui::TopBottomPanel::top("xgview-toolbar")
                .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(margin))
                .show(ctx, |ui| self.toolbar(ui));
            egui::TopBottomPanel::bottom("xgview-status").show(ctx, |ui| self.status_bar(ui));
        } else {
            // The bar is gone, and with it anything it could hand the remote's
            // focus to: keeping the list would point the focus at a control
            // that is not on screen.
            self.toolbar_items.clear();
            // A wall nobody is operating is a picture, and a pointer sitting on
            // it is the last thing left that is not part of it. Any input brings
            // the bars back, and the pointer with them.
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
        if self.show_settings {
            egui::SidePanel::right("xgview-settings")
                .default_width(SETTINGS_WIDTH)
                .min_width(SETTINGS_MIN_WIDTH)
                .show(ctx, |ui| self.settings_panel(ui));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(theme::BACKGROUND))
            .show(ctx, |ui| self.draw_grid(ui));

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
        }

        if self.camera_edit_window(ctx) {
            self.mark_dirty();
        }
        if self.remove_confirm_window(ctx) {
            self.mark_dirty();
        }

        self.draw_toast(ctx);
        self.draw_exit_hint(ctx);
        self.autosave();

        // A panel that appears takes the remote's focus, once: without it the
        // arrows would go on walking the wall behind it, which is the one thing
        // a viewer cannot see happening. See `Handoff`.
        if self.settings_handoff.entering(self.show_settings) {
            if let Some(id) = self.settings_anchor {
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
                ctx.memory_mut(|memory| memory.request_focus(id));
            }
        }
        if self.remove_confirm_handoff.entering(self.remove_confirm.is_some()) {
            self.swallow_enter = 10;
            if let Some(id) = self.remove_confirm_anchor {
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
        if ctx.memory(|memory| memory.focused()).is_none() {
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
            if Self::typing(ctx) {
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
        if let Some(focused) = ctx.memory(|memory| memory.focused()) {
            if !self.nav.owns_now(focused) {
                ctx.memory_mut(|memory| memory.surrender_focus(focused));
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
            crate::keyboard::set_wanted(Self::typing(ctx), focused);
        }

        let animation = self.needs_animation();
        ctx.request_repaint_after(if animation { LIVE_REPAINT } else { Duration::from_millis(500) });
    }
}
