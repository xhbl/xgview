//! Device discovery / camera management dialogs.
//!
//! The dialog never blocks: every scan, ONVIF import and Synology request runs
//! on the tokio runtime and reports back through a `crossbeam-channel`, which
//! the render loop drains once per frame.

use std::sync::Arc;

use crossbeam_channel::Sender;
use egui::{Align, Layout, RichText};
use tokio::runtime::Handle;

use monitor_core::config::{AppConfig, DiscoveryConfig, FrigateConfig, Go2rtcConfig, SynologyConfig};
use monitor_core::discovery::{
    DiscoveryEvent, DiscoveryReport, DiscoveryService, DiscoveredDevice, FrigateImport, PortHit,
    ProgressCallback,
};
use monitor_core::model::CameraSource;

use crate::controls;
use crate::nav::{Kind, Nav};
use crate::theme;

/// Key under which the text field being typed into leaves a copy of its text
/// for the floating input box (Android); see [`text_field`].
pub const MIRROR: &str = "xg-input-mirror";

/// What the floating input box shows: the focused text field, as of this frame.
#[derive(Clone)]
pub struct Mirror {
    pub id: egui::Id,
    pub label: String,
    pub text: String,
    pub password: bool,
    /// Whether the field holds several lines - what the soft keyboard's action
    /// key has to be built from; see `MainActivity.onCreateInputConnection`.
    pub multiline: bool,
}

