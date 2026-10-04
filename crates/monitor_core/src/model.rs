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

/// How a channel's picture is fitted into its tile on the wall.
///
/// The choice belongs to the camera and follows it across pages and layouts: a
/// 4:3 camera letterboxed among 16:9 ones can be made to fill, and a wall of
/// mixed sizes can be forced to one shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileAspect {
    /// The picture keeps the shape it arrives with, letterboxed in the tile.
    #[default]
    Original,
    /// Filled to the tile whole, stretched where the shapes differ.
    Stretch,
    /// Drawn at 16:9, whatever shape the stream sends.
    #[serde(rename = "16x9")]
    Ratio16x9,
    /// Drawn at 4:3.
    #[serde(rename = "4x3")]
    Ratio4x3,
    /// Drawn square.
    #[serde(rename = "1x1")]
    Ratio1x1,
}

impl TileAspect {
    /// Every mode, in the order the cycle button walks them.
    pub const ALL: [TileAspect; 5] = [
        TileAspect::Original,
        TileAspect::Stretch,
        TileAspect::Ratio16x9,
        TileAspect::Ratio4x3,
        TileAspect::Ratio1x1,
    ];

    /// The mode a press on the cycle button selects next.
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// The name shown on the settings button that cycles the mode.
    pub fn label(self) -> &'static str {
        match self {
            TileAspect::Original => "Original",
            TileAspect::Stretch => "Stretch",
            TileAspect::Ratio16x9 => "16:9",
            TileAspect::Ratio4x3 => "4:3",
            TileAspect::Ratio1x1 => "1:1",
        }
    }

    /// The shape, written short for a tile's on-screen display.
    pub fn short_label(self) -> &'static str {
        match self {
            TileAspect::Original => "orig",
            TileAspect::Stretch => "fill",
            TileAspect::Ratio16x9 => "16:9",
            TileAspect::Ratio4x3 => "4:3",
            TileAspect::Ratio1x1 => "1:1",
        }
    }

    /// The shape the picture is drawn in, for the modes that fix one.
    pub fn ratio(self) -> Option<f32> {
        match self {
            TileAspect::Original | TileAspect::Stretch => None,
            TileAspect::Ratio16x9 => Some(16.0 / 9.0),
            TileAspect::Ratio4x3 => Some(4.0 / 3.0),
            TileAspect::Ratio1x1 => Some(1.0),
        }
    }
}

/// One corner's worth of a tile's on-screen display.
///
/// The list is ordered, and the settings panel walks a corner through it: what
/// a viewer wants in a corner changes with the wall in front of them, and a
/// remote has no room for a list of eight. The last entry is the three measured
/// lines stacked together, for a corner with room to spare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsdItem {
    /// Nothing at all.
    #[default]
    Off,
    /// The camera's name.
    Name,
    /// Its number on the wall, then the name.
    NumberName,
    /// Whether the picture is moving, and which stream it comes from.
    Stream,
    /// Transport and the rate the stream arrives at.
    Link,
    /// Frame rate, and which decoder is doing the work.
    Fps,
    /// Picture size, the shape it is drawn in, and the decoder.
    Format,
    /// [`OsdItem::Link`], [`OsdItem::Fps`] and [`OsdItem::Format`], one per line.
    Detail,
}

impl OsdItem {
    /// Every item, in the order the settings button walks them.
    pub const ALL: [OsdItem; 8] = [
        OsdItem::Off,
        OsdItem::Name,
        OsdItem::NumberName,
        OsdItem::Stream,
        OsdItem::Link,
        OsdItem::Fps,
        OsdItem::Format,
        OsdItem::Detail,
    ];

    /// The item a press on the settings button selects next.
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|item| *item == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// What the settings button says, and the only name the item has.
    pub fn label(self) -> &'static str {
        match self {
            OsdItem::Off => "None",
            OsdItem::Name => "Name",
            OsdItem::NumberName => "Number + name",
            OsdItem::Stream => "Status + stream",
            OsdItem::Link => "Transport + rate",
            OsdItem::Fps => "Frame rate",
            OsdItem::Format => "Size + shape",
            OsdItem::Detail => "All of the measures",
        }
    }
}

/// What each corner of a tile shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Osd {
    pub top_left: OsdItem,
    pub top_right: OsdItem,
    pub bottom_left: OsdItem,
    pub bottom_right: OsdItem,
}

