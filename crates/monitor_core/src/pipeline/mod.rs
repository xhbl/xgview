//! Per channel streaming supervisor.
//!
//! The UI never touches a socket: [`ChannelManager::apply`] translates the
//! transitions computed by the [`Scheduler`](crate::scheduler::Scheduler) into
//! commands consumed by a supervisor task running on the tokio runtime, and
//! every channel reports back through a lock free `crossbeam-channel`.
//!
//! Each channel worker implements the exponential backoff reconnection state
//! machine: connect → describe → setup → play → read packets, and restart from
//! the first step when the session drops.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use monitor_codec::audio::{create_audio_decoder, AudioCodec, AudioDecoder, AudioStreamInfo};
use monitor_codec::sps;
use monitor_codec::{Codec, ColorSpace, DecodedFrame, DecoderConfig, VideoDecoder};

use crate::audio::{AacRtpParams, AudioHub};
use crate::config::ReconnectPolicy;
use crate::error::{FailureKind, Result};
use crate::h264::{nal_unit_type, AccessUnit, H264Depacketizer, START_CODE};
use crate::hevc::{nal_unit_type as hevc_nal_unit_type, HevcDepacketizer};
use crate::mjpeg::{JpegDepacketizer, MjpegClient};
use crate::model::{
    TalkMode,
    is_http_url, is_ws_url, CameraSource, ConnectionMode, ConnectionState, RtspTransport, StreamKind,
};
use crate::rtsp::{parse_rtp_header, MediaPacket, RtspClient, RtspStage, RtpHeader, SdpTrack};
use crate::scheduler::ScheduleChange;
use crate::talk::{
    route_from, talk_gap, Channel, TalkHub, TalkRoute, TalkStream, FRAME_SAMPLES, PROBE_DELAY,
    TALK_INTERVAL, UNSUPPORTED_TTL,
};

/// Time without any RTP payload after which the session is considered dead.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds on the deadline a running session is held to. See [`StallDeadline`].
const STALL_FLOOR: Duration = Duration::from_secs(5);
const STALL_CEILING: Duration = Duration::from_secs(15);
/// The same bounds for the wait before a session has produced its first picture,
/// which is longer because it is a wait for a key frame.
const FIRST_PICTURE_FLOOR: Duration = Duration::from_secs(10);
const FIRST_PICTURE_CEILING: Duration = Duration::from_secs(20);
/// Shortest interval between two key frames taken for a group of pictures rather
/// than for two key frames of one session start.
const MIN_PLAUSIBLE_GROUP: Duration = Duration::from_millis(500);
/// How long a stalled session is held before reconnecting. The last frame
/// stays on screen for this grace period, giving a flaky link time to recover
/// without the black screen of a reconnect.
const STALL_HARD_TIMEOUT: Duration = Duration::from_secs(30);
/// Time an MJPEG stream may go without a picture before it is considered dead.
///
/// A deadline counted in pictures, the way [`StallDeadline`] counts them, has
/// nothing to work with here: the stream is a slideshow by nature, running at
/// about one frame per second, so the only fault worth catching is a stream
/// that has stopped altogether. It is given a long leash for that reason - a
/// stall detector tuned to a frame rate this low would fire on the stream's
/// ordinary behaviour.
const MJPEG_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Interval of the statistics reported to the UI.
const STATS_INTERVAL: Duration = Duration::from_secs(2);
/// Interval between RTCP receiver reports, RFC 3550's value for a unicast session.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// SSRC the receiver reports go out under. Any fixed value does, it only has to
/// differ from the sender's.
const RECEIVER_SSRC: u32 = 0x5847_5631;

mod channel;
mod mjpeg_session;
mod rtsp_session;
mod shared;
mod supervisor;
mod ws_session;

use supervisor::Supervisor;

// The MSE socket is opened by the discovery probe as well as by the channel
// that streams it, so it keeps the name it had before the split.
pub(crate) use ws_session::open_mse;

/// What a channel reads afresh before each attempt.
///
/// Kept behind one lock so that a setting the user edits reaches the channels
/// that are already running at their next attempt, rather than only the ones
/// opened after the change.
#[derive(Debug, Clone)]
struct SessionSettings {
    /// The backoff applied between attempts.
    reconnect: ReconnectPolicy,
    /// The budget one attempt has to establish its session.
    ///
    /// The handshake is the one part of the path without a deadline of its own:
    /// `OPTIONS` / `DESCRIBE` / `SETUP` / `PLAY` each wait for the peer's answer
    /// with no timeout, and the stall detector only starts once packets are
    /// expected. A peer that accepts the connection and then stops answering -
    /// a half open connection, a middlebox that turns into a black hole, a
    /// device at its limit of sessions - would otherwise hold the channel for
    /// good, neither streaming nor reconnecting. Only the TCP connect was bound.
    handshake_timeout: Duration,
}

/// Why a call was not placed, or did not last.
///
/// The two are worth telling apart: one is the camera saying it has nothing to
/// talk into, which no retry will change, and the other is the network, which
/// the next press may well get through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallFailure {
    /// The camera answered, and has no back channel this build can send to.
    NoBackChannel,
    /// The camera could not be reached, or the session broke under the call.
    Unreachable,
}

/// Everything a channel reports to the renderer.
///
/// Each of these marks a change and has to be seen, so they travel on a channel
/// the renderer drains. The periodic numbers a stream produces are not here:
/// they are a measurement of the moment, kept in [`StatsStore`] where a newer
/// one replaces them.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Connection state changed.
    State {
        index: usize,
        camera_id: String,
        state: ConnectionState,
        detail: String,
    },
    /// Whether the camera behind a channel can be talked to.
    ///
    /// This is a camera's capability, not a session's state - the picture's
    /// session never asks for a back channel - so it is keyed by the address a
    /// call would be placed to, and it outlives any one session. The magnified
    /// bar shows its microphone on it.
    Talk {
        /// The channel that asked. Only for routing; the answer is the
        /// camera's, and a camera in two slots answers once.
        index: usize,
        camera_id: String,
        /// The address the probe asked, and the one a call would go to.
        uri: String,
        route: TalkRoute,
    },
    /// A call that could not be placed, or that ended without the viewer
    /// hanging up. The interface ends the call and says why.
    CallFailed {
        index: usize,
        camera_id: String,
        reason: CallFailure,
    },
    /// The channel switched from one stream to another (main <-> sub). The last
    /// frame of `from` is kept until `to` delivers its first frame.
    Transition {
        index: usize,
        camera_id: String,
        from: Option<StreamKind>,
        to: StreamKind,
    },
}