/// A text field of the navigation layer.
///
/// Every text field of the application is drawn through this, rather than
/// through `nav.tracked_kind(Kind::Text, ui.add(TextEdit...))`, because on
/// Android the soft keyboard covers the lower part of the screen and the field
/// may be underneath it. The string lives in the caller's state and egui keeps
/// only the caret, so the only way for the floating input box to show what is
/// being typed is for the field to leave a copy of it here. `build` makes the
/// `TextEdit` out of the string (hint, width, `password(true)` and so on);
/// `label`, `password` and `multiline` are only what the floating box and the
/// soft keyboard read.
///
/// The copy keeps being refreshed once the field is the mirrored one, even on
/// the frame it loses the focus: a press on the floating box's buttons is a
/// click elsewhere to egui, and the box has to be drawn that frame to see it.
pub fn text_field(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    text: &mut String,
    label: &str,
    password: bool,
    multiline: bool,
    build: impl for<'a> FnOnce(&'a mut String) -> egui::TextEdit<'a>,
) -> egui::Response {
    let response = nav.tracked_kind(Kind::Text, ui.add(build(text)));
    #[cfg(target_os = "android")]
    {
        let key = egui::Id::new(MIRROR);
        let current = ui.ctx().data(|data| data.get_temp::<Mirror>(key));
        if response.has_focus() || current.is_some_and(|mirror| mirror.id == response.id) {
            let mirror = Mirror { id: response.id, label: label.to_owned(), text: text.clone(), password, multiline };
            ui.ctx().data_mut(|data| data.insert_temp(key, mirror));
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = (label, password, multiline);
    response
}

/// Everything a background job reports to the UI thread.
#[derive(Debug, Clone)]
pub enum BackgroundEvent {
    /// Progress of a running discovery.
    Discovery(DiscoveryEvent),
    /// A discovery run completed.
    DiscoveryDone(DiscoveryReport),
    /// A discovery run failed.
    DiscoveryFailed(String),
    /// One device was resolved to a camera.
    Imported(Box<CameraSource>),
    /// An ONVIF import failed.
    ImportFailed { name: String, error: String },
    /// The Synology import returned cameras.
    SynologyDone(Vec<CameraSource>),
    /// The Synology import failed.
    SynologyFailed(String),
    /// The Frigate fetch returned its two groups.
    FrigateDone(FrigateImport),
    /// The Frigate fetch failed.
    FrigateFailed(String),
    /// The go2rtc fetch returned its streams.
    Go2rtcDone(Vec<CameraSource>),
    /// The go2rtc fetch failed.
    Go2rtcFailed(String),
}

/// Tab of the "Add devices" window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Onvif,
    Manual,
    Synology,
    Frigate,
    Go2rtc,
}

/// Editable mirror of [`DiscoveryConfig`] used by the scan form.
#[derive(Debug, Clone)]
pub struct ScanForm {
    pub broadcast: bool,
    pub subnet_scan: bool,
    pub ip_ranges: String,
    pub tcp_probe: bool,
    pub tcp_ports: String,
    pub timeout_ms: u64,
    pub concurrency: usize,
    pub username: String,
    pub password: String,
}

impl Default for ScanForm {
    fn default() -> Self {
        Self::from_config(&DiscoveryConfig::default())
    }
}

impl ScanForm {
    pub fn from_config(config: &DiscoveryConfig) -> Self {
        Self {
            broadcast: config.broadcast,
            subnet_scan: config.subnet_scan,
            ip_ranges: config.ip_ranges.join("\n"),
            tcp_probe: config.tcp_probe,
            tcp_ports: config
                .tcp_ports
                .iter()
                .map(|port| port.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            timeout_ms: config.probe_timeout_ms,
            concurrency: config.concurrency,
            username: config.onvif_username.clone().unwrap_or_default(),
            password: config.onvif_password.clone().unwrap_or_default(),
        }
    }

    /// Merges the form into a full [`DiscoveryConfig`].
    pub fn to_config(&self) -> DiscoveryConfig {
        DiscoveryConfig {
            broadcast: self.broadcast,
            subnet_scan: self.subnet_scan,
            ip_ranges: self
                .ip_ranges
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            tcp_probe: self.tcp_probe,
            tcp_ports: parse_ports(&self.tcp_ports),
            probe_timeout_ms: self.timeout_ms,
            concurrency: self.concurrency,
            onvif_username: Some(self.username.trim().to_string()).filter(|user| !user.is_empty()),
            onvif_password: Some(self.password.clone()).filter(|password| !password.is_empty()),
        }
    }

    /// Number of addresses the unicast / TCP scan will probe.
    pub fn target_count(&self) -> usize {
        monitor_core::discovery::parse_targets_multi(
            &self
                .ip_ranges
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
        .len()
    }
}

fn parse_ports(text: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = text
        .split([',', ' ', ';'])
        .filter_map(|item| item.trim().parse::<u16>().ok())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    if ports.is_empty() {
        ports.push(554);
    }
    ports
}

/// Manually entered camera.
#[derive(Debug, Clone, Default)]
pub struct CameraDraft {
    pub name: String,
    pub main: String,
    pub sub: String,
    pub username: String,
    pub password: String,
    pub infer_sub: bool,
}

impl CameraDraft {
    /// Validates the form and builds a camera.
    pub fn build(&self) -> Result<CameraSource, String> {
        let main = self.main.trim();
        if main.is_empty() {
            return Err(monitor_i18n::tr("dialog-an-rtsp-url-required"));
        }
        if !main.to_ascii_lowercase().starts_with("rtsp://") {
            return Err(monitor_i18n::tr("dialog-url-must-start"));
        }
        let mut source = CameraSource::new(String::new(), main);
        source.name = if self.name.trim().is_empty() {
            if source.host.is_empty() {
                main.to_string()
            } else {
                source.host.clone()
            }
        } else {
            self.name.trim().to_string()
        };
        if !self.username.trim().is_empty() {
            source.username = Some(self.username.trim().to_string());
            source.password = Some(self.password.clone());
        }
        let sub = self.sub.trim();
        if !sub.is_empty() {
            source.rtsp_sub = Some(sub.to_string());
        } else if self.infer_sub {
            source.apply_sub_inference();
        }
        Ok(source)
    }

    pub fn reset(&mut self) {
        *self = Self { infer_sub: true, ..Default::default() };
    }

    /// The form filled in from a camera already configured, for editing it.
    pub fn from_source(camera: &CameraSource) -> Self {
        Self {
            name: camera.name.clone(),
            main: camera.rtsp_main.clone(),
            sub: camera.rtsp_sub.clone().unwrap_or_default(),
            username: camera.username.clone().unwrap_or_default(),
            password: camera.password.clone().unwrap_or_default(),
            // A sub stream already there is not to be overwritten by inference.
            infer_sub: camera.rtsp_sub.is_none(),
        }
    }
}

/// State of the "Add devices" window.
#[derive(Debug, Default)]
pub struct DiscoveryUi {
    pub open: bool,
    pub tab: Tab,
    pub scan: ScanForm,
    pub running: bool,
    pub phase: String,
    pub devices: Vec<DiscoveredDevice>,
    pub ports: Vec<PortHit>,
    /// Name of the device currently being resolved.
    pub busy: Option<String>,
    /// What the last action reported, tagged with the tab it came from.
    ///
    /// One tab's status is not another's: the wording names the tab it belongs
    /// to ("on the NAS", "in Frigate"), so it is only drawn on that tab.
    pub error: Option<(Tab, String)>,
    pub message: Option<(Tab, String)>,
    pub manual: CameraDraft,
    pub synology_busy: bool,
    /// Cameras the last Synology fetch returned, waiting to be picked.
    ///
    /// Kept rather than imported on arrival, so the viewer chooses what comes
    /// onto the wall - see [`synology_tab`].
    pub synology_cameras: Vec<CameraSource>,
    /// First control of the window, so that a remote control can be handed the
    /// focus as the window opens rather than leaving it on the wall behind.
    pub focus_anchor: Option<egui::Id>,
    /// The Synology port, as its text box shows it. `None` until that tab is
    /// first drawn, so the box opens on the configured port. Only digits are
    /// kept; [`SynologyConfig::port`] follows it while the text names a valid
    /// port.
    pub synology_port: Option<String>,
    pub frigate_busy: bool,
    /// Cameras the last Frigate fetch returned, waiting to be picked.
    pub frigate_cameras: Vec<CameraSource>,
    /// go2rtc streams no Frigate camera references.
    pub frigate_leftovers: Vec<CameraSource>,
    /// The Frigate port, as its text box shows it. See [`DiscoveryUi::synology_port`].
    pub frigate_port: Option<String>,
    pub go2rtc_busy: bool,
    /// Streams the last go2rtc fetch returned, waiting to be picked.
    pub go2rtc_cameras: Vec<CameraSource>,
    /// The go2rtc API port, as its text box shows it.
    pub go2rtc_port: Option<String>,
}

impl DiscoveryUi {
    pub fn new(config: &DiscoveryConfig) -> Self {
        Self { scan: ScanForm::from_config(config), manual: CameraDraft { infer_sub: true, ..Default::default() }, ..Default::default() }
    }

    fn push_device(&mut self, device: &DiscoveredDevice) {
        let host = device.host();
        if let Some(host) = &host {
            if self.devices.iter().any(|known| known.host().as_deref() == Some(host.as_str())) {
                return;
            }
        }
        self.devices.push(device.clone());
    }

    fn push_port(&mut self, hit: &PortHit) {
        if self.ports.iter().any(|known| known.address == hit.address && known.port == hit.port) {
            return;
        }
        self.ports.push(hit.clone());
    }

    /// Records what an action of `tab` reported.
    fn note(&mut self, tab: Tab, message: impl Into<String>) {
        self.message = Some((tab, message.into()));
    }

    /// Records what an action of `tab` got wrong.
    fn fail(&mut self, tab: Tab, error: impl Into<String>) {
        self.error = Some((tab, error.into()));
    }

    /// Drops what `tab` last reported, leaving the other tabs' status alone.
    fn clear_status(&mut self, tab: Tab) {
        if self.message.as_ref().is_some_and(|(other, _)| *other == tab) {
            self.message = None;
        }
        if self.error.as_ref().is_some_and(|(other, _)| *other == tab) {
            self.error = None;
        }
    }

    /// Applies an event produced by a background job.
    pub fn apply(&mut self, event: BackgroundEvent) {
        match event {
            BackgroundEvent::Discovery(event) => match event {
                DiscoveryEvent::Phase(phase) => self.phase = phase,
                DiscoveryEvent::DeviceFound(device) => self.push_device(&device),
                DiscoveryEvent::PortFound(hit) => self.push_port(&hit),
                DiscoveryEvent::Finished { .. } => {}
            },
            BackgroundEvent::DiscoveryDone(report) => {
                for device in &report.devices {
                    self.push_device(device);
                }
                for hit in &report.port_hits {
                    self.push_port(hit);
                }
                self.running = false;
                self.phase.clear();
                self.note(Tab::Onvif, monitor_i18n::tr_args(
                    "dialog-discovery-finished",
                    &[("devices", self.devices.len().into()), ("ports", self.ports.len().into())],
                ));
            }
            BackgroundEvent::DiscoveryFailed(error) => {
                self.running = false;
                self.fail(Tab::Onvif, error);
            }
            BackgroundEvent::Imported(camera) => {
                self.busy = None;
                self.note(Tab::Onvif, monitor_i18n::tr_args(
                    "dialog-imported",
                    &[("name", camera.name.clone().into()), ("address", camera.display_address().into())],
                ));
            }
            BackgroundEvent::ImportFailed { name, error } => {
                self.busy = None;
                self.fail(Tab::Onvif, format!("{name}: {error}"));
            }
            BackgroundEvent::SynologyDone(cameras) => {
                self.synology_busy = false;
                self.note(Tab::Synology, monitor_i18n::tr_args(
                    "dialog-synology-count",
                    &[("count", cameras.len().into())],
                ));
                self.synology_cameras = cameras;
            }
            BackgroundEvent::SynologyFailed(error) => {
                self.synology_busy = false;
                self.fail(Tab::Synology, monitor_i18n::tr_args(
                    "toast-synology-failed",
                    &[("error", error.into())],
                ));
            }
            BackgroundEvent::FrigateDone(import) => {
                self.frigate_busy = false;
                self.note(Tab::Frigate, monitor_i18n::tr_args(
                    "dialog-frigate-count",
                    &[
                        ("count", import.cameras.len().into()),
                        ("extra", import.leftovers.len().into()),
                    ],
                ));
                self.frigate_cameras = import.cameras;
                self.frigate_leftovers = import.leftovers;
            }
            BackgroundEvent::FrigateFailed(error) => {
                self.frigate_busy = false;
                self.fail(Tab::Frigate, monitor_i18n::tr_args("toast-frigate-failed", &[("error", error.into())]));
            }
            BackgroundEvent::Go2rtcDone(cameras) => {
                self.go2rtc_busy = false;
                self.note(Tab::Go2rtc, monitor_i18n::tr_args(
                    "dialog-go2rtc-count",
                    &[("count", cameras.len().into())],
                ));
                self.go2rtc_cameras = cameras;
            }
            BackgroundEvent::Go2rtcFailed(error) => {
                self.go2rtc_busy = false;
                self.fail(Tab::Go2rtc, monitor_i18n::tr_args("toast-go2rtc-failed", &[("error", error.into())]));
            }
        }
    }
}

/// Spawns a full discovery run according to `config`.
pub fn start_discovery(handle: &Handle, events: &Sender<BackgroundEvent>, config: DiscoveryConfig) {
    let service = DiscoveryService::new(config);
    let progress_sender = events.clone();
    let progress: ProgressCallback =
        Arc::new(move |event| {
            let _ = progress_sender.send(BackgroundEvent::Discovery(event));
        });
    let sender = events.clone();
    handle.spawn(async move {
        match service.discover(Some(progress)).await {
            Ok(report) => {
                let _ = sender.send(BackgroundEvent::DiscoveryDone(report));
            }
            Err(err) => {
                let _ = sender.send(BackgroundEvent::DiscoveryFailed(err.to_string()));
            }
        }
    });
}

/// Spawns the ONVIF resolution of one device.
pub fn start_import(
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    config: DiscoveryConfig,
    device: DiscoveredDevice,
) {
    let service = DiscoveryService::new(config);
    let sender = events.clone();
    handle.spawn(async move {
        match service.import_device(&device).await {
            Ok(camera) => {
                let _ = sender.send(BackgroundEvent::Imported(Box::new(camera)));
            }
            Err(err) => {
                let _ = sender.send(BackgroundEvent::ImportFailed { name: device.display_name(), error: err.to_string() });
            }
        }
    });
}

/// Spawns the Synology Surveillance Station import.
pub fn start_synology(handle: &Handle, events: &Sender<BackgroundEvent>, config: SynologyConfig) {
    let service = DiscoveryService::new(DiscoveryConfig::default());
    let sender = events.clone();
    handle.spawn(async move {
        match service.import_synology(config).await {
            Ok(cameras) => {
                let _ = sender.send(BackgroundEvent::SynologyDone(cameras));
            }
            Err(err) => {
                let _ = sender.send(BackgroundEvent::SynologyFailed(err.to_string()));
            }
        }
    });
}

/// Spawns the Frigate camera fetch - login, `/api/config`, grouping and the
/// resolution probes behind it.
pub fn start_frigate(handle: &Handle, events: &Sender<BackgroundEvent>, config: FrigateConfig) {
    let service = DiscoveryService::new(DiscoveryConfig::default());
    let sender = events.clone();
    handle.spawn(async move {
        match service.import_frigate(config).await {
            Ok(import) => {
                let _ = sender.send(BackgroundEvent::FrigateDone(import));
            }
            Err(err) => {
                let _ = sender.send(BackgroundEvent::FrigateFailed(err.to_string()));
            }
        }
    });
}

/// Spawns the standalone go2rtc stream fetch.
pub fn start_go2rtc(handle: &Handle, events: &Sender<BackgroundEvent>, config: Go2rtcConfig) {
    let service = DiscoveryService::new(DiscoveryConfig::default());
    let sender = events.clone();
    handle.spawn(async move {
        match service.import_go2rtc(config).await {
            Ok(cameras) => {
                let _ = sender.send(BackgroundEvent::Go2rtcDone(cameras));
            }
            Err(err) => {
                let _ = sender.send(BackgroundEvent::Go2rtcFailed(err.to_string()));
            }
        }
    });
}

/// Draws the "Add devices" dialog. Returns `true` when the configuration changed.
pub fn add_devices_window(
    ctx: &egui::Context,
    state: &mut DiscoveryUi,
    config: &mut AppConfig,
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    nav: &mut Nav,
) -> bool {
    let mut changed = false;
    let mut close = false;

    // A modal, not a window: it centres itself, and its backdrop swallows the
    // presses that would otherwise land behind it. As a plain window the bar
    // stayed live underneath, so a second press on "Add devices" toggled the
    // dialog shut and the settings panel could be rearranged from under it.
    egui::Modal::new(egui::Id::new("xgview-add-devices")).show(ctx, |ui| {
        ui.set_width(620.0);
        ui.label(RichText::new(monitor_i18n::tr("dialog-add-devices")).heading());
        ui.add_space(theme::space::S);

        if tab_strip(ui, state, nav) {
            close = true;
        }
        ui.separator();

        // The body is one column: Up and Down walk its controls in the
        // order they are drawn, and Up off the first one leaves the scope,
        // which the definition sends back to the tab in use.
        //
        // Left and Right are spent inside the body on purpose. On a remote
        // the form is walked up and down, and sideways belongs to the tab
        // strip; a row of controls drawn side by side is walked as its
        // consecutive stops, one Down apart, never a sideways jump.
        nav.open("dialog-body");
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .auto_shrink([false, false])
            .show(ui, |ui| match state.tab {
                Tab::Onvif => changed |= onvif_tab(ui, state, config, handle, events, nav),
                Tab::Manual => changed |= manual_tab(ui, state, config, nav),
                Tab::Synology => changed |= synology_tab(ui, state, config, handle, events, nav),
                Tab::Frigate => changed |= frigate_tab(ui, state, config, handle, events, nav),
                Tab::Go2rtc => changed |= go2rtc_tab(ui, state, config, handle, events, nav),
            });
        nav.close();
    });

    // Only its own Close (or Back) leaves the dialog. A press on the backdrop
    // does nothing: the form behind is not reachable, and a stray click cannot
    // take the dialog away in the middle of filling it in.
    if close {
        state.open = false;
    }
    changed
}


/// The window's tab strip: draws the three tabs and registers them as a row.
///
/// egui does not give a button the focus when it is clicked - a press only takes
/// the focus away from whatever had it - and the press and the release of one
/// click can land in different frames. The chosen tab therefore takes the focus
/// there and then, and `focus_anchor` records the tab that is *on* - not the
/// first one, so that a window left without a focus comes back to the tab in use
/// instead of snapping away from the viewer's choice.
///
/// The row is a scope of the navigation layer: Left and Right walk the tabs in
/// the order drawn, never by geometry, and Down leaves the row for the body
/// through the scope's exit. Moving the focus onto a tab also selects it, so a
/// viewer arrow-keying across the strip sees the body follow, exactly as it did
/// when a click selected one.
///
/// The dialog's way out - its Close button - ends the same row, so the remote
/// reaches it with the arrows. Returns `true` when it was pressed.
fn tab_strip(ui: &mut egui::Ui, state: &mut DiscoveryUi, nav: &mut Nav) -> bool {
    const TABS: [(Tab, &str); 5] = [
        (Tab::Onvif, "dialog-tab-onvif"),
        (Tab::Manual, "dialog-tab-manual"),
        (Tab::Synology, "dialog-tab-synology"),
        (Tab::Frigate, "dialog-tab-frigate"),
        (Tab::Go2rtc, "dialog-tab-go2rtc"),
    ];

    let mut ids = [egui::Id::NULL; TABS.len()];
    let mut focused = None;
    let mut close = false;

    ui.horizontal(|ui| {
        nav.open("dialog-tabs");
        for (slot, (value, label)) in TABS.iter().enumerate() {
            let response = nav.tracked(ui.selectable_value(&mut state.tab, *value, monitor_i18n::tr(label)));
            if response.clicked() {
                response.request_focus();
            }
            if response.has_focus() {
                focused = Some(*value);
            }
            ids[slot] = response.id;
        }
        // An arrow lands the focus on a tab without a press; the selection has
        // to follow it, or the body would keep showing the old tab under the
        // new focus.
        if let Some(value) = focused {
            state.tab = value;
        }
        // The tab in use is where an Up out of the body comes back to, and what
        // a dialog reopened without a focus returns to.
        let slot = TABS.iter().position(|(value, _)| *value == state.tab).unwrap_or(0);
        state.focus_anchor = Some(ids[slot]);
        nav.set_entry("dialog-tabs", ids[slot]);
        // The way out, at the far end of the strip.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if nav.tracked(ui.button(monitor_i18n::tr("dialog-close"))).clicked() {
                close = true;
            }
        });
        nav.close();
    });
    close
}

