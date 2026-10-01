use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;

/// A reachable TCP port found by the fallback scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHit {
    pub address: Ipv4Addr,
    pub port: u16,
    /// Detected service, e.g. `rtsp` when the port answered an RTSP OPTIONS
    /// request. `None` means "port open but service unknown".
    pub service: Option<String>,
}

impl PortHit {
    pub fn is_rtsp(&self) -> bool {
        self.service.as_deref() == Some("rtsp")
    }
}

/// Concurrently probes `ports` on every address of `targets`.
///
/// The scan is the last resort for devices that never answer WS-Discovery:
/// an open 554 usually means an RTSP camera behind a non ONVIF firmware.
pub async fn scan_ports(
    targets: &[Ipv4Addr],
    ports: &[u16],
    timeout: Duration,
    concurrency: usize,
) -> Vec<PortHit> {
    let concurrency = concurrency.max(1);
    let mut hits = Vec::new();
    let mut workers: JoinSet<Vec<PortHit>> = JoinSet::new();
    let mut pending = Vec::with_capacity(targets.len() * ports.len());
    for address in targets {
        for port in ports {
            pending.push((*address, *port));
        }
    }

    let mut queue = pending.into_iter();
    for _ in 0..concurrency {
        match queue.next() {
            Some((address, port)) => {
                workers.spawn(async move { probe(address, port, timeout).await });
            }
            None => break,
        }
    }

    while let Some(result) = workers.join_next().await {
        match result {
            Ok(mut found) => hits.append(&mut found),
            Err(err) => tracing::debug!(target: "xgview::discovery", %err, "scan worker failed"),
        }
        if let Some((address, port)) = queue.next() {
            workers.spawn(async move { probe(address, port, timeout).await });
        }
    }

    hits.sort_by_key(|hit| (hit.address, hit.port));
    hits
}

async fn probe(address: Ipv4Addr, port: u16, timeout: Duration) -> Vec<PortHit> {
    let target = (address, port);
    let connect = tokio::time::timeout(timeout, TcpStream::connect(target)).await;
    let Ok(Ok(mut stream)) = connect else {
        return Vec::new();
    };

    let service = if port == 554 {
        rtsp_banner(&mut stream, address, timeout).await
    } else {
        None
    };

    vec![PortHit { address, port, service }]
}

/// Sends a minimal `OPTIONS` request to confirm the peer really speaks RTSP.
async fn rtsp_banner(stream: &mut TcpStream, address: Ipv4Addr, timeout: Duration) -> Option<String> {
    let request = format!("OPTIONS rtsp://{address}/ RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: XGView\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return None;
    }
    let mut buffer = [0u8; 128];
    let read = tokio::time::timeout(timeout, stream.read(&mut buffer)).await.ok()?.ok()?;
    let response = String::from_utf8_lossy(&buffer[..read]);
    if response.starts_with("RTSP/") {
        Some("rtsp".to_string())
    } else {
        Some("tcp".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closed_port_is_not_reported() {
        // Port 1 is never a listening service in a test environment.
        let hits = scan_ports(
            &[Ipv4Addr::new(127, 0, 0, 1)],
            &[1],
            Duration::from_millis(80),
            4,
        )
        .await;
        assert!(hits.is_empty());
    }
}
