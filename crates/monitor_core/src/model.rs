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
            StreamKind::Main => "stream-kind-main",
            StreamKind::Sub => "stream-kind-sub",
        }
    }

    /// Short tag rendered inside a grid tile.
    pub fn tag(self) -> &'static str {
        match self {
            StreamKind::Main => "stream-tag-main",
            StreamKind::Sub => "stream-tag-sub",
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
    /// Picture frozen: the transport may still be alive, so the last frame is
    /// held while waiting for the stream to recover rather than dropping to a
    /// black reconnect screen.
    Stalled,
    /// Lost the session, waiting for the backoff timer.
    Reconnecting,
    /// Retries exhausted, waiting for an explicit user action.
    Failed,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            ConnectionState::Idle => "state-idle",
            ConnectionState::Suspended => "state-suspended",
            ConnectionState::Connecting => "state-connecting",
            ConnectionState::Streaming => "state-streaming",
            ConnectionState::Stalled => "state-stalled",
            ConnectionState::Reconnecting => "state-reconnecting",
            ConnectionState::Failed => "state-failed",
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
    Frigate,
    Go2rtc,
}

impl CameraOrigin {
    pub fn label(self) -> &'static str {
        match self {
            CameraOrigin::Manual => "origin-manual",
            CameraOrigin::Onvif => "origin-onvif",
            CameraOrigin::Synology => "origin-synology",
            CameraOrigin::Frigate => "origin-frigate",
            CameraOrigin::Go2rtc => "origin-go2rtc",
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

/// How a channel reaches a camera.
///
/// A camera Frigate or go2rtc describes can be pulled twice over: the RTSP
/// restream, and go2rtc's MSE endpoint (fragmented MP4 over a WebSocket). Which
/// one is reachable depends on the network - the RTSP port is often closed
/// outside the LAN while the web port the MSE stream rides is not. `Auto` keeps
/// one preference, RTSP first, and answers an unreachable RTSP with the MSE
/// address rather than with a backoff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    /// Prefer RTSP, fall back to MSE when it cannot be reached.
    #[default]
    Auto,
    /// RTSP only, with the camera's [`RtspTransport`].
    Rtsp,
    /// MSE (go2rtc WebSocket) only.
    Mse,
}

/// One stop of the per-camera connection switch.
///
/// The switch folds the protocol and the RTSP transport into one walk: a camera
/// with an MSE address moves `Auto -> RtspTcp -> Mse` (its restream is TCP
/// only), and one without just flips the transport, `RtspTcp <-> RtspUdp`. It
/// is derived from [`CameraSource::connect`] and the transport, not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStop {
    Auto,
    RtspTcp,
    RtspUdp,
    Mse,
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
            TileAspect::Original => "aspect-original",
            TileAspect::Stretch => "aspect-stretch",
            TileAspect::Ratio16x9 => "aspect-16x9",
            TileAspect::Ratio4x3 => "aspect-4x3",
            TileAspect::Ratio1x1 => "aspect-1x1",
        }
    }

    /// The shape, written short for a tile's on-screen display.
    pub fn short_label(self) -> &'static str {
        match self {
            TileAspect::Original => "aspect-short-original",
            TileAspect::Stretch => "aspect-short-stretch",
            TileAspect::Ratio16x9 => "aspect-short-16x9",
            TileAspect::Ratio4x3 => "aspect-short-4x3",
            TileAspect::Ratio1x1 => "aspect-short-1x1",
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
    /// The codec, and the rate the stream arrives at.
    ///
    /// The `alias` is what this was called while it showed the transport: the
    /// enum is stored in `xgvconfig.json` by variant name, and a corner saved as
    /// `Link` has to keep working - an unknown variant would take the whole
    /// file, cameras included, down with it.
    #[serde(alias = "Link")]
    Codec,
    /// Frame rate, and which decoder is doing the work.
    Fps,
    /// Picture size, the shape it is drawn in, and the decoder.
    Format,
    /// The connection the channel pulls over and the masked address it pulls -
    /// `AUTO rtsp://…`, `MSE wss://…`, `TCP rtsp://…`.
    Transport,
    /// [`OsdItem::Codec`], [`OsdItem::Fps`], [`OsdItem::Format`] and
    /// [`OsdItem::Transport`], one per line.
    Detail,
}

