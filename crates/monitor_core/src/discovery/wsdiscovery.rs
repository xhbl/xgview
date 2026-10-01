use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use roxmltree::Document;
use tokio::net::UdpSocket;
use tokio::task::JoinSet;

use crate::error::{CoreError, Result};

/// WS-Discovery multicast group.
pub const WS_DISCOVERY_MULTICAST: &str = "239.255.255.250";
/// WS-Discovery port.
pub const WS_DISCOVERY_PORT: u16 = 3702;
/// Device type advertised by ONVIF network video transmitters.
pub const ONVIF_NVT_TYPE: &str = "dn:NetworkVideoTransmitter";
/// Default ONVIF device service path.
pub const ONVIF_DEVICE_SERVICE_PATH: &str = "/onvif/device_service";

/// How a device was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscoverySource {
    /// Answered the WS-Discovery multicast probe.
    #[default]
    Multicast,
    /// Answered a unicast WS-Discovery probe (cross subnet scan).
    Unicast,
    /// Found by the TCP port scan fallback.
    PortScan,
}

impl DiscoverySource {
    pub fn label(self) -> &'static str {
        match self {
            DiscoverySource::Multicast => "WS-Discovery (multicast)",
            DiscoverySource::Unicast => "WS-Discovery (unicast)",
            DiscoverySource::PortScan => "TCP port scan",
        }
    }
}

/// A device that answered a discovery probe.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiscoveredDevice {
    /// Source address of the UDP answer.
    pub address: Option<IpAddr>,
    /// `wsa:Address` endpoint (device service URI).
    pub endpoint: Option<String>,
    /// `d:XAddrs` device service endpoints.
    pub xaddrs: Vec<String>,
    /// Raw scope URIs, used to derive name / hardware / location.
    pub scopes: Vec<String>,
    /// Device types reported in the probe match.
    pub types: Option<String>,
    pub source: DiscoverySource,
}

impl DiscoveredDevice {
    /// Host of the device, derived from `XAddrs`, falling back to the sender
    /// address.
    pub fn host(&self) -> Option<String> {
        if let Some(url) = self.device_url() {
            if let Some((host, _)) = host_and_port(&url) {
                return Some(host);
            }
        }
        self.address.map(|address| address.to_string())
    }

    /// Best known ONVIF device service URL.
    pub fn device_url(&self) -> Option<String> {
        if let Some(url) = self.xaddrs.first() {
            return Some(url.clone());
        }
        if let Some(endpoint) = &self.endpoint {
            if endpoint.starts_with("http") {
                return Some(endpoint.clone());
            }
        }
        self.host()
            .map(|host| format!("http://{host}{ONVIF_DEVICE_SERVICE_PATH}"))
    }

    /// Port of the ONVIF device service.
    pub fn port(&self) -> Option<u16> {
        self.device_url()
            .and_then(|url| host_and_port(&url))
            .and_then(|(_, port)| port)
    }

    /// Value of an ONVIF scope, e.g. `name`, `hardware`, `location`.
    pub fn scope(&self, kind: &str) -> Option<String> {
        let marker = format!("/{kind}/");
        self.scopes.iter().find_map(|scope| {
            scope
                .split(&marker)
                .nth(1)
                .map(|value| value.split('/').next().unwrap_or(value).to_string())
        })
    }

    /// Friendly device name derived from the ONVIF name scope.
    pub fn name(&self) -> Option<String> {
        self.scope("name").filter(|name| !name.is_empty())
    }

    pub fn hardware(&self) -> Option<String> {
        self.scope("hardware").filter(|value| !value.is_empty())
    }

    pub fn location(&self) -> Option<String> {
        self.scope("location").filter(|value| !value.is_empty())
    }

    /// Label shown in the discovery list.
    pub fn display_name(&self) -> String {
        self.name()
            .or_else(|| self.address.map(|address| address.to_string()))
            .unwrap_or_else(|| "unknown device".to_string())
    }
}

