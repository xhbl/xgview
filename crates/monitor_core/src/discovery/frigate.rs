//! Frigate NVR import.
//!
//! Read through Frigate's API alone - the built-in go2rtc's management port is
//! normally not published, so nothing here talks to go2rtc directly. `GET
//! /api/config` describes the cameras and the streams behind them:
//!
//! * `cameras.<name>.ffmpeg.inputs[].path` - the feeds of one camera, which may
//!   be Frigate's own restream (`rtsp://127.0.0.1:8554/<stream>`) or the camera
//!   addressed directly;
//! * `go2rtc.streams` - every stream go2rtc serves, name -> sources, with the
//!   camera's credentials masked (`rtsp://*:*@…`);
//! * `go2rtc.rtsp` - the `:8554` restream account, in clear.
//!
//! Nothing reads a camera's own address: the restream URL is rebuilt from the
//! stream name, and which of a camera's streams is the main one is decided by a
//! measured resolution, never by Frigate's `detect` / `record` roles - those
//! name a purpose, and a viewer may record the sub feed to save space. The
//! resolution is measured over RTSP and over the MSE endpoint at once, and only
//! where neither answers does a camera with more than one stream fall back to
//! input order to name its sub (see [`select_pair`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde_json::{Map, Value};

use crate::config::FrigateConfig;
use crate::error::{CoreError, Result};
use crate::model::{CameraOrigin, CameraSource, FrigateOrigin};
use crate::rtsp::RtspUrl;

use super::go2rtc::{port_of_listen, restream_url, RESTREAM_PORT};
use super::probe::{probe_resolution, probe_resolution_mse};

/// How long one stream's resolution may take to read.
///
/// The wait is for the next IDR, so it is bounded by the camera's group of
/// pictures; 2.65 s was the worst of twenty streams measured against a real
/// Frigate, and most answer at once from their SDP.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// How many streams are measured at once.
const PROBE_CONCURRENCY: usize = 8;

/// A camera Frigate's table describes, and the go2rtc streams it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FrigateCamera {
    name: String,
    /// go2rtc stream names the camera's inputs map to, in input order.
    streams: Vec<String>,
}

/// A Frigate configuration, split into what the import uses.
#[derive(Debug, Clone, PartialEq, Default)]
struct FrigateParts {
    cameras: Vec<FrigateCamera>,
    /// Streams `go2rtc.streams` holds that no camera references.
    leftovers: Vec<String>,
    /// The `:8554` account from `go2rtc.rtsp`, when Frigate sets one.
    rtsp: Option<(String, String)>,
    /// The port the restream is served on, resolved from the tab, the input
    /// paths and `go2rtc.rtsp.listen` - see [`parse_config`].
    restream_port: u16,
}

/// What one Frigate import produced, as ready-to-add cameras.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrigateImport {
    /// One camera per entry in Frigate's table, its streams paired.
    pub cameras: Vec<CameraSource>,
    /// go2rtc streams no camera references, each a main-only feed.
    pub leftovers: Vec<CameraSource>,
}

/// One stretch of a camera's recording, as the server lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Where the stretch starts, in seconds since the epoch.
    pub start: i64,
    /// Where it ends.
    pub end: i64,
}

/// How much of a gap two segments may leave and still be one stretch.
///
/// The recorder writes in segments of about ten seconds and consecutive ones
/// leave a fraction of a second between them. Anything larger is a stretch the
/// camera did not record, and a window must not cross one: a clip asked for
/// across a hole comes back shorter with its timestamps running straight
/// through it, so the picture's time and the wall's would part company.
pub const JOIN: i64 = 2;

/// The stretches a list of segments makes, with the recorder's own seams closed.
///
/// A gap larger than [`JOIN`] is a stretch the camera did not record, and it
/// ends one stretch and begins another. Everything else is one continuous run,
/// which is what a timeline draws: one band with two rounded ends rather than a
/// row of dashes with a notch between each.
pub fn stretches(segments: &[Segment]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for segment in segments {
        match out.last_mut() {
            Some(last) if segment.start - last.end <= JOIN => {
                last.end = last.end.max(segment.end);
            }
            _ => out.push(*segment),
        }
    }
    out
}

/// The stretch of recording that begins at `from` and runs to the first hole or
/// to `longest` seconds, whichever comes first.
///
/// A moment in a hole is taken to the beginning of the next stretch instead:
/// nothing comes back for a range with no recording in it.
///
/// It used to begin at the beginning of the segment `from` falls in, so that the
/// clip would open on the keyframe that opens the segment. A clip cannot be cut
/// between two keyframes anyway - the cut lands on the one before it - and the
/// segments are ten seconds long, so that rule made every seek begin up to ten
/// seconds early and play its way to the moment asked for. Measured against the
/// box of 2026-11-01, a cut in the middle of a segment begins 0.8-0.9 s before
/// it, which is the distance to the keyframe and no more.
///
/// What that costs is exactness: the moments the frames are given are counted
/// from `from`, and the picture is really a keyframe or so behind it. On a
/// timeline ten minutes wide that is a pixel or two; on the recording's own
/// clock it is the interval between two keyframes.
pub fn window_at(segments: &[Segment], from: i64, longest: i64) -> Option<(i64, i64)> {
    let first = segments.iter().position(|segment| segment.end > from)?;
    let from = from.max(segments[first].start);
    let mut end = segments[first].end;
    for next in &segments[first + 1..] {
        if next.start - end > JOIN || next.end - from > longest {
            break;
        }
        end = next.end;
    }
    Some((from, end.min(from + longest)))
}

/// One fragment of a recording, as a VOD playlist lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    /// Its name, which is what the playlist's own address takes as a suffix.
    pub name: String,
    /// How long it lasts, in seconds.
    pub duration: f64,
}

/// What Frigate's `/vod` endpoint answers with: a recording cut into fragments
/// that can be fetched one at a time.
///
/// This is the shape Frigate's own web player reads, and the reason a seek
/// there is one small request rather than a new stream. See
/// `docs/frigate-hls-playback.md`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VodPlaylist {
    /// The address the fragments are named relative to, without a trailing
    /// slash. Set by the client; empty in a parsed body, which knows no address.
    pub base: String,
    /// The moment the playlist's own timeline starts, in seconds since the
    /// epoch. Its fragments count from zero, so a picture's wall clock is this
    /// plus its moment in the fragment.
    pub start: i64,
    /// The init segment every fragment is read through - the codec and its
    /// parameter sets - when the playlist names one.
    pub init: Option<String>,
    /// The fragments, in the order they play.
    pub fragments: Vec<Fragment>,
}

