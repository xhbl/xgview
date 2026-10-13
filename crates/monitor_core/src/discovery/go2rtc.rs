//! Standalone go2rtc import.
//!
//! go2rtc serves every stream it is configured with on its RTSP port, as
//! `rtsp://<host>:<port>/<name>` - 8554 unless `rtsp.listen` says otherwise -
//! and the same stream as MSE (fMP4 over a WebSocket) at `/api/ws?src=<name>`
//! on its HTTP API port. Its HTTP API lists the names; it carries no grouping
//! at all, so each stream is offered on its own and the viewer picks what to
//! add. This is deliberately independent of the Frigate import, which reads
//! Frigate's API instead - the built-in go2rtc's management port is normally
//! not published.

use std::time::Duration;

use serde_json::Value;

use crate::config::Go2rtcConfig;
use crate::error::{CoreError, Result};
use crate::model::{CameraOrigin, CameraSource};

/// go2rtc's default RTSP port, used when neither the tab nor the server names
/// another.
pub const RESTREAM_PORT: u16 = 8554;

/// `rtsp://<host>:<port>/<stream>` - the restream of one stream, which is what
/// XGView pulls rather than the camera behind it.
pub fn restream_url(host: &str, port: u16, stream: &str) -> String {
    format!("rtsp://{host}:{port}/{stream}")
}

/// The MSE (WebSocket) URL of one stream, on go2rtc's own HTTP API port.
///
/// go2rtc serves MSE at `/api/ws?src=<stream>` - the same shape Frigate fronts
/// at `/live/mse/api/ws`, but on go2rtc's own port and behind its `api:` account
/// (HTTP Basic) rather than Frigate's session login.
pub fn mse_url(config: &Go2rtcConfig, stream: &str) -> String {
    let scheme = if config.https { "wss" } else { "ws" };
    format!("{scheme}://{}:{}/api/ws?src={stream}", config.host, config.port)
}

