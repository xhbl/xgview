//! Asynchronous RTSP client using the TCP interleaved transport.
//!
//! The client implements just enough of RFC 2326 to negotiate a session
//! (`OPTIONS` / `DESCRIBE` / `SETUP` / `PLAY` / `TEARDOWN`) and to receive
//! interleaved RTP payloads. Decoding itself lives in the `monitor_codec`
//! crate; this module only deals with the transport so that it stays
//! testable on every platform.
//!
//! Cameras answer `401 Unauthorized` to the first `DESCRIBE` and advertise
//! either `Basic` or `Digest` in `WWW-Authenticate`; the challenge is answered
//! through [`crate::digest`] and the request replayed once.

use std::fmt;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::io::AsyncWrite;
use tokio::net::{TcpStream, UdpSocket};

use crate::digest::{self, Challenge};
use crate::error::{CoreError, Result};
use crate::model::RtspTransport;

/// Default RTSP port.
pub const RTSP_DEFAULT_PORT: u16 = 554;

/// Maximum size accepted for a single interleaved RTP payload.
const MAX_INTERLEAVED_PAYLOAD: usize = 1 << 21;

/// The `Require` header a camera that implements ONVIF Profile T looks for
/// before it advertises its talk-back media in the session description.
pub const BACKCHANNEL_REQUIRE: &str = "www.onvif.org/ver20/backchannel";

/// Adds one query parameter to a URL, before any fragment it carries.
pub fn append_query_flag(uri: &str, flag: &str) -> String {
    let (head, fragment) = match uri.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (uri, None),
    };
    let mut out = String::with_capacity(uri.len() + flag.len() + 1);
    out.push_str(head);
    out.push(if head.contains('?') { '&' } else { '?' });
    out.push_str(flag);
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Drops one query parameter from a URL, leaving the rest of it as it was.
pub fn strip_query_flag(uri: &str, name: &str) -> String {
    let (head, fragment) = match uri.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (uri, None),
    };
    let Some((path, query)) = head.split_once('?') else { return uri.to_string() };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|part| !part.starts_with(name) || !part[name.len()..].starts_with('='))
        .collect();
    let mut out = String::with_capacity(uri.len());
    out.push_str(path);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Keep alive delay used when the server announces no session timeout.
const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(25);

/// Shortest keep alive delay, so that a device announcing a tiny timeout is not
/// flooded with `OPTIONS` requests.
const MIN_KEEP_ALIVE: Duration = Duration::from_secs(5);

/// A parsed `rtsp://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspUrl {
    scheme: String,
    username: Option<String>,
    password: Option<String>,
    host: String,
    port: Option<u16>,
    path: String,
    query: Option<String>,
}

impl RtspUrl {
    /// Parses an RTSP URL. `rtsps://` is accepted as an alias of `rtsp://`.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| CoreError::parse(format!("missing scheme in url: {input}")))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "rtsp" && scheme != "rtsps" {
            return Err(CoreError::parse(format!("unsupported scheme: {scheme}")));
        }

        let (authority, tail) = match rest.find(['/', '?']) {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };

        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((userinfo, hostport)) => (Some(userinfo), hostport),
            None => (None, authority),
        };

        let (username, password) = match userinfo {
            Some(userinfo) => match userinfo.split_once(':') {
                Some((user, pass)) => (Some(user.to_string()), Some(pass.to_string())),
                None => (Some(userinfo.to_string()), None),
            },
            None => (None, None),
        };

        let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
            // IPv6 literal, e.g. [fe80::1]:554
            let end = stripped
                .find(']')
                .ok_or_else(|| CoreError::parse("unterminated IPv6 literal in url"))?;
            let host = &stripped[..end];
            let port = stripped[end + 1..].strip_prefix(':').and_then(|p| p.parse().ok());
            (host.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((host, port)) => match port.parse::<u16>() {
                    Ok(port) => (host.to_string(), Some(port)),
                    Err(_) => (hostport.to_string(), None),
                },
                None => (hostport.to_string(), None),
            }
        };

        if host.is_empty() {
            return Err(CoreError::parse("missing host in url"));
        }

        let (path, query) = match tail.split_once('?') {
            Some((path, query)) => {
                let path = if path.is_empty() { "/".to_string() } else { path.to_string() };
                (path, Some(query.to_string()))
            }
            None => (if tail.is_empty() { "/".to_string() } else { tail.to_string() }, None),
        };

        Ok(Self { scheme, username, password, host, port, path, query })
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Port used for the TCP connection, falling back to 554.
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(RTSP_DEFAULT_PORT)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn credentials(&self) -> Option<(&str, &str)> {
        match (self.username.as_deref(), self.password.as_deref()) {
            (Some(user), Some(pass)) => Some((user, pass)),
            (Some(user), None) => Some((user, "")),
            _ => None,
        }
    }

    /// Absolute request URI without user info, used inside RTSP requests.
    pub fn request_uri(&self) -> String {
        let mut out = format!("rtsp://{}", self.host);
        if let Some(port) = self.port {
            out.push_str(&format!(":{port}"));
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        out
    }

    /// Rebuilds the URL including credentials.
    pub fn full(&self) -> String {
        let mut out = format!("{}://", self.scheme);
        if let Some((user, pass)) = self.credentials() {
            out.push_str(user);
            if !pass.is_empty() {
                out.push(':');
                out.push_str(pass);
            }
            out.push('@');
        }
        out.push_str(&self.host);
        if let Some(port) = self.port {
            out.push_str(&format!(":{port}"));
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        out
    }

    /// URL with credentials replaced by `***`, safe for logs and UI.
    pub fn masked(&self) -> String {
        let mut out = format!("{}://", self.scheme);
        if self.username.is_some() {
            out.push_str("***");
            if self.password.is_some() {
                out.push_str(":***");
            }
            out.push('@');
        }
        out.push_str(&self.host);
        if let Some(port) = self.port {
            out.push_str(&format!(":{port}"));
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        out
    }
}

impl fmt::Display for RtspUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.masked())
    }
}

/// A parsed RTSP response.
#[derive(Debug, Clone)]
pub struct RtspResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RtspResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Case insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// Picks the authentication challenge advertised by a `401` response.
///
/// Several `WWW-Authenticate` headers may be present at once; `Digest` is
/// preferred over `Basic` because it does not send the password in clear.
fn parse_challenge(response: &RtspResponse) -> Option<Challenge> {
    let mut basic = None;
    for (key, value) in &response.headers {
        if !key.eq_ignore_ascii_case("www-authenticate") {
            continue;
        }
        let Some(challenge) = digest::parse(value) else { continue };
        if challenge.scheme == "digest" {
            return Some(challenge);
        }
        basic.get_or_insert(challenge);
    }
    basic
}

/// One item read from the RTSP connection.
#[derive(Debug, Clone)]
pub enum RtspPacket {
    /// RTSP control response.
    Response(RtspResponse),
    /// Interleaved RTP / RTCP payload.
    Interleaved { channel: u8, payload: Vec<u8> },
}

/// One item read from the media transport, whichever it is.
///
/// The interleaved transport multiplexes RTP and RTCP on the TCP connection and
/// both reach the caller. UDP binds a socket per direction, so only RTP is ever
/// returned: the control datagrams are drained inside [`RtspClient::read_media`]
/// so that their receive buffer never fills up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPacket {
    /// A complete RTP packet of the video track, header included.
    Rtp(Vec<u8>),
    /// An RTCP packet of the video track (interleaved transport only).
    Rtcp(Vec<u8>),
    /// A complete RTP packet of the audio track, which travels on the second
    /// `SETUP`'s interleaved pair.
    AudioRtp(Vec<u8>),
    /// An RTCP packet of the audio track.
    AudioRtcp(Vec<u8>),
}

/// A video (or audio) track described by the session SDP.
#[derive(Debug, Clone, PartialEq)]
pub struct SdpTrack {
    pub media: String,
    pub control: Option<String>,
    pub encoding: Option<String>,
    pub clock_rate: Option<u32>,
    pub payload_type: Option<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    /// Channel count from an audio `rtpmap` (`.../16000/1`), which the parser
    /// used to throw away.
    pub channels: Option<u8>,
    /// The raw `a=fmtp` body, kept for audio: AAC's `config=` and the RFC 3640
    /// size / index lengths a `MPEG4-GENERIC` depacketiser needs.
    pub fmtp: Option<String>,
    /// Offered direction: `a=sendonly` / `a=recvonly` / `a=sendrecv`. The back
    /// channel is the `sendonly` audio media (§10 of the audio plan).
    pub direction: SdpDirection,
    /// Raw parameter set NAL units the SDP advertised - H.264's SPS and PPS
    /// from `sprop-parameter-sets`, or H.265's VPS, SPS and PPS from
    /// `sprop-vps` / `sprop-sps` / `sprop-pps` - in the order the camera listed
    /// them. Empty when the device does not advertise them, in which case they
    /// have to be picked up from the stream itself.
    pub parameter_sets: Vec<Vec<u8>>,
}

/// Offered direction of an SDP media, `a=sendrecv` when the line is absent.
///
/// Only `sendonly` audio is load bearing here: that is the camera's back
/// channel, a media the client sends. A camera's own audio track is `recvonly`
/// from the client's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SdpDirection {
    #[default]
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl SdpTrack {
    pub fn is_video(&self) -> bool {
        self.media.eq_ignore_ascii_case("video")
    }

    /// An audio track, the counterpart to [`SdpTrack::is_video`] for the second
    /// `SETUP`.
    pub fn is_audio(&self) -> bool {
        self.media.eq_ignore_ascii_case("audio")
    }

    /// Offered with `a=sendonly`: the client sends this media. For audio that
    /// is the back channel; a media the client receives is `recvonly`.
    pub fn is_sendonly(&self) -> bool {
        self.direction == SdpDirection::SendOnly
    }

    /// Human readable resolution, e.g. `1920x1080`.
    pub fn resolution(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(format!("{w}x{h}")),
            _ => None,
        }
    }
}

/// Splits a `Session` header into its identifier and its timeout.
///
/// Cameras answer `SETUP` with `Session: 12345678;timeout=60`, where the
/// timeout is the number of seconds of client silence the server tolerates.
/// Both parts are optional.
pub fn parse_session_header(value: &str) -> (Option<String>, Option<Duration>) {
    let mut identifier = None;
    let mut timeout = None;
    for part in value.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((key, number)) if key.trim().eq_ignore_ascii_case("timeout") => {
                if let Ok(seconds) = number.trim().parse::<u64>() {
                    timeout = Some(Duration::from_secs(seconds));
                }
            }
            _ if identifier.is_none() => identifier = Some(part.to_string()),
            _ => {}
        }
    }
    (identifier, timeout)
}

/// Extracts the parameter sets advertised by an `a=fmtp` line.
///
/// H.264 lists them in one comma separated `sprop-parameter-sets` field, e.g.
/// `a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z0IAKeKQCgC3YC3AQEBpB4kRUA==,aM48gA==`;
/// HEVC (RFC 7798) advertises the three separately, e.g.
/// `a=fmtp:96 sprop-pps=RAHApXwMyQ==;sprop-sps=QgEBAWAA...;sprop-vps=QAEMAf//AWAA...`.
/// Both are collected here, in the order the camera listed them: the consumer
/// keys the sets by the type inside each NAL, so that order is not relied on.
///
/// Advertising them lets a viewer that joins mid-stream wait for a key frame
/// instead of having to guess the decoder configuration.
pub fn parse_parameter_sets(fmtp: &str) -> Vec<Vec<u8>> {
    use base64::Engine;

    let engine = base64::engine::general_purpose::STANDARD;
    let mut sets = Vec::new();
    for parameter in fmtp.split(';') {
        let Some((key, value)) = parameter.split_once('=') else { continue };
        let key = key.trim();
        if key.eq_ignore_ascii_case("sprop-parameter-sets") {
            // H.264: one field, the sets inside it comma separated.
            sets.extend(
                value
                    .split(',')
                    .filter_map(|entry| engine.decode(entry.trim()).ok())
                    .filter(|nal| !nal.is_empty()),
            );
        } else if key.eq_ignore_ascii_case("sprop-vps")
            || key.eq_ignore_ascii_case("sprop-sps")
            || key.eq_ignore_ascii_case("sprop-pps")
        {
            // H.265: VPS, SPS and PPS each in a field of its own.
            if let Ok(nal) = engine.decode(value.trim()) {
                if !nal.is_empty() {
                    sets.push(nal);
                }
            }
        }
    }
    sets
}

