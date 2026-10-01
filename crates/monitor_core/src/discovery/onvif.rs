use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use roxmltree::Document;
use sha1::{Digest, Sha1};

use crate::error::{CoreError, Result};
use crate::model::StreamKind;

/// ONVIF credentials used for the WS-Security `UsernameToken` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnvifCredentials {
    pub username: String,
    pub password: String,
}

impl OnvifCredentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self { username: username.into(), password: password.into() }
    }
}

/// Basic device information (`GetDeviceInformation`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceInformation {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware_version: Option<String>,
    pub serial_number: Option<String>,
    pub hardware_id: Option<String>,
}

impl DeviceInformation {
    /// `vendor model` label, e.g. `HIKVISION DS-2CD2342`.
    pub fn label(&self) -> String {
        match (self.manufacturer.as_deref(), self.model.as_deref()) {
            (Some(vendor), Some(model)) if vendor != model => format!("{vendor} {model}"),
            (Some(vendor), _) => vendor.to_string(),
            (_, Some(model)) => model.to_string(),
            _ => "unknown device".to_string(),
        }
    }
}

/// A media profile reported by `GetProfiles`.
#[derive(Debug, Clone, PartialEq)]
pub struct OnvifProfile {
    pub token: String,
    pub name: String,
    pub encoding: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    pub bitrate_kbps: Option<u32>,
}

impl OnvifProfile {
    pub fn resolution(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(width), Some(height)) => Some(format!("{width}x{height}")),
            _ => None,
        }
    }

    pub fn pixels(&self) -> u64 {
        u64::from(self.width.unwrap_or(0)) * u64::from(self.height.unwrap_or(0))
    }

    /// Heuristic used to tell the low resolution profile from the main one.
    pub fn looks_like_sub(&self) -> bool {
        let haystack = format!("{} {}", self.token, self.name).to_ascii_lowercase();
        ["sub", "minor", "second", "low", "small", "stream2", "_2"]
            .iter()
            .any(|marker| haystack.contains(marker))
    }
}

/// Everything needed to build a [`crate::model::CameraSource`] from a device.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResolvedDevice {
    pub device: DeviceInformation,
    pub profiles: Vec<OnvifProfile>,
    pub main_uri: Option<String>,
    pub sub_uri: Option<String>,
    pub main_profile: Option<String>,
    pub sub_profile: Option<String>,
}

/// Minimal ONVIF SOAP client.
#[derive(Debug, Clone)]
pub struct OnvifClient {
    /// Device service endpoint, as advertised by WS-Discovery.
    endpoint: String,
    /// Media service endpoint resolved through `GetCapabilities`.
    ///
    /// `None` until it has been resolved; it then holds either the media
    /// `XAddr` or, when the device does not expose one, the device endpoint as
    /// a fallback.
    media_endpoint: Arc<Mutex<Option<String>>>,
    http: reqwest::Client,
    credentials: Option<OnvifCredentials>,
}

