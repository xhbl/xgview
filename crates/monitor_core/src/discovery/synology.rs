use std::collections::HashMap;

use serde_json::Value;

use crate::config::SynologyConfig;
use crate::error::{CoreError, Result};
use crate::model::{CameraOrigin, CameraSource, RtspTransport, TileAspect};
use crate::rtsp::RtspUrl;

/// A camera as reported by `SYNO.SurveillanceStation.Camera` `List`.
#[derive(Debug, Clone, PartialEq)]
pub struct SynologyCamera {
    pub id: i64,
    pub name: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub enabled: bool,
    pub main_url: Option<String>,
    pub sub_url: Option<String>,
}

/// Client for the Synology DSM web API (auth + Surveillance Station).
#[derive(Debug, Clone)]
pub struct SynologyClient {
    config: SynologyConfig,
    http: reqwest::Client,
    session: Option<String>,
}

impl SynologyClient {
    /// Creates a client from the persisted configuration.
    pub fn new(config: SynologyConfig) -> Result<Self> {
        if !config.is_configured() {
            return Err(CoreError::config("Synology host and account must be configured"));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .danger_accept_invalid_certs(true)
            .build()?;
        Ok(Self { config, http, session: None })
    }

    pub fn config(&self) -> &SynologyConfig {
        &self.config
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// `SYNO.API.Auth` `login`, requesting a session bound to Surveillance
    /// Station.
    pub async fn login(&mut self) -> Result<()> {
        let account = self
            .config
            .account
            .clone()
            .unwrap_or_else(|| self.config.username.clone());
        let url = format!("{}/webapi/auth.cgi", self.config.base_url());
        let response = self
            .http
            .get(&url)
            .query(&[
                ("api", "SYNO.API.Auth"),
                ("version", "6"),
                ("method", "login"),
                ("account", account.as_str()),
                ("passwd", self.config.password.as_str()),
                ("session", "SurveillanceStation"),
                ("format", "sid"),
            ])
            .send()
            .await?;

        let payload: Value = response.json().await?;
        let data = unwrap_success(&payload, "SYNO.API.Auth.login")?;
        let sid = data
            .get("sid")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::parse("Synology login returned no session id"))?;
        self.session = Some(sid.to_string());
        tracing::info!(target: "xgview::synology", host = %self.config.host, "Synology session established");
        Ok(())
    }

    /// Ensures a session exists before calling a Surveillance Station API.
    async fn ensure_session(&mut self) -> Result<()> {
        if self.session.is_none() {
            self.login().await?;
        }
        Ok(())
    }

    /// `SYNO.API.Auth` `logout`, best effort.
    pub async fn logout(&mut self) {
        let Some(session) = self.session.take() else { return };
        let url = format!("{}/webapi/auth.cgi", self.config.base_url());
        let result = self
            .http
            .get(&url)
            .query(&[
                ("api", "SYNO.API.Auth"),
                ("version", "6"),
                ("method", "logout"),
                ("session", "SurveillanceStation"),
                ("_sid", session.as_str()),
            ])
            .send()
            .await;
        if let Err(err) = result {
            tracing::debug!(target: "xgview::synology", %err, "Synology logout failed");
        }
    }

    async fn call(&self, api: &str, method: &str, version: &str, params: &[(&str, &str)]) -> Result<Value> {
        let session = self
            .session
            .as_deref()
            .ok_or_else(|| CoreError::config("Synology session is not established"))?;
        let url = format!("{}/webapi/entry.cgi", self.config.base_url());

        let mut query: Vec<(&str, &str)> = vec![
            ("api", api),
            ("method", method),
            ("version", version),
            ("_sid", session),
        ];
        query.extend_from_slice(params);

        let response = self.http.get(&url).query(&query).send().await?;
        let payload: Value = response.json().await?;
        unwrap_success(&payload, api).map(Clone::clone)
    }

    /// Lists every camera bound to Surveillance Station.
    pub async fn list_cameras(&mut self) -> Result<Vec<SynologyCamera>> {
        self.ensure_session().await?;
        let data = self
            .call(
                "SYNO.SurveillanceStation.Camera",
                "List",
                "9",
                &[
                    ("basic", "true"),
                    ("streamInfo", "true"),
                    ("additional", r#"["streamInfo"]"#),
                ],
            )
            .await?;

        let cameras = data
            .get("cameras")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::parse("Surveillance Station returned no camera list"))?;

        Ok(cameras.iter().map(|camera| self.parse_camera(camera)).collect())
    }

    /// `SYNO.SurveillanceStation.Camera.GetLiveViewPath` for a batch of cameras.
    ///
    /// The RTSP URL Synology hands out is dynamic: it carries a `syno` user and
    /// a short lived stream key as the password, so it cannot be built by hand.
    async fn get_live_view_paths(&mut self, ids: &[i64]) -> Result<HashMap<i64, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.ensure_session().await?;
        let id_list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let data = self
            .call(
                "SYNO.SurveillanceStation.Camera",
                "GetLiveViewPath",
                "9",
                &[("idList", id_list.as_str())],
            )
            .await?;

        let mut paths = HashMap::new();
        if let Some(entries) = data.as_array() {
            for entry in entries {
                let id = entry.get("id").and_then(Value::as_i64);
                let path = entry.get("rtspPath").and_then(Value::as_str);
                if let (Some(id), Some(path)) = (id, path) {
                    paths.insert(id, path.to_string());
                }
            }
        }
        Ok(paths)
    }

