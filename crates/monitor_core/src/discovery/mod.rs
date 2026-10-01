//! Camera discovery and import.
//!
//! Three complementary strategies are implemented:
//!
//! 1. [`wsdiscovery::discover_multicast`] – ONVIF WS-Discovery probe sent to
//!    `239.255.255.250:3702`, finds every device of the local subnet.
//! 2. [`wsdiscovery::discover_unicast`] – the same probe sent as a unicast
//!    datagram to every address of the configured ranges, which also works
//!    across VLANs and subnets.
//! 3. [`scan::scan_ports`] – TCP connect scan of 554 / 80 / 8000 used as a
//!    fallback for silent devices that never answer WS-Discovery.
//!
//! Once a device is known, [`DiscoveryService::import_device`] uses the ONVIF
//! `GetDeviceInformation` / `GetProfiles` / `GetStreamUri` calls to build a
//! ready to use [`CameraSource`].

pub mod iprange;
pub mod onvif;
pub mod scan;
pub mod synology;
pub mod wsdiscovery;

use std::sync::Arc;
use std::time::Duration;

use crate::config::DiscoveryConfig;
use crate::error::{CoreError, Result};
use crate::model::{CameraOrigin, CameraSource};

pub use iprange::{parse_targets, parse_targets_multi};
pub use onvif::{
    DeviceInformation, OnvifClient, OnvifCredentials, OnvifProfile, ResolvedDevice,
};
pub use scan::{scan_ports, PortHit};
pub use synology::{SynologyCamera, SynologyClient};
pub use wsdiscovery::{
    build_probe_message, discover_multicast, discover_unicast, host_and_port,
    parse_probe_matches, DiscoveredDevice, DiscoverySource, ONVIF_DEVICE_SERVICE_PATH,
    WS_DISCOVERY_MULTICAST, WS_DISCOVERY_PORT,
};

/// Progress callback invoked while a scan runs.
pub type ProgressCallback = Arc<dyn Fn(DiscoveryEvent) + Send + Sync>;

/// Events emitted during a discovery run.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A phase started, e.g. `"WS-Discovery (multicast)"`.
    Phase(String),
    /// A new ONVIF device answered.
    DeviceFound(DiscoveredDevice),
    /// A TCP port answered.
    PortFound(PortHit),
    /// The run finished.
    Finished { devices: usize, port_hits: usize },
}

/// Result of a full discovery run.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    pub devices: Vec<DiscoveredDevice>,
    /// TCP hits that did not answer WS-Discovery.
    pub port_hits: Vec<PortHit>,
}

impl DiscoveryReport {
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty() && self.port_hits.is_empty()
    }
}

/// Runs the configured discovery strategies.
#[derive(Debug, Clone)]
pub struct DiscoveryService {
    config: DiscoveryConfig,
}

