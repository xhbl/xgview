//! Plays a camera's audio through the real sink for a few seconds, then exits.
//!
//! This is the exit-path repro: it keeps the cpal stream and the FFmpeg decoder
//! live and busy, then lets the process exit - which is where the viewer used to
//! die with a stack buffer overrun instead of exiting. Run it against a camera:
//!
//! ```text
//! cargo run -p monitor_gui --example audio_live -- rtsp://admin:pass@192.168.17.88:8561/doorbell 3
//! ```
//!
//! and read the exit code; `0` is the whole point.
//!
//! The sink is opened and shut down on the main thread, like the real viewer
//! (`audio::shutdown` after `run_native`), because the cpal stream lives in a
//! thread-local and only that thread's destructor is the dangerous one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use monitor_codec::audio::{create_audio_decoder, AudioCodec, AudioDecoder, AudioStreamInfo};

use monitor_core::audio::{aac_access_units, AacRtpParams, AudioOutput};
use monitor_core::rtsp::{parse_rtp_header, MediaPacket, RtspClient};
use monitor_gui::audio::CpalOutput;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1).expect("usage: audio_live <rtsp url> [seconds]");
    let seconds: u64 = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(3);

    // The system's default device: this is the exit-path repro, not a way to
    // choose one.
    let sink = CpalOutput::open(None).expect("no audio device on this machine");
    sink.set_active(true);

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(play(&url, seconds, sink.clone()));

    // The window is gone, so the sound goes with it - let the stream go here, on
    // the thread that opened it, while the process is still healthy. See
    // `audio::shutdown`.
    monitor_gui::audio::shutdown();

    result
}

async fn play(url: &str, seconds: u64, sink: Arc<CpalOutput>) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = RtspClient::connect(url).await?;
    let tracks = client.start().await?;
    let track = tracks
        .iter()
        .find(|track| track.is_audio() && !track.is_sendonly())
        .expect("no audio track the client receives");
    let named = AudioCodec::from_encoding(track.encoding.as_deref().unwrap_or_default());
    let codec = if named == AudioCodec::Unknown {
        track.payload_type.and_then(AudioCodec::from_static_payload_type).unwrap_or(named)
    } else {
        named
    };
    let info = AudioStreamInfo::new(codec, track.clock_rate.unwrap_or(8000), track.channels.unwrap_or(1));
    eprintln!("audio: {codec} {} Hz {} ch", info.sample_rate, info.channels);
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
                    if !pcm.is_empty() {
                        sink.configure(&info);
                        sink.push(&pcm);
                        samples += pcm.len();
                    }
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }
    eprintln!("played {packets} packets -> {samples} samples; exiting now");
    Ok(())
}