//! Asks a camera for its back channel without setting up the picture, and then
//! opens a session that carries the back channel alone.
//!
//! The two questions this answers, in the order the plan asks them:
//!
//! 1. **Does the probe work, and what does it cost?** `probe_backchannel` is
//!    `OPTIONS` + `DESCRIBE` and nothing else - no `SETUP`, so no session is
//!    kept on the server and the connection closing is the whole of the
//!    cleanup. Run with `--probe-only` to ask and leave.
//! 2. **Will a camera take a session that carries only the talk-back media?**
//!    That is the assumption the whole plan rests on: if a camera answers the
//!    track's `SETUP` and the `PLAY` that follows, the picture's session never
//!    has to carry a back channel at all.
//!
//! `--with-video` adds the third question: a second session holds the picture
//! the whole time, which is the arrangement the plan proposes. The two never
//! meet, so what this measures is whether the camera agrees - a device with a
//! session limit would drop the picture the moment a call opened.
//!
//! ```text
//! cargo run --example talk_only -- rtsp://192.168.17.88:8554/cam_doorbell_1 admin
//! cargo run --example talk_only -- rtsp://192.168.27.49:554/Preview_01_main admin --probe-only
//! cargo run --example talk_only -- rtsp://192.168.27.49:554/Preview_01_main admin --with-video
//! ```
//!
//! The password comes from the third argument or, when it is left out, from
//! `XGVIEW_TALK_PASSWORD` - so it need not appear in a shell's history or in
//! `ps` output.
//!
//! The tone is 660 Hz at half scale for two seconds, then a second of silence:
//! audible from across a room, and not loud enough to be unpleasant next to the
//! camera.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use monitor_core::rtsp::{MediaPacket, RtspClient};
use monitor_core::talk::{TalkStream, FRAME_SAMPLES, RATE};