/// Extracts the host (and optional port) from an `http` / `rtsp` URL.
pub fn host_and_port(url: &str) -> Option<(String, Option<u16>)> {
    let rest = url.split_once("://").map(|(_, rest)| rest)?;
    let authority = rest.split(['/', '?']).next()?;
    if authority.is_empty() {
        return None;
    }
    // Strip user info if present.
    let authority = authority.rsplit_once('@').map(|(_, host)| host).unwrap_or(authority);
    if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped.find(']')?;
        let host = stripped[..end].to_string();
        let port = stripped[end + 1..].strip_prefix(':').and_then(|value| value.parse().ok());
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => Some((host.to_string(), Some(port))),
            Err(_) => Some((authority.to_string(), None)),
        },
        None => Some((authority.to_string(), None)),
    }
}

/// Builds a WS-Discovery `Probe` SOAP envelope restricted to ONVIF devices.
pub fn build_probe_message(message_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
            xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <s:Header>
    <a:Action s:mustUnderstand="1">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</a:Action>
    <a:MessageID>uuid:{message_id}</a:MessageID>
    <a:ReplyTo>
      <a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address>
    </a:ReplyTo>
    <a:To s:mustUnderstand="1">urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To>
  </s:Header>
  <s:Body>
    <d:Probe>
      <d:Types>dn:NetworkVideoTransmitter</d:Types>
    </d:Probe>
  </s:Body>
</s:Envelope>"#
    )
}

/// Generates a new message id for a probe.
pub fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Parses a `ProbeMatches` answer.
pub fn parse_probe_matches(xml: &str, source: DiscoverySource) -> Result<Vec<DiscoveredDevice>> {
    let document = Document::parse(xml).map_err(|err| CoreError::xml(err.to_string()))?;

    let mut devices = Vec::new();
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "ProbeMatch")
    {
        let mut device = DiscoveredDevice { source, ..Default::default() };

        for child in node.descendants().filter(|child| child.is_element()) {
            let text = child.text().unwrap_or_default().trim();
            match child.tag_name().name() {
                "Address" if !text.is_empty() => {
                    device.endpoint.get_or_insert_with(|| text.to_string());
                }
                "XAddrs" if !text.is_empty() => {
                    device.xaddrs.extend(text.split_whitespace().map(str::to_string));
                }
                "Scopes" if !text.is_empty() => {
                    device.scopes.extend(text.split_whitespace().map(str::to_string));
                }
                "Types" if device.types.is_none() && !text.is_empty() => {
                    device.types = Some(text.to_string());
                }
                _ => {}
            }
        }

        // Only keep endpoints that actually are a device service URL; the
        // `urn:uuid:` endpoint references are of no use for the ONVIF calls.
        if !device
            .endpoint
            .as_deref()
            .map(|endpoint| endpoint.starts_with("http"))
            .unwrap_or(false)
        {
            device.endpoint = None;
        }

        devices.push(device);
    }

    Ok(devices)
}

/// Sends a WS-Discovery probe to the multicast group and collects the answers.
pub async fn discover_multicast(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))
        .await
        .map_err(|err| CoreError::network(format!("cannot bind discovery socket: {err}")))?;
    socket.set_broadcast(true).ok();

    let probe = build_probe_message(&new_message_id());
    socket
        .send_to(probe.as_bytes(), (WS_DISCOVERY_MULTICAST, WS_DISCOVERY_PORT))
        .await
        .map_err(|err| CoreError::network(format!("multicast probe failed: {err}")))?;

    tracing::debug!(target: "xgview::discovery", "sent multicast WS-Discovery probe");
    Ok(collect(&socket, timeout).await)
}

