//! A tone sent to a camera's speaker, to find out whether anything is behind it.
//!
//! The one thing no probe can answer. A camera accepts audio whether or not a
//! speaker is connected to its jack, and the protocols say nothing about what
//! came out the other end - so the only instrument is a person's ears, and this
//! is what gives them something to hear.
//!
//! It is written above the protocol on purpose: it asks the camera who it is
//! with the same probe a call relies on, decides the route with the same
//! [`route_from`], opens the channel through the same [`Channel::open`], and
//! sends the same packets at the same cadence. A tone that is heard therefore
//! says the route, the login, the framing and the speaker are all right, and one
//! that is not narrows the fault to one of them. A new maker's protocol needs
//! nothing here.

use std::time::{Duration, Instant};

use monitor_codec::audio::AudioCodec;

use super::{route_from, Channel, TalkStream, RATE};
use crate::model::TalkMode;
use crate::rtsp::RtspClient;

/// How long the tone is held. Long enough to walk to a camera that is nearby and
/// still hear it, short enough that nobody has to wait for the button.
const HOLD: Duration = Duration::from_millis(1_500);

/// The note itself: a fifth above middle C, at half scale - audible across a
/// room, and not unpleasant next to the camera.
const TONE_HZ: f64 = 660.0;
const LEVEL: f64 = 16_384.0;

/// Why a test tone did not play.
#[derive(Debug, Clone)]
pub enum ToneFailure {
    /// The camera's setting says it is not to be talked to at all.
    Off,
    /// The camera offers nothing this build can speak to.
    NoRoute,
    /// The channel could not be opened, or broke while the tone was going.
    Failed(String),
}

/// Plays a short tone to a camera's speaker, the way a call would.
pub async fn test_tone(
    uri: &str,
    credentials: Option<(String, String)>,
    mode: TalkMode,
) -> Result<(), ToneFailure> {
    if mode == TalkMode::Off {
        return Err(ToneFailure::Off);
    }
    // The probe is what a call does first, and it is the only way to learn the
    // maker - which is what decides the route.
    let probe = {
        let mut client = RtspClient::connect_with_auth(uri, credentials.clone())
            .await
            .map_err(|err| ToneFailure::Failed(err.to_string()))?;
        client
            .probe_backchannel()
            .await
            .map_err(|err| ToneFailure::Failed(err.to_string()))?
    };
    let encodable = probe
        .encoding
        .as_deref()
        .map(|encoding| TalkStream::new(AudioCodec::from_encoding(encoding)).is_some())
        .unwrap_or(false);
    let route = route_from(mode, probe.realm.as_deref(), encodable);
    if !route.reachable() {
        return Err(ToneFailure::NoRoute);
    }
    let mut channel = Channel::open(route, uri, credentials)
        .await
        .map_err(|err| ToneFailure::Failed(err.to_string()))?
        .ok_or(ToneFailure::NoRoute)?;

    let interval = channel.interval();
    let mut next = Instant::now();
    let mut phase = 0.0f64;
    let started = Instant::now();
    while started.elapsed() < HOLD {
        let samples: Vec<i16> = (0..channel.samples())
            .map(|_| {
                phase += TONE_HZ / f64::from(RATE);
                (f64::sin(phase * std::f64::consts::TAU) * LEVEL) as i16
            })
            .collect();
        if let Err(err) = channel.send(&samples) {
            channel.close().await;
            return Err(ToneFailure::Failed(err.to_string()));
        }
        // A write into a socket the camera has closed still succeeds, so the
        // connection being alive is read from the socket, not from the writes.
        if let Ok(Err(err)) = tokio::time::timeout(Duration::from_millis(1), channel.watch()).await {
            channel.close().await;
            return Err(ToneFailure::Failed(err.to_string()));
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        }
    }
    channel.close().await;
    Ok(())
}
