use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::rtsp::RtspUrl;

/// Which RTSP stream of a camera has to be pulled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// High resolution stream (1080p / 4K), pulled in 1x1 full screen mode.
    Main,
    /// Low resolution stream (360p / 480p), pulled in multi grid modes.
    Sub,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamKind::Main => "main",
            StreamKind::Sub => "sub",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StreamKind::Main => "Main stream",
            StreamKind::Sub => "Sub stream",
        }
    }

    /// Short tag rendered inside a grid tile.
    pub fn tag(self) -> &'static str {
        match self {
            StreamKind::Main => "MAIN",
            StreamKind::Sub => "SUB",
        }
    }
}

impl std::fmt::Display for StreamKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime connection state of a single channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// Not started yet.
    Idle,
    /// Deliberately parked because the channel is not on the visible page.
    Suspended,
    /// Establishing the RTSP session.
    Connecting,
    /// Receiving RTP packets.
    Streaming,
    /// Lost the session, waiting for the backoff timer.
    Reconnecting,
    /// Retries exhausted, waiting for an explicit user action.
    Failed,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            ConnectionState::Idle => "Idle",
            ConnectionState::Suspended => "Suspended",
            ConnectionState::Connecting => "Connecting...",
            ConnectionState::Streaming => "Live",
            ConnectionState::Reconnecting => "Reconnecting...",
            ConnectionState::Failed => "Failed",
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, ConnectionState::Streaming)
    }
}

/// Where a camera entry came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraOrigin {
    #[default]
    Manual,
    Onvif,
    Synology,
}

impl CameraOrigin {
    pub fn label(self) -> &'static str {
        match self {
            CameraOrigin::Manual => "Manual",
            CameraOrigin::Onvif => "ONVIF",
            CameraOrigin::Synology => "Synology",
        }
    }
}

/// Transport used to receive the RTP stream of a camera.
///
/// The interleaved (TCP) transport is the default: it crosses firewalls and
/// loses no packet. A relay that damages the interleaved framing - a Synology
/// Surveillance Station has been measured doing exactly that on its high
/// resolution stream - can be bypassed by switching that one camera to UDP,
/// where every RTP packet travels as a datagram of its own and a mangled packet
/// cannot shift the ones behind it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RtspTransport {
    #[default]
    Tcp,
    Udp,
}

impl RtspTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            RtspTransport::Tcp => "TCP",
            RtspTransport::Udp => "UDP",
        }
    }

    pub fn is_udp(self) -> bool {
        matches!(self, RtspTransport::Udp)
    }

    /// The other transport, used by the settings panel toggle.
    pub fn toggled(self) -> Self {
        match self {
            RtspTransport::Tcp => RtspTransport::Udp,
            RtspTransport::Udp => RtspTransport::Tcp,
        }
    }
}

fn default_onvif_port() -> u16 {
    80
}

fn default_true() -> bool {
    true
}

/// A single surveillance camera / NVR channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraSource {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Host name or IP address of the device. Used by discovery / ONVIF calls.
    pub host: String,
    #[serde(default = "default_onvif_port")]
    pub onvif_port: u16,
    /// Main (high resolution) RTSP URL.
    pub rtsp_main: String,
    /// Optional sub (low resolution) RTSP URL. Derived from `rtsp_main` when
    /// empty (see [`CameraSource::apply_sub_inference`]).
    #[serde(default)]
    pub rtsp_sub: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub origin: CameraOrigin,
    /// Transport used to pull both streams of this camera. Kept per camera so
    /// that a device whose TCP relay damages the stream can be singled out.
    #[serde(default)]
    pub transport: RtspTransport,
    /// ONVIF profile token of the main stream, kept for later re-negotiation.
    #[serde(default)]
    pub main_profile: Option<String>,
    /// ONVIF profile token of the sub stream.
    #[serde(default)]
    pub sub_profile: Option<String>,
}

impl CameraSource {
    /// Creates a camera with a generated id.
    pub fn new(name: impl Into<String>, rtsp_main: impl Into<String>) -> Self {
        let rtsp_main = rtsp_main.into();
        let host = RtspUrl::parse(&rtsp_main)
            .ok()
            .and_then(|uri| uri.host().map(str::to_string))
            .unwrap_or_default();
        Self {
            id: new_id(),
            name: name.into(),
            vendor: None,
            model: None,
            host,
            onvif_port: default_onvif_port(),
            rtsp_main,
            rtsp_sub: None,
            username: None,
            password: None,
            enabled: true,
            tags: Vec::new(),
            origin: CameraOrigin::Manual,
            transport: RtspTransport::default(),
            main_profile: None,
            sub_profile: None,
        }
    }