/// Minimal SDP parser extracting the media tracks of a session description.
pub fn parse_sdp(sdp: &str) -> Vec<SdpTrack> {
    use regex::Regex;
    use std::sync::LazyLock;

    static FRAMESIZE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^a=framesize:(\d+)\s+(\d+)-(\d+)").expect("valid framesize regex")
    });
    static RTPMAP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^a=rtpmap:(\d+)\s+([^/]+)/(\d+)(?:/(\d+))?").expect("valid rtpmap regex")
    });
    static FRAMERATE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^a=framerate:([0-9.]+)").expect("valid framerate regex")
    });
    static FMTP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^a=fmtp:(\d+)\s+(.+)").expect("valid fmtp regex"));

    let mut tracks: Vec<SdpTrack> = Vec::new();

    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            let mut parts = rest.split_whitespace();
            let media = parts.next().unwrap_or_default().to_string();
            let payload_type = parts.nth(2).and_then(|pt| pt.parse::<u8>().ok());
            tracks.push(SdpTrack {
                media,
                control: None,
                encoding: None,
                clock_rate: None,
                payload_type,
                width: None,
                height: None,
                fps: None,
                channels: None,
                fmtp: None,
                direction: SdpDirection::SendRecv,
                parameter_sets: Vec::new(),
            });
            continue;
        }

        let Some(track) = tracks.last_mut() else { continue };

        if let Some(rest) = line.strip_prefix("a=control:") {
            track.control = Some(rest.trim().to_string());
        } else if let Some(caps) = RTPMAP.captures(line) {
            let pt = caps[1].parse::<u8>().ok();
            if pt == track.payload_type || track.encoding.is_none() {
                track.encoding = Some(caps[2].to_ascii_uppercase());
                track.clock_rate = caps[3].parse::<u32>().ok();
                // The optional `/channels` an audio rtpmap carries.
                if let Some(channels) = caps.get(4).and_then(|m| m.as_str().parse::<u8>().ok()) {
                    track.channels = Some(channels);
                }
            }
        } else if let Some(caps) = FRAMESIZE.captures(line) {
            let pt = caps[1].parse::<u8>().ok();
            if pt == track.payload_type || track.width.is_none() {
                track.width = caps[2].parse::<u32>().ok();
                track.height = caps[3].parse::<u32>().ok();
            }
        } else if let Some(caps) = FRAMERATE.captures(line) {
            track.fps = caps[1].parse::<f32>().ok();
        } else if let Some(caps) = FMTP.captures(line) {
            let pt = caps[1].parse::<u8>().ok();
            if pt == track.payload_type || track.parameter_sets.is_empty() {
                track.parameter_sets = parse_parameter_sets(&caps[2]);
                // Keep the whole body too: the audio codecs read `config=` and
                // the RFC 3640 size / index lengths out of it.
                if track.fmtp.is_none() {
                    track.fmtp = Some(caps[2].to_string());
                }
            }
        } else if line.eq_ignore_ascii_case("a=sendonly") {
            track.direction = SdpDirection::SendOnly;
        } else if line.eq_ignore_ascii_case("a=recvonly") {
            track.direction = SdpDirection::RecvOnly;
        } else if line.eq_ignore_ascii_case("a=sendrecv") {
            track.direction = SdpDirection::SendRecv;
        } else if line.eq_ignore_ascii_case("a=inactive") {
            track.direction = SdpDirection::Inactive;
        }
    }

    // A G.711 track may carry no `rtpmap` at all: RFC 3551 gives PCMU and PCMA
    // the static payload types 0 and 8, and cameras lean on that.
    for track in &mut tracks {
        if track.encoding.is_none() {
            match track.payload_type {
                Some(0) => {
                    track.encoding = Some("PCMU".to_string());
                    track.clock_rate = Some(8000);
                }
                Some(8) => {
                    track.encoding = Some("PCMA".to_string());
                    track.clock_rate = Some(8000);
                }
                _ => {}
            }
        }
    }

    tracks
}

/// Minimal RTP header parser (RFC 3550).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub csrc_count: u8,
    pub header_len: usize,
}

/// Parses an RTP header, returning `None` when the buffer is too short.
pub fn parse_rtp_header(data: &[u8]) -> Option<RtpHeader> {
    if data.len() < 12 {
        return None;
    }
    let version = data[0] >> 6;
    if version != 2 {
        return None;
    }
    let padding = data[0] & 0x20 != 0;
    let extension = data[0] & 0x10 != 0;
    let csrc_count = data[0] & 0x0f;
    let marker = data[1] & 0x80 != 0;
    let payload_type = data[1] & 0x7f;
    let sequence = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let mut header_len = 12 + csrc_count as usize * 4;
    if extension && data.len() >= header_len + 4 {
        let ext_len = u16::from_be_bytes([data[header_len + 2], data[header_len + 3]]) as usize;
        header_len += 4 + ext_len * 4;
    }
    if data.len() < header_len {
        return None;
    }
    Some(RtpHeader {
        version,
        padding,
        extension,
        marker,
        payload_type,
        sequence,
        timestamp,
        ssrc,
        csrc_count,
        header_len,
    })
}

/// How many interleaved frames may wait for the socket. A call sends fifty a
/// second, so this is about two seconds of audio: past it the oldest frame is
/// dropped rather than growing the queue.
const OUTBOX_FRAMES: usize = 100;

/// Receive buffer requested for an RTP or RTCP socket, in bytes.
///
/// A camera, or the relay in front of it, hands a high bitrate stream over in
/// bursts: a key frame arrives as hundreds of datagrams back to back and the
/// line then goes quiet until the next picture. The kernel default - some 64 KiB
/// on Windows - holds a few tens of milliseconds of that, so any moment the
/// reader is away from the socket costs the tail of the burst. Every datagram
/// lost that way shows up as a sequence gap, and a gap writes the whole picture
/// its packets belonged to off, which is what a tile stuttering every few
/// seconds looks like. The buffer is therefore sized to hold a burst rather than
/// a moment. The kernel clamps the request to its own ceiling, so asking for
/// more than it will grant is harmless.
const UDP_RECEIVE_BUFFER: usize = 4 * 1024 * 1024;

/// Binds one UDP socket with the receive buffer a video stream needs.
///
/// The buffer cannot be set on a socket that tokio has already taken over, so
/// the socket is built with `socket2` and handed to tokio once it is ready.
fn bind_udp(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // Best effort: a socket that only got the default is still a working socket.
    // What was granted is logged because a stutter that survives this change is
    // worth being able to read back as "the buffer never grew".
    if let Err(err) = socket.set_recv_buffer_size(UDP_RECEIVE_BUFFER) {
        tracing::debug!(target: "xgview::rtsp", %err, "cannot enlarge the udp receive buffer");
    } else if let Ok(granted) = socket.recv_buffer_size() {
        tracing::debug!(
            target: "xgview::rtsp",
            requested = UDP_RECEIVE_BUFFER,
            granted,
            "udp receive buffer"
        );
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// Binds a consecutive pair of local UDP ports for RTP and RTCP.
///
/// RFC 3550 asks for an even RTP port followed by its odd neighbour. A socket
/// bound to port 0 is handed a random one, so the pair is retried until the
/// kernel returns an even number and the neighbour turns out to be free.
fn bind_rtp_pair(ipv6: bool) -> std::io::Result<(UdpSocket, UdpSocket)> {
    let any = if ipv6 {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    };
    for _ in 0..32 {
        let rtp = bind_udp(any)?;
        let port = rtp.local_addr()?.port();
        if port % 2 != 0 {
            continue;
        }
        if let Ok(rtcp) = bind_udp(SocketAddr::new(any.ip(), port + 1)) {
            return Ok((UdpSocket::from_std(rtp)?, UdpSocket::from_std(rtcp)?));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrNotAvailable,
        "no consecutive RTP / RTCP port pair available",
    ))
}

/// Reads a `name=low-high` parameter out of an RTSP `Transport` header, e.g.
/// `server_port=5004-5005`.
fn transport_ports(header: &str, name: &str) -> Option<(u16, u16)> {
    header.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        if !key.trim().eq_ignore_ascii_case(name) {
            return None;
        }
        let (first, second) = value.trim().split_once('-')?;
        Some((first.trim().parse().ok()?, second.trim().parse().ok()?))
    })
}

/// Connection state of the RTSP session as seen by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspSessionState {
    Connected,
    Described,
    Setup,
    Playing,
    Closed,
}

/// How far a session's handshake has got.
///
/// Kept on the client so that a deadline which fires part way through can name
/// the request it was waiting on: a silence at `SETUP` says something quite
/// different from one at `OPTIONS`, and only one of them is worth answering
/// with the other transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspStage {
    Options,
    Describe,
    Setup,
    Play,
}

/// An RTSP session over a single TCP connection using interleaved framing.
#[derive(Debug)]
pub struct RtspClient {
    url: RtspUrl,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// Interleaved frames waiting for the socket, with how far each was written.
    ///
    /// See [`RtspClient::send_interleaved`]: writing them inline would block the
    /// loop that reads the picture.
    outbox: VecDeque<(Vec<u8>, usize)>,
    cseq: u32,
    session: Option<String>,
    /// Silence the server tolerates before it drops the session, as announced by
    /// the `Session` header of `SETUP` / `PLAY`.
    session_timeout: Option<Duration>,
    content_base: Option<String>,
    state: RtspSessionState,
    /// Request of the handshake being waited on, `None` outside one.
    stage: Option<RtspStage>,
    user_agent: String,
    timeout: Duration,
    /// Credentials answering the server challenge. Kept separate from `url`
    /// because a password containing `@` or `:` cannot be carried by an RTSP
    /// URL without percent encoding.
    credentials: Option<(String, String)>,
    /// Challenge advertised by the server, cached so that only the first
    /// request pays for the `401` round trip.
    challenge: Option<Challenge>,
    /// Channel pair the server confirmed for the video track's interleaved
    /// transport, `(rtp, rtcp)`, `None` until `SETUP` answers.
    ///
    /// The pair a server *answers* with is the authoritative one: RFC 2326 lets
    /// it pick its own, and it does not have to be the pair that was asked for.
    /// go2rtc numbers the channels by track index, so a video track that
    /// follows an audio one is framed on `2-3` even when `0-1` was requested -
    /// reading channel `0` there returns the audio's packets and never a
    /// picture, which leaves the session looking stuck on "connecting".
    interleaved: Option<(u8, u8)>,
    /// Channel pair the server confirmed for the audio track, the mirror of
    /// [`RtspClient::interleaved`]. `None` when the track was not set up, in
    /// which case the session carries video alone.
    audio_interleaved: Option<(u8, u8)>,
    /// Channel pair the server confirmed for the talk-back track - the
    /// `a=sendonly` audio media a camera or go2rtc offers when the session asks
    /// for it. `None` when the session did not ask, or the server offered none.
    talk_interleaved: Option<(u8, u8)>,
    /// Whether this session carries `Require: www.onvif.org/ver20/backchannel`
    /// on the requests that follow the `DESCRIBE`.
    ///
    /// A camera that answered the header with a track is sent it again on the
    /// talk-back `SETUP` and on `PLAY`, as ONVIF Profile T expects; one that
    /// refused it - and was asked plainly instead - must not hear it again, or
    /// the `PLAY` is refused too and the picture goes with it.
    backchannel_require: bool,
    /// Address of the RTSP server. With the UDP transport the TCP connection
    /// carries only the control plane, and the media is delivered here.
    peer_addr: Option<SocketAddr>,
    /// RTP / RTCP sockets of the UDP transport, `None` while the interleaved
    /// transport is in use.
    rtp: Option<UdpSocket>,
    rtcp: Option<UdpSocket>,
    rtp_server: Option<SocketAddr>,
    rtcp_server: Option<SocketAddr>,
    /// Receive buffers reused by [`RtspClient::read_media`].
    ///
    /// A datagram larger than the buffer is silently truncated by the kernel:
    /// no error is raised, but the tail of an H.264 fragment is lost and the
    /// picture it belongs to is corrupted. The RTP one is therefore sized for
    /// the largest datagram UDP can carry, which is what a camera sending large
    /// fragments on a high resolution stream relies on.
    rtp_buffer: Box<[u8; 65536]>,
    rtcp_buffer: Box<[u8; 8192]>,
}

