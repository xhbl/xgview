use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::layout::GridLayout;
use crate::model::{CameraSource, Osd};
use crate::{APP_NAME, CONFIG_FILE_NAME};

/// Version of the configuration schema written to disk.
pub const CONFIG_VERSION: u32 = 1;

/// Exponential backoff parameters used when a stream has to be reconnected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconnectPolicy {
    /// Delay before the first retry.
    pub initial_delay_ms: u64,
    /// Delay before the first retry when no picture has ever been produced.
    /// A slow link needs longer than the running backoff before it is asked to
    /// reconnect, because the handshake itself is what is slow.
    pub first_frame_delay_ms: u64,
    /// Upper bound of the delay.
    pub max_delay_ms: u64,
    /// Growth factor applied on every attempt.
    pub multiplier: f64,
    /// Randomisation ratio applied to the delay (0.0 … 1.0).
    pub jitter: f64,
    /// Per channel offset added to the delay so that simultaneous failures do
    /// not all reconnect at once. Only applied to the first two attempts,
    /// where the clustering is worst; later attempts are spread by the
    /// exponential growth already.
    pub spread_ms: u64,
    /// Maximum number of attempts, `0` meaning "retry forever".
    pub max_attempts: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay_ms: 1_000,
            first_frame_delay_ms: 5_000,
            max_delay_ms: 30_000,
            multiplier: 1.8,
            jitter: 0.2,
            spread_ms: 100,
            max_attempts: 0,
        }
    }
}

impl ReconnectPolicy {
    /// Delay to wait before the given attempt (1 based). `first_frame` selects
    /// the longer initial delay used when no picture has ever been produced,
    /// and `index` adds a per channel offset so simultaneous failures do not
    /// all reconnect at once.
    pub fn delay_for(&self, attempt: u32, first_frame: bool, index: u32) -> Duration {
        let base = if first_frame {
            self.first_frame_delay_ms
        } else {
            self.initial_delay_ms
        };
        let exponent = attempt.max(1).saturating_sub(1).min(16) as i32;
        let raw = base as f64 * self.multiplier.powi(exponent);
        let capped = raw.min(self.max_delay_ms.max(base) as f64);
        let jitter = self.jitter.clamp(0.0, 1.0);
        let factor = if jitter > 0.0 {
            1.0 - jitter + 2.0 * jitter * pseudo_random()
        } else {
            1.0
        };
        let offset = if attempt <= 2 {
            (index as u64).saturating_mul(self.spread_ms)
        } else {
            0
        };
        Duration::from_millis((capped * factor).max(1.0) as u64 + offset)
    }

    /// `false` when the retry budget is exhausted.
    pub fn should_retry(&self, attempt: u32) -> bool {
        self.max_attempts == 0 || attempt <= self.max_attempts
    }
}

/// Cheap deterministic-enough randomness used for the backoff jitter.
fn pseudo_random() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64 + duration.as_secs())
        .unwrap_or(0);
    let mixed = nanos.wrapping_mul(6_364_136_223_846_793_005).rotate_left(17);
    (mixed >> 11) as f64 / (1u64 << 53) as f64
}

/// Camera discovery settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Send a WS-Discovery probe to the multicast group 239.255.255.250:3702.
    pub broadcast: bool,
    /// Probe every address of [`DiscoveryConfig::ip_ranges`] with a unicast
    /// WS-Discovery probe (needed across VLANs / subnets).
    pub subnet_scan: bool,
    /// IP ranges, e.g. `192.168.1.1-254` or `10.0.0.0/24`.
    pub ip_ranges: Vec<String>,
    /// Fall back to a TCP port scan for devices that never answer WS-Discovery.
    pub tcp_probe: bool,
    /// TCP ports probed by the fallback scan.
    pub tcp_ports: Vec<u16>,
    /// Time to wait for unicast answers.
    pub probe_timeout_ms: u64,
    /// Maximum number of concurrent probes / connection attempts.
    pub concurrency: usize,
    /// Credentials used for the ONVIF GetProfiles / GetStreamUri calls.
    pub onvif_username: Option<String>,
    pub onvif_password: Option<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            broadcast: true,
            subnet_scan: false,
            ip_ranges: Vec::new(),
            tcp_probe: true,
            tcp_ports: vec![554, 80, 8000],
            probe_timeout_ms: 1_500,
            concurrency: 64,
            onvif_username: Some("admin".to_string()),
            onvif_password: None,
        }
    }
}

