//! Device discovery / camera management dialogs.
//!
//! The dialog never blocks: every scan, ONVIF import and Synology request runs
//! on the tokio runtime and reports back through a `crossbeam-channel`, which
//! the render loop drains once per frame.

use std::sync::Arc;

use crossbeam_channel::Sender;
use egui::{Align, Layout, RichText};
use tokio::runtime::Handle;

use monitor_core::config::{
    AppConfig, DiscoveryConfig, FrigateConfig, Go2rtcConfig, StreamProtocol, SynologyConfig,
};
use monitor_core::discovery::{
    DiscoveryEvent, DiscoveryReport, DiscoveryService, DiscoveredDevice, FrigateImport, Go2rtcImport,
    PortHit, ProgressCallback,
};
use monitor_core::discovery::frigate::{
    Segment, VodPlaylist,
    extract_rtsp_port, extract_stream, mse_url as frigate_mse_url, rtsp_url as frigate_rtsp_url,
};
use monitor_core::discovery::go2rtc::{mse_url as go2rtc_mse_url, restream_url as go2rtc_rtsp_url};
use monitor_core::model::{infer_sub_stream, is_stream_url, CameraSource, FrigateOrigin, TalkMode};
use monitor_core::recording_download::Progress;

use crate::controls;
use crate::icons::{self, Icon};
use crate::nav::{Kind, Nav};
use crate::theme;

/// The width a modal is actually laid out at, given the width it asks for and
/// the screen it has to fit on. A form built for a television can be several
/// hundred points wide, which overhangs both edges of a 360pt upright phone.
/// Landscape screens are wider than every request, so they keep the requested
/// width unchanged; only a screen narrower than the request plus its margins is
/// pulled in. The margin is only enough to keep the window off the screen edge
/// - an upright phone needs every point it can get. The floor keeps a form
/// usable, but never wider than the screen itself.
pub(crate) fn modal_width(requested: f32, screen_width: f32) -> f32 {
    const MARGIN: f32 = 12.0;
    const FLOOR: f32 = 260.0;
    let room = (screen_width - 2.0 * MARGIN).max(0.0);
    let floor = FLOOR.min(room);
    requested.min(room).max(floor)
}

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
    Go2rtcDone(Go2rtcImport),
    /// The go2rtc fetch failed.
    Go2rtcFailed(String),
    /// A playback channel has been given the stretch of recording to play, or
    /// has been told there is none: `window` is `None` then.
    PlaybackReady {
        channel: usize,
        camera_id: String,
        origin: FrigateOrigin,
        credentials: Option<(String, String)>,
        window: Option<(i64, i64)>,
        /// Where playback begins inside that stretch, when it is not its start.
        /// A playlist's range reaches back before the moment the viewer is
        /// opening the recording at, so the picture begins inside it.
        begin: Option<i64>,
        /// The fragments the window was cut into, when the box serves them.
        /// `None` means it does not, and the stretch has to be played as one
        /// `clip.mp4`.
        playlist: Option<VodPlaylist>,
        /// What the camera recorded around the window, for the timeline. Empty
        /// when there is nothing to play.
        segments: Vec<Segment>,
        /// The stretch of time those segments were asked about - which is not
        /// the window: it is read wider, so that a strip either side of the
        /// moment has something to draw. What it says is "the box was asked
        /// about this", empty stretches included.
        read: (i64, i64),
    },
    /// A seek that left the playlist it was playing has been read: the stretch
    /// it asked for, and the fragments it is cut into - or `None` for a box
    /// that serves no playlist, which is played as one `clip.mp4` instead.
    ///
    /// The camera and its login are not carried: they are the ones the playback
    /// channel already has, and a copy here could only disagree with them.
    PlaylistReady {
        /// The playback channel the stretch is for. It is the same channel it
        /// already was: a seek does not open another one.
        index: usize,
        /// The camera whose recording it is, which is what says whether this
        /// answer still belongs to the recording on screen.
        camera_id: String,
        window: (i64, i64),
        /// The moment asked for, which the stretch is played from.
        at: i64,
        playlist: Option<VodPlaylist>,
        /// The seek that asked for it, as `Playback::seek_gen` numbered it.
        ///
        /// `None` when it was read before it was asked for, while a finger was
        /// still on the timeline: such an answer moves nothing by itself, and is
        /// kept until the seek that wants it. An answer that names a seek is
        /// only played if it is still the newest one - answers arrive in
        /// whatever order the box gives them.
        gen: Option<u64>,
    },
    /// When the camera's events began and ended, in the stretch asked for.
    /// `None` when the box could not be asked at all, which is a timeline with
    /// no marks rather than a camera that noticed nothing.
    EventsReady {
        index: usize,
        camera_id: String,
        /// The stretch the answer is for: the list is one window, and a jump or
        /// a widened strip must not read an answer for another as its own.
        from: i64,
        to: i64,
        events: Option<Vec<(i64, i64)>>,
    },
    /// The days the camera recorded on, as the box names them - `2026-10-31`.
    /// `None` when the camera could not be asked at all, which is not the same
    /// as a camera that recorded nothing.
    DaysReady {
        index: usize,
        camera_id: String,
        days: Option<Vec<String>>,
    },
    /// The segment list before what was held: what a timeline wider than an
    /// hour needs in order to be drawn that wide. `None` when the camera could
    /// not be asked at all, which is not the same as a stretch with nothing
    /// recorded in it.
    SegmentsReady {
        index: usize,
        camera_id: String,
        /// The stretch that was asked for. It is what says whether this answer
        /// is the one a picked day is waiting for.
        from: i64,
        to: i64,
        segments: Option<Vec<Segment>>,
    },
    /// Bytes of a clip being saved, from the task that owns it.
    ///
    /// `id` is the number [`crate::app::download::DownloadState`] gave the
    /// task. A cancelled download is aborted at its next await point, but a
    /// progress event may already be in the queue; the id says it is not the
    /// current one's.
    DownloadProgress { id: u64, progress: Progress },
    /// Whether a Frigate camera is recording, from the periodic segment read.
    ///
    /// `active` is `None` when the box could not be asked: the receiver keeps
    /// its last answer then, rather than showing a camera that is recording as
    /// one that stopped because the network blinked.
    RecordingStatus {
        camera_id: String,
        active: Option<bool>,
    },
    /// A clip arrived whole and has its final name on disk.
    DownloadDone { id: u64, path: String },
    /// A clip could not be saved. The reason is shown once, as a toast.
    DownloadFailed { id: u64, reason: String },
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

/// A camera as the entry form holds it.
///
/// The MSE fields are only filled in when the entry carries an MSE address (an
/// imported camera, or one detached from an import): a hand-added camera has
/// none, and the form leaves them out.
#[derive(Debug, Clone, Default)]
pub struct CameraDraft {
    pub name: String,
    pub main: String,
    pub sub: String,
    pub mse_main: String,
    pub mse_sub: String,
    pub username: String,
    pub password: String,
    pub mse_username: String,
    pub mse_password: String,
    pub infer_sub: bool,
    /// How this camera's speaker is reached. See [`TalkMode`].
    pub talk: TalkMode,
}

impl CameraDraft {
    /// Validates the form and builds a camera.
    pub fn build(&self) -> Result<CameraSource, String> {
        let main = self.main.trim();
        if main.is_empty() {
            return Err(monitor_i18n::tr("dialog-stream-url-required"));
        }
        if !is_stream_url(main) {
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
        }
        // The MSE address and its own login, when the entry carries them.
        let mse_main = self.mse_main.trim();
        if !mse_main.is_empty() {
            source.mse_main = Some(mse_main.to_string());
        }
        let mse_sub = self.mse_sub.trim();
        if !mse_sub.is_empty() {
            source.mse_sub = Some(mse_sub.to_string());
        }
        if !self.mse_username.trim().is_empty() {
            source.mse_username = Some(self.mse_username.trim().to_string());
            source.mse_password = Some(self.mse_password.clone());
        }
        source.talk = self.talk;
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
            mse_main: camera.mse_main.clone().unwrap_or_default(),
            mse_sub: camera.mse_sub.clone().unwrap_or_default(),
            username: camera.username.clone().unwrap_or_default(),
            password: camera.password.clone().unwrap_or_default(),
            mse_username: camera.mse_username.clone().unwrap_or_default(),
            mse_password: camera.mse_password.clone().unwrap_or_default(),
            // An existing sub stream is shown as typed, so leave the field
            // editable. One without starts in inference mode, showing what the
            // main url would derive until the viewer unticks it.
            infer_sub: camera.rtsp_sub.is_none(),
            talk: camera.talk,
        }
    }
}