impl OnvifClient {
    /// Creates a client for the given device service URL.
    pub fn new(endpoint: impl Into<String>, credentials: Option<OnvifCredentials>) -> Result<Self> {
        let endpoint = endpoint.into();
        if !endpoint.starts_with("http") {
            return Err(CoreError::network(format!("invalid ONVIF endpoint: {endpoint}")));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .danger_accept_invalid_certs(true)
            .build()?;
        Ok(Self {
            endpoint,
            media_endpoint: Arc::new(Mutex::new(None)),
            http,
            credentials,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Media service endpoint, resolved once through `GetCapabilities`.
    ///
    /// Cameras routinely answer `GetDeviceInformation` on the WS-Discovery
    /// device endpoint while rejecting `GetProfiles` there, so the media calls
    /// have to be sent to the `XAddr` reported by `GetCapabilities`.
    async fn media_endpoint(&self) -> String {
        if let Some(cached) = self
            .media_endpoint
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        {
            return cached;
        }

        let resolved = match self.get_capabilities().await {
            Ok(Some(xaddr)) => {
                tracing::debug!(
                    target: "xgview::discovery",
                    endpoint = %self.endpoint,
                    media = %xaddr,
                    "resolved the ONVIF media service endpoint"
                );
                xaddr
            }
            Ok(None) => {
                tracing::debug!(
                    target: "xgview::discovery",
                    endpoint = %self.endpoint,
                    "device exposes no media XAddr, falling back to the device endpoint"
                );
                self.endpoint.clone()
            }
            Err(err) => {
                tracing::debug!(
                    target: "xgview::discovery",
                    endpoint = %self.endpoint,
                    %err,
                    "GetCapabilities failed, falling back to the device endpoint"
                );
                self.endpoint.clone()
            }
        };

        *self
            .media_endpoint
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(resolved.clone());
        resolved
    }

    /// `tds:GetCapabilities` – returns the media service `XAddr`.
    ///
    /// The Media2 service (ONVIF ver20) is preferred over Media when both are
    /// advertised, since it is the one recent firmware exposes for profile
    /// management.
    pub async fn get_capabilities(&self) -> Result<Option<String>> {
        let body = "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>";
        let response = self
            .call_on(
                &self.endpoint,
                "http://www.onvif.org/ver10/device/wsdl/GetCapabilities",
                body,
            )
            .await?;
        let document = Document::parse(&response).map_err(|err| CoreError::xml(err.to_string()))?;
        Ok(media_xaddr(&document))
    }

    /// Sends a SOAP request to the given endpoint and returns the raw response
    /// body.
    ///
    /// When the device answers `401 Unauthorized` the `WWW-Authenticate`
    /// challenge is honoured and the request is replayed with an HTTP
    /// `Authorization` header: a number of cameras (Hikvision, Dahua, …) guard
    /// the ONVIF endpoints with HTTP Digest authentication *in addition* to the
    /// WS-Security username token inside the envelope.
    pub async fn call_on(&self, endpoint: &str, action: &str, body: &str) -> Result<String> {
        let envelope = build_envelope(body, self.credentials.as_ref());

        let send = |authorization: Option<&str>| {
            let mut request = self
                .http
                .post(endpoint)
                .header("Content-Type", "application/soap+xml; charset=utf-8")
                .header("SOAPAction", action)
                .body(envelope.clone());
            if let Some(value) = authorization {
                request = request.header(reqwest::header::AUTHORIZATION, value);
            }
            request.send()
        };

        let response = send(None).await?;
        let status = response.status();
        if status != reqwest::StatusCode::UNAUTHORIZED {
            return finish(action, endpoint, status, response.text().await?);
        }

        let challenge = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let _ = response.text().await;

        let authorization = match (&challenge, self.credentials.as_ref()) {
            (Some(challenge), Some(credentials)) => crate::digest::authorization(
                challenge,
                "POST",
                &request_uri(endpoint),
                &credentials.username,
                &credentials.password,
            ),
            _ => None,
        };

        let Some(authorization) = authorization else {
            return Err(unauthorized(action, endpoint, challenge.as_deref(), self.credentials.is_some()));
        };

        let response = send(Some(&authorization)).await?;
        let status = response.status();
        let text = response.text().await?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(unauthorized(action, endpoint, challenge.as_deref(), true));
        }
        finish(action, endpoint, status, text)
    }

    /// Sends a SOAP request to the device service endpoint.
    pub async fn call(&self, action: &str, body: &str) -> Result<String> {
        self.call_on(&self.endpoint, action, body).await
    }

    /// `tds:GetDeviceInformation`
    pub async fn get_device_information(&self) -> Result<DeviceInformation> {
        let body = "<tds:GetDeviceInformation/>";
        let response = self
            .call("http://www.onvif.org/ver10/device/wsdl/GetDeviceInformation", body)
            .await?;
        let document = Document::parse(&response).map_err(|err| CoreError::xml(err.to_string()))?;
        Ok(DeviceInformation {
            manufacturer: find_text(&document, "Manufacturer"),
            model: find_text(&document, "Model"),
            firmware_version: find_text(&document, "FirmwareVersion"),
            serial_number: find_text(&document, "SerialNumber"),
            hardware_id: find_text(&document, "HardwareId"),
        })
    }

    /// `trt:GetProfiles`
    pub async fn get_profiles(&self) -> Result<Vec<OnvifProfile>> {
        let body = "<trt:GetProfiles/>";
        let endpoint = self.media_endpoint().await;
        let response = self
            .call_on(
                &endpoint,
                "http://www.onvif.org/ver10/media/wsdl/GetProfiles",
                body,
            )
            .await?;
        let document = Document::parse(&response).map_err(|err| CoreError::xml(err.to_string()))?;

        let mut profiles = Vec::new();
        for node in document
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "Profiles")
        {
            let token = node.attribute("token").unwrap_or_default().to_string();
            let name = child_text(node, "Name").unwrap_or_else(|| token.clone());
            let encoder = node
                .descendants()
                .find(|child| child.is_element() && child.tag_name().name() == "VideoEncoderConfiguration");

            let (encoding, width, height, fps, bitrate_kbps) = match encoder {
                Some(encoder) => {
                    let encoding = child_text(encoder, "Encoding");
                    let resolution = encoder
                        .descendants()
                        .find(|child| child.is_element() && child.tag_name().name() == "Resolution");
                    let width = resolution
                        .and_then(|node| child_text(node, "Width"))
                        .and_then(|text| text.parse::<u32>().ok());
                    let height = resolution
                        .and_then(|node| child_text(node, "Height"))
                        .and_then(|text| text.parse::<u32>().ok());
                    let rate_control = encoder
                        .descendants()
                        .find(|child| child.is_element() && child.tag_name().name() == "RateControl");
                    let fps = rate_control
                        .and_then(|node| child_text(node, "FrameRateLimit"))
                        .and_then(|text| text.parse::<f32>().ok());
                    let bitrate_kbps = rate_control
                        .and_then(|node| child_text(node, "BitrateLimit"))
                        .and_then(|text| text.parse::<u32>().ok());
                    (encoding, width, height, fps, bitrate_kbps)
                }
                None => (None, None, None, None, None),
            };

            profiles.push(OnvifProfile {
                token,
                name,
                encoding,
                width,
                height,
                fps,
                bitrate_kbps,
            });
        }

        if profiles.is_empty() {
            return Err(CoreError::parse("device returned no media profile"));
        }
        Ok(profiles)
    }

    /// `trt:GetStreamUri` for one profile.
    pub async fn get_stream_uri(&self, profile_token: &str) -> Result<String> {
        let body = format!(
            "<trt:GetStreamUri>\
               <trt:StreamSetup>\
                 <tt:Stream>RTP-Unicast</tt:Stream>\
                 <tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport>\
               </trt:StreamSetup>\
               <trt:ProfileToken>{}</trt:ProfileToken>\
             </trt:GetStreamUri>",
            escape_xml(profile_token)
        );
        let endpoint = self.media_endpoint().await;
        let response = self
            .call_on(
                &endpoint,
                "http://www.onvif.org/ver10/media/wsdl/GetStreamUri",
                &body,
            )
            .await?;
        let document = Document::parse(&response).map_err(|err| CoreError::xml(err.to_string()))?;
        find_text(&document, "Uri")
            .ok_or_else(|| CoreError::parse("GetStreamUri returned no Uri element"))
    }

    /// Resolves the device information, its profiles and the main / sub stream
    /// URLs in a single call.
    pub async fn resolve(&self) -> Result<ResolvedDevice> {
        // Device information is a nice to have: a number of models refuse
        // `GetDeviceInformation` while serving the media calls fine.
        let device = match self.get_device_information().await {
            Ok(device) => device,
            Err(err) => {
                tracing::debug!(
                    target: "xgview::discovery",
                    endpoint = %self.endpoint,
                    %err,
                    "GetDeviceInformation failed, continuing without it"
                );
                DeviceInformation::default()
            }
        };
        let profiles = self.get_profiles().await?;

        let main = profiles
            .iter()
            .filter(|profile| !profile.looks_like_sub())
            .max_by_key(|profile| profile.pixels())
            .or_else(|| profiles.iter().max_by_key(|profile| profile.pixels()))
            .cloned();
        let sub = profiles
            .iter()
            .filter(|profile| profile.looks_like_sub())
            .min_by_key(|profile| profile.pixels())
            .cloned();

        let mut resolved = ResolvedDevice { device, profiles, ..Default::default() };
        if let Some(profile) = &main {
            resolved.main_profile = Some(profile.token.clone());
            resolved.main_uri = self.get_stream_uri(&profile.token).await.ok();
        }
        if let Some(profile) = &sub {
            resolved.sub_profile = Some(profile.token.clone());
            resolved.sub_uri = self.get_stream_uri(&profile.token).await.ok();
        }
        if resolved.main_uri.is_none() {
            return Err(CoreError::parse("no stream URI could be resolved"));
        }
        Ok(resolved)
    }

    /// Returns the stream URI of the requested kind (used when re-negotiating
    /// a channel at runtime).
    pub async fn stream_uri(&self, kind: StreamKind, profile_token: Option<&str>) -> Result<String> {
        match profile_token {
            Some(token) => self.get_stream_uri(token).await,
            None => {
                let resolved = self.resolve().await?;
                match kind {
                    StreamKind::Main => resolved
                        .main_uri
                        .ok_or_else(|| CoreError::parse("no main stream available")),
                    StreamKind::Sub => resolved
                        .sub_uri
                        .or(resolved.main_uri)
                        .ok_or_else(|| CoreError::parse("no sub stream available")),
                }
            }
        }
    }
}

/// Builds the SOAP envelope, adding the WS-Security header when credentials
/// are configured.
pub fn build_envelope(body: &str, credentials: Option<&OnvifCredentials>) -> String {
    let security = credentials.map(build_security_header).unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:tds="http://www.onvif.org/ver10/device/wsdl"
            xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
            xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Header>{security}</s:Header>
  <s:Body>{body}</s:Body>
</s:Envelope>"#
    )
}

/// WS-Security `UsernameToken` with a `PasswordDigest`.
pub fn build_security_header(credentials: &OnvifCredentials) -> String {
    const WSSE: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd";
    const WSU: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd";
    const DIGEST: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest";
    const NONCE_ENCODING: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary";

    let nonce_bytes = uuid::Uuid::new_v4().into_bytes();
    let created = utc_timestamp();
    let digest = {
        let mut hasher = Sha1::new();
        hasher.update(nonce_bytes);
        hasher.update(created.as_bytes());
        hasher.update(credentials.password.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
    };
    let nonce = base64::engine::general_purpose::STANDARD.encode(nonce_bytes);

    format!(
        r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="{WSSE}" xmlns:wsu="{WSU}">
      <wsse:UsernameToken>
        <wsse:Username>{}</wsse:Username>
        <wsse:Password Type="{DIGEST}">{digest}</wsse:Password>
        <wsse:Nonce EncodingType="{NONCE_ENCODING}">{nonce}</wsse:Nonce>
        <wsu:Created>{created}</wsu:Created>
      </wsse:UsernameToken>
    </wsse:Security>"#,
        escape_xml(&credentials.username)
    )
}

/// Interprets a HTTP response that is not a `401`.
fn finish(action: &str, endpoint: &str, status: reqwest::StatusCode, body: String) -> Result<String> {
    if status.is_success() {
        return Ok(body);
    }
    Err(soap_fault(action, endpoint, status, &body))
}

/// Builds the error returned for a non successful HTTP status.
///
/// A SOAP Fault carries a human readable reason, which is far more useful than
/// the bare status code, so it is extracted when present.
fn soap_fault(
    action: &str,
    endpoint: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> CoreError {
    let reason = Document::parse(body)
        .ok()
        .and_then(|document| find_fault_reason(&document));

    match reason {
        Some(reason) => CoreError::network(format!(
            "ONVIF {action} at {endpoint} failed with HTTP {status}: {reason}"
        )),
        None => CoreError::network(format!(
            "ONVIF {action} at {endpoint} failed with HTTP {status}"
        )),
    }
}

/// Extracts the `Text` of a SOAP 1.2 `Fault` / `Reason`, then of a SOAP 1.1
/// `faultstring`.
fn find_fault_reason(document: &Document) -> Option<String> {
    document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "Text")
        .and_then(|node| node.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .or_else(|| find_text(document, "faultstring"))
}

/// Builds the actionable error returned when the device demands authentication.
fn unauthorized(
    action: &str,
    endpoint: &str,
    challenge: Option<&str>,
    has_credentials: bool,
) -> CoreError {
    let scheme = challenge
        .and_then(|value| value.split_whitespace().next())
        .unwrap_or("HTTP");
    let hint = if has_credentials {
        "the configured ONVIF user name / password was rejected"
    } else {
        "no ONVIF credentials are configured"
    };
    CoreError::network(format!(
        "ONVIF {action} at {endpoint} requires authentication ({scheme} 401 Unauthorized) - {hint}; \
         set the ONVIF user name and password in the discovery dialog (F2)"
    ))
}

/// Extracts the media service address from a `GetCapabilities` response.
///
/// Media is preferred over Media2 because every request this client builds uses
/// the Onvif ver10 media namespaces; Media2 is only used when a device does not
/// advertise a ver10 media service at all.
fn media_xaddr(document: &Document) -> Option<String> {
    ["Media", "Media2"]
        .iter()
        .find_map(|service| {
            document
                .descendants()
                .find(|node| node.is_element() && node.tag_name().name() == *service)
                .and_then(|node| child_text(node, "XAddr"))
        })
}

/// Path and query of an endpoint, as used by the digest response.
fn request_uri(endpoint: &str) -> String {
    let after_scheme = endpoint
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint);
    match after_scheme.find('/') {
        Some(index) => after_scheme[index..].to_string(),
        None => "/".to_string(),
    }
}

/// Escapes the five XML predefined entities.
pub fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Current time as an ISO 8601 / RFC 3339 UTC timestamp.
pub fn utc_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let days = (seconds / 86_400) as i64;
    let remaining = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remaining / 3_600,
        (remaining % 3_600) / 60,
        remaining % 60
    )
}