impl VodPlaylist {
    /// Where a fragment is.
    pub fn fragment_uri(&self, fragment: &Fragment) -> String {
        format!("{}/{}", self.base.trim_end_matches('/'), fragment.name)
    }

    /// Where the init segment is, when the playlist names one.
    pub fn init_uri(&self) -> Option<String> {
        self.init
            .as_ref()
            .map(|name| format!("{}/{}", self.base.trim_end_matches('/'), name))
    }

    /// The moment a fragment begins, in seconds since the epoch.
    pub fn starts_at(&self, index: usize) -> i64 {
        self.start + self.fragments.iter().take(index).map(|f| f.duration).sum::<f64>() as i64
    }
}

/// Reads a VOD playlist.
///
/// Only what a session needs: the init segment's name and the fragments with
/// their lengths. The rest of the file - the target duration, the media
/// sequence, the end marker - is either a live stream's bookkeeping or said by
/// the list itself.
fn parse_playlist(text: &str) -> VodPlaylist {
    let mut playlist = VodPlaylist::default();
    let mut duration: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            playlist.init = attribute(rest, "URI").map(str::to_string);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = rest.split(',').next().unwrap_or_default().trim().parse().ok();
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        // A name with no length before it is not a fragment a session can place
        // on its timeline; it is still played, from wherever the last one ended.
        playlist.fragments.push(Fragment { name: line.to_string(), duration: duration.take().unwrap_or(0.0) });
    }
    playlist
}

/// One attribute of a `#EXT-X-…` line, which is `NAME="value"` between commas.
fn attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split(',').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim() == name).then(|| value.trim().trim_matches('"'))
    })
}

/// One listed segment, as whole seconds.
///
/// Frigate truncates a segment's start to the second and leaves its end exact,
/// so two consecutive files either leave a fraction of a second between them or
/// overlap by one. Both ends are rounded outwards - the start down, the end up -
/// which makes them touch. That fraction is the recorder changing file, not a
/// stretch the camera did not record, and drawing it would put a seam every ten
/// seconds along a timeline with nothing wrong with it.
fn rounded(start: f64, end: f64) -> Segment {
    Segment { start: start.floor() as i64, end: end.ceil() as i64 }
}

/// The shape `/api/{camera}/recordings` answers with, as far as this needs it.
#[derive(serde::Deserialize)]
struct Listed {
    start_time: f64,
    end_time: f64,
}

/// The logins this process has made, one per box and account.
///
/// A login is the most expensive request in playback: the box hashes the
/// password on the way in, measured at **0.7-1.0 s** against the box of
/// 2026-11-02, and Frigate's answer to it lasts a day. Without this, every
/// session that starts without a token mints another one - entering a
/// recording, and every drag that leaves the range - while the token from the
/// request just before it is thrown away. Measured: entering a recording spent
/// 952 ms of its 1.05 s here.
///
/// Keyed by account as well as address, because two accounts on one box are two
/// tokens with different rights.
static LOGINS: OnceLock<Mutex<HashMap<(String, String), String>>> = OnceLock::new();

fn logins() -> &'static Mutex<HashMap<(String, String), String>> {
    LOGINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The login this process already made for a box and account, if any.
pub(crate) fn cached_login(base: &str, user: &str) -> Option<String> {
    logins().lock().ok()?.get(&(base.to_string(), user.to_string())).cloned()
}

/// Keeps a login for the requests that follow it. See [`LOGINS`].
pub(crate) fn remember_login(base: &str, user: &str, token: &str) {
    if let Ok(mut logins) = logins().lock() {
        logins.insert((base.to_string(), user.to_string()), token.to_string());
    }
}

/// Drops a login the box has stopped accepting.
pub(crate) fn forget_login(base: &str, user: &str) {
    if let Ok(mut logins) = logins().lock() {
        logins.remove(&(base.to_string(), user.to_string()));
    }
}

/// Client for Frigate's API.
#[derive(Debug, Clone)]
pub struct FrigateClient {
    config: FrigateConfig,
    http: reqwest::Client,
    token: Option<String>,
}