/// Parameters for adding multiple RTSP cameras from one recorder.
#[derive(Debug, Clone)]
pub struct BulkCameraDraft {
    pub name_prefix: String,
    pub main_template: String,
    pub sub_template: String,
    pub username: String,
    pub password: String,
    /// Channel numbers to generate, as `1-4,7,9-11`. Replaces the old
    /// first/count/step triple: real recorders rarely number every channel
    /// with one arithmetic step.
    pub channels: String,
    pub infer_sub: bool,
}

impl Default for BulkCameraDraft {
    fn default() -> Self {
        Self {
            name_prefix: String::new(),
            main_template: String::new(),
            sub_template: String::new(),
            username: String::new(),
            password: String::new(),
            channels: "1-16".into(),
            infer_sub: true,
        }
    }
}
/// Expand `1-4,7,9-11` into channel numbers. Duplicates are dropped in the
/// order they appear, so a pasted list cannot import the same stream twice in
/// one batch. The 256-entry cap matches the old count limit.
fn parse_channels(spec: &str) -> Result<Vec<usize>, String> {
    let invalid = || monitor_i18n::tr("dialog-bulk-error-channels");
    let mut numbers = Vec::new();
    for token in spec.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return Err(invalid());
        }
        if let Some((first, last)) = token.split_once('-') {
            let first = first.trim().parse::<usize>().map_err(|_| invalid())?;
            let last = last.trim().parse::<usize>().map_err(|_| invalid())?;
            if first > last {
                return Err(invalid());
            }
            for number in first..=last {
                if number > 65535 || numbers.len() >= 256 {
                    return Err(invalid());
                }
                if !numbers.contains(&number) {
                    numbers.push(number);
                }
            }
        } else {
            let number = token.parse::<usize>().map_err(|_| invalid())?;
            if number > 65535 || numbers.len() >= 256 {
                return Err(invalid());
            }
            if !numbers.contains(&number) {
                numbers.push(number);
            }
        }
    }
    if numbers.is_empty() {
        return Err(invalid());
    }
    Ok(numbers)
}

impl BulkCameraDraft {
    pub fn build(&self) -> Result<Vec<CameraSource>, String> {
        let channels = parse_channels(&self.channels)?;
        if !self.main_template.contains("{n}") {
            return Err(monitor_i18n::tr("dialog-bulk-error-main-template"));
        }
        if !self.sub_template.trim().is_empty()
            && !self.sub_template.contains("{n}")
        {
            return Err(monitor_i18n::tr("dialog-bulk-error-sub-template"));
        }

        let mut cameras = Vec::with_capacity(channels.len());
        // One checkbox speaks for the whole batch. When the form cannot offer
        // it - no rule for every channel - the builder must not derive for a
        // subset either, or cameras get an address the viewer never saw.
        let derive = self.can_infer_sub() && self.infer_sub;

        for number in channels {
            let prefix = self.name_prefix.trim();
            let name = if prefix.is_empty() {
                monitor_i18n::tr_args("dialog-bulk-camera-name", &[("number", format!("{:02}", number).into())])
            } else {
                monitor_i18n::tr_args("dialog-bulk-camera-name-prefixed", &[("prefix", prefix.into()), ("number", format!("{:02}", number).into())])
            };

            let main = self.main_template.replace("{n}", &number.to_string());
            // The bulk form has no drawn `CameraDraft` field to fill from the
            // main url, so the checkbox has to be resolved here. A template
            // wins whenever one was typed; with neither a template nor the
            // checkbox there is simply no sub stream.
            let sub = if derive {
                infer_sub_stream(&main).unwrap_or_default()
            } else {
                self.sub_template.replace("{n}", &number.to_string())
            };
            let draft = CameraDraft {
                name,
                main,
                sub,
                username: self.username.clone(),
                password: self.password.clone(),
                infer_sub: false,
                ..Default::default()
            };

            cameras.push(draft.build()?);
        }

        Ok(cameras)
    }

    /// How many cameras the Add button will create, or zero while the channel
    /// specification is being edited and cannot be parsed yet.
    pub fn channel_count(&self) -> usize {
        parse_channels(&self.channels).map_or(0, |channels| channels.len())
    }

    /// The first channel for the URL example under the form.
    pub fn first_channel(&self) -> usize {
        parse_channels(&self.channels)
            .ok()
            .and_then(|channels| channels.first().copied())
            .unwrap_or(1)
    }

    /// Whether the derive-sub checkbox has anything to derive on this form.
    /// The single-camera form only offers the check when its main url matches
    /// a rule; the bulk form has to ask the same question of its template, or
    /// the offer would promise an address the builder cannot produce.
    pub fn can_infer_sub(&self) -> bool {
        if !self.sub_template.trim().is_empty() || !self.main_template.contains("{n}") {
            return false;
        }
        let channels = match parse_channels(&self.channels) {
            Ok(channels) if !channels.is_empty() => channels,
            _ => return false,
        };
        // One checkbox speaks for the whole batch, so it is only offered when
        // every channel it will create has a rule to follow. A template like
        // `/stream{n}` cannot keep that promise for `1-3`.
        channels.iter().all(|number| {
            let main = self.main_template.replace("{n}", &number.to_string());
            infer_sub_stream(&main).is_some()
        })
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
    /// to ("on Surveillance Station", "in Frigate"), so it is only drawn on that tab.
    pub error: Option<(Tab, String)>,
    pub message: Option<(Tab, String)>,
    /// An error from a press inside this dialog that the app should also show
    /// as a toast. Background failures already get one in the event loop, so
    /// only the synchronous form paths set this.
    pub toast_error: Option<String>,
    pub manual: CameraDraft,
    pub bulk: BulkCameraDraft,
    /// The manual tab's test button, which needs the runtime the dialog has not
    /// got: the application picks it up after drawing the dialog and plays the
    /// tone the way the edit window's button does.
    pub talk_test: bool,
    pub synology_busy: bool,
    /// Cameras the last Synology fetch returned, waiting to be picked.
    ///
    /// Kept rather than imported on arrival, so the viewer chooses what comes
    /// onto the wall - see [`synology_tab`].
    pub synology_cameras: Vec<CameraSource>,
    /// First control of the window, so that a remote control can be handed the
    /// focus as the window opens rather than leaving it on the wall behind.
    pub focus_anchor: Option<egui::Id>,
    /// The button that opened the window, to give it back its focus when the
    /// window closes - the toolbar's plus, or one of the settings panel's
    /// "Add devices" buttons.
    pub from: Option<egui::Id>,
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
    /// The Frigate restream port, as its text box shows it. Empty means the
    /// port Frigate's own configuration names.
    pub frigate_rtsp_port: Option<String>,
    pub go2rtc_busy: bool,
    /// Streams the last go2rtc fetch returned, waiting to be picked.
    pub go2rtc_cameras: Vec<CameraSource>,
    /// The go2rtc API port, as its text box shows it.
    pub go2rtc_port: Option<String>,
    /// The go2rtc restream port, as its text box shows it. Empty means the
    /// port `rtsp.listen` names.
    pub go2rtc_rtsp_port: Option<String>,
    /// Whether the last go2rtc fetch left the restream account empty and could
    /// not read one from go2rtc either - so a protected restream needs it
    /// entered by hand.
    pub go2rtc_account_missing: bool,
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
            // Answered by the window, not by the device list: nothing about a
            // camera's recordings belongs in these panels.
            BackgroundEvent::PlaybackReady { .. } => {}
            BackgroundEvent::PlaylistReady { .. } => {}
            BackgroundEvent::SegmentsReady { .. } => {}
            BackgroundEvent::DaysReady { .. } => {}
            BackgroundEvent::EventsReady { .. } => {}
            // A camera's recording state belongs on its tile, not in a panel.
            BackgroundEvent::RecordingStatus { .. } => {}
            // A saved clip is answered by the wall's download state, not by a
            // device panel.
            BackgroundEvent::DownloadProgress { .. } => {}
            BackgroundEvent::DownloadDone { .. } => {}
            BackgroundEvent::DownloadFailed { .. } => {}
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
            BackgroundEvent::Go2rtcDone(import) => {
                self.go2rtc_busy = false;
                self.note(Tab::Go2rtc, monitor_i18n::tr_args(
                    "dialog-go2rtc-count",
                    &[("count", import.cameras.len().into())],
                ));
                self.go2rtc_cameras = import.cameras;
                self.go2rtc_account_missing = import.restream_account_missing;
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
            Ok(import) => {
                let _ = sender.send(BackgroundEvent::Go2rtcDone(import));
            }
            Err(err) => {
                let _ = sender.send(BackgroundEvent::Go2rtcFailed(err.to_string()));
            }
        }
    });
}

