//! Standalone go2rtc import.
//!
//! go2rtc serves every stream it is configured with on its RTSP port, as
//! `rtsp://<host>:8554/<name>`. Its HTTP API lists the names; it carries no
//! grouping at all, so each stream is offered on its own and the viewer picks
//! what to add. This is deliberately independent of the Frigate import, which
//! reads Frigate's API instead - the built-in go2rtc's management port is
//! normally not published.

use std::time::Duration;

use serde_json::Value;

use crate::config::Go2rtcConfig;
use crate::error::{CoreError, Result};
use crate::model::{CameraOrigin, CameraSource};

/// `rtsp://<host>:8554/<stream>` - the restream of one stream, which is what
/// XGView pulls rather than the camera behind it.
pub fn restream_url(host: &str, stream: &str) -> String {
    format!("rtsp://{host}:8554/{stream}")
}

/// Client for a go2rtc HTTP API.
#[derive(Debug, Clone)]
pub struct Go2rtcClient {
    config: Go2rtcConfig,
    http: reqwest::Client,
}

impl Go2rtcClient {
    pub fn new(config: Go2rtcConfig) -> Result<Self> {
        if !config.is_configured() {
            return Err(CoreError::config("go2rtc host must be configured"));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(true)
            .build()?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &Go2rtcConfig {
        &self.config
    }

    /// `GET /api/streams` - the names go2rtc serves.
    pub async fn streams(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/streams", self.config.api_url());
        let mut request = self.http.get(&url);
        // go2rtc's `api:` account, when it has one - HTTP Basic, unlike
        // Frigate's login.
        if !self.config.api_username.trim().is_empty() {
            request = request.basic_auth(&self.config.api_username, Some(&self.config.api_password));
        }
        let response = request.send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CoreError::network("go2rtc refused the API credentials"));
        }
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(CoreError::network(format!("go2rtc /api/streams answered {status}")));
        }
        Ok(parse_streams(&text))
    }

    /// The streams as cameras, each one a main-only feed.
    ///
    /// go2rtc has no notion of a main and a sub stream, so nothing here pairs
    /// them: two feeds of one camera are two entries, and the viewer decides
    /// which to add. The edit form can give one of them a sub stream later.
    pub async fn import_cameras(&self) -> Result<Vec<CameraSource>> {
        Ok(self.streams().await?.iter().map(|name| self.to_source(name)).collect())
    }

    fn to_source(&self, name: &str) -> CameraSource {
        let mut source = CameraSource::new(name, restream_url(&self.config.host, name));
        source.host = self.config.host.clone();
        source.origin = CameraOrigin::Go2rtc;
        source.tags = vec!["go2rtc".to_string()];
        if !self.config.rtsp_username.trim().is_empty() {
            source.username = Some(self.config.rtsp_username.clone());
            source.password = Some(self.config.rtsp_password.clone());
        }
        source
    }
}

/// The stream names in an `/api/streams` body.
///
/// The body maps each name to its producers and consumers; only the names are
/// wanted, since the address XGView pulls is the restream.
fn parse_streams(body: &str) -> Vec<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.as_object().map(|map| map.keys().cloned().collect()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_stream_names() {
        let body = r#"{
            "cam_front_door_1": {"producers":[{"url":"rtsp://admin:pw@192.168.27.40:88/videoMain"}],"consumers":[]},
            "cam_doorbell_2": {"producers":[],"consumers":[{"id":10}]}
        }"#;
        let names = parse_streams(body);
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"cam_front_door_1".to_string()));
        assert!(names.contains(&"cam_doorbell_2".to_string()));
    }

    #[test]
    fn a_body_that_is_not_json_yields_nothing() {
        assert!(parse_streams("Unauthorized").is_empty());
    }

    #[test]
    fn the_restream_url_is_the_go2rtc_rtsp_port() {
        assert_eq!(
            restream_url("192.168.17.88", "cam_front_door_1"),
            "rtsp://192.168.17.88:8554/cam_front_door_1"
        );
    }
}