impl RtspClient {
    /// Connects to the RTSP server described by `url`.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_auth(url, None).await
    }

    /// Connects to the RTSP server, authenticating with the given credentials.
    ///
    /// Credentials passed here take precedence over any user info embedded in
    /// the URL.
    pub async fn connect_with_auth(url: &str, credentials: Option<(String, String)>) -> Result<Self> {
        Self::connect_full(url, credentials, Duration::from_secs(10)).await
    }

    /// Connects with an explicit TCP connect timeout.
    pub async fn connect_with_timeout(url: &str, timeout: Duration) -> Result<Self> {
        Self::connect_full(url, None, timeout).await
    }

    async fn connect_full(
        url: &str,
        credentials: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<Self> {
        let parsed = RtspUrl::parse(url)?;
        let credentials = credentials.or_else(|| {
            parsed
                .credentials()
                .map(|(user, pass)| (user.to_string(), pass.to_string()))
        });
        let address = format!("{}:{}", parsed.host, parsed.effective_port());
        let stream = tokio::time::timeout(timeout, TcpStream::connect(&address))
            .await
            .map_err(|_| CoreError::rtsp(format!("connect to {address} timed out")))?
            .map_err(|err| CoreError::rtsp(format!("connect to {address} failed: {err}")))?;
        stream.set_nodelay(true).ok();
        let peer_addr = stream.peer_addr().ok();
        let (read_half, write_half) = stream.into_split();
        tracing::debug!(target: "xgview::rtsp", url = %parsed.masked(), "rtsp connection established");
        Ok(Self {
            url: parsed,
            reader: BufReader::with_capacity(64 * 1024, read_half),
            writer: write_half,
            outbox: VecDeque::new(),
            cseq: 0,
            session: None,
            session_timeout: None,
            content_base: None,
            state: RtspSessionState::Connected,
            stage: None,
            user_agent: format!("XGView/{}", env!("CARGO_PKG_VERSION")),
            timeout,
            credentials,
            challenge: None,
            interleaved: None,
            audio_interleaved: None,
            talk_interleaved: None,
            backchannel_require: false,
            peer_addr,
            rtp: None,
            rtcp: None,
            rtp_server: None,
            rtcp_server: None,
            rtp_buffer: Box::new([0u8; 65536]),
            rtcp_buffer: Box::new([0u8; 8192]),
        })
    }

    pub fn url(&self) -> &RtspUrl {
        &self.url
    }

    pub fn state(&self) -> RtspSessionState {
        self.state
    }

    /// Request of the handshake being waited on, `None` outside one.
    pub fn stage(&self) -> Option<RtspStage> {
        self.stage
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }

    fn next_cseq(&mut self) -> u32 {
        self.cseq += 1;
        self.cseq
    }

    /// `Authorization` header value for a request, when a challenge is known.
    ///
    /// Nothing is sent before the server has challenged the session: answering
    /// with preemptive `Basic` would put the password on the wire even against
    /// devices that accept `Digest`.
    ///
    /// The request URI is the one on the request line: RTSP digests are
    /// computed over the absolute `rtsp://…` URI, unlike HTTP.
    fn authorization(&self, method: &str, uri: &str) -> Option<String> {
        let (user, pass) = self.credentials.as_ref()?;
        self.challenge
            .as_ref()?
            .authorization(method, uri, user, pass)
    }

    /// Caches the challenge carried by a `401` response.
    fn update_challenge(&mut self, response: &RtspResponse) -> Option<Challenge> {
        let challenge = parse_challenge(response)?;
        self.challenge = Some(challenge.clone());
        Some(challenge)
    }

    /// Sends a request and returns its CSeq.
    async fn send_request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<u32> {
        let cseq = self.next_cseq();
        let mut request = format!("{method} {uri} RTSP/1.0\r\n");
        request.push_str(&format!("CSeq: {cseq}\r\n"));
        request.push_str(&format!("User-Agent: {}\r\n", self.user_agent));
        if let Some(session) = &self.session {
            request.push_str(&format!("Session: {session}\r\n"));
        }
        if let Some(authorization) = self.authorization(method, uri) {
            request.push_str(&format!("Authorization: {authorization}\r\n"));
        }
        for (key, value) in extra {
            request.push_str(&format!("{key}: {value}\r\n"));
        }
        if let Some(body) = body {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
            request.push_str("Content-Type: application/sdp\r\n");
        }
        request.push_str("\r\n");
        if let Some(body) = body {
            request.push_str(body);
        }

        self.writer
            .write_all(request.as_bytes())
            .await
            .map_err(|err| CoreError::rtsp(format!("write {method} failed: {err}")))?;
        self.writer
            .flush()
            .await
            .map_err(|err| CoreError::rtsp(format!("flush {method} failed: {err}")))?;
        Ok(cseq)
    }

    /// Reads a full RTSP response, skipping any interleaved payload that may
    /// arrive in between (some devices start streaming before `PLAY` answers).
    pub async fn read_response(&mut self) -> Result<RtspResponse> {
        loop {
            match self.next_packet().await? {
                RtspPacket::Response(response) => return Ok(response),
                RtspPacket::Interleaved { .. } => continue,
            }
        }
    }

    /// Reads the next packet from the connection.
    pub async fn next_packet(&mut self) -> Result<RtspPacket> {
        Self::read_packet(&mut self.reader, &mut self.state).await
    }

    /// Reads one packet from an interleaved stream, framed.
    ///
    /// An associated function rather than a method so that the UDP path, which
    /// holds borrows of the media sockets, can read the control connection
    /// through the same framing instead of dropping bytes it cannot frame.
    async fn read_packet(
        reader: &mut BufReader<OwnedReadHalf>,
        state: &mut RtspSessionState,
    ) -> Result<RtspPacket> {
        let first = {
            let buffered = reader
                .fill_buf()
                .await
                .map_err(|err| CoreError::rtsp(format!("read failed: {err}")))?;
            if buffered.is_empty() {
                *state = RtspSessionState::Closed;
                return Err(CoreError::rtsp("connection closed by peer"));
            }
            buffered[0]
        };

        if first == b'$' {
            let mut header = [0u8; 4];
            reader
                .read_exact(&mut header)
                .await
                .map_err(|err| CoreError::rtsp(format!("read interleaved header: {err}")))?;
            let channel = header[1];
            let length = u16::from_be_bytes([header[2], header[3]]) as usize;
            if length > MAX_INTERLEAVED_PAYLOAD {
                return Err(CoreError::rtsp(format!("interleaved payload too large: {length}")));
            }
            let mut payload = vec![0u8; length];
            reader
                .read_exact(&mut payload)
                .await
                .map_err(|err| CoreError::rtsp(format!("read interleaved payload: {err}")))?;
            *state = RtspSessionState::Playing;
            return Ok(RtspPacket::Interleaved { channel, payload });
        }

        Ok(RtspPacket::Response(Self::read_response_headers(reader).await?))
    }

    /// Reads one response, body included.
    async fn read_response_headers(reader: &mut BufReader<OwnedReadHalf>) -> Result<RtspResponse> {
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .await
            .map_err(|err| CoreError::rtsp(format!("read status line: {err}")))?;
        let status_line = status_line.trim_end().to_string();
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default().to_string();
        if !version.to_ascii_uppercase().starts_with("RTSP/") {
            return Err(CoreError::rtsp(format!("unexpected status line: {status_line}")));
        }
        let status = parts.next().unwrap_or("0").parse::<u16>().unwrap_or(0);
        let reason = parts.next().unwrap_or_default().to_string();

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .map_err(|err| CoreError::rtsp(format!("read header line: {err}")))?;
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                headers.push((key.trim().to_string(), value.trim().to_string()));
            }
        }

        let content_length = headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader
                .read_exact(&mut body)
                .await
                .map_err(|err| CoreError::rtsp(format!("read body: {err}")))?;
        }

        Ok(RtspResponse { status, reason, headers, body })
    }

    /// Sends a request, reads its response and answers a `401` challenge once.
    ///
    /// Only the first request of a session pays for the extra round trip: the
    /// challenge is cached and reused by every following request.
    async fn request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<RtspResponse> {
        let response = self.send_and_read(method, uri, extra, body).await?;
        if response.status != 401 || self.credentials.is_none() {
            return Ok(response);
        }

        let previous = self.challenge.clone();
        let Some(challenge) = self.update_challenge(&response) else {
            return Ok(response);
        };
        if previous.as_ref() == Some(&challenge) {
            // Already answered this exact challenge, retrying would loop.
            return Ok(response);
        }

        tracing::debug!(
            target: "xgview::rtsp",
            method,
            scheme = %challenge.scheme,
            "retrying the rtsp request with credentials"
        );
        self.send_and_read(method, uri, extra, body).await
    }

    async fn send_and_read(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<RtspResponse> {
        self.send_request(method, uri, extra, body).await?;
        self.read_response().await
    }

    /// Turns a non successful response into an error, with an actionable
    /// message when the device rejected the credentials.
    fn expect_success(&self, response: RtspResponse, method: &str) -> Result<RtspResponse> {
        if response.is_success() {
            return Ok(response);
        }
        // A server may take one transport and refuse the other. go2rtc answers
        // this to every UDP `SETUP` on purpose, so that a client asking for UDP
        // first falls back, and naming it apart from the other failures is what
        // lets the pipeline do exactly that.
        if response.status == 461 {
            return Err(CoreError::transport(format!(
                "{method} refused the transport: {} {}",
                response.status, response.reason
            )));
        }
        if response.status == 401 {
            let scheme = match &self.challenge {
                Some(challenge) => format!("{} authentication", challenge.scheme),
                None => "no WWW-Authenticate challenge".to_string(),
            };
            let hint = match &self.credentials {
                Some((user, _)) => format!("user name {user:?} was rejected"),
                None => "no credentials are configured for this camera".to_string(),
            };
            return Err(CoreError::rtsp(format!(
                "{method} failed: 401 Unauthorized ({scheme}) - {hint}"
            )));
        }
        Err(CoreError::rtsp(format!(
            "{method} failed: {} {}",
            response.status, response.reason
        )))
    }

    /// `OPTIONS`, used both as a capability probe and as a keep alive.
    pub async fn options(&mut self) -> Result<RtspResponse> {
        let uri = self.url.request_uri();
        let response = self.request("OPTIONS", &uri, &[], None).await?;
        self.expect_success(response, "OPTIONS")
    }

    /// Caches the session identifier and the timeout announced by a response.
    fn update_session(&mut self, response: &RtspResponse) {
        let Some(value) = response.header("Session") else { return };
        let (identifier, timeout) = parse_session_header(value);
        if let Some(identifier) = identifier {
            self.session = Some(identifier);
        }
        if timeout.is_some() {
            self.session_timeout = timeout;
        }
    }

    /// Delay between two keep alive requests.
    ///
    /// A server that announces `timeout=60` must hear from the client well
    /// before that, so half of it is used. Devices that announce nothing - or
    /// barely anything - fall back to a conservative default.
    pub fn keep_alive_interval(&self) -> Duration {
        self.session_timeout
            .map(|timeout| timeout / 2)
            .unwrap_or(DEFAULT_KEEP_ALIVE)
            .max(MIN_KEEP_ALIVE)
    }

    /// `OPTIONS` keep alive that does not wait for the answer.
    ///
    /// Calling [`RtspClient::options`] in the middle of a playing session is
    /// what freezes the picture: [`RtspClient::read_response`] throws away every
    /// interleaved payload it meets while it waits for the answer, so a device
    /// that never answers `OPTIONS` stops the packet pump for good. The answer
    /// is not needed, so it is left to [`RtspClient::read_interleaved`], which
    /// already drops the control responses it finds between two RTP packets.
    pub async fn send_keep_alive(&mut self) -> Result<()> {
        let uri = self.url.request_uri();
        self.send_request("OPTIONS", &uri, &[], None).await.map(|_| ())
    }

    /// `DESCRIBE` returning the raw SDP body.
    pub async fn describe(&mut self) -> Result<String> {
        let uri = self.url.request_uri();
        let response = self
            .request("DESCRIBE", &uri, &[("Accept", "application/sdp".to_string())], None)
            .await?;
        if let Some(base) = response.header("Content-Base") {
            self.content_base = Some(base.to_string());
        }
        let response = self.expect_success(response, "DESCRIBE")?;
        self.state = RtspSessionState::Described;
        Ok(response.body_string())
    }

    /// `DESCRIBE` asking for the talk-back media as well.
    ///
    /// A camera that implements ONVIF Profile T adds an `a=sendonly` audio
    /// media when the request carries `Require: www.onvif.org/ver20/backchannel`.
    /// go2rtc ignores the header and answers from the session URL it kept at the
    /// first request, which is where [`RtspClient::start_with_options`] puts
    /// `backchannel=1` - so the query is deliberately **not** repeated here:
    /// a camera that does not know the parameter may treat the path as a
    /// different stream and answer `404`.
    ///
    /// A camera that does not take the header answers `551`, and some refuse it
    /// with `400` or `404`. None of those may cost the picture, so the request is
    /// repeated plainly: no header, no back channel, and the session proceeds
    /// the way it does for any camera.
    pub async fn describe_backchannel(&mut self) -> Result<String> {
        let uri = self.url.request_uri();
        let response = self
            .request(
                "DESCRIBE",
                &uri,
                &[
                    ("Accept", "application/sdp".to_string()),
                    ("Require", BACKCHANNEL_REQUIRE.to_string()),
                ],
                None,
            )
            .await?;
        if matches!(response.status, 400 | 404 | 551) {
            tracing::debug!(
                target: "xgview::rtsp",
                status = response.status,
                "the camera does not take the back-channel header, asking plainly"
            );
            // The session carries no header from here on: repeating it on the
            // `PLAY` would only be refused the same way, and take the picture
            // with it.
            self.backchannel_require = false;
            return self.describe().await;
        }
        let response = self.expect_success(response, "DESCRIBE")?;
        self.backchannel_require = true;
        if let Some(base) = response.header("Content-Base") {
            self.content_base = Some(base.to_string());
        }
        self.state = RtspSessionState::Described;
        Ok(response.body_string())
    }

    /// `SETUP` a track over the TCP interleaved transport, returning the pair the
    /// server confirmed.
    ///
    /// The pair it *answers* with is authoritative: RFC 2326 lets it pick its
    /// own, and it need not be the pair that was asked for. go2rtc numbers the
    /// channels by track index, so a track that follows another is framed on
    /// `2-3` even when `0-1` was requested. Without an `interleaved` parameter
    /// in the answer there is nothing to honour and the requested pair stands.
    async fn setup_interleaved_pair(
        &mut self,
        control: &str,
        rtp_channel: u8,
        require: bool,
    ) -> Result<(u8, u8)> {
        let track_uri = self.resolve_control(control);
        let transport = format!(
            "RTP/AVP/TCP;unicast;interleaved={}-{}",
            rtp_channel,
            rtp_channel + 1
        );
        let mut headers = vec![("Transport", transport)];
        if require {
            headers.push(("Require", BACKCHANNEL_REQUIRE.to_string()));
        }
        let response = self.request("SETUP", &track_uri, &headers, None).await?;
        self.update_session(&response);
        let response = self.expect_success(response, "SETUP")?;
        let (rtp, rtcp) = response
            .header("Transport")
            .and_then(|header| transport_ports(header, "interleaved"))
            .map(|(rtp, rtcp)| (rtp as u8, rtcp as u8))
            .unwrap_or((rtp_channel, rtp_channel + 1));
        self.state = RtspSessionState::Setup;
        Ok((rtp, rtcp))
    }

    /// `SETUP` the video track, storing the confirmed pair.
    ///
    /// The pair returned is the one the server confirmed in the answer, which
    /// is not necessarily the one requested - see [`RtspClient::interleaved`].
    pub async fn setup_interleaved(&mut self, control: &str, rtp_channel: u8) -> Result<u8> {
        let (rtp, rtcp) = self.setup_interleaved_pair(control, rtp_channel, false).await?;
        self.interleaved = Some((rtp, rtcp));
        Ok(rtp)
    }

    /// `SETUP` the audio track on its own interleaved pair, stored apart from
    /// the video's so that [`RtspClient::read_media`] can tell the two apart.
    pub async fn setup_audio_interleaved(&mut self, control: &str, rtp_channel: u8) -> Result<u8> {
        let (rtp, rtcp) = self.setup_interleaved_pair(control, rtp_channel, false).await?;
        self.audio_interleaved = Some((rtp, rtcp));
        Ok(rtp)
    }

    /// `SETUP` the talk-back track on its own interleaved pair.
    ///
    /// The media is `a=sendonly`: the client sends it, so nothing is ever read
    /// from this pair. Its channel is kept apart from the video's and from the
    /// camera's own audio, which is what [`RtspClient::read_media`] routes by.
    pub async fn setup_talk_interleaved(&mut self, control: &str, rtp_channel: u8) -> Result<u8> {
        let (rtp, rtcp) = self
            .setup_interleaved_pair(control, rtp_channel, self.backchannel_require)
            .await?;
        self.talk_interleaved = Some((rtp, rtcp));
        Ok(rtp)
    }

    /// `SETUP` using RTP over UDP.
    ///
    /// A pair of consecutive local ports is bound first - even for RTP, the next
    /// one for RTCP, as RFC 3550 requires - and advertised in `client_port`. The
    /// server answers with the pair it will send from, which is where the
    /// receiver reports go.
    pub async fn setup_udp(&mut self, control: &str) -> Result<()> {
        let ipv6 = self.peer_addr.map(|addr| addr.is_ipv6()).unwrap_or(false);
        let (rtp, rtcp) = bind_rtp_pair(ipv6)
            .map_err(|err| CoreError::rtsp(format!("cannot bind RTP / RTCP ports: {err}")))?;
        let client_port = rtp
            .local_addr()
            .map_err(|err| CoreError::rtsp(format!("cannot read the RTP port: {err}")))?
            .port();
        let track_uri = self.resolve_control(control);
        let transport = format!("RTP/AVP;unicast;client_port={client_port}-{}", client_port + 1);
        let response = self
            .request("SETUP", &track_uri, &[("Transport", transport)], None)
            .await?;
        self.update_session(&response);
        let response = self.expect_success(response, "SETUP")?;
        let header = response
            .header("Transport")
            .ok_or_else(|| CoreError::rtsp("SETUP answered without a Transport header"))?;
        let (server_rtp, server_rtcp) = transport_ports(header, "server_port").ok_or_else(|| {
            CoreError::rtsp(format!("SETUP Transport header carries no server_port: {header}"))
        })?;
        let ip = self
            .peer_addr
            .map(|addr| addr.ip())
            .ok_or_else(|| CoreError::rtsp("the RTSP peer address is unknown"))?;
        self.rtp_server = Some(SocketAddr::new(ip, server_rtp));
        self.rtcp_server = Some(SocketAddr::new(ip, server_rtcp));
        self.rtp = Some(rtp);
        self.rtcp = Some(rtcp);
        self.state = RtspSessionState::Setup;
        tracing::debug!(
            target: "xgview::rtsp",
            client_port,
            server_rtp,
            server_rtcp,
            "rtp over udp session"
        );
        Ok(())
    }

    /// `PLAY`. `range` defaults to `npt=0.000-` (live).
    ///
    /// A session whose `DESCRIBE` carried the back-channel header carries it
    /// here too: ONVIF Profile T asks for it on the presentation as well, and a
    /// camera that answered it once expects it again before it opens the
    /// `sendonly` media. The session that fell back to a plain `DESCRIBE` sends
    /// nothing, because that camera refused the header and would refuse the
    /// whole `PLAY` with it.
    ///
    /// The header is also tied to the media it was asked for: a camera that took
    /// it on the `DESCRIBE` and then refused the talk-back track's `SETUP` is
    /// left in a session with no such media, and repeating the header there
    /// would ask it to open something it has already said it will not - the one
    /// answer that costs the picture rather than the back channel. So the
    /// request carries it only when the track really was set up.
    pub async fn play(&mut self, range: &str) -> Result<()> {
        let uri = self.session_uri();
        let mut headers = vec![("Range", range.to_string())];
        if self.backchannel_require && self.talk_interleaved.is_some() {
            headers.push(("Require", BACKCHANNEL_REQUIRE.to_string()));
        }
        let response = self.request("PLAY", &uri, &headers, None).await?;
        self.update_session(&response);
        self.expect_success(response, "PLAY")?;
        self.state = RtspSessionState::Playing;
        Ok(())
    }

    /// `TEARDOWN`, best effort.
    pub async fn teardown(&mut self) {
        let uri = self.session_uri();
        if self.send_request("TEARDOWN", &uri, &[], None).await.is_ok() {
            let _ = tokio::time::timeout(self.timeout, self.read_response()).await;
        }
        self.state = RtspSessionState::Closed;
    }

    /// URI a request that targets the whole session addresses.
    ///
    /// `PLAY` covers the presentation rather than one track, so it goes to the
    /// base the server announced in `Content-Base`, falling back to the URL that
    /// was asked for when the server announces none. This is not cosmetic: a
    /// FOSCAM sub stream keeps a *second* server on a different port and answers
    /// a `PLAY` sent to the requested URL with a stream that carries only the
    /// per picture `SEI` and never a slice, which decodes to a slideshow.
    fn session_uri(&self) -> String {
        self.content_base
            .clone()
            .unwrap_or_else(|| self.url.request_uri())
    }

    /// Resolves a track control attribute against the session base URI.
    ///
    /// The query is dropped from the base: a track URI never carries one, and a
    /// server that reads it would misread the track. go2rtc is exactly such a
    /// server - its `reqTrackID` looks at the query first when there is one, so
    /// a `SETUP` of `trackID=2` sent to `.../cam?backchannel=1` would arrive as
    /// track 1.
    fn resolve_control(&self, control: &str) -> String {
        if control.starts_with("rtsp://") {
            return control.to_string();
        }
        let base = self
            .content_base
            .clone()
            .unwrap_or_else(|| self.url.request_uri());
        let base = base.split_once('?').map_or(base.as_str(), |(head, _)| head);
        let base = base.trim_end_matches('/');
        if control.is_empty() {
            base.to_string()
        } else if control.starts_with('?') {
            format!("{base}{control}")
        } else {
            format!("{base}/{}", control.trim_start_matches('/'))
        }
    }

    /// Convenience helper: `OPTIONS` + `DESCRIBE` + `SETUP` + `PLAY`.
    ///
    /// Returns the parsed SDP tracks of the session. The video track is always
    /// set up. The camera's audio track is set up too, on its own interleaved
    /// pair - a media the client *receives*, never the `sendonly` back channel -
    /// but a failure there is not fatal: a camera that refuses the second
    /// `SETUP` still gives a picture.
    pub async fn start_with_transport(&mut self, transport: RtspTransport) -> Result<Vec<SdpTrack>> {
        self.start_with_options(transport, false).await
    }

    /// [`RtspClient::start_with_transport`], asking for the talk-back track too
    /// when `backchannel` is set.
    ///
    /// Two servers answer two different ways, and the session asks both in the
    /// way each of them reads: go2rtc publishes the `a=sendonly` media when the
    /// **first request** of the connection carries `backchannel=1`, and a camera
    /// that implements ONVIF Profile T publishes it when the `DESCRIBE` carries
    /// `Require: www.onvif.org/ver20/backchannel` (see
    /// [`RtspClient::describe_backchannel`]). The query is therefore put on the
    /// `OPTIONS` URL and taken off again before the `DESCRIBE`: a camera that
    /// does not know it treats the path as a different stream and answers `404`,
    /// which would otherwise cost the picture on every camera without go2rtc.
    ///
    /// A URL handed in that already carries the flag is cleaned the same way -
    /// the parameter belongs to the probe and to nothing after it, whoever put
    /// it there.
    pub async fn start_with_options(
        &mut self,
        transport: RtspTransport,
        backchannel: bool,
    ) -> Result<Vec<SdpTrack>> {
        self.backchannel_require = false;
        let clean_uri = strip_query_flag(&self.url.request_uri(), "backchannel");
        let probe_added = backchannel && !self.url.request_uri().contains("backchannel=");
        if backchannel {
            let probe_uri = append_query_flag(&clean_uri, "backchannel=1");
            self.url = RtspUrl::parse(&probe_uri)?;
        }
        // Each step is stamped before it is waited on, so that a deadline which
        // fires somewhere in here can say which request was not answered.
        self.stage = Some(RtspStage::Options);
        let probe = self.options().await;
        // The probe is over, whatever it answered: the path the camera knows is
        // the one it was given, and go2rtc has already kept the query.
        self.url = RtspUrl::parse(&clean_uri)?;
        match probe {
            Ok(_) => {}
            // A camera may refuse the parameter on the path itself - a `404`
            // for a stream it does not serve under that name. That is a status,
            // not a dead connection: ask again on the clean URL and let the
            // `DESCRIBE` carry the question. A network failure is the caller's,
            // which reconnects.
            Err(CoreError::Rtsp(err)) if backchannel && probe_added => {
                tracing::debug!(
                    target: "xgview::rtsp",
                    error = %err,
                    "the camera refused the back-channel parameter on the path, asking again without it"
                );
                self.challenge = None;
                self.stage = Some(RtspStage::Options);
                self.options().await?;
            }
            Err(err) => return Err(err),
        }
        self.stage = Some(RtspStage::Describe);
        let sdp = if backchannel {
            self.describe_backchannel().await?
        } else {
            self.describe().await?
        };
        let tracks = parse_sdp(&sdp);
        let video = tracks
            .iter()
            .find(|track| track.is_video())
            .ok_or_else(|| CoreError::rtsp("no video track in session description"))?;
        let control = video.control.clone().unwrap_or_default();
        self.stage = Some(RtspStage::Setup);
        match transport {
            RtspTransport::Tcp => {
                self.setup_interleaved(&control, 0).await?;
            }
            RtspTransport::Udp => {
                self.setup_udp(&control).await?;
            }
        }
        // The audio track the client receives. Over TCP it gets its own
        // interleaved pair - asked for far from the video's, so a server that
        // honours the request cannot collide with it - and a refusal is logged
        // and left behind rather than failing the session, because the picture
        // must survive a camera that will not open a second track. Over UDP the
        // two tracks would share the single bound socket pair, so audio is left
        // out there.
        if let Some(audio) = tracks.iter().find(|track| track.is_audio() && !track.is_sendonly()) {
            let audio_control = audio.control.clone().unwrap_or_default();
            match transport {
                RtspTransport::Tcp => {
                    match self.setup_audio_interleaved(&audio_control, 2).await {
                        Ok(channel) => tracing::debug!(
                            target: "xgview::rtsp",
                            channel,
                            "audio track set up"
                        ),
                        Err(err) => tracing::warn!(
                            target: "xgview::rtsp",
                            error = %err,
                            "audio track was not set up, the session carries video alone"
                        ),
                    }
                }
                RtspTransport::Udp => tracing::debug!(
                    target: "xgview::rtsp",
                    "audio is set up over TCP only, skipping it on the UDP transport"
                ),
            }
        }
        // The back channel, when the session asked for one and the answer
        // carries it. It is asked for far past the two receive pairs so that a
        // server honouring the request cannot land on them.
        if backchannel {
            if let Some(track) = tracks.iter().find(|track| track.is_audio() && track.is_sendonly()) {
                let control = track.control.clone().unwrap_or_default();
                match transport {
                    RtspTransport::Tcp => match self.setup_talk_interleaved(&control, 4).await {
                        Ok(channel) => tracing::debug!(
                            target: "xgview::rtsp",
                            channel,
                            codec = ?track.encoding,
                            "talk-back track set up"
                        ),
                        Err(err) => tracing::warn!(
                            target: "xgview::rtsp",
                            error = %err,
                            "the talk-back track was not set up"
                        ),
                    },
                    RtspTransport::Udp => tracing::debug!(
                        target: "xgview::rtsp",
                        "talk-back is set up over TCP only, skipping it on the UDP transport"
                    ),
                }
            }
        }
        self.stage = Some(RtspStage::Play);
        self.play("npt=0.000-").await?;
        Ok(tracks)
    }

    /// [`RtspClient::start_with_transport`] over the interleaved transport.
    pub async fn start(&mut self) -> Result<Vec<SdpTrack>> {
        self.start_with_transport(RtspTransport::Tcp).await
    }

    /// Writes one interleaved packet.
    ///
    /// Media and control share the connection in this transport, and control is
    /// what the client has to put back on it: a receiver report is the only way
    /// to tell a camera the stream is arriving.
    pub fn send_interleaved(&mut self, channel: u8, payload: &[u8]) {
        let mut frame = Vec::with_capacity(payload.len() + 4);
        frame.push(b'$');
        frame.push(channel);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(payload);
        self.outbox.push_back((frame, 0));
        // Audio is worth less than the picture: a queue the camera is not
        // draining loses its oldest frames rather than growing without bound.
        while self.outbox.len() > OUTBOX_FRAMES {
            self.outbox.pop_front();
        }
        self.flush_outbox();
    }

    /// Writes as much of the queued bytes as the socket takes right now.
    ///
    /// This is the point of the queue: the talk-back's audio is written from the
    /// same loop that reads the picture, and a camera that stops draining its
    /// socket would otherwise stall that read - the picture stops arriving, the
    /// read times out, and the session reconnects. Polling the write instead of
    /// waiting for it keeps the read going; see [`Self::send_interleaved`].
    pub fn flush_outbox(&mut self) {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        while let Some((frame, offset)) = self.outbox.front_mut() {
            match std::pin::Pin::new(&mut self.writer).poll_write(&mut cx, &frame[*offset..]) {
                std::task::Poll::Ready(Ok(0)) => break,
                std::task::Poll::Ready(Ok(written)) => {
                    *offset += written;
                    if *offset >= frame.len() {
                        self.outbox.pop_front();
                    }
                }
                std::task::Poll::Ready(Err(_)) => {
                    self.outbox.clear();
                    break;
                }
                std::task::Poll::Pending => break,
            }
        }
    }

    /// Sends a control packet (a receiver report) on whichever transport is in
    /// use: the RTCP socket on UDP, the `SETUP`-confirmed RTCP interleaved
    /// channel on TCP.
    pub fn send_rtcp(&mut self, payload: &[u8]) {
        match (self.rtcp.as_ref(), self.rtcp_server) {
            (Some(socket), Some(target)) => {
                // Fire and forget, as everywhere else here: a report that did not
                // leave is worth less than the loop it would hold up.
                let _ = socket.try_send_to(payload, target);
            }
            _ => {
                let channel = self.rtcp_channel();
                self.send_interleaved(channel, payload);
            }
        }
    }

    /// Channel a receiver report goes out on over the interleaved transport.
    fn rtcp_channel(&self) -> u8 {
        self.interleaved.map(|(_, rtcp)| rtcp).unwrap_or(1)
    }

    /// Channel media is framed on over the interleaved transport.
    fn rtp_channel(&self) -> u8 {
        self.interleaved.map(|(rtp, _)| rtp).unwrap_or(0)
    }

    /// RTP channel of the audio track, `None` when it was not set up.
    fn audio_rtp_channel(&self) -> Option<u8> {
        self.audio_interleaved.map(|(rtp, _)| rtp)
    }

    /// RTP channel the talk-back track writes on, `None` when the session has
    /// no back channel.
    pub fn talk_rtp_channel(&self) -> Option<u8> {
        self.talk_interleaved.map(|(rtp, _)| rtp)
    }

    /// RTCP channel of the audio track, `None` when it was not set up.
    fn audio_rtcp_channel(&self) -> Option<u8> {
        self.audio_interleaved.map(|(_, rtcp)| rtcp)
    }

    /// Reads the next interleaved payload, ignoring control responses.
    ///
    /// A control response that arrives while media is flowing is noise, never a
    /// failure: it is usually the answer to the [`RtspClient::send_keep_alive`]
    /// request, and plenty of cameras answer an `OPTIONS` sent during a session
    /// with `501` or `400`. Treating those as fatal would reconnect every keep
    /// alive interval, so they are logged and dropped. Only the handshake turns
    /// a status code into an error.
    pub async fn read_interleaved(&mut self) -> Result<(u8, Vec<u8>)> {
        loop {
            match self.next_packet().await? {
                RtspPacket::Interleaved { channel, payload } => return Ok((channel, payload)),
                RtspPacket::Response(response) => {
                    if response.is_success() {
                        tracing::debug!(
                            target: "xgview::rtsp",
                            status = response.status,
                            "discarded a control response"
                        );
                    } else {
                        tracing::warn!(
                            target: "xgview::rtsp",
                            status = response.status,
                            reason = %response.reason,
                            "discarded a failed control response, the session keeps streaming"
                        );
                    }
                }
            }
        }
    }

    /// Reads the next media packet from the transport in use.
    ///
    /// On the interleaved transport this maps the channel onto [`MediaPacket`];
    /// on UDP the RTP socket is read and the RTCP one is drained, because a
    /// sender report that is never read would eventually fill its buffer.
    pub async fn read_media(&mut self) -> Result<MediaPacket> {
        if self.rtp.is_none() {
            let (channel, payload) = self.read_interleaved().await?;
            // Route by the channel the two `SETUP`s confirmed. Anything that is
            // neither the video nor the audio RTP channel is control the caller
            // does not act on: it is handed back as video RTCP, which the caller
            // already discards.
            if channel == self.rtp_channel() {
                return Ok(MediaPacket::Rtp(payload));
            }
            if self.audio_rtp_channel() == Some(channel) {
                return Ok(MediaPacket::AudioRtp(payload));
            }
            if self.audio_rtcp_channel() == Some(channel) {
                return Ok(MediaPacket::AudioRtcp(payload));
            }
            return Ok(MediaPacket::Rtcp(payload));
        }

        let rtp = self.rtp.as_ref().expect("bound by setup_udp");
        let rtcp = self.rtcp.as_ref().expect("bound by setup_udp");
        let buffer = &mut self.rtp_buffer;
        let scratch = &mut self.rtcp_buffer;
        loop {
            tokio::select! {
                result = rtp.recv(&mut buffer[..]) => {
                    let length =
                        result.map_err(|err| CoreError::rtsp(format!("read rtp failed: {err}")))?;
                    if length == buffer.len() {
                        tracing::warn!(
                            target: "xgview::rtsp",
                            length,
                            "an rtp datagram filled the receive buffer, it was truncated"
                        );
                    }
                    return Ok(MediaPacket::Rtp(buffer[..length].to_vec()));
                }
                result = rtcp.recv(&mut scratch[..]) => {
                    result.map_err(|err| CoreError::rtsp(format!("read rtcp failed: {err}")))?;
                }
                result = self.reader.fill_buf() => {
                    // Media travels on UDP, but the TCP connection still carries
                    // the keep alive answers, and on some servers the RTCP they
                    // interleave. One whole packet is read here and dropped so
                    // that the socket does not fill up over an unattended run -
                    // dropped as a packet, not as "whatever the buffer happened
                    // to hold", because the buffer can end in the middle of one,
                    // and everything after a half-read packet is read at the
                    // wrong offset.
                    let length = result
                        .map_err(|err| CoreError::rtsp(format!("read control failed: {err}")))?
                        .len();
                    if length == 0 {
                        return Err(CoreError::rtsp("connection closed by peer"));
                    }
                    self.reader.consume(length);
                }
            }
        }
    }

    /// Waits until the transport has something to read.
    ///
    /// A peek rather than a read, so that a caller which gives up waiting can
    /// drop this future without a packet being half consumed behind it: a read
    /// cancelled part way through one takes the bytes it has already taken with
    /// it, and every packet after them is read at the wrong offset, which ends
    /// the session rather than the wait.
    pub async fn wait_for_media(&mut self) -> Result<()> {
        let (Some(rtp), Some(rtcp)) = (self.rtp.as_ref(), self.rtcp.as_ref()) else {
            return match self.reader.fill_buf().await {
                Ok(buffered) if !buffered.is_empty() => Ok(()),
                Ok(_) => Err(CoreError::rtsp("connection closed by peer")),
                Err(err) => Err(CoreError::rtsp(format!("read failed: {err}"))),
            };
        };
        tokio::select! {
            result = rtp.readable() => {
                result.map_err(|err| CoreError::rtsp(format!("rtp socket failed: {err}")))
            }
            result = rtcp.readable() => {
                result.map_err(|err| CoreError::rtsp(format!("rtcp socket failed: {err}")))
            }
            result = self.reader.fill_buf() => match result {
                Ok(buffered) if !buffered.is_empty() => Ok(()),
                Ok(_) => Err(CoreError::rtsp("connection closed by peer")),
                Err(err) => Err(CoreError::rtsp(format!("read failed: {err}"))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_with_credentials_and_port() {
        let url = RtspUrl::parse("rtsp://admin:pass@10.1.2.3:8554/live/ch0?a=1").unwrap();
        assert_eq!(url.host(), Some("10.1.2.3"));
        assert_eq!(url.port(), Some(8554));
        assert_eq!(url.path(), "/live/ch0");
        assert_eq!(url.credentials(), Some(("admin", "pass")));
        assert_eq!(url.request_uri(), "rtsp://10.1.2.3:8554/live/ch0?a=1");
        assert_eq!(url.masked(), "rtsp://***:***@10.1.2.3:8554/live/ch0?a=1");
    }

    #[test]
    fn parses_url_without_path() {
        let url = RtspUrl::parse("rtsp://192.168.0.10").unwrap();
        assert_eq!(url.effective_port(), 554);
        assert_eq!(url.request_uri(), "rtsp://192.168.0.10/");
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(RtspUrl::parse("http://192.168.0.10/").is_err());
    }

    #[test]
    fn parses_sdp_tracks() {
        let sdp = "v=0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=framesize:96 1920-1080\r\n\
a=framerate:25\r\n\
a=control:trackID=1\r\n";
        let tracks = parse_sdp(sdp);
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].is_video());
        assert_eq!(tracks[0].encoding.as_deref(), Some("H264"));
        assert_eq!(tracks[0].resolution().as_deref(), Some("1920x1080"));
        assert_eq!(tracks[0].control.as_deref(), Some("trackID=1"));
    }

    #[test]
    fn parses_the_audio_track_and_its_direction() {
        // The shape go2rtc serves: the video track, then the camera's own audio
        // (`recvonly`) and the back channel (`sendonly`). Only the first audio
        // is what the client plays; the `sendonly` one is the talk-back media.
        let sdp = "v=0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=control:trackID=0\r\n\
a=recvonly\r\n\
m=audio 0 RTP/AVP 97\r\n\
a=rtpmap:97 mpeg4-generic/16000/1\r\n\
a=fmtp:97 config=1408; mode=AAC-hbr; sizelength=13\r\n\
a=control:trackID=1\r\n\
a=recvonly\r\n\
m=audio 0 RTP/AVP 0 8\r\n\
a=control:trackID=2\r\n\
a=sendonly\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n";
        let tracks = parse_sdp(sdp);
        assert_eq!(tracks.len(), 3);
        assert!(tracks[0].is_video());

        let audio = &tracks[1];
        assert!(audio.is_audio());
        assert!(!audio.is_sendonly());
        assert_eq!(audio.encoding.as_deref(), Some("MPEG4-GENERIC"));
        assert_eq!(audio.clock_rate, Some(16000));
        assert_eq!(audio.channels, Some(1));
        assert!(audio.fmtp.as_deref().unwrap().contains("config=1408"));

        let back = &tracks[2];
        assert!(back.is_audio());
        assert!(back.is_sendonly());
        assert_eq!(back.control.as_deref(), Some("trackID=2"));
    }

    #[test]
    fn names_the_g711_static_payload_types() {
        // Some cameras send no `rtpmap` for PCMU / PCMA at all: RFC 3551 gives
        // them the static payload types 0 and 8.
        let tracks = parse_sdp("m=audio 0 RTP/AVP 0\r\n");
        assert_eq!(tracks[0].encoding.as_deref(), Some("PCMU"));
        assert_eq!(tracks[0].clock_rate, Some(8000));

        let tracks = parse_sdp("m=audio 0 RTP/AVP 8\r\n");
        assert_eq!(tracks[0].encoding.as_deref(), Some("PCMA"));
    }

    #[test]
    fn a_plain_track_defaults_to_sendrecv() {
        let tracks = parse_sdp("m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n");
        assert_eq!(tracks[0].direction, SdpDirection::SendRecv);
        assert!(!tracks[0].is_sendonly());
        assert!(!tracks[0].is_audio());
    }

    #[test]
    fn parses_a_session_header() {
        let (identifier, timeout) = parse_session_header("12345678;timeout=60");
        assert_eq!(identifier.as_deref(), Some("12345678"));
        assert_eq!(timeout, Some(Duration::from_secs(60)));

        let (identifier, timeout) = parse_session_header("ABCD1234");
        assert_eq!(identifier.as_deref(), Some("ABCD1234"));
        assert_eq!(timeout, None);

        // Real devices are inconsistent about spacing and casing.
        let (identifier, timeout) = parse_session_header(" 42 ; Timeout=10 ");
        assert_eq!(identifier.as_deref(), Some("42"));
        assert_eq!(timeout, Some(Duration::from_secs(10)));
    }

    #[test]
    fn parses_parameter_sets_from_fmtp() {
        use base64::Engine;

        let fmtp = "packetization-mode=1;\
sprop-parameter-sets=Z0IAKeKQCgC3YC3AQEBpB4kRUA==,aM48gA==;\
profile-level-id=42e00a";
        let sets = parse_parameter_sets(fmtp);
        assert_eq!(sets.len(), 2);
        // First entry is an SPS (NAL type 7), second a PPS (NAL type 8).
        assert_eq!(sets[0][0] & 0x1f, 7);
        assert_eq!(sets[1][0] & 0x1f, 8);
        assert_eq!(
            sets[0],
            base64::engine::general_purpose::STANDARD
                .decode("Z0IAKeKQCgC3YC3AQEBpB4kRUA==")
                .unwrap()
        );
    }

    #[test]
    fn accepts_a_parameter_set_fmtp_that_has_no_base64_payload() {
        assert!(parse_parameter_sets("packetization-mode=1;profile-level-id=42e00a").is_empty());
        assert!(parse_parameter_sets("").is_empty());
    }

    #[test]
    fn sdp_surfaces_parameter_sets() {
        let sdp = "v=0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z0IAKeKQCgC3YC3AQEBpB4kRUA==,aM48gA==\r\n\
a=control:trackID=1\r\n";
        let tracks = parse_sdp(sdp);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].parameter_sets.len(), 2);
        assert_eq!(tracks[0].parameter_sets[0][0] & 0x1f, 7);
    }

    /// HEVC advertises its three parameter sets in `sprop-vps` / `sprop-sps` /
    /// `sprop-pps`, and any order is accepted: the types are in the NAL headers.
    #[test]
    fn parses_hevc_parameter_sets_from_fmtp() {
        // The fmtp the H.265 camera at 192.168.17.87 sent, its sets listed
        // pps, sps, vps.
        let fmtp = "sprop-pps=RAHApXwMyQ==;\
sprop-sps=QgEBAWAAAAMAkAAAAwAAAwB4oAPAgBEHy5ZnSkIRkb9QwFqAgICCAAADAAIAAAMAPMALvKIAAW42AAB6EgI=;\
sprop-vps=QAEMAf//AWAAAAMAkAAAAwAAAwB4mcCQ";
        let sets = parse_parameter_sets(fmtp);
        assert_eq!(sets.len(), 3, "VPS, SPS and PPS");
        // The type is (byte0 >> 1) & 0x3f in HEVC, not the H.264 byte0 & 0x1f.
        let kinds: Vec<u8> = sets.iter().map(|nal| (nal[0] >> 1) & 0x3f).collect();
        for expected in [32, 33, 34] {
            assert!(kinds.contains(&expected), "missing NAL type {expected} in {kinds:?}");
        }
    }

    fn response(status: u16, headers: &[(&str, &str)]) -> RtspResponse {
        RtspResponse {
            status,
            reason: "Unauthorized".to_string(),
            headers: headers
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn prefers_digest_over_basic_challenge() {
        let response = response(
            401,
            &[
                ("WWW-Authenticate", r#"Basic realm="Camera""#),
                (
                    "WWW-Authenticate",
                    r#"Digest realm="IP Camera", nonce="abc123", qop="auth""#,
                ),
            ],
        );
        let challenge = parse_challenge(&response).unwrap();
        assert_eq!(challenge.scheme, "digest");
        assert_eq!(challenge.nonce, "abc123");
    }

    #[test]
    fn falls_back_to_basic_challenge() {
        let response = response(401, &[("WWW-Authenticate", r#"Basic realm="Camera""#)]);
        assert_eq!(parse_challenge(&response).unwrap().scheme, "basic");
    }

    #[test]
    fn ignores_responses_without_usable_challenge() {
        assert!(parse_challenge(&response(401, &[("Server", "Hikvision")])).is_none());
        assert!(parse_challenge(&response(401, &[("WWW-Authenticate", "Negotiate")])).is_none());
    }

    /// Reads a full RTSP request (request line + headers) from the socket.
    async fn read_request(reader: &mut BufReader<OwnedReadHalf>) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.is_empty() {
                break;
            }
            lines.push(line);
        }
        lines
    }

    async fn write_response(writer: &mut OwnedWriteHalf, text: &str) {
        writer.write_all(text.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    #[tokio::test]
    async fn answers_a_digest_challenge_and_replays_the_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);

            // First attempt: unauthenticated, answered with a challenge.
            let first = read_request(&mut reader).await;
            assert!(first[0].starts_with("DESCRIBE "));
            assert!(!first.iter().any(|line| line.starts_with("Authorization:")));
            write_response(
                &mut write_half,
                "RTSP/1.0 401 Unauthorized\r\n\
                 CSeq: 1\r\n\
                 WWW-Authenticate: Digest realm=\"IP Camera\", nonce=\"abc123\", qop=\"auth\"\r\n\
                 \r\n",
            )
            .await;

            // Replay: must carry the digest answering that challenge.
            let second = read_request(&mut reader).await;
            let authorization = second
                .iter()
                .find(|line| line.starts_with("Authorization:"))
                .cloned()
                .expect("replayed request carries an Authorization header");

            let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=control:trackID=1\r\n";
            write_response(
                &mut write_half,
                &format!(
                    "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n\
                     Content-Length: {}\r\n\r\n{sdp}",
                    sdp.len()
                ),
            )
            .await;
            authorization
        });

        let url = format!("rtsp://127.0.0.1:{port}/live");
        let mut client = RtspClient::connect_with_auth(&url, Some(("admin".into(), "secret".into())))
            .await
            .unwrap();
        let sdp = client.describe().await.unwrap();
        assert!(sdp.contains("m=video"));

        let authorization = server.await.unwrap();
        assert!(authorization.starts_with("Authorization: Digest "), "got {authorization}");
        assert!(authorization.contains(r#"username="admin""#));
        assert!(authorization.contains(r#"nonce="abc123""#));
        // RTSP digests are computed over the absolute request URI.
        assert!(authorization.contains(&format!(r#"uri="{url}""#)));
    }

    #[tokio::test]
    async fn reports_a_rejected_password_instead_of_looping() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            // Answer every attempt with the same challenge: the client must
            // stop after one replay instead of spinning forever.
            for _ in 0..2 {
                let _ = read_request(&mut reader).await;
                write_response(
                    &mut write_half,
                    "RTSP/1.0 401 Unauthorized\r\n\
                     CSeq: 1\r\n\
                     WWW-Authenticate: Digest realm=\"IP Camera\", nonce=\"abc123\", qop=\"auth\"\r\n\
                     \r\n",
                )
                .await;
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/live");
        let mut client = RtspClient::connect_with_auth(&url, Some(("admin".into(), "wrong".into())))
            .await
            .unwrap();
        let error = client.describe().await.unwrap_err().to_string();
        assert!(error.contains("401"), "got {error}");
        assert!(error.contains("digest authentication"), "got {error}");
        assert!(error.contains("admin"), "got {error}");
    }

    #[test]
    fn parses_rtp_header() {
        let packet = [
            0x80, 0xE0, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x64, 0xDE, 0xAD, 0xBE, 0xEF,
        ];
        let header = parse_rtp_header(&packet).unwrap();
        assert_eq!(header.payload_type, 96);
        assert!(header.marker);
        assert_eq!(header.sequence, 10);
        assert_eq!(header.header_len, 12);
    }

    #[test]
    fn reads_transport_ports() {
        let header =
            "RTP/AVP;unicast;client_port=34567-34568;server_port=5004-5005;ssrc=1234ABCD";
        assert_eq!(transport_ports(header, "server_port"), Some((5004, 5005)));
        assert_eq!(transport_ports(header, "client_port"), Some((34567, 34568)));
        // Header parameter names are case insensitive on the wire.
        assert_eq!(transport_ports(header, "SERVER_PORT"), Some((5004, 5005)));
        assert_eq!(transport_ports(header, "mode"), None);
        assert_eq!(transport_ports("RTP/AVP;unicast", "server_port"), None);
    }

    /// A server that takes one transport and refuses the other, the way go2rtc
    /// does on purpose, has to be reported apart from every other failure: the
    /// pipeline has somewhere to go with that answer and nowhere with a
    /// generic one.
    #[tokio::test]
    async fn reports_a_refused_transport_apart_from_other_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 JPEG/90000\r\na=control:trackID=1\r\n";
            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n").await;
                } else if method.starts_with("DESCRIBE") {
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{sdp}",
                            sdp.len()
                        ),
                    )
                    .await;
                } else if method.starts_with("SETUP") {
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 461 Unsupported transport\r\nCSeq: 3\r\n\r\n",
                    )
                    .await;
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 4\r\n\r\n").await;
                }
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/live");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let error = client.start_with_transport(RtspTransport::Udp).await.unwrap_err();
        assert!(matches!(error, CoreError::Transport(_)), "got {error}");
    }

    /// A server may frame the video on interleaved channels of its own choosing.
    /// go2rtc numbers them by track index, so a video track that follows an
    /// audio one is answered with `interleaved=2-3` even though `0-1` was asked
    /// for. The client has to read the channel the server confirmed: reading
    /// channel `0` there returns the audio, and the picture never arrives, which
    /// leaves the channel looking stuck on "connecting".
    #[tokio::test]
    async fn reads_the_video_from_the_channel_the_server_confirmed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Audio first, video second, the way go2rtc describes a proxied camera.
        let sdp = "v=0\r\n\
m=audio 0 RTP/AVP 96\r\n\
a=rtpmap:96 PCMU/8000\r\n\
a=control:trackID=0\r\n\
m=video 0 RTP/AVP 97\r\n\
a=rtpmap:97 H264/90000\r\n\
a=control:trackID=1\r\n";

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n").await;
                } else if method.starts_with("DESCRIBE") {
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{sdp}",
                            sdp.len()
                        ),
                    )
                    .await;
                } else if method.starts_with("SETUP") {
                    // Confirm a pair of its own rather than the requested 0-1.
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 1234\r\n\
                         Transport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n",
                    )
                    .await;
                } else if method.starts_with("PLAY") {
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 1234\r\n\r\n",
                    )
                    .await;
                    // One RTP packet on channel 2, the channel the server named.
                    let payload = [0x80u8, 0x61, 0x00, 0x2a, 0, 0, 0, 1, 0, 0, 0, 1, 0xab, 0xcd];
                    let mut frame = vec![b'$', 2, 0, payload.len() as u8];
                    frame.extend_from_slice(&payload);
                    write_half.write_all(&frame).await.unwrap();
                    write_half.flush().await.unwrap();
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 5\r\n\r\n").await;
                }
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/cam_front_door_2");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let tracks = client.start().await.unwrap();
        assert!(tracks.iter().any(|track| track.is_video()));
        assert_eq!(client.interleaved, Some((2, 3)));
        match client.read_media().await.unwrap() {
            MediaPacket::Rtp(payload) => assert_eq!(payload.len(), 14),
            MediaPacket::Rtcp(_) => panic!("the video arrived on the wrong channel"),
            MediaPacket::AudioRtp(_) | MediaPacket::AudioRtcp(_) => {
                panic!("the audio arrived where the video was expected")
            }
        }
        drop(client);
        server.await.unwrap();
    }

    /// The back-channel query belongs to the request go2rtc reads the session's
    /// URL from - the first one - and nowhere else. A camera that does not know
    /// the parameter treats the path as a different stream and answers `404`
    /// (the `.40` in the field did exactly that), so `OPTIONS` is the one request
    /// that carries it and the `DESCRIBE` asks with the header instead.
    #[tokio::test]
    async fn asks_for_the_back_channel_on_the_first_request_only() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let sdp = "v=0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=control:trackID=1\r\n\
m=audio 0 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=control:trackID=2\r\n\
a=sendonly\r\n";

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    assert_eq!(
                        method,
                        format!("OPTIONS rtsp://127.0.0.1:{port}/cam?backchannel=1 RTSP/1.0")
                    );
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n").await;
                } else if method.starts_with("DESCRIBE") {
                    assert_eq!(method, format!("DESCRIBE rtsp://127.0.0.1:{port}/cam RTSP/1.0"));
                    assert!(request.iter().any(|line| line.contains("ver20/backchannel")));
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{sdp}",
                            sdp.len()
                        ),
                    )
                    .await;
                } else if method.starts_with("SETUP") {
                    let transport = request
                        .iter()
                        .find(|line| line.starts_with("Transport:"))
                        .cloned()
                        .unwrap_or_default();
                    let pair = if transport.contains("interleaved=4-5") { "4-5" } else { "0-1" };
                    // The talk-back track's `SETUP` repeats the header the
                    // `DESCRIBE` was answered with; the video's does not.
                    let require = request.iter().any(|line| line.contains("ver20/backchannel"));
                    assert_eq!(require, pair == "4-5", "SETUP carried the wrong headers: {request:?}");
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 1234\r\n\
                             Transport: RTP/AVP/TCP;unicast;interleaved={pair}\r\n\r\n"
                        ),
                    )
                    .await;
                } else if method.starts_with("PLAY") {
                    // Profile T asks for the header on the presentation too.
                    assert!(request.iter().any(|line| line.contains("ver20/backchannel")));
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 1234\r\n\r\n",
                    )
                    .await;
                    break;
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 5\r\n\r\n").await;
                }
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/cam");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let tracks = client.start_with_options(RtspTransport::Tcp, true).await.unwrap();
        assert!(tracks.iter().any(|track| track.is_audio() && track.is_sendonly()));
        assert_eq!(client.talk_rtp_channel(), Some(4));
        server.await.unwrap();
    }

    /// A camera may take the header on the `DESCRIBE` and still refuse the
    /// `sendonly` track's `SETUP` - it has a back channel it will not open for
    /// this client, or it wants the video set up first. That refusal is not
    /// allowed to cost the picture, so the session carries video alone.
    ///
    /// The `PLAY` is where it could: a camera that has just refused to open the
    /// media would answer a `Require` that asks for it again the same way, and
    /// that refusal takes the whole presentation down. The header is therefore
    /// sent only when the track really was set up.
    #[tokio::test]
    async fn a_play_asks_for_the_back_channel_only_when_the_track_was_set_up() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let sdp = "v=0\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=control:trackID=1\r\n\
m=audio 0 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=control:trackID=2\r\n\
a=sendonly\r\n";

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n").await;
                } else if method.starts_with("DESCRIBE") {
                    assert!(request.iter().any(|line| line.contains("ver20/backchannel")));
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{sdp}",
                            sdp.len()
                        ),
                    )
                    .await;
                } else if method.starts_with("SETUP") {
                    let transport = request
                        .iter()
                        .find(|line| line.starts_with("Transport:"))
                        .cloned()
                        .unwrap_or_default();
                    if transport.contains("interleaved=4-5") {
                        // The talk-back track: offered, asked for, refused.
                        write_response(
                            &mut write_half,
                            "RTSP/1.0 461 Unsupported Transport\r\nCSeq: 3\r\n\r\n",
                        )
                        .await;
                    } else {
                        write_response(
                            &mut write_half,
                            "RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 1234\r\n\
                             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
                        )
                        .await;
                    }
                } else if method.starts_with("PLAY") {
                    assert!(
                        !request.iter().any(|line| line.contains("ver20/backchannel")),
                        "the play asked for a track the setup refused: {request:?}"
                    );
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 5\r\nSession: 1234\r\n\r\n",
                    )
                    .await;
                    break;
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 6\r\n\r\n").await;
                }
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/cam");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let tracks = client.start_with_options(RtspTransport::Tcp, true).await.unwrap();
        assert!(tracks.iter().any(|track| track.is_video()));
        assert_eq!(client.talk_rtp_channel(), None, "the refused track has no channel");
        server.await.unwrap();
    }

    /// A camera that will not take the back-channel header - `551`, or `400` /
    /// `404` - must still give its picture. The `DESCRIBE` is repeated plainly
    /// and the session carries on with no back channel, which is the path the
    /// cameras that do not implement Profile T take.
    #[tokio::test]
    async fn a_camera_that_refuses_the_back_channel_header_still_plays() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:realvideo\r\n";

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut plain_describes = 0;
            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n").await;
                } else if method.starts_with("DESCRIBE") {
                    if request.iter().any(|line| line.contains("ver20/backchannel")) {
                        write_response(
                            &mut write_half,
                            "RTSP/1.0 551 Option not supported\r\nCSeq: 2\r\n\r\n",
                        )
                        .await;
                    } else {
                        plain_describes += 1;
                        write_response(
                            &mut write_half,
                            &format!(
                                "RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Type: application/sdp\r\n\
                                 Content-Length: {}\r\n\r\n{sdp}",
                                sdp.len()
                            ),
                        )
                        .await;
                    }
                } else if method.starts_with("SETUP") {
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 1234\r\n\
                         Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
                    )
                    .await;
                } else if method.starts_with("PLAY") {
                    // The header was refused, so it must not come back here.
                    assert!(!request.iter().any(|line| line.contains("ver20/backchannel")));
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 5\r\nSession: 1234\r\n\r\n",
                    )
                    .await;
                    break;
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 6\r\n\r\n").await;
                }
            }
            plain_describes
        });

        let url = format!("rtsp://127.0.0.1:{port}/videoMain");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let tracks = client.start_with_options(RtspTransport::Tcp, true).await.unwrap();
        assert!(tracks.iter().any(|track| track.is_video()));
        assert!(!tracks.iter().any(|track| track.is_audio() && track.is_sendonly()));
        assert_eq!(client.talk_rtp_channel(), None);
        assert_eq!(server.await.unwrap(), 1);
    }

    /// A caller may hand the flag in already - the channel manager did, before
    /// this was moved into the client - and it must not change where the
    /// parameter ends up: the probe asks once with it, and every request after
    /// it is sent to the clean path. A camera that does not serve the parameter
    /// answers `404` to a `DESCRIBE` that carries it, which is the picture gone.
    #[tokio::test]
    async fn a_url_that_already_carries_the_flag_is_cleaned_before_the_describe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:track1\r\n";

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    assert!(
                        method.contains("/videoMain?backchannel=1"),
                        "the probe must carry the flag: {method}"
                    );
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n").await;
                } else if method.starts_with("DESCRIBE") {
                    assert_eq!(method, format!("DESCRIBE rtsp://127.0.0.1:{port}/videoMain RTSP/1.0"));
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{sdp}",
                            sdp.len()
                        ),
                    )
                    .await;
                } else if method.starts_with("SETUP") {
                    assert_eq!(
                        method,
                        format!("SETUP rtsp://127.0.0.1:{port}/videoMain/track1 RTSP/1.0")
                    );
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 1234\r\n\
                         Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
                    )
                    .await;
                } else if method.starts_with("PLAY") {
                    assert_eq!(method, format!("PLAY rtsp://127.0.0.1:{port}/videoMain RTSP/1.0"));
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 1234\r\n\r\n",
                    )
                    .await;
                    break;
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 5\r\n\r\n").await;
                }
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/videoMain?backchannel=1");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let tracks = client.start_with_options(RtspTransport::Tcp, true).await.unwrap();
        assert!(tracks.iter().any(|track| track.is_video()));
        server.await.unwrap();
    }

    /// A camera may refuse the parameter on the path itself - a `404` for the
    /// `OPTIONS` that carries it. That costs a round trip, not the session: the
    /// probe is repeated on the clean URL, and the header asks the question
    /// instead. Neither refusal may cost the picture.
    #[tokio::test]
    async fn a_camera_that_refuses_the_back_channel_parameter_on_the_path_still_plays() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:realvideo\r\n";

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut clean_options = 0;
            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    if method.contains("backchannel=1") {
                        write_response(
                            &mut write_half,
                            "RTSP/1.0 404 Stream Not Found\r\nCSeq: 1\r\n\r\n",
                        )
                        .await;
                    } else {
                        clean_options += 1;
                        write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n").await;
                    }
                } else if method.starts_with("DESCRIBE") {
                    if request.iter().any(|line| line.contains("ver20/backchannel")) {
                        write_response(
                            &mut write_half,
                            "RTSP/1.0 551 Option not supported\r\nCSeq: 3\r\n\r\n",
                        )
                        .await;
                    } else {
                        write_response(
                            &mut write_half,
                            &format!(
                                "RTSP/1.0 200 OK\r\nCSeq: 4\r\nContent-Type: application/sdp\r\n\
                                 Content-Length: {}\r\n\r\n{sdp}",
                                sdp.len()
                            ),
                        )
                        .await;
                    }
                } else if method.starts_with("SETUP") {
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 5\r\nSession: 1234\r\n\
                         Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
                    )
                    .await;
                } else if method.starts_with("PLAY") {
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 6\r\nSession: 1234\r\n\r\n",
                    )
                    .await;
                    break;
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n").await;
                }
            }
            clean_options
        });

        let url = format!("rtsp://127.0.0.1:{port}/videoMain");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let tracks = client.start_with_options(RtspTransport::Tcp, true).await.unwrap();
        assert!(tracks.iter().any(|track| track.is_video()));
        assert_eq!(client.talk_rtp_channel(), None);
        assert_eq!(server.await.unwrap(), 1);
    }

    /// **The assumption the whole back channel rests on, and the test that
    /// notices when it stops holding.**
    ///
    /// `?backchannel=1` rides on the first request of the connection because
    /// **go2rtc** keeps the session URL from it (`internal/rtsp/rtsp.go`, `c.URL`
    /// on the `OPTIONS`) - an implementation detail, not something RTSP or ONVIF
    /// promises. A go2rtc upgrade that reads the query somewhere else, or only
    /// from the `DESCRIBE`, would leave every restreamed camera without a back
    /// channel and nothing in the viewer to say why. This test connects to a
    /// restream and asks; it is `#[ignore]` because it needs that host, and is
    /// run by hand:
    ///
    /// ```text
    /// XGVIEW_TALK_TEST_URL=rtsp://192.168.17.88:8554/cam_doorbell_1 \
    /// XGVIEW_TALK_TEST_USER=admin XGVIEW_TALK_TEST_PASSWORD=... \
    /// cargo test -p monitor_core --lib the_go2rtc_restream -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs the reference go2rtc on the network"]
    async fn the_go2rtc_restream_still_offers_the_back_channel() {
        let url = std::env::var("XGVIEW_TALK_TEST_URL").expect(
            "set XGVIEW_TALK_TEST_URL to a go2rtc restream, e.g. \
             rtsp://192.168.17.88:8554/cam_doorbell_1",
        );
        let credentials = std::env::var("XGVIEW_TALK_TEST_USER")
            .ok()
            .map(|user| (user, std::env::var("XGVIEW_TALK_TEST_PASSWORD").unwrap_or_default()));
        let mut client = RtspClient::connect_with_auth(&url, credentials)
            .await
            .expect("connecting to the go2rtc restream");
        let tracks = client
            .start_with_options(RtspTransport::Tcp, true)
            .await
            .expect("opening a session with the back channel");
        assert!(
            tracks.iter().any(|track| track.is_audio() && track.is_sendonly()),
            "back-channel probe failed for {url}: the answer carries no sendonly audio \
             media. go2rtc reads `backchannel=1` off the first request of the connection, \
             which is an implementation detail - an upgrade may have moved it. See \
             docs/DevMemo.md section 34."
        );
        println!("back channel offered: {tracks:?}");
        client.teardown().await;
    }

    /// Full UDP handshake against a minimal RTSP server: the client has to bind
    /// a port pair, advertise it in `SETUP`, read the server pair back and then
    /// receive a datagram on the RTP socket.
    #[tokio::test]
    async fn sets_up_rtp_over_udp_and_reads_a_datagram() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=1\r\n";
            let mut client_port = 0u16;

            loop {
                let request = read_request(&mut reader).await;
                if request.is_empty() {
                    break;
                }
                let method = request[0].clone();
                if method.starts_with("OPTIONS") {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n").await;
                } else if method.starts_with("DESCRIBE") {
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\n\
                             Content-Length: {}\r\n\r\n{sdp}",
                            sdp.len()
                        ),
                    )
                    .await;
                } else if method.starts_with("SETUP") {
                    let transport = request
                        .iter()
                        .find(|line| line.starts_with("Transport:"))
                        .cloned()
                        .unwrap_or_default();
                    let value = transport
                        .split_once(':')
                        .map(|(_, value)| value.trim().to_string())
                        .unwrap_or_default();
                    client_port = transport_ports(&value, "client_port")
                        .map(|(rtp, _)| rtp)
                        .unwrap_or(0);
                    write_response(
                        &mut write_half,
                        &format!(
                            "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 1234\r\n\
                             Transport: RTP/AVP;unicast;client_port={client_port}-{};\
                             server_port=5004-5005\r\n\r\n",
                            client_port + 1
                        ),
                    )
                    .await;
                } else if method.starts_with("PLAY") {
                    write_response(
                        &mut write_half,
                        "RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 1234\r\n\r\n",
                    )
                    .await;
                    let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    // Deliberately larger than any MTU: a receive buffer that is
                    // too small truncates it silently, and the assertion below
                    // would then see a shorter packet - which is exactly what
                    // corrupts the picture of a large high resolution stream.
                    let mut packet = vec![0u8; 5000];
                    packet[..12].copy_from_slice(&[
                        0x80, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 0,
                    ]);
                    rtp.send_to(&packet, ("127.0.0.1", client_port)).await.unwrap();
                    // Hold the control connection open long enough for the
                    // client to read the datagram, so closing it cannot win the
                    // race against the RTP socket.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    break;
                } else {
                    write_response(&mut write_half, "RTSP/1.0 200 OK\r\nCSeq: 5\r\n\r\n").await;
                }
            }
        });

        let url = format!("rtsp://127.0.0.1:{port}/live");
        let mut client = RtspClient::connect(&url).await.unwrap();
        let tracks = client.start_with_transport(RtspTransport::Udp).await.unwrap();
        assert!(tracks.iter().any(|track| track.is_video()));

        let packet = tokio::time::timeout(Duration::from_secs(2), client.read_media())
            .await
            .expect("a datagram arrives")
            .unwrap();
        match packet {
            MediaPacket::Rtp(payload) => assert_eq!(payload.len(), 5000),
            other => panic!("expected an rtp packet, got {other:?}"),
        }
        server.await.unwrap();
    }
}