impl StreamEvent {
    pub fn index(&self) -> usize {
        match self {
            StreamEvent::State { index, .. }
            | StreamEvent::Talk { index, .. }
            | StreamEvent::CallFailed { index, .. }
            | StreamEvent::Transition { index, .. } => *index,
        }
    }

    pub fn camera_id(&self) -> &str {
        match self {
            StreamEvent::State { camera_id, .. }
            | StreamEvent::Talk { camera_id, .. }
            | StreamEvent::CallFailed { camera_id, .. }
            | StreamEvent::Transition { camera_id, .. } => camera_id,
        }
    }
}

/// One decoded picture, ready to be uploaded as a texture.
///
/// The planes are handed over as they came out of the decoder rather than
/// converted to colour first: uploading two planes costs a third of what RGBA
/// costs, and the renderer turns them into colour on the GPU, once, for every
/// tile at the same time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Monotonic per channel; the renderer re-uploads only when it changes.
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    /// How the two planes are to be read back as colour.
    pub colorspace: ColorSpace,
    /// Luma plane: `height` rows, each padded to the alignment a texture
    /// upload wants, of which the leading `width` bytes hold the picture.
    pub y: Vec<u8>,
    /// Interleaved chroma over `height / 2` rows of that same padded length:
    /// one byte of U and one of V for every two luma samples.
    pub uv: Vec<u8>,
}

/// The newest picture of every channel, and how many each has published.
///
/// A channel publishes and the renderer reads, both without ever blocking. A
/// repaint that falls behind therefore skips the pictures it did not see
/// instead of queueing them, which is what keeps a 4x4 grid of 25 fps streams
/// from building an unbounded backlog of megabytes.
#[derive(Debug, Default)]
pub struct FrameStore {
    channels: Mutex<HashMap<usize, ChannelFrames>>,
}

/// What the store holds for one channel.
#[derive(Debug, Default)]
struct ChannelFrames {
    /// Newest picture, the one a repaint takes.
    newest: Option<Arc<VideoFrame>>,
    /// Pictures this channel has published since the process started.
    ///
    /// The sequence number a frame carries restarts with every session, so it
    /// cannot answer the question a reconnecting channel asks: whether the
    /// session that has just ended put anything on screen at all.
    published: u64,
}

impl FrameStore {
    fn publish(&self, index: usize, frame: Arc<VideoFrame>) {
        if let Ok(mut channels) = self.channels.lock() {
            let entry = channels.entry(index).or_default();
            entry.newest = Some(frame);
            entry.published += 1;
        }
    }

    /// Newest picture of a channel, `None` until the first one is decoded.
    pub fn latest(&self, index: usize) -> Option<Arc<VideoFrame>> {
        self.channels.lock().ok()?.get(&index)?.newest.clone()
    }

    /// Pictures a channel has published since the process started.
    fn published(&self, index: usize) -> u64 {
        self.channels
            .lock()
            .ok()
            .and_then(|channels| channels.get(&index).map(|entry| entry.published))
            .unwrap_or(0)
    }
}

/// What one channel measures about the stream it is reading.
///
/// A sample describes the stream as it is at the moment it is taken: the next
/// one replaces it whole. Nothing here is a change to be observed, which is why
/// it is not a [`StreamEvent`].
#[derive(Debug, Clone, PartialEq)]
pub struct StreamStats {
    pub index: usize,
    pub camera_id: String,
    pub stream: StreamKind,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// True once the decoder has proved it runs on the GPU. A channel whose
    /// hardware decoder refused the stream reports `false` here, whatever the
    /// preference asked for.
    pub hardware: bool,
    /// True when this stream carries an audio track the build can decode.
    /// The magnified bar offers its sound switch on this: a camera that is
    /// heard from nothing should not show a button that does nothing.
    pub audio: bool,
    /// Frames per second measured on RTP marker bits.
    pub fps: f32,
    pub bitrate_kbps: f32,
    pub total_frames: u64,
}

/// One channel's newest sample, and the number of the sample it is.
#[derive(Debug)]
pub struct StatsEntry {
    /// Which sample this is, counted over the whole store. A renderer applies a
    /// sample when this has moved rather than on every repaint, and the count
    /// keeps rising across a `clear` so a fresh session's first sample is never
    /// mistaken for one already applied.
    pub sequence: u64,
    pub stats: StreamStats,
}

/// The newest sample of every channel.
///
/// Statistics are periodic and level-triggered: a channel publishes one every
/// `STATS_INTERVAL`, and the renderer wants the numbers as they are now, not
/// the history of them. Queued on the event channel they accumulate for as long
/// as nothing repaints - an Android app in the background, a window behind
/// another - which is unbounded growth for data that is stale by the time it is
/// read. Here a publish replaces the previous sample, so an interface that
/// stays away costs each channel one sample however long it stays away.
///
/// The same arrangement as [`FrameStore`], for the same reason.
#[derive(Debug, Default)]
pub struct StatsStore {
    inner: Mutex<StatsInner>,
}

#[derive(Debug, Default)]
struct StatsInner {
    channels: HashMap<usize, Arc<StatsEntry>>,
    /// Samples published since the process started.
    published: u64,
}