fn onvif_tab(
    ui: &mut egui::Ui,
    state: &mut DiscoveryUi,
    config: &mut AppConfig,
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    nav: &mut Nav,
) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        let busy = state.running;
        // The first control of this tab, and the one the strip's Down lands on.
        let probe = nav.tracked(ui.add_enabled(!busy, egui::Button::new(monitor_i18n::tr("dialog-probe"))));
        if probe.clicked() {
            let mut scan = state.scan.clone();
            scan.broadcast = true;
            scan.subnet_scan = false;
            let discovery = scan.to_config();
            state.running = true;
            state.clear_status(Tab::Onvif);
            state.devices.clear();
            state.ports.clear();
            state.phase = monitor_i18n::tr("dialog-starting");
            config.discovery = discovery.clone();
            changed = true;
            start_discovery(handle, events, discovery);
        }
        let full = nav.tracked(ui.add_enabled(!busy, egui::Button::new(monitor_i18n::tr("dialog-full-scan"))));
        if full.clicked() {
            let mut scan = state.scan.clone();
            scan.subnet_scan = true;
            scan.tcp_probe = true;
            let discovery = scan.to_config();
            state.running = true;
            state.clear_status(Tab::Onvif);
            state.devices.clear();
            state.ports.clear();
            state.phase = monitor_i18n::tr("dialog-starting");
            config.discovery = discovery.clone();
            changed = true;
            start_discovery(handle, events, discovery);
        }
        if busy {
            grid_spinner(ui);
            ui.label(RichText::new(&state.phase).color(theme::ACCENT));
        }
    });

    ui.add_space(4.0);
    // The fold is a stop of its own, ahead of the controls it hides: Down from
    // the buttons above reaches the header, and Enter - or a click - folds it
    // away. See `controls::fold` for what that takes.
    let scan_settings_label = monitor_i18n::tr("dialog-scan-settings");
    controls::fold(
        nav,
        ui,
        &scan_settings_label,
        state.scan.ip_ranges.trim().is_empty(),
        |ui, nav| {
            nav.tracked(ui.checkbox(&mut state.scan.broadcast, monitor_i18n::tr("dialog-multicast-probe")));
            nav.tracked(ui.checkbox(&mut state.scan.subnet_scan, monitor_i18n::tr("dialog-unicast-probe")));
            // A short label above the field, as the fields of the camera form
            // have; `text_field` carries the same label into the Android
            // floating input box. The longer explanation is the box's own hint,
            // so it is not repeated and disappears once the box has text.
            ui.label(monitor_i18n::tr("dialog-ip-ranges"));
            text_field(ui, nav, &mut state.scan.ip_ranges, &monitor_i18n::tr("dialog-ip-ranges"), false, true, |s| {
                egui::TextEdit::multiline(s)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text(theme::hint(&monitor_i18n::tr("dialog-ip-ranges-hint")))
            });
            ui.horizontal(|ui| {
                let targets = state.scan.target_count();
                ui.label(RichText::new(monitor_i18n::tr_args("dialog-target-count", &[("count", targets.into())])).small().color(theme::TEXT_DIM));
            });
            ui.horizontal(|ui| {
                nav.tracked(ui.checkbox(&mut state.scan.tcp_probe, monitor_i18n::tr("dialog-tcp-fallback")));
                ui.label(monitor_i18n::tr("dialog-tcp-ports"));
                text_field(ui, nav, &mut state.scan.tcp_ports, &monitor_i18n::tr("dialog-tcp-ports"), false, false, |s| {
                    egui::TextEdit::singleline(s).desired_width(140.0).hint_text(theme::hint("554, 80, 8000"))
                });
            });
            // Both value controls are sliders: Left / Right move the thumb, Up
            // / Down walks the column.
            controls::slider(nav, ui, &mut state.scan.timeout_ms, 200..=8000, 100.0, " ms", &monitor_i18n::tr("dialog-probe-timeout"), 1_500);
            controls::slider(nav, ui, &mut state.scan.concurrency, 1..=512, 1.0, "", &monitor_i18n::tr("dialog-concurrent-probes"), 64);
        },
    );

    // The ONVIF account is a stream account, so it belongs below the separator
    // with the list it acts on - and there is nothing to act on until the scan
    // has answered.
    ui.add_space(6.0);
    ui.separator();
    if !state.devices.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new(monitor_i18n::tr("dialog-onvif-credentials")).small());
        credentials_row(ui, nav, &mut state.scan.username, &mut state.scan.password);
    }

    ui.add_space(6.0);
    ui.label(RichText::new(monitor_i18n::tr_args("dialog-onvif-devices", &[("count", state.devices.len().into())])).strong());
    if state.devices.is_empty() {
        ui.label(RichText::new(monitor_i18n::tr("dialog-no-device")).small().color(theme::TEXT_DIM));
    }
    let mut to_import: Option<DiscoveredDevice> = None;
    for device in &state.devices {
        let host = device.host().unwrap_or_default();
        // Only the address is known until the device is resolved, so that is
        // what "already on the wall" is judged on here: an approximate check,
        // where the Synology list can compare the whole camera.
        let added = config.cameras.iter().any(|known| known.host.eq_ignore_ascii_case(&host));
        ui.horizontal(|ui| {
            let busy = state.busy.is_some();
            let label = if added { monitor_i18n::tr("dialog-added") } else { monitor_i18n::tr("dialog-add") };
            let add = nav.tracked(ui.add_enabled(!added && !busy, egui::Button::new(label)));
            if add.clicked() {
                to_import = Some(device.clone());
            }
            ui.label(RichText::new(device.display_name()).strong());
            ui.label(RichText::new(host).monospace().color(theme::TEXT_DIM));
            if let Some(vendor) = device.hardware() {
                ui.label(RichText::new(vendor).small().color(theme::TEXT_DIM));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(monitor_i18n::tr(device.source.label())).small().color(theme::TEXT_DIM));
            });
        });
    }
    if let Some(device) = to_import {
        state.busy = Some(device.display_name());
        let scan = state.scan.clone();
        let discovery = scan.to_config();
        config.discovery = discovery.clone();
        changed = true;
        start_import(handle, events, discovery, device);
    }

    ui.add_space(6.0);
    ui.label(RichText::new(monitor_i18n::tr_args("dialog-other-ports", &[("count", state.ports.len().into())])).strong());
    ui.label(
        RichText::new(monitor_i18n::tr("dialog-other-ports-hint"))
            .small()
            .color(theme::TEXT_DIM),
    );
    let mut to_manual: Option<String> = None;
    for hit in &state.ports {
        ui.horizontal(|ui| {
            if nav.tracked(ui.button(monitor_i18n::tr("dialog-use"))).clicked() {
                to_manual = Some(format!("rtsp://{}:{}/", hit.address, hit.port));
            }
            ui.label(RichText::new(hit.address.to_string()).monospace());
            ui.label(RichText::new(format!(":{}", hit.port)).monospace().color(theme::TEXT_DIM));
            let service = if hit.is_rtsp() { "RTSP".to_string() } else { hit.service.as_deref().map(str::to_string).unwrap_or_else(|| monitor_i18n::tr("dialog-open-port")) };
            ui.label(RichText::new(service).small().color(theme::TEXT_DIM));
        });
    }
    if let Some(url) = to_manual {
        state.manual.main = url;
        state.tab = Tab::Manual;
    }

    status_lines(ui, state, Tab::Onvif);
    changed
}

