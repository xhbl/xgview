//! The RTSP session: negotiation, the packet loop, and the worker that carries a
//! call of its own.

use super::*;

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

/// The state one RTSP session carries between packets.
///
/// Everything the loop carried from one iteration to the next is a field,
/// because the loop is about to be split into methods that each answer one
/// question - the clocks, the talk-back, one media packet - and a method cannot
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
    baseline_fix: BaselineFix,
    sequence: u64,
    decode_errors: u64,
    sps_logged: bool,
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
        // [`BaselineFix`].
        let baseline_fix = BaselineFix::new();
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
            baseline_fix,
            sequence,
            decode_errors,
            sps_logged,
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
            // Talk-back, before waiting on the picture: the session's clock is the
            // RTP clock, and it must not be tied to how often a camera sends video.
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
                    // process - is not worth a burst of catch-up packets: the
                    // camera's speaker wants the present, not the backlog.
                    if self.last_talk.elapsed() > TALK_INTERVAL * 4 {
                        self.last_talk = Instant::now();
                        break;
                    }
                }
            }
            // A channel with no decoder has no picture to expect - it shows a
            // placeholder - and is left out of this entirely.
            let patience = self.stall.patience();
            let since_picture = self.stall.since_picture();
            if self.decoder.is_some() && since_picture >= patience {
                // Soft timeout: the picture has frozen, but the transport may
                // still be alive. Hold the last frame and keep waiting rather than
                // dropping to a black reconnect screen, which is the worse
                // experience on a flaky link. The state is announced once, when
                // the stall begins, so the grid can mark it without a flood of
                // events.
                if self.stall.stalled_since.is_none() {
                    self.stall.stalled_since = Some(Instant::now());
                    emit(
                        self.sinks.events,
                        StreamEvent::State {
                            index: self.index,
                            camera_id: self.camera_id.clone(),
                            state: ConnectionState::Stalled,
                            detail: format!("stalled for {} s", since_picture.as_secs()),
                        },
                    );
                }
                // Hard timeout: the stall has gone on long enough that
                // reconnecting is more likely to help than waiting.
                if self.stall.stalled_since.unwrap().elapsed() >= STALL_HARD_TIMEOUT {
                    return Err(crate::error::CoreError::timeout(format!(
                        "stalled for {} s, giving up",
                        since_picture.as_secs()
                    )));
                }
            }
            if self.last_keep_alive.elapsed() >= self.keep_alive_interval {
                // Fire and forget. Waiting for the answer would discard every RTP
                // packet that arrives first, and block the pump for good on a device
                // that never answers. Only a failed write is an error.
                self.client.send_keep_alive().await?;
                self.last_keep_alive = Instant::now();
            }
            if self.received_packets && self.last_report.elapsed() >= REPORT_INTERVAL {
                // Cameras differ in how much they care about being reported to. Some
                // stream regardless, but one was measured sending a single key frame
                // and then nothing but `SEI` until a receiver report arrived, which
                // a viewer shows as a slideshow whose timecode jumps. The counts it
                // carries are the session's real ones, so a camera that does adapt
                // to them has something to adapt to.
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

            // Waiting no longer than the picture deadline allows, so that a session
            // gone quiet is noticed on time rather than at the next packet. The wait
            // is a peek at the transport and the packet is only read once something
            // has arrived: a read cancelled part way through a packet takes the bytes
            // it has already consumed with it, and every packet after them is then
            // read at the wrong offset, which costs the session where the wait would
            // only have cost a wait.
            //
            // While stalled the patience is already spent, so the wait would be
            // zero and the loop would spin. A one second poll keeps it from
            // busy-waiting without blocking long enough to miss the hard timeout.
            // A channel with no decoder has no picture to wait for and no stall
            // state to read: the only signal it has is whether packets are still
            // arriving, so it waits for the full read timeout and lets that be the
            // one that reports silence on the wire.
            let wait = if self.decoder.is_some() && since_picture >= patience {
                STALL_HARD_TIMEOUT
                    .saturating_sub(self.stall.stalled_since.unwrap().elapsed())
                    .min(Duration::from_secs(1))
            } else if self.decoder.is_some() {
                READ_TIMEOUT.min(patience.saturating_sub(since_picture))
            } else {
                READ_TIMEOUT
            };
            // A session feeding a camera's speaker has one more clock to honour:
            // the wait is cut so that the next talk packet is written on time even
            // while the picture keeps the loop from ever timing out. Idle, that
            // clock is the keepalive's, not the frame rate's.
            let talk_wait = self.talk_sender
                .as_ref()
                .map(|_| {
                    talk_gap(self.talk.talking_on(self.index)).saturating_sub(self.last_talk.elapsed())
                });
            let wait = match talk_wait {
                Some(talk_wait) => wait.min(talk_wait),
                None => wait,
            };
            match tokio::time::timeout(wait, self.client.wait_for_media()).await {
                Ok(result) => result?,
                Err(_) => {
                    // The talk clock waking the loop is not silence on the wire.
                    if self.talk_sender.is_some()
                        && self.last_talk.elapsed() >= talk_gap(self.talk.talking_on(self.index))
                    {
                        continue;
                    }
                    // A wait that the picture deadline cut short is not silence on
                    // the wire, and reporting it as such would name the wrong fault
                    // and the wrong duration. The loop's own check reports that one.
                    // A channel with no decoder is exempt: its wait was the full
                    // read timeout, not a deadline slice, so an expiry here really
                    // is silence on the wire.
                    if self.decoder.is_some() && self.stall.since_picture() >= patience {
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

            // Channel 2 carries the audio RTP. It is decoded and played only while
            // this channel is the one the user is listening to and the sink is open;
            // otherwise the packet is dropped, which is what keeps the grid's nine
            // tiles from all playing their microphones.
            if channel == 2 {
                if self.audio.wants(self.index) {
                    if let Some(output) = self.audio.output().filter(|output| output.is_active()) {
                        if let Some(track) = self.audio_decoder.as_mut() {
                            if let Some(header) = parse_rtp_header(&payload) {
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
                                    output.push(&pcm);
                                }
                            }
                        }
                    }
                }
                continue;
            }
            if channel == 3 {
                continue;
            }

            // Channel 0 carries RTP, channel 1 carries RTCP.
            if channel == 0 {
                if let Some(header) = parse_rtp_header(&payload) {
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
                                continue;
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
                        self.stream,
                        &mut self.announced,
                    );

                    if let Some(decoder) = self.decoder.as_mut() {
                        let payload = &payload[header.header_len..];
                        // A packet completes at most two pictures: the one the RTP
                        // timestamp change released, and the one its marker closed.
                        let units = self.depacketizer.push(&header, payload);
                        let completed_none = units.is_empty();
                        for mut unit in units {
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
                            self.baseline_fix.observe(self.index, self.kind, &mut unit.data);
                            self.stall.observe(unit.keyframe, Instant::now());
                            let started = Instant::now();
                            let decoded = decoder.decode(&unit.data, unit.pts_us(), unit.keyframe);
                            self.window.counters.decode_took(started.elapsed());
                            match decoded {
                                Ok(decoded) => {
                                    // A picture came out, whatever the renderer can
                                    // do with it: on the platform whose frames stay
                                    // in a GPU buffer there are no planes to publish,
                                    // and the stream is no less alive for it.
                                    if !decoded.is_empty() {
                                        let was_stalled = self.stall.stalled_since.is_some();
                                        self.stall.picture(Instant::now());
                                        if was_stalled {
                                            emit(
                                                self.sinks.events,
                                                StreamEvent::State {
                                                    index: self.index,
                                                    camera_id: self.camera_id.clone(),
                                                    state: ConnectionState::Streaming,
                                                    detail: format!(
                                                        "receiving {} stream",
                                                        self.stream.label()
                                                    ),
                                                },
                                            );
                                        }
                                    }
                                    self.window.counters.decoded += decoded.len() as u64;
                                    if let Some((picture_width, picture_height)) =
                                        publish_frames(
                                            self.sinks.frames,
                                            self.index,
                                            &mut self.sequence,
                                            decoded,
                                        )
                                    {
                                        self.width = Some(picture_width);
                                        self.height = Some(picture_height);
                                    }
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
                        if completed_none && header.marker {
                            // The marker closed a picture but nothing came out of
                            // it: the unit held no slice, or its head was lost.
                            self.window.counters.dropped += 1;
                        }
                    }
                }
            }

            if self.window.due() {
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
                    fps,
                    kbps,
                    "stream window"
                );
            }
        }
    }
}
