//! The RTSP session: negotiation, the packet loop, and the worker that carries a
//! call of its own.

use super::*;

use std::collections::VecDeque;

use super::shared::{
    BaselineFix, Sinks, StatsWindow, announce_streaming, emit, publish_frames, uses_hardware,
    video_decoder,
};

/// An audio track the session feeds the hub, with the decoder opened for it.
struct AudioTrack {
    decoder: Box<dyn AudioDecoder>,
    info: AudioStreamInfo,
    /// The RFC 3640 AU-header field widths, for the AAC depacketiser.
    aac: AacRtpParams,
    /// Decoded sound waiting for the lag in front of it to drain.
    waiting: VecDeque<i16>,
    /// The rate the sink was last opened at, which is how a sink that has just
    /// been opened is told from one that was already playing.
    opened: u32,
}

/// Opens an audio decoder for the camera's audio track, or `None` when there is
/// no track the client receives or the codec has no backend in this build.
fn audio_track_decoder(tracks: &[SdpTrack]) -> Option<AudioTrack> {
    let track = tracks.iter().find(|track| track.is_audio() && !track.is_sendonly())?;
    let named = AudioCodec::from_encoding(track.encoding.as_deref().unwrap_or_default());
    let codec = if named == AudioCodec::Unknown {
        track
            .payload_type
            .and_then(AudioCodec::from_static_payload_type)
            .unwrap_or(named)
    } else {
        named
    };
    let rate = track.clock_rate.unwrap_or(8000);
    let channels = track.channels.unwrap_or(1);
    let mut info = AudioStreamInfo::new(codec, rate, channels);
    if let Some(fmtp) = track.fmtp.as_deref() {
        if let Some(config) = aac_config(fmtp) {
            info.extradata = config;
        }
    }
    let mut decoder = create_audio_decoder(codec);
    match decoder.configure(&info) {
        Ok(()) => Some(AudioTrack {
            decoder,
            info,
            aac: AacRtpParams::from_fmtp(track.fmtp.as_deref()),
            waiting: VecDeque::new(),
            opened: 0,
        }),
        Err(err) => {
            tracing::debug!(
                target: "xgview::pipeline",
                codec = %codec,
                error = %err,
                "no audio backend for this track, the channel stays silent"
            );
            None
        }
    }
}

/// The AAC AudioSpecificConfig out of an SDP `fmtp` body (`config=1408`).
fn aac_config(fmtp: &str) -> Option<Vec<u8>> {
    let value = fmtp.split(';').find_map(|part| part.trim().strip_prefix("config="))?;
    let value = value.trim();
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let mut chars = value.chars();
    while let (Some(hi), Some(lo)) = (chars.next(), chars.next()) {
        bytes.push((hi.to_digit(16)? as u8) << 4 | lo.to_digit(16)? as u8);
    }
    Some(bytes)
}

/// NAL unit types of an Annex-B access unit, in order, read in the codec's own
/// layout.
///
/// A rejected picture is named by what it holds: an H.264 slice that arrived
/// without its parameter sets reads as `[7, 1]` or `[8, 1]`, while a complete
/// picture the decoder merely has no reference for reads as `[7, 8, 1]`. For
/// H.265 the numbers are the RFC 7798 ones - 32 / 33 / 34 for the sets, 0..=31
/// for the slices - because the type lives in two bytes there.
fn nal_types(data: &[u8], kind: Codec) -> Vec<u8> {
    let nal_type: fn(u8) -> u8 = match kind {
        Codec::H265 => hevc_nal_unit_type,
        _ => nal_unit_type,
    };
    let mut types = Vec::new();
    let mut offset = 0;
    while offset + 3 <= data.len() {
        let code = if data[offset..].starts_with(&START_CODE) {
            4
        } else if data[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        offset += code;
        if let Some(&first) = data.get(offset) {
            types.push(nal_type(first));
        }
    }
    types
}

/// Type and length of every cached parameter set.
///
/// `nal_type` is the codec's own type reader, since an H.264 set and an H.265
/// set put the type in different bits of different sized headers.
fn sets<'a>(nals: impl Iterator<Item = &'a [u8]>, nal_type: fn(u8) -> u8) -> Vec<(u8, usize)> {
    nals.map(|nal| (nal_type(nal[0]), nal.len())).collect()
}