/// Height of a dialog's title bar: the close cross plus 1px above and below it.
/// The bar is this tall whether or not the dialog has a cross, so that every
/// dialog's title sits at the same height.
pub const CLOSE_SIDE: f32 = 28.0;
pub(crate) const TITLE_EDGE: f32 = 1.0;
pub const TITLE_BAR_H: f32 = CLOSE_SIDE + 2.0 * TITLE_EDGE;
/// Gap between the dialog's left border and the title text.
pub(crate) const TITLE_INSET: f32 = 4.0;
/// Nudge of the title text, to correct the glyphs' optical centre (+ is down).
const TITLE_DY: f32 = 0.0;

/// What [`title_bar`] hands back.
pub struct TitleBar {
    /// The close cross, when the dialog has one. Whether it joins the
    /// navigation layer is left to the caller.
    pub close: Option<egui::Response>,
    /// The 1px rule under the bar, for a caller that anchors the focus on it.
    pub rule: egui::Rect,
}

/// Draws a dialog's title bar - the heading, an optional close cross and the
/// rule under them - flush to the dialog's border.
///
/// Must be the first thing drawn in an `egui::Modal` (after `set_width`): the
/// modal's frame keeps a margin around its content, and the bar is painted out
/// into that margin so that the cross sits 1px from the border, and the rule
/// runs from border to border.
pub fn title_bar(ui: &mut egui::Ui, title: &str, closable: bool, close_tip: &str) -> TitleBar {
    let margin = ui.style().spacing.menu_margin;
    let (ml, mr, mt) = (f32::from(margin.left), f32::from(margin.right), f32::from(margin.top));
    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    let origin = ui.cursor().min;
    let width = ui.available_width();

    // The bar starts at the frame's own corner, not at the content's.
    let bar = egui::Rect::from_min_size(
        egui::pos2(origin.x - ml, origin.y - mt),
        egui::vec2(width + ml + mr, TITLE_BAR_H),
    );
    // The content already starts `mt` below the frame's top; reserve the rest.
    ui.allocate_exact_size(
        egui::vec2(width, TITLE_BAR_H + stroke.width - mt),
        egui::Sense::hover(),
    );

    let right = if closable { bar.right() - TITLE_BAR_H } else { bar.right() };
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(bar.left() + TITLE_INSET, bar.top() + TITLE_DY),
        egui::pos2(right, bar.bottom() + TITLE_DY),
    );
    let mut text_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    text_ui.add(egui::Label::new(RichText::new(title).heading()).truncate());

    let close = closable.then(|| {
        let rect = egui::Rect::from_min_size(
            egui::pos2(
                bar.right() - TITLE_EDGE - CLOSE_SIDE,
                bar.top() + TITLE_EDGE,
            ),
            egui::vec2(CLOSE_SIDE, CLOSE_SIDE),
        );
        let mut button_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        // A hovered or focused button grows by 1px; here that would put it
        // against the border and the rule.
        let widgets = &mut button_ui.visuals_mut().widgets;
        widgets.hovered.expansion = 0.0;
        widgets.active.expansion = 0.0;
        icons::button_sized(&mut button_ui, Icon::Close, false, false, close_tip, CLOSE_SIDE)
    });

    let rule = egui::Rect::from_min_size(egui::pos2(bar.left(), bar.bottom()), egui::vec2(bar.width(), stroke.width));
    ui.painter().rect_filled(rule, 0.0, stroke.color);
    TitleBar { close, rule }
}