/// Sends a unicast WS-Discovery probe to every address, limited by
/// `concurrency` simultaneous probes.
pub async fn discover_unicast(
    targets: Vec<Ipv4Addr>,
    timeout: Duration,
    concurrency: usize,
) -> Result<Vec<DiscoveredDevice>> {
    let mut devices = Vec::new();
    if targets.is_empty() {
        return Ok(devices);
    }

    let concurrency = concurrency.max(1);
    let mut workers: JoinSet<Vec<DiscoveredDevice>> = JoinSet::new();
    let mut queue = targets.into_iter();

    for _ in 0..concurrency {
        match queue.next() {
            Some(target) => {
                workers.spawn(async move { probe_one(target, timeout).await });
            }
            None => break,
        }
    }

    while let Some(result) = workers.join_next().await {
        match result {
            Ok(mut found) => devices.append(&mut found),
            Err(err) => tracing::debug!(target: "xgview::discovery", %err, "probe worker failed"),
        }
        if let Some(target) = queue.next() {
            workers.spawn(async move { probe_one(target, timeout).await });
        }
    }

    Ok(devices)
}

async fn probe_one(target: Ipv4Addr, timeout: Duration) -> Vec<DiscoveredDevice> {
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)).await else {
        return Vec::new();
    };
    let probe = build_probe_message(&new_message_id());
    let destination = SocketAddr::from((target, WS_DISCOVERY_PORT));
    if socket.send_to(probe.as_bytes(), destination).await.is_err() {
        return Vec::new();
    }

    let mut devices = collect(&socket, timeout).await;
    for device in &mut devices {
        device.address.get_or_insert(IpAddr::V4(target));
        device.source = DiscoverySource::Unicast;
    }
    devices
}

/// Reads answers until the deadline expires.
async fn collect(socket: &UdpSocket, timeout: Duration) -> Vec<DiscoveredDevice> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut devices: Vec<DiscoveredDevice> = Vec::new();
    let mut buffer = vec![0u8; 16 * 1024];

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let received = tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await;
        let Ok(Ok((size, from))) = received else { break };
        let response = String::from_utf8_lossy(&buffer[..size]);
        match parse_probe_matches(&response, DiscoverySource::Multicast) {
            Ok(mut found) => {
                for device in &mut found {
                    device.address.get_or_insert(from.ip());
                }
                devices.append(&mut found);
            }
            Err(err) => tracing::debug!(target: "xgview::discovery", from = %from, %err, "ignoring malformed probe match"),
        }
    }

    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE_MATCH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery">
  <s:Body>
    <d:ProbeMatches>
      <d:ProbeMatch>
        <a:EndpointReference>
          <a:Address>urn:uuid:6d5f1b1c-1a2b-4c3d-9e8f-0123456789ab</a:Address>
        </a:EndpointReference>
        <d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>
        <d:Scopes>onvif://www.onvif.org/name/FRONT-DOOR onvif://www.onvif.org/hardware/HIKVISION%20DS-2CD</d:Scopes>
        <d:XAddrs>http://192.168.1.64/onvif/device_service</d:XAddrs>
      </d:ProbeMatch>
    </d:ProbeMatches>
  </s:Body>
</s:Envelope>"#;

    #[test]
    fn parses_probe_match() {
        let devices = parse_probe_matches(PROBE_MATCH, DiscoverySource::Multicast).unwrap();
        assert_eq!(devices.len(), 1);
        let device = &devices[0];
        assert_eq!(device.host().as_deref(), Some("192.168.1.64"));
        assert_eq!(device.name().as_deref(), Some("FRONT-DOOR"));
        assert_eq!(device.port(), None);
        assert_eq!(
            device.device_url().as_deref(),
            Some("http://192.168.1.64/onvif/device_service")
        );
    }

    #[test]
    fn probe_message_contains_onvif_type() {
        let probe = build_probe_message("test-id");
        assert!(probe.contains("test-id"));
        assert!(probe.contains(ONVIF_NVT_TYPE));
    }

    #[test]
    fn extracts_host_and_port() {
        assert_eq!(
            host_and_port("http://192.168.1.5:8080/onvif/device_service"),
            Some(("192.168.1.5".to_string(), Some(8080)))
        );
        assert_eq!(host_and_port("invalid"), None);
    }
}