impl OsdItem {
    /// Every item, in the order the settings button walks them.
    pub const ALL: [OsdItem; 9] = [
        OsdItem::Off,
        OsdItem::Name,
        OsdItem::NumberName,
        OsdItem::Stream,
        OsdItem::Codec,
        OsdItem::Fps,
        OsdItem::Format,
        OsdItem::Transport,
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
            OsdItem::Off => "osd-off",
            OsdItem::Name => "osd-name",
            OsdItem::NumberName => "osd-number-name",
            OsdItem::Stream => "osd-stream",
            OsdItem::Codec => "osd-codec",
            OsdItem::Fps => "osd-fps",
            OsdItem::Format => "osd-format",
            OsdItem::Transport => "osd-transport",
            OsdItem::Detail => "osd-detail",
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
    /// Optional MSE (go2rtc WebSocket) main URL. A Frigate or go2rtc import
    /// fills it in beside the RTSP one; the viewer prefers RTSP and reaches for
    /// this only as the fallback of [`ConnectionMode::Auto`], or when the mode
    /// is [`ConnectionMode::Mse`].
    #[serde(default)]
    pub mse_main: Option<String>,
    /// Optional MSE sub URL, the counterpart of `rtsp_sub`.
    #[serde(default)]
    pub mse_sub: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Login the MSE endpoint is pulled with. Frigate's WebSocket stream is
    /// behind its own session auth, which is not the restream account in
    /// `username` / `password`; they are kept apart so one camera can carry
    /// both.
    #[serde(default)]
    pub mse_username: Option<String>,
    #[serde(default)]
    pub mse_password: Option<String>,
    /// How the channel reaches this camera. See [`ConnectionMode`].
    #[serde(default)]
    pub connect: ConnectionMode,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub origin: CameraOrigin,
    /// Whether a server import set this camera's addresses and still manages
    /// them. While set, the edit form shows the address and credential fields
    /// read-only: hand-editing one address of a managed pair - a Frigate or
    /// go2rtc camera's RTSP and MSE ones - would leave the other pointing at a
    /// different stream. Detaching clears this and the MSE pair, so the camera
    /// becomes one to edit by hand; re-importing refreshes the addresses.
    #[serde(default)]
    pub imported: bool,
    /// Transport used to pull both streams of this camera. Kept per camera so
    /// that a device whose TCP relay damages the stream can be singled out.
    #[serde(default)]
    pub transport: RtspTransport,
    /// How this camera's picture is fitted into its tile on the wall.
    #[serde(default)]
    pub aspect: TileAspect,
    /// How this camera's picture is fitted while the wall stands upright.
    /// Independent from `aspect` so a camera can be stretched in landscape and
    /// kept at its original ratio in portrait.
    #[serde(default)]
    pub portrait_aspect: TileAspect,
    /// ONVIF profile token of the main stream, kept for later re-negotiation.
    #[serde(default)]
    pub main_profile: Option<String>,
    /// ONVIF profile token of the sub stream.
    #[serde(default)]
    pub sub_profile: Option<String>,
    /// Which camera group this entry belongs to, as an index into
    /// [`crate::config::AppConfig::groups`]. The groups are not deduplicated
    /// against each other, so the same camera may be an entry in more than one.
    #[serde(default)]
    pub group: u8,
}

impl CameraSource {
    /// Creates a camera with a generated id.
    pub fn new(name: impl Into<String>, rtsp_main: impl Into<String>) -> Self {
        let rtsp_main = rtsp_main.into();
        let host = stream_host(&rtsp_main).unwrap_or_default();
        Self {
            id: new_id(),
            name: name.into(),
            vendor: None,
            model: None,
            host,
            onvif_port: default_onvif_port(),
            rtsp_main,
            rtsp_sub: None,
            mse_main: None,
            mse_sub: None,
            username: None,
            password: None,
            mse_username: None,
            mse_password: None,
            connect: ConnectionMode::default(),
            enabled: true,
            tags: Vec::new(),
            origin: CameraOrigin::Manual,
            imported: false,
            transport: RtspTransport::default(),
            aspect: TileAspect::default(),
            portrait_aspect: TileAspect::default(),
            main_profile: None,
            sub_profile: None,
            group: 0,
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

    /// Writes back the fields an edit form owns, leaving the rest of the entry
    /// as it is.
    ///
    /// A form is a handful of fields - name, urls, credentials - while an entry
    /// carries more than a form shows: where it came from, the transport it was
    /// switched to, how its picture is fitted into its tile, its tags, its ONVIF
    /// profile tokens. Those are the user's or the device's, and the form has no
    /// value to offer for them: it is built from [`CameraSource::new`], where
    /// every one of them is a default. Replacing the entry wholesale with what
    /// the form built is therefore what silently resets them.
    ///
    /// The host follows the main url, which is the only part of the address the
    /// form can change. The ONVIF port is left alone: the form cannot name one,
    /// and the entry's was discovered for this device.
    pub fn apply_edit(&mut self, form: &CameraSource) {
        self.name = form.name.clone();
        self.rtsp_main = form.rtsp_main.clone();
        self.rtsp_sub = form.rtsp_sub.clone();
        self.mse_main = form.mse_main.clone();
        self.mse_sub = form.mse_sub.clone();
        self.username = form.username.clone();
        self.password = form.password.clone();
        self.mse_username = form.mse_username.clone();
        self.mse_password = form.mse_password.clone();
        self.host = form.host.clone();
    }

    /// Takes a camera out of the import's hands, so its addresses can be edited
    /// by hand.
    ///
    /// Only the mark is cleared: the addresses, the credentials and the
    /// connection are kept, so an imported camera keeps the MSE address its
    /// `Auto` preference falls back to. The edit form offers the two addresses
    /// together, so a hand edit keeps them in step. The origin is kept too:
    /// where the camera came from is still true.
    pub fn detach(&mut self) {
        self.imported = false;
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

    /// The MSE URL of a stream, when one is configured; an empty sub falls back
    /// to the MSE main, the way [`CameraSource::stream_uri`] does for RTSP.
    pub fn mse_uri(&self, kind: StreamKind) -> Option<&str> {
        let value = match kind {
            StreamKind::Main => self.mse_main.as_deref(),
            StreamKind::Sub => self.mse_sub.as_deref().or(self.mse_main.as_deref()),
        };
        value.filter(|url| !url.trim().is_empty())
    }

    /// Whether an MSE address is available - so the connection switch can offer
    /// the MSE stop, and `Auto` has something to fall back to.
    pub fn has_mse(&self) -> bool {
        self.mse_uri(StreamKind::Main).is_some()
    }

    /// The login the MSE endpoint is pulled with, if one is configured.
    pub fn mse_credentials(&self) -> Option<(String, String)> {
        let username = self.mse_username.as_deref().filter(|name| !name.trim().is_empty())?;
        Some((username.to_string(), self.mse_password.clone().unwrap_or_default()))
    }

    /// The URL a channel pulls first, for the configured [`ConnectionMode`]: the
    /// MSE address in `Mse`, the RTSP address otherwise.
    pub fn active_uri(&self, kind: StreamKind) -> &str {
        match self.connect {
            ConnectionMode::Mse => self.mse_uri(kind).unwrap_or_else(|| self.stream_uri(kind)),
            ConnectionMode::Auto | ConnectionMode::Rtsp => self.stream_uri(kind),
        }
    }

    /// Whether this camera is carried over a WebSocket (MSE) address as its one
    /// transport - a hand-entered `ws(s)://` url standing in for the RTSP one.
    ///
    /// Such a camera has no RTSP session to fall back to and no transport to
    /// pick between, so the connection switch has nothing to offer and is drawn
    /// without.
    pub fn is_ws_stream(&self) -> bool {
        is_ws_url(&self.rtsp_main)
    }

    /// Whether this camera is addressed through a go2rtc or Frigate restream
    /// rather than the device itself.
    ///
    /// Both importers hold the camera in their own configuration and serve it
    /// under a stream name, which tells two things apart from a camera carried
    /// directly: the RTSP transport is the server's own (TCP only - go2rtc
    /// answers every UDP `SETUP` with `461` on purpose), and the vendor
    /// sub-stream patterns, which read a camera's own url, have nothing to
    /// match.
    pub fn is_restream(&self) -> bool {
        matches!(self.origin, CameraOrigin::Frigate | CameraOrigin::Go2rtc)
    }

    /// The stops the per-camera connection switch walks, in the order a press
    /// moves through them.
    ///
    /// A camera with an MSE address offers `Auto -> Rtsp -> Mse`; one without
    /// offers only the RTSP transport (`Tcp -> Udp`), which is what it always
    /// did. UDP is left out where the restream carries TCP alone. A camera
    /// carried over a WebSocket url has one transport, so its only stop is MSE.
    pub fn connection_stops(&self) -> &'static [ConnectionStop] {
        if self.is_ws_stream() {
            return &[ConnectionStop::Mse];
        }
        match (self.has_mse(), !self.is_restream()) {
            (true, true) => &[
                ConnectionStop::Auto,
                ConnectionStop::RtspTcp,
                ConnectionStop::RtspUdp,
                ConnectionStop::Mse,
            ],
            (true, false) => &[ConnectionStop::Auto, ConnectionStop::RtspTcp, ConnectionStop::Mse],
            (false, _) => &[ConnectionStop::RtspTcp, ConnectionStop::RtspUdp],
        }
    }

    /// The stop the camera is on now.
    ///
    /// `Auto` without an MSE address has nothing to fall back to, so it reads
    /// as the RTSP transport it really pulls rather than as a stop the switch
    /// does not offer.
    pub fn connection_stop(&self) -> ConnectionStop {
        if self.is_ws_stream() {
            return ConnectionStop::Mse;
        }
        let transport = match self.transport {
            RtspTransport::Tcp => ConnectionStop::RtspTcp,
            RtspTransport::Udp => ConnectionStop::RtspUdp,
        };
        match self.connect {
            ConnectionMode::Auto if self.has_mse() => ConnectionStop::Auto,
            ConnectionMode::Auto | ConnectionMode::Rtsp => transport,
            ConnectionMode::Mse => ConnectionStop::Mse,
        }
    }

    /// The label the connection switch shows for the camera's current stop. It
    /// names the protocol where there are two to choose from, and the transport
    /// where there is only RTSP.
    pub fn connection_label(&self) -> &'static str {
        match self.connection_stop() {
            ConnectionStop::Auto => "AUTO",
            ConnectionStop::Mse => "MSE",
            ConnectionStop::RtspTcp => {
                if self.has_mse() {
                    "RTSP"
                } else {
                    "TCP"
                }
            }
            ConnectionStop::RtspUdp => "UDP",
        }
    }

    /// Moves the connection switch on to the next stop it offers, writing both
    /// the mode and the transport back.
    pub fn advance_connection(&mut self) {
        let stops = self.connection_stops();
        let current = self.connection_stop();
        let index = stops.iter().position(|stop| *stop == current).unwrap_or(0);
        let next = stops[(index + 1) % stops.len()];
        match next {
            ConnectionStop::Auto => self.connect = ConnectionMode::Auto,
            ConnectionStop::RtspTcp => {
                self.connect = ConnectionMode::Rtsp;
                self.transport = RtspTransport::Tcp;
            }
            ConnectionStop::RtspUdp => {
                self.connect = ConnectionMode::Rtsp;
                self.transport = RtspTransport::Udp;
            }
            ConnectionStop::Mse => self.connect = ConnectionMode::Mse,
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
    /// It follows the connection mode: the URL shown is the one the channel
    /// pulls first, so a camera forced onto MSE reads out its MSE address and
    /// the tile, the log and the settings row all agree with what is running.
    ///
    /// A Synology camera keeps its sub stream on HTTP MJPEG rather than RTSP, so
    /// the RTSP parser is only the first of two readings: an `http://` URL is
    /// masked here instead. The stream key it carries as a query parameter is no
    /// less of a credential than a password, and a log is where it would end up.
    pub fn masked_uri(&self, kind: StreamKind) -> String {
        mask_stream_url(self.active_uri(kind))
    }

    /// The masked RTSP URL of a stream, whatever the connection mode - so an
    /// edit form can show both addresses of a managed pair at once.
    pub fn masked_rtsp_uri(&self, kind: StreamKind) -> String {
        mask_stream_url(self.stream_uri(kind))
    }

    /// The masked MSE URL of a stream, when one is configured - so a row can
    /// show the fallback beside the address it is actually pulling.
    pub fn masked_mse_uri(&self, kind: StreamKind) -> Option<String> {
        self.mse_uri(kind).map(mask_stream_url)
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
        "https" | "wss" => 443,
        "http" | "ws" => 80,
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

/// The host of a stream url, whatever scheme it carries: read through the RTSP
/// parser where the scheme is one it knows (its userinfo and IPv6 handling come
/// along), and otherwise off the authority an HTTP or WebSocket url gives.
fn stream_host(url: &str) -> Option<String> {
    if let Ok(parsed) = RtspUrl::parse(url) {
        return parsed.host().map(str::to_string);
    }
    let (_, rest) = url.trim().split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, _) = split_host_port(authority);
    (!host.is_empty()).then(|| host.to_string())
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

/// True for a `ws://` or `wss://` URL, which is how a go2rtc MSE (WebSocket)
/// stream is addressed.
pub fn is_ws_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("ws://") || url.starts_with("wss://")
}

/// True for the schemes a stream url may carry: RTSP (`rtsp` / `rtsps`), an
/// MJPEG stream (`http` / `https`) or an MSE one (`ws` / `wss`).
///
/// The entry form validates a hand-typed address against this, so a camera can
/// be a WebSocket stream alone rather than only over RTSP.
pub fn is_stream_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    is_http_url(&url)
        || is_ws_url(&url)
        || url.starts_with("rtsp://")
        || url.starts_with("rtsps://")
}

/// A stream URL with its credentials masked, whatever scheme it carries: RTSP
/// through its own parser, an HTTP (MJPEG) or WebSocket (MSE) URL through the
/// HTTP one, which also hides the query a stream key may ride in.
fn mask_stream_url(url: &str) -> String {
    if let Ok(parsed) = RtspUrl::parse(url) {
        return parsed.masked();
    }
    if is_http_url(url) || is_ws_url(url) {
        return mask_http_url(url);
    }
    "<invalid stream url>".to_string()
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

    /// The OSD enum is stored in `xgvconfig.json` by variant name, so a corner
    /// saved before `Link` became `Codec` has to keep reading - an unknown
    /// variant fails the whole file, cameras and all. The write side is the new
    /// name, which is what makes the alias a one-way door rather than a second
    /// spelling to maintain.
    #[test]
    fn a_corner_saved_as_link_still_reads() {
        let item: OsdItem = serde_json::from_str("\"Link\"").expect("a corner saved earlier");
        assert_eq!(item, OsdItem::Codec);
        assert_eq!(serde_json::to_string(&OsdItem::Codec).expect("written"), "\"Codec\"");
    }

    /// An edit form writes back only the fields it shows.
    #[test]
    fn an_edit_keeps_the_fields_the_form_does_not_show() {
        let mut camera = CameraSource::new("front", "rtsp://10.0.0.1/main")
            .with_sub("rtsp://10.0.0.1/sub")
            .with_credentials("admin", "pw");
        camera.aspect = TileAspect::Ratio16x9;
        camera.transport = RtspTransport::Udp;
        camera.tags = vec!["gate".to_string()];
        camera.vendor = Some("FOSCAM".to_string());
        camera.main_profile = Some("prof0".to_string());

        // What the form builds on Save: a fresh camera, everything else default.
        let mut form = CameraSource::new("front door", "rtsp://10.0.0.2/main");
        form.rtsp_sub = Some("rtsp://10.0.0.2/sub".to_string());
        form.username = Some("admin".to_string());
        form.password = Some("pw".to_string());

        camera.apply_edit(&form);

        assert_eq!(camera.name, "front door");
        assert_eq!(camera.rtsp_main, "rtsp://10.0.0.2/main");
        assert_eq!(camera.host, "10.0.0.2", "the host follows the url it names");
        assert_eq!(camera.aspect, TileAspect::Ratio16x9, "the display mode survives");
        assert_eq!(camera.transport, RtspTransport::Udp, "the transport survives");
        assert_eq!(camera.tags, vec!["gate".to_string()]);
        assert_eq!(camera.vendor.as_deref(), Some("FOSCAM"));
        assert_eq!(camera.main_profile.as_deref(), Some("prof0"));
    }

    /// A camera an import gave a pair of addresses to, as Frigate does.
    fn frigate_camera() -> CameraSource {
        let mut camera = CameraSource::new("cam", "rtsp://10.0.0.1:8554/cam_1");
        camera.origin = CameraOrigin::Frigate;
        camera.mse_main = Some("wss://10.0.0.1:8971/live/mse/api/ws?src=cam_1".to_string());
        camera.mse_username = Some("frigate".to_string());
        camera.mse_password = Some("pw".to_string());
        camera
    }

    /// A camera with an MSE address walks `AUTO -> RTSP -> MSE` and back.
    #[test]
    fn a_camera_with_an_mse_address_walks_auto_rtsp_mse() {
        let mut camera = frigate_camera();
        assert_eq!(camera.connection_label(), "AUTO");
        camera.advance_connection();
        assert_eq!(camera.connect, ConnectionMode::Rtsp);
        assert_eq!(camera.transport, RtspTransport::Tcp);
        assert_eq!(camera.connection_label(), "RTSP");
        camera.advance_connection();
        assert_eq!(camera.connect, ConnectionMode::Mse);
        assert_eq!(camera.connection_label(), "MSE");
        camera.advance_connection();
        assert_eq!(camera.connect, ConnectionMode::Auto);
        assert_eq!(camera.connection_label(), "AUTO");
    }

    /// Only the two importer's holdings are a restream; a camera carried
    /// directly - manual or ONVIF - is not.
    #[test]
    fn a_restream_is_a_go2rtc_or_frigate_camera() {
        assert!(frigate_camera().is_restream());
        assert!(!CameraSource::new("plain", "rtsp://10.0.0.1/live").is_restream());
        let mut synology = CameraSource::new("syno", "rtsp://nas/live");
        synology.origin = CameraOrigin::Synology;
        assert!(!synology.is_restream());
    }

    /// A go2rtc/Frigate restream carries RTSP over TCP alone, so the switch
    /// never lands on UDP for it.
    #[test]
    fn a_restream_never_offers_udp() {
        let mut camera = frigate_camera();
        for _ in 0..3 {
            camera.advance_connection();
            assert!(!camera.transport.is_udp(), "RTSP over TCP alone here");
        }
    }

    /// A camera with no MSE address keeps the transport-only switch it had.
    #[test]
    fn a_camera_without_an_mse_address_cycles_its_transport() {
        let mut camera = CameraSource::new("plain", "rtsp://10.0.0.1/live");
        assert_eq!(camera.connection_label(), "TCP");
        camera.advance_connection();
        assert_eq!(camera.transport, RtspTransport::Udp);
        assert_eq!(camera.connect, ConnectionMode::Rtsp);
        assert_eq!(camera.connection_label(), "UDP");
        camera.advance_connection();
        assert_eq!(camera.transport, RtspTransport::Tcp);
        assert_eq!(camera.connection_label(), "TCP");
    }

    /// A hand-entered WebSocket address carries the host the rest of the app
    /// reads, and its one transport is MSE - so the switch has nothing to walk.
    #[test]
    fn a_websocket_camera_reads_its_host_and_carries_no_switch() {
        let camera = CameraSource::new("mse", "ws://10.0.0.5:1984/api/ws?src=cam");
        assert_eq!(camera.host, "10.0.0.5");
        assert_eq!(camera.display_address(), "10.0.0.5");
        assert!(camera.is_ws_stream());
        assert_eq!(camera.connection_stops(), &[ConnectionStop::Mse]);
        assert_eq!(camera.connection_label(), "MSE");
    }

    /// The WebSocket camera's stop is MSE whatever the mode says, and a press
    /// leaves it there rather than walking onto an RTSP transport it lacks.
    #[test]
    fn a_websocket_camera_stays_on_mse() {
        let mut camera = CameraSource::new("mse", "wss://user:pw@cam.local/api/ws?src=a");
        assert_eq!(camera.host, "cam.local");
        assert_eq!(camera.identity(), "wss://cam.local:443/api/ws?src=a");
        camera.advance_connection();
        assert_eq!(camera.connect, ConnectionMode::Mse);
        assert_eq!(camera.connection_label(), "MSE");
    }

    /// The URL pulled first follows the mode: MSE reads out its own address,
    /// RTSP and Auto the RTSP one.
    #[test]
    fn the_active_uri_follows_the_mode() {
        let mut camera = frigate_camera();
        assert_eq!(camera.active_uri(StreamKind::Main), "rtsp://10.0.0.1:8554/cam_1");
        camera.connect = ConnectionMode::Mse;
        assert_eq!(
            camera.active_uri(StreamKind::Main),
            "wss://10.0.0.1:8971/live/mse/api/ws?src=cam_1"
        );
    }

    /// The masked URL shown, in a row or a log, is the one the mode pulls.
    #[test]
    fn the_masked_uri_follows_the_mode() {
        let mut camera = frigate_camera();
        assert_eq!(camera.masked_uri(StreamKind::Main), "rtsp://10.0.0.1:8554/cam_1");
        camera.connect = ConnectionMode::Mse;
        assert_eq!(
            camera.masked_uri(StreamKind::Main),
            "wss://10.0.0.1:8971/live/mse/api/ws?…"
        );
        // The MSE credentials are separate from the restream account, and the
        // masked RTSP URL is available whatever the mode.
        assert_eq!(camera.mse_credentials(), Some(("frigate".to_string(), "pw".to_string())));
        assert_eq!(camera.masked_rtsp_uri(StreamKind::Main), "rtsp://10.0.0.1:8554/cam_1");
    }

    /// A hand-added camera is nobody's import, so its form is editable.
    #[test]
    fn a_hand_added_camera_is_not_imported() {
        assert!(!CameraSource::new("front", "rtsp://10.0.0.1/main").imported);
    }

    /// Detaching only clears the mark: the MSE pair, its login and the
    /// connection are kept, so the camera still has the address `Auto` falls
    /// back to. The form is what becomes editable.
    #[test]
    fn detaching_only_unlocks_the_form() {
        let mut camera = frigate_camera();
        camera.imported = true;
        camera.connect = ConnectionMode::Mse;

        camera.detach();

        assert!(!camera.imported);
        assert!(camera.has_mse());
        assert_eq!(camera.connect, ConnectionMode::Mse);
        assert_eq!(camera.mse_credentials(), Some(("frigate".to_string(), "pw".to_string())));
        assert_eq!(camera.rtsp_main, "rtsp://10.0.0.1:8554/cam_1");
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