    pub fn with_sub(mut self, rtsp_sub: impl Into<String>) -> Self {
        self.rtsp_sub = Some(rtsp_sub.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// RTSP URL for the requested stream. Falls back to the main stream when no
    /// dedicated sub stream is configured, and vice versa.
    pub fn stream_uri(&self, kind: StreamKind) -> &str {
        match kind {
            StreamKind::Main => &self.rtsp_main,
            StreamKind::Sub => self.rtsp_sub.as_deref().unwrap_or(&self.rtsp_main),
        }
    }

    /// Returns the configured URL of a stream, if any.
    pub fn configured_uri(&self, kind: StreamKind) -> Option<&str> {
        match kind {
            StreamKind::Main => Some(&self.rtsp_main),
            StreamKind::Sub => self.rtsp_sub.as_deref(),
        }
    }

    /// `host:port` string used inside the UI.
    pub fn display_address(&self) -> String {
        match RtspUrl::parse(&self.rtsp_main) {
            Ok(uri) => match uri.port() {
                Some(port) => format!("{}:{}", uri.host().unwrap_or(&self.host), port),
                None => uri.host().unwrap_or(&self.host).to_string(),
            },
            Err(_) => self.host.clone(),
        }
    }

    /// Stream URL with credentials replaced by a mask, safe for logging / UI.
    pub fn masked_uri(&self, kind: StreamKind) -> String {
        match RtspUrl::parse(self.stream_uri(kind)) {
            Ok(uri) => uri.masked(),
            Err(_) => "<invalid rtsp url>".to_string(),
        }
    }

    /// Derives a sub stream URL from the main stream URL using well known
    /// vendor URL patterns. Returns `true` when a sub stream was filled in.
    pub fn apply_sub_inference(&mut self) -> bool {
        if self.rtsp_sub.is_some() {
            return false;
        }
        match infer_sub_stream(&self.rtsp_main) {
            Some(sub) => {
                self.rtsp_sub = Some(sub);
                true
            }
            None => false,
        }
    }

    /// Short label used in the grid tile header.
    pub fn short_label(&self, max: usize) -> String {
        let name = self.name.trim();
        if name.chars().count() <= max {
            name.to_string()
        } else {
            let mut out: String = name.chars().take(max.saturating_sub(1)).collect();
            out.push('…');
            out
        }
    }
}

/// Generates a stable, collision free identifier for a camera entry.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Vendor specific main -> sub stream URL inference rules.
///
/// The rules cover the URL schemes of the most common device families
/// (Hikvision, Dahua, Axis, generic `stream=main` style NVRs).
static SUB_STREAM_RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let rule = |pattern: &str, replacement: &'static str| {
        (
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .expect("valid sub stream regex"),
            replacement,
        )
    };
    vec![
        // Hikvision / HiWatch / generic NVR: /Streaming/Channels/101 -> 102
        rule(r"(/Streaming/Channels/\d)01$", "${1}02"),
        // Dahua / Amcrest: ...&subtype=0 -> subtype=1
        rule(r"([?&]subtype=)0\b", "${1}1"),
        // Foscam: /videoMain -> /videoSub
        rule(r"/videoMain$", "/videoSub"),
        // Axis / generic: ?stream=main -> ?stream=sub
        rule(
            r"([?&](?:stream|profile|streamtype|streamprofile)=)(?:main|high|primary)\b",
            "${1}sub",
        ),
        // RtspSimpleServer / ffmpeg style: /stream1 -> /stream2
        rule(r"/stream1([/?#]|$)", "/stream2${1}"),
        // Generic path suffix: /main -> /sub
        rule(r"/(?:main|primary)(\.(?:sdp|mp4|m4v|h264|h265))?$", "/sub${1}"),
    ]
});

/// Infers a low resolution sub stream URL from a main stream URL.
pub fn infer_sub_stream(main: &str) -> Option<String> {
    let main = main.trim();
    if main.is_empty() {
        return None;
    }
    for (pattern, replacement) in SUB_STREAM_RULES.iter() {
        let candidate = pattern.replace(main, *replacement);
        if candidate != main {
            return Some(candidate.into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_hikvision_sub_stream() {
        let url = "rtsp://admin:pw@192.168.1.64:554/Streaming/Channels/101";
        assert_eq!(
            infer_sub_stream(url).as_deref(),
            Some("rtsp://admin:pw@192.168.1.64:554/Streaming/Channels/102")
        );
    }

    #[test]
    fn infers_dahua_sub_stream() {
        let url = "rtsp://admin:pw@10.0.0.10/cam/realmonitor?channel=1&subtype=0";
        assert_eq!(
            infer_sub_stream(url).as_deref(),
            Some("rtsp://admin:pw@10.0.0.10/cam/realmonitor?channel=1&subtype=1")
        );
    }

    #[test]
    fn infers_foscam_sub_stream() {
        let url = "rtsp://admin:pw@192.168.27.40:88/videoMain";
        assert_eq!(
            infer_sub_stream(url).as_deref(),
            Some("rtsp://admin:pw@192.168.27.40:88/videoSub")
        );
    }

    #[test]
    fn leaves_unknown_urls_untouched() {
        assert_eq!(infer_sub_stream("rtsp://10.0.0.5/live/ch0"), None);
    }

    #[test]
    fn builds_camera_from_url() {
        let camera = CameraSource::new("Front door", "rtsp://10.0.0.9:8554/live");
        assert_eq!(camera.host, "10.0.0.9");
        assert_eq!(camera.display_address(), "10.0.0.9:8554");
    }

    #[test]
    fn masks_credentials() {
        let camera = CameraSource::new("Door", "rtsp://admin:secret@10.0.0.9/live");
        assert_eq!(camera.masked_uri(StreamKind::Main), "rtsp://***:***@10.0.0.9/live");
    }
}
