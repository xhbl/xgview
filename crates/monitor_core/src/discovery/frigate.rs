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
//! name a purpose, and a viewer may record the sub feed to save space.

use std::collections::HashSet;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde_json::{Map, Value};

use crate::config::FrigateConfig;
use crate::error::{CoreError, Result};
use crate::model::{CameraOrigin, CameraSource};
use crate::rtsp::RtspUrl;

use super::go2rtc::{port_of_listen, restream_url, RESTREAM_PORT};
use super::probe::probe_resolution;

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
        let url = format!("{}/api/login", self.config.base_url());
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
        Ok(())
    }

    /// `GET /api/config`, logging in first when an account is configured.
    pub async fn config_json(&mut self) -> Result<Value> {
        if self.config.needs_login() && self.token.is_none() {
            self.login().await?;
        }
        let url = format!("{}/api/config", self.config.base_url());
        let mut request = self.http.get(&url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CoreError::network("Frigate rejected the API token"));
        }
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(CoreError::network(format!("Frigate /api/config answered {status}")));
        }
        serde_json::from_str(&text)
            .map_err(|err| CoreError::parse(format!("Frigate config is not JSON: {err}")))
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
/// smallest one that is *smaller* than it. Nothing is guessed: a device with
/// several equally sized feeds has no sub, and one whose sizes could not be read
/// has none either - an empty sub falls back to the main stream.
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
    let sub = pixels(sizes.get(main).copied().flatten()).and_then(|main_pixels| {
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

async fn probe_all(
    urls: &[String],
    credentials: Option<(String, String)>,
) -> Vec<Option<(u32, u32)>> {
    stream::iter(urls.iter().cloned().map(|url| {
        let credentials = credentials.clone();
        async move { probe_resolution(&url, credentials, PROBE_TIMEOUT).await }
    }))
    .buffered(PROBE_CONCURRENCY)
    .collect()
    .await
}

fn source_for(
    config: &FrigateConfig,
    port: u16,
    name: &str,
    stream: &str,
    credentials: &Option<(String, String)>,
) -> CameraSource {
    let mut source = CameraSource::new(name, restream_url(&config.host, port, stream));
    source.host = config.host.clone();
    source.origin = CameraOrigin::Frigate;
    source.tags = vec!["frigate".to_string()];
    if let Some((username, password)) = credentials {
        source.username = Some(username.clone());
        source.password = Some(password.clone());
    }
    source
}

async fn build_sources(config: &FrigateConfig, parts: FrigateParts) -> FrigateImport {
    let credentials = rtsp_credentials(config, parts.rtsp.clone());
    let port = parts.restream_port;

    let urls: Vec<String> = parts
        .cameras
        .iter()
        .flat_map(|camera| camera.streams.iter().map(|name| restream_url(&config.host, port, name)))
        .collect();
    let sizes = probe_all(&urls, credentials.clone()).await;

    let mut cameras = Vec::new();
    let mut cursor = 0usize;
    for camera in &parts.cameras {
        let count = camera.streams.len();
        let (main, sub) = select_pair(&sizes[cursor..cursor + count]);
        cursor += count;
        let mut source = source_for(config, port, &camera.name, &camera.streams[main], &credentials);
        if let Some(sub) = sub {
            source.rtsp_sub = Some(restream_url(&config.host, port, &camera.streams[sub]));
        }
        cameras.push(source);
    }

    let leftovers = parts
        .leftovers
        .iter()
        .map(|name| source_for(config, port, name, name, &credentials))
        .collect();

    FrigateImport { cameras, leftovers }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn equal_or_unknown_sizes_have_no_sub() {
        assert_eq!(select_pair(&[Some((1920, 1080)), Some((1920, 1080))]), (0, None));
        assert_eq!(select_pair(&[None, None]), (0, None));
        // One size known, the other not: no size to be smaller than.
        assert_eq!(select_pair(&[None, Some((1920, 1080))]), (1, None));
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
}
