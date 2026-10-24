//! A smoke probe for the audio path: connects to an RTSP stream, plays nothing,
//! and counts the audio packets decoded into PCM over a couple of seconds.
//!
//! It is a manual check, run against the go2rtc restream or a camera directly:
//!
//! ```text
//! cargo run -p monitor_core --example audio_probe -- rtsp://192.168.17.88:8554/cam_driveway_1
//! ```
//!
//! It prints the audio track the SDP advertised and how many RTP packets decoded
//! into how many PCM samples, which is enough to tell the whole path - SETUP,
//! the audio channel, the depacketiser and the decoder - apart from a silent one.

use std::time::{Duration, Instant};

use monitor_codec::audio::{create_audio_decoder, AudioCodec, AudioDecoder, AudioStreamInfo};

use monitor_core::audio::{aac_access_units, AacRtpParams};
use monitor_core::rtsp::{parse_rtp_header, MediaPacket, RtspClient};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).expect("usage: audio_probe <rtsp url>");
    let seconds: u64 = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(2);

    let mut client = RtspClient::connect(&url).await?;
    let tracks = client.start().await?;
    for (i, track) in tracks.iter().enumerate() {
        println!(
            "track {i}: media={} encoding={:?} rate={:?} channels={:?} sendonly={}",
            track.media,
            track.encoding,
            track.clock_rate,
            track.channels,
            track.is_sendonly()
        );
    }

    let Some(track) = tracks.iter().find(|track| track.is_audio() && !track.is_sendonly()) else {
        println!("no audio track the client receives");
        return Ok(());
    };

    let named = AudioCodec::from_encoding(track.encoding.as_deref().unwrap_or_default());
    let codec = if named == AudioCodec::Unknown {
        track.payload_type.and_then(AudioCodec::from_static_payload_type).unwrap_or(named)
    } else {
        named
    };
    let info = AudioStreamInfo::new(codec, track.clock_rate.unwrap_or(8000), track.channels.unwrap_or(1));
    println!("audio: codec={} rate={} channels={}", codec, info.sample_rate, info.channels);
    let aac = AacRtpParams::from_fmtp(track.fmtp.as_deref());

    let mut decoder: Box<dyn AudioDecoder> = create_audio_decoder(codec);
    decoder.configure(&info)?;

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut packets = 0usize;
    let mut samples = 0usize;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(600), client.read_media()).await {
            Ok(Ok(MediaPacket::AudioRtp(payload))) => {
                if let Some(header) = parse_rtp_header(&payload) {
                    packets += 1;
                    let body = &payload[header.header_len..];
                    let mut pcm = Vec::new();
                    if codec == AudioCodec::Aac {
                        for range in aac_access_units(body, aac.size_length, aac.index_length) {
                            let _ = decoder.decode(&body[range], &mut pcm);
                        }
                    } else {
                        let _ = decoder.decode(body, &mut pcm);
                    }
                    samples += pcm.len();
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    println!("decoded: {packets} audio packets -> {samples} PCM samples in {seconds}s");
    Ok(())
}