/// Draws the "Add devices" dialog. Returns `true` when the configuration changed.
pub fn add_devices_window(
    ctx: &egui::Context,
    state: &mut DiscoveryUi,
    config: &mut AppConfig,
    handle: &Handle,
    events: &Sender<BackgroundEvent>,
    nav: &mut Nav,
    body_scroll: &mut Option<f32>,
) -> bool {
    let mut changed = false;
    let mut close = false;

    // A modal, not a window: it centres itself, and its backdrop swallows the
    // presses that would otherwise land behind it. As a plain window the bar
    // stayed live underneath, so a second press on "Add devices" toggled the
    // dialog shut and the settings panel could be rearranged from under it.
    egui::Modal::new(egui::Id::new("xgview-add-devices")).show(ctx, |ui| {
        ui.set_width(modal_width(620.0, ctx.screen_rect().width()));
        // The title row carries the way out, the way the layout picker's does:
        // a cross at the far end, for a mouse or a finger. It is deliberately
        // not registered with the navigation layer - a remote leaves the dialog
        // with Back, and a second stop for the same thing would only be in the
        // way of the tab strip below.
        let bar = title_bar(
            ui,
            &group_heading(
                config,
                config.active_index(),
                "dialog-add-devices",
                "dialog-add-devices-group",
            ),
            true,
            &monitor_i18n::tr("dialog-close"),
        );
        if bar.close.is_some_and(|close_button| close_button.clicked()) {
            close = true;
        }

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
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // A Down off the last control has nowhere else to go. The
                // settings panel spends the same request from its scroll area;
                // here it lets a remote reach the far end of a long form.
                if let Some(points) = body_scroll.take() {
                    ui.scroll_with_delta(egui::vec2(0.0, -points));
                }
                match state.tab {
                    Tab::Onvif => changed |= onvif_tab(ui, state, config, handle, events, nav),
                    Tab::Manual => changed |= manual_tab(ui, state, config, nav),
                    Tab::Synology => changed |= synology_tab(ui, state, config, handle, events, nav),
                    Tab::Frigate => changed |= frigate_tab(ui, state, config, handle, events, nav),
                    Tab::Go2rtc => changed |= go2rtc_tab(ui, state, config, handle, events, nav),
                }
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
/// The dialog's way out is not here: it is the cross in the title row above,
/// which a remote reaches with Back rather than with the arrows.
fn tab_strip(ui: &mut egui::Ui, state: &mut DiscoveryUi, nav: &mut Nav) {
    const TABS: [(Tab, &str); 5] = [
        (Tab::Onvif, "dialog-tab-onvif"),
        (Tab::Manual, "dialog-tab-manual"),
        (Tab::Synology, "dialog-tab-synology"),
        (Tab::Frigate, "dialog-tab-frigate"),
        (Tab::Go2rtc, "dialog-tab-go2rtc"),
    ];

    let mut ids = [egui::Id::NULL; TABS.len()];
    let mut focused = None;

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
        // where the Synology list can compare the whole camera. It is judged
        // within the group the wall is on: the same device may sit in another
        // group as an entry of its own.
        let added = config
            .cameras
            .iter()
            .any(|known| config.in_active_group(known) && known.host.eq_ignore_ascii_case(&host));
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
    /// Whether the "Detach" button was pressed - the camera is to leave the
    /// import's hands, its addresses becoming editable by hand.
    pub detach: bool,
    /// Whether the test tone was asked for. The form cannot play it: that needs
    /// the runtime and a channel, and both belong to the window that owns it.
    pub test: bool,
}

/// A camera field, drawn greyed and uneditable while the entry is managed by an
/// import, so the layout still shows what the import put there.
fn field_text(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    text: &mut String,
    label: &str,
    locked: bool,
    build: impl for<'a> FnOnce(&'a mut String) -> egui::TextEdit<'a>,
) -> egui::Response {
    if locked {
        ui.add_enabled(false, build(text))
    } else {
        text_field(ui, nav, text, label, false, false, build)
    }
}

/// A hidden infer check still remembers having been ticked. When its rule is
/// gone and the viewer types over the address it left behind, that address is
/// theirs: the check must not come back later and grey it away.
fn keep_a_hand_typed_sub(infer_ok: bool, changed: bool, infer_sub: &mut bool) {
    if !infer_ok && changed {
        *infer_sub = false;
    }
}

/// The camera fields, as the "Manual entry" tab and the settings panel's edit
/// window both lay them out.
///
/// `grid_id` names the grid - egui keys a grid's state on it, so the two must
/// not share one.
///
/// `camera` is the entry being edited, when there is one. An entry an import
/// brought in (`imported`) is drawn with its address and credential fields
/// greyed and uneditable, because hand-editing one address of a managed pair -
/// a Frigate or go2rtc camera's RTSP and MSE ones - would leave the other
/// pointing at a different stream; the name stays editable, and a "Detach"
/// button offers to take the camera over (see [`CameraSource::detach`]). The MSE
/// fields only appear when the entry carries an MSE address, so a hand-added
/// camera keeps the short form. See [`CameraFields`] for what comes back.
pub fn camera_fields(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    grid_id: &str,
    draft: &mut CameraDraft,
    camera: Option<&CameraSource>,
) -> CameraFields {
    let locked = camera.is_some_and(|camera| camera.imported);
    let mut first = None;
    let mut detach = false;
    let mut test = false;
    // The MSE rows appear only when the entry carries an MSE address: an
    // imported camera, or one detached from an import.
    let has_mse = !draft.mse_main.trim().is_empty();
    // The vendor sub-stream rules only match a camera's own address. A go2rtc or
    // Frigate restream is a stream name, and its addresses are the import's, so
    // the "infer" offer is drawn only on a hand-carried camera whose url matches
    // a rule - a manual or ONVIF one.
    let infer_ok = !camera.is_some_and(|camera| camera.is_restream())
        && infer_sub_stream(&draft.main).is_some();
    // The sub field's placeholder says how it may be filled: from the main url
    // where a vendor rule can read one, and only "optional" where none can.
    let sub_hint = if infer_ok { "dialog-sub-hint" } else { "dialog-sub-hint-plain" };

    egui::Grid::new(grid_id).num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label(monitor_i18n::tr("dialog-name"));
        // The first control of the form, and the one a window opens on. Always
        // editable: a name holds no address that could fall out of step.
        let name = text_field(ui, nav, &mut draft.name, &monitor_i18n::tr("dialog-name"), false, false, |s| {
            egui::TextEdit::singleline(s).hint_text(theme::hint(&monitor_i18n::tr("dialog-name-hint"))).desired_width(f32::INFINITY)
        });
        first = Some(name.id);
        ui.end_row();

        ui.label(monitor_i18n::tr("dialog-main-stream"));
        field_text(ui, nav, &mut draft.main, &monitor_i18n::tr("dialog-main-stream"), locked, |s| {
            egui::TextEdit::singleline(s)
                .hint_text(theme::hint(&monitor_i18n::tr("dialog-main-stream-hint")))
                .desired_width(f32::INFINITY)
        });
        ui.end_row();

        ui.label(monitor_i18n::tr("dialog-sub-stream"));
        // The sub field fills its column like the name and main fields do, so
        // all three share one right edge. Ticking "derive sub" fills this field
        // with the address read off the main url and locks it grey; unticking
        // keeps that address and makes the field editable again. If the main
        // url stops matching a rule, the check goes away and the last derived
        // address stays visible and editable; typing over it makes the address
        // the viewer's own, so a rule matching again later cannot take it back.
        // Nothing is inferred at save time - what is shown is what gets saved.
        if !locked && infer_ok && draft.infer_sub {
            // Keep the field equal to the derived address for as long as the
            // check is on - including the frame the main url first becomes
            // inferable, so a default-checked form is never an empty grey box.
            // Unticking stops the write and leaves this address editable.
            if let Some(derived) = infer_sub_stream(&draft.main) {
                draft.sub = derived;
            }
            let mut sub = draft.sub.as_str();
            ui.add_enabled(
                false,
                egui::TextEdit::singleline(&mut sub)
                    .hint_text(theme::hint(&monitor_i18n::tr("dialog-sub-hint")))
                    .desired_width(f32::INFINITY),
            );
        } else {
            let sub = field_text(ui, nav, &mut draft.sub, &monitor_i18n::tr("dialog-sub-stream"), locked, |s| {
                egui::TextEdit::singleline(s)
                    .hint_text(theme::hint(&monitor_i18n::tr(sub_hint)))
                    .desired_width(f32::INFINITY)
            });
            // The check may still be set from a frame when a rule applied.
            // Editing the visible field while no rule applies makes that
            // address manual; otherwise a rule coming back would grey the
            // field again and quietly replace what the viewer just typed.
            keep_a_hand_typed_sub(infer_ok, sub.changed(), &mut draft.infer_sub);
        }
        ui.end_row();

        // The MSE address the import also captured, so both halves of the pair
        // are in front of the user and stay in step when edited by hand.
        if has_mse {
            ui.label(monitor_i18n::tr("dialog-mse-main"));
            field_text(ui, nav, &mut draft.mse_main, &monitor_i18n::tr("dialog-mse-main"), locked, |s| {
                egui::TextEdit::singleline(s)
                    .hint_text(theme::hint("wss://host:8971/live/mse/api/ws?src=…"))
                    .desired_width(f32::INFINITY)
            });
            ui.end_row();

            ui.label(monitor_i18n::tr("dialog-mse-sub"));
            field_text(ui, nav, &mut draft.mse_sub, &monitor_i18n::tr("dialog-mse-sub"), locked, |s| {
                egui::TextEdit::singleline(s).desired_width(f32::INFINITY)
            });
            ui.end_row();
        }
    });
    if !locked && infer_ok {
        nav.tracked(ui.checkbox(&mut draft.infer_sub, monitor_i18n::tr("dialog-derive-sub")));
    }

    // The stream account in the layout every other tab gives one: a heading
    // naming it, then the two fields on one row.
    ui.separator();
    ui.label(RichText::new(monitor_i18n::tr("dialog-stream-credentials")).small());
    credentials_fields(ui, nav, &mut draft.username, &mut draft.password, locked);

    if has_mse {
        ui.add_space(6.0);
        ui.label(RichText::new(monitor_i18n::tr("dialog-mse-credentials")).small());
        credentials_fields(ui, nav, &mut draft.mse_username, &mut draft.mse_password, locked);
    }

    // How this camera's speaker is reached, and whether it is reached at all. A
    // camera cannot tell whether anything is plugged into its speaker jack and
    // will happily play into an empty one, so this is the owner's call. A
    // cycling button like the rest of the settings: the popup a drop-down opens
    // is egui's own and the remote cannot walk into it.
    ui.add_space(6.0);
    egui::Grid::new(format!("{grid_id}-talk")).num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label(monitor_i18n::tr("dialog-talk"));
        ui.horizontal(|ui| {
            controls::cycle_button(nav, ui, &TalkMode::ALL, &mut draft.talk, |mode| {
                monitor_i18n::tr(mode.label())
            });
            // The tone is the only way to find out whether anything is plugged
            // into the camera's speaker jack: the camera itself cannot say.
            if nav.tracked(ui.button(monitor_i18n::tr("camera-talk-test"))).clicked() {
                test = true;
            }
        });
        ui.end_row();
    });

    if locked {
        ui.add_space(6.0);
        ui.label(
            RichText::new(monitor_i18n::tr("camera-edit-imported-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        if nav.tracked(ui.button(monitor_i18n::tr("camera-edit-detach"))).clicked() {
            detach = true;
        }
    }

    CameraFields { first, detach, test }
}

/// A credentials row, editable or greyed - the shape a managed camera's account
/// takes until it is detached.
fn credentials_fields(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    username: &mut String,
    password: &mut String,
    locked: bool,
) {
    if locked {
        ui.horizontal(|ui| {
            let user = monitor_i18n::tr("dialog-user");
            let pass = monitor_i18n::tr("dialog-password");
            ui.label(user);
            ui.add_enabled(false, egui::TextEdit::singleline(username).desired_width(140.0));
            ui.label(pass);
            ui.add_enabled(false, egui::TextEdit::singleline(password).password(true).desired_width(140.0));
        });
    } else {
        credentials_row(ui, nav, username, password);
    }
}

/// A stream is a duplicate only within the group currently being edited.
fn bulk_camera_exists(config: &AppConfig, source: &CameraSource) -> bool {
    let active_group = config.active_index();
    config.cameras.iter().any(|known| {
        usize::from(known.group) == active_group
            && known.rtsp_main == source.rtsp_main
    })
}
fn manual_tab(ui: &mut egui::Ui, state: &mut DiscoveryUi, config: &mut AppConfig, nav: &mut Nav) -> bool {
    let mut changed = false;
    let fields = camera_fields(ui, nav, "manual-camera", &mut state.manual, None);
    // The dialog has no runtime to play a tone with, so the press is handed
    // back to the application, which plays it like the edit window's button.
    if fields.test {
        state.talk_test = true;
    }

    // The form and its two buttons are two groups, not one run of controls: the
    // same gap the edit window puts before Save and Cancel. Without it the last
    // row of the form - the talk setting, which is a cycling button and a button
    // of its own - reads as part of the row that adds the camera.
    ui.add_space(theme::space::M);
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
                Err(err) => {
                    state.fail(Tab::Manual, err.clone());
                    state.toast_error = Some(err);
                },
            }
        }
        if nav.tracked(ui.button(monitor_i18n::tr("dialog-clear"))).clicked() {
            state.manual.reset();
        }
    });

    ui.add_space(theme::space::M);
    ui.separator();
    ui.label(monitor_i18n::tr("dialog-bulk-title"));
    ui.label(
        RichText::new(monitor_i18n::tr("dialog-bulk-hint"))
            .small()
            .color(theme::TEXT_DIM),
    );

    ui.label(monitor_i18n::tr("dialog-bulk-recorder-name"));
    text_field(ui, nav, &mut state.bulk.name_prefix, &monitor_i18n::tr("dialog-bulk-recorder-name"), false, false, |s| {
        egui::TextEdit::singleline(s)
            .desired_width(f32::INFINITY)
            .hint_text(theme::hint(&monitor_i18n::tr("dialog-bulk-recorder-placeholder")))
    });

    ui.label(monitor_i18n::tr("dialog-bulk-main-template"));
    text_field(ui, nav, &mut state.bulk.main_template, &monitor_i18n::tr("dialog-bulk-main-template"), false, false, |s| {
        egui::TextEdit::singleline(s)
            .desired_width(f32::INFINITY)
            .hint_text(theme::hint("rtsp://192.168.1.100/Streaming/Channels/{n}01"))
    });

    ui.label(monitor_i18n::tr("dialog-bulk-sub-template"));
    text_field(ui, nav, &mut state.bulk.sub_template, &monitor_i18n::tr("dialog-bulk-sub-template"), false, false, |s| {
        egui::TextEdit::singleline(s)
            .desired_width(f32::INFINITY)
            .hint_text(theme::hint("rtsp://192.168.1.100/Streaming/Channels/{n}02"))
    });
    // With no template typed, the sub addresses can be read off the main
    // template instead, but only when that template has a rule to follow.
    // Keep the offer next to the field it stands in for.
    if state.bulk.can_infer_sub() {
        nav.tracked(ui.checkbox(&mut state.bulk.infer_sub, monitor_i18n::tr("dialog-bulk-infer-sub")));
    }

    ui.label(RichText::new(monitor_i18n::tr("dialog-bulk-credentials")).small());
    credentials_row(ui, nav, &mut state.bulk.username, &mut state.bulk.password);

    ui.label(monitor_i18n::tr("dialog-bulk-channels"));
    text_field(ui, nav, &mut state.bulk.channels, &monitor_i18n::tr("dialog-bulk-channels"), false, false, |s| {
        egui::TextEdit::singleline(s)
            .desired_width(f32::INFINITY)
            .hint_text(theme::hint("1-16, 20, 23-24"))
    });

    if !state.bulk.main_template.trim().is_empty() {
        let first_url = state.bulk.main_template
            .replace("{n}", &state.bulk.first_channel().to_string());
        ui.label(RichText::new(monitor_i18n::tr_args("dialog-bulk-example", &[("url", first_url.into())]))
            .small()
            .color(theme::TEXT_DIM));
    }

    let button_label = monitor_i18n::tr_args("dialog-bulk-add", &[("count", state.bulk.channel_count().into())]);
    if nav.tracked(ui.button(button_label)).clicked() {
        match state.bulk.build() {
            Ok(cameras) => {
                let mut added = 0usize;
                let mut skipped = 0usize;

                for source in cameras {
                    let duplicate = bulk_camera_exists(config, &source);

                    if duplicate {
                        skipped += 1;
                    } else {
                        config.upsert_camera(source);
                        added += 1;
                    }
                }

                state.clear_status(Tab::Manual);
                state.note(
                    Tab::Manual,
                    monitor_i18n::tr_args("dialog-bulk-result", &[("added", added.into()), ("skipped", skipped.into())]),
                );
                changed |= added > 0;
            }
            Err(err) => {
                state.fail(Tab::Manual, err.clone());
                state.toast_error = Some(err);
            },
        }
    }
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

