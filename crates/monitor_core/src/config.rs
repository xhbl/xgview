use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::layout::GridLayout;
use crate::model::CameraSource;
use crate::{APP_NAME, CONFIG_FILE_NAME};

/// Version of the configuration schema written to disk.
pub const CONFIG_VERSION: u32 = 1;

/// Exponential backoff parameters used when a stream has to be reconnected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconnectPolicy {
    /// Delay before the first retry.
    pub initial_delay_ms: u64,
    /// Upper bound of the delay.
    pub max_delay_ms: u64,
    /// Growth factor applied on every attempt.
    pub multiplier: f64,
    /// Randomisation ratio applied to the delay (0.0 … 1.0).
    pub jitter: f64,
    /// Maximum number of attempts, `0` meaning "retry forever".
    pub max_attempts: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay_ms: 1_000,
            max_delay_ms: 30_000,
            multiplier: 1.8,
            jitter: 0.2,
            max_attempts: 0,
        }
    }
}

impl ReconnectPolicy {
    /// Delay to wait before the given attempt (1 based).
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exponent = attempt.max(1).saturating_sub(1).min(16) as i32;
        let raw = self.initial_delay_ms as f64 * self.multiplier.powi(exponent);
        let capped = raw.min(self.max_delay_ms.max(self.initial_delay_ms) as f64);
        let jitter = self.jitter.clamp(0.0, 1.0);
        let factor = if jitter > 0.0 {
            1.0 - jitter + 2.0 * jitter * pseudo_random()
        } else {
            1.0
        };
        Duration::from_millis((capped * factor).max(1.0) as u64)
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
    /// Decode on the GPU where the machine offers a decoder for it. It is only
    /// a preference: a device that cannot be opened, or a stream the hardware
    /// decoder will not take, is decoded on the CPU.
    pub prefer_hardware_decode: bool,
    pub reconnect: ReconnectPolicy,
    pub discovery: DiscoveryConfig,
    pub synology: SynologyConfig,
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
            prefer_hardware_decode: true,
            reconnect: ReconnectPolicy::default(),
            discovery: DiscoveryConfig::default(),
            synology: SynologyConfig::default(),
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
        let mut config: AppConfig = serde_json::from_str(&data)?;
        config.normalize();
        Ok(config)
    }

    /// Loads the configuration from [`AppConfig::default_path`], ignoring read
    /// errors so that a corrupt file never prevents the app from starting.
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        Self::load(path).unwrap_or_else(|err| {
            tracing::warn!(target: "xgview::config", %err, "falling back to default configuration");
            Self::default()
        })
    }

    /// Writes the configuration atomically (temporary file + rename).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = serde_json::to_string_pretty(self)?;
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
    pub fn upsert_camera(&mut self, camera: CameraSource) -> String {
        if let Some(existing) = self
            .cameras
            .iter_mut()
            .find(|existing| existing.id == camera.id || existing.rtsp_main == camera.rtsp_main)
        {
            let id = existing.id.clone();
            let origin = existing.origin;
            let enabled = existing.enabled;
            // The transport is a user choice, not something a re-import may
            // reset: a camera kept on UDP stays on UDP.
            let transport = existing.transport;
            *existing = CameraSource { id: id.clone(), origin, enabled, transport, ..camera };
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

    #[test]
    fn backoff_grows_and_is_capped() {
        let policy = ReconnectPolicy { jitter: 0.0, ..Default::default() };
        assert_eq!(policy.delay_for(1), Duration::from_millis(1_000));
        assert!(policy.delay_for(3) > policy.delay_for(2));
        assert!(policy.delay_for(30) <= Duration::from_millis(30_000));
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
}