impl FrigateClient {
    pub fn new(config: FrigateConfig) -> Result<Self> {
        if !config.is_configured() {
            return Err(CoreError::config("Frigate host must be configured"));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            // 8971 serves a self-signed certificate.
            .danger_accept_invalid_certs(true)
            .build()?;
        Ok(Self { config, http, token: None })
    }

    pub fn config(&self) -> &FrigateConfig {
        &self.config
    }

    /// `POST /api/login`. The JWT comes back as the `frigate_token` cookie and
    /// nowhere else, so the header is what is read.
    pub async fn login(&mut self) -> Result<()> {
        let base = self.config.base_url();
        // A login this process already made is the same login: the token lasts a
        // day, and minting one costs the box most of a second. See [`LOGINS`].
        if let Some(token) = cached_login(&base, &self.config.username) {
            self.token = Some(token);
            return Ok(());
        }
        let url = format!("{}/api/login", base);
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "user": self.config.username,
                "password": self.config.password,
            }))
            .send()
            .await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CoreError::network("Frigate rejected the login"));
        }
        if !status.is_success() {
            return Err(CoreError::network(format!("Frigate /api/login answered {status}")));
        }
        self.token = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|cookie| {
                let (name, rest) = cookie.split_once('=')?;
                (name.trim() == "frigate_token")
                    .then(|| rest.split(';').next().unwrap_or_default().to_string())
            });
        if let Some(token) = self.token.as_deref() {
            remember_login(&base, &self.config.username, token);
        }
        Ok(())
    }

    /// Forgets the login this process holds for this box, so that the next
    /// request asks the box again.
    ///
    /// The import is where a viewer's password is checked, and a token this
    /// process already holds would answer "fine" to a password that is not: it
    /// was minted with the old one, and the box honours it until it expires. See
    /// [`LOGINS`].
    pub fn forget_login(&mut self) {
        forget_login(&self.config.base_url(), &self.config.username);
        self.token = None;
    }

    /// One `GET` from the box, logged in when an account is configured.
    ///
    /// A token this process already holds is tried first, and a `401` with it
    /// means the box has withdrawn the login rather than that the account is
    /// wrong: the token is dropped, one more login is made, and the request goes
    /// out once more. That is the only way a day-long token goes bad, and
    /// without it a stale one would refuse every request until the process
    /// restarted.
    async fn get(&mut self, url: &str) -> Result<reqwest::Response> {
        let response = self.send_get(url).await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        forget_login(&self.config.base_url(), &self.config.username);
        self.token = None;
        let response = self.send_get(url).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CoreError::network("Frigate rejected the API token"));
        }
        Ok(response)
    }

    /// The `GET` itself: log in when there is no token, then send it as a bearer.
    async fn send_get(&mut self, url: &str) -> Result<reqwest::Response> {
        if self.config.needs_login() && self.token.is_none() {
            self.login().await?;
        }
        let mut request = self.http.get(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        Ok(request.send().await?)
    }

    /// `GET /api/config`, logging in first when an account is configured.
    pub async fn config_json(&mut self) -> Result<Value> {
        let url = format!("{}/api/config", self.config.base_url());
        let response = self.get(&url).await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(CoreError::network(format!("Frigate /api/config answered {status}")));
        }
        serde_json::from_str(&text)
            .map_err(|err| CoreError::parse(format!("Frigate config is not JSON: {err}")))
    }

    /// The segments a camera recorded between two moments.
    ///
    /// Both bounds are seconds since the epoch, and the answer is in order. A
    /// segment is what the recorder wrote in one go - about ten seconds of it -
    /// and consecutive ones leave a fraction of a second between them; a gap
    /// larger than [`JOIN`] is a stretch the camera did not record.
    pub async fn segments(
        &mut self,
        camera: &str,
        after: i64,
        before: i64,
    ) -> Result<Vec<Segment>> {
        let url = format!(
            "{}/api/{camera}/recordings?after={after}&before={before}",
            self.config.base_url()
        );
        let response = self.get(&url).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(CoreError::network(format!(
                "Frigate /api/{camera}/recordings answered {status}"
            )));
        }
        let listed: Vec<Listed> = response
            .json()
            .await
            .map_err(|err| CoreError::parse(format!("the segment list is not JSON: {err}")))?;
        Ok(listed
            .into_iter()
            .map(|entry| rounded(entry.start_time, entry.end_time))
            .collect())
    }

    /// The fragments a camera's recording is served in between two moments.
    ///
    /// Frigate's VOD endpoint answers with an HLS playlist: one init segment and
    /// the fragments the range is cut into, ten to twenty seconds each. It is
    /// what its own web player reads, and it is cheaper than `clip.mp4` because
    /// a fragment can be fetched on its own - a seek is one request for one
    /// fragment rather than a new stream and a new decoder.
    ///
    /// A range that begins at a segment's beginning is answered from the
    /// segment list; one that begins inside a segment is cut first. Measured
    /// against the box of 2026-11-01: 82-90 ms against 270-380 ms.
    pub async fn vod_playlist(&mut self, camera: &str, from: i64, to: i64) -> Result<VodPlaylist> {
        let base = format!("{}/vod/{camera}/start/{from}/end/{to}", self.config.base_url());
        let url = format!("{base}/index.m3u8");
        let response = self.get(&url).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(CoreError::network(format!("Frigate /vod/{camera} answered {status}")));
        }
        let text = response.text().await.unwrap_or_default();
        let mut playlist = parse_playlist(&text);
        if playlist.fragments.is_empty() {
            // The range was read and holds nothing: the same answer a `400` from
            // `clip.mp4` gives, and no more worth asking again.
            return Err(CoreError::empty("no recording in that range"));
        }
        playlist.base = base;
        playlist.start = from;
        Ok(playlist)
    }

    /// Fetches, groups, measures and builds the two groups of cameras.
    pub async fn import_cameras(&mut self) -> Result<FrigateImport> {
        let config = self.config_json().await?;
        let parts = parse_config(&config, &self.config.host, self.config.rtsp_port);
        Ok(build_sources(&self.config, parts).await)
    }
}