impl StatsStore {
    fn publish(&self, stats: StreamStats) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.published += 1;
            let sequence = inner.published;
            inner.channels.insert(stats.index, Arc::new(StatsEntry { sequence, stats }));
        }
    }

    /// Newest sample of a channel, `None` until it has published one.
    pub fn latest(&self, index: usize) -> Option<Arc<StatsEntry>> {
        self.inner.lock().ok()?.channels.get(&index).cloned()
    }

    /// Drops a channel's sample.
    ///
    /// Called wherever the session that took it stops existing: the attempt
    /// ended, the channel gave up, it was suspended, or its task was aborted to
    /// be replaced. What it measured belonged to that session, and the next one
    /// has its own codec, resolution and rate - and because the store has no
    /// ordering against the event channel, a sample left behind is read after
    /// the event that said the session was over and overwrites what it set.
    fn clear(&self, index: usize) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.channels.remove(&index);
        }
    }
}

/// Commands sent to the supervisor task.
#[derive(Debug)]
enum ChannelCommand {
    Activate {
        index: usize,
        source: CameraSource,
        stream: StreamKind,
        /// Stream decoded before this activation, when it is a switch.
        previous_stream: Option<StreamKind>,
    },
    Suspend {
        index: usize,
    },
    /// Decode on the GPU from now on, or stop trying to.
    PreferHardware(bool),
    /// Replace the reconnection policy of every channel, live ones included.
    SetPolicy(ReconnectPolicy),
    /// Replace the budget a session has to establish itself.
    SetHandshakeTimeout(Duration),
    /// Talk to the camera on this channel, or to none when it is `None`.
    ///
    /// The back channel is opened by the session that carries it, so a change
    /// reopens the two channels it moves between - the one that loses the
    /// microphone and the one that gains it.
    Talk(Option<usize>),
    /// Ask whether the camera behind this channel can be talked to.
    ///
    /// Answered from the cache when the answer is still fresh, and by a short
    /// connection of its own otherwise - never by the picture's session, which
    /// carries no back channel and asks for none.
    ProbeTalk { index: usize },
    /// A probe's answer, from the task that asked. The supervisor keeps the
    /// cache, so the task only does the asking.
    TalkProbed {
        uri: String,
        route: TalkRoute,
        mode: TalkMode,
    },
    /// Place a call: a session of its own that carries the microphone to the
    /// camera behind this channel.
    Call { index: usize },
    /// The camera would not take a call of its own, so the media goes on the
    /// picture's session instead - the way it was carried before the split.
    /// Only used when a call has already been refused.
    TalkFallback { index: usize },
    /// The viewer came back to the window. Every channel waiting out a backoff
    /// retries at once instead: the failures it counted happened while the
    /// process was in the background, and waiting them out now would only add
    /// that delay to the time the viewer was already away.
    Wake,
    Shutdown,
}

/// Handle owned by the UI thread.
#[derive(Debug)]
pub struct ChannelManager {
    commands: UnboundedSender<ChannelCommand>,
    events: Receiver<StreamEvent>,
    frames: Arc<FrameStore>,
    stats: Arc<StatsStore>,
    audio: Arc<AudioHub>,
    talk: Arc<TalkHub>,
    handle: Handle,
}

impl ChannelManager {
    /// Spawns the supervisor on the given runtime.
    ///
    /// `prefer_hardware` is the decoding preference live channels are opened
    /// with; see [`ChannelManager::set_hardware_preference`].
    pub fn spawn(
        handle: &Handle,
        policy: ReconnectPolicy,
        handshake_timeout: Duration,
        prefer_hardware: bool,
    ) -> Self {
        let (command_tx, command_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded();
        let frames = Arc::new(FrameStore::default());
        let stats = Arc::new(StatsStore::default());
        let audio = AudioHub::new();
        let talk = TalkHub::new();
        handle.spawn(
            Supervisor::new(
                command_rx,
                command_tx.clone(),
                event_tx,
                frames.clone(),
                stats.clone(),
                audio.clone(),
                talk.clone(),
                Arc::new(Mutex::new(SessionSettings { reconnect: policy, handshake_timeout })),
                prefer_hardware,
                handle.clone(),
            )
            .run(),
        );
        Self { commands: command_tx, events: event_rx, frames, stats, audio, talk, handle: handle.clone() }
    }

    /// The shared talk hub: the interface pushes captured audio here and names
    /// the channel the viewer has the microphone on.
    pub fn talk(&self) -> &Arc<TalkHub> {
        &self.talk
    }

    /// Names the channel the viewer may talk to, `None` when nothing is
    /// magnified. Only that channel's session carries the back channel, which
    /// is why the sessions either side of a change are reopened.
    pub fn set_talk(&self, index: Option<usize>) {
        self.send(ChannelCommand::Talk(index));
    }

    /// Switches decoding between the GPU and the CPU.
    ///
    /// A decoder is picked once, when a session opens, so the live channels are
    /// reopened for the change to take effect. The preference only ever asks:
    /// a machine without a usable device, or a stream the hardware decoder will
    /// not take, keeps decoding on the CPU.
    pub fn set_hardware_preference(&self, prefer: bool) {
        self.send(ChannelCommand::PreferHardware(prefer));
    }

    /// Replaces the reconnection policy of every channel.
    ///
    /// Unlike the decoder preference this needs no reopen: the delay is
    /// computed afresh before each attempt, so a channel that is running takes
    /// the new timing at its next retry, and a healthy stream is never
    /// interrupted by a change to retries it is not making.
    pub fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        self.send(ChannelCommand::SetPolicy(policy));
    }

    /// Replaces how long a session has to establish itself.
    ///
    /// Read afresh before each attempt, like the reconnection policy, so a
    /// channel stuck handshaking picks the new budget up at its next attempt
    /// without having to be reopened.
    pub fn set_handshake_timeout(&self, timeout: Duration) {
        self.send(ChannelCommand::SetHandshakeTimeout(timeout));
    }

    /// Asks whether the camera behind a channel can be talked to.
    ///
    /// The answer arrives as [`StreamEvent::Talk`], and it is the camera's
    /// rather than this session's: the picture never asks for a back channel.
    /// A call while a probe is in flight, or once the answer is still fresh,
    /// costs nothing - the caller may ask on every change of the viewport.
    pub fn probe_talk(&self, index: usize) {
        self.send(ChannelCommand::ProbeTalk { index });
    }