/// Hexadecimal form of the leading bytes of a unit, for reading a rejected
/// access unit by hand.
fn hex(data: &[u8], limit: usize) -> String {
    data.iter().take(limit).fold(String::new(), |mut text, byte| {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// How long a session may go without a picture before it is treated as dead.
///
/// Timing a session by its packets only catches a stream that stops entirely. A
/// camera at its limit of concurrent sessions answers a stream request, keeps
/// the connection open and then trickles: a picture every six to eight seconds
/// where the stream is a fifteen to twenty-five frame one. No error is raised,
/// no gap is long enough for [`READ_TIMEOUT`], and a frozen picture behind a
/// `LIVE` badge is the worst way for a wall to fail. So the deadline counts
/// pictures instead.
///
/// A fixed deadline is wrong in both directions, because the only legitimate
/// reason for a long gap between pictures is that the next one has to wait for a
/// key frame: the wait is bounded by the stream's group of pictures, which is a
/// property of the encoder and can be a second or ten. So it is measured rather
/// than assumed, and the measurement only ever *falls*.
///
/// That direction matters. An estimate that followed the stream's recent
/// behaviour would be fed by the very condition it exists to detect - a stream
/// that has begun to dribble would widen the deadline meant to catch it, and
/// widen it again, until it never fired. Learning the shortest interval instead
/// means a degraded stream cannot move the deadline at all: it can only be
/// tightened by evidence, never loosened by it.
pub(super) struct StallDeadline {
    /// Shortest interval between two key frames seen so far, `MAX` until the
    /// stream has shown one.
    group: Duration,
    /// When the last key frame was handed over, `None` until the first.
    last_key: Option<Instant>,
    /// When the decoder last produced a picture. Packets arriving is not the
    /// same fact, and only this one tells a live stream from a frozen one.
    last_picture: Instant,
    /// A picture has come out, so the session is held to the shorter deadline.
    running: bool,
    /// When the session first exceeded its patience without a picture, `None`
    /// while it is still delivering. The last frame is held for
    /// [`STALL_HARD_TIMEOUT`] after this before reconnecting.
    stalled_since: Option<Instant>,
}

impl StallDeadline {
    /// A deadline that knows nothing yet, and is therefore as patient as it is
    /// allowed to be.
    ///
    /// Assuming a short group until the stream shows otherwise would cut off a
    /// camera that is legitimately waiting for its next key frame - and since a
    /// group is only ever learned from two key frames, a session cut off before
    /// the second one could never learn it, and would be cut off again. The
    /// patience falls as soon as the stream has been seen.
    pub(super) fn new() -> Self {
        Self {
            group: Duration::MAX,
            last_key: None,
            last_picture: Instant::now(),
            running: false,
            stalled_since: None,
        }
    }

    /// Folds in one access unit. Its key frames are what the group is measured
    /// from.
    pub(super) fn observe(&mut self, keyframe: bool, now: Instant) {
        if !keyframe {
            return;
        }
        if let Some(previous) = self.last_key.replace(now) {
            let interval = now.saturating_duration_since(previous);
            if interval >= MIN_PLAUSIBLE_GROUP {
                self.group = self.group.min(interval);
            }
        }
    }

    /// A picture came out of the decoder.
    pub(super) fn picture(&mut self, now: Instant) {
        self.running = true;
        self.last_picture = now;
        self.stalled_since = None;
    }

    /// How long the session has gone without one.
    fn since_picture(&self) -> Duration {
        self.last_picture.elapsed()
    }

    /// How long it may still go. Twice the group, because a picture arrives
    /// within a group of the previous one unless a key frame has to be waited
    /// for, and doubling leaves room for the delivery of that key frame.
    pub(super) fn patience(&self) -> Duration {
        let budget = self.group.saturating_mul(2);
        if self.running {
            budget.clamp(STALL_FLOOR, STALL_CEILING)
        } else {
            budget.clamp(FIRST_PICTURE_FLOOR, FIRST_PICTURE_CEILING)
        }
    }

    /// The two numbers the loop compares, read together so that the wait and the
    /// stall are judged by the same reading of the clock.
    pub(super) fn lateness(&self) -> Lateness {
        Lateness { since: self.since_picture(), patience: self.patience() }
    }
}

/// How late the picture is, and how late it may be.
///
/// Read once per turn of the loop and carried as one value. Once, because the
/// clock is read rather than stored - `since` is how long ago the last picture
/// was, so asking twice gives two answers. As one value, because both numbers are
/// `Duration`, and two arguments of the same type are two arguments that can be
/// handed over the wrong way round.
#[derive(Clone, Copy)]
pub(super) struct Lateness {
    /// How long the decoder has gone without producing a picture.
    pub(super) since: Duration,
    /// How long it may still go, learned from the stream.
    pub(super) patience: Duration,
}

/// The RTP depacketizer a session pulls its pictures out of.
///
/// A session picks one when it opens, from the codec the SDP named, and the
/// loop around it is the same for all of them: a packet goes in, zero or more
/// complete pictures come out. These are the codecs that reach a decoder, so
/// these are the reassemblies that are needed.
enum Depacketizer {
    /// H.264, the NAL units of an access unit spread over RTP packets.
    H264(H264Depacketizer),
    /// H.265 / HEVC, the same idea with RFC 7798's header and packet types.
    Hevc(HevcDepacketizer),
    /// MJPEG, the fragments of one JPEG scan spread over RTP packets.
    Jpeg(JpegDepacketizer),
}

impl Depacketizer {
    fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> Vec<AccessUnit> {
        match self {
            Depacketizer::H264(depacketizer) => depacketizer.push(header, payload),
            Depacketizer::Hevc(depacketizer) => depacketizer.push(header, payload),
            Depacketizer::Jpeg(depacketizer) => depacketizer.push(header, payload),
        }
    }

    /// Writes off the picture being assembled, after a gap in the RTP sequence
    /// numbers.
    fn invalidate(&mut self) {
        match self {
            Depacketizer::H264(depacketizer) => depacketizer.invalidate_current(),
            Depacketizer::Hevc(depacketizer) => depacketizer.invalidate_current(),
            Depacketizer::Jpeg(depacketizer) => depacketizer.invalidate(),
        }
    }

    /// Cached parameter sets, which an H.264 or H.265 stream has: a JPEG
    /// carries its tables in the stream rather than in a cache.
    fn parameter_sets(&self) -> Vec<(u8, usize)> {
        match self {
            Depacketizer::H264(depacketizer) => sets(depacketizer.parameter_sets(), nal_unit_type),
            Depacketizer::Hevc(depacketizer) => {
                sets(depacketizer.parameter_sets(), hevc_nal_unit_type)
            }
            Depacketizer::Jpeg(_) => Vec::new(),
        }
    }
}

/// Interarrival jitter, in the form RFC 3550 §6.4.1 defines it.
///
/// Kept as the difference between consecutive packets rather than against a
/// session origin: the RTP timestamp wraps every thirteen hours or so, and an
/// absolute transit would read that wrap as one enormous jitter.
pub(super) struct Jitter {
    /// RTP timestamp units per second, as the session description announced it.
    clock_rate: f64,
    /// The smoothed estimate, in those units.
    value: f64,
    /// When the last packet arrived, and the timestamp it carried. `None` until
    /// the first, because a jitter is a difference between two.
    last: Option<(Instant, u32)>,
}

impl Jitter {
    pub(super) fn new(clock_rate: f64) -> Self {
        Self { clock_rate, value: 0.0, last: None }
    }

    /// Folds in one packet. The first is the baseline and adds nothing.
    pub(super) fn observe(&mut self, timestamp: u32, now: Instant) {
        if let Some((previous_arrival, previous_timestamp)) = self.last {
            let arrived =
                now.saturating_duration_since(previous_arrival).as_secs_f64() * self.clock_rate;
            // How far apart the two were *sent*, read as a signed distance:
            // `wrapping_sub` already carries the counter over its wrap, and the
            // cast is what makes a packet whose timestamp is behind the last one
            // a short step back rather than four billion units on.
            let sent = f64::from(timestamp.wrapping_sub(previous_timestamp) as i32);
            let difference = (arrived - sent).abs();
            // The smoothing constant the RFC names: a sixteenth of the new
            // difference, so one late packet moves the estimate a little.
            self.value += (difference - self.value) / 16.0;
        }
        self.last = Some((now, timestamp));
    }

    /// The estimate, in the units the receiver report carries.
    pub(super) fn value(&self) -> u32 {
        self.value.round() as u32
    }
}

/// What a session knows about the sequence numbers it has received.
///
/// Two questions are answered here, and they are asked over different
/// transports: the gaps and the stragglers are what UDP does, while the
/// extended highest sequence number the receiver report carries is counted on
/// both.
pub(super) struct SequenceWindow {
    /// The last sequence number accepted, `None` until the first packet. Only
    /// UDP moves it, since the interleaved transport neither reorders nor
    /// duplicates.
    last: Option<u16>,
    /// The highest sequence number seen, with its cycles folded into the high
    /// half the way RFC 3550 counts it, `None` until the first packet.
    highest: Option<u32>,
    /// What the gaps say was never delivered, over the whole session.
    lost: u64,
}

/// What a packet's sequence number says about the ones before it.
pub(super) enum Arrival {
    /// The packet follows the last one accepted.
    Next,
    /// A duplicate, or a straggler that belongs to a picture already handed
    /// over.
    Stale,
    /// The packets in between were lost; this many.
    Gap(u16),
}

impl SequenceWindow {
    pub(super) fn new() -> Self {
        Self { last: None, highest: None, lost: 0 }
    }

    /// Folds one sequence number into the extended count, on either transport.
    ///
    /// The first packet is the origin. Folding it against a zero base instead
    /// would read every number in the top half of the range as a straggler from
    /// the previous cycle: half of all sessions held at zero, and a count that
    /// is a whole cycle short from then on, because it can only climb once the
    /// sequence numbers have wrapped round to it - minutes of reports that say
    /// the session has just started.
    pub(super) fn see(&mut self, sequence: u16) {
        let Some(highest) = self.highest else {
            self.highest = Some(u32::from(sequence));
            return;
        };
        // The same cycle as the highest seen so far, until one of the two
        // bounds below says otherwise.
        let candidate = (highest & 0xffff_0000) | u32::from(sequence);
        let folded = if candidate + 0x8000 < highest {
            // More than half the range behind: it has wrapped forwards.
            candidate + 0x1_0000
        } else if candidate > highest + 0x8000 {
            // More than half the range ahead: it belongs to the cycle before.
            candidate.saturating_sub(0x1_0000)
        } else {
            candidate
        };
        // The count is the highest received, so a straggler may be folded but
        // may not pull it backwards.
        self.highest = Some(highest.max(folded));
    }

    /// What a packet's sequence number says about the ones before it, for the
    /// transport that reorders and duplicates.
    pub(super) fn accept(&mut self, sequence: u16) -> Arrival {
        if let Some(previous) = self.last {
            let delta = sequence.wrapping_sub(previous);
            if delta == 0 || delta > 0x8000 {
                return Arrival::Stale;
            }
            if delta > 1 {
                let missing = delta - 1;
                self.lost += u64::from(missing);
                self.last = Some(sequence);
                return Arrival::Gap(missing);
            }
        }
        self.last = Some(sequence);
        Arrival::Next
    }

    /// The extended highest sequence number, for the receiver report.
    pub(super) fn highest(&self) -> u32 {
        self.highest.unwrap_or(0)
    }

    /// Everything the gaps have said was never delivered, over the session.
    pub(super) fn lost(&self) -> u64 {
        self.lost
    }
}

/// How an attempt at establishing a session ended.
///
/// The two failures that are worth answering with the other transport are kept
/// apart from the rest, and from each other, because the answer differs: a
/// refusal is the server saying so, a timeout is the server saying nothing, and
/// where it went quiet decides whether the other transport is worth a try.
#[derive(Debug)]
pub(super) enum HandshakeFailure {
    /// The server answered `461` to `SETUP`, with what it said.
    Refused(String),
    /// The peer stopped answering; `stage` is how far the handshake had got.
    TimedOut {
        stage: Option<RtspStage>,
        timeout: Duration,
    },
    /// Anything else.
    Failed(crate::error::CoreError),
}

impl HandshakeFailure {
    /// Whether the handshake should be tried once more over TCP.
    ///
    /// Two answers justify it, and only over UDP. A `461` is the server saying
    /// so outright - go2rtc answers it to every UDP `SETUP` on purpose, so that
    /// a client asking for UDP first falls back. A silence from `SETUP` onwards
    /// usually means the datagrams are being dropped while the connection the
    /// handshake has been running over is plainly working, which is what a
    /// routed or NATed path does to UDP. A silence before that is the control
    /// connection itself, which the other transport would only repeat.
    pub(super) fn worth_the_other_transport(&self, transport: RtspTransport) -> bool {
        if !transport.is_udp() {
            return false;
        }
        match self {
            HandshakeFailure::Refused(_) => true,
            HandshakeFailure::TimedOut { stage, .. } => {
                matches!(stage, Some(RtspStage::Setup) | Some(RtspStage::Play))
            }
            HandshakeFailure::Failed(_) => false,
        }
    }

    /// The failure as the channel reports it.
    fn into_error(self) -> crate::error::CoreError {
        use crate::error::CoreError;
        match self {
            HandshakeFailure::Failed(err) => err,
            HandshakeFailure::Refused(message) => CoreError::transport(message),
            HandshakeFailure::TimedOut { stage, timeout } => {
                // Which request went unanswered, kept in the message: it is the
                // difference between a path that drops datagrams and one that
                // never carried the control connection at all.
                let stage = match stage {
                    Some(RtspStage::Options) => " during OPTIONS",
                    Some(RtspStage::Describe) => " during DESCRIBE",
                    Some(RtspStage::Setup) => " during SETUP",
                    Some(RtspStage::Play) => " during PLAY",
                    None => "",
                };
                // A timeout and not a plain `Rtsp` failure: the connection was
                // up and the peer stopped answering, which is a different fault
                // from a peer that answered wrongly. See `FailureKind`.
                CoreError::timeout(format!(
                    "the session handshake timed out after {} s{stage}",
                    timeout.as_secs()
                ))
            }
        }
    }
}

/// Establishes a session, within `timeout`.
///
/// The client is dropped rather than reused when the deadline fires, so a
/// request left half read costs nothing: the next attempt opens its own
/// connection. That is what makes an overall deadline safe here where a
/// cancelled read mid session would not be.
pub(super) async fn negotiate(
    client: &mut RtspClient,
    transport: RtspTransport,
    timeout: Duration,
    index: usize,
    backchannel: bool,
) -> Result<Vec<SdpTrack>, HandshakeFailure> {
    match tokio::time::timeout(timeout, client.start_with_options(transport, backchannel)).await {
        Ok(Ok(tracks)) => Ok(tracks),
        Ok(Err(crate::error::CoreError::Transport(message))) => {
            Err(HandshakeFailure::Refused(message))
        }
        Ok(Err(err)) => Err(HandshakeFailure::Failed(err)),
        Err(_) => {
            let stage = client.stage();
            tracing::warn!(
                target: "xgview::pipeline",
                index,
                timeout_s = timeout.as_secs(),
                stage = ?stage,
                "the handshake timed out, the peer stopped answering"
            );
            Err(HandshakeFailure::TimedOut { stage, timeout })
        }
    }
}

/// Whether a failed call says something about the *shape* of the session it
/// asked for, and so whether the picture's own session is worth trying instead.
///
/// A camera that will not take a `SETUP` of its own can still carry the
/// microphone on the session it is already streaming, and that is what
/// `TalkFallback` reopens for. Nothing else is worth reopening: a rejected
/// password, a peer that stopped answering and an address that cannot be
/// reached all say the same thing about the second session as about the first,
/// and reopening it would cost the picture a connect for nothing. A refusal of
/// the transport is a different question with an answer of its own - the other
/// transport, inside the session that asked - so it is not this one either.
///
/// The question used to be asked as `matches!(err, CoreError::Rtsp(_))`, which
/// answered yes for a `401` - the one failure that is certainly not about the
/// shape of anything. See [`crate::error::FailureKind`].
pub(super) fn takes_the_picture_session(err: &crate::error::CoreError) -> bool {
    err.kind() == crate::error::FailureKind::Protocol
}

/// One call: a session of its own that carries the microphone and nothing else.
///
/// A task of its own rather than part of the picture's loop, for the two reasons
/// the split exists: opening it takes a handshake, which must not hold up a
/// frame, and a camera that breaks it must not be able to touch the picture.
///
/// It ends when the viewer hangs up, which is [`TalkHub::talking_on`] going
/// false - no command is needed for that, and the three ways a call ends (the
/// handset, the bar closing, the viewport moving) all reach it that way.
pub(super) async fn talk_worker(
    index: usize,
    endpoint: Endpoint,
    route: TalkRoute,
    talk: Arc<TalkHub>,
    events: Sender<StreamEvent>,
    commands: UnboundedSender<ChannelCommand>,
    timeout: Duration,
) {
    let Endpoint { camera_id, uri, credentials, .. } = endpoint;
    let failed = |reason| {
        let _ = events.send(StreamEvent::CallFailed {
            index,
            camera_id: camera_id.clone(),
            reason,
        });
    };

    let mut channel = match tokio::time::timeout(timeout, Channel::open(route, &uri, credentials)).await {
        Ok(Ok(Some(channel))) => channel,
        Ok(Ok(None)) => {
            tracing::debug!(
                target: "xgview::talk",
                camera = %camera_id,
                "the camera has no back channel to call"
            );
            failed(CallFailure::NoBackChannel);
            return;
        }
        Ok(Err(err)) => {
            // A protocol refusal is the camera saying something about *this
            // shape* of session, and the picture's own session can still carry
            // the media - that is the fallback. A transport failure says nothing
            // about the shape, and reopening would only repeat it.
            if takes_the_picture_session(&err) {
                tracing::debug!(
                    target: "xgview::talk",
                    camera = %camera_id,
                    error = %err,
                    "the camera refused a call of its own, falling back to the picture's session"
                );
                let _ = commands.send(ChannelCommand::TalkFallback { index });
            } else {
                tracing::debug!(
                    target: "xgview::talk",
                    camera = %camera_id,
                    error = %err,
                    "the call could not be opened"
                );
                failed(CallFailure::Unreachable);
            }
            return;
        }
        Err(_) => {
            tracing::debug!(
                target: "xgview::talk",
                camera = %camera_id,
                timeout_s = timeout.as_secs(),
                "the call timed out opening"
            );
            failed(CallFailure::Unreachable);
            return;
        }
    };

    // The microphone has been pushing since the press, and the handshake left a
    // backlog behind it. From here on the audio is consumed at the rate it is
    // produced, so that backlog would stay for the whole call: it is cut back to
    // two packets, which is what the camera's jitter buffer expects to see.
    talk.trim(2 * channel.samples());
    tracing::debug!(
        target: "xgview::talk",
        camera = %camera_id,
        channel = channel.label(),
        "the call is live"
    );

    let interval = channel.interval();
    let mut next_send = Instant::now();
    // `None` is a protocol that needs no keep-alive: its audio is the proof of
    // life. A protocol that does need one names how often.
    let mut next_keep_alive = channel.keep_alive_interval().map(|every| Instant::now() + every);
    while talk.talking_on(index) {
        let now = Instant::now();
        if let Some(due) = next_keep_alive {
            if now >= due {
                if channel.keep_alive().await.is_err() {
                    break;
                }
                next_keep_alive = channel.keep_alive_interval().map(|every| now + every);
            }
        }
        if now >= next_send {
            let samples = talk.take(channel.samples());
            if channel.send(&samples).is_err() {
                break;
            }
            next_send += interval;
            // A loop that fell well behind - a stalled socket, a suspended
            // process - is not worth a burst of catch-up packets: the camera's
            // speaker wants the present, not the backlog.
            if next_send < now {
                next_send = now + interval;
            }
        }
        // One read, bounded by the time left before the next thing this loop has
        // to do. It is what keeps the answers to the keep alive from piling up
        // in the socket, and it is the only way a camera that has closed the
        // session is noticed before the next packet goes nowhere.
        let next_wake = match next_keep_alive {
            Some(due) => next_send.min(due),
            None => next_send,
        };
        let wait = next_wake.saturating_duration_since(Instant::now());
        if let Ok(Err(_)) = tokio::time::timeout(wait, channel.watch()).await {
            break;
        }
    }

    // Only a call the viewer did not end is worth a word: a hang-up arrives here
    // as `talking_on` going false, and the interface has already said so.
    let hung_up = !talk.talking_on(index);
    channel.close().await;
    if !hung_up {
        tracing::debug!(target: "xgview::talk", camera = %camera_id, "the call was lost");
        failed(CallFailure::Unreachable);
    } else {
        tracing::debug!(target: "xgview::talk", camera = %camera_id, "the call ended");
    }
}

/// One RTSP session: negotiation then packet pumping.
pub(super) async fn run_session(
    endpoint: Endpoint,
    prefer_hardware: bool,
    handshake_timeout: Duration,
    sinks: Sinks<'_>,
    audio: &AudioHub,
    talk: &TalkHub,
) -> Result<()> {
    RtspSession::open(endpoint, prefer_hardware, handshake_timeout, sinks, audio, talk)
        .await?
        .pump()
        .await
}

/// How far ahead of the clock the first live picture is placed.
///
/// A live main stream does not arrive one picture per frame interval. Measured
/// on the wall with a client that does not decode at all, three or four
/// pictures arrive within a millisecond of each other and then the wire is
/// quiet for a hundred and fifty: the forwarder sends a batch. A player that
/// decodes each picture as it lands gets a batch of four pictures into one
/// refresh of the display, which shows the last of them, and then a hundred
/// and fifty milliseconds of a still picture. That is eight updates a second
/// out of twenty-six pictures, which is exactly what a magnified live view
/// looked like next to a replay of the same camera.
///
/// So pictures are placed on a clock taken from their own timestamps and
/// handed to the decoder when their moment comes, the way the replay path
/// already did it. This is the distance between the first picture's moment and
/// the moment it arrived: enough for a batch's first picture not to be shown
/// before its own time, and small enough that a monitor does not feel lagged.
///
/// It is a starting point and a floor, not a fixed number: the clock widens
/// when the wire strains it, and never holds less than one frame's worth -
/// a twelve frame a second camera is eighty milliseconds between pictures, and
/// a lead shorter than that has nothing in hand to place, so each picture is
/// handed the moment it lands and the arrival jitter is played out on the
/// screen. One frame is what makes a slow stream even.
const LIVE_LEAD: Duration = Duration::from_millis(40);

/// The narrowest and widest the lead is allowed to become.
///
/// The floor is the smallest distance that still leaves a picture a place of its
/// own before the one behind it; the ceiling is where a second of delay would
/// begin to be felt as a lag rather than as a buffer.
const LIVE_LEAD_MIN: Duration = Duration::from_millis(20);
const LIVE_LEAD_MAX: Duration = Duration::from_millis(160);

/// The shortest and longest gap between two pictures that still reads as a frame
/// interval.
///
/// Outside these a step is not the stream's rate: a repeated timestamp says two
/// pictures share a moment, and a step of a second is a camera that stopped.
/// Both would otherwise redefine what "one frame" means.
const LIVE_INTERVAL_MIN: Duration = Duration::from_millis(8);
const LIVE_INTERVAL_MAX: Duration = Duration::from_millis(250);

/// How far past its moment a picture has to be before the lead is blamed.
///
/// Not any lateness: a picture handed over twenty milliseconds after its moment
/// is a picture the loop reached a fraction of a refresh late - the wake is a
/// millisecond of timer and a decode - which costs nothing, because it is
/// decoded and shown and never seen to be late. What the lead buys is a queue
/// deep enough that the wire's gaps do not empty it, so the only lateness that
/// means anything is lateness deep enough to say the queue had run out, and
/// that is a hundred and fifty milliseconds of silence where a picture should
/// have been.
const LIVE_STRAIN: Duration = Duration::from_millis(150);

/// How far one retune moves the lead.
const LIVE_LEAD_STEP: Duration = Duration::from_millis(10);

/// How often the lead is reconsidered.
const LIVE_RETUNE: Duration = Duration::from_secs(2);

/// How long without a strained picture before the lead is narrowed.
///
/// Narrowing is the direction that costs if it is wrong, so it waits far longer
/// than widening: a clock that narrows eagerly spends its life oscillating
/// between two settings, which is a delay that breathes.
const LIVE_TRIM_AFTER: Duration = Duration::from_secs(15);

/// How many narrowings a widening may undo before this stream stops being
/// narrowed at all.
///
/// One is a transient - a paused process, a lost packet, a decoder that
/// hiccupped once - and is not proof the lead is too small. Two is a pattern.
///
/// The count is not fenced by a time window, because how long a narrowing takes
/// to show that it was wrong is the stream's own business: measured on the Mi
/// Pad's doorbell, the pictures a ten-millisecond narrowing moved forward
/// arrived late six seconds later one time and fourteen the next. Counting "a
/// widening while the last thing that moved the lead was a narrowing" is exact,
/// and it is the safer error: a stream that is settled a little wider than it
/// needed is a few milliseconds of delay nobody sees, where one that keeps
/// breathing is visible motion.
const LIVE_TRIM_BOUNCES: u8 = 2;

/// How long after it was made a narrowing is still understood as the thing a
/// widening is undoing.
///
/// The rebound the count above is for shows up seconds after the step, not
/// minutes. Sixty is four times the latest one measured and still far short of
/// the hours a session runs, which is what makes it a fence rather than a
/// second clock: a widening from an unrelated hiccup later in the session is
/// not read as the stream taking back a step it gave up long before. Without
/// it, every widening marks the narrowing behind it, and two of them - a
/// wireless drop and a decoder preempted, an hour apart - would settle a stream
/// that never bounced at all.
const LIVE_TRIM_BOUNCE_WINDOW: Duration = Duration::from_secs(60);

/// The most pictures the clock may hold while the decoder catches up.
///
/// A batch of four arrives while the three pictures before it are still waiting
/// for their moments - the queue sits at five or six through a healthy stream,
/// and a batch lands on top of that - so the ceiling has to clear a whole batch
/// before it means anything. Twelve pictures are half a second, and past that
/// what is given up is the picture rather than the delay: a viewer watching a
/// door wants the present.
const LIVE_MAX_HELD: usize = 12;

/// How many decoded pictures the session keeps on the far side of the pacer.
///
/// One that is handed back early waits here for its turn, and a decoder that ran
/// a frame behind hands back two at once - four is the largest batch a decoder
/// here was seen to return (`priv/docs/foscam_probe.md` §13). Anything past it
/// is a backlog, not motion. Dropping a picture costs a frame the viewer would
/// have seen, so the hold is sized to the batch rather than to the steady state,
/// which is one.
const HELD_PICTURES: usize = 4;

/// Puts a decoder's pictures in the hold, keeping the ones nearest the present.
///
/// The oldest go first: what the hold is for is the turn that is coming, and a
/// queue longer than the hold is a decoder that got ahead of the clock. Bounded
/// by what the merge put in the queue and not by what arrived - a batch bigger
/// than the hold would otherwise never satisfy the loop, which is a session
/// thread that spins for ever instead of one that drops a picture.
fn hold(pending: &mut VecDeque<DecodedFrame>, decoded: Vec<DecodedFrame>) {
    pending.extend(decoded);
    while pending.len() > HELD_PICTURES {
        pending.pop_front();
    }
}

/// The most pictures a batch can put in the queue at once.
///
/// A batch is what the forwarder sends in one read: four or five pictures on a
/// twenty-five frame stream. A queue deeper than that has something else in it.
const LIVE_BATCH_PICTURES: usize = 5;

/// How long a queue that deep has to stay deep before it is a backlog.
///
/// A batch fills the queue for a frame or two and is gone by the next one. A
/// queue still that deep a second later is not a batch: it is a stream that was
/// already sending when the session joined, and its pictures are the past.
const LIVE_CROWD_AFTER: Duration = Duration::from_secs(1);

/// How far behind its own clock a picture may be before the clock is moved to
/// it instead of the picture being shown late.
const LIVE_LATE_LIMIT: Duration = Duration::from_millis(500);

/// The `H.264` RTP clock, in ticks a second.
const LIVE_CLOCK: i64 = 90_000;

/// How much of the distance to the timestamps' own idea of where a picture
/// belongs is taken back with each picture.
///
/// A camera's timestamps are not a metronome. Measured on the living room
/// stream, the step between two pictures runs from ten to seventy milliseconds
/// around a thirty-nine millisecond mean, and a clock that follows them frame by
/// frame shows that jitter as motion that speeds up and slows down. Placing each
/// picture one learned interval after the one before turns it into even motion;
/// this is what keeps the even clock from drifting away from the stream, a
/// fraction of the difference at a time.
///
/// Thirty-two is an eighth of the jitter left - a step thirty milliseconds out
/// of place moves the one after it under four - and a bounded offset: a rate
/// wrong by a tick a frame settles a couple of milliseconds out, which is not a
/// delay anybody sees. It is also the time to close a larger error, which is why
/// it is not smaller: a clock that came back from a fold settles in a second.
const LIVE_PHASE_GAIN: i64 = 32;

/// How many pictures the mean frame interval is taken over.
///
/// At twenty-five frames a second this is twenty seconds of the stream: long
/// enough that the jitter of a few hundred steps averages away, short enough
/// that a camera which changed its rate is followed while the viewer is still
/// watching. It is a window and not a total, because a session can last hours.
const LIVE_STEP_WINDOW: u64 = 512;

/// How far the clock may run from the timestamps before it is put back on them
/// rather than pulled towards them.
///
/// A pull is for jitter. A difference of seconds is a camera that restarted its
/// counter, or a clock the session has been away from, and closing that at a
/// fraction a frame would be a minute of wrong speed.
const LIVE_PHASE_SNAP: i64 = 4 * LIVE_CLOCK;

/// One picture waiting for its moment, with the timestamp that placed it.
struct HeldPicture {
    /// Ticks on the stream's 90 kHz clock, counted from the session's first
    /// picture rather than from the camera's power-on: RTP timestamps wrap
    /// every thirteen hours and a session outlives the wrap.
    at: i64,
    unit: AccessUnit,
}

/// Places live pictures on the clock their timestamps describe.
///
/// The clock is an anchor and not an accumulator: every moment is the anchor
/// plus the picture's own distance from the anchor's timestamp. An anchor that
/// moved with each handover would drift - a batch handed over late would move
/// every picture after it, and one that arrived early would pull them all
/// forward - which is the failure the first attempt at this had.
struct LivePacer {
    /// Whether a first picture has placed the anchor.
    started: bool,
    /// The wall clock the anchor's timestamp was placed at.
    anchor_wall: Instant,
    /// The stream timestamp the anchor was taken from, in unwrapped ticks.
    anchor_at: i64,
    /// The last raw timestamp seen, and where it sat once unwrapped.
    last_raw: u32,
    /// The place the anchor's picture holds on the clock, and the raw timestamp
    /// it was taken from: together they are what the timestamps' own idea of
    /// where a picture belongs is measured against.
    anchor_raw: u32,
    anchor_place: i64,
    /// Where the newest picture was placed, in ticks counted from the session's
    /// first one. This is the clock's own idea of the present, and it runs on
    /// the stream's frame interval rather than on its timestamps.
    held: i64,
    /// Every step between two pictures this clock has seen, summed, and how
    /// many: their mean is the interval pictures are placed apart by.
    ///
    /// Averaged and not smoothed: the average of a jittery step is itself exact,
    /// where a smoothed one keeps a share of the jitter - a share that a clock
    /// placing pictures one interval apart carries into their spacing. The window
    /// is bounded so a stream that changed its rate is followed in seconds and
    /// not in minutes; within a session the rate does not change.
    step_total: i64,
    step_count: u64,
    queue: VecDeque<HeldPicture>,
    /// How far ahead of its arrival a picture is placed, retuned from what the
    /// wire turns out to be doing. Never shorter than one frame.
    lead: Duration,
    /// The stream's own frame interval, smoothed from the timestamps.
    ///
    /// It sets the floor the lead may not go under, because a lead shorter than
    /// the distance between two pictures leaves nothing in hand: the next
    /// picture is handed the moment it arrives, and the wire's jitter goes
    /// straight to the screen.
    interval: Duration,
    /// The window the late pictures are counted in, and how many there were.
    window: Instant,
    late: u32,
    /// How much later than its own moment the latest picture of this window
    /// arrived.
    ///
    /// A lead is the distance between a picture's moment and its arrival, and
    /// the wire's pictures arrive in batches: a lead of one frame covers the
    /// gap within a batch and not the gap between two of them. A picture that
    /// arrived after its moment says so exactly, and says by how much - which is
    /// that much more lead. Measured at the arrival and not at the handover,
    /// because a slow decoder delays a handover too and is not the wire.
    starved: Duration,
    /// When the lead was last narrowed, which decides when it may be again.
    trimmed: Instant,
    /// Until this moment a late picture is not counted.
    ///
    /// Narrowing moves the clock forward, so the picture in front is late by
    /// exactly the step taken - which would be read as the wire needing the
    /// wider lead that was just given up, and the two would undo each other for
    /// ever.
    settle: Instant,
    /// What the session did with the clock since it opened, for the window's
    /// line: handed pictures, pictures the clock could not hold, and the moments
    /// the clock had to be moved to a picture rather than the other way round.
    handed: u64,
    dropped: u64,
    moved: u64,
    /// Whether the clock is still opening, and how many pictures were folded
    /// into the one moment they are all shown at.
    ///
    /// A stream that was already sending when the session joined arrives as a
    /// crowd - the forwarder flushing what it buffered - and anchoring on the
    /// first of those pictures would keep the viewer that far behind the wire
    /// for as long as the session lasts.
    ///
    /// The fold ends at the first picture the loop actually hands over, which
    /// covers a crowd that arrives before the first moment comes: with the lead
    /// at a hundred and sixty milliseconds, two pictures of a twenty-five frame
    /// stream. A forwarder that flushes more than that is not lost, only caught
    /// late: the queue stays deeper than a batch, and `deep_since` folds it back
    /// to the present a second later, which the viewer sees as one jump rather
    /// than as half a second of lag.
    opening: bool,
    folded: u64,
    /// When the queue first went deeper than a batch, if it still is.
    deep_since: Option<Instant>,
    widened: u64,
    narrowed: u64,
    /// Whether the last thing that moved the lead was a narrowing rather than a
    /// widening. A widening that finds this set is undoing that narrowing, which
    /// is what a bounce is counted from.
    pending_narrowing: bool,
    /// When that narrowing was made, so that one far enough back stops counting
    /// as the thing being undone. See [`LIVE_TRIM_BOUNCE_WINDOW`].
    narrowed_at: Option<Instant>,
    /// How many narrowings a widening has undone. At [`LIVE_TRIM_BOUNCES`] the
    /// stream has said twice that its lead is the one it needs, and is no
    /// longer narrowed this session.
    bounces: u8,
    /// Whether this stream keeps its lead. Set once it has bounced enough.
    settled: bool,
    /// Pictures handed over after their moment, for the window's line.
    behind: u64,
    /// The deepest the queue has been since the window's line was last written,
    /// which is the batch the clock had to hold.
    deepest: usize,
    /// The gaps between the pictures this clock actually handed over.
    ///
    /// Measured at the handover and not at the timestamps: the lead's whole job
    /// is to turn an uneven wire into an even pace, and this is the only place
    /// that says whether it did. A mean that matches the interval with a wide
    /// spread is a rhythm the eye reads as stutter even while every counter
    /// says the stream is fine.
    gap_count: u32,
    gap_total: Duration,
    gap_max: Duration,
    gap_min: Duration,
    last_handed: Option<Instant>,
}

impl Default for LivePacer {
    fn default() -> Self {
        Self {
            started: false,
            anchor_wall: Instant::now(),
            anchor_at: 0,
            last_raw: 0,
            anchor_raw: 0,
            anchor_place: 0,
            held: 0,
            step_total: 0,
            step_count: 0,
            queue: VecDeque::new(),
            lead: LIVE_LEAD,
            interval: LIVE_LEAD,
            window: Instant::now(),
            late: 0,
            starved: Duration::ZERO,
            trimmed: Instant::now(),
            settle: Instant::now(),
            handed: 0,
            dropped: 0,
            moved: 0,
            opening: false,
            folded: 0,
            deep_since: None,
            widened: 0,
            narrowed: 0,
            pending_narrowing: false,
            narrowed_at: None,
            bounces: 0,
            settled: false,
            behind: 0,
            deepest: 0,
            gap_count: 0,
            gap_total: Duration::ZERO,
            gap_max: Duration::ZERO,
            gap_min: Duration::MAX,
            last_handed: None,
        }
    }
}

impl LivePacer {
    /// Puts a picture on the clock, dropping the oldest when the clock holds
    /// more than the decoder can be expected to get through.
    fn push(&mut self, unit: AccessUnit, now: Instant) {
        let raw = unit.timestamp;
        let step = raw.wrapping_sub(self.last_raw) as i32;
        // A timestamp that jumped is not a frame interval: the camera restarted
        // its counter, or the session outlived a wrap this arithmetic could not
        // follow. The pictures held are for a clock that no longer applies.
        let jumped = step > (LIVE_CLOCK * 2) as i32 || step < -(LIVE_CLOCK as i32);
        if !self.started || jumped {
            self.clear();
            self.started = true;
            self.last_raw = unit.timestamp;
            self.anchor_raw = unit.timestamp;
            self.anchor_place = 0;
            self.held = 0;
            // The anchor is the moment the timestamp names, with no lead on it:
            // the lead is added when a picture's moment is worked out, so it can
            // be retuned without the anchor having to be moved with it.
            self.anchor_wall = now;
            self.anchor_at = 0;
            self.opening = true;
            self.queue.push_back(HeldPicture { at: 0, unit });
            return;
        }
        // A picture is placed one interval after the one before it rather than
        // where its own timestamp falls: two pictures that claim the same moment
        // are still two pictures, and stacking them on one moment would hand both
        // over at once and show only the second. Until an interval has been
        // learned there is nothing to place them apart by, and they go where the
        // timestamps say.
        self.last_raw = raw;
        let first_of_a_clock = self.step_count == 0;
        self.learn_the_interval(step);
        // Where the timestamps say this picture belongs, counted from the
        // anchor's picture: the stream's own idea of the present.
        let truth = self.anchor_place + i64::from(raw.wrapping_sub(self.anchor_raw));
        // And where the clock puts it: one learned interval after the picture
        // before it, so a step the camera made long or short is even motion
        // rather than motion that speeds up and slows down. What the two differ
        // by is taken back a fraction at a time, which is what keeps the even
        // clock from drifting away from the stream; a difference too large to be
        // jitter puts the clock back on the timestamps instead of crawling there.
        if first_of_a_clock {
            self.held = truth;
        } else {
            self.held += self.step_ticks();
            let error = truth - self.held;
            if error.abs() > LIVE_PHASE_SNAP {
                self.held = truth;
            } else {
                self.held += error / LIVE_PHASE_GAIN;
            }
        }
        let at = self.held;
        self.queue.push_back(HeldPicture { at, unit });
        // Nothing has been handed over yet, so the clock is still finding where
        // the stream is now. A stream that opened while the camera was already
        // sending arrives as a crowd - a forwarder flushing what it buffered
        // before the session joined - and anchoring on the first of those would
        // pin the viewer that far behind the wire for as long as the session
        // lasts. The anchor follows the newest instead: the crowd ends up
        // sharing one moment, and the pictures before it are decoded as fast as
        // the decoder takes them, which is a stream settling and not a stutter.
        if self.opening {
            self.reanchor(at, raw, now);
            self.folded += 1;
        }
        while self.queue.len() > LIVE_MAX_HELD {
            // Give up a run rather than a picture. Every picture but the newest
            // is predicted from the one before it, so dropping the oldest
            // non-key frame leaves everything behind it without its reference:
            // the decoder refuses those pictures or paints garbage until the
            // next key frame, and the ones that were "kept" were never
            // showable. What goes is the run before the oldest key frame that
            // has another key frame after it, so what stays starts on a key
            // frame and carries its own chain.
            let cut = self
                .queue
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, held)| held.unit.keyframe)
                .map(|(index, _)| index);
            match cut {
                Some(index) => {
                    for _ in 0..index {
                        if self.queue.pop_front().is_some() {
                            self.dropped += 1;
                        }
                    }
                }
                // One key frame, and it is the first: nothing in this queue can
                // be given up without orphaning what follows. The newest goes
                // instead - a picture the viewer has not seen yet - and the
                // chain stays whole. A queue that stays this deep is the crowd
                // rule's business, not this one's.
                None => {
                    if self.queue.pop_back().is_some() {
                        self.dropped += 1;
                    }
                }
            }
            // The clock moves to the picture that is now first, so what is
            // given up is the past and not the delay.
            if let Some(front) = self.queue.front() {
                let (at, raw) = (front.at, front.unit.timestamp);
                self.reanchor(at, raw, now);
            }
        }
        // A picture that arrived after its own moment is one the clock was
        // waiting for, and how far past it came is how much more lead this
        // stream needs. Not counted while the clock is still opening: every
        // picture of a stream that was already sending arrives late, and the
        // fold is what settles that. And not counted while the clock is
        // settling either: a narrowing moves every moment forward by the step
        // it gave up, so the pictures that follow arrive late by exactly that
        // step - reading it as the wire needing the lead just given up is what
        // makes a narrowing undo itself for ever. `due` guards the handover
        // the same way, and for the same reason.
        if !self.opening && now >= self.settle {
            let late_by = now.saturating_duration_since(self.moment(at));
            if late_by > self.starved {
                self.starved = late_by;
            }
        }
        self.deepest = self.deepest.max(self.queue.len());
        // A batch fills the queue for a frame or two and is gone. A queue still
        // deeper than a batch a second later is not a batch: it is a wire that
        // was already sending when the session joined, and the pictures in it are
        // the past. The clock moves to the newest of them, and the ones before it
        // are decoded as fast as the decoder takes them - a stream settling, not a
        // viewer left half a second behind for as long as the session lasts.
        if self.queue.len() > LIVE_BATCH_PICTURES {
            let since = *self.deep_since.get_or_insert(now);
            if now.duration_since(since) >= LIVE_CROWD_AFTER {
                if let Some(back) = self.queue.back() {
                    let (at, raw) = (back.at, back.unit.timestamp);
                    self.reanchor(at, raw, now);
                    self.folded += 1;
                }
                self.deep_since = None;
            }
        } else {
            self.deep_since = None;
        }
    }

    /// The wall clock a moment on the stream's clock falls at.
    ///
    /// The anchor's own moment, plus the lead, plus the distance the timestamp
    /// names. The lead is not baked into the anchor, so widening or narrowing it
    /// moves every picture already holding a place by the same amount and leaves
    /// the clock itself where it was.
    fn moment(&self, at: i64) -> Instant {
        let ahead = (at - self.anchor_at).max(0) as u64;
        self.anchor_wall
            + self.lead
            + Duration::from_micros(ahead.saturating_mul(100) / 9)
    }

    /// One frame of this stream, in ticks: the mean step between two pictures.
    fn step_ticks(&self) -> i64 {
        if self.step_count == 0 {
            // Nothing learned yet, and a session that has only seen one picture
            // is placed by its timestamp anyway.
            LIVE_CLOCK / 25
        } else {
            self.step_total / self.step_count as i64
        }
    }

    /// Learns how far apart this stream's pictures are, and keeps the lead at
    /// least one of those apart.
    ///
    /// Smoothed, so one picture that came eighty milliseconds after the one
    /// before does not redefine the stream's rate; and bounded, so a repeated
    /// timestamp or a camera that stopped for a second is not mistaken for a
    /// frame interval.
    fn learn_the_interval(&mut self, step: i32) {
        let gap = Duration::from_micros((step.max(0) as u64).saturating_mul(100) / 9);
        if gap < LIVE_INTERVAL_MIN || gap > LIVE_INTERVAL_MAX {
            return;
        }
        // Kept in ticks, which is what pictures are placed apart by: a duration
        // rounded to microseconds would come back as seven thousand four hundred
        // and seventy for a stream that steps seven thousand five hundred, and a
        // clock a third of a millisecond a frame short drifts.
        // The first step is taken as it is: a stream that opens at twelve
        // frames a second would otherwise be placed forty milliseconds apart
        // and take a second to learn better, and the viewer would see that
        // second as motion running fast.
        self.step_total += i64::from(step.max(0));
        self.step_count += 1;
        if self.step_count > LIVE_STEP_WINDOW {
            // Halved rather than cleared, which keeps the mean and forgets the
            // pictures of a rate this stream no longer has.
            self.step_total /= 2;
            self.step_count /= 2;
        }
        self.interval =
            Duration::from_micros((self.step_ticks().max(0) as u64).saturating_mul(100) / 9);
        // One frame in hand is the least that lets a picture be placed rather
        // than played out where it landed. Nothing over the ceiling: a stream so
        // slow that a frame is a fifth of a second would otherwise put the
        // viewer that far behind the door it is watching.
        let floor = self.interval.min(LIVE_LEAD_MAX);
        if floor > self.lead {
            self.lead = floor;
        }
    }

    /// Moves the clock to a picture: that picture's moment becomes now.
    ///
    /// The lead comes off the anchor rather than going onto the picture. Moving
    /// the clock has to cost nothing: the pictures still queued keep the
    /// distances between them, and if the move also re-applied the lead the
    /// queue would grow by it every time - a backlog that feeds itself, which is
    /// what a clock moved this way did once.
    fn reanchor(&mut self, at: i64, raw: u32, now: Instant) {
        self.anchor_at = at;
        self.anchor_wall = now.checked_sub(self.lead).unwrap_or(now);
        // The place the timestamps are measured from moves with the clock, or
        // the distance to them would be read against a picture that is no longer
        // where the clock says the present is.
        self.anchor_raw = raw;
        self.anchor_place = at;
    }

    /// The lowest the lead may be narrowed to: a setting, and one frame.
    fn lead_floor(&self) -> Duration {
        LIVE_LEAD_MIN.max(self.interval.min(LIVE_LEAD_MAX))
    }

    /// The next picture whose moment has come, if there is one.
    ///
    /// A picture whose moment passed long ago is not shown late: that is a
    /// decoder that fell behind, and catching up by holding the clock back
    /// would turn a stall into a growing delay.
    fn due(&mut self, now: Instant) -> Option<AccessUnit> {
        self.retune(now);
        let front = self.queue.front()?;
        let moment = self.moment(front.at);
        if moment > now {
            return None;
        }
        // A picture that was already due when the loop reached it is the one
        // measurement of the lead that means anything - but only a picture late
        // enough to say the queue had run out. A few tens of milliseconds is the
        // loop reaching a picture a fraction of a refresh late, which costs
        // nothing: the picture is still decoded and still shown.
        if now >= self.settle && now.duration_since(moment) > LIVE_STRAIN {
            self.late += 1;
        }
        if now.duration_since(moment) > Duration::from_millis(1) {
            self.behind += 1;
        }
        if now.duration_since(moment) > LIVE_LATE_LIMIT {
            let (at, raw) = (front.at, front.unit.timestamp);
            self.reanchor(at, raw, now);
            self.moved += 1;
        }
        let held = self.queue.pop_front();
        if held.is_some() {
            self.handed += 1;
            // The pace the viewer got, as opposed to the pace the timestamps
            // asked for. Taken here because this is the moment the picture is
            // let go, and a lead that is being trimmed too far shows up as a
            // spread between these gaps before it shows up anywhere else.
            if let Some(previous) = self.last_handed.replace(now) {
                let gap = now.saturating_duration_since(previous);
                self.gap_count += 1;
                self.gap_total += gap;
                self.gap_max = self.gap_max.max(gap);
                self.gap_min = self.gap_min.min(gap);
            }
            // The opening is over: the clock has hands on it now, and what
            // arrives next is placed after this picture rather than before it.
            self.opening = false;
        }
        held.map(|held| held.unit)
    }

    /// The mean of the handover gaps of this window, or zero before any.
    fn gap_mean(&self) -> Duration {
        if self.gap_count == 0 {
            return Duration::ZERO;
        }
        self.gap_total / self.gap_count
    }

    /// Forgets the handover gaps, leaving the last one in place so the first gap
    /// of the next window is measured from a real handover and not from nothing.
    fn clear_gaps(&mut self) {
        self.gap_count = 0;
        self.gap_total = Duration::ZERO;
        self.gap_max = Duration::ZERO;
        self.gap_min = Duration::MAX;
    }

    /// Retunes the lead from what the wire did in the last window.
    ///
    /// Widening is immediate and narrowing waits fifteen seconds without a single
    /// late picture - the shape a jitter buffer uses, and for the same reason: a
    /// clock that narrows eagerly oscillates between two settings, and a delay
    /// that breathes is worse than one that is slightly too wide. Neither
    /// direction goes under one frame's worth, below which there is nothing to
    /// place and the wire's jitter reaches the screen.
    ///
    /// A narrowing is also on probation: a widening in the window right after it
    /// is the stream taking back the step the narrowing gave up, and two of those
    /// stop the stream being narrowed at all. Without that, a low-frame-rate
    /// stream whose decode occasionally overruns the margin narrows, rebounds and
    /// narrows again for as long as the session lasts, and every one of those
    /// steps is a picture's moment moving under the viewer.
    fn retune(&mut self, now: Instant) {
        if now.duration_since(self.window) < LIVE_RETUNE {
            return;
        }
        self.window = now;
        let mut widened_now = false;
        if self.starved > Duration::ZERO {
            // What the wait cost, at least a step and at most four: one slow
            // handover should not open the buffer to its ceiling, and a stream
            // that starves for a tenth of a second should not take ten windows
            // to say so.
            let add = self
                .starved
                .clamp(LIVE_LEAD_STEP, LIVE_LEAD_STEP * 4);
            let widened = (self.lead + add).min(LIVE_LEAD_MAX);
            if widened > self.lead {
                self.lead = widened;
                self.widened += 1;
                widened_now = true;
            }
            self.trimmed = now;
        } else if self.late > 0 {
            let widened = (self.lead + LIVE_LEAD_STEP).min(LIVE_LEAD_MAX);
            if widened > self.lead {
                self.lead = widened;
                self.widened += 1;
                widened_now = true;
            }
            self.trimmed = now;
        } else if !self.settled && now.duration_since(self.trimmed) >= LIVE_TRIM_AFTER {
            let narrowed = self.lead.saturating_sub(LIVE_LEAD_STEP).max(self.lead_floor());
            if narrowed < self.lead {
                self.lead = narrowed;
                self.narrowed += 1;
                // The clock just moved forward by the step it gave up; the
                // pictures that move with it are not the wire being late.
                self.settle = now + LIVE_RETUNE;
                self.pending_narrowing = true;
                self.narrowed_at = Some(now);
            }
            self.trimmed = now;
        }
        // A widening that comes while the last thing to move the lead was a
        // narrowing is that narrowing being undone by the stream itself, not by
        // a passing hiccup: the pictures the narrowing moved forward arrived late
        // enough to say the lead it gave up was the one in use. Two of those and
        // this stream keeps its lead - the alternative is a delay that breathes
        // between two settings for as long as the session lasts, which is worse
        // than one a few milliseconds wide.
        //
        // Only while the narrowing is recent: one an hour back has been lived
        // with, and a widening now says something else.
        if self
            .narrowed_at
            .is_some_and(|at| now.duration_since(at) > LIVE_TRIM_BOUNCE_WINDOW)
        {
            self.pending_narrowing = false;
        }
        if widened_now && std::mem::take(&mut self.pending_narrowing) {
            self.bounces = self.bounces.saturating_add(1);
            if self.bounces >= LIVE_TRIM_BOUNCES {
                self.settled = true;
            }
        }
        self.late = 0;
        self.starved = Duration::ZERO;
    }

    /// When the picture in front is due, if any is waiting.
    fn next_due(&self) -> Option<Instant> {
        self.queue.front().map(|held| self.moment(held.at))
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.started = false;
        self.held = 0;
        self.anchor_raw = 0;
        self.anchor_place = 0;
        self.step_total = 0;
        self.step_count = 0;
        self.starved = Duration::ZERO;
        self.opening = false;
        self.deep_since = None;
        // A reconnect is a different stream until it says otherwise: a camera
        // that restarted may have a new group of pictures or a new key frame
        // interval, and the proof that the old one needed its lead is not
        // proof about this one. The lead itself is kept - it is where the last
        // stream left the clock, and starting wide and narrowing is the safe
        // direction - but what was learned about trimming it is not.
        self.pending_narrowing = false;
        self.narrowed_at = None;
        self.bounces = 0;
        self.settled = false;
    }
}

/// The state one RTSP session carries between packets.
///
/// Everything the loop carried from one iteration to the next is a field,
/// because the loop is answered by methods that each own one question - the
/// clocks, the talk-back, one media packet, the window - and a method cannot
/// reach a local. The split is the reason the fields are here; the fields
/// themselves are what the loop always held.
struct RtspSession<'a> {
    /// What it publishes through, and the two hubs it shares with the rest of
    /// the application.
    sinks: Sinks<'a>,
    audio: &'a AudioHub,
    talk: &'a TalkHub,
    /// What the session is about, fixed once it has negotiated.
    index: usize,
    camera_id: String,
    stream: StreamKind,
    kind: Codec,
    codec: Option<String>,
    payload_type: Option<u8>,
    keep_alive_interval: Duration,
    /// The connection, and what it is carrying.
    client: RtspClient,
    transport: RtspTransport,
    width: Option<u32>,
    height: Option<u32>,
    audio_decoder: Option<AudioTrack>,
    talk_sender: Option<(TalkStream, u8)>,
    /// The picture's path, from the packet to the frame store.
    decoder: Option<Box<dyn VideoDecoder>>,
    depacketizer: Depacketizer,
    /// Where each picture waits for the moment its timestamp names.
    pacer: LivePacer,
    baseline_fix: BaselineFix,
    sequence: u64,
    decode_errors: u64,
    sps_logged: bool,
    /// Pictures the decoder handed back that the wall has not been shown yet.
    ///
    /// Filled when an access unit's turn comes and emptied one per turn: a
    /// decoder that runs a frame or two behind hands two back at once, and the
    /// wall keeps only the newest of what it is given. See [`Self::collect`].
    pending: VecDeque<DecodedFrame>,
    /// The gaps between the pictures this session handed to the frame store.
    ///
    /// The pacer's own gaps say when the access units left; these say when the
    /// pictures were shown, and the wall can only follow the second.
    pub_last: Option<Instant>,
    pub_count: u32,
    pub_total: Duration,
    pub_max: Duration,
    pub_min: Duration,
    /// What it counts, and what it has already announced.
    window: StatsWindow,
    total_frames: u64,
    announced: bool,
    /// The clocks it keeps: the probe, the report, the talk-back, and the one
    /// that decides the session is dead.
    last_keep_alive: Instant,
    last_report: Instant,
    last_talk: Instant,
    stall: StallDeadline,
    /// The numbers the receiver report carries.
    source_ssrc: u32,
    received_packets: bool,
    sequence_window: SequenceWindow,
    jitter: Jitter,
}