/// Howard Hinnant's `civil_from_days` algorithm (days since 1970-01-01).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Text of the first element with the given local name.
fn find_text(document: &Document, name: &str) -> Option<String> {
    document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == name)
        .and_then(|node| node.text())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Text of the first direct child element with the given local name.
fn child_text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
        .and_then(|child| child.text())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_expected_iso_timestamp() {
        // 2024-01-01T00:00:00Z == 19723 days since the epoch.
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        let timestamp = utc_timestamp();
        assert!(timestamp.ends_with('Z'));
        assert_eq!(timestamp.len(), 20);
    }

    #[test]
    fn security_header_contains_digest() {
        let header = build_security_header(&OnvifCredentials::new("admin", "secret"));
        assert!(header.contains("<wsse:Username>admin</wsse:Username>"));
        assert!(header.contains("PasswordDigest"));
    }

    #[test]
    fn parses_device_information() {
        let xml = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tds:GetDeviceInformationResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
      <tds:Manufacturer>HIKVISION</tds:Manufacturer>
      <tds:Model>DS-2CD2342</tds:Model>
      <tds:SerialNumber>ABC123</tds:SerialNumber>
    </tds:GetDeviceInformationResponse>
  </s:Body>
</s:Envelope>"#;
        let document = Document::parse(xml).unwrap();
        assert_eq!(find_text(&document, "Manufacturer").as_deref(), Some("HIKVISION"));
        assert_eq!(find_text(&document, "SerialNumber").as_deref(), Some("ABC123"));
        assert_eq!(find_text(&document, "FirmwareVersion"), None);
    }

    #[test]
    fn parses_profiles_response() {
        let xml = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
                             xmlns:tt="http://www.onvif.org/ver10/schema">
      <trt:Profiles token="Profile_1">
        <tt:Name>mainStream</tt:Name>
        <tt:VideoEncoderConfiguration>
          <tt:Encoding>H264</tt:Encoding>
          <tt:Resolution><tt:Width>1920</tt:Width><tt:Height>1080</tt:Height></tt:Resolution>
          <tt:RateControl><tt:FrameRateLimit>25</tt:FrameRateLimit><tt:BitrateLimit>4096</tt:BitrateLimit></tt:RateControl>
        </tt:VideoEncoderConfiguration>
      </trt:Profiles>
      <trt:Profiles token="Profile_2">
        <tt:Name>subStream</tt:Name>
        <tt:VideoEncoderConfiguration>
          <tt:Encoding>H264</tt:Encoding>
          <tt:Resolution><tt:Width>640</tt:Width><tt:Height>360</tt:Height></tt:Resolution>
        </tt:VideoEncoderConfiguration>
      </trt:Profiles>
    </trt:GetProfilesResponse>
  </s:Body>
</s:Envelope>"#;
        let document = Document::parse(xml).unwrap();
        let nodes: Vec<_> = document
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "Profiles")
            .collect();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].attribute("token"), Some("Profile_1"));
        assert_eq!(child_text(nodes[0], "Name").as_deref(), Some("mainStream"));
        assert_eq!(child_text(nodes[1], "Name").as_deref(), Some("subStream"));
    }

    #[test]
    fn detects_sub_profiles() {
        let profile = OnvifProfile {
            token: "Profile_2".into(),
            name: "subStream".into(),
            encoding: Some("H264".into()),
            width: Some(640),
            height: Some(360),
            fps: Some(15.0),
            bitrate_kbps: Some(512),
        };
        assert!(profile.looks_like_sub());
        assert_eq!(profile.resolution().as_deref(), Some("640x360"));
    }

    #[test]
    fn request_uri_keeps_path_and_query() {
        assert_eq!(request_uri("http://192.168.1.10/onvif/device_service"), "/onvif/device_service");
        assert_eq!(request_uri("https://host/onvif/media?x=1"), "/onvif/media?x=1");
        assert_eq!(request_uri("http://192.168.1.10"), "/");
    }

    #[test]
    fn extracts_media_xaddr_from_capabilities() {
        let xml = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tds:GetCapabilitiesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"
                                 xmlns:tt="http://www.onvif.org/ver10/schema">
      <tds:Capabilities>
        <tt:Device><tt:XAddr>http://192.168.1.10/onvif/device_service</tt:XAddr></tt:Device>
        <tt:Media><tt:XAddr>http://192.168.1.10/onvif/media_service</tt:XAddr></tt:Media>
      </tds:Capabilities>
    </tds:GetCapabilitiesResponse>
  </s:Body>
</s:Envelope>"#;
        let document = Document::parse(xml).unwrap();
        assert_eq!(media_xaddr(&document).as_deref(), Some("http://192.168.1.10/onvif/media_service"));
    }

    #[test]
    fn falls_back_to_media2_xaddr() {
        let xml = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tds:GetCapabilitiesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"
                                 xmlns:tt="http://www.onvif.org/ver10/schema">
      <tds:Capabilities>
        <tt:Media2><tt:XAddr>http://192.168.1.10/onvif/media2_service</tt:XAddr></tt:Media2>
      </tds:Capabilities>
    </tds:GetCapabilitiesResponse>
  </s:Body>
</s:Envelope>"#;
        let document = Document::parse(xml).unwrap();
        assert_eq!(media_xaddr(&document).as_deref(), Some("http://192.168.1.10/onvif/media2_service"));
    }

    #[test]
    fn reports_authentication_hint() {
        let message = unauthorized(
            "GetProfiles",
            "http://192.168.1.10/onvif/media_service",
            Some(r#"Digest realm="IP Camera", nonce="abc""#),
            false,
        )
        .to_string();
        assert!(message.contains("401"));
        assert!(message.contains("Digest"));
        assert!(message.contains("no ONVIF credentials"));
    }

    #[test]
    fn keeps_the_soap_fault_reason() {
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <s:Fault>
      <s:Reason><s:Text xml:lang="en">Sender not Authorized</s:Text></s:Reason>
    </s:Fault>
  </s:Body>
</s:Envelope>"#;
        let error = soap_fault("GetProfiles", "http://192.168.1.10/onvif/media_service", reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert!(error.to_string().contains("Sender not Authorized"));
    }
}