/// What [`camera_fields`] reports back to its caller.
pub struct CameraFields {
    /// The first control of the form, for a window that opens on it.
    pub first: Option<egui::Id>,
    /// Whether the "Infer" button was pressed.
    pub inferred: bool,
}

/// The camera fields, as the "Manual entry" tab and the settings panel's edit
/// window both lay them out.
///
/// `grid_id` names the grid - egui keys a grid's state on it, so the two must
/// not share one. See [`CameraFields`] for what comes back.
pub fn camera_fields(ui: &mut egui::Ui, nav: &mut Nav, grid_id: &str, draft: &mut CameraDraft) -> CameraFields {
    let mut first = None;
    let mut inferred = false;
    egui::Grid::new(grid_id).num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label(monitor_i18n::tr("dialog-name"));
        // The first control of the form, and the one a window opens on.
        let name = text_field(ui, nav, &mut draft.name, &monitor_i18n::tr("dialog-name"), false, false, |s| {
            egui::TextEdit::singleline(s).hint_text(theme::hint(&monitor_i18n::tr("dialog-name-hint"))).desired_width(f32::INFINITY)
        });
        first = Some(name.id);
        ui.end_row();

        ui.label(monitor_i18n::tr("dialog-main-stream"));
        text_field(ui, nav, &mut draft.main, &monitor_i18n::tr("dialog-main-stream"), false, false, |s| {
            egui::TextEdit::singleline(s)
                .hint_text(theme::hint("rtsp://user:pass@192.168.1.64:554/Streaming/Channels/101"))
                .desired_width(f32::INFINITY)
        });
        ui.end_row();

        ui.label(monitor_i18n::tr("dialog-sub-stream"));
        ui.horizontal(|ui| {
            text_field(ui, nav, &mut draft.sub, &monitor_i18n::tr("dialog-sub-stream"), false, false, |s| {
                egui::TextEdit::singleline(s)
                    .hint_text(theme::hint(&monitor_i18n::tr("dialog-sub-hint")))
                    .desired_width(320.0)
            });
            if nav.tracked(ui.button(monitor_i18n::tr("dialog-infer"))).clicked() {
                draft.infer_sub = true;
                inferred = true;
                let mut probe = draft.clone();
                probe.sub.clear();
                if let Ok(source) = probe.build() {
                    draft.sub = source.rtsp_sub.unwrap_or_default();
                }
            }
        });
        ui.end_row();
    });
    nav.tracked(ui.checkbox(&mut draft.infer_sub, monitor_i18n::tr("dialog-derive-sub")));

    // The stream account in the layout every other tab gives one: a heading
    // naming it, then the two fields on one row.
    ui.separator();
    ui.label(RichText::new(monitor_i18n::tr("dialog-stream-credentials")).small());
    credentials_row(ui, nav, &mut draft.username, &mut draft.password);
    CameraFields { first, inferred }
}