/// Synology Surveillance Station connection settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SynologyConfig {
    /// NAS host name or IP address.
    pub host: String,
    pub port: u16,
    pub https: bool,
    pub username: String,
    pub password: String,
    /// DSM account used for Surveillance Station when different from `username`.
    pub account: Option<String>,
}

impl Default for SynologyConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 5000,
            https: false,
            username: String::new(),
            password: String::new(),
            account: None,
        }
    }
}

impl SynologyConfig {
    pub fn base_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty() && !self.username.trim().is_empty()
    }
}

/// Persisted application configuration (`config.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    /// Grid layout restored at start-up.
    pub layout: GridLayout,
    pub page: usize,
    pub focus: Option<usize>,
    /// Mirror of the start-on-boot registration.
    pub autostart: bool,
    /// Open the window in full screen (TV / kiosk deployment).
    pub start_fullscreen: bool,
    /// Android: leave the navigation bar in place and keep its strip free of the
    /// wall, instead of hiding it and drawing under it. Ignored on the other
    /// platforms, which have no such bar.
    pub reserve_navigation_bar: bool,
    /// Interface language: a BCP-47 tag such as `zh-CN`, `auto` to follow the
    /// operating system, or `en` for English. A language that is not installed
    /// (no matching `.ftl` pack) falls back to English.
    pub language: String,
    /// Decode on the GPU where the machine offers a decoder for it. It is only
    /// a preference: a device that cannot be opened, or a stream the hardware
    /// decoder will not take, is decoded on the CPU.
    pub prefer_hardware_decode: bool,
    pub reconnect: ReconnectPolicy,
    /// How long a session has to establish itself before the attempt is written
    /// off and retried: the `OPTIONS` / `DESCRIBE` / `SETUP` / `PLAY` exchange,
    /// or the response headers of an HTTP stream. It is a budget for the whole
    /// handshake, not per request, and it is what bounds a peer that accepts
    /// the connection and then stops answering. Longer for a link that is slow
    /// to answer, which a camera reached over the internet can be.
    pub handshake_timeout_ms: u64,
    pub discovery: DiscoveryConfig,
    pub synology: SynologyConfig,
    /// What each corner of a tile shows. It is a property of the wall rather
    /// than of a camera: a viewer reads the same thing in the same corner
    /// wherever they look.
    #[serde(default)]
    pub osd: Osd,
    pub cameras: Vec<CameraSource>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            layout: GridLayout::default(),
            page: 0,
            focus: None,
            autostart: false,
            start_fullscreen: false,
            reserve_navigation_bar: true,
            language: "auto".to_string(),
            prefer_hardware_decode: true,
            reconnect: ReconnectPolicy::default(),
            handshake_timeout_ms: 15_000,
            discovery: DiscoveryConfig::default(),
            synology: SynologyConfig::default(),
            osd: Osd::default(),
            cameras: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Loads the configuration from an explicit path.
    ///
    /// A missing file yields the default configuration so that a fresh install
    /// starts without any manual setup.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            tracing::info!(target: "xgview::config", path = %path.display(), "no configuration file, using defaults");
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path)?;
        Self::from_json(&data)
    }

    /// Parses configuration JSON, normalized the way [`Self::load`] normalizes
    /// it.
    ///
    /// An import reads the text through whatever the platform offers - a file
    /// on the desktop, a document the viewer picked on Android - so the parsing
    /// cannot live inside `load`.
    pub fn from_json(data: &str) -> Result<Self> {
        let mut config: AppConfig = serde_json::from_str(data)?;
        config.normalize();
        Ok(config)
    }

    /// Loads the configuration from [`AppConfig::default_path`], ignoring read
    /// errors so that a corrupt file never prevents the app from starting.
    ///
    /// A file that is not there is written back as the defaults, so the first
    /// run of a fresh install leaves one behind: the settings on screen then
    /// have a file behind them, ready to be backed up, edited, or carried to the
    /// next machine. A file that is there but unreadable is left as it is - it
    /// is the user's, and whatever it still holds is not the defaults' to
    /// overwrite.
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let missing = !path.exists();
        match Self::load(path) {
            Ok(config) => {
                if missing {
                    if let Err(err) = config.save(path) {
                        tracing::warn!(target: "xgview::config", path = %path.display(), %err, "cannot write the default configuration");
                    } else {
                        tracing::debug!(target: "xgview::config", path = %path.display(), "wrote the default configuration");
                    }
                }
                config
            }
            Err(err) => {
                tracing::warn!(target: "xgview::config", %err, "falling back to default configuration");
                Self::default()
            }
        }
    }

    /// The configuration as the JSON an export carries.
    ///
    /// Android hands this text to `MediaStore` rather than writing a path
    /// itself, so the serialization cannot live inside `save`.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Writes the configuration atomically (temporary file + rename).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = self.to_json()?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, json)?;
        std::fs::rename(&temporary, path).map_err(|err| {
            CoreError::config(format!("cannot replace {}: {err}", path.display()))
        })?;
        tracing::debug!(target: "xgview::config", path = %path.display(), "configuration saved");
        Ok(())
    }

    /// Saves to [`AppConfig::default_path`].
    pub fn save_default(&self) -> Result<()> {
        self.save(Self::default_path())
    }

    /// Platform specific configuration file path.
    pub fn default_path() -> PathBuf {
        config_dir().join(CONFIG_FILE_NAME)
    }

    /// Cameras that are enabled, in file order.
    pub fn enabled_cameras(&self) -> Vec<&CameraSource> {
        self.cameras.iter().filter(|camera| camera.enabled).collect()
    }

    /// Owned clone of the enabled cameras, in file order. The channel index
    /// used by the scheduler refers to a position inside this list.
    pub fn active_cameras(&self) -> Vec<CameraSource> {
        self.cameras.iter().filter(|camera| camera.enabled).cloned().collect()
    }

    pub fn enabled_count(&self) -> usize {
        self.cameras.iter().filter(|camera| camera.enabled).count()
    }

    pub fn find_camera(&self, id: &str) -> Option<&CameraSource> {
        self.cameras.iter().find(|camera| camera.id == id)
    }

    pub fn find_camera_mut(&mut self, id: &str) -> Option<&mut CameraSource> {
        self.cameras.iter_mut().find(|camera| camera.id == id)
    }

    /// Adds a camera, or refreshes the streams of an already imported one.
    /// Returns the identifier of the stored entry.
    ///
    /// A refresh takes what the device reported - the urls, the credentials, the
    /// vendor, the ONVIF profile tokens - and keeps what the user chose in this
    /// application. The transport was already kept, because a camera switched to
    /// UDP is a decision about a misbehaving relay and not something a re-import
    /// knows better than the person who made it; the display mode and the tags
    /// are the same kind of thing, and an import carries neither, so leaving
    /// them to the incoming entry is what silently resets them to their
    /// defaults. Enabled and the origin are kept for the same reason.
    pub fn upsert_camera(&mut self, camera: CameraSource) -> String {
        let identity = camera.identity();
        if let Some(existing) = self
            .cameras
            .iter_mut()
            .find(|existing| existing.id == camera.id || existing.identity() == identity)
        {
            let id = existing.id.clone();
            let origin = existing.origin;
            let enabled = existing.enabled;
            let transport = existing.transport;
            let aspect = existing.aspect;
            let tags = existing.tags.clone();
            *existing = CameraSource { id: id.clone(), origin, enabled, transport, aspect, tags, ..camera };
            return id;
        }
        let id = camera.id.clone();
        self.cameras.push(camera);
        id
    }

    /// Removes a camera by id.
    pub fn remove_camera(&mut self, id: &str) -> bool {
        let before = self.cameras.len();
        self.cameras.retain(|camera| camera.id != id);
        self.cameras.len() != before
    }

    /// Reorders the cameras to match `order`, a list of camera identifiers.
    ///
    /// Identifiers that name no camera are skipped, and a camera the list does
    /// not mention keeps its place at the end: an order built a moment ago -
    /// before a removal or an import - therefore cannot drop an entry. The
    /// order decides the channel index and the place on the wall, so this is
    /// what a rearranged list is written back as. Returns `true` when the
    /// resulting order differs from the current one.
    pub fn apply_order(&mut self, order: &[String]) -> bool {
        let before: Vec<String> = self.cameras.iter().map(|camera| camera.id.clone()).collect();
        let mut ordered = Vec::with_capacity(self.cameras.len());
        for id in order {
            if let Some(position) = self.cameras.iter().position(|camera| &camera.id == id) {
                ordered.push(self.cameras.remove(position));
            }
        }
        ordered.append(&mut self.cameras);
        self.cameras = ordered;
        self.cameras.iter().map(|camera| camera.id.as_str()).ne(before.iter().map(String::as_str))
    }

    /// Clamps `page` / `focus` once the camera list changed.
    pub fn normalize(&mut self) {
        if self.version == 0 {
            self.version = CONFIG_VERSION;
        }
        let count = self.enabled_count();
        let capacity = self.layout.capacity();
        let pages = crate::layout::page_count(count, capacity).max(1);
        if self.page >= pages {
            self.page = pages - 1;
        }
        match self.focus {
            Some(focus) if focus >= count => self.focus = if count == 0 { None } else { Some(count - 1) },
            None if count > 0 => self.focus = Some(self.page * capacity),
            _ => {}
        }
    }
}