#[tokio::main]
async fn main() {
    // `--seconds N` holds the tone for longer than the default three seconds,
    // which is what a person standing next to the camera needs.
    let mut probe_only = false;
    let mut with_video = false;
    let mut seconds = 3u64;
    let mut positional = Vec::new();
    let mut rest = std::env::args().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--probe-only" => probe_only = true,
            "--with-video" => with_video = true,
            "--seconds" => seconds = rest.next().and_then(|value| value.parse().ok()).unwrap_or(3),
            _ => positional.push(arg),
        }
    }

    let mut positional = positional.into_iter();
    let Some(url) = positional.next() else {
        eprintln!(
            "usage: talk_only <rtsp url> [username] [password] \
             [--probe-only] [--with-video] [--seconds N]"
        );
        return;
    };
    let username = positional.next();
    let password = positional
        .next()
        .or_else(|| std::env::var("XGVIEW_TALK_PASSWORD").ok());
    let credentials = username.map(|user| (user, password.unwrap_or_default()));

    // 1. The probe. Its own connection, dropped the moment the answer is in.
    let asked = Instant::now();
    let mut prober = match RtspClient::connect_with_auth(&url, credentials.clone()).await {
        Ok(client) => client,
        Err(err) => {
            eprintln!("connect failed: {err}");
            return;
        }
    };
    match prober.probe_backchannel().await {
        Ok(Some(encoding)) => {
            println!("probe: back channel offered as {encoding} ({:?})", asked.elapsed())
        }
        Ok(None) => {
            println!("probe: no back channel offered ({:?})", asked.elapsed());
            return;
        }
        Err(err) => {
            eprintln!("probe failed: {err}");
            return;
        }
    }
    drop(prober);
    if probe_only {
        println!("probe only: the connection is closed, nothing to tear down");
        return;
    }

    // 2. The picture, when asked for: its own session, reading flat out on a
    // task of its own so that the call's 20 ms clock cannot starve it.
    let picture = if with_video {
        match open_picture(&url, credentials.clone()).await {
            Ok(client) => Some(Picture::start(client)),
            Err(err) => {
                eprintln!("the picture session failed: {err}");
                return;
            }
        }
    } else {
        None
    };
    if let Some(picture) = picture.as_ref() {
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("picture before the call: {} packets in 1 s", picture.count());
    }

    // 3. A session of its own, carrying the back channel and nothing else.
    let mut client = match RtspClient::connect_with_auth(&url, credentials).await {
        Ok(client) => client,
        Err(err) => {
            eprintln!("reconnect failed: {err}");
            return;
        }
    };
    let opened = Instant::now();
    let track = match client.start_talk_only().await {
        Ok(Some(track)) => track,
        Ok(None) => {
            eprintln!("the talk-only session found no back channel in the answer");
            return;
        }
        Err(err) => {
            eprintln!("the talk-only session was refused: {err}");
            return;
        }
    };
    println!(
        "talk-only session live in {:?}: interleaved channel {}, encoding {:?}",
        opened.elapsed(),
        track.channel,
        track.encoding
    );

    let codec =
        monitor_codec::audio::AudioCodec::from_encoding(track.encoding.as_deref().unwrap_or("PCMU"));
    let Some(mut stream) = TalkStream::new(codec) else {
        eprintln!("the back channel offers {codec}, which this build cannot encode");
        return;
    };

    // A continuous tone for as long as `--seconds` asks - three seconds by
    // default, which is enough for a measurement, and minutes when a person is
    // walking over to the camera to listen to it. The cadence is the media's:
    // 20 ms of 8 kHz audio a packet, silence included.
    let before = picture.as_ref().map_or(0, Picture::count);
    let interval = Duration::from_millis(20);
    let hold = Duration::from_secs(seconds);
    let mut phase = 0.0f64;
    let mut frames = 0u32;
    let mut announced = 0u64;
    let first_packet = Instant::now();
    let started = Instant::now();
    let mut next = Instant::now();
    while started.elapsed() < hold {
        let samples: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|_| {
                phase += 660.0 / f64::from(RATE);
                (f64::sin(phase * std::f64::consts::TAU) * 16_384.0) as i16
            })
            .collect();
        let packet = stream.packet(&samples);
        client.send_interleaved(track.channel, &packet);
        frames += 1;
        if frames == 1 {
            println!("first packet written after {:?}", first_packet.elapsed());
        }
        if seconds > 5 && started.elapsed().as_secs() / 5 > announced {
            announced = started.elapsed().as_secs() / 5;
            println!("  {} s, {frames} frames sent", announced * 5);
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        }
    }
    println!("sent {frames} frames over {seconds} s");

    let closing = Instant::now();
    client.teardown().await;
    println!("teardown took {:?}", closing.elapsed());

    if let Some(picture) = picture.as_ref() {
        let during = picture.count() - before;
        println!("picture during the call: {during} packets in {seconds} s");
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("picture after the call: {} packets in 1 s", picture.count() - before - during);
        println!("picture session died during the run: {}", picture.died());
    }
}

/// A session that pulls the picture and asks for no back channel at all - the
/// shape the plan gives every tile.
async fn open_picture(
    url: &str,
    credentials: Option<(String, String)>,
) -> Result<RtspClient, String> {
    let mut client = RtspClient::connect_with_auth(url, credentials)
        .await
        .map_err(|err| err.to_string())?;
    client.start().await.map_err(|err| err.to_string())?;
    Ok(client)
}

/// The picture's session, read on a task of its own.
///
/// It has to be its own task: a stream of this size is hundreds of packets a
/// second, and a reader that only took one between two 20 ms talk packets would
/// fill its socket buffer and stall the picture - which looks exactly like the
/// camera having stopped it, and is not.
struct Picture {
    packets: Arc<AtomicU64>,
    died: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Picture {
    fn start(mut client: RtspClient) -> Self {
        let packets = Arc::new(AtomicU64::new(0));
        let died = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let counter = packets.clone();
        let died_flag = died.clone();
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            while !stopping.load(Ordering::Relaxed) {
                match tokio::time::timeout(Duration::from_millis(500), client.read_media()).await {
                    Ok(Ok(MediaPacket::Rtp(_))) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => {
                        // The session ended under us: the socket closed, or the
                        // camera took it away. Either is the answer to the
                        // question this mode exists for.
                        died_flag.store(true, Ordering::Relaxed);
                        return;
                    }
                    Err(_) => {}
                }
            }
            // Dropping the client closes the socket; the camera times the
            // session out on its own, which is what a killed tile does too.
        });
        Self { packets, died, stop, task }
    }

    fn count(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }

    fn died(&self) -> bool {
        self.died.load(Ordering::Relaxed)
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}