    fn parse_camera(&self, value: &Value) -> SynologyCamera {
        let id = value.get("id").and_then(Value::as_i64).unwrap_or_default();
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("camera {id}"));
        let host = value
            .get("ip")
            .or_else(|| value.get("host"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.is_empty());
        let port = value
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok());
        let status = value.get("status").and_then(Value::as_i64).unwrap_or(1);
        let stream_info = value.get("streamInfo").map(normalize_json);

        let main_url = stream_info
            .as_ref()
            .and_then(|info| pick_stream_url(info, &["main", "mainStream", "liveview1", "stream1"]));
        let sub_url = stream_info
            .as_ref()
            .and_then(|info| pick_stream_url(info, &["sub", "subStream", "liveview2", "stream2"]));

        SynologyCamera {
            id,
            name,
            host,
            port,
            vendor: value.get("vendor").and_then(Value::as_str).map(str::to_string),
            model: value.get("model").and_then(Value::as_str).map(str::to_string),
            enabled: status != 0,
            main_url,
            sub_url,
        }
    }

    /// Imports the NAS cameras as [`CameraSource`] entries, converting the
    /// Surveillance Station stream URLs into RTSP URLs.
    pub async fn import_cameras(&mut self) -> Result<Vec<CameraSource>> {
        let cameras = self.list_cameras().await?;
        let ids: Vec<i64> = cameras.iter().map(|camera| camera.id).collect();
        let live_paths = self.get_live_view_paths(&ids).await?;
        Ok(cameras
            .iter()
            .map(|camera| self.to_source(camera, live_paths.get(&camera.id).map(String::as_str)))
            .collect())
    }

    fn to_source(&self, camera: &SynologyCamera, live_path: Option<&str>) -> CameraSource {
        // The live view path Synology hands out is preferred: it is the only URL
        // that actually streams, and it carries the `syno`/stream-key credentials.
        let (rtsp_main, username, password) = match live_path {
            Some(path) => split_rtsp_path(path),
            None => {
                let url = camera
                    .main_url
                    .clone()
                    .unwrap_or_else(|| self.fallback_stream_url(camera.id, 0));
                (
                    self.absolute_stream_url(&url),
                    Some(self.config.username.clone()),
                    Some(self.config.password.clone()),
                )
            }
        };
        // The sub stream is the NAS's MJPEG endpoint rather than an RTSP URL.
        // Synology transcodes it down to a low frame rate, which is what makes
        // it worth having: a tile of a grid only needs enough of a picture to
        // recognise a scene, and the full rate stream stays on the main channel.
        // It is authorised by the same stream key the live view path carries as
        // its password, so it can only be built once that path is known.
        let sub = password
            .as_deref()
            .map(|key| self.mjpeg_url(camera.id, key))
            .or_else(|| camera.sub_url.clone().map(|url| self.absolute_stream_url(&url)));

        CameraSource {
            id: format!("syno-{}", camera.id),
            name: camera.name.clone(),
            vendor: camera.vendor.clone(),
            model: camera.model.clone(),
            host: camera.host.clone().unwrap_or_else(|| self.config.host.clone()),
            onvif_port: 80,
            rtsp_main,
            rtsp_sub: sub,
            username,
            password,
            enabled: camera.enabled,
            tags: vec!["synology".to_string()],
            origin: CameraOrigin::Synology,
            transport: RtspTransport::default(),
            aspect: TileAspect::default(),
            main_profile: None,
            sub_profile: None,
        }
    }

    /// Makes a stream URL returned by the NAS absolute.
    fn absolute_stream_url(&self, url: &str) -> String {
        if url.starts_with("rtsp://") {
            url.to_string()
        } else if url.starts_with('/') {
            format!("rtsp://{}:{}{}", self.config.host, 554, url)
        } else {
            format!(
                "rtsp://{}:{}/{}",
                self.config.host,
                554,
                url.trim_start_matches('/')
            )
        }
    }

    /// Surveillance Station camera proxy URL used when the API did not return
    /// an explicit stream URL. `stream_type` 0 = main, 1 = sub.
    fn fallback_stream_url(&self, camera_id: i64, stream_type: u8) -> String {
        format!(
            "rtsp://{}:554/SurveillanceStation/camera.cgi?id={camera_id}&streamType={stream_type}",
            self.config.host
        )
    }

    /// The MJPEG endpoint of a Surveillance Station camera.
    ///
    /// `SYNO.SurveillanceStation.Stream.VideoStreaming` with `format=mjpeg`
    /// serves the low resolution stream as `multipart/x-mixed-replace`, which is
    /// what a grid tile is pulled from. The stream key authorises it exactly as
    /// it authorises the RTSP path.
    fn mjpeg_url(&self, camera_id: i64, stream_key: &str) -> String {
        format!(
            "{}/webapi/entry.cgi?api=SYNO.SurveillanceStation.Stream.VideoStreaming\
             &version=1&method=Stream&format=mjpeg&cameraId={camera_id}&StmKey={stream_key}",
            self.config.base_url()
        )
    }
}

