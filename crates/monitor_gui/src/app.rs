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
use monitor_core::model::{ConnectionState, StreamKind};
use monitor_core::pipeline::{ChannelManager, StreamEvent};
use monitor_core::scheduler::Scheduler;
use monitor_core::{CameraSource, Direction};

use crate::dialogs::{self, BackgroundEvent, DiscoveryUi, Tab};
use crate::grid::{self, Tile, TileActions};
use crate::theme;
use crate::video::{VideoRenderer, VideoSurface};
use crate::RunOptions;

/// Debounce applied before writing `config.json`.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(700);

/// Repaint interval while at least one channel is showing video.
const LIVE_REPAINT: Duration = Duration::from_millis(33);

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
    show_settings: bool,
    show_stats: bool,
    discovery: DiscoveryUi,
    slide: Slide,
    needs_sync: bool,
    dirty: bool,
    dirty_since: Option<Instant>,
    toast: Option<Toast>,
    fullscreen: bool,
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
            show_settings: false,
            show_stats: true,
            discovery,
            slide: Slide::default(),
            needs_sync: true,
            dirty: false,
            dirty_since: None,
            toast: None,
            fullscreen,
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

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.dirty_since = Some(Instant::now());
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

    fn navigate(&mut self, dir: Direction) {
        let previous_page = self.scheduler.page();
        let previous_focus = self.scheduler.focus();
        let zoomed = self.scheduler.is_zoomed();
        let outcome = self.scheduler.navigate(dir);
        if matches!(outcome, NavigateOutcome::Blocked) {
            return;
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
    }

    fn back(&mut self, ctx: &egui::Context) {
        // BACK / Esc leaves the magnified viewport, then the full screen mode.
        if self.scheduler.is_zoomed() {
            self.zoom_out();
        } else if self.discovery.open {
            self.discovery.open = false;
        } else if self.fullscreen {
            self.set_fullscreen(ctx, false);
        }
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
                    if !cameras.is_empty() {
                        for camera in cameras {
                            self.config.upsert_camera(camera.clone());
                        }
                        touched_config = true;
                    }
                    toast = Some((
                        format!("{} camera(s) imported from Surveillance Station", cameras.len()),
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

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let keys = ctx.input_mut(|input| {
            let none = Modifiers::NONE;
            let mut keys = Keys::default();
            keys.left = input.consume_key(none, Key::ArrowLeft);
            keys.right = input.consume_key(none, Key::ArrowRight);
            keys.up = input.consume_key(none, Key::ArrowUp);
            keys.down = input.consume_key(none, Key::ArrowDown);
            keys.enter = input.consume_key(none, Key::Enter) || input.consume_key(none, Key::Space);
            keys.back = input.consume_key(none, Key::Escape) || input.consume_key(none, Key::Backspace);
            keys.page_prev = input.consume_key(none, Key::PageUp);
            keys.page_next = input.consume_key(none, Key::PageDown);
            keys.settings = input.consume_key(none, Key::F1);
            keys.devices = input.consume_key(none, Key::F2);
            keys.fullscreen = input.consume_key(none, Key::F11);
            keys.layout = [
                input.consume_key(none, Key::Num1),
                input.consume_key(none, Key::Num2),
                input.consume_key(none, Key::Num3),
                input.consume_key(none, Key::Num4),
            ];
            keys
        });

        if keys.left {
            self.navigate(Direction::Left);
        }
        if keys.right {
            self.navigate(Direction::Right);
        }
        if keys.up {
            self.navigate(Direction::Up);
        }
        if keys.down {
            self.navigate(Direction::Down);
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
        if keys.page_prev {
            self.turn_page(false);
        }
        if keys.page_next {
            self.turn_page(true);
        }
        if keys.settings {
            self.show_settings = !self.show_settings;
        }
        if keys.devices {
            self.discovery.open = !self.discovery.open;
        }
        if keys.fullscreen {
            let enabled = !self.fullscreen;
            self.set_fullscreen(ctx, enabled);
        }
        for (slot, pressed) in keys.layout.iter().enumerate() {
            if *pressed {
                if let Some(layout) = GridLayout::ALL.get(slot) {
                    self.set_layout(*layout);
                }
            }
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
        ui.horizontal(|ui| {
            ui.label(RichText::new(monitor_core::APP_DISPLAY_NAME).heading().strong());
            ui.separator();

            for layout in GridLayout::ALL {
                let selected = self.scheduler.layout() == layout && !self.scheduler.is_zoomed();
                if ui.selectable_label(selected, layout.label()).clicked() {
                    self.set_layout(layout);
                }
            }

            ui.separator();
            let info = self.scheduler.page_info();
            if ui.add_enabled(info.has_previous(), egui::Button::new("◀")).clicked() {
                self.turn_page(false);
            }
            ui.label(format!("page {} / {}", info.page + 1, info.page_count));
            if ui.add_enabled(info.has_next(), egui::Button::new("▶")).clicked() {
                self.turn_page(true);
            }

            ui.separator();
            if self.scheduler.is_zoomed() {
                if ui.button("Back to grid (Esc)").clicked() {
                    self.zoom_out();
                }
            } else if ui.button("Zoom 1x1 (Enter)").clicked() {
                self.zoom_in();
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.selectable_label(self.show_settings, "Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                if ui.selectable_label(self.discovery.open, "Add devices").clicked() {
                    self.discovery.open = !self.discovery.open;
                }
                let live = self.channels.values().filter(|channel| channel.state.is_live()).count();
                ui.label(
                    RichText::new(format!("{live}/{} live", self.config.enabled_count()))
                        .color(if live > 0 { theme::LIVE } else { theme::TEXT_DIM }),
                );
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let info = self.scheduler.page_info();
            ui.label(format!("{} channel(s)", info.total));
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
            if let Some(toast) = &self.toast {
                if toast.at.elapsed() < Duration::from_secs(6) {
                    ui.separator();
                    let color = if toast.kind == ToastKind::Error { theme::ERROR } else { theme::LIVE };
                    ui.label(RichText::new(theme::truncate(&toast.text, 96)).color(color));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("decoder: {}", self.decoder)).small().color(theme::TEXT_DIM));
                ui.label(RichText::new(self.config_path.display().to_string()).small().color(theme::TEXT_DIM));
            });
        });
    }

    fn settings_body(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;

        ui.heading("Display");
        ui.horizontal_wrapped(|ui| {
            for layout in GridLayout::ALL {
                let selected = self.scheduler.layout() == layout;
                if ui.selectable_label(selected, layout.label()).clicked() {
                    self.set_layout(layout);
                }
            }
        });
        ui.checkbox(&mut self.show_stats, "Show fps and bitrate on the tiles");
        let mut fullscreen = self.fullscreen;
        if ui.checkbox(&mut fullscreen, "Full screen (F11)").changed() {
            self.set_fullscreen(ui.ctx(), fullscreen);
        }
        let mut start_fullscreen = self.config.start_fullscreen;
        if ui.checkbox(&mut start_fullscreen, "Open in full screen at start-up").changed() {
            self.config.start_fullscreen = start_fullscreen;
            dirty = true;
        }

        ui.separator();
        ui.heading("Start-up");
        let supported = self.autostart.supported;
        let mut enabled = self.config.autostart;
        ui.add_enabled_ui(supported, |ui| {
            if ui.checkbox(&mut enabled, "Start with the system boot").changed() {
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

        ui.separator();
        ui.heading("Streams");
        ui.label(
            RichText::new("Multi grid pulls the sub stream, 1x1 (or a magnified viewport) pulls the main stream.")
                .small()
                .color(theme::TEXT_DIM),
        );
        let reconnect = &mut self.config.reconnect;
        ui.horizontal(|ui| {
            ui.label("reconnect delay");
            dirty |= ui.add(egui::DragValue::new(&mut reconnect.initial_delay_ms).range(100..=10_000).suffix(" ms")).changed();
            ui.label("max");
            dirty |= ui.add(egui::DragValue::new(&mut reconnect.max_delay_ms).range(1_000..=300_000).suffix(" ms")).changed();
        });
        ui.horizontal(|ui| {
            ui.label("factor");
            dirty |= ui.add(egui::DragValue::new(&mut reconnect.multiplier).speed(0.05).range(1.0..=5.0)).changed();
            ui.label("jitter");
            dirty |= ui.add(egui::DragValue::new(&mut reconnect.jitter).speed(0.02).range(0.0..=1.0)).changed();
            ui.label("attempts (0 = forever)");
            dirty |= ui.add(egui::DragValue::new(&mut reconnect.max_attempts).range(0..=100)).changed();
        });
        ui.label(RichText::new("a change applies to the connections opened afterwards").small().color(theme::TEXT_DIM));

        ui.separator();
        ui.heading("Decoding");
        let mut prefer_hardware = self.config.prefer_hardware_decode;
        if ui.checkbox(&mut prefer_hardware, "Prefer hardware decoding").changed() {
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
        ui.label(
            RichText::new(if self.hardware_decoder {
                "Pictures are decoded on the GPU where the machine offers a decoder for the stream, and on the CPU everywhere else. Each tile shows which one it got."
            } else {
                "This build has no hardware decoder: the CPU decodes, whatever this setting says."
            })
            .small()
            .color(theme::TEXT_DIM),
        );

        ui.separator();
        ui.heading(format!("Cameras ({})", self.config.cameras.len()));
        ui.horizontal(|ui| {
            if ui.button("Add devices…").clicked() {
                self.discovery.open = true;
            }
            if ui.button("Add manually…").clicked() {
                self.discovery.open = true;
                self.discovery.tab = Tab::Manual;
            }
        });
        ui.label(RichText::new(dialogs::discovery_summary(&self.config.discovery)).small().color(theme::TEXT_DIM));

        let mut toggle: Option<(usize, bool)> = None;
        let mut remove: Option<String> = None;
        let mut infer: Option<usize> = None;
        for (slot, camera) in self.config.cameras.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let mut camera_enabled = camera.enabled;
                if ui.checkbox(&mut camera_enabled, "").changed() {
                    toggle = Some((slot, camera_enabled));
                }
                ui.label(RichText::new(camera.short_label(20)).strong());
                if let Some(sub) = camera.rtsp_sub.as_deref() {
                    let inferred = sub == monitor_core::model::infer_sub_stream(&camera.rtsp_main).unwrap_or_default();
                    ui.label(
                        RichText::new(if inferred { "sub: derived" } else { "sub: set" })
                            .small()
                            .color(theme::ACCENT),
                    );
                } else {
                    if ui.small_button("infer sub").clicked() {
                        infer = Some(slot);
                    }
                }
                ui.label(RichText::new(camera.masked_uri(StreamKind::Main)).small().color(theme::TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("remove").clicked() {
                        remove = Some(camera.id.clone());
                    }
                    ui.label(RichText::new(camera.origin.label()).small().color(theme::TEXT_DIM));
                });
            });
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
        if let Some(id) = remove {
            if self.config.remove_camera(&id) {
                self.needs_sync = true;
                dirty = true;
            }
        }

        ui.separator();
        ui.heading("About");
        ui.label(format!("{} {}", monitor_core::APP_DISPLAY_NAME, env!("CARGO_PKG_VERSION")));
        ui.label(format!("author: {}", monitor_core::APP_AUTHOR));
        ui.label(format!(
            "decoder: {} ({})",
            self.decoder,
            if self.hardware_decoder { "hardware decoding available" } else { "software decoding only" }
        ));
        ui.label(format!("config: {}", self.config_path.display()));
        ui.label(RichText::new("DPAD / arrows: move · Enter: 1x1 · Esc: back · PgUp/PgDn: page · F1: settings · F2: devices · F11: full screen").small().color(theme::TEXT_DIM));

        if dirty {
            self.mark_dirty();
        }
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
            let rect = grid::tile_rect(origin, view.layout, cell, grid::GAP);
            if rect.max.x < area.min.x - 4.0 || rect.min.x > area.max.x + 4.0 {
                continue;
            }
            let tile = Tile {
                id: Id::new(("xgview-tile", tag, cell, index.unwrap_or(usize::MAX))),
                index: *index,
                camera: index.and_then(|index| cameras.get(index)),
                channel: index.and_then(|index| self.channels.get(&index)),
                video: index.and_then(|index| self.textures.get(&index)).map(|entry| entry.surface),
                focused: interactive && *index == focus,
                interactive,
                dim: !interactive,
                show_stats: self.show_stats,
                time: self.time,
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

        let view = self.current_view(total);

        // Page sliding out, or the neighbour page revealed by a finger drag.
        if let Some(outgoing) = self.slide.outgoing.clone() {
            let dx = (self.slide.offset - self.slide.dir) * area.width() + self.slide.drag;
            self.paint_view(ui, area, &cameras, &outgoing, dx, false, 3);
        } else if self.slide.dragging && self.slide.drag.abs() > 1.0 {
            let forward = self.slide.drag < 0.0;
            if let Some(neighbour) = self.neighbour_view(forward, total) {
                let dx = if forward { self.slide.drag + area.width() } else { self.slide.drag - area.width() };
                self.paint_view(ui, area, &cameras, &neighbour, dx, false, 3);
            }
        }

        let dx = self.slide.offset * area.width() + self.slide.drag;
        let actions = self.paint_view(ui, area, &cameras, &view, dx, true, 2);

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
}

impl eframe::App for XgViewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.time = ctx.input(|input| input.time);

        if ctx.input(|input| input.viewport().close_requested()) {
            self.save_now();
            self.manager.shutdown();
        }

        self.poll_events();
        self.handle_keys(ctx);
        self.advance_animations(ctx);
        self.sync();
        self.upload_frames();

        egui::TopBottomPanel::top("xgview-toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::TopBottomPanel::bottom("xgview-status").show(ctx, |ui| self.status_bar(ui));
        if self.show_settings {
            egui::SidePanel::right("xgview-settings")
                .default_width(360.0)
                .min_width(300.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        self.settings_body(ui);
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(theme::BACKGROUND))
            .show(ctx, |ui| self.draw_grid(ui));

        if self.discovery.open {
            let handle = self.handle.clone();
            let events = self.events_tx.clone();
            if dialogs::add_devices_window(ctx, &mut self.discovery, &mut self.config, &handle, &events) {
                self.needs_sync = true;
                self.mark_dirty();
            }
        }

        self.draw_toast(ctx);
        self.autosave();

        let animation = self.needs_animation();
        ctx.request_repaint_after(if animation { LIVE_REPAINT } else { Duration::from_millis(500) });
    }
}