/// What one go2rtc fetch produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Go2rtcImport {
    /// One camera per stream go2rtc serves.
    pub cameras: Vec<CameraSource>,
    /// Whether the restream account was left empty and none could be read from
    /// go2rtc either - its `/api/config` serves no file (a configuration given
    /// on the command line answers 410), or the file it serves names no `rtsp:`
    /// account. A protected restream then has nowhere to take one from but the
    /// tab's own field.
    pub restream_account_missing: bool,
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

    /// A GET carrying go2rtc's `api:` account when it has one - HTTP Basic,
    /// unlike Frigate's login.
    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self.http.get(url);
        if self.config.api_username.trim().is_empty() {
            request
        } else {
            request.basic_auth(&self.config.api_username, Some(&self.config.api_password))
        }
    }

    /// `GET /api/streams` - the names go2rtc serves.
    pub async fn streams(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/streams", self.config.api_url());
        let response = self.get(&url).send().await?;
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
    pub async fn import_cameras(&self) -> Result<Go2rtcImport> {
        let port = self.restream_port().await;
        let (credentials, restream_account_missing) = self.restream_credentials().await;
        let cameras = self
            .streams()
            .await?
            .iter()
            .map(|name| self.to_source(name, port, &credentials))
            .collect();
        Ok(Go2rtcImport { cameras, restream_account_missing })
    }

    /// The port the restream is served on: the one entered on the tab, else the
    /// one go2rtc reports as `rtsp.listen`, else [`RESTREAM_PORT`].
    async fn restream_port(&self) -> u16 {
        match self.config.rtsp_port {
            Some(port) => port,
            None => self.listen_port().await.unwrap_or(RESTREAM_PORT),
        }
    }

    /// The restream account, and whether one was wanted but not found: the one
    /// entered on the tab, else the one go2rtc's configuration file names.
    async fn restream_credentials(&self) -> (Option<(String, String)>, bool) {
        if !self.config.rtsp_username.trim().is_empty() {
            let entered = (self.config.rtsp_username.clone(), self.config.rtsp_password.clone());
            return (Some(entered), false);
        }
        let read = self.config_account().await;
        let missing = read.is_none();
        (read, missing)
    }

    /// `rtsp.listen` from `GET /api` - go2rtc's Info, which names the port but
    /// hides the account. `None` when the endpoint is unreachable or does not
    /// say.
    async fn listen_port(&self) -> Option<u16> {
        let url = format!("{}/api", self.config.api_url());
        let response = self.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        parse_listen(&response.text().await.ok()?)
    }

    /// The `rtsp:` account from `GET /api/config` - go2rtc hands its own
    /// configuration file back, and that file may name the account `/api`
    /// hides. `None` when no file is served (a configuration given on the
    /// command line answers 410), or when the file it serves names no account -
    /// which a Frigate-managed go2rtc does, its `rtsp:` account living in
    /// Frigate's own configuration instead.
    async fn config_account(&self) -> Option<(String, String)> {
        let url = format!("{}/api/config", self.config.api_url());
        let response = self.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        parse_account(&response.text().await.ok()?)
    }

    fn to_source(&self, name: &str, port: u16, credentials: &Option<(String, String)>) -> CameraSource {
        let mut source = CameraSource::new(name, restream_url(&self.config.host, port, name));
        source.host = self.config.host.clone();
        source.origin = CameraOrigin::Go2rtc;
        source.tags = vec!["go2rtc".to_string()];
        // Both addresses are captured, as a Frigate import does: the RTSP
        // restream, and the MSE endpoint on go2rtc's own API port. The camera
        // prefers RTSP and reaches for the WebSocket when RTSP cannot be.
        source.mse_main = Some(mse_url(&self.config, name));
        source.connect = self.config.protocol.connection_mode();
        // The restream account, for the RTSP address.
        if let Some((username, password)) = credentials {
            source.username = Some(username.clone());
            source.password = Some(password.clone());
        }
        // go2rtc's `api:` account, for the WebSocket address - a different
        // credential from the restream one.
        if !self.config.api_username.trim().is_empty() {
            source.mse_username = Some(self.config.api_username.clone());
            source.mse_password = Some(self.config.api_password.clone());
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

/// The restream port in a go2rtc `/api` body - its `rtsp.listen`.
fn parse_listen(body: &str) -> Option<u16> {
    let value: Value = serde_json::from_str(body).ok()?;
    port_of_listen(value.get("rtsp")?.get("listen")?)
}

/// The port in a `listen` value, in the shapes go2rtc writes it - a string, or
/// a list of them when several addresses are bound.
pub(crate) fn port_of_listen(value: &Value) -> Option<u16> {
    match value {
        Value::String(text) => port_in(text),
        Value::Array(items) => items.iter().find_map(port_of_listen),
        Value::Number(number) => number.as_u64().and_then(|port| u16::try_from(port).ok()),
        _ => None,
    }
}

/// The port of one `listen` string - `:8554`, `0.0.0.0:8554`, a bare `8554`, or
/// an IPv6 form whose port is the last colon-separated segment.
fn port_in(listen: &str) -> Option<u16> {
    listen.rsplit(':').next()?.trim().parse::<u16>().ok().filter(|port| *port >= 1)
}

/// The `rtsp:` account in a go2rtc configuration file.
///
/// `/api` hides it (`json:"-"` on the rtsp module), but `/api/config` hands the
/// file back as written. go2rtc accepts the file as YAML or JSON; a JSON body is
/// read as JSON, a YAML one by the two keys wanted rather than through a YAML
/// parser pulled in for them.
fn parse_account(body: &str) -> Option<(String, String)> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let rtsp = value.get("rtsp")?;
        return account_pair(
            rtsp.get("username").and_then(Value::as_str)?,
            rtsp.get("password").and_then(Value::as_str),
        );
    }
    account_pair(yaml_scalar(body, "rtsp", "username")?, yaml_scalar(body, "rtsp", "password"))
}

/// A username and an optional password, rejected when a value is one only the
/// server can resolve - a `${VAR}` expression.
fn account_pair(username: &str, password: Option<&str>) -> Option<(String, String)> {
    let username = resolved(username)?;
    let password = match password {
        Some(password) => resolved(password)?,
        None => String::new(),
    };
    Some((username, password))
}

/// A scalar that is an actual answer: not empty, and not a `${VAR}` expression
/// go2rtc substitutes at load time.
fn resolved(value: &str) -> Option<String> {
    (!value.is_empty() && !value.contains("${")).then(|| value.to_string())
}

/// The value of `section.key` in a YAML body, from the block the section opens.
/// `None` when the section or the key is absent - a flow-style `rtsp: {…}` is
/// not read.
fn yaml_scalar<'a>(body: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let mut inside = false;
    for line in body.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            let trimmed = line.trim();
            if !inside || trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.trim() == key {
                    return Some(strip_scalar(value));
                }
            }
        } else {
            inside = line.split_once(':').is_some_and(|(name, _)| name.trim() == section);
        }
    }
    None
}