/// Splits a `/api/config` body into the cameras, the leftover streams, the
/// restream account and the port the restream is served on.
fn parse_config(config: &Value, host: &str, override_port: Option<u16>) -> FrigateParts {
    let go2rtc = config.get("go2rtc");
    let streams = go2rtc
        .and_then(|go2rtc| go2rtc.get("streams"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let rtsp = go2rtc.and_then(|go2rtc| go2rtc.get("rtsp")).and_then(|rtsp| {
        let user = rtsp.get("username").and_then(Value::as_str).unwrap_or_default();
        let password = rtsp.get("password").and_then(Value::as_str).unwrap_or_default();
        (!user.is_empty()).then(|| (user.to_string(), password.to_string()))
    });
    let table = config.get("cameras").and_then(Value::as_object);

    // The restream port: the one entered on the tab, else the one a loopback
    // input names (inside Frigate's container the restream is always loopback),
    // else `go2rtc.rtsp.listen`, else 8554.
    let path_port = table.and_then(|table| {
        table.values().flat_map(input_paths).find_map(|path| loopback_port(&path))
    });
    let listen_port = go2rtc
        .and_then(|go2rtc| go2rtc.get("rtsp"))
        .and_then(|rtsp| rtsp.get("listen"))
        .and_then(port_of_listen);
    let restream_port = override_port.or(path_port).or(listen_port).unwrap_or(RESTREAM_PORT);

    let mut used: HashSet<String> = HashSet::new();
    let mut cameras = Vec::new();
    if let Some(table) = table {
        for (name, camera) in table {
            let mut streams_of_camera: Vec<String> = Vec::new();
            for path in input_paths(camera) {
                if let Some(stream) = stream_for_path(&path, host, restream_port, &streams) {
                    if !streams_of_camera.contains(&stream) {
                        streams_of_camera.push(stream);
                    }
                }
            }
            used.extend(streams_of_camera.iter().cloned());
            if !streams_of_camera.is_empty() {
                cameras.push(FrigateCamera { name: name.clone(), streams: streams_of_camera });
            }
        }
    }

    let leftovers = streams
        .keys()
        .filter(|name| !used.contains(name.as_str()))
        .cloned()
        .collect();

    FrigateParts { cameras, leftovers, rtsp, restream_port }
}

/// The `ffmpeg.inputs[].path` of one camera, in order.
fn input_paths(camera: &Value) -> Vec<String> {
    camera
        .get("ffmpeg")
        .and_then(|ffmpeg| ffmpeg.get("inputs"))
        .and_then(Value::as_array)
        .map(|inputs| {
            inputs
                .iter()
                .filter_map(|input| input.get("path").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The go2rtc stream an `ffmpeg` input path names, if any.
fn stream_for_path(path: &str, host: &str, port: u16, streams: &Map<String, Value>) -> Option<String> {
    // Frigate's own restream: the loopback the container uses - any port, since
    // a camera is never at the loopback - or the restream port on an address
    // Frigate itself is reached at.
    if let Ok(uri) = RtspUrl::parse(path) {
        let path_host = uri.host().unwrap_or_default();
        let is_frigate_go2rtc = is_loopback(path_host)
            || (path_host.eq_ignore_ascii_case(host) && uri.port() == Some(port));
        if is_frigate_go2rtc {
            let name = uri.path().trim_matches('/');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // The camera addressed directly: it is the source of whichever go2rtc
    // stream lists the same address. Credentials are masked in both, so they are
    // dropped before the comparison.
    let wanted = credentialless(path);
    streams.iter().find_map(|(name, sources)| {
        sources
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .any(|source| credentialless(source) == wanted)
            .then(|| name.clone())
    })
}

/// `127.0.0.1` / `localhost`, where a camera is never addressed but Frigate's
/// own go2rtc always is.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("127.0.0.1") || host.eq_ignore_ascii_case("localhost")
}

/// The restream port a loopback input path names, if it is one.
fn loopback_port(path: &str) -> Option<u16> {
    let uri = RtspUrl::parse(path).ok()?;
    is_loopback(uri.host().unwrap_or_default()).then(|| uri.port()).flatten()
}

/// A URL with its scheme and its credentials removed, for comparing a stream's
/// source with an input path.
fn credentialless(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let rest = rest.rsplit_once('@').map(|(_, rest)| rest).unwrap_or(rest);
    rest.trim_end_matches('/').to_ascii_lowercase()
}

/// Main and sub indices into one camera's streams, from their measured sizes.
///
/// The main stream is the largest one whose size could be read, and the sub the
/// smallest one that is *smaller* than it. When that leaves no sub - the sizes
/// came back equal, or none could be read at all - a camera with more than one
/// stream still gets one, by input order: Frigate lists the main feed first, so
/// the first stream that is not the main becomes the sub. That last part is the
/// one guess here, kept as the fallback for when the reliable fact (the
/// picture size) is missing - a closed port, say - rather than leaving the
/// camera without a sub stream.
fn select_pair(sizes: &[Option<(u32, u32)>]) -> (usize, Option<usize>) {
    let pixels = |size: Option<(u32, u32)>| size.map(|(w, h)| u64::from(w) * u64::from(h));
    // The largest size wins; among equal sizes the earlier input is the main,
    // so a camera with several feeds of one size does not depend on how the
    // device happened to order them.
    let main = sizes
        .iter()
        .enumerate()
        .filter_map(|(index, size)| pixels(*size).map(|pixels| (pixels, index)))
        .max_by_key(|(pixels, index)| (*pixels, std::cmp::Reverse(*index)))
        .map(|(_, index)| index)
        .unwrap_or(0);
    let measured = pixels(sizes.get(main).copied().flatten()).and_then(|main_pixels| {
        sizes
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != main)
            .filter_map(|(index, size)| {
                pixels(*size)
                    .filter(|pixels| *pixels < main_pixels)
                    .map(|pixels| (pixels, index))
            })
            .min_by_key(|(pixels, _)| *pixels)
            .map(|(_, index)| index)
    });
    let sub = measured.or_else(|| {
        (sizes.len() >= 2)
            .then(|| (0..sizes.len()).find(|index| *index != main))
            .flatten()
    });
    (main, sub)
}

/// The account XGView pulls the restream with: the one entered on the tab, or
/// the one read from `go2rtc.rtsp` when the field was left empty.
fn rtsp_credentials(config: &FrigateConfig, read: Option<(String, String)>) -> Option<(String, String)> {
    if config.rtsp_username.trim().is_empty() {
        read
    } else {
        Some((config.rtsp_username.clone(), config.rtsp_password.clone()))
    }
}

/// Measures one target's resolution over both its addresses at once.
///
/// The protocol only decides what a camera is *played* with, not how it is
/// measured, so the probe is the same for `auto`, `rtsp` and `mse`: RTSP and
/// MSE are asked together and whichever reads a size first answers. A closed
/// RTSP port therefore costs the measurement nothing - which is the case MSE is
/// for.
async fn probe_target(
    rtsp_url: String,
    mse_url: String,
    rtsp_credentials: Option<(String, String)>,
    ws_credentials: Option<(String, String)>,
) -> Option<(u32, u32)> {
    let rtsp = probe_resolution(&rtsp_url, rtsp_credentials, PROBE_TIMEOUT);
    let mse = probe_resolution_mse(&mse_url, ws_credentials, PROBE_TIMEOUT);
    tokio::pin!(rtsp);
    tokio::pin!(mse);
    // The first to read a size wins; the other is waited on only when the first
    // could not.
    tokio::select! {
        size = &mut rtsp => match size {
            Some(size) => Some(size),
            None => mse.await,
        },
        size = &mut mse => match size {
            Some(size) => Some(size),
            None => rtsp.await,
        },
    }
}

async fn probe_all(
    targets: &[(String, String)],
    rtsp_credentials: Option<(String, String)>,
    ws_credentials: Option<(String, String)>,
) -> Vec<Option<(u32, u32)>> {
    stream::iter(targets.iter().cloned().map(|(rtsp_url, mse_url)| {
        let rtsp_credentials = rtsp_credentials.clone();
        let ws_credentials = ws_credentials.clone();
        async move { probe_target(rtsp_url, mse_url, rtsp_credentials, ws_credentials).await }
    }))
    .buffered(PROBE_CONCURRENCY)
    .collect()
    .await
}

/// The login Frigate's WebSocket endpoint is pulled with, when the tab names
/// one - Frigate's own login, not the restream account.
fn ws_credentials(config: &FrigateConfig) -> Option<(String, String)> {
    (!config.username.trim().is_empty())
        .then(|| (config.username.clone(), config.password.clone()))
}

fn source_for(
    config: &FrigateConfig,
    port: u16,
    name: &str,
    stream: &str,
    credentials: &Option<(String, String)>,
) -> CameraSource {
    let mut source = CameraSource::new(name, rtsp_url(config, port, stream));
    source.host = config.host.clone();
    source.origin = CameraOrigin::Frigate;
    // Which camera of which server, kept apart from every address: the
    // recordings live behind Frigate's API, not behind the restream this
    // entry pulls its picture from.
    source.frigate = Some(FrigateOrigin {
        base_url: config.base_url(),
        camera: name.to_string(),
    });
    source.imported = true;
    source.tags = vec!["frigate".to_string()];
    // Both addresses are captured, always: which one is reachable depends on the
    // network, so the camera holds the pair and the connection switch decides.
    source.mse_main = Some(mse_url(config, stream));
    source.connect = config.protocol.connection_mode();
    // The restream account, for the RTSP address.
    if let Some((username, password)) = credentials {
        source.username = Some(username.clone());
        source.password = Some(password.clone());
    }
    // Frigate's own login, for the WebSocket stream: it goes through Frigate's
    // own port and uses its session auth, not the restream account.
    if let Some((username, password)) = ws_credentials(config) {
        source.mse_username = Some(username);
        source.mse_password = Some(password);
    }
    source
}

/// One of the streams no camera references: a feed on its own.
///
/// It is not a camera, so it is not given a camera's recordings either. The name
/// it carries is go2rtc's, and Frigate would answer `404` to a camera of that
/// name - which is a playback button that opens nothing.
fn leftover_for(
    config: &FrigateConfig,
    port: u16,
    name: &str,
    credentials: &Option<(String, String)>,
) -> CameraSource {
    let mut source = source_for(config, port, name, name, credentials);
    source.frigate = None;
    source
}

/// The RTSP restream URL of one stream - the address the viewer prefers.
pub fn rtsp_url(config: &FrigateConfig, restream_port: u16, stream: &str) -> String {
    restream_url(&config.host, restream_port, stream)
}

/// The MSE (go2rtc WebSocket) URL of one stream.
///
/// Frigate fronts go2rtc's MSE endpoint at `/live/mse/api/ws` on the port its
/// web UI answers on, behind its own session auth - so this URL needs no
/// restream account, only Frigate's login.
pub fn mse_url(config: &FrigateConfig, stream: &str) -> String {
    let scheme = if config.https { "wss" } else { "ws" };
    format!(
        "{scheme}://{}:{}/live/mse/api/ws?src={stream}",
        config.host, config.port
    )
}

/// The stream name embedded in a restream or WebSocket URL, so the URL can be
/// rewritten when the protocol is switched without re-fetching the server.
/// `rtsp://host:port/<stream>` gives `<stream>`; `wss://…?src=<stream>` gives
/// `<stream>` too. `None` when the URL is neither shape.
pub fn extract_stream(url: &str) -> Option<String> {
    if url.starts_with("rtsp://") || url.starts_with("rtsps://") {
        let after_scheme = &url[url.find("://")? + 3..];
        let path = after_scheme.split_once('/')?.1;
        return Some(path.trim_end_matches('/').to_string());
    }
    if url.starts_with("wss://") || url.starts_with("ws://") {
        let src = url.split("src=").nth(1)?;
        return Some(src.to_string());
    }
    None
}

/// The restream port in an RTSP URL, so it can be preserved when switching back
/// from WebSocket to RTSP. `None` for a WebSocket URL or a malformed one.
pub fn extract_rtsp_port(url: &str) -> Option<u16> {
    if !url.starts_with("rtsp://") && !url.starts_with("rtsps://") {
        return None;
    }
    let after_scheme = &url[url.find("://")? + 3..];
    let authority = after_scheme.split('/').next()?;
    let host_part = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host_part.rsplit_once(':').and_then(|(_, port)| port.parse().ok())
}

async fn build_sources(config: &FrigateConfig, parts: FrigateParts) -> FrigateImport {
    let credentials = rtsp_credentials(config, parts.rtsp.clone());
    let port = parts.restream_port;
    // Both addresses of every stream are measured at once, so a camera whose
    // RTSP port is closed outside the LAN - the case MSE is for - is still
    // paired by the size its MSE stream reports.
    let targets: Vec<(String, String)> = parts
        .cameras
        .iter()
        .flat_map(|camera| {
            camera
                .streams
                .iter()
                .map(|name| (restream_url(&config.host, port, name), mse_url(config, name)))
        })
        .collect();
    let sizes = probe_all(&targets, credentials.clone(), ws_credentials(config)).await;

    let mut cameras = Vec::new();
    let mut cursor = 0usize;
    for camera in &parts.cameras {
        let count = camera.streams.len();
        let (main, sub) = select_pair(&sizes[cursor..cursor + count]);
        cursor += count;
        let mut source = source_for(config, port, &camera.name, &camera.streams[main], &credentials);
        if let Some(sub) = sub {
            let name = &camera.streams[sub];
            source.rtsp_sub = Some(rtsp_url(config, port, name));
            source.mse_sub = Some(mse_url(config, name));
        }
        cameras.push(source);
    }

    let leftovers = parts
        .leftovers
        .iter()
        .map(|name| leftover_for(config, port, name, &credentials))
        .collect();

    FrigateImport { cameras, leftovers }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StreamProtocol;
    use crate::model::ConnectionMode;

    /// A login is kept for the requests that follow it, and dropped when the box
    /// stops accepting it. Two accounts on one box are two logins.
    #[test]
    fn a_login_is_kept_until_it_is_forgotten() {
        let base = "https://cached.invalid:8971";
        assert_eq!(cached_login(base, "someone"), None, "nothing is kept before a login");
        remember_login(base, "someone", "first");
        assert_eq!(cached_login(base, "someone").as_deref(), Some("first"));
        assert_eq!(cached_login(base, "another"), None, "an account is not another's token");
        assert_eq!(cached_login("https://elsewhere.invalid:8971", "someone"), None);
        forget_login(base, "someone");
        assert_eq!(cached_login(base, "someone"), None, "a withdrawn login is gone");
    }

    fn frigate_config() -> FrigateConfig {
        FrigateConfig { host: "192.168.17.88".into(), ..Default::default() }
    }

    /// A camera whose inputs go through Frigate's restream, one whose inputs are
    /// the camera itself (matched against `go2rtc.streams` by the masked
    /// address), and a stream no camera references.
    #[test]
    fn groups_the_cameras_and_leaves_the_rest() {
        let config: Value = serde_json::from_str(
            r#"{
              "cameras": {
                "cam_front_door": {
                  "ffmpeg": { "inputs": [
                    { "path": "rtsp://127.0.0.1:8554/cam_front_door_1" },
                    { "path": "rtsp://127.0.0.1:8554/cam_front_door_2" }
                  ] }
                },
                "cam_direct": {
                  "ffmpeg": { "inputs": [
                    { "path": "rtsp://*:*@192.168.27.46:88/videoSub" }
                  ] }
                }
              },
              "go2rtc": {
                "rtsp": { "username": "admin", "password": "_sxtmmadmin1" },
                "streams": {
                  "cam_front_door_1": ["rtsp://*:*@192.168.27.40:88/videoMain"],
                  "cam_front_door_2": ["rtsp://*:*@192.168.27.40:88/videoSub"],
                  "cam_direct_1": ["rtsp://*:*@192.168.27.46:88/videoMain"],
                  "cam_direct_2": ["rtsp://*:*@192.168.27.46:88/videoSub"],
                  "cam_garage_1": ["rtsp://*:*@192.168.27.48:88/videoMain"]
                }
              }
            }"#,
        )
        .unwrap();

        let parts = parse_config(&config, "192.168.17.88", None);

        assert_eq!(parts.cameras.len(), 2);
        let door = parts.cameras.iter().find(|camera| camera.name == "cam_front_door").unwrap();
        assert_eq!(door.streams, vec!["cam_front_door_1", "cam_front_door_2"]);
        // The direct input is matched to the stream that lists the same address.
        let direct = parts.cameras.iter().find(|camera| camera.name == "cam_direct").unwrap();
        assert_eq!(direct.streams, vec!["cam_direct_2"]);
        // `cam_direct_1` is referenced by nobody (no input names videoMain).
        assert!(parts.leftovers.contains(&"cam_direct_1".to_string()));
        assert!(parts.leftovers.contains(&"cam_garage_1".to_string()));
        assert_eq!(parts.rtsp, Some(("admin".to_string(), "_sxtmmadmin1".to_string())));
        // The loopback input names the restream port.
        assert_eq!(parts.restream_port, 8554);
    }

    /// The restream port is not assumed to be 8554: a loopback input names it,
    /// `go2rtc.rtsp.listen` names it, and only then does it default.
    #[test]
    fn the_restream_port_is_read_then_defaulted() {
        let loopback: Value = serde_json::from_str(
            r#"{
              "cameras": { "cam_a": { "ffmpeg": { "inputs": [
                { "path": "rtsp://127.0.0.1:9000/cam_a_1" }
              ] } } },
              "go2rtc": { "streams": { "cam_a_1": ["rtsp://*:*@192.168.27.40:88/videoMain"] } }
            }"#,
        )
        .unwrap();
        let parts = parse_config(&loopback, "192.168.17.88", None);
        assert_eq!(parts.restream_port, 9000);
        assert_eq!(parts.cameras[0].streams, vec!["cam_a_1"]);

        // No loopback path: `rtsp.listen` is where the port comes from, and it
        // is what lets a host-form input path be recognised at all.
        let listen: Value = serde_json::from_str(
            r#"{
              "cameras": { "cam_a": { "ffmpeg": { "inputs": [
                { "path": "rtsp://192.168.17.88:9000/cam_a_1" }
              ] } } },
              "go2rtc": {
                "rtsp": { "listen": ":9000" },
                "streams": { "cam_a_1": ["rtsp://*:*@192.168.27.40:88/videoMain"] }
              }
            }"#,
        )
        .unwrap();
        let parts = parse_config(&listen, "192.168.17.88", None);
        assert_eq!(parts.restream_port, 9000);
        assert_eq!(parts.cameras[0].streams, vec!["cam_a_1"]);

        // Nothing names a port: 8554.
        let bare: Value = serde_json::from_str(r#"{"cameras":{},"go2rtc":{"streams":{}}}"#).unwrap();
        assert_eq!(parse_config(&bare, "192.168.17.88", None).restream_port, 8554);
    }

    #[test]
    fn the_entered_port_overrides_the_one_read() {
        let config: Value = serde_json::from_str(
            r#"{
              "cameras": { "cam_a": { "ffmpeg": { "inputs": [
                { "path": "rtsp://127.0.0.1:9000/cam_a_1" }
              ] } } },
              "go2rtc": { "streams": { "cam_a_1": ["rtsp://*:*@192.168.27.40:88/videoMain"] } }
            }"#,
        )
        .unwrap();

        let parts = parse_config(&config, "192.168.17.88", Some(8554));
        assert_eq!(parts.restream_port, 8554);
    }

    #[test]
    fn the_largest_stream_is_the_main_and_the_smallest_is_the_sub() {
        let sizes = [Some((640, 480)), Some((1920, 1080))];
        // Order does not matter: the sizes decide.
        assert_eq!(select_pair(&sizes), (1, Some(0)));
    }

    /// When the sizes cannot name a sub - they are equal, or none was read at
    /// all - a camera with more than one stream still gets one by input order:
    /// the first stream that is not the main.
    #[test]
    fn a_pair_with_no_measured_sub_falls_back_to_input_order() {
        // Equal sizes: nothing is smaller than the main.
        assert_eq!(select_pair(&[Some((1920, 1080)), Some((1920, 1080))]), (0, Some(1)));
        // Nothing read at all.
        assert_eq!(select_pair(&[None, None]), (0, Some(1)));
        // One size known: the readable input is the main and the other the sub.
        assert_eq!(select_pair(&[None, Some((1920, 1080))]), (1, Some(0)));
    }

    #[test]
    fn a_single_stream_is_the_main_with_no_sub() {
        assert_eq!(select_pair(&[Some((1920, 1080))]), (0, None));
    }

    #[test]
    fn the_restream_credentials_come_from_the_config_or_the_tab() {
        // Nothing entered: what Frigate serves is used.
        let config = frigate_config();
        assert_eq!(
            rtsp_credentials(&config, Some(("a".into(), "b".into()))),
            Some(("a".into(), "b".into()))
        );
        // Entered: it overrides what Frigate serves.
        let config = FrigateConfig {
            rtsp_username: "me".into(),
            rtsp_password: "pw".into(),
            ..frigate_config()
        };
        assert_eq!(
            rtsp_credentials(&config, Some(("a".into(), "b".into()))),
            Some(("me".into(), "pw".into()))
        );
    }

    /// Both addresses are captured: the RTSP restream and the MSE endpoint,
    /// whichever protocol the import was told to prefer.
    #[test]
    fn both_addresses_are_captured() {
        let config = frigate_config();
        let source = source_for(&config, 8554, "cam", "cam_1", &None);
        assert_eq!(source.rtsp_main, "rtsp://192.168.17.88:8554/cam_1");
        assert_eq!(
            source.mse_main.as_deref(),
            Some("wss://192.168.17.88:8971/live/mse/api/ws?src=cam_1")
        );
        // The default import prefers RTSP.
        assert_eq!(source.connect, ConnectionMode::Auto);
        assert!(source.username.is_none());
        // An imported camera is marked as the import's to manage.
        assert!(source.imported);
    }

    /// The login the WebSocket endpoint is probed and played with is Frigate's
    /// own, and only when the tab names one.
    #[test]
    fn the_ws_login_comes_from_the_frigate_account() {
        assert!(ws_credentials(&frigate_config()).is_none());
        let config = FrigateConfig {
            username: "frigate-user".into(),
            password: "frigate-pass".into(),
            ..frigate_config()
        };
        assert_eq!(
            ws_credentials(&config),
            Some(("frigate-user".to_string(), "frigate-pass".to_string()))
        );
    }

    /// The import preference is written onto each camera's connection mode.
    #[test]
    fn the_protocol_sets_the_connection_mode() {
        for (protocol, expected) in [
            (StreamProtocol::Auto, ConnectionMode::Auto),
            (StreamProtocol::Rtsp, ConnectionMode::Rtsp),
            (StreamProtocol::Mse, ConnectionMode::Mse),
        ] {
            let config = FrigateConfig { protocol, ..frigate_config() };
            let source = source_for(&config, 8554, "cam", "cam_1", &None);
            assert_eq!(source.connect, expected);
        }
    }

    /// The two protocols carry their own credentials: the restream account on
    /// the RTSP address, Frigate's own login on the MSE one.
    #[test]
    fn each_address_carries_its_own_credentials() {
        let config = FrigateConfig {
            username: "frigate-user".into(),
            password: "frigate-pass".into(),
            ..frigate_config()
        };
        let source = source_for(&config, 8554, "cam", "cam_1", &Some(("admin".into(), "pw".into())));
        assert_eq!(source.username.as_deref(), Some("admin"));
        assert_eq!(source.password.as_deref(), Some("pw"));
        assert_eq!(source.mse_username.as_deref(), Some("frigate-user"));
        assert_eq!(source.mse_password.as_deref(), Some("frigate-pass"));
    }

    /// No Frigate login set: the MSE address is still captured, just without a
    /// credential of its own.
    #[test]
    fn the_mse_address_captures_without_a_login() {
        let config = frigate_config();
        let source = source_for(&config, 8554, "cam", "cam_1", &None);
        assert!(source.mse_main.is_some());
        assert!(source.mse_username.is_none());
        assert!(source.mse_credentials().is_none());
    }

    #[test]
    fn the_mse_url_scheme_follows_https() {
        let config = FrigateConfig { https: false, port: 5000, ..frigate_config() };
        assert_eq!(
            mse_url(&config, "cam_1"),
            "ws://192.168.17.88:5000/live/mse/api/ws?src=cam_1"
        );
    }

    #[test]
    fn extract_stream_from_rtsp_url() {
        assert_eq!(
            extract_stream("rtsp://192.168.17.88:8554/cam_front_door_1"),
            Some("cam_front_door_1".into())
        );
        assert_eq!(
            extract_stream("rtsp://192.168.17.88:8554/cam_front_door_1/"),
            Some("cam_front_door_1".into())
        );
    }

    #[test]
    fn extract_stream_from_wss_url() {
        assert_eq!(
            extract_stream("wss://192.168.17.88:8971/live/mse/api/ws?src=cam_front_door_1"),
            Some("cam_front_door_1".into())
        );
        assert_eq!(
            extract_stream("ws://192.168.17.88:5000/live/mse/api/ws?src=cam_1"),
            Some("cam_1".into())
        );
    }

    #[test]
    fn extract_stream_rejects_unknown_schemes() {
        assert_eq!(extract_stream("http://example.com/stream"), None);
        assert_eq!(extract_stream("not a url"), None);
    }

    #[test]
    fn extract_rtsp_port_from_url() {
        assert_eq!(extract_rtsp_port("rtsp://192.168.17.88:8554/cam_1"), Some(8554));
        assert_eq!(extract_rtsp_port("rtsp://192.168.17.88:9000/cam_1"), Some(9000));
        assert_eq!(extract_rtsp_port("wss://192.168.17.88:8971/live/mse/api/ws?src=cam_1"), None);
    }

    /// The recorder's own file boundaries are fractions of a second - it
    /// truncates a start to the second and leaves the end exact - and rounded
    /// outwards they close, so a continuous recording is one band and not a row
    /// of dashes.
    #[test]
    fn a_segment_is_rounded_outwards_so_the_recorder_s_own_seams_close() {
        // The three pairs the box actually answers with, to the millisecond.
        let a = rounded(1793580453.000, 1793580462.704);
        let b = rounded(1793580462.000, 1793580471.682);
        let c = rounded(1793580472.000, 1793580481.702);
        assert_eq!(a, Segment { start: 1793580453, end: 1793580463 });
        assert_eq!(b, Segment { start: 1793580462, end: 1793580472 });
        assert_eq!(c, Segment { start: 1793580472, end: 1793580482 });
        // Touching or overlapping, never a gap.
        assert!(b.start <= a.end);
        assert!(c.start <= b.end);
    }

    /// Segments that touch are one stretch; a hole larger than the recorder's
    /// own seam ends it and begins another.
    #[test]
    fn stretches_close_the_seams_and_stop_at_a_hole() {
        let segments = [
            Segment { start: 100, end: 110 },
            // Overlapping by one, which the box does as often as it leaves a gap.
            Segment { start: 109, end: 120 },
            // A second between files: still one stretch.
            Segment { start: 121, end: 130 },
            // Ten: the camera recorded nothing, and a new stretch begins.
            Segment { start: 140, end: 150 },
        ];
        assert_eq!(
            stretches(&segments),
            vec![Segment { start: 100, end: 130 }, Segment { start: 140, end: 150 }]
        );
        assert!(stretches(&[]).is_empty());
    }

    /// A playlist is the init segment and the fragments, in order, with their
    /// lengths - and the box of 2026-11-01 answered with exactly this one, CRLF
    /// and all.
    #[test]
    fn a_playlist_lists_its_init_segment_and_fragments() {
        let text = "#EXTM3U\r\n\
#EXT-X-TARGETDURATION:20\r\n\
#EXT-X-ALLOW-CACHE:YES\r\n\
#EXT-X-PLAYLIST-TYPE:VOD\r\n\
#EXT-X-VERSION:6\r\n\
#EXT-X-MEDIA-SEQUENCE:1\r\n\
#EXT-X-MAP:URI=\"init-v1-a1.mp4\"\r\n\
#EXTINF:20.304,\r\n\
seg-1-v1-a1.m4s\r\n\
#EXTINF:9.713,\r\n\
seg-3-v1-a1.m4s\r\n\
#EXTINF:19.410,\r\n\
seg-4-v1-a1.m4s\r\n\
#EXT-X-ENDLIST\r\n";

        let playlist = parse_playlist(text);
        assert_eq!(playlist.init.as_deref(), Some("init-v1-a1.mp4"));
        let names: Vec<&str> = playlist.fragments.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["seg-1-v1-a1.m4s", "seg-3-v1-a1.m4s", "seg-4-v1-a1.m4s"]);
        assert!((playlist.fragments[0].duration - 20.304).abs() < 1e-9);
        assert!((playlist.fragments[2].duration - 19.410).abs() < 1e-9);
    }

    /// A playlist that names no init segment, and one that is nothing but
    /// comments: a session reads what is there and asks the same of both.
    #[test]
    fn a_playlist_without_an_init_segment_says_so() {
        let playlist = parse_playlist("#EXTM3U\n#EXT-X-ENDLIST\n");
        assert_eq!(playlist.init, None);
        assert!(playlist.fragments.is_empty());
        assert_eq!(parse_playlist(""), VodPlaylist::default());
    }

    /// Each fragment begins where the ones before it ended, counted from the
    /// playlist's own start, and its address is the playlist's own.
    #[test]
    fn fragments_are_placed_and_addressed_from_the_playlist() {
        let text = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:20.304,\n\
seg-1.m4s\n\
#EXTINF:9.713,\n\
seg-3.m4s\n";
        let mut playlist = parse_playlist(text);
        playlist.base = "https://box:8971/vod/cam/start/100/end/200".to_string();
        playlist.start = 100;

        assert_eq!(
            playlist.fragment_uri(&playlist.fragments[1]),
            "https://box:8971/vod/cam/start/100/end/200/seg-3.m4s"
        );
        assert_eq!(
            playlist.init_uri().as_deref(),
            Some("https://box:8971/vod/cam/start/100/end/200/init.mp4")
        );
        assert_eq!(playlist.starts_at(0), 100);
        assert_eq!(playlist.starts_at(1), 120, "twenty seconds and a fraction later");
    }

    /// A window runs from the moment asked for, through every segment that
    /// touches the one before it, and stops at the first hole - a clip asked for
    /// across one comes back short with its timestamps running straight through
    /// it.
    #[test]
    fn a_window_begins_where_it_was_asked_for_and_stops_at_a_hole() {
        let segments = [
            Segment { start: 100, end: 110 },
            Segment { start: 110, end: 120 },
            // A second's gap: the recorder changed file, the stretch did not.
            Segment { start: 121, end: 130 },
            // Ten seconds: the camera recorded nothing, and the window ends.
            Segment { start: 140, end: 150 },
        ];
        assert_eq!(window_at(&segments, 105, 900), Some((105, 130)));
        assert_eq!(window_at(&segments, 100, 900), Some((100, 130)), "a segment's own beginning is itself");

        // A moment in a hole is taken to the beginning of the next stretch: a
        // window asked for there would come back with nothing in it.
        assert_eq!(window_at(&segments, 135, 900), Some((140, 150)));
        assert_eq!(window_at(&segments, 90, 900), Some((100, 130)), "before the recording");
    }

    /// The window is capped, and the cap is measured from its own start.
    #[test]
    fn a_window_is_capped() {
        let segments: Vec<Segment> = (0..10)
            .map(|i| Segment { start: 100 + i * 10, end: 110 + i * 10 })
            .collect();
        assert_eq!(window_at(&segments, 100, 35), Some((100, 130)));
        assert_eq!(window_at(&segments, 105, 35), Some((105, 140)));
    }

    /// A moment no segment covers has no window at all.
    #[test]
    fn a_moment_outside_the_recording_has_no_window() {
        let segments = [Segment { start: 100, end: 110 }];
        assert_eq!(window_at(&segments, 200, 900), None);
        assert_eq!(window_at(&[], 100, 900), None);
    }

    /// A stream that no camera references is not a camera, and must not claim
    /// to be one: the name it carries is go2rtc's, and a recording asked for
    /// under that name does not exist.
    #[test]
    fn a_stream_no_camera_references_is_not_a_camera() {
        let leftover = leftover_for(&frigate_config(), 8554, "cam_garage_1", &None);
        assert!(leftover.frigate.is_none());
        // It is still an entry, with the address it was imported with.
        assert_eq!(leftover.rtsp_main, "rtsp://192.168.17.88:8554/cam_garage_1");

        // A camera of the same server is one.
        let camera = source_for(&frigate_config(), 8554, "cam_living_room", "cam_living_room_1", &None);
        assert_eq!(
            camera.frigate.map(|origin| origin.camera),
            Some("cam_living_room".to_string())
        );
    }

    /// An imported camera remembers which camera of which server it is, and it
    /// is the API's address that is remembered - not the restream's, which is a
    /// different port on the same box and carries no recordings.
    #[test]
    fn an_imported_camera_remembers_the_camera_and_the_server() {
        let mut config = frigate_config();
        config.port = 8971;
        config.https = true;

        let camera = source_for(&config, 8554, "cam_front_door", "cam_front_door_1", &None);

        let origin = camera.frigate.expect("an imported camera knows its own");
        assert_eq!(origin.base_url, "https://192.168.17.88:8971");
        assert_eq!(origin.camera, "cam_front_door");
        // The two addresses are kept apart: the picture comes from the
        // restream, the recordings from the API.
        assert!(camera.rtsp_main.contains(":8554/"), "{}", camera.rtsp_main);
    }
}