/// What a group goes by: its own name, or - when it has none - the word the
/// built-in first group goes by, and "Group N" for the ones after it.
///
/// The settings page, the add-devices dialog, the empty wall and the status bar
/// all name the same group, so they name it the same way.
pub fn group_name(config: &AppConfig, index: usize) -> String {
    let Some(group) = config.groups.get(index) else {
        return String::new();
    };
    let named = group.name.trim();
    if !named.is_empty() {
        return named.to_string();
    }
    if index == 0 {
        monitor_i18n::tr("settings-group-builtin")
    } else {
        monitor_i18n::tr_args("settings-group-default", &[("n", (index + 1).into())])
    }
}

/// The heading of something that belongs to one group: `named` with the group
/// in it when there is more than one group to tell apart, and `plain` when the
/// built-in one is all there is - with a single group the word would be on every
/// heading and tell the viewer nothing.
pub fn group_heading(config: &AppConfig, index: usize, plain: &str, named: &str) -> String {
    if config.groups.len() > 1 {
        monitor_i18n::tr_args(named, &[("group", group_name(config, index).into())])
    } else {
        monitor_i18n::tr(plain)
    }
}

/// The heading of the remove confirmation.
///
/// The camera's name, and the group it is leaving when there is more than one to
/// tell apart: two groups can hold a camera of the same name, and the question is
/// about one entry of one of them - the same reason the edit window names the
/// group it is editing in.
pub fn remove_title(config: &AppConfig, group: usize, name: &str) -> String {
    if config.groups.len() > 1 {
        monitor_i18n::tr_args(
            "remove-title-group",
            &[("name", name.into()), ("group", group_name(config, group).into())],
        )
    } else {
        monitor_i18n::tr_args("remove-title", &[("name", name.into())])
    }
}

/// Whether a listed camera is already on the wall, and whether importing it
/// would change anything.
///
/// "The same camera" is [`CameraSource::identity`] - scheme, host, port and
/// path, with the way the address is written taken out - so a camera added by
/// hand, by ONVIF or by Synology is all recognised, and not only the ones this
/// tab imported. It is searched for in the group the wall is on: the groups are
/// not deduplicated against each other, so the same camera sitting in another
/// group is a separate entry, and adding it here is a new one.
fn add_offer(config: &AppConfig, camera: &CameraSource) -> AddOffer {
    let known = config
        .cameras
        .iter()
        .find(|known| config.in_active_group(known) && known.identity() == camera.identity());
    let Some(known) = known else {
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
/// id, origin, enabled, transport, aspect, tags - are left out: an import never
/// touches them, so a difference there is no reason to offer a refresh.
///
/// `frigate` is here for the opposite reason: only an import writes it, and a
/// configuration from before 2.1 has none. Left out, re-importing an older
/// Frigate camera reads as "already added", the button that would fill it in is
/// greyed out, and that camera never gets its recordings button.
fn same_settings(a: &CameraSource, b: &CameraSource) -> bool {
    a.name == b.name
        && a.rtsp_main == b.rtsp_main
        && a.rtsp_sub == b.rtsp_sub
        && a.username == b.username
        && a.password == b.password
        && a.main_profile == b.main_profile
        && a.sub_profile == b.sub_profile
        && a.frigate == b.frigate
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
    let port = synology.port;
    let port_box = state.synology_port.get_or_insert_with(|| port.to_string());
    server_grid(
        ui, nav, "synology", &mut synology.host, &mut synology.port, port_box,
        &mut synology.https,
    );

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
            // fetch would invite a press on a camera Surveillance Station may not
            // return.
            state.synology_cameras.clear();
            start_synology(handle, events, synology.clone());
        }
        if state.synology_busy {
            grid_spinner(ui);
            ui.label(RichText::new(monitor_i18n::tr("dialog-contacting-nas")).color(theme::ACCENT));
        }
    });

    ui.separator();

    // What Surveillance Station answered, one row per camera: added on the
    // viewer's press, like the ONVIF device list, rather than all at once as
    // they arrive.
    ui.add_space(6.0);
    ui.label(RichText::new(monitor_i18n::tr_args("dialog-cameras-on-nas", &[("count", state.synology_cameras.len().into())])).strong());
    if state.synology_cameras.is_empty() {
        ui.label(RichText::new(monitor_i18n::tr("dialog-nothing-fetched")).small().color(theme::TEXT_DIM));
    }
    if let Some((camera, offer)) = camera_rows(ui, nav, config, &state.synology_cameras) {
        apply_add(config, state, Tab::Synology, camera, offer);
        changed = true;
    }

    status_lines(ui, state, Tab::Synology);
    changed
}