impl<'a> RtspSession<'a> {
    /// Negotiates the session, and takes everything the packet loop will need.
    async fn open(
        endpoint: Endpoint,
        prefer_hardware: bool,
        handshake_timeout: Duration,
        sinks: Sinks<'a>,
        audio: &'a AudioHub,
        talk: &'a TalkHub,
    ) -> Result<Self> {
        let Endpoint { index, camera_id, uri, credentials, stream, transport, talk: wants_talk, .. } =
            endpoint;
        let uri = uri.as_str();
        let mut client = RtspClient::connect_with_auth(uri, credentials.clone()).await?;
        // A server may take one transport and refuse the other, which is worth a
        // second attempt rather than a failed session: go2rtc answers every UDP
        // `SETUP` with 461 on purpose, so that a client asking for UDP first falls
        // back, and a preference must never cost a channel its picture. A UDP
        // handshake that simply goes quiet from `SETUP` onwards is answered the same
        // way - see `HandshakeFailure::worth_the_other_transport`. The session is
        // opened afresh rather than patched, because the failed attempt left the
        // old one half set up.
        let mut transport = transport;
        let tracks = match negotiate(&mut client, transport, handshake_timeout, index, wants_talk).await {
            Ok(tracks) => tracks,
            Err(failure) if failure.worth_the_other_transport(transport) => {
                tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    "the udp session did not come up, reopening the session over tcp"
                );
                client = RtspClient::connect_with_auth(uri, credentials).await?;
                transport = RtspTransport::Tcp;
                negotiate(&mut client, transport, handshake_timeout, index, wants_talk)
                    .await
                    .map_err(HandshakeFailure::into_error)?
            }
            Err(failure) => return Err(failure.into_error()),
        };
        let video = tracks.iter().find(|track| track.is_video());
        let codec = video.and_then(|track| track.encoding.clone());
        // The SDP's resolution is a hint: `a=framesize` is optional, and where it
        // is missing the size comes from the sequence parameter set below.
        let mut width = video.and_then(|track| track.width);
        let mut height = video.and_then(|track| track.height);
        let payload_type = video.and_then(|track| track.payload_type);
        let parameter_sets = video.map(|track| track.parameter_sets.clone()).unwrap_or_default();
        let kind = Codec::from_encoding(codec.as_deref().unwrap_or_default());