impl Default for Osd {
    fn default() -> Self {
        Self {
            top_left: OsdItem::Off,
            top_right: OsdItem::Off,
            bottom_left: OsdItem::Off,
            bottom_right: OsdItem::Off,
        }
    }
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
    /// How this camera's picture is fitted into its tile on the wall.
    #[serde(default)]
    pub aspect: TileAspect,
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
            aspect: TileAspect::default(),
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
    ///
    /// A Synology camera keeps its sub stream on HTTP MJPEG rather than RTSP, so
    /// the RTSP parser is only the first of two readings: an `http://` URL is
    /// masked here instead. The stream key it carries as a query parameter is no
    /// less of a credential than a password, and a log is where it would end up.
    pub fn masked_uri(&self, kind: StreamKind) -> String {
        let uri = self.stream_uri(kind);
        if let Ok(parsed) = RtspUrl::parse(uri) {
            return parsed.masked();
        }
        if is_http_url(uri) {
            return mask_http_url(uri);
        }
        "<invalid stream url>".to_string()
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

    /// A key that identifies the stream this camera pulls, used to recognise
    /// the same device being imported twice.
    ///
    /// Built from the main stream URL and nothing else. The credentials are
    /// stored on the camera, so the same address written with them in the URL
    /// and without them is the same camera; so are a host's letter case, a
    /// default port left implicit, and a trailing slash.
    pub fn identity(&self) -> String {
        normalize_stream_url(&self.rtsp_main)
    }
}

/// Generates a stable, collision free identifier for a camera entry.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// The normalised form of a stream URL: `scheme://host:port/path?query`.
///
/// Everything a duplicate check must not be fooled by is taken out - the
/// credentials in the user info, the host's letter case, a default port left
/// implicit, a trailing slash - while the parts that name the stream stay,
/// the query of an MJPEG url included. `rtsps` folds onto `rtsp`: TLS is a
/// transport, not another stream.
fn normalize_stream_url(url: &str) -> String {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let scheme = scheme.to_ascii_lowercase();
    let scheme = match scheme.as_str() {
        "rtsps" => "rtsp",
        other => other,
    };
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    // The user info names who may connect, not which camera it is.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, port) = split_host_port(authority);
    let default_port = match scheme {
        "rtsp" => 554,
        "https" => 443,
        "http" => 80,
        _ => 0,
    };
    let host = host.to_ascii_lowercase();
    // An IPv6 literal keeps its brackets, or the port would read as a group.
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (tail, None),
    };
    let mut key = format!(
        "{scheme}://{host}:{}{}",
        port.unwrap_or(default_port),
        path.trim_end_matches('/')
    );
    if let Some(query) = query {
        key.push('?');
        key.push_str(query);
    }
    key
}

/// Splits `host`, `host:port` or `[ipv6]:port` into its two parts.
fn split_host_port(authority: &str) -> (&str, Option<u16>) {
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let port = rest[end + 1..].strip_prefix(':').and_then(|port| port.parse().ok());
            return (&rest[..end], port);
        }
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => (host, Some(port)),
            Err(_) => (authority, None),
        },
        None => (authority, None),
    }
}

/// True for an `http://` or `https://` URL, which is how an MJPEG stream is
/// addressed.
///
/// The scheme is what tells an MJPEG stream from an RTSP session: MJPEG is
/// negotiated by nothing, so there is no codec or transport to read before the
/// first picture arrives.
pub fn is_http_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("http://") || url.starts_with("https://")
}

/// An `http://` stream URL with its credentials and its query masked.
fn mask_http_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("***@{host}"),
        None => authority.to_string(),
    };
    let tail = match tail.split_once('?') {
        Some((path, _)) => format!("{path}?…"),
        None => tail.to_string(),
    };
    format!("{scheme}://{authority}{tail}")
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

    /// The cycle button walks every mode once and comes back to the first.
    #[test]
    fn the_aspect_button_walks_every_mode() {
        let mut mode = TileAspect::default();
        assert_eq!(mode, TileAspect::Original, "a new camera is left as it arrives");
        let mut seen = vec![mode];
        for _ in 1..TileAspect::ALL.len() {
            mode = mode.next();
            seen.push(mode);
        }
        assert_eq!(seen, TileAspect::ALL.to_vec());
        assert_eq!(TileAspect::Ratio1x1.next(), TileAspect::Original);
        assert_eq!(TileAspect::Stretch.ratio(), None, "stretch is the tile's own shape");
        assert_eq!(TileAspect::Original.ratio(), None, "the stream's own shape");
        assert_eq!(TileAspect::Ratio16x9.ratio(), Some(16.0 / 9.0));
        assert_eq!(TileAspect::Ratio1x1.ratio(), Some(1.0));
    }

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

    #[test]
    fn masks_the_stream_key_of_an_mjpeg_sub_stream() {
        // The stream key travels as a query parameter, so the query is what has
        // to go: it authorises the stream exactly as a password would.
        let camera = CameraSource::new("Door", "rtsp://10.0.0.9:554/Sms=13").with_sub(
            "http://nas.local:5000/webapi/entry.cgi?api=Stream&cameraId=13&StmKey=secret",
        );
        assert_eq!(
            camera.masked_uri(StreamKind::Sub),
            "http://nas.local:5000/webapi/entry.cgi?…"
        );
    }
}
