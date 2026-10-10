//! Measures the resolution of an RTSP stream.
//!
//! Frigate's `/api/config` and go2rtc's `/api/streams` both hand out a list of
//! streams with no resolution in it, and neither a stream's name nor Frigate's
//! `detect` / `record` roles say which of two streams is the main one and which
//! is the sub. The one reliable fact is the picture size, so both importers
//! measure it here rather than guess.
//!
//! The size comes from the stream's sequence parameter set. A `DESCRIBE` whose
//! SDP advertises `sprop-parameter-sets` answers without a packet being read;
//! otherwise the SPS arrives with the next IDR, so the wait is bounded by the
//! camera's group of pictures, not by anything the client controls. A probe that
//! does not answer in `timeout` yields `None`, which callers treat as "no sub
//! stream" - the safe direction, since an empty sub falls back to the main
//! stream.

use std::time::Duration;

use monitor_codec::sps;

use crate::error::Result;
use crate::h264::{nal_unit_type, H264Depacketizer};
use crate::rtsp::{parse_rtp_header, parse_sdp, MediaPacket, RtspClient};

/// Measures the resolution of an RTSP stream, or `None` if it could not be read
/// within `timeout`.
pub async fn probe_resolution(
    url: &str,
    credentials: Option<(String, String)>,
    timeout: Duration,
) -> Option<(u32, u32)> {
    match tokio::time::timeout(timeout, read_resolution(url, credentials)).await {
        Ok(Ok(size)) => size,
        Ok(Err(err)) => {
            tracing::debug!(target: "xgview::discovery", %url, %err, "resolution probe failed");
            None
        }
        Err(_) => {
            tracing::debug!(
                target: "xgview::discovery",
                %url,
                timeout_s = timeout.as_secs(),
                "resolution probe timed out"
            );
            None
        }
    }
}

async fn read_resolution(
    url: &str,
    credentials: Option<(String, String)>,
) -> Result<Option<(u32, u32)>> {
    let mut client = RtspClient::connect_with_auth(url, credentials).await?;
    client.options().await?;
    let sdp = client.describe().await?;
    let tracks = parse_sdp(&sdp);
    let Some(video) = tracks.iter().find(|track| track.is_video()) else {
        return Ok(None);
    };

    // A `DESCRIBE` that already advertises the parameter sets answers without a
    // packet being read - which is most of them.
    let mut depacketizer = H264Depacketizer::new(video.payload_type, video.parameter_sets.clone());
    if let Some(size) = size_from_sets(&depacketizer) {
        return Ok(Some(size));
    }

    let control = video.control.clone().unwrap_or_default();
    client.setup_interleaved(&control, 0).await?;
    client.play("npt=0.000-").await?;
    loop {
        let payload = match client.read_media().await? {
            MediaPacket::Rtp(payload) => payload,
            MediaPacket::Rtcp(_) => continue,
        };
        let Some(header) = parse_rtp_header(&payload) else { continue };
        // The depacketizer takes the RTP payload, not the whole packet: the
        // 12 byte header in front of it reads as a NAL header otherwise.
        depacketizer.push(&header, &payload[header.header_len..]);
        if let Some(size) = size_from_sets(&depacketizer) {
            return Ok(Some(size));
        }
    }
}

/// The picture size a stream's cached sequence parameter set describes.
fn size_from_sets(depacketizer: &H264Depacketizer) -> Option<(u32, u32)> {
    depacketizer
        .parameter_sets()
        .find(|nal| nal_unit_type(nal[0]) == 7)
        .and_then(sps::picture_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_parameter_set_means_no_size() {
        let depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        assert!(size_from_sets(&depacketizer).is_none());
    }

    /// Only a sequence parameter set carries a size: a cached picture parameter
    /// set must not be read as one.
    #[test]
    fn a_picture_parameter_set_is_not_read_as_a_sequence_parameter_set() {
        let pps = [0x68u8, 0xce, 0x38, 0x80];
        let depacketizer = H264Depacketizer::new(Some(96), vec![pps.to_vec()]);
        assert!(size_from_sets(&depacketizer).is_none());
    }
}