fn manual_tab(ui: &mut egui::Ui, state: &mut DiscoveryUi, config: &mut AppConfig, nav: &mut Nav) -> bool {
    let mut changed = false;
    if camera_fields(ui, nav, "manual-camera", &mut state.manual).inferred {
        state.note(Tab::Manual, monitor_i18n::tr("dialog-sub-derived-msg"));
    }

    ui.horizontal(|ui| {
        if nav.tracked(ui.button(monitor_i18n::tr("dialog-add-camera"))).clicked() {
            match state.manual.build() {
                Ok(source) => {
                    config.upsert_camera(source);
                    state.manual.reset();
                    state.clear_status(Tab::Manual);
                    state.note(Tab::Manual, monitor_i18n::tr("dialog-camera-added"));
                    changed = true;
                }
                Err(err) => state.fail(Tab::Manual, err),
            }
        }
        if nav.tracked(ui.button(monitor_i18n::tr("dialog-clear"))).clicked() {
            state.manual.reset();
        }
    });

    status_lines(ui, state, Tab::Manual);
    changed
}

/// What the button of a listed camera should offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddOffer {
    /// Not on the wall yet.
    Add,
    /// The same camera is there, with settings an import would refresh.
    Update,
    /// The same camera, the same settings: nothing to add.
    Added,
}