/// Root directory holding the configuration.
///
/// `XGVIEW_CONFIG_DIR` always wins, which is what Android and portable
/// deployments use.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XGVIEW_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    match platform_config_dir() {
        Some(base) => base.join(APP_NAME),
        None => PathBuf::from(".").join(APP_NAME),
    }
}

#[cfg(windows)]
fn platform_config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(all(unix, not(target_os = "android")))]
fn platform_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

#[cfg(target_os = "android")]
fn platform_config_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("XGVIEW_HOME") {
        return Some(PathBuf::from(home));
    }
    // Application private storage; the Android entry point sets XGVIEW_HOME to
    // the app specific files directory whenever it is available.
    Some(PathBuf::from("/data/local/tmp"))
}

#[cfg(not(any(windows, unix)))]
fn platform_config_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let config = AppConfig::load("does-not-exist/config.json").unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert!(config.cameras.is_empty());
    }

    /// The first run of a fresh install starts on the defaults and leaves them
    /// on disk, so the settings on screen have a file behind them.
    #[test]
    fn a_first_run_leaves_a_configuration_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert!(!path.exists());

        let config = AppConfig::load_or_default(&path);

        assert_eq!(config.version, CONFIG_VERSION);
        assert!(path.exists(), "the run writes the file it started from");
        assert_eq!(AppConfig::load(&path).unwrap(), config);
    }

    /// A file that is there but unreadable is the user's: the viewer starts on
    /// the defaults, and the file is not overwritten with them.
    #[test]
    fn a_broken_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not json").unwrap();

        let config = AppConfig::load_or_default(&path);

        assert_eq!(config, AppConfig::default());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ not json",
            "the file is the user's, not the defaults' to replace"
        );
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = AppConfig {
            layout: GridLayout::G3x3,
            page: 1,
            autostart: true,
            ..Default::default()
        };
        // Two pages of channels so that page 1 survives normalization.
        for index in 0..12 {
            config.upsert_camera(CameraSource::new(
                format!("cam {index}"),
                format!("rtsp://10.0.0.{index}/live"),
            ));
        }
        config.save(&path).unwrap();
        let loaded = AppConfig::load(&path).unwrap();
        assert_eq!(loaded.layout, GridLayout::G3x3);
        assert_eq!(loaded.page, 1);
        assert!(loaded.autostart);
        assert_eq!(loaded.cameras.len(), 12);
    }

    /// An older `config.json` written before the language field existed must
    /// still load, and the interface must default to auto-detection rather
    /// than a language named by an empty string.
    #[test]
    fn a_configuration_without_a_language_defaults_to_auto() {
        let config: AppConfig = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(config.language, "auto");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let policy = ReconnectPolicy { jitter: 0.0, ..Default::default() };
        assert_eq!(policy.delay_for(1, false, 0), Duration::from_millis(1_000));
        assert!(policy.delay_for(3, false, 0) > policy.delay_for(2, false, 0));
        assert!(policy.delay_for(30, false, 0) <= Duration::from_millis(30_000));
    }

    #[test]
    fn retry_budget_is_respected() {
        let policy = ReconnectPolicy { max_attempts: 3, ..Default::default() };
        assert!(policy.should_retry(3));
        assert!(!policy.should_retry(4));
    }

    #[test]
    fn upsert_replaces_same_url() {
        let mut config = AppConfig::default();
        let first = config.upsert_camera(CameraSource::new("door", "rtsp://10.0.0.1/live"));
        let second = config.upsert_camera(CameraSource::new("door renamed", "rtsp://10.0.0.1/live"));
        assert_eq!(first, second);
        assert_eq!(config.cameras.len(), 1);
        assert_eq!(config.cameras[0].name, "door renamed");
    }

    /// The same camera reached with its credentials in the URL, a default port
    /// spelled out and a trailing slash is still the same camera.
    #[test]
    fn upsert_matches_the_same_stream_written_differently() {
        let mut config = AppConfig::default();
        let first = config.upsert_camera(CameraSource::new("door", "rtsp://10.0.0.1/live"));
        let second =
            config.upsert_camera(CameraSource::new("door renamed", "rtsp://admin:secret@10.0.0.1:554/live/"));

        assert_eq!(first, second);
        assert_eq!(config.cameras.len(), 1);
        assert_eq!(config.cameras[0].name, "door renamed");
    }

    /// Two streams of one device are two cameras: the normalisation must not
    /// collapse them onto the device's address.
    #[test]
    fn upsert_keeps_two_streams_of_one_host_apart() {
        let mut config = AppConfig::default();
        let main = config.upsert_camera(CameraSource::new("main", "rtsp://10.0.0.1/Streaming/Channels/101"));
        let sub = config.upsert_camera(CameraSource::new("sub", "rtsp://10.0.0.1/Streaming/Channels/102"));

        assert_ne!(main, sub);
        assert_eq!(config.cameras.len(), 2);
    }

    /// A re-import refreshes what the device reported and keeps what the user
    /// chose here. The display mode and the tags are the ones an import carries
    /// no value for, so they are the ones a wholesale replace reset.
    #[test]
    fn upsert_keeps_the_choices_an_import_cannot_make() {
        use crate::model::{RtspTransport, TileAspect};

        let mut config = AppConfig::default();
        let id = config.upsert_camera(CameraSource::new("door", "rtsp://10.0.0.1/live"));
        let camera = config.find_camera_mut(&id).unwrap();
        camera.aspect = TileAspect::Ratio16x9;
        camera.transport = RtspTransport::Udp;
        camera.tags = vec!["gate".to_string()];
        camera.enabled = false;

        // What a discovery refresh hands in: the same device, its fields filled
        // from the answer, and every choice above at its default.
        let again = config.upsert_camera(CameraSource::new("door", "rtsp://10.0.0.1/live"));

        assert_eq!(again, id);
        assert_eq!(config.cameras.len(), 1);
        let camera = config.find_camera(&id).unwrap();
        assert_eq!(camera.aspect, TileAspect::Ratio16x9, "the display mode survives");
        assert_eq!(camera.transport, RtspTransport::Udp, "the transport survives");
        assert_eq!(camera.tags, vec!["gate".to_string()], "the tags survive");
        assert!(!camera.enabled, "a camera switched off stays off");
    }

    /// The order a rearranged list is written back as. An identifier that names
    /// no camera is skipped, and an entry the list forgets keeps its place at
    /// the end rather than disappearing from the wall.
    #[test]
    fn apply_order_reorders_and_keeps_the_entries_it_was_not_given() {
        let mut config = AppConfig::default();
        let a = config.upsert_camera(CameraSource::new("a", "rtsp://10.0.0.1/live"));
        let b = config.upsert_camera(CameraSource::new("b", "rtsp://10.0.0.2/live"));
        let c = config.upsert_camera(CameraSource::new("c", "rtsp://10.0.0.3/live"));
        let ids = |config: &AppConfig| -> Vec<String> {
            config.cameras.iter().map(|camera| camera.id.clone()).collect()
        };

        assert!(config.apply_order(&[c.clone(), a.clone(), b.clone()]));
        assert_eq!(ids(&config), vec![c.clone(), a.clone(), b.clone()]);

        // The order that is already in place changes nothing.
        assert!(!config.apply_order(&[c.clone(), a.clone(), b.clone()]));

        // A list that forgets an entry, and one that names a stranger: the
        // forgotten camera stays, at the end.
        assert!(config.apply_order(&[b.clone(), "nobody".to_string(), c.clone()]));
        assert_eq!(ids(&config), vec![b, c, a]);
    }
}
