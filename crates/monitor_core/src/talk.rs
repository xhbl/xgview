//! Talk-back: what the viewer says to the camera it has magnified.
//!
//! The playback side ([`crate::audio`]) carries a camera's microphone to the
//! speaker; this is the other direction, and it exists only while one camera is
//! magnified. The audio travels on a back channel the session asked for - the
//! `a=sendonly` media of the camera's own session description, which a camera
//! implementing ONVIF Profile T offers and which go2rtc adds to a `DESCRIBE`
//! carrying `backchannel=1` (see [`crate::rtsp`]).
//!
//! Two halves meet here:
//!
//! - the interface owns the microphone, captures it at whatever rate the device
//!   runs at, and pushes 8 kHz mono samples into the hub;
//! - the channel worker owns the RTSP session, and writes one 20 ms RTP packet
//!   of that audio to the back channel every 20 ms.
//!
//! The hub is therefore the only thing they share, and it is shared behind
//! atomics and one short lock: the audio callback must never wait for the
//! network, and the network task must never wait for the audio callback.
//!
//! Only one camera can be talked to at a time, and it is the magnified one, so
//! the hub has one target rather than a channel per camera - the same rule the
//! playback side follows.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use monitor_codec::audio::AudioCodec;
use monitor_codec::g711;

/// Sample rate of the back channel. Every camera measured takes 8 kHz mono,
/// and speech needs nothing more.
pub const RATE: u32 = 8_000;

/// Samples in one RTP packet: 20 ms of 8 kHz audio, the interval the cameras
/// send their own audio at.
pub const FRAME_SAMPLES: usize = 160;

/// How much audio the hub may hold before the oldest samples are dropped.
///
/// Half a second: enough to ride out a scheduling hiccup on the way to the
/// socket, short enough that the sound the viewer hears on the camera is not
/// half a second behind their voice.
const QUEUE_SAMPLES: usize = (RATE as usize) / 2;

/// The value [`TalkHub::target`] carries when nothing is magnified.
const NO_TARGET: usize = usize::MAX;

/// The microphone side of talk-back, shared between the interface and the
/// channel worker of the magnified camera.
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

    /// Fills one frame: whatever was captured, padded with silence when the
    /// microphone has not delivered enough for a whole packet yet.
    pub fn next_frame(&self) -> Vec<i16> {
        let mut frame = vec![0i16; FRAME_SAMPLES];
        let Ok(mut queue) = self.pcm.lock() else { return frame };
        let take = queue.len().min(FRAME_SAMPLES);
        for (slot, sample) in frame.iter_mut().zip(queue.drain(..take)) {
            *slot = sample;
        }
        frame
    }
}

/// Writes one channel's audio into RTP packets for the back channel.
///
/// The payload type and the companding follow the codec the camera offered in
/// its session description: static 0 (`PCMU`) and 8 (`PCMA`) are the only two
/// a talk-back media carries in practice, and both are a table lookup per
/// sample rather than a decoder.
#[derive(Debug)]
pub struct TalkStream {
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    payload_type: u8,
    alaw: bool,
}