/// Whether a listed camera is already on the wall, and whether importing it
/// would change anything.
///
/// "The same camera" is [`CameraSource::identity`] - scheme, host, port and
/// path, with the way the address is written taken out - so a camera added by
/// hand, by ONVIF or by Synology is all recognised, and not only the ones this
/// tab imported.
fn add_offer(config: &AppConfig, camera: &CameraSource) -> AddOffer {
    let Some(known) = config.cameras.iter().find(|known| known.identity() == camera.identity()) else {
        return AddOffer::Add;
    };
    if same_settings(known, camera) {
        AddOffer::Added
    } else {
        AddOffer::Update
    }
}

/// The fields an import writes, compared for [`add_offer`].
///
/// The ones [`AppConfig::upsert_camera`] keeps from the camera already there -
/// id, origin, enabled, transport - are left out: an import never touches
/// them, so a difference there is no reason to offer a refresh.
fn same_settings(a: &CameraSource, b: &CameraSource) -> bool {
    a.name == b.name
        && a.rtsp_main == b.rtsp_main
        && a.rtsp_sub == b.rtsp_sub
        && a.username == b.username
        && a.password == b.password
        && a.main_profile == b.main_profile
        && a.sub_profile == b.sub_profile
}

fn synology_tab(
    ui: &mut egui::Ui,
    state: &mut DiscoveryUi,
    config: &mut AppConfig,
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    nav: &mut Nav,
) -> bool {
    let mut changed = false;
    ui.label(
        RichText::new(monitor_i18n::tr("dialog-synology-hint"))
            .small()
            .color(theme::TEXT_DIM),
    );
    ui.add_space(4.0);

    let synology = &mut config.synology;
    let before = synology.clone();
    egui::Grid::new("synology").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label(monitor_i18n::tr("dialog-host"));
        // The first control of this tab, and the one the strip's Down lands on.
        text_field(ui, nav, &mut synology.host, &monitor_i18n::tr("dialog-host"), false, false, |s| {
            egui::TextEdit::singleline(s).hint_text(theme::hint("192.168.1.10")).desired_width(220.0)
        });
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-port"));
        // A text box rather than a drag value: a port is typed, and on Android a
        // typed field is the one the floating input box mirrors. Only digits are
        // kept, at most five; the number is taken from the text while it names a
        // valid port, so the box can be cleared to retype it without the port
        // being lost, and shows the port in use again once it is left.
        let port_text = state.synology_port.get_or_insert_with(|| synology.port.to_string());
        let port_field = text_field(ui, nav, port_text, &monitor_i18n::tr("dialog-port"), false, false, |s| {
            egui::TextEdit::singleline(s).desired_width(220.0).char_limit(5)
        });
        port_text.retain(|c| c.is_ascii_digit());
        if let Ok(port) = port_text.parse::<u16>() {
            if port >= 1 {
                synology.port = port;
            }
        }
        if !port_field.has_focus() {
            *port_text = synology.port.to_string();
        }
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-scheme"));
        nav.tracked(ui.checkbox(&mut synology.https, monitor_i18n::tr("dialog-https")));
        ui.end_row();
    });

    // The login is a server parameter like the address above it, so it stays on
    // the near side of the separator; the list it acts on is beyond it.
    ui.add_space(6.0);
    ui.label(RichText::new(monitor_i18n::tr("dialog-synology-credentials")).small());
    credentials_row(ui, nav, &mut synology.username, &mut synology.password);
    if *synology != before {
        changed = true;
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let ready = synology.is_configured() && !state.synology_busy;
        if nav.tracked(ui.add_enabled(ready, egui::Button::new(monitor_i18n::tr("dialog-fetch-cameras")))).clicked() {
            state.synology_busy = true;
            state.clear_status(Tab::Synology);
            // The list is about to be replaced; a stale one under a running
            // fetch would invite a press on a camera the NAS may not return.
            state.synology_cameras.clear();
            start_synology(handle, events, synology.clone());
        }
        if state.synology_busy {
            grid_spinner(ui);
            ui.label(RichText::new(monitor_i18n::tr("dialog-contacting-nas")).color(theme::ACCENT));
        }
    });

    ui.separator();

    // What the NAS answered, one row per camera: added on the viewer's press,
    // like the ONVIF device list, rather than all at once as they arrive.
    ui.add_space(6.0);
    ui.label(RichText::new(monitor_i18n::tr_args("dialog-cameras-on-nas", &[("count", state.synology_cameras.len().into())])).strong());
    if state.synology_cameras.is_empty() {
        ui.label(RichText::new(monitor_i18n::tr("dialog-nothing-fetched")).small().color(theme::TEXT_DIM));
    }
    let mut to_add: Option<(CameraSource, AddOffer)> = None;
    for camera in &state.synology_cameras {
        let offer = add_offer(config, camera);
        ui.horizontal(|ui| {
            let (label, enabled) = match offer {
                AddOffer::Add => (monitor_i18n::tr("dialog-add"), true),
                AddOffer::Update => (monitor_i18n::tr("dialog-update"), true),
                AddOffer::Added => (monitor_i18n::tr("dialog-added"), false),
            };
            if nav.tracked(ui.add_enabled(enabled, egui::Button::new(label))).clicked() {
                to_add = Some((camera.clone(), offer));
            }
            ui.label(RichText::new(camera.short_label(24)).strong());
            ui.label(RichText::new(&camera.host).monospace().color(theme::TEXT_DIM));
            if camera.rtsp_sub.is_some() {
                ui.label(RichText::new(monitor_i18n::tr("dialog-sub-tag")).small().color(theme::ACCENT));
            }
            if offer == AddOffer::Update {
                ui.label(RichText::new(monitor_i18n::tr("dialog-already-added")).small().color(theme::WARN));
            }
        });
    }
    if let Some((camera, offer)) = to_add {
        apply_add(config, state, Tab::Synology, camera, offer);
        changed = true;
    }

    status_lines(ui, state, Tab::Synology);
    changed
}