impl DiscoveryService {
    pub fn new(config: DiscoveryConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &DiscoveryConfig {
        &self.config
    }

    /// Executes every enabled strategy and returns the merged report.
    pub async fn discover(&self, progress: Option<ProgressCallback>) -> Result<DiscoveryReport> {
        let notify = |event: DiscoveryEvent| {
            if let Some(callback) = &progress {
                callback(event);
            }
        };

        let timeout = Duration::from_millis(self.config.probe_timeout_ms.max(100));
        let mut report = DiscoveryReport::default();

        if self.config.broadcast {
            notify(DiscoveryEvent::Phase(DiscoverySource::Multicast.label().to_string()));
            match discover_multicast(timeout).await {
                Ok(devices) => {
                    for device in devices {
                        notify(DiscoveryEvent::DeviceFound(device.clone()));
                        report.devices.push(device);
                    }
                }
                Err(err) => tracing::warn!(target: "xgview::discovery", %err, "multicast discovery failed"),
            }
        }

        let targets = parse_targets_multi(&self.config.ip_ranges);
        if self.config.subnet_scan && !targets.is_empty() {
            notify(DiscoveryEvent::Phase(DiscoverySource::Unicast.label().to_string()));
            match discover_unicast(targets.clone(), timeout, self.config.concurrency).await {
                Ok(devices) => {
                    for device in devices {
                        if report
                            .devices
                            .iter()
                            .any(|known| known.host() == device.host())
                        {
                            continue;
                        }
                        notify(DiscoveryEvent::DeviceFound(device.clone()));
                        report.devices.push(device);
                    }
                }
                Err(err) => tracing::warn!(target: "xgview::discovery", %err, "unicast discovery failed"),
            }
        }

        if self.config.tcp_probe && !targets.is_empty() {
            notify(DiscoveryEvent::Phase("TCP port scan".to_string()));
            let hits = scan_ports(
                &targets,
                &self.config.tcp_ports,
                Duration::from_millis(self.config.probe_timeout_ms.min(800).max(100)),
                self.config.concurrency,
            )
            .await;
            for hit in hits {
                // Devices already discovered through ONVIF do not appear twice.
                let known = report.devices.iter().any(|device| {
                    device.host().as_deref() == Some(hit.address.to_string().as_str())
                });
                if known {
                    continue;
                }
                notify(DiscoveryEvent::PortFound(hit.clone()));
                report.port_hits.push(hit);
            }
        }

        report.devices.sort_by_key(|device| device.host());
        report.port_hits.sort_by_key(|hit| (hit.address, hit.port));
        notify(DiscoveryEvent::Finished {
            devices: report.devices.len(),
            port_hits: report.port_hits.len(),
        });
        Ok(report)
    }

    /// ONVIF credentials from the discovery configuration.
    pub fn credentials(&self) -> Option<OnvifCredentials> {
        let username = self.config.onvif_username.clone()?;
        if username.trim().is_empty() {
            return None;
        }
        Some(OnvifCredentials::new(username, self.config.onvif_password.clone().unwrap_or_default()))
    }

    /// Queries a discovered device and builds the matching [`CameraSource`].
    pub async fn import_device(&self, device: &DiscoveredDevice) -> Result<CameraSource> {
        let endpoint = device
            .device_url()
            .ok_or_else(|| CoreError::network("discovered device has no service endpoint"))?;
        let client = OnvifClient::new(endpoint, self.credentials())?;
        let resolved = client.resolve().await?;

        let host = device
            .host()
            .ok_or_else(|| CoreError::network("discovered device has no address"))?;
        let name = device
            .name()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| resolved.device.label());

        let main_uri = resolved
            .main_uri
            .ok_or_else(|| CoreError::parse("device exposes no main stream"))?;

        let mut source = CameraSource::new(name, main_uri);
        source.host = host;
        source.onvif_port = device.port().unwrap_or(80);
        source.vendor = resolved.device.manufacturer.clone();
        source.model = resolved.device.model.clone();
        source.rtsp_sub = resolved.sub_uri.clone();
        source.main_profile = resolved.main_profile.clone();
        source.sub_profile = resolved.sub_profile.clone();
        source.origin = CameraOrigin::Onvif;
        if let Some(credentials) = self.credentials() {
            source.username = Some(credentials.username);
            source.password = Some(credentials.password);
        }
        if source.rtsp_sub.is_none() {
            // No ONVIF sub profile: derive it from the well known URL patterns.
            source.apply_sub_inference();
        }
        Ok(source)
    }

    /// Imports the cameras of a Synology Surveillance Station.
    pub async fn import_synology(&self, config: crate::config::SynologyConfig) -> Result<Vec<CameraSource>> {
        let mut client = SynologyClient::new(config)?;
        let sources = client.import_cameras().await;
        client.logout().await;
        sources
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_report_is_empty() {
        assert!(DiscoveryReport::default().is_empty());
    }

    #[test]
    fn credentials_are_optional() {
        let mut config = DiscoveryConfig::default();
        config.onvif_username = None;
        let service = DiscoveryService::new(config);
        assert!(service.credentials().is_none());
    }
}
