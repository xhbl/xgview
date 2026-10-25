//! The channel a camera's own talk-back media is spoken on: RTP and G.711 over
//! an RTSP session's interleaved transport.
//!
//! This is the standard route - ONVIF Profile T on a camera, go2rtc's restream
//! in front of one - and the one every camera measured so far takes. The session
//! it rides on is opened by [`RtspClient::start_talk_only`]: `OPTIONS` with
//! `?backchannel=1`, a `DESCRIBE` carrying the header, the `sendonly` track set
//! up on its own, and a `PLAY`. The picture's session is not involved, which is
//! the whole point - see the plan in `docs/`.
//!
//! A camera that has no such media is reached some other way; [`super::foscam`]
//! is where that starts.

use std::time::Duration;

use monitor_codec::audio::AudioCodec;
use monitor_codec::g711;

use crate::rtsp::RtspClient;
use crate::Result;

/// How often the talk-back track is fed: one RTP packet every 20 ms, which is
/// what the cameras send their own audio at and what their speakers expect.
pub const TALK_INTERVAL: Duration = Duration::from_millis(20);

/// How often a talk-back channel nobody is talking on is refreshed.
///
/// The same 20 ms as a spoken frame, and deliberately not slower. An idle rate
/// of one a second was tried to save the 70 kbit/s of silence (1.5.33) and the
/// cameras would not take it: some stopped sending video too, and the session
/// died of `no media data` thirty seconds later. That experiment ran before the
/// hub had a target, so the stream was at one a second throughout rather than
/// falling to it after a call - which makes the cadence itself, not the change
/// in it, what the cameras rejected.
pub const TALK_KEEPALIVE: Duration = Duration::from_millis(20);

/// Samples in one RTP packet: 20 ms of 8 kHz audio, the interval the cameras
/// send their own audio at.
pub const FRAME_SAMPLES: usize = 160;

/// The gap between two packets on the talk-back track.
///
/// Kept as a function even though both arms are the same today: the cadence is
/// the cameras' to decide, and it was once two values.
pub fn talk_gap(talking: bool) -> Duration {
    if talking { TALK_INTERVAL } else { TALK_KEEPALIVE }
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
        let samples = (elapsed.as_secs_f64() * super::RATE as f64).round() as u32;
        self.timestamp = self.timestamp.wrapping_add(samples);
    }

    /// A silence packet after an idle stretch, its timestamp moved on by the
    /// time that actually passed.
    ///
    /// The idle cadence is the spoken one today - the cameras stopped sending
    /// video when the audio did, so the track is never left quiet for long - but
    /// the clock is carried by the timestamp rather than by the packet count, so
    /// a loop that fell behind, or a rate that is changed once more, does not
    /// leave the camera reading a stream that runs at the wrong speed. See
    /// [`talk_gap`].
    pub fn silence_after(&mut self, elapsed: Duration) -> Vec<u8> {
        let frame = Duration::from_secs_f64(FRAME_SAMPLES as f64 / super::RATE as f64);
        self.skip(elapsed.saturating_sub(frame));
        self.silence()
    }
}

/// A session opened for the talk-back alone, and the packetiser that feeds it.
///
/// The client is this type's to own: nothing else reads from it, so the audio
/// path and the session that carries it cannot drift apart. Opening is the only
/// part that waits on the network - [`TalkChannel::send`] queues into the
/// client's outbox and returns.
pub struct RtspTalkChannel {
    client: RtspClient,
    channel: u8,
    stream: TalkStream,
}

impl RtspTalkChannel {
    /// Opens a session that carries the talk-back media and nothing else.
    ///
    /// `Ok(None)` is a camera that offers no such media, or offers one this
    /// build cannot encode - neither is a failure. An `Err` is the camera
    /// refusing this shape of session, which is what the fallback is for.
    pub async fn open(url: &str, credentials: Option<(String, String)>) -> Result<Option<Self>> {
        let mut client = RtspClient::connect_with_auth(url, credentials).await?;
        let Some(track) = client.start_talk_only().await? else {
            return Ok(None);
        };
        let codec = AudioCodec::from_encoding(track.encoding.as_deref().unwrap_or_default());
        let Some(stream) = TalkStream::new(codec) else {
            return Ok(None);
        };
        Ok(Some(Self { client, channel: track.channel, stream }))
    }

    /// The interleaved channel the server put the media on.
    pub fn channel(&self) -> u8 {
        self.channel
    }

    /// Keeps the session from timing out under a long call.
    ///
    /// A camera that says `timeout=60` on its `SETUP` expects to hear from the
    /// client before that. The audio itself may well count as hearing from us,
    /// but RTSP does not promise it, and a session that dies mid-call is heard
    /// as the far end going quiet.
    pub async fn keep_alive(&mut self) -> Result<()> {
        self.client.send_keep_alive().await
    }

    /// How often [`RtspTalkChannel::keep_alive`] is due.
    pub fn keep_alive_interval(&self) -> Duration {
        self.client.keep_alive_interval()
    }

    /// Reads whatever the camera says, and reports the session dying.
    ///
    /// The media is `sendonly`, so nothing of interest arrives on it - but the
    /// socket still has to be read: the answers to the keep alive pile up
    /// otherwise, and a camera that has closed the session is only visible as a
    /// read that fails. `Ok(())` therefore means "nothing worth acting on",
    /// which is the common case.
    pub async fn watch(&mut self) -> Result<()> {
        self.client.read_interleaved().await.map(|_| ())
    }

    /// Ends the session without waiting for the camera to agree.
    ///
    /// A `TEARDOWN` the server answers costs one round trip; one it does not
    /// costs the client's whole timeout, and go2rtc answers none (measured:
    /// ten seconds, 2026-10-25). Hanging up is a button, so the request is
    /// written and the connection dropped - the session goes with the socket.
    pub async fn close(mut self) {
        self.client.teardown_nowait().await;
    }
}

impl super::TalkChannel for RtspTalkChannel {
    fn interval(&self) -> Duration {
        TALK_INTERVAL
    }

    fn samples(&self) -> usize {
        FRAME_SAMPLES
    }

    fn send(&mut self, samples: &[i16]) -> Result<()> {
        let packet = self.stream.packet(samples);
        self.client.send_interleaved(self.channel, &packet);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An idle keepalive says a second has passed, not 20 ms: the camera's
    /// jitter buffer has to read the gap as silence, not as a stream running at
    /// a fiftieth of real time.
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