/// The user and password fields of one credential set, on one row.
///
/// The layout the ONVIF tab gives its own credentials, which the tabs that
/// followed it keep: a heading naming what the account is for, then this.
fn credentials_row(ui: &mut egui::Ui, nav: &mut Nav, username: &mut String, password: &mut String) {
    ui.horizontal(|ui| {
        ui.label(monitor_i18n::tr("dialog-user"));
        text_field(ui, nav, username, &monitor_i18n::tr("dialog-user"), false, false, |s| {
            egui::TextEdit::singleline(s).desired_width(140.0)
        });
        ui.label(monitor_i18n::tr("dialog-password"));
        text_field(ui, nav, password, &monitor_i18n::tr("dialog-password"), true, false, |s| {
            egui::TextEdit::singleline(s).password(true).desired_width(140.0)
        });
    });
}

/// Draws a list of fetched cameras, one row each, and reports the one whose
/// button was pressed.
///
/// What the Frigate and go2rtc tabs list before any of it is added, like the
/// Synology list they mirror.
fn camera_rows(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    config: &AppConfig,
    cameras: &[CameraSource],
) -> Option<(CameraSource, AddOffer)> {
    let mut to_add = None;
    for camera in cameras {
        let offer = add_offer(config, camera);
        ui.horizontal(|ui| {
            let (label, enabled) = match offer {
                AddOffer::Add => (monitor_i18n::tr("dialog-add"), true),
                AddOffer::Update => (monitor_i18n::tr("dialog-update"), true),
                AddOffer::Added => (monitor_i18n::tr("dialog-added"), false),
            };
            if nav.tracked(ui.add_enabled(enabled, egui::Button::new(label))).clicked() {
                to_add = Some((camera.clone(), offer));
            }
            ui.label(RichText::new(camera.short_label(24)).strong());
            ui.label(RichText::new(&camera.host).monospace().color(theme::TEXT_DIM));
            if camera.rtsp_sub.is_some() {
                ui.label(RichText::new(monitor_i18n::tr("dialog-sub-tag")).small().color(theme::ACCENT));
            }
            if offer == AddOffer::Update {
                ui.label(RichText::new(monitor_i18n::tr("dialog-already-added")).small().color(theme::WARN));
            }
        });
    }
    to_add
}

/// Puts a picked camera on the wall and says so, on the tab it came from.
fn apply_add(config: &mut AppConfig, state: &mut DiscoveryUi, tab: Tab, camera: CameraSource, offer: AddOffer) {
    let name = camera.name.clone();
    config.upsert_camera(camera);
    state.note(tab, if offer == AddOffer::Update {
        monitor_i18n::tr_args("dialog-updated-msg", &[("name", name.into())])
    } else {
        monitor_i18n::tr_args("dialog-added-msg", &[("name", name.into())])
    });
}

