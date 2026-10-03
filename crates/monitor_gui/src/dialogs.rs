//! Device discovery / camera management dialogs.
//!
//! The dialog never blocks: every scan, ONVIF import and Synology request runs
//! on the tokio runtime and reports back through a `crossbeam-channel`, which
//! the render loop drains once per frame.

use std::sync::Arc;

use crossbeam_channel::Sender;
use egui::{Align, Layout, RichText};
use tokio::runtime::Handle;

use monitor_core::config::{AppConfig, DiscoveryConfig, SynologyConfig};
use monitor_core::discovery::{
    DiscoveryEvent, DiscoveryReport, DiscoveryService, DiscoveredDevice, PortHit, ProgressCallback,
};
use monitor_core::model::CameraSource;

use crate::controls;
use crate::nav::{Kind, Nav};
use crate::theme;

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
}

/// Tab of the "Add devices" window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Onvif,
    Manual,
    Synology,
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
            return Err("an RTSP url is required".to_string());
        }
        if !main.to_ascii_lowercase().starts_with("rtsp://") {
            return Err("the url must start with rtsp://".to_string());
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
    /// Hosts already imported into the configuration.
    pub imported: Vec<String>,
    /// Name of the device currently being resolved.
    pub busy: Option<String>,
    pub error: Option<String>,
    pub message: Option<String>,
    pub manual: CameraDraft,
    pub synology_busy: bool,
    /// First control of the window, so that a remote control can be handed the
    /// focus as the window opens rather than leaving it on the wall behind.
    pub focus_anchor: Option<egui::Id>,

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
                self.message = Some(format!(
                    "discovery finished: {} ONVIF device(s), {} open port(s)",
                    self.devices.len(),
                    self.ports.len()
                ));
            }
            BackgroundEvent::DiscoveryFailed(error) => {
                self.running = false;
                self.error = Some(error);
            }
            BackgroundEvent::Imported(camera) => {
                self.busy = None;
                self.imported.push(camera.host.clone());
                self.message = Some(format!("imported {} ({})", camera.name, camera.display_address()));
            }
            BackgroundEvent::ImportFailed { name, error } => {
                self.busy = None;
                self.error = Some(format!("{name}: {error}"));
            }
            BackgroundEvent::SynologyDone(cameras) => {
                self.synology_busy = false;
                self.message = Some(format!("imported {} camera(s) from Surveillance Station", cameras.len()));
            }
            BackgroundEvent::SynologyFailed(error) => {
                self.synology_busy = false;
                self.error = Some(format!("Synology: {error}"));
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

/// Draws the "Add devices" window. Returns `true` when the configuration changed.
pub fn add_devices_window(
    ctx: &egui::Context,
    state: &mut DiscoveryUi,
    config: &mut AppConfig,
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    nav: &mut Nav,
) -> bool {
    let mut open = state.open;
    let mut changed = false;

    egui::Window::new("Add devices")
        .open(&mut open)
        .collapsible(false)
        // Not resizable on purpose. A resizable window carries resize grips, and
        // egui's grips are focusable: the remote control's arrows and its Tab
        // both land on them instead of on the controls, and the form becomes
        // impossible to walk. The window is sized for the screen it opens on.
        .resizable(false)
        .default_size([640.0, 560.0])
        .min_width(520.0)
        .show(ctx, |ui| {
            // Keep the remote control's focus inside the window.
            //
            // The navigation layer answers the arrows between the window's own
            // controls; a modal layer is what keeps egui's walk - which still
            // runs, and still serves whatever is not registered with the layer -
            // from landing on a control of the bar behind.
            ctx.memory_mut(|memory| memory.set_modal_layer(ui.layer_id()));

            tab_strip(ui, state, nav);
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
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match state.tab {
                Tab::Onvif => {
                    changed |= onvif_tab(ui, state, config, handle, events, nav);
                }
                Tab::Manual => {
                    changed |= manual_tab(ui, state, config, nav);
                }
                Tab::Synology => {
                    changed |= synology_tab(ui, state, config, handle, events, nav);
                }
            });
            nav.close();
        });

    state.open = open;
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
fn tab_strip(ui: &mut egui::Ui, state: &mut DiscoveryUi, nav: &mut Nav) {
    const TABS: [(Tab, &str); 3] = [
        (Tab::Onvif, "ONVIF / network scan"),
        (Tab::Manual, "Manual entry"),
        (Tab::Synology, "Synology NAS"),
    ];

    let mut ids = [egui::Id::NULL; TABS.len()];
    let mut focused = None;

    ui.horizontal(|ui| {
        nav.open("dialog-tabs");
        for (slot, (value, label)) in TABS.iter().enumerate() {
            let response = nav.tracked(ui.selectable_value(&mut state.tab, *value, *label));
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
        // a window reopened without a focus returns to.
        let slot = TABS.iter().position(|(value, _)| *value == state.tab).unwrap_or(0);
        state.focus_anchor = Some(ids[slot]);
        nav.set_entry("dialog-tabs", ids[slot]);
        nav.close();
    });
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
        let probe = nav.tracked(ui.add_enabled(!busy, egui::Button::new("Probe 239.255.255.250:3702")));
        if probe.clicked() {
            let mut scan = state.scan.clone();
            scan.broadcast = true;
            scan.subnet_scan = false;
            let discovery = scan.to_config();
            state.running = true;
            state.error = None;
            state.message = None;
            state.devices.clear();
            state.ports.clear();
            state.phase = "starting…".to_string();
            config.discovery = discovery.clone();
            changed = true;
            start_discovery(handle, events, discovery);
        }
        let full = nav.tracked(ui.add_enabled(!busy, egui::Button::new("Full scan (WS-Discovery + TCP)")));
        if full.clicked() {
            let mut scan = state.scan.clone();
            scan.subnet_scan = true;
            scan.tcp_probe = true;
            let discovery = scan.to_config();
            state.running = true;
            state.error = None;
            state.message = None;
            state.devices.clear();
            state.ports.clear();
            state.phase = "starting…".to_string();
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
    controls::fold(
        nav,
        ui,
        "Scan settings",
        state.scan.ip_ranges.trim().is_empty(),
        |ui, nav| {
            nav.tracked(ui.checkbox(&mut state.scan.broadcast, "Multicast probe (local subnet)"));
            nav.tracked(ui.checkbox(&mut state.scan.subnet_scan, "Unicast probe over the ranges below (VLAN / cross subnet)"));
            ui.label(RichText::new("IP ranges — one per line, e.g. 192.168.1.1-254 or 10.0.0.0/24").small());
            nav.tracked_kind(Kind::Text, ui.add(egui::TextEdit::multiline(&mut state.scan.ip_ranges).desired_rows(3).desired_width(f32::INFINITY)));
            ui.horizontal(|ui| {
                let targets = state.scan.target_count();
                ui.label(RichText::new(format!("{targets} target address(es)")).small().color(theme::TEXT_DIM));
            });
            ui.horizontal(|ui| {
                nav.tracked(ui.checkbox(&mut state.scan.tcp_probe, "TCP fallback scan"));
                nav.tracked_kind(Kind::Text, ui.add(egui::TextEdit::singleline(&mut state.scan.tcp_ports).desired_width(140.0).hint_text(theme::hint("554, 80, 8000"))));
            });
            // The two value controls: the slider keeps Left / Right for its
            // thumb, the drag value keeps Up / Down for its step, and each walks
            // the column with the other axis.
            let timeout = ui.add(egui::Slider::new(&mut state.scan.timeout_ms, 200..=8000).text("probe timeout (ms)"));
            nav.item_kind(Kind::Slider, &timeout);
            controls::drag_value(nav, ui, &mut state.scan.concurrency, 1..=512, 1.0, " concurrent probes");
            ui.separator();
            ui.label(RichText::new("ONVIF credentials (used by GetProfiles / GetStreamUri)").small());
            ui.horizontal(|ui| {
                ui.label("User");
                nav.tracked_kind(Kind::Text, ui.add(egui::TextEdit::singleline(&mut state.scan.username).desired_width(140.0)));
                ui.label("Password");
                nav.tracked_kind(Kind::Text, ui.add(egui::TextEdit::singleline(&mut state.scan.password).password(true).desired_width(140.0)));
            });
        },
    );

    ui.add_space(6.0);
    ui.label(RichText::new(format!("ONVIF devices ({})", state.devices.len())).strong());
    if state.devices.is_empty() {
        ui.label(RichText::new("no device answered yet").small().color(theme::TEXT_DIM));
    }
    let mut to_import: Option<DiscoveredDevice> = None;
    for device in &state.devices {
        let host = device.host().unwrap_or_default();
        let imported = state.imported.iter().any(|known| *known == host);
        ui.horizontal(|ui| {
            let busy = state.busy.is_some();
            let label = if imported { "Added" } else { "Add" };
            let add = nav.tracked(ui.add_enabled(!imported && !busy, egui::Button::new(label)));
            if add.clicked() {
                to_import = Some(device.clone());
            }
            ui.label(RichText::new(device.display_name()).strong());
            ui.label(RichText::new(host).monospace().color(theme::TEXT_DIM));
            if let Some(vendor) = device.hardware() {
                ui.label(RichText::new(vendor).small().color(theme::TEXT_DIM));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(device.source.label()).small().color(theme::TEXT_DIM));
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
    ui.label(RichText::new(format!("Other open ports ({})", state.ports.len())).strong());
    ui.label(
        RichText::new("devices that do not answer WS-Discovery; add them manually with the RTSP url")
            .small()
            .color(theme::TEXT_DIM),
    );
    let mut to_manual: Option<String> = None;
    for hit in &state.ports {
        ui.horizontal(|ui| {
            if nav.tracked(ui.button("Use")).clicked() {
                to_manual = Some(format!("rtsp://{}:{}/", hit.address, hit.port));
            }
            ui.label(RichText::new(hit.address.to_string()).monospace());
            ui.label(RichText::new(format!(":{}", hit.port)).monospace().color(theme::TEXT_DIM));
            let service = if hit.is_rtsp() { "RTSP" } else { hit.service.as_deref().unwrap_or("open port") };
            ui.label(RichText::new(service).small().color(theme::TEXT_DIM));
        });
    }
    if let Some(url) = to_manual {
        state.manual.main = url;
        state.tab = Tab::Manual;
    }

    status_lines(ui, state);
    changed
}

fn manual_tab(ui: &mut egui::Ui, state: &mut DiscoveryUi, config: &mut AppConfig, nav: &mut Nav) -> bool {
    let mut changed = false;
    egui::Grid::new("manual-camera").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label("Name");
        // The first control of this tab, and the one the strip's Down lands on.
        nav.tracked_kind(Kind::Text, ui.add(
            egui::TextEdit::singleline(&mut state.manual.name).hint_text(theme::hint("Front door")).desired_width(f32::INFINITY),
        ));
        ui.end_row();

        ui.label("Main stream");
        nav.tracked_kind(Kind::Text, ui.add(
            egui::TextEdit::singleline(&mut state.manual.main)
                .hint_text(theme::hint("rtsp://user:pass@192.168.1.64:554/Streaming/Channels/101"))
                .desired_width(f32::INFINITY),
        ));
        ui.end_row();

        ui.label("Sub stream");
        ui.horizontal(|ui| {
            nav.tracked_kind(Kind::Text, ui.add(
                egui::TextEdit::singleline(&mut state.manual.sub)
                    .hint_text(theme::hint("optional, derived from the main url"))
                    .desired_width(320.0),
            ));
            if nav.tracked(ui.button("Infer")).clicked() {
                state.manual.infer_sub = true;
                let mut probe = state.manual.clone();
                probe.sub.clear();
                if let Ok(source) = probe.build() {
                    state.manual.sub = source.rtsp_sub.unwrap_or_default();
                    state.message = Some("sub stream url derived from the main url".to_string());
                }
            }
        });
        ui.end_row();

        // One field per row. The column walk visits them in draw order, so the
        // two are never confused for one another the way a geometric walk did,
        // which used to drop the caret into the password from the sub stream box.
        ui.label("User");
        nav.tracked_kind(Kind::Text, ui.add(
            egui::TextEdit::singleline(&mut state.manual.username)
                .hint_text(theme::hint("user"))
                .desired_width(f32::INFINITY),
        ));
        ui.end_row();

        ui.label("Password");
        nav.tracked_kind(Kind::Text, ui.add(
            egui::TextEdit::singleline(&mut state.manual.password)
                .password(true)
                .hint_text(theme::hint("password"))
                .desired_width(f32::INFINITY),
        ));
        ui.end_row();
    });

    nav.tracked(ui.checkbox(&mut state.manual.infer_sub, "Derive the sub stream from the main url when it is empty"));

    ui.horizontal(|ui| {
        if nav.tracked(ui.button("Add camera")).clicked() {
            match state.manual.build() {
                Ok(source) => {
                    config.upsert_camera(source);
                    state.manual.reset();
                    state.error = None;
                    state.message = Some("camera added".to_string());
                    changed = true;
                }
                Err(err) => state.error = Some(err),
            }
        }
        if nav.tracked(ui.button("Clear")).clicked() {
            state.manual.reset();
        }
    });

    status_lines(ui, state);
    changed
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
        RichText::new("SYNO.API.Auth + SYNO.SurveillanceStation.Camera: pulls every camera bound to the NAS with its main and sub stream.")
            .small()
            .color(theme::TEXT_DIM),
    );
    ui.add_space(4.0);

    let synology = &mut config.synology;
    let before = synology.clone();
    egui::Grid::new("synology").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label("Host");
        // The first control of this tab, and the one the strip's Down lands on.
        nav.tracked_kind(Kind::Text, ui.add(egui::TextEdit::singleline(&mut synology.host).hint_text(theme::hint("192.168.1.10")).desired_width(220.0)));
        ui.end_row();
        ui.label("Port");
        // A drag value: Up / Down step it, Left / Right walk the column.
        controls::drag_value(nav, ui, &mut synology.port, 1..=65535, 1.0, "");
        ui.end_row();
        ui.label("Scheme");
        nav.tracked(ui.checkbox(&mut synology.https, "https"));
        ui.end_row();
        ui.label("Account");
        nav.tracked_kind(Kind::Text, ui.add(egui::TextEdit::singleline(&mut synology.username).desired_width(220.0)));
        ui.end_row();
        ui.label("Password");
        nav.tracked_kind(Kind::Text, ui.add(egui::TextEdit::singleline(&mut synology.password).password(true).desired_width(220.0)));
        ui.end_row();
    });
    if *synology != before {
        changed = true;
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let ready = config.synology.is_configured() && !state.synology_busy;
        if nav.tracked(ui.add_enabled(ready, egui::Button::new("Fetch cameras"))).clicked() {
            state.synology_busy = true;
            state.error = None;
            start_synology(handle, events, config.synology.clone());
        }
        if state.synology_busy {
            grid_spinner(ui);
            ui.label(RichText::new("contacting the NAS…").color(theme::ACCENT));
        }
    });

    status_lines(ui, state);
    changed
}

fn status_lines(ui: &mut egui::Ui, state: &DiscoveryUi) {
    ui.add_space(8.0);
    if let Some(message) = &state.message {
        ui.label(RichText::new(message).color(theme::LIVE));
    }
    if let Some(error) = &state.error {
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
        parts.push("multicast".to_string());
    }
    if config.subnet_scan {
        parts.push(format!("unicast ({} ranges)", config.ip_ranges.len()));
    }
    if config.tcp_probe {
        parts.push(format!("tcp {}", config.tcp_ports.iter().map(u16::to_string).collect::<Vec<_>>().join("/")));
    }
    if parts.is_empty() {
        "disabled".to_string()
    } else {
        parts.join(" + ")
    }
}