impl TalkStream {
    /// Builds the stream for a codec, or `None` when the camera offered one
    /// this cannot encode.
    pub fn new(codec: AudioCodec) -> Option<Self> {
        let (payload_type, alaw) = match codec {
            AudioCodec::Pcmu => (0, false),
            AudioCodec::Pcma => (8, true),
            _ => return None,
        };
        Some(Self {
            // A random SSRC keeps two talk sessions from looking like one to
            // the camera. Derived from the clock rather than a hash: it only
            // has to be unlikely to collide, not unpredictable.
            ssrc: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos() as u32)
                .unwrap_or(1),
            sequence: 0,
            timestamp: 0,
            payload_type,
            alaw,
        })
    }

    /// One RTP packet carrying `samples`, which must be one frame.
    pub fn packet(&mut self, samples: &[i16]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(12 + samples.len());
        packet.push(0x80); // version 2, no padding, no extension, no CSRC
        packet.push(self.payload_type);
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(&self.timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        for sample in samples {
            packet.push(if self.alaw {
                g711::encode_alaw(*sample)
            } else {
                g711::encode_ulaw(*sample)
            });
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples.len() as u32);
        packet
    }

    /// A packet of silence, which is what an idle talk session keeps sending so
    /// that the camera's speaker is not left waiting on an empty socket.
    pub fn silence(&mut self) -> Vec<u8> {
        let silence = vec![0i16; FRAME_SAMPLES];
        self.packet(&silence)
    }

    /// Moves the clock on without sending anything: the samples the camera's
    /// speaker never heard.
    pub fn skip(&mut self, elapsed: Duration) {
        let samples = (elapsed.as_secs_f64() * RATE as f64).round() as u32;
        self.timestamp = self.timestamp.wrapping_add(samples);
    }

    /// A silence packet after an idle stretch, its timestamp moved on by the
    /// time that actually passed.
    ///
    /// This is what makes a packet a second enough to keep the channel warm: the
    /// camera reads a stream that went quiet rather than one running at a
    /// fiftieth of real time, and the first spoken frame lands where the clock
    /// says it should. See [`crate::pipeline`]'s `talk_gap`.
    pub fn silence_after(&mut self, elapsed: Duration) -> Vec<u8> {
        let frame = Duration::from_secs_f64(FRAME_SAMPLES as f64 / RATE as f64);
        self.skip(elapsed.saturating_sub(frame));
        self.silence()
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
        let frame = hub.next_frame();
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert_eq!(&frame[..100], &[7i16; 100]);
        assert!(frame[100..].iter().all(|sample| *sample == 0));
        // The queue is empty afterwards, so the next frame is silence.
        assert!(hub.next_frame().iter().all(|sample| *sample == 0));
    }

    /// An idle keepalive says a second has passed, not 20 ms: the camera's
    /// jitter buffer has to read the gap as silence, not as a stream running at a
    /// fiftieth of real time.
    #[test]
    fn an_idle_packet_moves_the_clock_by_the_time_that_passed() {
        let mut stream = TalkStream::new(AudioCodec::Pcmu).unwrap();
        let first = stream.silence();
        let stamp = |packet: &[u8]| u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        assert_eq!(stamp(&first), 0);
        assert_eq!(&first[2..4], &0u16.to_be_bytes());

        let second = stream.silence_after(Duration::from_secs(1));
        assert_eq!(&second[2..4], &1u16.to_be_bytes(), "one packet, one sequence number");
        assert_eq!(stamp(&second), 8_000, "one second of 8 kHz");

        // Talking again continues from there: the clock is continuous across the
        // idle stretch, not reset by it.
        let voice = stream.packet(&[0i16; FRAME_SAMPLES]);
        assert_eq!(stamp(&voice), 8_160);
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

    #[test]
    fn rtp_packets_follow_the_codec_and_the_counters() {
        let mut stream = TalkStream::new(AudioCodec::Pcmu).unwrap();
        let packet = stream.packet(&[0i16; FRAME_SAMPLES]);
        assert_eq!(packet.len(), 12 + FRAME_SAMPLES);
        assert_eq!(packet[1], 0, "PCMU is payload type 0");
        assert_eq!(&packet[2..4], &0u16.to_be_bytes());
        assert_eq!(&packet[4..8], &0u32.to_be_bytes());
        assert!(packet[12..].iter().all(|byte| *byte == 0xFF), "mu-law silence");

        let packet = stream.packet(&[0i16; FRAME_SAMPLES]);
        assert_eq!(&packet[2..4], &1u16.to_be_bytes());
        assert_eq!(&packet[4..8], &160u32.to_be_bytes());

        // A skip moves the clock with nothing on the wire: the samples a gap
        // between two packets stands for. The header carries the timestamp the
        // packet starts at, so this one is written at 320 + 4000.
        stream.skip(Duration::from_millis(500));
        let packet = stream.packet(&[0i16; FRAME_SAMPLES]);
        assert_eq!(&packet[4..8], &4_320u32.to_be_bytes());

        let mut stream = TalkStream::new(AudioCodec::Pcma).unwrap();
        let packet = stream.packet(&[0i16; FRAME_SAMPLES]);
        assert_eq!(packet[1], 8, "PCMA is payload type 8");
        assert_eq!(packet[12], 0xD5, "A-law silence");

        assert!(TalkStream::new(AudioCodec::Aac).is_none());
        assert!(TalkStream::new(AudioCodec::Opus).is_none());
    }
}