fn frigate_tab(
    ui: &mut egui::Ui,
    state: &mut DiscoveryUi,
    config: &mut AppConfig,
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    nav: &mut Nav,
) -> bool {
    let mut changed = false;
    ui.label(RichText::new(monitor_i18n::tr("dialog-frigate-hint")).small().color(theme::TEXT_DIM));
    ui.add_space(4.0);

    let frigate = &mut config.frigate;
    let before = frigate.clone();
    egui::Grid::new("frigate").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label(monitor_i18n::tr("dialog-host"));
        text_field(ui, nav, &mut frigate.host, &monitor_i18n::tr("dialog-host"), false, false, |s| {
            egui::TextEdit::singleline(s).hint_text(theme::hint("192.168.1.10")).desired_width(220.0)
        });
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-port"));
        let port_text = state.frigate_port.get_or_insert_with(|| frigate.port.to_string());
        let port_field = text_field(ui, nav, port_text, &monitor_i18n::tr("dialog-port"), false, false, |s| {
            egui::TextEdit::singleline(s).desired_width(220.0).char_limit(5)
        });
        port_text.retain(|c| c.is_ascii_digit());
        if let Ok(port) = port_text.parse::<u16>() {
            if port >= 1 {
                frigate.port = port;
            }
        }
        if !port_field.has_focus() {
            *port_text = frigate.port.to_string();
        }
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-scheme"));
        nav.tracked(ui.checkbox(&mut frigate.https, monitor_i18n::tr("dialog-https")));
        ui.end_row();
    });

    // The login is a server parameter like the address above it, so it stays on
    // the near side of the separator.
    ui.add_space(6.0);
    ui.label(RichText::new(monitor_i18n::tr("dialog-frigate-credentials")).small());
    credentials_row(ui, nav, &mut frigate.username, &mut frigate.password);

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let ready = frigate.is_configured() && !state.frigate_busy;
        if nav.tracked(ui.add_enabled(ready, egui::Button::new(monitor_i18n::tr("dialog-fetch-cameras")))).clicked() {
            state.frigate_busy = true;
            state.clear_status(Tab::Frigate);
            // A stale list under a running fetch would invite a press on a
            // camera the server may not return.
            state.frigate_cameras.clear();
            state.frigate_leftovers.clear();
            start_frigate(handle, events, frigate.clone());
        }
        if state.frigate_busy {
            grid_spinner(ui);
            ui.label(RichText::new(monitor_i18n::tr("dialog-contacting-server")).color(theme::ACCENT));
        }
    });

    ui.separator();

    // The restream account is a stream account, so it belongs below the
    // separator with the streams - and it is normally read from Frigate's own
    // configuration, so it is only worth showing once there is something to
    // pull.
    if !state.frigate_cameras.is_empty() || !state.frigate_leftovers.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new(monitor_i18n::tr("dialog-rtsp-credentials")).small());
        credentials_row(ui, nav, &mut frigate.rtsp_username, &mut frigate.rtsp_password);
        ui.label(
            RichText::new(monitor_i18n::tr("dialog-rtsp-override-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
    }
    if *frigate != before {
        changed = true;
    }

    ui.add_space(6.0);
    ui.label(
        RichText::new(monitor_i18n::tr_args(
            "dialog-frigate-cameras",
            &[("count", state.frigate_cameras.len().into())],
        ))
        .strong(),
    );
    if state.frigate_cameras.is_empty() {
        ui.label(RichText::new(monitor_i18n::tr("dialog-nothing-fetched")).small().color(theme::TEXT_DIM));
    }
    if let Some((camera, offer)) = camera_rows(ui, nav, config, &state.frigate_cameras) {
        apply_add(config, state, Tab::Frigate, camera, offer);
        changed = true;
    }

    ui.add_space(6.0);
    ui.label(
        RichText::new(monitor_i18n::tr_args(
            "dialog-frigate-streams",
            &[("count", state.frigate_leftovers.len().into())],
        ))
        .strong(),
    );
    if state.frigate_leftovers.is_empty() {
        ui.label(RichText::new(monitor_i18n::tr("dialog-nothing-fetched")).small().color(theme::TEXT_DIM));
    }
    if let Some((camera, offer)) = camera_rows(ui, nav, config, &state.frigate_leftovers) {
        apply_add(config, state, Tab::Frigate, camera, offer);
        changed = true;
    }

    status_lines(ui, state, Tab::Frigate);
    changed
}

fn go2rtc_tab(
    ui: &mut egui::Ui,
    state: &mut DiscoveryUi,
    config: &mut AppConfig,
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    nav: &mut Nav,
) -> bool {
    let mut changed = false;
    ui.label(RichText::new(monitor_i18n::tr("dialog-go2rtc-hint")).small().color(theme::TEXT_DIM));
    ui.add_space(4.0);

    let go2rtc = &mut config.go2rtc;
    let before = go2rtc.clone();
    egui::Grid::new("go2rtc").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label(monitor_i18n::tr("dialog-host"));
        text_field(ui, nav, &mut go2rtc.host, &monitor_i18n::tr("dialog-host"), false, false, |s| {
            egui::TextEdit::singleline(s).hint_text(theme::hint("192.168.1.10")).desired_width(220.0)
        });
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-port"));
        let port_text = state.go2rtc_port.get_or_insert_with(|| go2rtc.port.to_string());
        let port_field = text_field(ui, nav, port_text, &monitor_i18n::tr("dialog-port"), false, false, |s| {
            egui::TextEdit::singleline(s).desired_width(220.0).char_limit(5)
        });
        port_text.retain(|c| c.is_ascii_digit());
        if let Ok(port) = port_text.parse::<u16>() {
            if port >= 1 {
                go2rtc.port = port;
            }
        }
        if !port_field.has_focus() {
            *port_text = go2rtc.port.to_string();
        }
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-scheme"));
        nav.tracked(ui.checkbox(&mut go2rtc.https, monitor_i18n::tr("dialog-https")));
        ui.end_row();
    });

    // The API login is a server parameter, so it stays on the near side of the
    // separator.
    ui.add_space(6.0);
    ui.label(RichText::new(monitor_i18n::tr("dialog-go2rtc-credentials")).small());
    credentials_row(ui, nav, &mut go2rtc.api_username, &mut go2rtc.api_password);

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let ready = go2rtc.is_configured() && !state.go2rtc_busy;
        if nav.tracked(ui.add_enabled(ready, egui::Button::new(monitor_i18n::tr("dialog-fetch-cameras")))).clicked() {
            state.go2rtc_busy = true;
            state.clear_status(Tab::Go2rtc);
            state.go2rtc_cameras.clear();
            start_go2rtc(handle, events, go2rtc.clone());
        }
        if state.go2rtc_busy {
            grid_spinner(ui);
            ui.label(RichText::new(monitor_i18n::tr("dialog-contacting-server")).color(theme::ACCENT));
        }
    });

    ui.separator();

    // The RTSP account is a stream account, so it belongs below the separator
    // with the streams. go2rtc's API does not serve it - it has to be entered
    // by hand - and it only matters once there are streams to pull.
    if !state.go2rtc_cameras.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new(monitor_i18n::tr("dialog-rtsp-credentials")).small());
        credentials_row(ui, nav, &mut go2rtc.rtsp_username, &mut go2rtc.rtsp_password);
    }
    if *go2rtc != before {
        changed = true;
    }

    ui.add_space(6.0);
    ui.label(
        RichText::new(monitor_i18n::tr_args(
            "dialog-go2rtc-streams",
            &[("count", state.go2rtc_cameras.len().into())],
        ))
        .strong(),
    );
    if state.go2rtc_cameras.is_empty() {
        ui.label(RichText::new(monitor_i18n::tr("dialog-nothing-fetched")).small().color(theme::TEXT_DIM));
    }
    if let Some((camera, offer)) = camera_rows(ui, nav, config, &state.go2rtc_cameras) {
        apply_add(config, state, Tab::Go2rtc, camera, offer);
        changed = true;
    }

    status_lines(ui, state, Tab::Go2rtc);
    changed
}

/// Draws what `tab` last reported.
///
/// Another tab's status is not this one's: the wording names the tab it belongs
/// to ("on the NAS", "in Frigate"), so anything tagged with another tab is left
/// out rather than read where it does not apply.
fn status_lines(ui: &mut egui::Ui, state: &DiscoveryUi, tab: Tab) {
    ui.add_space(8.0);
    if let Some((_, message)) = state.message.as_ref().filter(|(other, _)| *other == tab) {
        ui.label(RichText::new(message).color(theme::LIVE));
    }
    if let Some((_, error)) = state.error.as_ref().filter(|(other, _)| *other == tab) {
        ui.label(RichText::new(error).color(theme::ERROR));
    }
}

fn grid_spinner(ui: &mut egui::Ui) {
    ui.add(egui::Spinner::new().size(16.0));
}

/// Small helper used by the settings panel to describe a discovery configuration.
pub fn discovery_summary(config: &DiscoveryConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    if config.broadcast {
        parts.push(monitor_i18n::tr("summary-multicast"));
    }
    if config.subnet_scan {
        parts.push(monitor_i18n::tr_args("summary-unicast", &[("count", config.ip_ranges.len().into())]));
    }
    if config.tcp_probe {
        let ports = config.tcp_ports.iter().map(u16::to_string).collect::<Vec<_>>().join("/");
        parts.push(monitor_i18n::tr_args("summary-tcp", &[("ports", ports.into())]));
    }
    if parts.is_empty() {
        monitor_i18n::tr("summary-disabled")
    } else {
        parts.join(" + ")
    }
}