/// Where a server is, which port it answers on, and whether the connection is
/// over TLS - the three fields every tab that reaches out asks for first.
///
/// One function because the port is typed in all three, and a typed port is the
/// same dance each time: a text box rather than a drag value, because on Android
/// a typed field is the one the floating input box mirrors; only digits kept, at
/// most five; the number taken from the text while it names a valid port, so the
/// box can be cleared to retype it without the port being lost; and the port in
/// use shown again once the box is left.
fn server_grid(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    id: &str,
    host: &mut String,
    port: &mut u16,
    port_box: &mut String,
    https: &mut bool,
) {
    egui::Grid::new(id).num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
        ui.label(monitor_i18n::tr("dialog-host"));
        // The first control of its tab, and the one the strip's Down lands on.
        text_field(ui, nav, host, &monitor_i18n::tr("dialog-host"), false, false, |s| {
            egui::TextEdit::singleline(s).hint_text(theme::hint("192.168.1.10")).desired_width(220.0)
        });
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-port"));
        let field = port_field(ui, nav, port_box, &monitor_i18n::tr("dialog-port"), 220.0);
        if let Ok(typed) = port_box.parse::<u16>() {
            if typed >= 1 {
                *port = typed;
            }
        }
        if !field.has_focus() {
            *port_box = port.to_string();
        }
        ui.end_row();
        ui.label(monitor_i18n::tr("dialog-scheme"));
        nav.tracked(ui.checkbox(https, monitor_i18n::tr("dialog-https")));
        ui.end_row();
    });
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

/// A port box, and the one rule every port in this program is typed under.
///
/// A text box rather than a drag value, because a port is typed and on Android a
/// typed field is the one the floating input box mirrors. Only digits are kept,
/// at most five. What the text then means belongs to the caller: a port that is
/// required takes it while it names one, and an optional one is cleared with the
/// box - see [`restream_port_row`] for that half.
fn port_field(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    text: &mut String,
    tip: &str,
    width: f32,
) -> egui::Response {
    let field = text_field(ui, nav, text, tip, false, false, |s| {
        egui::TextEdit::singleline(s).desired_width(width).char_limit(5)
    });
    text.retain(|c| c.is_ascii_digit());
    field
}

/// Draws the restream-port field: empty reads the port from the server.
///
/// The box keeps only digits; the port follows it while the text names a valid
/// one, and is cleared - so the server decides - when the box is emptied.
fn restream_port_row(ui: &mut egui::Ui, nav: &mut Nav, text: &mut String, port: &mut Option<u16>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(monitor_i18n::tr("dialog-rtsp-port")).small());
        let field = port_field(ui, nav, text, &monitor_i18n::tr("dialog-rtsp-port"), 70.0);
        *port = text.parse::<u16>().ok().filter(|port| *port >= 1);
        if !field.has_focus() {
            *text = port_text(*port);
        }
    });
    ui.label(RichText::new(monitor_i18n::tr("dialog-rtsp-port-hint")).small().color(theme::TEXT_DIM));
}

/// A port as its text box shows it - empty when unset.
fn port_text(port: Option<u16>) -> String {
    port.map(|port| port.to_string()).unwrap_or_default()
}

/// Draws a list of fetched cameras, one row each, and reports the one whose
/// button was pressed.
///
/// What all three server tabs list before any of it is added: the Synology box,
/// the Frigate server and the go2rtc one fetch the same shape and offer the same
/// three answers.
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

/// Re-applies the Frigate settings to the cameras already fetched, when the
/// protocol, the address or a credential changes, so the server does not have
/// to be contacted again.
///
/// Both addresses are held on every camera, so the two set is refreshed from the
/// stream name each carries and the connection mode from the tab's choice: the
/// cameras are the same, only what XGView prefers and the addresses it holds
/// change.
fn rewrite_frigate_urls(frigate: &FrigateConfig, cameras: &mut [CameraSource]) {
    let restream_port = cameras
        .first()
        .and_then(|c| extract_rtsp_port(&c.rtsp_main))
        .or(frigate.rtsp_port)
        .unwrap_or(8554);
    let connect = frigate.protocol.connection_mode();
    for camera in cameras {
        if let Some(stream) = extract_stream(&camera.rtsp_main) {
            camera.rtsp_main = frigate_rtsp_url(frigate, restream_port, &stream);
            camera.mse_main = Some(frigate_mse_url(frigate, &stream));
        }
        if let Some(sub) = camera.rtsp_sub.as_mut() {
            if let Some(stream) = extract_stream(sub) {
                *sub = frigate_rtsp_url(frigate, restream_port, &stream);
                camera.mse_sub = Some(frigate_mse_url(frigate, &stream));
            }
        }
        camera.connect = connect;
        // The restream account, for the RTSP address - kept as the import read
        // it when the tab names none of its own.
        if !frigate.rtsp_username.trim().is_empty() {
            camera.username = Some(frigate.rtsp_username.clone());
            camera.password = Some(frigate.rtsp_password.clone());
        }
        // Frigate's own login, for the WebSocket stream.
        if !frigate.username.trim().is_empty() {
            camera.mse_username = Some(frigate.username.clone());
            camera.mse_password = Some(frigate.password.clone());
        } else {
            camera.mse_username = None;
            camera.mse_password = None;
        }
        // The recordings' address moves with the rest. It is the API's own
        // host, port and scheme - not the restream's - and a server whose
        // address was just edited would otherwise leave playback pointing at
        // the one it used to answer on.
        if let Some(origin) = camera.frigate.as_mut() {
            origin.base_url = frigate.base_url();
        }
    }
}

/// The protocol choice a server import offers: both addresses are always
/// captured, so this only picks which one the imported cameras start on - a
/// fallback from one to the other, or one forced. `https` only renames the
/// WebSocket stop, `wss` or `ws`.
fn protocol_choice(
    ui: &mut egui::Ui,
    nav: &mut Nav,
    protocol: &mut StreamProtocol,
    https: bool,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(monitor_i18n::tr("dialog-stream-protocol")).small());
        nav.tracked(ui.radio_value(protocol, StreamProtocol::Auto, monitor_i18n::tr("dialog-protocol-auto")));
        nav.tracked(ui.radio_value(protocol, StreamProtocol::Rtsp, monitor_i18n::tr("dialog-protocol-rtsp")));
        let ws_label = if https { "wss" } else { "ws" };
        nav.tracked(ui.radio_value(protocol, StreamProtocol::Mse, ws_label));
    });
    ui.label(
        RichText::new(monitor_i18n::tr(match protocol {
            StreamProtocol::Auto => "dialog-protocol-auto-hint",
            StreamProtocol::Rtsp => "dialog-protocol-rtsp-hint",
            StreamProtocol::Mse => "dialog-protocol-ws-hint",
        }))
        .small()
        .color(theme::TEXT_DIM),
    );
}

