//! Talk-back: what the viewer says to the camera it has magnified.
//!
//! The playback side ([`crate::audio`]) carries a camera's microphone to the
//! speaker; this is the other direction, and it exists only while one camera is
//! magnified. The audio travels on a back channel the session asked for - the
//! `a=sendonly` media of the camera's own session description, which a camera
//! implementing ONVIF Profile T offers and which go2rtc adds to a `DESCRIBE`
//! carrying `backchannel=1` (see [`crate::rtsp`]).
//!
//! Three things meet here, and each lives in its own file:
//!
//! - [`TalkHub`], in this module, is the microphone side: the interface captures
//!   it at whatever rate the device runs at and pushes 8 kHz mono samples in;
//!   whoever owns a channel takes them out. It is the only thing the two sides
//!   share, and it is shared behind atomics and one short lock - the audio
//!   callback must never wait for the network, and the network task must never
//!   wait for the audio callback.
//! - [`rtsp`] is the channel a camera's own back channel is spoken on: RTP and
//!   G.711 over the RTSP session's interleaved transport.
//! - [`foscam`] is where a camera that has no such channel will be reached - its
//!   own protocol over TCP - and is empty until that work starts.
//!
//! [`TalkChannel`] is the seam between the two: whatever a camera needs, the
//! loop above it only asks how often to send, how much, and sends. A camera's
//! cadence is its own - 20 ms of 8 kHz audio is the RTSP one, and the Foscam
//! protocol wants 60 ms - so neither number is allowed to live in a loop.
//!
//! Only one camera can be talked to at a time, and it is the magnified one, so
//! the hub has one target rather than a channel per camera - the same rule the
//! playback side follows.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Result;

pub mod foscam;
pub mod rtsp;

pub use rtsp::{
    talk_gap, RtspTalkChannel, TalkStream, FRAME_SAMPLES, TALK_INTERVAL, TALK_KEEPALIVE,
};

/// How long a "no" is trusted.
///
/// A camera does not grow a speaker inside a run, but a firmware update or a
/// different account can change the answer, and asking again costs one short
/// connection. A "yes" is kept for the whole run: it is the answer the picture
/// depends on, and it is the one that cannot be stale in a way that matters.
pub const UNSUPPORTED_TTL: Duration = Duration::from_secs(600);

/// How long after a camera is magnified its capability is asked for.
///
/// Magnifying is also when the picture's session is reopened for the other
/// stream, and a camera with a tight limit on concurrent sessions should not
/// see both arrive at once.
pub const PROBE_DELAY: Duration = Duration::from_secs(1);

/// Sample rate of the back channel. Every camera measured takes 8 kHz mono,
/// and speech needs nothing more.
pub const RATE: u32 = 8_000;

/// How much audio the hub may hold before the oldest samples are dropped.
///
/// Half a second: enough to ride out a scheduling hiccup on the way to the
/// socket, short enough that the sound the viewer hears on the camera is not
/// half a second behind their voice.
const QUEUE_SAMPLES: usize = (RATE as usize) / 2;

/// The value [`TalkHub::target`] carries when nothing is magnified.
const NO_TARGET: usize = usize::MAX;

/// Whether a camera can be talked to, as far as this process knows.
///
/// The question is the camera's, not a session's: the picture's session never
/// asks for a back channel, so there is nothing in it to read the answer from.
/// A probe asks on its own connection - `OPTIONS` and `DESCRIBE`, no `SETUP` -
/// and what comes back is cached against the address a call would be placed to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TalkSupport {
    /// Not asked yet, or asked and the answer was a failure rather than a
    /// refusal. Nothing is drawn on this.
    #[default]
    Unknown,
    /// The camera offers a back channel, and it carries an encoding this build
    /// can send.
    Supported,
    /// The camera answered, and the answer was no: no such media, a header it
    /// will not take, or a codec this build cannot encode.
    Unsupported,
}

/// One channel to one camera, as the loop that feeds it sees it.
///
/// A camera's cadence and its encoding are its own business: the loop asks how
/// often and how much, then hands over that many samples of 8 kHz mono audio.
/// Nothing above this trait knows whether they leave as G.711 over an RTSP
/// session or as raw PCM over a socket of the camera's own.
pub trait TalkChannel: Send {
    /// How often the camera's speaker wants a packet.
    fn interval(&self) -> Duration;

    /// How many samples one packet carries.
    fn samples(&self) -> usize;

    /// Sends one packet. `samples` is exactly [`TalkChannel::samples`] long.
    ///
    /// Synchronous on purpose: this is called from the loop that also reads the
    /// picture, and an `await` here would put the network between two frames. An
    /// implementation that cannot write right now drops the packet - audio is
    /// worth less than the picture, which is the rule the outbox already
    /// follows.
    fn send(&mut self, samples: &[i16]) -> Result<()>;
}

/// The microphone side of talk-back, shared between the interface and the
/// channel that carries it to the camera.
#[derive(Debug, Default)]
pub struct TalkHub {
    /// The channel the viewer is magnified on, or [`NO_TARGET`].
    target: AtomicUsize,
    /// Whether the viewer has the microphone switched on.
    talking: AtomicBool,
    /// Audio captured but not yet sent, oldest first.
    pcm: Mutex<VecDeque<i16>>,
}

