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
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::digest::{self, Challenge};
use crate::error::{CoreError, Result};

/// Default RTSP port.
pub const RTSP_DEFAULT_PORT: u16 = 554;

/// Maximum size accepted for a single interleaved RTP payload.
const MAX_INTERLEAVED_PAYLOAD: usize = 1 << 21;

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
    /// Raw SPS / PPS NAL units of the `sprop-parameter-sets` attribute, in the
    /// order the camera listed them. Empty when the device does not advertise
    /// them, in which case they have to be picked up from the stream itself.
    pub parameter_sets: Vec<Vec<u8>>,
}

impl SdpTrack {
    pub fn is_video(&self) -> bool {
        self.media.eq_ignore_ascii_case("video")
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

/// Extracts the SPS / PPS NAL units advertised by an `a=fmtp` line.
///
/// Cameras list them base64 encoded, e.g.
/// `a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z0IAKeKQCgC3YC3AQEBpB4kRUA==,aM48gA==`.
/// Advertising them lets a viewer that joins mid-stream wait for an IDR instead
/// of having to guess the decoder configuration.
pub fn parse_parameter_sets(fmtp: &str) -> Vec<Vec<u8>> {
    use base64::Engine;

    for parameter in fmtp.split(';') {
        let Some((key, value)) = parameter.split_once('=') else { continue };
        if !key.trim().eq_ignore_ascii_case("sprop-parameter-sets") {
            continue;
        }
        return value
            .split(',')
            .filter_map(|entry| base64::engine::general_purpose::STANDARD.decode(entry.trim()).ok())
            .filter(|nal| !nal.is_empty())
            .collect();
    }
    Vec::new()
}

/// Minimal SDP parser extracting the media tracks of a session description.
pub fn parse_sdp(sdp: &str) -> Vec<SdpTrack> {
    use regex::Regex;
    use std::sync::LazyLock;

    static FRAMESIZE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^a=framesize:(\d+)\s+(\d+)-(\d+)").expect("valid framesize regex")
    });
    static RTPMAP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^a=rtpmap:(\d+)\s+([^/]+)/(\d+)").expect("valid rtpmap regex")
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

/// Connection state of the RTSP session as seen by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspSessionState {
    Connected,
    Described,
    Setup,
    Playing,
    Closed,
}

/// An RTSP session over a single TCP connection using interleaved framing.
#[derive(Debug)]
pub struct RtspClient {
    url: RtspUrl,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    cseq: u32,
    session: Option<String>,
    /// Silence the server tolerates before it drops the session, as announced by
    /// the `Session` header of `SETUP` / `PLAY`.
    session_timeout: Option<Duration>,
    content_base: Option<String>,
    state: RtspSessionState,
    user_agent: String,
    timeout: Duration,
    /// Credentials answering the server challenge. Kept separate from `url`
    /// because a password containing `@` or `:` cannot be carried by an RTSP
    /// URL without percent encoding.
    credentials: Option<(String, String)>,
    /// Challenge advertised by the server, cached so that only the first
    /// request pays for the `401` round trip.
    challenge: Option<Challenge>,
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
        let (read_half, write_half) = stream.into_split();
        tracing::debug!(target: "xgview::rtsp", url = %parsed.masked(), "rtsp connection established");
        Ok(Self {
            url: parsed,
            reader: BufReader::with_capacity(64 * 1024, read_half),
            writer: write_half,
            cseq: 0,
            session: None,
            session_timeout: None,
            content_base: None,
            state: RtspSessionState::Connected,
            user_agent: format!("XGView/{}", env!("CARGO_PKG_VERSION")),
            timeout,
            credentials,
            challenge: None,
        })
    }

    pub fn url(&self) -> &RtspUrl {
        &self.url
    }

    pub fn state(&self) -> RtspSessionState {
        self.state
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
        loop {
            let first = {
                let buffered = self
                    .reader
                    .fill_buf()
                    .await
                    .map_err(|err| CoreError::rtsp(format!("read failed: {err}")))?;
                if buffered.is_empty() {
                    self.state = RtspSessionState::Closed;
                    return Err(CoreError::rtsp("connection closed by peer"));
                }
                buffered[0]
            };

            if first == b'$' {
                let mut header = [0u8; 4];
                self.reader
                    .read_exact(&mut header)
                    .await
                    .map_err(|err| CoreError::rtsp(format!("read interleaved header: {err}")))?;
                let channel = header[1];
                let length = u16::from_be_bytes([header[2], header[3]]) as usize;
                if length > MAX_INTERLEAVED_PAYLOAD {
                    return Err(CoreError::rtsp(format!("interleaved payload too large: {length}")));
                }
                let mut payload = vec![0u8; length];
                self.reader
                    .read_exact(&mut payload)
                    .await
                    .map_err(|err| CoreError::rtsp(format!("read interleaved payload: {err}")))?;
                self.state = RtspSessionState::Playing;
                return Ok(RtspPacket::Interleaved { channel, payload });
            }

            return Ok(RtspPacket::Response(self.read_response_headers().await?));
        }
    }

    async fn read_response_headers(&mut self) -> Result<RtspResponse> {
        let mut status_line = String::new();
        self.reader
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
            self.reader
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
            self.reader
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

    /// `SETUP` using the TCP interleaved transport.
    pub async fn setup_interleaved(&mut self, control: &str, rtp_channel: u8) -> Result<u8> {
        let track_uri = self.resolve_control(control);
        let transport = format!(
            "RTP/AVP/TCP;unicast;interleaved={}-{}",
            rtp_channel,
            rtp_channel + 1
        );
        let response = self
            .request("SETUP", &track_uri, &[("Transport", transport)], None)
            .await?;
        self.update_session(&response);
        self.expect_success(response, "SETUP")?;
        self.state = RtspSessionState::Setup;
        Ok(rtp_channel)
    }

    /// `PLAY`. `range` defaults to `npt=0.000-` (live).
    pub async fn play(&mut self, range: &str) -> Result<()> {
        let uri = self.session_uri();
        let response = self
            .request("PLAY", &uri, &[("Range", range.to_string())], None)
            .await?;
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
    fn resolve_control(&self, control: &str) -> String {
        if control.starts_with("rtsp://") {
            return control.to_string();
        }
        let base = self
            .content_base
            .clone()
            .unwrap_or_else(|| self.url.request_uri());
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
    /// Returns the parsed SDP tracks of the session.
    pub async fn start(&mut self) -> Result<Vec<SdpTrack>> {
        self.options().await?;
        let sdp = self.describe().await?;
        let tracks = parse_sdp(&sdp);
        let video = tracks
            .iter()
            .find(|track| track.is_video())
            .ok_or_else(|| CoreError::rtsp("no video track in session description"))?;
        let control = video.control.clone().unwrap_or_default();
        self.setup_interleaved(&control, 0).await?;
        self.play("npt=0.000-").await?;
        Ok(tracks)
    }

    /// Writes one interleaved packet.
    ///
    /// Media and control share the connection in this transport, and control is
    /// what the client has to put back on it: a receiver report is the only way
    /// to tell a camera the stream is arriving.
    pub async fn send_interleaved(&mut self, channel: u8, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 4);
        frame.push(b'$');
        frame.push(channel);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(payload);
        self.writer
            .write_all(&frame)
            .await
            .map_err(|err| CoreError::rtsp(format!("write interleaved failed: {err}")))?;
        self.writer
            .flush()
            .await
            .map_err(|err| CoreError::rtsp(format!("flush interleaved failed: {err}")))?;
        Ok(())
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
}
