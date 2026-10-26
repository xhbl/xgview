//! Speaks a Foscam camera's own protocol: connect, log in, send a tone.
//!
//! The step-3 tool from `docs/xgview-厂家对讲通道方案.md`: it exercises
//! [`FoscamTalkChannel`] on its own, with no viewer, no RTSP and no interface, so
//! that the protocol can be proved before anything is built on it.
//!
//! ```text
//! cargo run -p monitor_core --example foscam_talk -- 192.168.27.40 admin
//! cargo run -p monitor_core --example foscam_talk -- 192.168.27.40 admin --seconds 10
//! cargo run -p monitor_core --example foscam_talk -- 192.168.27.40 admin --silence
//! ```
//!
//! The password comes from `XGVIEW_TALK_PASSWORD`, so it need not appear in a
//! shell's history or in `ps` output.
//!
//! The tone is 660 Hz at half scale - the same one the standard route's tool
//! uses, so the two are compared by ear on the same camera. `--silence` sends
//! frames of zeros instead, which is what a viewer that is listening but not
//! talking would send: it proves the framing without making a sound.
//!
//! **What the protocol cannot tell you** is whether the credentials were
//! accepted or whether a speaker is connected: a wrong password gets the same
//! answer as the right one, and the camera never acknowledges a frame. The tone
//! is the test, and the ears are the instrument.

use std::time::{Duration, Instant};

use monitor_core::talk::foscam::{FoscamTalkChannel, FRAME_SAMPLES};
use monitor_core::talk::{TalkChannel, RATE};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(host) = args.next() else {
        eprintln!("usage: foscam_talk <host> [user] [--port N] [--seconds N] [--silence]");
        return;
    };
    let mut user = "admin".to_string();
    let mut port: u16 = 88;
    let mut seconds: u64 = 3;
    let mut silent = false;
    let mut rest = args.peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--port" => port = rest.next().and_then(|value| value.parse().ok()).unwrap_or(88),
            "--seconds" => {
                seconds = rest.next().and_then(|value| value.parse().ok()).unwrap_or(3)
            }
            "--silence" => silent = true,
            other if !other.starts_with("--") => user = other.to_string(),
            other => {
                eprintln!("unknown argument: {other}");
                return;
            }
        }
    }
    let password = match std::env::var("XGVIEW_TALK_PASSWORD") {
        Ok(password) if !password.is_empty() => password,
        _ => {
            eprintln!("set XGVIEW_TALK_PASSWORD to the camera's password");
            return;
        }
    };

    let opened = Instant::now();
    let mut channel = match FoscamTalkChannel::open(&host, port, &user, &password).await {
        Ok(channel) => channel,
        Err(err) => {
            eprintln!("the camera would not take the connection: {err}");
            return;
        }
    };
    println!(
        "connected to {host}:{port} in {:?}: {:?} a frame of {} samples, keep alive {:?}",
        opened.elapsed(),
        channel.interval(),
        channel.samples(),
        // The channel has no keep-alive of its own; every frame is one.
        "none"
    );

    let interval = channel.interval();
    let hold = Duration::from_secs(seconds);
    let started = Instant::now();
    let mut next = Instant::now();
    let mut phase = 0.0f64;
    let mut frames = 0u32;
    while started.elapsed() < hold {
        let samples: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|_| {
                if silent {
                    return 0;
                }
                phase += 660.0 / f64::from(RATE);
                (f64::sin(phase * std::f64::consts::TAU) * 16_384.0) as i16
            })
            .collect();
        if let Err(err) = channel.send(&samples) {
            println!("sent {frames} frames before the connection broke: {err}");
            return;
        }
        frames += 1;
        // A camera that hangs up is only visible as a read that returns nothing,
        // which is what the viewer's loop does between frames.
        if let Err(err) = tokio::time::timeout(Duration::from_millis(1), channel.watch()).await
            .unwrap_or(Ok(()))
        {
            println!("sent {frames} frames before the camera closed the call: {err}");
            return;
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        }
    }
    println!("sent {frames} frames over {seconds} s, the connection held");

    let closing = Instant::now();
    channel.close().await;
    println!("closed in {:?}", closing.elapsed());
}
