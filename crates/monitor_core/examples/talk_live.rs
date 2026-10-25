//! Plays a tone into a camera's speaker, through the RTSP back channel.
//!
//! The talk-back equivalent of `audio_live`: it opens the same kind of session
//! the viewer opens - asking for the back channel - and writes PCMU packets to
//! the media the answer offers. What comes out of the camera proves the whole
//! path: the `DESCRIBE` that carried `backchannel=1`, the `SETUP` of the
//! `sendonly` track, and the RTP framing on the confirmed interleaved channel.
//!
//! ```text
//! cargo run --example talk_live -- rtsp://192.168.17.88:8554/cam_north_passage_1 admin password
//! ```
//!
//! The tone is 660 Hz at half scale for two seconds, then a second of silence:
//! audible from across a room, and not loud enough to be unpleasant next to the
//! camera.

use std::time::{Duration, Instant};

use monitor_core::rtsp::RtspClient;
use monitor_core::talk::{TalkHub, TalkStream, FRAME_SAMPLES, RATE};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(url) = args.next() else {
        eprintln!("usage: talk_live <rtsp url> [username] [password]");
        return;
    };
    let credentials = match (args.next(), args.next()) {
        (Some(user), Some(password)) => Some((user, password)),
        _ => None,
    };

    let mut client = match RtspClient::connect_with_auth(&url, credentials).await {
        Ok(client) => client,
        Err(err) => {
            eprintln!("connect failed: {err}");
            return;
        }
    };
    let tracks = match client.start_with_options(monitor_core::model::RtspTransport::Tcp, true).await {
        Ok(tracks) => tracks,
        Err(err) => {
            eprintln!("handshake failed: {err}");
            return;
        }
    };
    let back = tracks.iter().find(|track| track.is_audio() && track.is_sendonly());
    println!("tracks: {tracks:?}");
    println!("back channel media: {back:?}");
    let Some(channel) = client.talk_rtp_channel() else {
        eprintln!("this session offers no back channel");
        return;
    };
    println!("writing talk-back on interleaved channel {channel}");

    let codec = back
        .and_then(|track| track.encoding.as_deref())
        .unwrap_or("PCMU");
    let codec = monitor_codec::audio::AudioCodec::from_encoding(codec);
    let Some(mut stream) = TalkStream::new(codec) else {
        eprintln!("the back channel offers {codec}, which this build cannot encode");
        return;
    };

    // Two seconds of tone, then a second of silence, then stop.
    let hub = TalkHub::new();
    hub.set_talking(true);
    let mut phase = 0.0f64;
    let mut frames = 0u32;
    let started = Instant::now();
    let mut next = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        let samples: Vec<i16> = if started.elapsed() < Duration::from_secs(2) {
            (0..FRAME_SAMPLES)
                .map(|_| {
                    phase += 660.0 / f64::from(RATE);
                    (f64::sin(phase * std::f64::consts::TAU) * 16_384.0) as i16
                })
                .collect()
        } else {
            vec![0i16; FRAME_SAMPLES]
        };
        hub.push(&samples);
        let packet = stream.packet(&samples);
        client.send_interleaved(channel, &packet);
        frames += 1;
        next += Duration::from_millis(20);
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        }
    }
    println!("sent {frames} frames");
    client.teardown().await;
}