impl TalkHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Names the channel that may be talked to, `None` when nothing is
    /// magnified. Switching the target stops any talk in progress: the viewer
    /// was talking to the camera they just left.
    pub fn set_target(&self, index: Option<usize>) {
        let index = index.unwrap_or(NO_TARGET);
        if self.target.swap(index, Ordering::Relaxed) != index {
            self.set_talking(false);
        }
    }

    /// The channel that may be talked to, `None` when nothing is magnified.
    pub fn target(&self) -> Option<usize> {
        match self.target.load(Ordering::Relaxed) {
            NO_TARGET => None,
            index => Some(index),
        }
    }

    /// Switches the microphone on or off. Turning it off drops whatever was
    /// captured but not yet sent, so a later switch-on does not start with the
    /// tail of the previous sentence.
    pub fn set_talking(&self, on: bool) {
        if !on {
            if let Ok(mut queue) = self.pcm.lock() {
                queue.clear();
            }
        }
        self.talking.store(on, Ordering::Relaxed);
    }

    pub fn is_talking(&self) -> bool {
        self.talking.load(Ordering::Relaxed)
    }

    /// Whether this channel should be sending the microphone right now.
    pub fn talking_on(&self, index: usize) -> bool {
        self.target() == Some(index) && self.is_talking()
    }

    /// Adds captured audio, dropping the oldest when the queue is full.
    pub fn push(&self, samples: &[i16]) {
        let Ok(mut queue) = self.pcm.lock() else { return };
        if samples.len() >= QUEUE_SAMPLES {
            queue.clear();
            queue.extend(samples[samples.len() - QUEUE_SAMPLES..].iter().copied());
            return;
        }
        let overflow = queue.len() + samples.len();
        if overflow > QUEUE_SAMPLES {
            let drop = (overflow - QUEUE_SAMPLES).min(queue.len());
            queue.drain(..drop);
        }
        queue.extend(samples.iter().copied());
    }

    /// Takes `n` samples, padded with silence when the microphone has not
    /// delivered that many yet.
    ///
    /// The count is the channel's to name: a packet is as long as the camera
    /// wants it, and nothing in the hub knows what that is.
    pub fn take(&self, n: usize) -> Vec<i16> {
        let mut frame = vec![0i16; n];
        let Ok(mut queue) = self.pcm.lock() else { return frame };
        let take = queue.len().min(n);
        for (slot, sample) in frame.iter_mut().zip(queue.drain(..take)) {
            *slot = sample;
        }
        frame
    }

    /// Keeps only the newest `keep` samples.
    ///
    /// A call takes a handshake to come up while the microphone is already
    /// pushing, and the queue is half a second deep: those samples are the only
    /// thing between the viewer's voice and the camera's speaker, and once the
    /// channel is live the two run at the same rate, so the backlog would stay
    /// for the whole call. Trimming it on the way in trades the first words of a
    /// call - which nobody could have heard anyway - for every later one
    /// arriving on time.
    pub fn trim(&self, keep: usize) {
        let Ok(mut queue) = self.pcm.lock() else { return };
        let drop = queue.len().saturating_sub(keep);
        queue.drain(..drop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_keeps_only_the_newest_half_second() {
        let hub = TalkHub::new();
        hub.push(&vec![1i16; QUEUE_SAMPLES]);
        hub.push(&[2i16; 100]);
        assert_eq!(hub.pcm.lock().unwrap().len(), QUEUE_SAMPLES);
        assert_eq!(hub.pcm.lock().unwrap().back(), Some(&2));
    }

    #[test]
    fn a_frame_is_padded_with_silence() {
        let hub = TalkHub::new();
        hub.push(&[7i16; 100]);
        let frame = hub.take(FRAME_SAMPLES);
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert_eq!(&frame[..100], &[7i16; 100]);
        assert!(frame[100..].iter().all(|sample| *sample == 0));
        // The queue is empty afterwards, so the next frame is silence.
        assert!(hub.take(FRAME_SAMPLES).iter().all(|sample| *sample == 0));
    }

    /// A packet is as long as the channel says, not as long as one RTSP frame
    /// happens to be: the same hub feeds a camera that wants 60 ms of it.
    #[test]
    fn a_frame_is_as_long_as_the_channel_asks_for() {
        let hub = TalkHub::new();
        hub.push(&[5i16; 480]);
        let frame = hub.take(480);
        assert_eq!(frame.len(), 480);
        assert!(frame.iter().all(|sample| *sample == 5));
        let short = hub.take(160);
        assert_eq!(short.len(), 160);
    }

    /// A call trims the backlog the handshake left behind, and keeps the newest
    /// of it: what the viewer said while the session was coming up is not worth
    /// a call that runs half a second behind.
    #[test]
    fn trimming_keeps_the_newest_samples() {
        let hub = TalkHub::new();
        let samples: Vec<i16> = (0..QUEUE_SAMPLES).map(|n| n as i16).collect();
        hub.push(&samples);
        hub.trim(2 * FRAME_SAMPLES);
        assert_eq!(hub.pcm.lock().unwrap().len(), 2 * FRAME_SAMPLES);
        assert_eq!(hub.pcm.lock().unwrap().back(), Some(&(QUEUE_SAMPLES as i16 - 1)));
    }

    #[test]
    fn switching_the_target_stops_the_talk() {
        let hub = TalkHub::new();
        hub.set_target(Some(2));
        hub.set_talking(true);
        assert!(hub.talking_on(2));
        hub.set_target(Some(3));
        assert!(!hub.talking_on(3));
        assert!(!hub.is_talking());
    }
}