/// A YAML scalar without its surrounding quotes or inline comment.
fn strip_scalar(raw: &str) -> &str {
    let raw = raw.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = raw.strip_prefix(quote).and_then(|rest| rest.split(quote).next()) {
            return inner;
        }
    }
    raw.split(" #").next().unwrap_or(raw).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StreamProtocol;
    use crate::model::ConnectionMode;

    /// A stream is offered with both addresses - the RTSP restream and the MSE
    /// endpoint on go2rtc's own API port - and each carries its own account.
    #[test]
    fn a_stream_carries_both_addresses() {
        let config = Go2rtcConfig {
            host: "192.168.17.90".into(),
            port: 1984,
            api_username: "api".into(),
            api_password: "api-pw".into(),
            ..Default::default()
        };
        let client = Go2rtcClient::new(config).expect("a configured host");
        let source = client.to_source("cam_1", 8554, &Some(("admin".into(), "secret".into())));

        assert_eq!(source.rtsp_main, "rtsp://192.168.17.90:8554/cam_1");
        assert_eq!(source.mse_main.as_deref(), Some("ws://192.168.17.90:1984/api/ws?src=cam_1"));
        // The restream account on the RTSP address, the api account on the ws one.
        assert_eq!(source.username.as_deref(), Some("admin"));
        assert_eq!(source.password.as_deref(), Some("secret"));
        assert_eq!(source.mse_username.as_deref(), Some("api"));
        assert_eq!(source.mse_password.as_deref(), Some("api-pw"));
        assert_eq!(source.origin, CameraOrigin::Go2rtc);
        assert_eq!(source.connect, ConnectionMode::Auto);
    }

    /// No `api:` account: the MSE address is still captured, just without a
    /// credential of its own.
    #[test]
    fn the_mse_address_captures_without_an_api_account() {
        let config = Go2rtcConfig { host: "192.168.17.90".into(), ..Default::default() };
        let client = Go2rtcClient::new(config).expect("a configured host");
        let source = client.to_source("cam_1", 8554, &None);
        assert_eq!(source.mse_main.as_deref(), Some("ws://192.168.17.90:1984/api/ws?src=cam_1"));
        assert!(source.mse_username.is_none());
    }

    /// The tab's protocol choice is written onto each imported stream.
    #[test]
    fn the_protocol_sets_the_connection_mode() {
        for (protocol, expected) in [
            (StreamProtocol::Auto, ConnectionMode::Auto),
            (StreamProtocol::Rtsp, ConnectionMode::Rtsp),
            (StreamProtocol::Mse, ConnectionMode::Mse),
        ] {
            let config = Go2rtcConfig { host: "10.0.0.1".into(), protocol, ..Default::default() };
            let client = Go2rtcClient::new(config).expect("a configured host");
            let source = client.to_source("cam_1", 8554, &None);
            assert_eq!(source.connect, expected);
        }
    }

    #[test]
    fn the_mse_url_scheme_follows_https() {
        let config = Go2rtcConfig {
            host: "10.0.0.1".into(),
            port: 1984,
            https: true,
            ..Default::default()
        };
        assert_eq!(mse_url(&config, "cam_1"), "wss://10.0.0.1:1984/api/ws?src=cam_1");
    }

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
    fn the_restream_url_carries_the_port() {
        assert_eq!(
            restream_url("192.168.17.88", 8554, "cam_front_door_1"),
            "rtsp://192.168.17.88:8554/cam_front_door_1"
        );
        assert_eq!(
            restream_url("192.168.17.88", 9000, "cam_front_door_1"),
            "rtsp://192.168.17.88:9000/cam_front_door_1"
        );
    }

    #[test]
    fn the_restream_port_is_read_from_rtsp_listen() {
        assert_eq!(parse_listen(r#"{"rtsp":{"listen":":8554"}}"#), Some(8554));
        // A bound address, an IPv6 form, a bare port and a list are all read.
        assert_eq!(parse_listen(r#"{"rtsp":{"listen":"0.0.0.0:9000"}}"#), Some(9000));
        assert_eq!(parse_listen(r#"{"rtsp":{"listen":"[::]:9000"}}"#), Some(9000));
        assert_eq!(parse_listen(r#"{"rtsp":{"listen":8554}}"#), Some(8554));
        assert_eq!(parse_listen(r#"{"rtsp":{"listen":[":8554",":9000"]}}"#), Some(8554));
        // Nothing to read: an absent section, or one without a usable port.
        assert_eq!(parse_listen(r#"{"rtsp":{"default_query":"src=x"}}"#), None);
        assert_eq!(parse_listen("Unauthorized"), None);
    }

    #[test]
    fn the_restream_account_is_read_from_the_config_file() {
        let yaml = "rtsp:\n  listen: \":8554\"\n  username: admin\n  password: _sxtmmadmin1\nstreams:\n  cam: rtsp://x\n";
        assert_eq!(parse_account(yaml), Some(("admin".into(), "_sxtmmadmin1".into())));

        // Quotes, an inline comment, and the block ending at the next section.
        let yaml = "rtsp:\n  username: 'admin'  # the restream account\n  password: \"p#1\"\napi:\n  username: someone\n";
        assert_eq!(parse_account(yaml), Some(("admin".into(), "p#1".into())));

        // go2rtc also accepts a JSON configuration.
        assert_eq!(
            parse_account(r#"{"rtsp":{"username":"admin","password":"pw"}}"#),
            Some(("admin".into(), "pw".into()))
        );

        // A `${VAR}` password is the file's template, not the running account,
        // and a file that names no account gives nothing.
        assert_eq!(parse_account("rtsp:\n  username: admin\n  password: ${RTSP_PASS:secret}\n"), None);
        assert_eq!(parse_account("rtsp:\n  listen: \":8554\"\n"), None);
        assert_eq!(parse_account("streams:\n  cam: rtsp://x\n"), None);
        // An `api:` account is not the restream account - only `rtsp:` counts.
        assert_eq!(
            parse_account("api:\n  username: admin\n  password: pw\nstreams:\n  cam: rtsp://x\n"),
            None
        );
    }
}
