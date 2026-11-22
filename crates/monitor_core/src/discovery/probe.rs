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

use std::time::{Duration, Instant};

use monitor_codec::sps;

use crate::error::{CoreError, Result};
use crate::h264::{nal_unit_type, H264Depacketizer};
use crate::mse::{MseParser, ParsedMessage};
use crate::pipeline::open_mse;
use crate::rtsp::{mask_url, parse_rtp_header, parse_sdp, MediaPacket, RtspClient};

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
            tracing::debug!(
                target: "xgview::discovery",
                url = %mask_url(url),
                %err,
                "resolution probe failed"
            );
            None
        }
        Err(_) => {
            tracing::debug!(
                target: "xgview::discovery",
                url = %mask_url(url),
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
            // The probe reads the video only; a camera that also carries audio
            // is not what it is measuring.
            MediaPacket::AudioRtp(_) | MediaPacket::AudioRtcp(_) => continue,
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

/// Measures the resolution of an MSE (WebSocket) stream, or `None` if it could
/// not be read within `timeout`.
///
/// The init segment go2rtc sends on join is only a *candidate*: it is built
/// before a slow camera has sent its first SPS and can advertise a placeholder
/// 128x96 that is never corrected in a later init. The trustworthy size is the
/// in-band SPS in a media segment - the same SPS the player decodes to learn the
/// real picture size. [`read_mse_size`] therefore reads past the init until an
/// access unit carries an SPS, falling back to the init's declared size only if
/// no media sample answers within the timeout.
pub async fn probe_resolution_mse(
    url: &str,
    credentials: Option<(String, String)>,
    timeout: Duration,
) -> Option<(u32, u32)> {
    match read_mse_size(url, credentials, timeout).await {
        Ok(size) => size,
        Err(err) => {
            tracing::debug!(
                target: "xgview::discovery",
                url = %mask_url(url),
                %err,
                "mse resolution probe failed"
            );
            None
        }
    }
}

/// The size a parsed MSE message reports: an in-band SPS wins, the init
/// segment's declared size is kept as a fallback (`declared`) when no SPS has
/// arrived yet. Kept separate from the socket loop so the choice is testable.
fn mse_size_from_message(
    parsed: &ParsedMessage,
    declared: &mut Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    if declared.is_none() {
        if let Some(init) = &parsed.init {
            *declared = (init.width > 0 && init.height > 0).then_some((init.width, init.height));
        }
    }
    // Access units are Annex-B; the in-band SPS in one of them is the same one
    // the decoder reads to get the real picture size after the placeholder init.
    parsed
        .samples
        .iter()
        .find_map(|sample| sps::access_unit_size(&sample.data))
}

async fn read_mse_size(
    url: &str,
    credentials: Option<(String, String)>,
    timeout: Duration,
) -> Result<Option<(u32, u32)>> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let mut ws = open_mse(url, credentials.as_ref(), timeout).await?;
    let mut parser = MseParser::new();
    let mut declared: Option<(u32, u32)> = None;
    // The deadline is owned here, not by an outer `timeout`, so that running
    // out of time still returns the init's declared size instead of dropping it.
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(declared);
        }
        let message = match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(message)) => message,
            // Socket closed, or no media sample arrived in time: either way the
            // declared size is the best answer left.
            Ok(None) | Err(_) => return Ok(declared),
        };
        let message = message.map_err(|err| CoreError::network(format!("WebSocket read failed: {err}")))?;
        let data = match message {
            Message::Binary(data) => data,
            // The JSON reply confirming the codec; the init segment follows.
            Message::Text(_) => continue,
            Message::Close(_) => return Ok(declared),
            _ => continue,
        };
        let parsed = parser.parse_message(&data)?;
        if let Some(size) = mse_size_from_message(&parsed, &mut declared) {
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
    use crate::mse::{MseInit, MseSample};
    use monitor_codec::Codec;

    fn empty_message(init: Option<MseInit>, samples: Vec<MseSample>) -> ParsedMessage {
        ParsedMessage {
            init,
            audio: None,
            samples,
            audio_samples: Vec::new(),
        }
    }

    fn declared(w: u32, h: u32) -> MseInit {
        MseInit {
            codec: Codec::H264,
            param_sets: Vec::new(),
            width: w,
            height: h,
        }
    }

    /// A front-door access unit: Annex-B SPS (640x480) then PPS.
    fn real_access_unit() -> MseSample {
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(&[
            0x67, 0x64, 0x00, 0x33, 0xac, 0x15, 0x14, 0xa0, 0xa0, 0x3d, 0x90, 0x00,
        ]);
        data.extend_from_slice(&[0, 0, 1, 0x68, 0xee, 0x3c, 0xb0]);
        MseSample {
            data,
            pts_us: 0,
            keyframe: true,
        }
    }

    /// go2rtc's 128x96 placeholder init must not win over a real in-band SPS:
    /// that is the north-passage mispairing this fixes.
    #[test]
    fn in_band_sps_overrides_the_declared_placeholder() {
        let mut declared_size = None;
        let init_msg = empty_message(Some(declared(128, 96)), Vec::new());
        assert!(mse_size_from_message(&init_msg, &mut declared_size).is_none());
        assert_eq!(declared_size, Some((128, 96)), "init kept as fallback");

        let media_msg = empty_message(None, vec![real_access_unit()]);
        assert_eq!(
            mse_size_from_message(&media_msg, &mut declared_size),
            Some((640, 480)),
            "the real SPS wins"
        );
    }

    /// With no media sample yet, the init size stays as the fallback answer.
    #[test]
    fn declared_size_is_kept_until_an_sps_arrives() {
        let mut declared_size = None;
        let msg = empty_message(Some(declared(1920, 1080)), Vec::new());
        assert_eq!(mse_size_from_message(&msg, &mut declared_size), None);
        assert_eq!(declared_size, Some((1920, 1080)));
    }

    #[test]
    fn no_size_anywhere_stays_none() {
        let mut declared_size = None;
        let msg = empty_message(None, Vec::new());
        assert_eq!(mse_size_from_message(&msg, &mut declared_size), None);
        assert_eq!(declared_size, None);
    }

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