/// Splits a Synology live view path into a credential free URL plus the
/// `syno`/stream-key credentials it embeds.
fn split_rtsp_path(path: &str) -> (String, Option<String>, Option<String>) {
    match RtspUrl::parse(path) {
        Ok(uri) => {
            let (username, password) = match uri.credentials() {
                Some((user, pass)) => (Some(user.to_string()), Some(pass.to_string())),
                None => (None, None),
            };
            (uri.request_uri(), username, password)
        }
        Err(_) => (path.to_string(), None, None),
    }
}

/// `streamInfo` is sometimes returned as a JSON encoded string.
fn normalize_json(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// Looks for a stream URL inside a `streamInfo` object using several known
/// key spellings.
fn pick_stream_url(info: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(entry) = info.get(*key) {
            if let Some(url) = entry.get("url").and_then(Value::as_str) {
                if !url.is_empty() {
                    return Some(url.to_string());
                }
            }
            if let Some(url) = entry.as_str() {
                if !url.is_empty() {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

/// Validates the `success` flag of a DSM response.
fn unwrap_success<'a>(payload: &'a Value, api: &str) -> Result<&'a Value> {
    let success = payload.get("success").and_then(Value::as_bool).unwrap_or(false);
    if !success {
        let code = payload
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        return Err(CoreError::network(format!("{api} failed with DSM error code {code}")));
    }
    payload
        .get("data")
        .ok_or_else(|| CoreError::parse(format!("{api} returned no data")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SynologyConfig {
        SynologyConfig {
            host: "nas.local".into(),
            username: "admin".into(),
            password: "secret".into(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_fallback_urls() {
        let client = SynologyClient::new(config()).unwrap();
        assert_eq!(
            client.fallback_stream_url(3, 1),
            "rtsp://nas.local:554/SurveillanceStation/camera.cgi?id=3&streamType=1"
        );
        assert_eq!(
            client.absolute_stream_url("/SurveillanceStation/live?id=1"),
            "rtsp://nas.local:554/SurveillanceStation/live?id=1"
        );
    }

    #[test]
    fn builds_the_mjpeg_endpoint_of_a_camera() {
        let client = SynologyClient::new(config()).unwrap();
        let url = client.mjpeg_url(13, "abc123");
        assert!(url.starts_with("http://nas.local:5000/webapi/entry.cgi?"), "got {url}");
        assert!(url.contains("format=mjpeg"), "got {url}");
        assert!(url.contains("cameraId=13"), "got {url}");
        assert!(url.contains("StmKey=abc123"), "got {url}");
    }

    #[test]
    fn imports_the_mjpeg_endpoint_as_the_sub_stream() {
        let client = SynologyClient::new(config()).unwrap();
        let camera = SynologyCamera {
            id: 13,
            name: "Doorbell".into(),
            host: Some("10.0.0.9".into()),
            port: Some(554),
            vendor: None,
            model: None,
            enabled: true,
            main_url: None,
            sub_url: None,
        };
        let source = client.to_source(
            &camera,
            Some("rtsp://syno:key123@nas.local:554/Sms=13.unicast"),
        );
        // The main stream stays on RTSP, and keeps the credentials the live view
        // path embedded; the sub stream is the NAS's MJPEG endpoint, authorised
        // by the stream key that path carried.
        assert_eq!(source.rtsp_main, "rtsp://nas.local:554/Sms=13.unicast");
        assert_eq!(source.username.as_deref(), Some("syno"));
        assert_eq!(source.password.as_deref(), Some("key123"));
        let expected = client.mjpeg_url(13, "key123");
        assert_eq!(source.rtsp_sub.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn picks_stream_url_from_json_string() {
        let info = normalize_json(&Value::String(
            r#"{"main":{"url":"rtsp://10.0.0.9:554/main"},"sub":{"url":"rtsp://10.0.0.9:554/sub"}}"#.into(),
        ));
        assert_eq!(
            pick_stream_url(&info, &["main", "mainStream"]).as_deref(),
            Some("rtsp://10.0.0.9:554/main")
        );
        assert_eq!(
            pick_stream_url(&info, &["sub"]).as_deref(),
            Some("rtsp://10.0.0.9:554/sub")
        );
    }

    #[test]
    fn rejects_missing_configuration() {
        assert!(SynologyClient::new(SynologyConfig::default()).is_err());
    }
}