    /// Places a call to the camera behind a channel.
    ///
    /// A session of its own, carrying the microphone and nothing else: the
    /// picture's session is not touched, and hanging up is simply the hub's
    /// target going quiet. A failure arrives as [`StreamEvent::CallFailed`].
    pub fn call(&self, index: usize) {
        self.send(ChannelCommand::Call { index });
    }

    /// The runtime used by the streaming tasks (the UI uses it to spawn the
    /// discovery and import jobs as well).
    /// Retries every channel that is waiting out a backoff, at once.
    ///
    /// Called when the window comes back to the foreground: the failures that
    /// grew the backoff happened while the process was in the background, and
    /// the viewer should not have to wait them out after returning.
    pub fn wake(&self) {
        let _ = self.commands.send(ChannelCommand::Wake);
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// The newest decoded picture of every channel.
    pub fn frames(&self) -> &Arc<FrameStore> {
        &self.frames
    }

    /// The newest statistics of every channel.
    ///
    /// Read on repaint rather than drained: see [`StatsStore`] for why the
    /// numbers are kept instead of sent.
    pub fn stats(&self) -> &Arc<StatsStore> {
        &self.stats
    }

    /// The shared audio hub: the interface installs its sink here and names the
    /// channel whose sound should play.
    pub fn audio(&self) -> &Arc<AudioHub> {
        &self.audio
    }

    /// Applies the transitions produced by the scheduler.
    ///
    /// `cameras` is the list of enabled cameras; a channel index is a position
    /// inside that list.
    pub fn apply(&self, changes: &[ScheduleChange], cameras: &[CameraSource]) {
        for change in changes {
            match change {
                ScheduleChange::Activate { index, stream, previous_stream, .. } => {
                    let Some(source) = cameras.get(*index) else {
                        tracing::warn!(target: "xgview::pipeline", index, "no camera for channel");
                        continue;
                    };
                    self.send(ChannelCommand::Activate {
                        index: *index,
                        source: source.clone(),
                        stream: *stream,
                        previous_stream: *previous_stream,
                    });
                }
                ScheduleChange::Suspend { index, .. } => self.send(ChannelCommand::Suspend { index: *index }),
            }
        }
    }

    /// Sends a raw command, ignoring a closed supervisor (application exit).
    fn send(&self, command: ChannelCommand) {
        if let Err(err) = self.commands.send(command) {
            tracing::debug!(target: "xgview::pipeline", %err, "supervisor is gone");
        }
    }

    /// Stops every channel and terminates the supervisor.
    pub fn shutdown(&self) {
        self.send(ChannelCommand::Shutdown);
    }

    /// Non blocking drain of the pending events.
    pub fn poll(&self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    /// Receiver kept for callers that prefer to select on the channel.
    pub fn events(&self) -> &Receiver<StreamEvent> {
        &self.events
    }
}

/// The stream a channel pulls, and everything needed to open it again: after a
/// failure, and after a change that only the supervisor knows about, such as the
/// decoding preference.
#[derive(Clone)]
struct Endpoint {
    index: usize,
    camera_id: String,
    uri: String,
    credentials: Option<(String, String)>,
    stream: StreamKind,
    transport: RtspTransport,
    /// Whether this session also asks for the camera's talk-back media. Set for
    /// the magnified channel alone: a back channel is a camera resource, and
    /// sixteen tiles asking for one at once would fight over it.
    talk: bool,
    /// How this camera's speaker is reached, as the user configured it. See
    /// [`TalkMode`].
    talk_mode: TalkMode,
    /// The second address a channel may try when the first cannot be reached,
    /// with its own credentials. Present only for [`ConnectionMode::Auto`]: a
    /// protocol the user forced has no fallback.
    fallback: Option<Fallback>,
}

/// The MSE address a channel falls back to, and the login it is pulled with.
#[derive(Clone, PartialEq, Eq)]
struct Fallback {
    uri: String,
    credentials: Option<(String, String)>,
}

impl Endpoint {
    /// True when this is the same stream as the one already running, so that a
    /// revisit of a channel does not reopen it. The camera is not compared: the
    /// URI it contributes is, and so is the transport it is pulled with - and
    /// the fallback, so that switching `Auto` off (which drops it) reopens the
    /// channel rather than leaving the old attempt in place.
    fn is_same_stream(&self, other: &Endpoint) -> bool {
        self.stream == other.stream
            && self.uri == other.uri
            && self.credentials == other.credentials
            && self.transport == other.transport
            && self.talk == other.talk
            && self.talk_mode == other.talk_mode
            && self.fallback == other.fallback
    }

    /// True when the stream is an MJPEG endpoint rather than an RTSP session.
    ///
    /// A device can carry both: a Synology camera has its main stream on RTSP
    /// and its sub stream on HTTP MJPEG, so one `Endpoint` shape has to serve
    /// either and the URL is what picks the session it opens.
    fn is_mjpeg(&self) -> bool {
        is_http_url(&self.uri)
    }

    fn is_ws(&self) -> bool {
        is_ws_url(&self.uri)
    }

    /// Asks the same address for the camera's back channel, or stops asking.
    ///
    /// Only the intent is recorded here; where the parameter goes on the wire is
    /// the RTSP client's business, because it is the client that knows which
    /// request each server reads it from and that it must be gone again before
    /// the `DESCRIBE`. Appending it to the endpoint's URL instead put it on
    /// every request - a direct Foscam answers `404` to the `DESCRIBE` that way
    /// - and that is a picture lost for the sake of a switch.
    fn with_talk(mut self, on: bool) -> Self {
        self.talk = on;
        self
    }
}

struct ChannelRuntime {
    endpoint: Endpoint,
    task: JoinHandle<()>,
    /// Wakes this channel out of a backoff wait; see [`ChannelCommand::Wake`].
    wake: Arc<Notify>,
}

/// Credentials configured for a camera, if any.
///
/// They are handed to the RTSP client separately from the stream URI: the
/// password never travels inside the URL, where `@` or `:` would break it.
fn credentials_of(source: &CameraSource) -> Option<(String, String)> {
    let username = source.username.as_deref()?;
    Some((username.to_string(), source.password.clone().unwrap_or_default()))
}

/// Builds the endpoint one channel pulls, from the camera and the connection
/// mode it carries.
///
/// The address pulled first is the RTSP one everywhere but a camera forced onto
/// MSE. An `Auto` camera keeps the MSE address as the fallback, with Frigate's
/// own login rather than the restream account; a forced protocol carries no
/// fallback, so `Rtsp` means RTSP alone and `Mse` means MSE alone.
///
/// The session built from this carries the picture and nothing else: no back
/// channel is asked for, so a magnified tile holds no camera's speaker open
/// while nobody is talking. A camera that will not take a call on its own gets
/// [`Endpoint::with_talk`] applied later - see `ChannelCommand::TalkFallback`.
fn endpoint_for(source: &CameraSource, stream: StreamKind, index: usize) -> Endpoint {
    let mse_primary = matches!(source.connect, ConnectionMode::Mse) && source.has_mse();
    let (uri, credentials) = if mse_primary {
        (source.active_uri(stream).to_string(), source.mse_credentials())
    } else {
        (source.stream_uri(stream).to_string(), credentials_of(source))
    };
    let fallback = match source.connect {
        ConnectionMode::Auto => source.mse_uri(stream).map(|uri| Fallback {
            uri: uri.to_string(),
            credentials: source.mse_credentials(),
        }),
        ConnectionMode::Rtsp | ConnectionMode::Mse => None,
    };
    Endpoint {
        index,
        camera_id: source.id.clone(),
        uri,
        credentials,
        stream,
        transport: source.transport,
        talk: false,
        talk_mode: source.talk,
        fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::rtsp_session::{
        Arrival, HandshakeFailure, Jitter, SequenceWindow, StallDeadline, negotiate,
        takes_the_picture_session,
    };
    use super::shared::{BaselineFix, StatsWindow};
    use super::supervisor::TalkEntry;
    use super::ws_session::is_frigate_mse;
    use super::*;
    use crate::model::{CameraOrigin, TileAspect};

    /// A "yes" is kept for the run and a "no" is asked again after a while; a
    /// failure is not an answer at all, so the next magnification asks again.
    #[test]
    fn a_talk_answer_is_trusted_for_as_long_as_it_is_worth() {
        let mut entry = TalkEntry::default();
        assert!(!entry.fresh(), "nothing has been asked yet");

        entry.route = TalkRoute::Standard;
        entry.answered = Some(Instant::now());
        assert!(entry.fresh(), "a camera that offered a back channel still does");

        entry.route = TalkRoute::Vendor(crate::talk::Vendor::Foscam);
        assert!(entry.fresh(), "and so does a maker's own protocol");

        entry.route = TalkRoute::None;
        assert!(entry.fresh(), "a fresh refusal is worth believing");
        entry.answered = Some(Instant::now() - UNSUPPORTED_TTL - Duration::from_secs(1));
        assert!(!entry.fresh(), "but it is asked again after a while");

        entry.route = TalkRoute::Unknown;
        entry.answered = None;
        assert!(!entry.fresh(), "a failed probe caches nothing");
    }

    /// The fallback reopens the picture's session so the microphone can ride on
    /// it, which costs the picture a connect. That is worth paying when the
    /// camera refused the *shape* of the call, and not when it refused the
    /// account: the picture's session is opened with the same credentials and
    /// is refused the same way, so the fallback would only blank the wall.
    #[test]
    fn only_a_protocol_refusal_takes_the_picture_session() {
        use crate::error::CoreError;

        assert!(takes_the_picture_session(&CoreError::rtsp(
            "SETUP failed: 453 Not Enough Bandwidth"
        )));
        assert!(takes_the_picture_session(&CoreError::parse("no SDP body")));

        assert!(!takes_the_picture_session(&CoreError::auth(
            "SETUP failed: 401 Unauthorized (digest authentication)"
        )));
        assert!(!takes_the_picture_session(&CoreError::timeout(
            "the session handshake timed out after 10 s during SETUP"
        )));
        assert!(!takes_the_picture_session(&CoreError::network("connection refused")));
        assert!(!takes_the_picture_session(&CoreError::transport(
            "SETUP refused the transport: 461 Unsupported Transport"
        )));
    }

    fn camera(id: &str, uri: &str) -> CameraSource {
        CameraSource {
            id: id.to_string(),
            name: id.to_string(),
            vendor: None,
            model: None,
            host: "10.0.0.1".to_string(),
            onvif_port: 80,
            rtsp_main: uri.to_string(),
            rtsp_sub: Some(format!("{uri}-sub")),
            mse_main: None,
            mse_sub: None,
            username: None,
            password: None,
            mse_username: None,
            mse_password: None,
            connect: ConnectionMode::default(),
            talk: TalkMode::default(),
            enabled: true,
            tags: Vec::new(),
            origin: CameraOrigin::Manual,
            imported: false,
            transport: RtspTransport::default(),
            stream: None,
            aspect: TileAspect::default(),
            portrait_aspect: TileAspect::default(),
            zoom_aspect: None,
                        main_profile: None,
            sub_profile: None,
            group: 0,
        }
    }

    /// The connection mode decides what a channel pulls and what it may fall
    /// back to: `Auto` keeps the MSE address as a fallback with Frigate's own
    /// login, a forced protocol carries none.
    #[test]
    fn the_connection_mode_decides_the_fallback() {
        let mut camera = camera("cam", "rtsp://10.0.0.1:8554/cam_1");
        camera.origin = CameraOrigin::Frigate;
        camera.mse_main = Some("wss://10.0.0.1:8971/live/mse/api/ws?src=cam_1".to_string());
        camera.mse_username = Some("frigate".to_string());
        camera.mse_password = Some("pw".to_string());

        camera.connect = ConnectionMode::Auto;
        let endpoint = endpoint_for(&camera, StreamKind::Main, 0);
        assert_eq!(endpoint.uri, "rtsp://10.0.0.1:8554/cam_1");
        let fallback = endpoint.fallback.expect("an auto camera keeps the mse address");
        assert_eq!(fallback.uri, "wss://10.0.0.1:8971/live/mse/api/ws?src=cam_1");
        assert_eq!(fallback.credentials, Some(("frigate".to_string(), "pw".to_string())));

        camera.connect = ConnectionMode::Rtsp;
        let endpoint = endpoint_for(&camera, StreamKind::Main, 0);
        assert_eq!(endpoint.uri, "rtsp://10.0.0.1:8554/cam_1");
        assert!(endpoint.fallback.is_none(), "a forced rtsp has no fallback");

        camera.connect = ConnectionMode::Mse;
        let endpoint = endpoint_for(&camera, StreamKind::Main, 0);
        assert_eq!(endpoint.uri, "wss://10.0.0.1:8971/live/mse/api/ws?src=cam_1");
        assert_eq!(endpoint.credentials, Some(("frigate".to_string(), "pw".to_string())));
        assert!(endpoint.fallback.is_none(), "a forced mse has no fallback");
    }

    /// The two MSE endpoints are told apart by their paths, because they
    /// authenticate differently.
    #[test]
    fn frigate_and_go2rtc_mse_endpoints_are_told_apart() {
        assert!(is_frigate_mse("wss://10.0.0.1:8971/live/mse/api/ws?src=cam_1"));
        // A reverse proxy may place it under a subpath.
        assert!(is_frigate_mse("wss://host/frigate/live/mse/api/ws?src=cam_1"));
        assert!(!is_frigate_mse("ws://10.0.0.1:1984/api/ws?src=cam_1"));
        assert!(!is_frigate_mse("ws://host/rtc/api/ws?src=cam_1"));
    }

    /// The plain-Baseline fix rewrites an H.264 set that needs it, once, and
    /// leaves an already-constrained set and another codec alone.
    #[test]
    fn the_baseline_fix_rewrites_a_plain_h264_set_once() {
        // Annex-B: a start code, then an SPS NAL (type 7) with profile_idc 66
        // and a clear constraint byte.
        let mut unit = vec![0x00, 0x00, 0x01, 0x67, 66, 0x00, 0x1e];
        let mut fix = BaselineFix::new();
        assert!(fix.observe(0, Codec::H264, &mut unit), "the set is rewritten");
        assert_eq!(unit[5], 0x40, "constraint_set1_flag is what changes");
        assert!(!sps::needs_constrained_baseline(&unit[3..]), "and it is now constrained");

        // A set already constrained, and a stream of another codec, are left as
        // they arrived.
        let mut already = vec![0x00, 0x00, 0x01, 0x67, 66, 0x40, 0x1e];
        assert!(!BaselineFix::new().observe(0, Codec::H264, &mut already));
        assert_eq!(already[5], 0x40);

        let mut hevc = vec![0x00, 0x00, 0x01, 0x42, 0x01, 0x02];
        assert!(!BaselineFix::new().observe(0, Codec::H265, &mut hevc));
        assert_eq!(hevc[3], 0x42, "an H.265 unit is untouched");
    }

    #[test]
    fn a_long_group_of_pictures_widens_the_deadline() {
        let base = Instant::now();
        let mut stall = StallDeadline::new();
        stall.observe(true, base);
        stall.observe(true, base + Duration::from_secs(10));
        stall.picture(base + Duration::from_secs(10));
        // A camera whose key frames are ten seconds apart may legitimately leave
        // a picture for that long. A fixed five second deadline would have cut
        // it off every time it waited for one.
        assert_eq!(stall.patience(), STALL_CEILING);
    }

    #[test]
    fn a_startup_pair_of_key_frames_is_not_a_group() {
        let base = Instant::now();
        let mut stall = StallDeadline::new();
        stall.observe(true, base);
        stall.observe(true, base + Duration::from_millis(50));
        stall.picture(base);
        // Two key frames a moment apart are one session start, not a group of
        // pictures, and reading them as one would pin the deadline shut.
        assert_eq!(stall.patience(), STALL_CEILING);
    }

    #[test]
    fn a_dribbling_stream_cannot_widen_its_own_deadline() {
        let base = Instant::now();
        let mut stall = StallDeadline::new();
        stall.observe(true, base);
        stall.observe(true, base + Duration::from_secs(2));
        stall.picture(base + Duration::from_secs(2));
        assert_eq!(stall.patience(), STALL_FLOOR);

        // The stream then spends half a minute over its next picture, which is
        // the condition the deadline exists to catch. Following the recent
        // interval would widen the deadline here and let it through; learning
        // the shortest one leaves it exactly where it was.
        stall.observe(true, base + Duration::from_secs(32));
        assert_eq!(stall.patience(), STALL_FLOOR);
    }

    #[test]
    fn the_first_wait_is_the_longer_one() {
        let base = Instant::now();
        let mut stall = StallDeadline::new();
        // Nothing learned yet, so nothing is assumed: a camera is allowed to be
        // waiting for its next key frame.
        assert_eq!(stall.patience(), FIRST_PICTURE_CEILING);

        stall.observe(true, base);
        stall.observe(true, base + Duration::from_secs(2));
        assert_eq!(stall.patience(), FIRST_PICTURE_FLOOR);

        stall.picture(base + Duration::from_secs(2));
        assert_eq!(stall.patience(), STALL_FLOOR);
    }

    /// A jitter is a difference between two packets, so the first one has
    /// nothing to be different from.
    #[test]
    fn the_first_packet_is_the_baseline_and_adds_no_jitter() {
        let base = Instant::now();
        let mut jitter = Jitter::new(90_000.0);
        jitter.observe(1_000, base);
        assert_eq!(jitter.value(), 0);
    }

    /// A packet that arrives exactly as far apart as it was sent is the case the
    /// estimate exists to sit at zero on.
    #[test]
    fn a_packet_that_arrives_as_it_was_sent_is_no_jitter() {
        let base = Instant::now();
        let mut jitter = Jitter::new(90_000.0);
        jitter.observe(0, base);
        // Twenty milliseconds at ninety kilohertz.
        jitter.observe(1_800, base + Duration::from_millis(20));
        assert_eq!(jitter.value(), 0);
    }

    /// One sixteenth of the new difference, which is the smoothing RFC 3550
    /// names: a single late packet moves the estimate a little, not to it.
    #[test]
    fn a_late_packet_moves_the_estimate_by_a_sixteenth() {
        let base = Instant::now();
        let mut jitter = Jitter::new(90_000.0);
        jitter.observe(0, base);
        // It took twenty milliseconds to arrive, and the sender's clock says the
        // two were only two hundred units - about two milliseconds - apart.
        jitter.observe(200, base + Duration::from_millis(20));
        assert_eq!(jitter.value(), 100, "1600 units of difference, over sixteen");
    }

    /// The RTP timestamp is thirty-two bits and wraps: read as an absolute
    /// transit, the wrap is four billion units of jitter.
    #[test]
    fn a_timestamp_that_wrapped_is_a_short_step_forward() {
        let base = Instant::now();
        let mut jitter = Jitter::new(8_000.0);
        jitter.observe(0xffff_fff0, base);
        jitter.observe(0x0000_0010, base + Duration::from_millis(20));
        // Thirty-two units sent, a hundred and sixty arrived, a hundred and
        // twenty-eight of difference over sixteen.
        assert_eq!(jitter.value(), 8);
    }

    /// And the same read the other way: a packet whose timestamp is behind the
    /// last one is a short step back, not four billion units on.
    #[test]
    fn a_timestamp_behind_the_last_one_is_a_short_step_back() {
        let base = Instant::now();
        let mut jitter = Jitter::new(8_000.0);
        jitter.observe(0x0000_0100, base);
        jitter.observe(0x0000_00f0, base + Duration::from_millis(20));
        // Sixteen units back and a hundred and sixty arrived: a hundred and
        // seventy-six over sixteen.
        assert_eq!(jitter.value(), 11);
    }

    /// The extended count starts at the first packet, whichever half of the
    /// range it lands in. Folding it against a zero base would read every number
    /// above 32767 as a straggler from the cycle before, and hold the count at
    /// zero until the counter wrapped.
    #[test]
    fn the_first_packet_is_the_origin_of_the_extended_count() {
        let mut window = SequenceWindow::new();
        window.see(40_000);
        assert_eq!(window.highest(), 40_000);
    }

    /// The count is the sixteen bit field with its cycles folded into the high
    /// half, so a session that runs past the wrap does not look like one that
    /// restarted.
    #[test]
    fn the_extended_count_carries_the_cycles() {
        let mut window = SequenceWindow::new();
        for sequence in [65_534, 65_535, 0, 1] {
            window.see(sequence);
        }
        assert_eq!(window.highest(), 65_537);
    }

    /// A packet from the cycle before is folded to where it belongs, and the
    /// count - being the highest received - stays where it was.
    #[test]
    fn a_straggler_does_not_pull_the_extended_count_backwards() {
        let mut window = SequenceWindow::new();
        window.see(65_535);
        window.see(2);
        assert_eq!(window.highest(), 65_538);
        window.see(65_534);
        assert_eq!(window.highest(), 65_538, "the straggler is not the highest");
    }

    /// A duplicate and a straggler are the same answer: the picture that packet
    /// belonged to has already been handed over.
    #[test]
    fn a_duplicate_and_a_straggler_are_both_stale() {
        let mut window = SequenceWindow::new();
        assert!(matches!(window.accept(100), Arrival::Next));
        assert!(matches!(window.accept(101), Arrival::Next));
        assert!(matches!(window.accept(101), Arrival::Stale), "a duplicate");
        assert!(matches!(window.accept(100), Arrival::Stale), "a straggler");
        assert_eq!(window.lost(), 0, "neither is a loss");
        // The stale packets did not move what the next one is measured from.
        assert!(matches!(window.accept(102), Arrival::Next));
    }

    /// The sequence numbers wrap too, and the packet after the wrap is the next
    /// one - not a straggler half a range behind it.
    #[test]
    fn the_packet_after_the_wrap_is_the_next_one() {
        let mut window = SequenceWindow::new();
        assert!(matches!(window.accept(65_535), Arrival::Next));
        assert!(matches!(window.accept(0), Arrival::Next), "one on, over the wrap");
        assert_eq!(window.lost(), 0, "and nothing was missed in between");
    }

    /// A gap is what the packets in between were, and it is counted once: the
    /// packet after the gap is measured from the packet that closed it.
    #[test]
    fn a_gap_says_how_many_were_missing() {
        let mut window = SequenceWindow::new();
        assert!(matches!(window.accept(100), Arrival::Next));
        match window.accept(104) {
            Arrival::Gap(missing) => assert_eq!(missing, 3),
            _ => panic!("three packets were lost"),
        }
        assert_eq!(window.lost(), 3);
        assert!(matches!(window.accept(105), Arrival::Next), "and the gap is closed");
        assert_eq!(window.lost(), 3, "and counted once");
    }

    /// A sample describes the stream as it is now, so a newer one replaces it:
    /// an interface that was not repainting leaves one sample waiting per
    /// channel rather than a queue of them.
    #[test]
    fn a_channel_keeps_only_its_newest_sample() {
        let store = StatsStore::default();
        let sample = |fps: f32| StreamStats {
            index: 3,
            camera_id: "cam".to_string(),
            stream: StreamKind::Main,
            codec: Some("H264".to_string()),
            width: Some(1920),
            height: Some(1080),
            hardware: false,
            audio: false,
            fps,
            bitrate_kbps: 0.0,
            total_frames: 0,
        };

        assert!(store.latest(3).is_none(), "nothing has been published yet");
        store.publish(sample(12.0));
        let first = store.latest(3).expect("the sample is kept");
        store.publish(sample(25.0));
        let second = store.latest(3).expect("the newer sample is the one kept");

        assert_eq!(second.stats.fps, 25.0, "the older sample is gone, not queued");
        assert!(second.sequence > first.sequence, "and is told from the one before it");
        assert!(store.latest(4).is_none(), "one channel's sample is not another's");
    }

    /// The numbers of a session that has ended describe nothing: the next one
    /// has its own codec, resolution and rate, and a renderer that read the old
    /// ones would show a failed channel still running.
    #[test]
    fn a_sample_taken_by_a_finished_session_is_dropped() {
        let store = StatsStore::default();
        let sample = StreamStats {
            index: 7,
            camera_id: "cam".to_string(),
            stream: StreamKind::Main,
            codec: None,
            width: None,
            height: None,
            hardware: false,
            audio: false,
            fps: 25.0,
            bitrate_kbps: 0.0,
            total_frames: 0,
        };

        store.publish(sample.clone());
        let last = store.latest(7).expect("published");
        store.clear(7);
        assert!(store.latest(7).is_none(), "the sample went with the session");

        // The next session's first sample is still a new one, though the store
        // holds nothing before it: the count keeps rising across a clear, so a
        // renderer that applied the old sample does not mistake it for a repeat.
        store.publish(sample);
        assert!(
            store.latest(7).expect("published").sequence > last.sequence,
            "a sample after a clear is newer than the one cleared"
        );
    }

    #[test]
    fn only_a_udp_media_stage_silence_is_worth_the_other_transport() {
        let timeout_at =
            |stage| HandshakeFailure::TimedOut { stage, timeout: Duration::from_secs(15) };
        // A refusal is the server saying so outright, and it only means anything
        // over UDP.
        let refused = HandshakeFailure::Refused("SETUP refused the transport".to_string());
        assert!(refused.worth_the_other_transport(RtspTransport::Udp));
        assert!(!refused.worth_the_other_transport(RtspTransport::Tcp));
        // Silence once the media transport was being set up is what a path that
        // drops datagrams looks like, and is worth one try over TCP.
        assert!(timeout_at(Some(RtspStage::Setup)).worth_the_other_transport(RtspTransport::Udp));
        assert!(timeout_at(Some(RtspStage::Play)).worth_the_other_transport(RtspTransport::Udp));
        // Silence before that is the control connection itself, which the other
        // transport would only repeat.
        assert!(!timeout_at(Some(RtspStage::Options)).worth_the_other_transport(RtspTransport::Udp));
        assert!(!timeout_at(Some(RtspStage::Describe)).worth_the_other_transport(RtspTransport::Udp));
        // And nothing else is answered with the other transport.
        let other = HandshakeFailure::Failed(crate::error::CoreError::rtsp("no video track"));
        assert!(!other.worth_the_other_transport(RtspTransport::Udp));
    }

    #[tokio::test]
    async fn a_silent_peer_says_which_request_it_was_waiting_on() {
        // A listener that accepts and then never answers: the state a half open
        // connection leaves a handshake in, and the one that used to hold a
        // channel for good.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let quiet = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut client = RtspClient::connect_with_auth(&format!("rtsp://{address}/live"), None)
            .await
            .expect("the connection is accepted");
        let failure = negotiate(&mut client, RtspTransport::Tcp, Duration::from_millis(100), 0, false)
            .await
            .unwrap_err();

        match failure {
            HandshakeFailure::TimedOut { stage, .. } => {
                assert_eq!(stage, Some(RtspStage::Options), "it stalls on the first request");
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        quiet.abort();
    }

    #[tokio::test]
    async fn unreachable_camera_reports_failure() {
        let handle = Handle::current();
        let policy = ReconnectPolicy {
            max_attempts: 1,
            jitter: 0.0,
            initial_delay_ms: 10,
            max_delay_ms: 20,
            ..Default::default()
        };
        let manager = ChannelManager::spawn(&handle, policy, Duration::from_secs(5), false);
        let cameras = vec![camera("cam-1", "rtsp://127.0.0.1:1/stream")];
        manager.apply(
            &[ScheduleChange::Activate {
                index: 0,
                camera_id: "cam-1".to_string(),
                stream: StreamKind::Sub,
                previous_stream: None,
            }],
            &cameras,
        );

        let mut saw_failure = false;
        // A refused TCP connection can take a couple of seconds on hardened
        // hosts, so the budget is generous while still exiting early.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            for event in manager.poll() {
                if let StreamEvent::State { state: ConnectionState::Failed, .. } = event {
                    saw_failure = true;
                }
            }
            if saw_failure {
                break;
            }
        }
        assert!(saw_failure, "the channel must report a failure for a dead endpoint");
        manager.shutdown();
    }

    /// A window hands back what it counted and starts again, so that the sample
    /// published and the line that logs it describe the same seconds - and so
    /// that the reset cannot be forgotten, which it could be in three places.
    #[test]
    fn a_window_hands_back_what_it_counted_and_starts_again() {
        let mut window = StatsWindow::new();
        assert!(!window.due(), "a window that has just started is not due");

        window.counters.frames = 50;
        window.counters.bytes = 100_000;
        window.counters.decode_took(Duration::from_millis(12));
        window.counters.decode_took(Duration::from_millis(7));
        assert_eq!(
            window.counters.slowest,
            Duration::from_millis(12),
            "the slowest decode of the window, not the last one"
        );

        let counted = window.close();
        assert_eq!(counted.frames, 50);
        assert_eq!(counted.bytes, 100_000);
        assert_eq!(counted.slowest, Duration::from_millis(12));
        assert_eq!(window.counters.frames, 0, "the next window starts empty");
        assert!(!window.due(), "and the clock has started again with it");
    }

    /// The two numbers every session publishes, from the counters it kept.
    #[test]
    fn a_window_divides_by_the_seconds_it_lasted() {
        let mut window = StatsWindow::new();
        window.counters.frames = 50;
        window.counters.bytes = 125_000;

        assert_eq!(window.counters.fps(2.0), 25.0);
        assert_eq!(window.counters.kbps(2.0), 500.0, "125 kB over two seconds");
    }
}