        // The camera's audio, when it carries a track the client receives. The
        // decoder is opened once here and fed only while this channel is the audible
        // one. A codec with no backend yet (AAC, until the FFmpeg path lands) fails
        // at `configure`, and the channel simply plays no sound. The sound travels
        // interleaved with the picture on the TCP transport only, so a UDP session
        // has none to hear whatever its SDP lists.
        let audio_decoder = if transport == RtspTransport::Tcp {
            audio_track_decoder(&tracks)
        } else {
            None
        };

        // The talk-back track, when this session asked for one and the answer
        // carried it. Its codec is the one the media advertises - PCMU or PCMA in
        // practice - and a media this build cannot encode is reported as absent
        // rather than half used: the bar's microphone is drawn on this flag.
        let talk_track = tracks.iter().find(|track| track.is_audio() && track.is_sendonly());
        let talk_codec = talk_track
            .map(|track| AudioCodec::from_encoding(track.encoding.as_deref().unwrap_or_default()));
        let talk_sender = client.talk_rtp_channel().and_then(|channel| {
            let stream = TalkStream::new(talk_codec?)?;
            Some((stream, channel))
        });
        let backchannel = talk_sender.is_some();
        if backchannel {
            // This session is the one carrying the microphone, which only happens
            // when the camera would not take a call of its own. The handshake took
            // as long here as it would have there, so the backlog the microphone
            // built up is cut back the same way - see `talk_worker`.
            talk.trim(2 * FRAME_SAMPLES);
        }
        if wants_talk && !backchannel {
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                offered = ?talk_track.map(|track| track.encoding.clone()),
                "this session offers no usable back channel, the camera cannot be talked to"
            );
        }

        // `a=framesize` is optional, but a decoder has to be sized before its first
        // picture arrives: Android builds its image reader to that size, and a
        // reader that does not match the stream cannot be read back at all. The
        // sequence parameter set the SDP advertises carries the real size, so it is
        // read from there when the SDP itself did not name one.
        if width.is_none() || height.is_none() {
            if let Some((sps_width, sps_height)) =
                parameter_sets.iter().find_map(|nal| sps::picture_size(nal))
            {
                width = width.or(Some(sps_width));
                height = height.or(Some(sps_height));
            }
        }

        let decoder = video_decoder(kind, width, height, index, prefer_hardware);
        // Each codec arrives in its own shape over RTP: an H.264 access unit spread
        // over NAL units, an H.265 one in the RFC 7798 shape - two byte headers, AP
        // and FU instead of STAP and FU-A - or the fragments of a single JPEG scan.
        let depacketizer = match kind {
            Codec::Mjpeg => Depacketizer::Jpeg(JpegDepacketizer::new(payload_type)),
            Codec::H265 => Depacketizer::Hevc(HevcDepacketizer::new(payload_type, parameter_sets)),
            _ => Depacketizer::H264(H264Depacketizer::new(payload_type, parameter_sets)),
        };
        let sequence: u64 = 0;
        let decode_errors: u64 = 0;

        tracing::debug!(
            target: "xgview::pipeline",
            index,
            codec = ?codec,
            resolution = ?video.and_then(|track| track.resolution()),
            // The size the decoder is actually opened with: `a=framesize` when the
            // SDP carried one, the sequence parameter set's when it did not, and
            // nothing when neither did. A hardware hwaccel is set up at open time,
            // and a decoder opened without a size is one it can refuse - which is
            // why this is worth a field of its own rather than left to the SDP's
            // optional answer.
            size = ?width.zip(height),
            decoder = ?decoder.as_ref().map(|decoder| decoder.name()),
            // The parameter sets the SDP advertised, by NAL type and length. A
            // camera whose SDP carries none is served from the stream, which has to
            // reach a key frame before the first picture decodes.
            sets = ?depacketizer.parameter_sets(),
            "session negotiated"
        );

        sinks.stats.publish(StreamStats {
            index,
            camera_id: camera_id.clone(),
            stream,
            codec: codec.clone(),
            width,
            height,
            hardware: uses_hardware(&decoder),
            audio: audio_decoder.is_some(),
            fps: 0.0,
            bitrate_kbps: 0.0,
            total_frames: 0,
        });

        let total_frames: u64 = 0;
        // What this window has counted, and when the next sample is due. See
        // `WindowCounters` for what each counter means.
        let window = StatsWindow::new();
        let announced = false;
        // The stream's own parameter set, logged once per session. It is the only
        // place the profile, the level, the chroma sampling and the bit depth
        // appear, and a hardware decoder that turns a stream down is otherwise a
        // refusal with no visible cause.
        let sps_logged = false;
        // The plain-Baseline declaration a hardware decoder has no mode for; the
        // flag that says otherwise is set before any picture is handed over. See
        // [`BaselineFix`]. This is the session that may rewrite a set past that
        // flag: every access unit here carries the parameter sets, so a decoder
        // reopened on the next one is configured from what the camera sends.
        let baseline_fix = BaselineFix::new_live();
        // The server announces how much silence it tolerates. The probe is sent
        // before the read, never after: a check placed after a blocking read is
        // starved for as long as the camera stays quiet.
        let keep_alive_interval = client.keep_alive_interval();
        let last_keep_alive = Instant::now();
        // When the report is due, and which stream it is about. The counters it
        // carries are the two below.
        let last_report = Instant::now();
        let source_ssrc: u32 = 0;
        let received_packets = false;
        // The sequence numbers, folded the way the receiver report counts them, and
        // the gaps the datagram transport leaves behind. Both accumulate over the
        // session, which is the window RFC 3550 defines them in. See
        // [`SequenceWindow`].
        let sequence_window = SequenceWindow::new();
        // RTP timestamp units per second, as the session description announced it.
        let clock_rate = f64::from(video.and_then(|track| track.clock_rate).unwrap_or(90_000));
        // Interarrival jitter, the third number the report carries: kept as the
        // difference between consecutive packets rather than against a session
        // origin, because the timestamp wraps and an absolute transit would read
        // that wrap as one enormous jitter. See [`Jitter`].
        let jitter = Jitter::new(clock_rate);
        // How long this session may go without a picture, learned from the stream
        // itself: see `StallDeadline`.
        let stall = StallDeadline::new();

        // The talk-back clock: one packet every 20 ms, silence included, which is
        // what keeps the camera's speaker media alive while the viewer has the
        // microphone switched on. Nothing is sent when the session has no back
        // channel.
        let last_talk = Instant::now();

        Ok(Self {
            sinks,
            audio,
            talk,
            index,
            camera_id,
            stream,
            kind,
            codec,
            payload_type,
            keep_alive_interval,
            client,
            transport,
            width,
            height,
            audio_decoder,
            talk_sender,
            decoder,
            depacketizer,
            pacer: LivePacer::default(),
            baseline_fix,
            sequence,
            decode_errors,
            sps_logged,
            pending: VecDeque::new(),
            pub_last: None,
            pub_count: 0,
            pub_total: Duration::ZERO,
            pub_max: Duration::ZERO,
            pub_min: Duration::MAX,
            window,
            total_frames,
            announced,
            last_keep_alive,
            last_report,
            last_talk,
            stall,
            source_ssrc,
            received_packets,
            sequence_window,
            jitter,
        })
    }

    /// The packet loop: one iteration per packet, or per clock that came due.
    async fn pump(&mut self) -> Result<()> {
        loop {
            // The channel this session belongs to has been given to another one,
            // or parked. Stopping here rather than at the next abort is what
            // keeps two sessions off one channel: see `ChannelRuntime::stop`.
            if self.sinks.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            // Talk-back, before waiting on the picture: the session's clock is the
            // RTP clock, and it must not be tied to how often a camera sends video.
            self.pump_talk();
            // Before anything that can wait: a picture whose moment has come is
            // owed to the viewer now, and the wait below is measured from here.
            self.hand_over_due();
            let late = self.stall.lateness();
            self.check_stall(late)?;
            self.keep_alive().await?;
            self.send_report();
            let wait = self.next_wait(late);

            // The wait is a peek at the transport and the packet is only read once
            // something has arrived: a read cancelled part way through a packet
            // takes the bytes it has already consumed with it, and every packet
            // after them is then read at the wrong offset, which costs the session
            // where the wait would only have cost a wait.
            match tokio::time::timeout(wait, self.client.wait_for_media()).await {
                Ok(result) => result?,
                Err(_) => {
                    // The talk clock waking the loop is not silence on the wire.
                    if self.talk_sender.is_some()
                        && self.last_talk.elapsed() >= talk_gap(self.talk.talking_on(self.index))
                    {
                        continue;
                    }
                    // A wait the picture's own clock cut short is neither silence
                    // on the wire nor a stall: the next picture is simply not due
                    // yet, and the loop has nothing to read until it is.
                    if !self.pacer.is_empty() {
                        continue;
                    }
                    // A wait that the picture deadline cut short is not silence on
                    // the wire, and reporting it as such would name the wrong fault
                    // and the wrong duration. The stall check reports that one.
                    // A channel with no decoder is exempt: its wait was the full
                    // read timeout, not a deadline slice, so an expiry here really
                    // is silence on the wire.
                    if self.decoder.is_some() && self.stall.since_picture() >= late.patience {
                        continue;
                    }
                    return Err(crate::error::CoreError::timeout(format!(
                        "no media received for {} s",
                        READ_TIMEOUT.as_secs()
                    )));
                }
            }
            // Channel 0 carries RTP, channel 1 carries RTCP, on the interleaved
            // transport. UDP has no channels: `read_media` returns RTP only and
            // drains the control socket itself, so only RTP reaches here.
            let (channel, payload) = match self.client.read_media().await? {
                MediaPacket::Rtp(payload) => (0u8, payload),
                MediaPacket::Rtcp(payload) => (1u8, payload),
                MediaPacket::AudioRtp(payload) => (2u8, payload),
                MediaPacket::AudioRtcp(_) => (3u8, Vec::new()),
            };

            // Channel 2 carries the audio RTP.
            if channel == 2 {
                self.on_audio(&payload);
                continue;
            }
            if channel == 3 {
                continue;
            }

            // Channel 0 carries RTP, channel 1 carries RTCP.
            if channel == 0 {
                if let Some(header) = parse_rtp_header(&payload) {
                    if !self.on_rtp(&header, &payload) {
                        continue;
                    }
                    self.on_video(&header, &payload);
                }
            }
            self.publish_window();
        }
    }

    /// Writes the talk-back packets the clock owes. Nothing is written when the
    /// session has no back channel.
    fn pump_talk(&mut self) {
        if let Some((stream, channel)) = self.talk_sender.as_mut() {
            while self.last_talk.elapsed() >= talk_gap(self.talk.talking_on(self.index)) {
                let talking = self.talk.talking_on(self.index);
                let elapsed = self.last_talk.elapsed();
                let packet = if talking {
                    // A gap left by the idle rate is silence the speaker never
                    // heard: the clock is moved over it before the voice starts,
                    // so the talk does not begin a second in the past.
                    if elapsed > TALK_INTERVAL * 2 {
                        stream.skip(elapsed - TALK_INTERVAL);
                    }
                    stream.packet(&self.talk.take(FRAME_SAMPLES))
                } else {
                    stream.silence_after(elapsed)
                };
                self.client.send_interleaved(*channel, &packet);
                if talking {
                    self.last_talk += TALK_INTERVAL;
                } else {
                    self.last_talk = Instant::now();
                }
                // A loop that fell well behind - a stalled socket, a suspended
                // process - is not worth a burst of catch-up packets: the camera's
                // speaker wants the present, not the backlog.
                if self.last_talk.elapsed() > TALK_INTERVAL * 4 {
                    self.last_talk = Instant::now();
                    break;
                }
            }
        }
    }

    /// Holds the last frame while the picture is frozen, and gives the session up
    /// when it has been frozen for longer than reconnecting would take.
    fn check_stall(&mut self, late: Lateness) -> Result<()> {
        // A channel with no decoder has no picture to expect - it shows a
        // placeholder - and is left out of this entirely.
        if self.decoder.is_none() || late.since < late.patience {
            return Ok(());
        }
        // Soft timeout: the picture has frozen, but the transport may still be
        // alive. Hold the last frame and keep waiting rather than dropping to a
        // black reconnect screen, which is the worse experience on a flaky link.
        // The state is announced once, when the stall begins, so the grid can mark
        // it without a flood of events.
        if self.stall.stalled_since.is_none() {
            self.stall.stalled_since = Some(Instant::now());
            emit(
                self.sinks.events,
                StreamEvent::State {
                    index: self.index,
                    camera_id: self.camera_id.clone(),
                    state: ConnectionState::Stalled,
                    // The stalled state leaves the last picture to the viewer
                    // and draws no detail; there is nothing to report here.
                    detail: None,
                },
            );
        }
        // Hard timeout: the stall has gone on long enough that reconnecting is
        // more likely to help than waiting.
        if self.stall.stalled_since.unwrap().elapsed() >= STALL_HARD_TIMEOUT {
            return Err(crate::error::CoreError::timeout(format!(
                "stalled for {} s, giving up",
                late.since.as_secs()
            )));
        }
        Ok(())
    }

    /// Sends the probe the server asked for, when it is due.
    async fn keep_alive(&mut self) -> Result<()> {
        if self.last_keep_alive.elapsed() < self.keep_alive_interval {
            return Ok(());
        }
        // Fire and forget. Waiting for the answer would discard every RTP packet
        // that arrives first, and block the pump for good on a device that never
        // answers. Only a failed write is an error.
        self.client.send_keep_alive().await?;
        self.last_keep_alive = Instant::now();
        Ok(())
    }

    /// Sends the receiver report, when it is due.
    fn send_report(&mut self) {
        if !self.received_packets || self.last_report.elapsed() < REPORT_INTERVAL {
            return;
        }
        // Cameras differ in how much they care about being reported to. Some
        // stream regardless, but one was measured sending a single key frame and
        // then nothing but `SEI` until a receiver report arrived, which a viewer
        // shows as a slideshow whose timecode jumps. The counts it carries are the
        // session's real ones, so a camera that does adapt to them has something
        // to adapt to.
        let report = crate::rtcp::receiver_report(
            RECEIVER_SSRC,
            self.source_ssrc,
            self.sequence_window.highest(),
            self.sequence_window.lost().min(i32::MAX as u64) as i32,
            self.jitter.value(),
        );
        self.client.send_rtcp(&report);
        self.last_report = Instant::now();
    }

    /// How long the loop may block before it has to look at its clocks again.
    ///
    /// Waiting no longer than the picture deadline allows, so that a session gone
    /// quiet is noticed on time rather than at the next packet.
    ///
    /// While stalled the patience is already spent, so the wait would be zero and
    /// the loop would spin. A one second poll keeps it from busy-waiting without
    /// blocking long enough to miss the hard timeout. A channel with no decoder has
    /// no picture to wait for and no stall state to read: the only signal it has
    /// is whether packets are still arriving, so it waits for the full read
    /// timeout and lets that be the one that reports silence on the wire.
    fn next_wait(&self, late: Lateness) -> Duration {
        let wait = if self.decoder.is_some() && late.since >= late.patience {
            STALL_HARD_TIMEOUT
                .saturating_sub(self.stall.stalled_since.unwrap().elapsed())
                .min(Duration::from_secs(1))
        } else if self.decoder.is_some() {
            READ_TIMEOUT.min(late.patience.saturating_sub(late.since))
        } else {
            READ_TIMEOUT
        };
        // A session feeding a camera's speaker has one more clock to honour: the
        // wait is cut so that the next talk packet is written on time even while
        // the picture keeps the loop from ever timing out. Idle, that clock is the
        // keepalive's, not the frame rate's.
        let talk_wait = self.talk_sender.as_ref().map(|_| {
            talk_gap(self.talk.talking_on(self.index)).saturating_sub(self.last_talk.elapsed())
        });
        let wait = match talk_wait {
            Some(talk_wait) => wait.min(talk_wait),
            None => wait,
        };
        // The picture in front is owed no earlier than its own moment, and
        // waking for it rather than for the next packet is what keeps a batch
        // from waiting out the whole gap between batches: the wire is quiet for
        // a hundred and fifty milliseconds after a batch, and three of the
        // pictures in it are due inside that silence.
        let paced = self
            .pacer
            .next_due()
            .map(|due| due.saturating_duration_since(Instant::now()))
            .unwrap_or(wait);
        // Never zero: a loop that is told not to wait would poll the transport
        // and the clock without ever parking, and could starve the runtime it
        // shares with the other channels.
        wait.min(paced).max(Duration::from_millis(1))
    }

    /// Decodes one audio packet and hands it to the sink, while this channel is
    /// the one being listened to and the sink is open.
    ///
    /// Otherwise the packet is dropped, which is what keeps the grid's nine tiles
    /// from all playing their microphones.
    fn on_audio(&mut self, payload: &[u8]) {
        if self.audio.wants(self.index) {
            // Nothing is playing, so nothing is being held for it: the sound at
            // the moment the sink is opened is the present, not a backlog from
            // before it was muted.
            if !self.audio.output().is_some_and(|output| output.is_active()) {
                if let Some(track) = self.audio_decoder.as_mut() {
                    track.waiting.clear();
                }
                return;
            }
            if let Some(output) = self.audio.output().filter(|output| output.is_active()) {
                if let Some(track) = self.audio_decoder.as_mut() {
                    if let Some(header) = parse_rtp_header(payload) {
                        let body = &payload[header.header_len..];
                        let mut pcm: Vec<i16> = Vec::with_capacity(512);
                        // RFC 3640 carries the AAC access units behind their
                        // AU headers; G.711 is the payload as it stands. The
                        // header field widths come from the track's fmtp.
                        let result = if track.info.codec == AudioCodec::Aac {
                            crate::audio::aac_access_units(
                                body,
                                track.aac.size_length,
                                track.aac.index_length,
                            )
                            .into_iter()
                            .try_for_each(|range| track.decoder.decode(&body[range], &mut pcm))
                        } else {
                            track.decoder.decode(body, &mut pcm)
                        };
                        if result.is_ok() && !pcm.is_empty() {
                            output.configure(&track.info);
                            // A sink that has just been opened is not the one the
                            // sound in hand was held for. What is queued belongs
                            // to the track before it, and to the sound is no
                            // longer the present: the sink starts from here.
                            if track.opened != track.info.sample_rate {
                                track.waiting.clear();
                                track.opened = track.info.sample_rate;
                            }
                            // Play the sound as it is decoded. A waterline
                            // here holds the sound back to match the picture's
                            // clock, but the picture's own delay is the
                            // forwarder's, which the sink cannot share: the
                            // result was a voice about half a second further
                            // behind than the version without a waterline.
                            // The sink's own buffer is delay enough. Keep
                            // `waiting` only as the thing a sink reopen drops.
                            output.push(&pcm);
                        }
                    }
                }
            }
        } else if let Some(track) = self.audio_decoder.as_mut() {
            // Not the channel being listened to. What it holds was held for the
            // moment it was decoded at, and by the time the viewer switches back
            // that moment is a quarter of a second gone: pushing it then puts the
            // past in front of the present, which the ear hears as a stutter
            // repeating itself.
            track.waiting.clear();
        }
    }

    /// Takes one RTP packet: the sequence numbers, the jitter, the counters and
    /// the announcement that the stream is running, and says whether it is worth
    /// decoding.
    fn on_rtp(&mut self, header: &RtpHeader, payload: &[u8]) -> bool {
        // UDP reorders and duplicates. A stale fragment fed to the
        // depacketizer would be spliced into the picture being
        // assembled, so such a packet is dropped before it reaches it.
        // The interleaved transport never does either.
        if self.transport.is_udp() {
            match self.sequence_window.accept(header.sequence) {
                Arrival::Stale => {
                    // A duplicate, or a straggler that belongs to a
                    // picture already handed over.
                    self.window.counters.reordered += 1;
                    return false;
                }
                Arrival::Gap(missing) => {
                    // The packets in between were lost, so the unit
                    // being assembled is missing part of its picture. It
                    // is written off rather than stitched together and
                    // decoded into a screenful of corruption.
                    self.window.counters.lost += u64::from(missing);
                    self.depacketizer.invalidate();
                }
                Arrival::Next => {}
            }
        }
        self.window.counters.bytes += payload.len() as u64;
        // The receiver report counts sequence numbers the way RFC 3550
        // does: the 16 bit RTP field folded with the number of times it
        // has wrapped, so a long session does not look like it restarted.
        self.source_ssrc = header.ssrc;
        self.received_packets = true;
        self.sequence_window.see(header.sequence);
        // How far apart this packet and the last one were sent, against
        // how far apart they arrived. RFC 3550 smooths the difference
        // into the jitter the receiver report carries.
        self.jitter.observe(header.timestamp, Instant::now());
        // The marker bit is set on the last packet of an access unit.
        if header.marker && header.payload_type < 192 {
            self.window.counters.frames += 1;
            self.total_frames += 1;
        }
        if self.payload_type.is_some_and(|expected| header.payload_type != expected) {
            self.window.counters.foreign += 1;
        }
        announce_streaming(
            self.sinks.events,
            self.index,
            &self.camera_id,
            &mut self.announced,
        );
        true
    }

    /// Hands the access units a packet completed to the decoder, and the pictures
    /// that come out of it to the frame store.
    fn on_video(&mut self, header: &RtpHeader, payload: &[u8]) {
        if self.decoder.is_none() {
            return;
        }
        let payload = &payload[header.header_len..];
        // A packet completes at most two pictures: the one the RTP
        // timestamp change released, and the one its marker closed.
        let units = self.depacketizer.push(header, payload);
        let completed_none = units.is_empty();
        for unit in units {
            self.window.counters.units += 1;
            self.window.counters.unit_bytes += unit.data.len() as u64;
            // Once per session, from the first access unit that
            // carries a parameter set of its own: a camera whose
            // SDP advertised none only ever says what it is here.
            // H.264's reader, so an H.265 access unit is left out of
            // it rather than read with H.264's layout.
            if self.kind == Codec::H264 && !self.sps_logged {
                if let Some(nal) = sps::access_unit_sps(&unit.data) {
                    if let Some(info) = sps::sps_info(nal) {
                        // What a hardware decoder screens the
                        // stream on: a profile or level it does not
                        // implement, a chroma sampling other than
                        // 4:2:0, more than eight bits per sample, a
                        // set carrying scaling matrices. One line
                        // per session, and the first place to look
                        // when a channel decodes on the CPU.
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index = self.index,
                            profile = info.profile,
                            level = info.level,
                            chroma = info.chroma,
                            bit_depth = info.bit_depth,
                            scaling_matrices = info.scaling_matrices,
                            size = ?info.size,
                            "stream parameter set"
                        );
                        self.sps_logged = true;
                    }
                }
            }
            self.pacer.push(unit, Instant::now());
        }
        if completed_none && header.marker {
            // The marker closed a picture but nothing came out of
            // it: the unit held no slice, or its head was lost.
            self.window.counters.dropped += 1;
        }
    }

    /// Hands the decoder every picture whose moment has come.
    ///
    /// One per call would be enough for a stream that keeps up, but not for one
    /// that arrived while the loop was busy: a batch of four is due within a
    /// hundred and twenty milliseconds, and a decoder that fell behind would be
    /// handed them a loop iteration apart instead of at once.
    fn hand_over_due(&mut self) {
        while let Some(unit) = self.pacer.due(Instant::now()) {
            self.decode_unit(unit);
        }
    }

    /// Decodes one picture, and hands what comes out of it to the frame store.
    ///
    /// Called once the picture's own timestamp says its moment has come, so
    /// the pictures reach the decoder at the pace the stream describes rather
    /// than at the pace the wire delivered them. See [`LivePacer`].
    fn decode_unit(&mut self, mut unit: AccessUnit) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        // A stream the decoder has watched come back out of order is one that
        // reorders, and the set must be left to say so - and the decoder that
        // was configured from the rewritten one has to go, or it keeps a
        // declaration the stream contradicts for the rest of the session. The
        // next access unit opens a new one on the sets as the camera sends
        // them.
        if decoder.reordering() && self.baseline_fix.retract(self.index) {
            decoder.restart();
        }
        self.baseline_fix.observe(self.index, self.kind, &mut unit.data);
        self.stall.observe(unit.keyframe, Instant::now());
        // Offered back to the platform decoder on a key frame, and only
        // there: a decoder cannot be resumed past a picture it never saw,
        // so a return to hardware that started mid-group would leave the
        // tile dark until the next one. A channel that fell back to the CPU
        // because the device was out of hardware sessions is the case this
        // is for - one of them frees up when a channel closes or the grid
        // is made smaller, and nothing else ever asks again.
        if unit.keyframe {
            decoder.try_upgrade();
        }
        let started = Instant::now();
        // Counted before and after, because a decoder given a surface keeps
        // its pictures: `decode` comes back empty on a stream that is
        // arriving perfectly well, and a session that read that as "no
        // picture" declared the channel stalled - an orange dot over a
        // moving picture, and a reconnect after the hard timeout.
        let pictures_before = decoder.pictures();
        let decoded = decoder.decode(&unit.data, unit.pts_us(), unit.keyframe);
        // A decoder rendering into the activity's surface hands back no frame,
        // so `release` cannot report a size from one: the picture is on screen
        // but its width and height never reach the OSD, which then shows the
        // last sub stream's size or `?x?`. The decoder knows the real size from
        // the stream's own parameter set - `open` wrote it into `info` - so
        // take it here, before `release` gets a chance to replace it with a
        // frame's size on the byte-buffer path. `collect` holds frames back and
        // `release` publishes the due one, so on a grid this write is replaced
        // by the frame's own size in the same call; on a surface there is no
        // frame and this is the size that stands.
        let info_size = decoder.info().and_then(|info| {
            (info.width > 0 && info.height > 0).then_some((info.width, info.height))
        });
        self.window.counters.decode_took(started.elapsed());
        match decoded {
            Ok(decoded) => {
                if let Some((width, height)) = info_size {
                    self.width = Some(width);
                    self.height = Some(height);
                }
                self.collect(decoded, pictures_before);
                self.release();
            }
            // One damaged picture is not worth a new session:
            // the encoder recovers at its next IDR.
            Err(err) => {
                // Pictures refused before the session has shown
                // one are its opening rather than damage. A
                // camera that sends a run of pictures before its
                // first IDR leaves the decoder nothing to
                // reference - one here spends its first thirty
                // five that way and then runs clean - so
                // counting them as errors would report a
                // working channel as a broken one at every
                // connect. They are counted apart, and the
                // warning is kept for a session that had a
                // picture and then lost it.
                if !self.stall.running {
                    self.window.counters.opening += 1;
                    if self.window.counters.opening == 1 {
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index = self.index,
                            %err,
                            "no picture yet, waiting for the first key frame"
                        );
                    }
                } else {
                    self.decode_errors += 1;
                    self.window.counters.errors += 1;
                    // The first rejections carry the payload
                    // that caused them, which names the NAL
                    // units the decoder was given.
                    if self.decode_errors <= 3 {
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index = self.index,
                            len = unit.data.len(),
                            types = ?nal_types(&unit.data, self.kind),
                            keyframe = unit.keyframe,
                            au = %hex(&unit.data, 24),
                            cached = ?self.depacketizer.parameter_sets(),
                            "access unit rejected by the decoder"
                        );
                    }
                    if self.decode_errors == 1 {
                        tracing::warn!(
                            target: "xgview::pipeline",
                            index = self.index,
                            %err,
                            "decoding failed, waiting for the next key frame"
                        );
                    } else {
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index = self.index,
                            errors = self.decode_errors,
                            %err,
                            "decoding failed"
                        );
                    }
                }
            }
        }
    }

    /// Takes what a decode produced, and shows one picture.
    ///
    /// The decode is counted and the pictures are queued; the wall is handed one
    /// of them, one per handover. That separation is the whole point: a hardware
    /// decoder runs a picture or two behind the access unit it was just given,
    /// so what it hands back arrives tied to the *next* handover - none on one
    /// turn and two on the next - and a session that published both at once
    /// published a picture the wall never showed, because a channel keeps only
    /// its newest. Measured on the Mi Pad's magnified view, a twelve frame
    /// 2560x1920 stream reached the screen as six pictures a second in bursts
    /// while the pacer reported handovers eighty-three milliseconds apart and no
    /// fault of its own. Keeping one back and letting it out on the next turn
    /// moves that picture instead of losing it, and every picture then reaches
    /// the screen on the turn its own timestamp asked for.
    ///
    /// The hold costs a picture's worth of delay on a stream whose decoder is
    /// late, which is what the lead in front of the whole pipeline exists for.
    fn collect(&mut self, decoded: Vec<DecodedFrame>, pictures_before: u64) {
        let pictures = self.decoder.as_ref().map_or(0, |decoder| decoder.pictures());
        // A picture came out, whatever the renderer can do with it: on the
        // platform whose frames stay in a GPU buffer there are no planes to
        // publish, and the stream is no less alive for it.
        if !decoded.is_empty() || pictures != pictures_before {
            let was_stalled = self.stall.stalled_since.is_some();
            // The first picture of a session, which is a recovery too: a
            // channel that was cut and has opened again is "reconnecting" as
            // far as the wall knows, and a tile in that state is painted over
            // with the pending canvas - the picture is there and nobody can see
            // it. A session that never says it is streaming leaves that canvas
            // up for as long as it lasts.
            let first_picture = !self.stall.running;
            self.stall.picture(Instant::now());
            if was_stalled || first_picture {
                emit(
                    self.sinks.events,
                    StreamEvent::State {
                        index: self.index,
                        camera_id: self.camera_id.clone(),
                        state: ConnectionState::Streaming,
                        detail: None,
                    },
                );
            }
        }
        self.window.counters.decoded += decoded.len() as u64;
        let decoded_len = decoded.len() as u64;
        if decoded_len > 0 {
            hold(&mut self.pending, decoded);
            return;
        }
        // A picture that went to the surface has no frame to publish, and it is
        // on screen all the same: counted here, or a session that works looks to
        // the channel like one that never came up - the mse address is tried,
        // the backoff is not reset, and the health line says `pictures=0` about
        // a picture the viewer is watching.
        //
        // Both branches below are the same fact - this access unit put no pixels
        // in the renderer's hands - so both are counted here and nowhere else. A
        // handover is one decode, and counting it again where the picture is let
        // go would make the number twice what it says and unreadable in either
        // direction.
        self.window.counters.undrawn_calls += 1;
        if pictures != pictures_before {
            self.sinks.frames.note_picture(self.index, None);
        }
    }

    /// Hands the wall the picture that is due, and keeps the rest.
    ///
    /// Called once per handover, which is what makes the screen's rhythm the
    /// pacer's rhythm rather than the decoder's.
    ///
    /// An empty hold is not counted: [`Self::collect`] already counted this
    /// handover as one that put no pixels in the renderer's hands, and the
    /// picture being due here is the same fact seen from the other side.
    fn release(&mut self) {
        let Some(frame) = self.pending.pop_front() else {
            return;
        };
        let now = Instant::now();
        if let Some(previous) = self.pub_last.replace(now) {
            let gap = now.saturating_duration_since(previous);
            self.pub_count += 1;
            self.pub_total += gap;
            self.pub_max = self.pub_max.max(gap);
            self.pub_min = self.pub_min.min(gap);
        }
        match publish_frames(self.sinks.frames, self.index, &mut self.sequence, vec![frame], None) {
            Some((picture_width, picture_height)) => {
                self.width = Some(picture_width);
                self.height = Some(picture_height);
                self.window.counters.drawn += 1;
            }
            None => {
                // The decode did produce a picture - it is `collect` that counts
                // the calls which produced none - but this one had no planes the
                // renderer could take. Count the picture, not another call.
                self.window.counters.undrawn_frames += 1;
            }
        }
    }

    /// Publishes the sample the window was counting, and starts the next one.
    fn publish_window(&mut self) {
        if !self.window.due() {
            return;
        }
        let seconds = self.window.seconds();
        let fps = self.window.counters.fps(seconds);
        let kbps = self.window.counters.kbps(seconds);
        self.sinks.stats.publish(StreamStats {
            index: self.index,
            camera_id: self.camera_id.clone(),
            stream: self.stream,
            codec: self.codec.clone(),
            width: self.width,
            height: self.height,
            hardware: uses_hardware(&self.decoder),
            audio: self.audio_decoder.is_some(),
            fps,
            bitrate_kbps: kbps,
            total_frames: self.total_frames,
        });
        // `units` is what the network delivered, `decoded` what came out of
        // the decoder: a frozen picture with a steady `units` blames the
        // decoder (or the renderer), a `units` that drops to zero and then
        // bursts blames the camera, the link, or this loop being starved.
        let counted = self.window.close();
        self.sinks.stats.add_bytes(counted.bytes);
        tracing::debug!(
            target: "xgview::pipeline",
            index = self.index,
            stream = self.stream.as_str(),
            hardware = uses_hardware(&self.decoder),
            // The picture deadline this session is being held to. It is
            // learned from the stream, so a channel cut off at a number of
            // seconds that is not the one in the source reads here as the
            // group of pictures that camera turned out to have.
            deadline_s = self.stall.patience().as_secs(),
            units = counted.units,
            unit_bytes = counted.unit_bytes,
            decoded = counted.decoded,
            errors = counted.errors,
            opening = counted.opening,
            dropped = counted.dropped,
            foreign = counted.foreign,
            reordered = counted.reordered,
            lost = counted.lost,
            slowest_decode_ms = counted.slowest.as_millis() as u64,
            // What the picture clock did with them. `handed` is the rate the
            // viewer sees the stream at, `held` the delay it added, and `moved`
            // the times the decoder had fallen far enough behind for the clock
            // to be moved to a picture instead of the picture shown late.
            handed = self.pacer.handed,
            held = self.pacer.queue.len(),
            // The deepest the queue has been in this window, which is the batch
            // the clock had to hold, and the pictures the loop reached after
            // their moment, which is the pace it could not keep.
            deepest = self.pacer.deepest,
            late_handed = self.pacer.behind,
            gave_up = self.pacer.dropped,
            moved_clock = self.pacer.moved,
            folded = self.pacer.folded,
            lead_ms = self.pacer.lead.as_millis() as u64,
            // The stream's own rate, which is the floor the lead is held at: a
            // doorbell at twelve frames a second is eighty milliseconds a frame.
            frame_ms = self.pacer.interval.as_millis() as u64,
            widened = self.pacer.widened,
            narrowed = self.pacer.narrowed,
            bounces = self.pacer.bounces,
            settled = self.pacer.settled,
            // The rhythm the viewer actually got: the mean handover gap, and
            // how far the fastest and slowest of them were from it. On a stream
            // that is being paced properly these sit either side of `frame_ms`
            // by a few milliseconds; a `gap_max_ms` near twice `frame_ms` is one
            // picture that came a frame late and the next one chasing it.
            gap_mean_ms = self.pacer.gap_mean().as_millis() as u64,
            gap_min_ms = if self.pacer.gap_count > 0 {
                self.pacer.gap_min.as_millis() as u64
            } else {
                0
            },
            gap_max_ms = self.pacer.gap_max.as_millis() as u64,
            // Pictures that reached the renderer at all, against `decoded`
            // above: a stream whose decoded count climbs while this one does
            // not is a tile that is being decoded for nobody.
            published = self.sinks.frames.published(self.index),
            undrawn_calls = counted.undrawn_calls,
            undrawn_frames = counted.undrawn_frames,
            drawn = counted.drawn,
            newest_seq = self.sinks.frames.newest_sequence(self.index).unwrap_or(0),
            pub_mean_ms = self.pub_mean().as_millis() as u64,
            pub_min_ms = if self.pub_count > 0 { self.pub_min.as_millis() as u64 } else { 0 },
            pub_max_ms = self.pub_max.as_millis() as u64,
            pub_count = self.pub_count,
            fps,
            kbps,
            "stream window"
        );
        // The depth is a window's own: what one batch held is not the story of
        // the whole session.
        self.pacer.deepest = self.pacer.queue.len();
        self.pacer.clear_gaps();
        self.pub_count = 0;
        self.pub_total = Duration::ZERO;
        self.pub_max = Duration::ZERO;
        self.pub_min = Duration::MAX;
    }

    /// The mean gap between the pictures this session published.
    fn pub_mean(&self) -> Duration {
        if self.pub_count == 0 {
            return Duration::ZERO;
        }
        self.pub_total / self.pub_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(timestamp: u32, keyframe: bool) -> AccessUnit {
        AccessUnit {
            data: vec![0; 8],
            timestamp,
            keyframe,
        }
    }

    /// One picture of a 25 frame a second stream, in 90 kHz ticks.
    const FRAME: u32 = 3600;

    /// Pushes a first picture and has the loop reach it, which is the moment the
    /// clock stops settling and starts placing what arrives.
    fn establish(pacer: &mut LivePacer) -> Instant {
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        assert!(pacer.due(start + pacer.lead).is_some(), "the first picture was not handed over");
        assert!(!pacer.opening, "the clock is still settling");
        start
    }

    /// A lead of one frame covers the gap within a batch. The gap between two
    /// batches is not in the frame interval at all, and the only way to learn it
    /// is to notice a picture arriving after its own moment - and by how much.
    #[test]
    fn a_picture_that_arrives_after_its_moment_widens_the_lead() {
        let mut pacer = LivePacer::default();
        let start = establish(&mut pacer);
        // The next picture arrives a quarter of a second after its moment: the
        // wire's batch gap, which the frame interval knows nothing about.
        pacer.push(picture(FRAME, false), start + Duration::from_millis(240));
        assert!(pacer.due(start + Duration::from_millis(240)).is_some());
        assert!(
            pacer.starved >= Duration::from_millis(150),
            "the wait was not measured: {:?}",
            pacer.starved
        );
        // The window closes and the lead takes on the shortfall it measured.
        let later = start + Duration::from_millis(2400);
        pacer.push(picture(FRAME * 2, false), later);
        assert!(pacer.due(later).is_some());
        assert_eq!(pacer.widened, 1, "the lead was not widened once");
        assert!(pacer.lead > LIVE_LEAD, "the lead did not widen: {:?}", pacer.lead);
        assert!(
            pacer.lead <= LIVE_LEAD_MAX,
            "the lead went past its ceiling: {:?}",
            pacer.lead
        );
    }

    /// A camera's timestamps are not a metronome: measured on the living room
    /// stream the step between two pictures runs from ten to seventy
    /// milliseconds around a thirty-nine millisecond mean. The clock places them
    /// evenly all the same, and what the timestamps say is only what it is
    /// pulled back towards.
    #[test]
    fn a_wandering_timestamp_is_placed_on_an_even_clock() {
        let mut pacer = LivePacer::default();
        let arrival = establish(&mut pacer);
        let mut raw = 0u32;
        let mut places = Vec::new();
        for i in 0..12u32 {
            // Twenty milliseconds, then sixty: the mean is still one frame.
            raw += if i % 2 == 0 { 1800 } else { 5400 };
            pacer.push(picture(raw, false), arrival);
            places.push(pacer.held);
        }
        let gaps: Vec<i64> = places
            .windows(2)
            .skip(4)
            .map(|pair| pair[1] - pair[0])
            .collect();
        let off: Vec<i64> = gaps.iter().map(|gap| gap - i64::from(FRAME)).collect();
        assert!(
            off.iter().all(|off| off.abs() < 400),
            "the clock followed the jitter: {gaps:?}"
        );
    }

    /// A batch that arrives together is not shown together: each picture waits
    /// for the moment its own timestamp names.
    #[test]
    fn a_batch_is_spread_over_the_clock_its_timestamps_describe() {
        let mut pacer = LivePacer::default();
        let arrival = establish(&mut pacer);
        // Four pictures of a batch land in one read, a frame apart on the clock.
        for i in 1..4u32 {
            pacer.push(picture(FRAME * i, false), arrival);
        }
        let due: Vec<Duration> = pacer
            .queue
            .iter()
            .map(|held| pacer.moment(held.at).duration_since(arrival))
            .collect();
        let lead = LIVE_LEAD.as_millis() as u64;
        assert_eq!(
            due,
            vec![
                Duration::from_millis(lead + 40),
                Duration::from_millis(lead + 80),
                Duration::from_millis(lead + 120),
            ]
        );
    }

    /// The clock is an anchor and not an accumulator: a hundred batches of four
    /// pictures still leave every one of them where its timestamp puts it, and no
    /// picture is dropped or late, which is what the first attempt at this could
    /// not do.
    #[test]
    fn the_clock_does_not_drift_across_a_hundred_batches() {
        let mut pacer = LivePacer::default();
        let start = establish(&mut pacer);
        let mut sent = 1u32;
        for batch in 0..100u32 {
            let arrival = start + Duration::from_millis(160 * u64::from(batch));
            for i in 0..4u32 {
                let keyframe = batch == 0 && i == 0;
                pacer.push(picture(FRAME * (1 + batch * 4 + i), keyframe), arrival);
                sent += 1;
            }
            // The loop wakes for each picture at the moment it falls due, which
            // is what the pump's clock does with the picture's deadline.
            let horizon = arrival + Duration::from_millis(160);
            while let Some(due) = pacer.next_due() {
                if due > horizon {
                    break;
                }
                assert!(pacer.due(due).is_some(), "a picture came due and was not handed over");
            }
        }
        // The tail of the last batch falls past the last horizon; a session
        // goes on waking for it.
        while let Some(due) = pacer.next_due() {
            assert!(pacer.due(due).is_some());
        }
        assert_eq!(sent, 401);
        assert_eq!(pacer.handed, u64::from(sent), "the clock held pictures back");
        assert_eq!(pacer.dropped, 0, "the clock gave a picture up");
        assert_eq!(pacer.anchor_at, 0, "the anchor moved and the clock drifted");
    }

    /// A picture that was already due when the loop reached it means the lead was
    /// shorter than the gap between a batch arriving and its pictures falling
    /// due: the lead widens.
    #[test]
    fn a_late_picture_widens_the_lead() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        // The loop reaches the picture two seconds after its moment.
        assert!(pacer.due(start + Duration::from_secs(2)).is_some());
        // The window that follows is where its lateness is acted on.
        assert!(pacer.due(start + Duration::from_secs(4)).is_none());
        assert_eq!(pacer.lead, LIVE_LEAD + LIVE_LEAD_STEP);
        assert_eq!(pacer.widened, 1);
    }

    /// A stream that opens while the camera was already sending arrives as a
    /// crowd, and the clock settles on the newest of it: the pictures before are
    /// decoded as fast as the decoder takes them rather than played out one
    /// frame at a time, which would leave the viewer that far behind for ever.
    #[test]
    fn a_stream_that_opens_on_a_crowd_settles_on_the_newest_picture() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        // Eight pictures of a twelve frame a second stream arrive together, the
        // way a forwarder flushes what it buffered before the session joined.
        const SLOW: u32 = 7500;
        for i in 0..8u32 {
            pacer.push(picture(SLOW * i, i == 0), start);
        }
        assert_eq!(pacer.folded, 7, "the crowd was not folded into one moment");
        // The whole crowd falls due at once, so the loop hands all of it over.
        let mut handed = 0;
        while pacer.due(start).is_some() {
            handed += 1;
        }
        assert_eq!(handed, 8, "the crowd was played out instead of settled");
        assert!(!pacer.opening, "the clock never started");
        // What arrives next keeps its own moment, a frame after the last.
        pacer.push(picture(SLOW * 8, false), start + Duration::from_millis(83));
        let next = pacer.next_due().expect("held");
        assert!(
            next.duration_since(start) >= pacer.lead,
            "the picture after the crowd was not placed at its own moment"
        );
        assert_eq!(pacer.folded, 7, "the clock kept folding pictures in");
    }

    /// The sound is a crowd too, but not the picture's: an ordinary start of one
    /// picture per frame places the second one a frame after the first, and the
    /// clock stops moving with it.
    #[test]
    fn an_ordinary_start_does_not_fold_the_stream_in() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        assert!(pacer.due(start + pacer.lead).is_some());
        pacer.push(picture(FRAME, false), start + Duration::from_millis(40));
        assert_eq!(pacer.folded, 0);
        assert_eq!(pacer.anchor_at, 0);
    }

    /// A batch is gone within a frame or two. A queue that is still deeper than
    /// one a second later is not a batch but a wire that was already sending, and
    /// the clock moves to the newest picture rather than playing the past out.
    #[test]
    fn a_queue_that_stays_deep_is_folded_into_the_present() {
        let mut pacer = LivePacer::default();
        let start = establish(&mut pacer);
        const SLOW: u32 = 7500;
        // Eight pictures land at once, the way a forwarder flushes its buffer.
        for i in 1..9u32 {
            pacer.push(picture(SLOW * i, false), start);
        }
        assert_eq!(pacer.queue.len(), 8);
        let before = pacer.folded;
        // From then on the wire sends one per frame and the clock hands one per
        // frame, so the queue keeps its depth - exactly what a batch does not do.
        let mut now = start;
        for i in 9..24u32 {
            now = start + Duration::from_micros(u64::from(i) * 83_333);
            pacer.push(picture(SLOW * i, false), now);
            while pacer.due(now).is_some() {}
        }
        assert!(
            pacer.folded > before,
            "a queue that stayed deep for a second was kept as if it were a batch"
        );
        assert!(
            pacer.queue.len() <= LIVE_BATCH_PICTURES,
            "the clock is still holding the past: {} pictures",
            pacer.queue.len()
        );
    }

    /// A stretch with no late picture at all narrows the lead, slowly.
    #[test]
    fn a_quiet_while_without_a_late_picture_narrows_the_lead() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        // A fast stream, held wider than one of its frames: the lead has room to
        // come down without going under the one frame it may not.
        pacer.interval = Duration::from_millis(10);
        pacer.lead = Duration::from_millis(60);
        pacer.push(picture(0, true), start);
        // Handed over at its own moment: on time, not late.
        assert!(pacer.due(start + pacer.lead).is_some());
        // Nothing else arrives for sixteen seconds.
        assert!(pacer.due(start + Duration::from_secs(16)).is_none());
        assert_eq!(pacer.lead, Duration::from_millis(50));
        assert_eq!(pacer.narrowed, 1);
    }

    /// Narrowing stops at one frame: below that a picture is handed the moment it
    /// lands, and the wire's jitter goes straight to the screen.
    #[test]
    fn narrowing_stops_at_one_frame() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.interval = Duration::from_millis(83);
        pacer.lead = Duration::from_millis(100);
        pacer.push(picture(0, true), start);
        assert!(pacer.due(start + pacer.lead).is_some());
        // Even twenty seconds of quiet only narrows it as far as one frame.
        assert!(pacer.due(start + Duration::from_secs(24)).is_none());
        assert_eq!(pacer.lead, Duration::from_millis(90));
    }

    /// A picture that is late because the clock was just narrowed is not the wire
    /// being late. Narrowing moves every moment forward by the step it gave up,
    /// so the pictures that follow arrive late by exactly that step - blaming the
    /// wire for it is what makes a narrowing undo itself, and that is the shape of
    /// a delay that breathes.
    #[test]
    fn a_picture_made_late_by_a_narrowing_is_not_read_as_a_starved_wire() {
        let mut pacer = LivePacer::default();
        let start = establish(&mut pacer);
        pacer.interval = Duration::from_millis(83);
        pacer.lead = Duration::from_millis(150);
        pacer.trimmed = start;
        // Sixteen quiet seconds: the lead narrows and the clock starts settling.
        assert!(pacer.due(start + Duration::from_secs(16)).is_none());
        assert_eq!(pacer.lead, Duration::from_millis(140), "the lead did not narrow");
        // A picture lands while the clock is still settling - late by far more
        // than the step, but the step is not what it is being blamed for.
        pacer.push(picture(FRAME, false), start + Duration::from_secs(17));
        assert_eq!(
            pacer.starved,
            Duration::ZERO,
            "the narrowing was read as the wire needing the lead it just gave up"
        );
    }

    /// A widening with no narrowing behind it is a transient - a paused process,
    /// a lost packet - and is not proof the lead is too small. It must not lock
    /// the stream, and the quiet rule still narrows it afterwards.
    #[test]
    fn a_widening_with_no_narrowing_behind_it_is_not_a_bounce() {
        let mut pacer = LivePacer::default();
        let start = establish(&mut pacer);
        pacer.interval = Duration::from_millis(66);
        pacer.trimmed = start;
        // One picture arrives well after its moment - a transient the wire did
        // not ask for.
        pacer.push(picture(FRAME, false), start + Duration::from_millis(600));
        assert!(pacer.due(start + Duration::from_millis(600)).is_some());
        assert!(pacer.starved >= Duration::from_millis(150));
        // The window closes and the lead widens.
        let later = start + Duration::from_millis(2400);
        pacer.push(picture(FRAME * 2, false), later);
        assert!(pacer.due(later).is_some());
        assert_eq!(pacer.widened, 1, "the lead did not widen");
        assert_eq!(pacer.bounces, 0, "a transient was counted against the stream");
        assert!(!pacer.settled, "one transient settled the stream");
    }

    /// Two narrowing-and-rebound pairs are a pattern, not a transient: the stream
    /// keeps its lead and is not narrowed again, which is what stops a
    /// low-frame-rate stream breathing for the life of the session.
    #[test]
    fn two_narrowings_undone_by_the_stream_stop_the_narrowing() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.interval = Duration::from_millis(83);
        pacer.lead = Duration::from_millis(150);
        pacer.window = start;
        pacer.trimmed = start;
        let mut now = start;

        // A quiet window narrows the lead to 140.
        now += LIVE_TRIM_AFTER + LIVE_RETUNE;
        pacer.retune(now);
        assert_eq!(pacer.lead, Duration::from_millis(140), "the quiet window did not narrow");
        assert_eq!(pacer.narrowed, 1);

        // The pictures it moved forward are late: the stream takes the step back.
        pacer.starved = Duration::from_millis(20);
        now += LIVE_RETUNE;
        pacer.retune(now);
        assert_eq!(pacer.lead, Duration::from_millis(160), "the stream did not take its lead back");
        assert_eq!(pacer.bounces, 1, "a widening right after a narrowing was not counted");

        // Quiet again, narrow again, and the same rebound - the second one.
        now += LIVE_TRIM_AFTER;
        pacer.retune(now);
        assert_eq!(pacer.lead, Duration::from_millis(150));
        pacer.starved = Duration::from_millis(20);
        now += LIVE_RETUNE;
        pacer.retune(now);
        assert_eq!(pacer.lead, LIVE_LEAD_MAX);
        assert_eq!(pacer.bounces, 2, "the second rebound was not counted");
        assert!(pacer.settled, "two rebounds did not settle the stream");

        // From here it keeps its lead: a minute of quiet does not narrow it.
        now += LIVE_TRIM_AFTER * 4;
        pacer.retune(now);
        assert_eq!(pacer.lead, LIVE_LEAD_MAX, "a settled stream was narrowed anyway");
    }

    /// A widening long after a narrowing is not that narrowing being undone:
    /// the stream lived with the narrower lead for an hour, so what went wrong
    /// is something else - and a stream that never bounced must not be settled
    /// by two of those an hour apart.
    #[test]
    fn a_widening_long_after_a_narrowing_is_not_a_bounce() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.interval = Duration::from_millis(83);
        pacer.lead = Duration::from_millis(150);
        pacer.window = start;
        pacer.trimmed = start;

        let mut now = start + LIVE_TRIM_AFTER + LIVE_RETUNE;
        pacer.retune(now);
        assert_eq!(pacer.lead, Duration::from_millis(140), "the quiet window did not narrow");
        assert!(pacer.pending_narrowing);

        // An hour of a stream that was fine, and then a hiccup widens it.
        now += Duration::from_secs(3600);
        pacer.starved = Duration::from_millis(20);
        now += LIVE_RETUNE;
        pacer.retune(now);
        assert_eq!(pacer.lead, Duration::from_millis(160), "the hiccup did not widen it");
        assert_eq!(pacer.bounces, 0, "a narrowing an hour back was counted as undone");
        assert!(!pacer.settled, "one hiccup settled the stream");
    }

    /// The stream's own rate is what the lead may not go under, learned from the
    /// timestamps and not from when the pictures happened to land.
    #[test]
    fn a_slow_stream_raises_the_lead_to_its_own_frame() {
        let mut pacer = LivePacer::default();
        let start = establish(&mut pacer);
        // Twelve frames a second arrive together, the way a forwarder sends them.
        const SLOW: u32 = 7500;
        for i in 1..6u32 {
            pacer.push(picture(SLOW * i, false), start);
        }
        assert!(
            pacer.interval > Duration::from_millis(70),
            "the stream's rate was not learned: {:?}",
            pacer.interval
        );
        assert_eq!(pacer.lead, pacer.interval, "the lead is not a whole frame");
        let moments: Vec<Instant> = pacer
            .queue
            .iter()
            .map(|held| pacer.moment(held.at))
            .collect();
        for pair in moments.windows(2) {
            let apart = pair[1].duration_since(pair[0]);
            assert!(
                apart > Duration::from_millis(82) && apart < Duration::from_millis(84),
                "two pictures of a twelve frame stream are {apart:?} apart"
            );
        }
    }

    /// A gap too long to be a frame interval - the camera stopped, or a packet
    /// was lost - does not redefine the stream's rate.
    #[test]
    fn a_stall_does_not_redefine_a_frame() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        pacer.push(picture(FRAME, false), start);
        let learned = pacer.interval;
        pacer.push(picture(FRAME * 2, false), start);
        pacer.push(picture(FRAME * 2 + 90_000, false), start);
        assert_eq!(pacer.interval, learned, "a second of silence became a frame");
    }

    /// A batch bigger than the hold is trimmed rather than waited on. The loop
    /// that trims has to be bounded by what is in the queue: bounded by what
    /// arrived, a decoder handing back four pictures at once leaves it with a
    /// condition no pop can satisfy, and the session thread spins for ever
    /// instead of dropping a picture - which is what happened on the Mi Pad at
    /// the moment a magnified view switched to the main stream, and what froze
    /// the picture and silenced the session's log.
    #[test]
    fn a_batch_larger_than_the_hold_is_trimmed_to_it() {
        let batch = |count: u32| -> Vec<DecodedFrame> {
            (0..count)
                .map(|at| DecodedFrame::hardware(16, 16, i64::from(at), false, 0))
                .collect()
        };
        let mut pending = VecDeque::new();

        // A batch that fits is kept whole, which is the case the hold is sized
        // for: dropping it would cost pictures the viewer would have seen.
        hold(&mut pending, batch(4));
        assert_eq!(pending.len(), HELD_PICTURES);
        assert_eq!(pending.front().map(|frame| frame.pts_us), Some(0));
        pending.clear();

        hold(&mut pending, batch(6));
        assert_eq!(pending.len(), HELD_PICTURES, "the hold kept more than it holds");
        assert_eq!(pending.front().map(|frame| frame.pts_us), Some(2), "the oldest went");

        hold(&mut pending, batch(1));

        assert_eq!(pending.len(), HELD_PICTURES);
        assert_eq!(pending.back().map(|frame| frame.pts_us), Some(0), "the next one is in hand");
    }

    /// Neither direction leaves the bounds the delay is allowed to move in.
    #[test]
    fn the_lead_stays_between_its_bounds() {
        let mut pacer = LivePacer::default();
        pacer.lead = LIVE_LEAD_MAX;
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        assert!(pacer.due(start + Duration::from_secs(2)).is_some());
        assert!(pacer.due(start + Duration::from_secs(4)).is_none());
        assert_eq!(pacer.lead, LIVE_LEAD_MAX);

        let mut pacer = LivePacer::default();
        pacer.lead = LIVE_LEAD_MIN;
        pacer.trimmed = Instant::now() - Duration::from_secs(20);
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        assert!(pacer.due(start + LIVE_LEAD_MIN).is_some());
        assert!(pacer.due(start + Duration::from_secs(16)).is_none());
        assert_eq!(pacer.lead, LIVE_LEAD_MIN);
    }

    /// A queue that overflows gives up the oldest picture that nothing after it
    /// is predicted from - never the key frame the others are predicted from.
    #[test]
    fn a_full_queue_gives_up_a_predicted_picture_before_a_key_one() {
        let mut pacer = LivePacer::default();
        let now = Instant::now();
        pacer.push(picture(0, true), now);
        for i in 1..=LIVE_MAX_HELD as u32 {
            pacer.push(picture(FRAME * i, false), now);
        }
        assert_eq!(pacer.queue.len(), LIVE_MAX_HELD);
        assert!(
            pacer.queue.iter().any(|held| held.unit.keyframe),
            "the key frame was given up while pictures after it were still held"
        );
        assert_eq!(pacer.queue.front().expect("not empty").at, 0);
        // One key frame and nothing but predictions behind it: the newest went,
        // because nothing here could be given up without orphaning the rest.
        assert_eq!(pacer.dropped, 1);
    }

    /// A queue that must give something up is cut at a key frame. What is kept
    /// has to be decodable on its own: dropping a picture in the middle of a
    /// chain leaves everything behind it without its reference, and the decoder
    /// paints garbage until the camera sends the next key frame - while the
    /// pictures that were "kept" are ones nobody can see.
    #[test]
    fn a_full_queue_is_cut_at_a_key_frame() {
        let mut pacer = LivePacer::default();
        let now = Instant::now();
        // A key frame with eight pictures predicted from it, then a second key
        // frame with four more: fourteen pictures, two over the ceiling.
        pacer.push(picture(0, true), now);
        for i in 1..=8u32 {
            pacer.push(picture(FRAME * i, false), now);
        }
        pacer.push(picture(FRAME * 9, true), now);
        for i in 10..=13u32 {
            pacer.push(picture(FRAME * i, false), now);
        }
        assert!(pacer.queue.len() <= LIVE_MAX_HELD);
        let front = pacer.queue.front().expect("not empty");
        assert!(front.unit.keyframe, "the queue was cut in the middle of a chain");
        assert_eq!(front.at, i64::from(FRAME) * 9, "the second key frame was not kept");
        assert_eq!(pacer.dropped, 9, "the run before the key frame was not given up");
    }

    /// A picture that is due and long past is not shown late: the clock moves to
    /// it, so a decoder that fell behind does not turn into a growing delay.
    #[test]
    fn a_clock_far_behind_the_wire_is_moved_to_the_picture() {
        let mut pacer = LivePacer::default();
        let arrival = Instant::now();
        pacer.push(picture(0, true), arrival);
        let late = arrival + Duration::from_secs(2);
        assert!(pacer.due(late).is_some());
        assert_eq!(pacer.anchor_wall + pacer.lead, late);
    }

    /// Two seconds of timestamps in one step is the camera starting its counter
    /// over, not a frame interval.
    #[test]
    fn a_timestamp_that_jumps_starts_the_clock_over() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        pacer.push(picture(FRAME, false), start);
        pacer.push(picture(90_000 * 30, false), start + Duration::from_millis(80));
        assert_eq!(pacer.queue.len(), 1, "the old clock's pictures are gone");
        assert_eq!(pacer.anchor_at, 0);
        assert_eq!(pacer.held, 0);
    }

    /// Two pictures that claim the same moment wait together rather than the
    /// second being placed before the first.
    #[test]
    fn a_repeated_timestamp_does_not_move_the_clock_back() {
        let mut pacer = LivePacer::default();
        let start = Instant::now();
        pacer.push(picture(0, true), start);
        pacer.push(picture(0, false), start);
        assert_eq!(pacer.queue.len(), 2);
        assert_eq!(pacer.held, 0);
        assert_eq!(
            pacer.moment(pacer.queue.back().expect("held").at),
            pacer.moment(pacer.queue.front().expect("held").at)
        );
    }
}