/// Re-applies the go2rtc settings to the streams already fetched, when the
/// protocol, the address or a credential changes, so the server does not have
/// to be contacted again.
fn rewrite_go2rtc_urls(go2rtc: &Go2rtcConfig, cameras: &mut [CameraSource]) {
    let restream_port = cameras
        .first()
        .and_then(|c| extract_rtsp_port(&c.rtsp_main))
        .or(go2rtc.rtsp_port)
        .unwrap_or(8554);
    let connect = go2rtc.protocol.connection_mode();
    for camera in cameras {
        if let Some(stream) = extract_stream(&camera.rtsp_main) {
            camera.rtsp_main = go2rtc_rtsp_url(&go2rtc.host, restream_port, &stream);
            camera.mse_main = Some(go2rtc_mse_url(go2rtc, &stream));
        }
        camera.connect = connect;
        // The restream account, for the RTSP address.
        if !go2rtc.rtsp_username.trim().is_empty() {
            camera.username = Some(go2rtc.rtsp_username.clone());
            camera.password = Some(go2rtc.rtsp_password.clone());
        }
        // go2rtc's `api:` account, for the WebSocket address.
        if !go2rtc.api_username.trim().is_empty() {
            camera.mse_username = Some(go2rtc.api_username.clone());
            camera.mse_password = Some(go2rtc.api_password.clone());
        } else {
            camera.mse_username = None;
            camera.mse_password = None;
        }
    }
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
    let port = frigate.port;
    let port_box = state.frigate_port.get_or_insert_with(|| port.to_string());
    server_grid(
        ui, nav, "frigate", &mut frigate.host, &mut frigate.port, port_box,
        &mut frigate.https,
    );

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
        // The protocol is a preference between two addresses both of which are
        // captured, so it can be switched without contacting the server again.
        protocol_choice(ui, nav, &mut frigate.protocol, frigate.https);

        // The restream account belongs below the separator with the streams. It
        // is always shown: the RTSP address is captured whichever protocol is
        // preferred, and a switch back to it must find the account in place.
        ui.label(RichText::new(monitor_i18n::tr("dialog-rtsp-credentials")).small());
        credentials_row(ui, nav, &mut frigate.rtsp_username, &mut frigate.rtsp_password);
        ui.label(
            RichText::new(monitor_i18n::tr("dialog-rtsp-override-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        restream_port_row(
            ui,
            nav,
            state.frigate_rtsp_port.get_or_insert_with(|| port_text(frigate.rtsp_port)),
            &mut frigate.rtsp_port,
        );
    }
    if *frigate != before {
        // The cameras already fetched hold both addresses, so a change to the
        // protocol, the address or a credential is applied to them here rather
        // than waiting for another fetch.
        rewrite_frigate_urls(frigate, &mut state.frigate_cameras);
        rewrite_frigate_urls(frigate, &mut state.frigate_leftovers);
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
    let port = go2rtc.port;
    let port_box = state.go2rtc_port.get_or_insert_with(|| port.to_string());
    server_grid(
        ui, nav, "go2rtc", &mut go2rtc.host, &mut go2rtc.port, port_box,
        &mut go2rtc.https,
    );

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
            state.go2rtc_account_missing = false;
            start_go2rtc(handle, events, go2rtc.clone());
        }
        if state.go2rtc_busy {
            grid_spinner(ui);
            ui.label(RichText::new(monitor_i18n::tr("dialog-contacting-server")).color(theme::ACCENT));
        }
    });

    ui.separator();

    // The RTSP account is a stream account, so it belongs below the separator
    // with the streams. go2rtc's `/api` does not serve it, but `/api/config`
    // serves the file it is set in, so it is only worth showing once there are
    // streams to pull.
    if !state.go2rtc_cameras.is_empty() {
        ui.add_space(6.0);
        // The protocol is a preference between two addresses both of which are
        // captured, so it can be switched without contacting the server again.
        protocol_choice(ui, nav, &mut go2rtc.protocol, go2rtc.https);

        ui.label(RichText::new(monitor_i18n::tr("dialog-rtsp-credentials")).small());
        credentials_row(ui, nav, &mut go2rtc.rtsp_username, &mut go2rtc.rtsp_password);
        // No account could be read, so there is nothing to fall back to: say so
        // rather than invite the account to be left empty.
        if state.go2rtc_account_missing && go2rtc.rtsp_username.trim().is_empty() {
            ui.label(RichText::new(monitor_i18n::tr("dialog-go2rtc-account-missing")).small().color(theme::WARN));
        } else {
            ui.label(RichText::new(monitor_i18n::tr("dialog-go2rtc-rtsp-hint")).small().color(theme::TEXT_DIM));
        }
        restream_port_row(
            ui,
            nav,
            state.go2rtc_rtsp_port.get_or_insert_with(|| port_text(go2rtc.rtsp_port)),
            &mut go2rtc.rtsp_port,
        );
    }
    if *go2rtc != before {
        // The streams already fetched hold both addresses, so a change to the
        // protocol, the address or a credential is applied to them here rather
        // than waiting for another fetch.
        rewrite_go2rtc_urls(go2rtc, &mut state.go2rtc_cameras);
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
/// to ("on Surveillance Station", "in Frigate"), so anything tagged with another tab is left
/// out rather than read where it does not apply.
fn status_lines(ui: &mut egui::Ui, state: &DiscoveryUi, tab: Tab) {
    let message = state.message.as_ref().filter(|(other, _)| *other == tab);
    let error = state.error.as_ref().filter(|(other, _)| *other == tab);
    if message.is_none() && error.is_none() {
        return;
    }
    ui.add_space(8.0);
    if let Some((_, message)) = message {
        ui.label(RichText::new(message).color(theme::LIVE));
    }
    if let Some((_, error)) = error {
        ui.label(RichText::new(error).color(theme::ERROR));
    }
}

fn grid_spinner(ui: &mut egui::Ui) {
    ui.add(egui::Spinner::new().size(16.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_camera_address_field_fills_its_column() {
        let source = include_str!("dialogs.rs");
        let form = source.find("pub fn camera_fields").unwrap();
        let end = source[form..].find("fn credentials_fields").unwrap() + form;
        let body = &source[form..end];
        // Name, main, sub (editable and derived) and both MSE addresses all
        // stretch to the column's width, so their right edges line up.
        assert_eq!(
            body.matches("desired_width(f32::INFINITY)").count(),
            6,
            "name, main, sub (both forms) and both MSE fields must fill their column"
        );
        // While checked the field is kept equal to the derived address and
        // locked grey; build() saves whatever is in the field, so nothing is
        // inferred at save time and unticking keeps the address to edit.
        assert!(
            body.contains("draft.sub = derived"),
            "the checked field must be filled from the main url"
        );
        assert!(
            body.contains("infer_sub_stream(&draft.main)"),
            "the address filled while checked comes from the main url"
        );
        assert!(
            body.contains("let mut sub = draft.sub.as_str()"),
            "the grey field shows the stored address, not a fresh preview"
        );
        assert!(
            body.contains("ui.add_enabled(\n                false,"),
            "the derived address must be greyed and read-only"
        );
        assert!(
            !body.contains("dialog-infer"),
            "the Infer button is gone; the check does its job"
        );
        assert!(
            body.contains("infer_ok && draft.infer_sub"),
            "the grey field shows only where the check is drawn"
        );
    }

    #[test]
    fn a_background_failure_keeps_its_one_toast() {
        let mut state = DiscoveryUi::default();
        assert!(state.toast_error.is_none());
        state.fail(Tab::Synology, "already toasted by the event loop");
        assert!(state.toast_error.is_none(), "fail() alone must not flash twice");
    }

    #[test]
    fn form_errors_in_the_devices_dialog_ask_for_a_toast() {
        let source = include_str!("dialogs.rs");
        let call = format!("state.toast_error = Some({});", "err");
        assert_eq!(
            source.matches(&call).count(),
            2,
            "manual add and bulk add both answer a press with a toast"
        );
    }

    #[test]
    fn the_bulk_form_uses_the_existing_label_and_hint_styles() {
        let source = include_str!("dialogs.rs");
        let plain_title = format!("ui.label(monitor_i18n::tr(\"{}\"));", "dialog-bulk-title");
        assert!(
            source.contains(&plain_title),
            "the section title uses the same plain label as the field names"
        );
        assert!(
            source.contains("RichText::new(monitor_i18n::tr(\"dialog-bulk-credentials\")).small()"),
            "the credentials caption matches the other tabs"
        );
        assert!(
            source.contains("theme::hint(") && source.contains("dialog-bulk-recorder-placeholder"),
            "placeholders use the dialog's dimmed hint colour"
        );
        assert!(
            !source.contains("text_field(ui, nav, &mut state.bulk.name_prefix, \"bulk-camera-name\""),
            "the Android mirror needs the localized field label"
        );
    }

    #[test]
    fn bulk_cameras_resolve_the_inference_checkbox_when_building() {
        let main_template = "rtsp://192.168.1.100/Streaming/Channels/{n}01".to_owned();
        let derived = "rtsp://192.168.1.100/Streaming/Channels/102";

        let draft = BulkCameraDraft {
            main_template: main_template.clone(),
            channels: "1".into(),
            infer_sub: true,
            ..Default::default()
        };
        let cameras = draft.build().unwrap();
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0].rtsp_sub.as_deref(), Some(derived));
        assert!(draft.can_infer_sub(), "the offer is made where a rule applies");

        let without = BulkCameraDraft { infer_sub: false, ..draft.clone() };
        assert!(
            without.build().unwrap()[0].rtsp_sub.is_none(),
            "an unticked box leaves the camera without a sub stream"
        );

        let typed = BulkCameraDraft {
            sub_template: "rtsp://192.168.1.100/channel/{n}/sub".into(),
            ..draft.clone()
        };
        assert!(!typed.can_infer_sub(), "a typed template replaces the offer");
        assert_eq!(
            typed.build().unwrap()[0].rtsp_sub.as_deref(),
            Some("rtsp://192.168.1.100/channel/1/sub"),
            "a typed template wins over the checkbox"
        );

        let unmatched = BulkCameraDraft {
            main_template: "rtsp://192.168.1.100/channel/{n}".into(),
            channels: "1".into(),
            infer_sub: true,
            ..Default::default()
        };
        assert!(
            !unmatched.can_infer_sub(),
            "no rule means there is nothing to offer"
        );
        assert!(unmatched.build().unwrap()[0].rtsp_sub.is_none());

        let partial = BulkCameraDraft {
            main_template: "rtsp://192.168.1.100/stream{n}".into(),
            channels: "1-2".into(),
            infer_sub: true,
            ..Default::default()
        };
        assert!(
            !partial.can_infer_sub(),
            "one batch checkbox cannot promise an address for every channel"
        );
        assert!(
            partial.build().unwrap()[0].rtsp_sub.is_none(),
            "a hidden check must not derive for just part of the batch"
        );

        let literal = BulkCameraDraft {
            main_template: "rtsp://192.168.1.100/Streaming/Channels/101".into(),
            channels: "1".into(),
            infer_sub: true,
            ..Default::default()
        };
        assert!(
            !literal.can_infer_sub(),
            "no channel token means the form cannot build a batch at all"
        );
    }

    #[test]
    fn a_camera_draft_saves_only_the_address_on_the_form() {
        let source = CameraSource::new(
            "cam",
            "rtsp://192.168.1.100/Streaming/Channels/101",
        );
        let draft = CameraDraft::from_source(&source);
        assert!(draft.infer_sub, "a camera without a sub starts in infer mode");
        assert!(
            draft.build().unwrap().rtsp_sub.is_none(),
            "the builder saves no address the form has not shown"
        );

        let mut stale = draft.clone();
        stale.main = "rtsp://192.168.1.100/other/1".into();
        stale.sub = "rtsp://192.168.1.100/Streaming/Channels/102".into();
        assert_eq!(
            stale.build().unwrap().rtsp_sub.as_deref(),
            Some("rtsp://192.168.1.100/Streaming/Channels/102"),
            "when the main url no longer offers inference, the visible address is kept"
        );

        let mut edited = draft;
        edited.sub = "rtsp://192.168.1.100/Streaming/Channels/102".into();
        edited.infer_sub = false;
        assert_eq!(
            edited.build().unwrap().rtsp_sub.as_deref(),
            Some("rtsp://192.168.1.100/Streaming/Channels/102"),
            "unticking keeps the address for editing and saves it"
        );

        let mut with_sub = source;
        with_sub.rtsp_sub = Some("rtsp://192.168.1.100/Streaming/Channels/102".into());
        assert!(
            !CameraDraft::from_source(&with_sub).infer_sub,
            "an existing address is left editable"
        );
    }

    #[test]
    fn typing_over_a_hidden_infer_check_makes_the_sub_manual() {
        let mut check = true;
        keep_a_hand_typed_sub(false, true, &mut check);
        assert!(!check, "a typed address is no longer the rule's to replace");

        let mut untouched = true;
        keep_a_hand_typed_sub(false, false, &mut untouched);
        assert!(untouched, "the address still shown is still derived");

        let mut manual = false;
        keep_a_hand_typed_sub(true, true, &mut manual);
        assert!(!manual, "an unticked check stays unticked when a rule applies");
    }

    #[test]
    fn bulk_cameras_allow_reuse_across_groups() {
        let draft = BulkCameraDraft {
            main_template: "rtsp://192.168.1.100/channel/{n}".into(),
            channels: "1-2".into(),
            infer_sub: false,
            ..Default::default()
        };
        let mut config = AppConfig::default();

        for source in draft.build().unwrap() {
            assert!(!bulk_camera_exists(&config, &source));
            config.upsert_camera(source);
        }
        assert_eq!(config.cameras.len(), 2);

        for source in draft.build().unwrap() {
            assert!(bulk_camera_exists(&config, &source));
        }

        let second = config.add_group("Second wall".into());
        config.active_group = second;

        for source in draft.build().unwrap() {
            assert!(!bulk_camera_exists(&config, &source));
            config.upsert_camera(source);
        }

        assert_eq!(config.cameras.len(), 4);
        assert_eq!(
            config.cameras.iter()
                .filter(|camera| usize::from(camera.group) == second)
                .count(),
            2
        );

        for source in draft.build().unwrap() {
            assert!(bulk_camera_exists(&config, &source));
        }
    }
    #[test]
    fn bulk_cameras_expand_channel_templates() {
        monitor_i18n::init("en", &[]);
        let draft = BulkCameraDraft {
            name_prefix: "Recorder A".into(),
            main_template: "rtsp://192.168.1.100/Streaming/Channels/{n}01".into(),
            sub_template: "rtsp://192.168.1.100/Streaming/Channels/{n}02".into(),
            username: "admin".into(),
            password: "secret".into(),
            channels: "1-16".into(),
            ..Default::default()
        };

        let cameras = draft.build().unwrap();
        assert_eq!(cameras.len(), 16);
        assert_eq!(cameras[0].name, "Recorder A — Camera 01");
        assert_eq!(cameras[0].rtsp_main, "rtsp://192.168.1.100/Streaming/Channels/101");
        assert_eq!(cameras[0].rtsp_sub.as_deref(), Some("rtsp://192.168.1.100/Streaming/Channels/102"));
        assert_eq!(cameras[15].rtsp_main, "rtsp://192.168.1.100/Streaming/Channels/1601");
        assert_eq!(cameras[15].rtsp_sub.as_deref(), Some("rtsp://192.168.1.100/Streaming/Channels/1602"));
        assert_eq!(cameras[0].username.as_deref(), Some("admin"));
        assert_eq!(cameras[0].password.as_deref(), Some("secret"));
    }

    #[test]
    fn bulk_cameras_expand_channel_lists() {
        monitor_i18n::init("en", &[]);
        let draft = BulkCameraDraft {
            main_template: "rtsp://192.168.1.100/channel/{n}".into(),
            channels: "3,5-6,9".into(),
            infer_sub: false,
            ..Default::default()
        };

        let cameras = draft.build().unwrap();
        assert_eq!(cameras.len(), 4);
        assert_eq!(cameras[0].name, "Camera 03");
        assert_eq!(cameras[1].rtsp_main, "rtsp://192.168.1.100/channel/5");
        assert_eq!(cameras[2].rtsp_main, "rtsp://192.168.1.100/channel/6");
        assert_eq!(cameras[3].rtsp_main, "rtsp://192.168.1.100/channel/9");
        assert!(cameras.iter().all(|camera| camera.rtsp_sub.is_none()));
    }

    #[test]
    fn bulk_cameras_reject_invalid_parameters() {
        let valid = BulkCameraDraft {
            main_template: "rtsp://192.168.1.100/channel/{n}".into(),
            infer_sub: false,
            ..Default::default()
        };

        for channels in ["", "x", "5-3", "1-", "1,,2", "65536", "1-65536"] {
            assert!(
                BulkCameraDraft { channels: channels.into(), ..valid.clone() }
                    .build()
                    .is_err(),
                "channels: {channels}"
            );
        }
        let too_many = format!("1-{}", 257);
        assert!(BulkCameraDraft {
            channels: too_many,
            ..valid.clone()
        }.build().is_err());
        let duplicated = BulkCameraDraft {
            channels: "1,1,2".into(),
            ..valid.clone()
        };
        assert_eq!(duplicated.build().unwrap().len(), 2, "duplicates are dropped");
        assert!(BulkCameraDraft {
            main_template: "rtsp://192.168.1.100/channel/1".into(),
            ..valid.clone()
        }.build().is_err());
        assert!(BulkCameraDraft {
            sub_template: "rtsp://192.168.1.100/sub/1".into(),
            ..valid
        }.build().is_err());
    }

    /// A configuration from 1.x has no `frigate` on a camera a Frigate import
    /// brought in. Every field `add_offer` used to compare is unchanged, so a
    /// re-import was offered as `Added` - whose button is disabled - and the
    /// information playback needs was never written.
    #[test]
    fn an_older_frigate_camera_is_offered_as_an_update() {
        let old = CameraSource::new("front_door", "rtsp://192.168.17.88:8554/front_door");
        let mut imported = old.clone();
        imported.frigate = Some(FrigateOrigin {
            base_url: "http://192.168.17.88:5000".into(),
            camera: "front_door".into(),
        });

        let mut config = AppConfig::default();
        config.cameras.push(old);
        assert_eq!(add_offer(&config, &imported), AddOffer::Update);

        // Once it has been refreshed, importing the same server again has
        // nothing left to change: the button goes back to the disabled "Added".
        config.cameras[0] = imported.clone();
        assert_eq!(add_offer(&config, &imported), AddOffer::Added);
    }
}
