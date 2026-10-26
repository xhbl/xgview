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
use tokio::task::JoinHandle;

use monitor_codec::audio::{create_audio_decoder, AudioCodec, AudioDecoder, AudioStreamInfo};
use monitor_codec::sps;
use monitor_codec::{Codec, ColorSpace, DecodedFrame, DecoderConfig, VideoDecoder};

use crate::audio::{AacRtpParams, AudioHub};
use crate::config::ReconnectPolicy;
use crate::error::Result;
use crate::h264::{nal_unit_type, AccessUnit, H264Depacketizer, START_CODE};
use crate::hevc::{nal_unit_type as hevc_nal_unit_type, HevcDepacketizer};
use crate::mjpeg::{JpegDepacketizer, MjpegClient};
use crate::model::{
    is_http_url, is_ws_url, CameraSource, ConnectionMode, ConnectionState, RtspTransport, StreamKind,
};
use crate::rtsp::{parse_rtp_header, MediaPacket, RtspClient, RtspStage, RtpHeader, SdpTrack};
use crate::scheduler::ScheduleChange;
use crate::talk::{
    talk_gap, Channel, TalkHub, TalkStream, TalkSupport, FRAME_SAMPLES, PROBE_DELAY, TALK_INTERVAL,
    UNSUPPORTED_TTL,
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
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Connection state changed.
    State {
        index: usize,
        camera_id: String,
        state: ConnectionState,
        detail: String,
    },
    /// Periodic statistics of a live stream.
    Stats {
        index: usize,
        camera_id: String,
        stream: StreamKind,
        codec: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        /// True once the decoder has proved it runs on the GPU. A channel whose
        /// hardware decoder refused the stream reports `false` here, whatever
        /// the preference asked for.
        hardware: bool,
        /// True when this stream carries an audio track the build can decode.
        /// The magnified bar offers its sound switch on this: a camera that is
        /// heard from nothing should not show a button that does nothing.
        audio: bool,
        /// Frames per second measured on RTP marker bits.
        fps: f32,
        bitrate_kbps: f32,
        total_frames: u64,
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
        support: TalkSupport,
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
            | StreamEvent::Stats { index, .. }
            | StreamEvent::Talk { index, .. }
            | StreamEvent::CallFailed { index, .. }
            | StreamEvent::Transition { index, .. } => *index,
        }
    }

    pub fn camera_id(&self) -> &str {
        match self {
            StreamEvent::State { camera_id, .. }
            | StreamEvent::Stats { camera_id, .. }
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
    TalkProbed { uri: String, support: TalkSupport },
    /// Place a call: a session of its own that carries the microphone to the
    /// camera behind this channel.
    Call { index: usize },
    /// The camera would not take a call of its own, so the media goes on the
    /// picture's session instead - the way it was carried before the split.
    /// Only used when a call has already been refused.
    TalkFallback { index: usize },
    Shutdown,
}

/// Handle owned by the UI thread.
#[derive(Debug)]
pub struct ChannelManager {
    commands: UnboundedSender<ChannelCommand>,
    events: Receiver<StreamEvent>,
    frames: Arc<FrameStore>,
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
        let audio = AudioHub::new();
        let talk = TalkHub::new();
        handle.spawn(supervisor(
            command_rx,
            command_tx.clone(),
            event_tx,
            frames.clone(),
            audio.clone(),
            talk.clone(),
            Arc::new(Mutex::new(SessionSettings { reconnect: policy, handshake_timeout })),
            prefer_hardware,
            handle.clone(),
        ));
        Self { commands: command_tx, events: event_rx, frames, audio, talk, handle: handle.clone() }
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
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// The newest decoded picture of every channel.
    pub fn frames(&self) -> &Arc<FrameStore> {
        &self.frames
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
        fallback,
    }
}

/// What the supervisor knows about one camera's back channel.
#[derive(Default)]
struct TalkEntry {
    support: TalkSupport,
    /// When the answer was given; `None` until one is.
    answered: Option<Instant>,
    /// A probe is in flight for this address.
    probing: bool,
    /// This camera would not take a call on a connection of its own, and the
    /// media has to ride on the picture's session instead. Kept for the run:
    /// asking again costs a reopen of that session every time.
    refuses_call: bool,
}

impl TalkEntry {
    /// Whether the answer is still worth answering with.
    ///
    /// A "no" is asked again after a while - a firmware or an account can
    /// change it - while a "yes" is kept: it is the answer the microphone is
    /// drawn on, and a camera that offered a back channel a minute ago still
    /// does.
    fn fresh(&self) -> bool {
        let Some(at) = self.answered else { return false };
        match self.support {
            TalkSupport::Supported => true,
            TalkSupport::Unsupported => at.elapsed() < UNSUPPORTED_TTL,
            TalkSupport::Unknown => false,
        }
    }
}

async fn supervisor(
    mut commands: UnboundedReceiver<ChannelCommand>,
    command_tx: UnboundedSender<ChannelCommand>,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
    audio: Arc<AudioHub>,
    talk: Arc<TalkHub>,
    settings: Arc<Mutex<SessionSettings>>,
    mut prefer_hardware: bool,
    handle: Handle,
) {
    let mut channels: HashMap<usize, ChannelRuntime> = HashMap::new();
    // The channel the viewer may talk to. Kept beside the channels because it
    // outlives the sessions it opens: a channel reopened for another reason
    // gets the back channel back if it is still the one magnified.
    let mut talk_target: Option<usize> = None;
    // What is known about each camera's back channel, keyed by the address a
    // call would be placed to. A camera sitting in two slots is one entry, and
    // a channel that moves between streams asks about the stream it is on.
    let mut talk_support: HashMap<String, TalkEntry> = HashMap::new();
    // The call in progress, if any. One at a time: the hub has a single target,
    // and two workers would send the same microphone twice.
    let mut call: Option<JoinHandle<()>> = None;

    while let Some(command) = commands.recv().await {
        match command {
            ChannelCommand::Activate { index, source, stream, previous_stream } => {
                // A camera whose sub stream is neither configured nor inferable
                // is served by its main stream: report the stream that is really
                // pulled instead of labelling a 1080p picture as `SUB`.
                let stream = match stream {
                    StreamKind::Sub if source.configured_uri(StreamKind::Sub).is_none() => {
                        StreamKind::Main
                    }
                    kind => kind,
                };
                let endpoint = endpoint_for(&source, stream, index);
                if let Some(existing) = channels.get(&index) {
                    if existing.endpoint.is_same_stream(&endpoint) {
                        continue;
                    }
                }
                if let Some(existing) = channels.remove(&index) {
                    existing.task.abort();
                }
                if previous_stream.is_some() {
                    let _ = events.send(StreamEvent::Transition {
                        index,
                        camera_id: endpoint.camera_id.clone(),
                        from: previous_stream,
                        to: stream,
                    });
                }
                tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    camera = %source.masked_uri(stream),
                    stream = stream.as_str(),
                    "starting channel"
                );
                let task = handle.spawn(run_channel(
                    endpoint.clone(),
                    prefer_hardware,
                    settings.clone(),
                    events.clone(),
                    frames.clone(),
                    audio.clone(),
                    talk.clone(),
                ));
                channels.insert(index, ChannelRuntime { endpoint, task });
            }
            ChannelCommand::Suspend { index } => {
                if let Some(existing) = channels.remove(&index) {
                    existing.task.abort();
                    tracing::debug!(target: "xgview::pipeline", index, "channel suspended");
                }
            }
            ChannelCommand::PreferHardware(prefer) => {
                if prefer == prefer_hardware {
                    continue;
                }
                prefer_hardware = prefer;
                tracing::debug!(
                    target: "xgview::pipeline",
                    hardware = prefer,
                    channels = channels.len(),
                    "decoding preference changed, reopening the channels"
                );
                // The decoder is chosen when a session opens, so the live
                // channels have to be reopened for the new preference to reach
                // them. They are started again from here because the scheduler
                // has no reason to see a change: nothing about the cameras did.
                for (index, runtime) in channels.drain().collect::<Vec<_>>() {
                    runtime.task.abort();
                    let endpoint = runtime.endpoint;
                    let task = handle.spawn(run_channel(
                        endpoint.clone(),
                        prefer_hardware,
                        settings.clone(),
                        events.clone(),
                        frames.clone(),
                        audio.clone(),
                        talk.clone(),
                    ));
                    channels.insert(index, ChannelRuntime { endpoint, task });
                }
            }
            ChannelCommand::Talk(target) => {
                if talk_target == target {
                    continue;
                }
                talk_target = target;
                // The hub the captured audio arrives on has to name the same
                // channel, or the worker below keeps writing silence: the mic
                // samples are pushed into the hub, but `TalkHub::talking_on`
                // only answers for its target, and a target nobody sets is
                // `None` for the life of the process. See `TalkHub::set_target`.
                //
                // Nothing about a session changes here, which is the point of
                // the split: the picture never carried a back channel, so a
                // change of viewport no longer reopens the channel it left or
                // the one it arrived at. The call, when there is one, is on its
                // own connection and ends by itself - `talking_on` goes false
                // for the channel it belonged to.
                talk.set_target(target);
                debug_assert_eq!(
                    talk.target(),
                    target,
                    "the hub has to follow the session's talk target"
                );
                tracing::debug!(
                    target: "xgview::pipeline",
                    camera = ?target,
                    "the talk target moved"
                );
            }
            ChannelCommand::ProbeTalk { index } => {
                let Some(runtime) = channels.get(&index) else { continue };
                let endpoint = runtime.endpoint.clone();
                let uri = endpoint.uri.clone();
                let answer = |support| StreamEvent::Talk {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    uri: uri.clone(),
                    support,
                };
                // MJPEG and the MSE fallback carry no media the client sends,
                // so there is nothing to ask: the answer is known without a
                // connection.
                if endpoint.is_mjpeg() || endpoint.is_ws() {
                    let _ = events.send(answer(TalkSupport::Unsupported));
                    continue;
                }
                let entry = talk_support.entry(uri.clone()).or_default();
                if entry.fresh() {
                    let _ = events.send(answer(entry.support));
                    continue;
                }
                // One probe per address at a time: the second caller would only
                // ask the same camera the same question.
                if entry.probing {
                    continue;
                }
                entry.probing = true;
                let timeout = {
                    let settings = settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    settings.handshake_timeout
                };
                let credentials = endpoint.credentials.clone();
                let camera_id = endpoint.camera_id.clone();
                let commands = command_tx.clone();
                handle.spawn(async move {
                    // Late enough that the picture's own session - reopened for
                    // the other stream at the same moment - is not still
                    // connecting when this one arrives.
                    tokio::time::sleep(PROBE_DELAY).await;
                    let ask = async {
                        let mut client = RtspClient::connect_with_auth(&uri, credentials).await?;
                        client.probe_backchannel().await
                    };
                    let support = match tokio::time::timeout(timeout, ask).await {
                        Ok(Ok(Some(encoding))) => {
                            let codec = AudioCodec::from_encoding(&encoding);
                            if TalkStream::new(codec).is_some() {
                                tracing::debug!(
                                    target: "xgview::talk",
                                    camera = %camera_id,
                                    encoding,
                                    "the camera offers a back channel"
                                );
                                TalkSupport::Supported
                            } else {
                                tracing::debug!(
                                    target: "xgview::talk",
                                    camera = %camera_id,
                                    encoding,
                                    "the back channel carries an encoding this build cannot send"
                                );
                                TalkSupport::Unsupported
                            }
                        }
                        Ok(Ok(None)) => {
                            tracing::debug!(
                                target: "xgview::talk",
                                camera = %camera_id,
                                "the camera offers no back channel"
                            );
                            TalkSupport::Unsupported
                        }
                        // A failure is not an answer: nothing is cached on it,
                        // so the next magnification asks again.
                        Ok(Err(err)) => {
                            tracing::debug!(
                                target: "xgview::talk",
                                camera = %camera_id,
                                error = %err,
                                "the talk-back probe failed"
                            );
                            TalkSupport::Unknown
                        }
                        Err(_) => {
                            tracing::debug!(
                                target: "xgview::talk",
                                camera = %camera_id,
                                timeout_s = timeout.as_secs(),
                                "the talk-back probe timed out"
                            );
                            TalkSupport::Unknown
                        }
                    };
                    let _ = commands.send(ChannelCommand::TalkProbed { uri, support });
                });
            }
            ChannelCommand::TalkProbed { uri, support } => {
                let entry = talk_support.entry(uri.clone()).or_default();
                entry.probing = false;
                if support != TalkSupport::Unknown {
                    entry.support = support;
                    entry.answered = Some(Instant::now());
                }
                // The answer is the camera's, so every channel showing that
                // address hears it - not only the one that asked.
                for (index, runtime) in &channels {
                    if runtime.endpoint.uri == uri {
                        let _ = events.send(StreamEvent::Talk {
                            index: *index,
                            camera_id: runtime.endpoint.camera_id.clone(),
                            uri: uri.clone(),
                            support: entry.support,
                        });
                    }
                }
            }
            ChannelCommand::Call { index } => {
                let Some(runtime) = channels.get(&index) else {
                    // The channel went away between the press and this command.
                    // Saying so beats a microphone that does nothing.
                    let _ = events.send(StreamEvent::CallFailed {
                        index,
                        camera_id: String::new(),
                        reason: CallFailure::Unreachable,
                    });
                    continue;
                };
                let endpoint = runtime.endpoint.clone();
                // MJPEG and the MSE fallback carry no media the client sends.
                // The probe already answers `Unsupported` for them, so this is
                // only the case where the press beat the answer.
                if endpoint.is_mjpeg() || endpoint.is_ws() {
                    let _ = events.send(StreamEvent::CallFailed {
                        index,
                        camera_id: endpoint.camera_id.clone(),
                        reason: CallFailure::NoBackChannel,
                    });
                    continue;
                }
                // This camera has already said it will not take a call of its
                // own. Going straight to the session that can carry the media
                // spares the viewer a wait for a refusal that is already known.
                if talk_support.get(&endpoint.uri).is_some_and(|entry| entry.refuses_call) {
                    let _ = command_tx.send(ChannelCommand::TalkFallback { index });
                    continue;
                }
                if let Some(previous) = call.take() {
                    previous.abort();
                }
                let timeout = {
                    let settings = settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    settings.handshake_timeout
                };
                call = Some(handle.spawn(talk_worker(
                    index,
                    endpoint,
                    talk.clone(),
                    events.clone(),
                    command_tx.clone(),
                    timeout,
                )));
            }
            ChannelCommand::TalkFallback { index } => {
                let Some(runtime) = channels.remove(&index) else { continue };
                runtime.task.abort();
                let endpoint = runtime.endpoint.with_talk(true);
                talk_support.entry(endpoint.uri.clone()).or_default().refuses_call = true;
                tracing::debug!(
                    target: "xgview::talk",
                    camera = %endpoint.camera_id,
                    "this camera takes the back channel on the picture's session"
                );
                let task = handle.spawn(run_channel(
                    endpoint.clone(),
                    prefer_hardware,
                    settings.clone(),
                    events.clone(),
                    frames.clone(),
                    audio.clone(),
                    talk.clone(),
                ));
                channels.insert(index, ChannelRuntime { endpoint, task });
            }
            ChannelCommand::SetPolicy(new_policy) => {
                match settings.lock() {
                    Ok(mut current) => current.reconnect = new_policy,
                    Err(poisoned) => poisoned.into_inner().reconnect = new_policy,
                }
                tracing::debug!(target: "xgview::pipeline", "reconnection policy replaced");
            }
            ChannelCommand::SetHandshakeTimeout(timeout) => {
                match settings.lock() {
                    Ok(mut current) => current.handshake_timeout = timeout,
                    Err(poisoned) => poisoned.into_inner().handshake_timeout = timeout,
                }
                tracing::debug!(
                    target: "xgview::pipeline",
                    timeout_ms = timeout.as_millis() as u64,
                    "handshake timeout replaced"
                );
            }
            ChannelCommand::Shutdown => break,
        }
    }

    for (_, runtime) in channels.drain() {
        runtime.task.abort();
    }
    tracing::debug!(target: "xgview::pipeline", "supervisor stopped");
}

/// The one H.264 declaration a hardware decoder has no mode for.
///
/// A stream that declares plain Baseline - `profile_idc` 66 without
/// `constraint_set1_flag` - finds no entry in the Direct3D 11 / DXVA profile
/// table (`CONSTRAINED_BASELINE`, `MAIN` and `HIGH` are the whole of it), so
/// `dxva_get_decoder_guid` fails and libavcodec falls back to the software
/// decoder. `sps::constrain_baseline` sets that one flag, which is true of every
/// stream here and is what the camera should have said itself. The decision is
/// made once, from the first parameter set a session carries, and applied to
/// every access unit after it; the healthy majority of streams is never touched.
/// Baseline is an H.264 profile, so the codec gates it.
struct BaselineFix {
    decided: bool,
    apply: bool,
}

impl BaselineFix {
    fn new() -> Self {
        Self { decided: false, apply: false }
    }

    /// Rewrites `data` in place when the stream needs it, and says whether it
    /// changed anything. Called with every access unit - and with the MSE init
    /// segment's parameter sets - so the decision is taken from the first set
    /// that arrives, whichever form the session is in.
    fn observe(&mut self, index: usize, codec: Codec, data: &mut [u8]) -> bool {
        if codec == Codec::H264 && !self.decided {
            if let Some(nal) = sps::access_unit_sps(data) {
                self.apply = sps::needs_constrained_baseline(nal);
                self.decided = true;
                if self.apply {
                    // Said out loud: the channel is about to be decoded from
                    // bytes the camera did not send.
                    tracing::info!(
                        target: "xgview::pipeline",
                        index,
                        "declaring the stream's baseline profile as constrained, \
                         which is what the hardware decoder asks of it"
                    );
                }
            }
        }
        self.apply && sps::constrain_baseline(data) > 0
    }
}

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

/// Runs one attempt on one endpoint, opening the session its address calls for.
///
/// The scheme is what decides: an `http://` address is an MJPEG stream, a
/// `ws://`/`wss://` one go2rtc's MSE, and anything else an RTSP session.
async fn run_once(
    endpoint: &Endpoint,
    prefer_hardware: bool,
    handshake_timeout: Duration,
    events: &Sender<StreamEvent>,
    frames: &FrameStore,
    audio: &AudioHub,
    talk: &TalkHub,
) -> Result<()> {
    if endpoint.is_mjpeg() {
        run_mjpeg_session(endpoint.clone(), handshake_timeout, events, frames).await
    } else if endpoint.is_ws() {
        run_ws_session(endpoint.clone(), prefer_hardware, handshake_timeout, events, frames, audio).await
    } else {
        run_session(endpoint.clone(), prefer_hardware, handshake_timeout, events, frames, audio, talk).await
    }
}

/// Reconnection state machine of one channel.
async fn run_channel(
    endpoint: Endpoint,
    prefer_hardware: bool,
    settings: Arc<Mutex<SessionSettings>>,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
    audio: Arc<AudioHub>,
    talk: Arc<TalkHub>,
) {
    let index = endpoint.index;
    let mut attempt: u32 = 0;
    // Reason of the last failure, kept so that the countdown does not hide it:
    // a rejected password or an unreachable device is the only explanation the
    // user has, and the grid tile truncates the detail line.
    let mut failure = "connection attempt".to_string();

    loop {
        attempt += 1;
        // Read afresh every attempt, so a setting the user edits while this
        // channel is running is picked up at its next attempt rather than at
        // the next reopen: retry timing is not worth interrupting a healthy
        // stream for, and a handshake that keeps failing wants the new budget
        // now rather than after a restart.
        let settings = match settings.lock() {
            Ok(settings) => settings.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let state = if attempt == 1 {
            ConnectionState::Connecting
        } else {
            ConnectionState::Reconnecting
        };
        emit(
            &events,
            StreamEvent::State {
                index,
                camera_id: endpoint.camera_id.clone(),
                state,
                detail: format!("#{attempt} {failure}"),
            },
        );

        // Pictures already on screen when the attempt starts, so that an
        // attempt which delivered can be told from one that never did.
        let published = frames.published(index);

        // Each attempt opens its own session, so it is handed its own copy. The
        // sessions have nothing in common beyond the backoff around them, so the
        // endpoint picks the one it needs rather than a single session branching
        // on it statement by statement.
        let started = Instant::now();
        let mut outcome = run_once(&endpoint, prefer_hardware, settings.handshake_timeout, &events, &frames, &audio, &talk).await;

        // `Auto`: RTSP is preferred, and an RTSP that could not be reached at
        // all is answered with the MSE address at once rather than with a
        // backoff - which is the whole point of keeping two addresses. Only when
        // the attempt put nothing on screen: a stream that worked and was then
        // cut keeps to RTSP, so a healthy channel does not flap between the two.
        if outcome.is_err() && endpoint.fallback.is_some() && frames.published(index) == published {
            let fallback = endpoint.fallback.clone().expect("checked above");
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                "rtsp was not reachable, trying the mse address"
            );
            emit(
                &events,
                StreamEvent::State {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    state,
                    detail: format!("#{attempt} rtsp unreachable, trying mse"),
                },
            );
            let mse = Endpoint { uri: fallback.uri, credentials: fallback.credentials, fallback: None, ..endpoint.clone() };
            outcome = run_once(&mse, prefer_hardware, settings.handshake_timeout, &events, &frames, &audio, &talk).await;
        }

        let lived = started.elapsed();
        match outcome {
            Ok(()) => failure = "stream closed by peer".to_string(),
            Err(err) => {
                // A failing session is the only clue the user gets about a
                // rejected password or an unreachable device, so it is
                // reported at `warn` rather than hidden behind `debug`. The
                // causes are walked too: the outermost layer of an HTTP error
                // names only the stage, not what the peer or the transport did.
                tracing::warn!(
                    target: "xgview::pipeline",
                    index,
                    attempt,
                    lived = ?lived,
                    err = %describe(&err),
                    "session failed"
                );
                failure = err.to_string();
            }
        }

        // A session that put pictures on screen is a stream that worked and was
        // then cut, not one that never came up, so the next attempt is worth the
        // shortest delay again. Without this the counter is only ever raised, and
        // a channel that keeps being cut - an MJPEG stream its server resets, say
        // - ends up waiting the ceiling delay between two pictures long after a
        // single quick retry would have brought it back.
        if frames.published(index) > published {
            attempt = 0;
        }

        // The snapshot taken at the top of the attempt is what this reads: one
        // policy for the whole attempt, and the newest one the user set.
        if !settings.reconnect.should_retry(attempt) {
            emit(
                &events,
                StreamEvent::State {
                    index,
                    camera_id: endpoint.camera_id.clone(),
                    state: ConnectionState::Failed,
                    detail: format!("{failure} (gave up after {attempt} attempts)"),
                },
            );
            return;
        }

        let first_frame = frames.published(index) == 0;
        let delay = settings.reconnect.delay_for(attempt, first_frame, index as u32);
        emit(
            &events,
            StreamEvent::State {
                index,
                camera_id: endpoint.camera_id.clone(),
                state: ConnectionState::Reconnecting,
                detail: format!("{failure} | retry in {} ms", delay.as_millis()),
            },
        );
        tokio::time::sleep(delay).await;
    }
}

/// An error followed by every cause under it, outermost first.
///
/// `reqwest` reports the stage a request failed at and keeps the reason in the
/// source chain, so the stage on its own - "error decoding response body" -
/// says nothing about whether the peer closed, the transport reset or a
/// timeout fired.
fn describe(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(" <- ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
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
struct StallDeadline {
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
    fn new() -> Self {
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
    fn observe(&mut self, keyframe: bool, now: Instant) {
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
    fn picture(&mut self, now: Instant) {
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
    fn patience(&self) -> Duration {
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

/// How an attempt at establishing a session ended.
///
/// The two failures that are worth answering with the other transport are kept
/// apart from the rest, and from each other, because the answer differs: a
/// refusal is the server saying so, a timeout is the server saying nothing, and
/// where it went quiet decides whether the other transport is worth a try.
#[derive(Debug)]
enum HandshakeFailure {
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
    fn worth_the_other_transport(&self, transport: RtspTransport) -> bool {
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
                CoreError::rtsp(format!(
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
async fn negotiate(
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

/// One call: a session of its own that carries the microphone and nothing else.
///
/// A task of its own rather than part of the picture's loop, for the two reasons
/// the split exists: opening it takes a handshake, which must not hold up a
/// frame, and a camera that breaks it must not be able to touch the picture.
///
/// It ends when the viewer hangs up, which is [`TalkHub::talking_on`] going
/// false - no command is needed for that, and the three ways a call ends (the
/// handset, the bar closing, the viewport moving) all reach it that way.
async fn talk_worker(
    index: usize,
    endpoint: Endpoint,
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

    let mut channel = match tokio::time::timeout(timeout, Channel::open(&uri, credentials)).await {
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
            if matches!(err, crate::error::CoreError::Rtsp(_)) {
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
async fn run_session(
    endpoint: Endpoint,
    prefer_hardware: bool,
    handshake_timeout: Duration,
    events: &Sender<StreamEvent>,
    frames: &FrameStore,
    audio: &AudioHub,
    talk: &TalkHub,
) -> Result<()> {
    let Endpoint { index, camera_id, uri, credentials, stream, transport, talk: wants_talk, .. } = endpoint;
    let camera_id = camera_id.as_str();
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
    let mut audio_decoder = if transport == RtspTransport::Tcp {
        audio_track_decoder(&tracks)
    } else {
        None
    };

    // The talk-back track, when this session asked for one and the answer
    // carried it. Its codec is the one the media advertises - PCMU or PCMA in
    // practice - and a media this build cannot encode is reported as absent
    // rather than half used: the bar's microphone is drawn on this flag.
    let talk_track = tracks.iter().find(|track| track.is_audio() && track.is_sendonly());
    let talk_codec =
        talk_track.map(|track| AudioCodec::from_encoding(track.encoding.as_deref().unwrap_or_default()));
    let mut talk_sender = client.talk_rtp_channel().and_then(|channel| {
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

    let mut decoder = video_decoder(kind, width, height, index, prefer_hardware);
    // Each codec arrives in its own shape over RTP: an H.264 access unit spread
    // over NAL units, an H.265 one in the RFC 7798 shape - two byte headers, AP
    // and FU instead of STAP and FU-A - or the fragments of a single JPEG scan.
    let mut depacketizer = match kind {
        Codec::Mjpeg => Depacketizer::Jpeg(JpegDepacketizer::new(payload_type)),
        Codec::H265 => Depacketizer::Hevc(HevcDepacketizer::new(payload_type, parameter_sets)),
        _ => Depacketizer::H264(H264Depacketizer::new(payload_type, parameter_sets)),
    };
    let mut sequence: u64 = 0;
    let mut decode_errors: u64 = 0;

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

    emit(
        events,
        StreamEvent::Stats {
            index,
            camera_id: camera_id.to_string(),
            stream,
            codec: codec.clone(),
            width,
            height,
            hardware: uses_hardware(&decoder),
            audio: audio_decoder.is_some(),
            fps: 0.0,
            bitrate_kbps: 0.0,
            total_frames: 0,
        },
    );

    let mut total_frames: u64 = 0;
    let mut window_frames: u64 = 0;
    let mut window_bytes: u64 = 0;
    // Per window counters. A live view that freezes and then runs to catch up is
    // either a starving network or a lagging decoder, and only these numbers
    // tell the two apart.
    let mut window_units: u64 = 0;
    // Annex-B bytes handed to the decoder. Compared against the bitrate of the
    // window it tells whether the pictures arrive whole: assembled bytes far
    // below what the RTP packets carried mean fragments are being thrown away.
    let mut window_unit_bytes: u64 = 0;
    let mut window_decoded: u64 = 0;
    let mut window_errors: u64 = 0;
    // Pictures refused while the session is still waiting for its first one.
    // Kept apart from the errors, which count a session that had a picture and
    // then lost it.
    let mut window_opening: u64 = 0;
    // Marker terminated packets that assembled no access unit, and packets that
    // do not carry the negotiated payload type. A channel whose pictures never
    // assemble is either fragmenting them in a way the depacketizer does not
    // recognise or sending them under a second payload type, and the two are
    // indistinguishable without these counters.
    let mut window_dropped: u64 = 0;
    let mut window_foreign: u64 = 0;
    // Duplicate or late RTP packets dropped on a UDP session. Neither happens
    // on TCP, so the counter stays zero there.
    let mut window_reordered: u64 = 0;
    // RTP packets the sequence numbers say were lost on a UDP session.
    let mut window_lost: u64 = 0;
    // Sequence number of the last RTP packet accepted on a UDP session.
    let mut last_sequence: Option<u16> = None;
    let mut slowest_decode = Duration::ZERO;
    let mut window_start = Instant::now();
    let mut announced = false;
    // The stream's own parameter set, logged once per session. It is the only
    // place the profile, the level, the chroma sampling and the bit depth
    // appear, and a hardware decoder that turns a stream down is otherwise a
    // refusal with no visible cause.
    let mut sps_logged = false;
    // The plain-Baseline declaration a hardware decoder has no mode for; the
    // flag that says otherwise is set before any picture is handed over. See
    // [`BaselineFix`].
    let mut baseline_fix = BaselineFix::new();
    // The server announces how much silence it tolerates. The probe is sent
    // before the read, never after: a check placed after a blocking read is
    // starved for as long as the camera stays quiet.
    let keep_alive_interval = client.keep_alive_interval();
    let mut last_keep_alive = Instant::now();
    // What the receiver report has to carry: the SSRC of the stream and how far
    // its sequence numbers have run.
    let mut last_report = Instant::now();
    let mut source_ssrc: u32 = 0;
    let mut highest_sequence: u32 = 0;
    let mut received_packets = false;
    // Beyond the sequence numbers, the report also says how badly the stream is
    // arriving. Both of these accumulate over the session, which is the window
    // RFC 3550 defines them in.
    //
    // `total_lost` is what the sequence gaps between packets say was never
    // delivered. A camera that honours the report can only act on a count, so
    // reporting the zero a TCP-shaped client would send tells it nothing about
    // a link that is dropping datagrams.
    let mut total_lost: u64 = 0;
    // Interarrival jitter, kept as the difference between consecutive packets
    // rather than against a session origin: the RTP timestamp wraps every
    // thirteen hours or so, and an absolute transit would read that wrap as one
    // enormous jitter.
    let mut jitter: f64 = 0.0;
    let mut last_arrival: Option<(Instant, u32)> = None;
    // RTP timestamp units per second, as the session description announced it.
    let clock_rate = f64::from(video.and_then(|track| track.clock_rate).unwrap_or(90_000));
    // How long this session may go without a picture, learned from the stream
    // itself: see `StallDeadline`.
    let mut stall = StallDeadline::new();

    // The talk-back clock: one packet every 20 ms, silence included, which is
    // what keeps the camera's speaker media alive while the viewer has the
    // microphone switched on. Nothing is sent when the session has no back
    // channel.
    let mut last_talk = Instant::now();

    loop {
        // Talk-back, before waiting on the picture: the session's clock is the
        // RTP clock, and it must not be tied to how often a camera sends video.
        if let Some((stream, channel)) = talk_sender.as_mut() {
            while last_talk.elapsed() >= talk_gap(talk.talking_on(index)) {
                let talking = talk.talking_on(index);
                let elapsed = last_talk.elapsed();
                let packet = if talking {
                    // A gap left by the idle rate is silence the speaker never
                    // heard: the clock is moved over it before the voice starts,
                    // so the talk does not begin a second in the past.
                    if elapsed > TALK_INTERVAL * 2 {
                        stream.skip(elapsed - TALK_INTERVAL);
                    }
                    stream.packet(&talk.take(FRAME_SAMPLES))
                } else {
                    stream.silence_after(elapsed)
                };
                client.send_interleaved(*channel, &packet);
                if talking {
                    last_talk += TALK_INTERVAL;
                } else {
                    last_talk = Instant::now();
                }
                // A loop that fell well behind - a stalled socket, a suspended
                // process - is not worth a burst of catch-up packets: the
                // camera's speaker wants the present, not the backlog.
                if last_talk.elapsed() > TALK_INTERVAL * 4 {
                    last_talk = Instant::now();
                    break;
                }
            }
        }
        // A channel with no decoder has no picture to expect - it shows a
        // placeholder - and is left out of this entirely.
        let patience = stall.patience();
        let since_picture = stall.since_picture();
        if decoder.is_some() && since_picture >= patience {
            // Soft timeout: the picture has frozen, but the transport may
            // still be alive. Hold the last frame and keep waiting rather than
            // dropping to a black reconnect screen, which is the worse
            // experience on a flaky link. The state is announced once, when
            // the stall begins, so the grid can mark it without a flood of
            // events.
            if stall.stalled_since.is_none() {
                stall.stalled_since = Some(Instant::now());
                emit(
                    events,
                    StreamEvent::State {
                        index,
                        camera_id: camera_id.to_string(),
                        state: ConnectionState::Stalled,
                        detail: format!("stalled for {} s", since_picture.as_secs()),
                    },
                );
            }
            // Hard timeout: the stall has gone on long enough that
            // reconnecting is more likely to help than waiting.
            if stall.stalled_since.unwrap().elapsed() >= STALL_HARD_TIMEOUT {
                return Err(crate::error::CoreError::rtsp(format!(
                    "stalled for {} s, giving up",
                    since_picture.as_secs()
                )));
            }
        }
        if last_keep_alive.elapsed() >= keep_alive_interval {
            // Fire and forget. Waiting for the answer would discard every RTP
            // packet that arrives first, and block the pump for good on a device
            // that never answers. Only a failed write is an error.
            client.send_keep_alive().await?;
            last_keep_alive = Instant::now();
        }
        if received_packets && last_report.elapsed() >= REPORT_INTERVAL {
            // Cameras differ in how much they care about being reported to. Some
            // stream regardless, but one was measured sending a single key frame
            // and then nothing but `SEI` until a receiver report arrived, which
            // a viewer shows as a slideshow whose timecode jumps. The counts it
            // carries are the session's real ones, so a camera that does adapt
            // to them has something to adapt to.
            let report = crate::rtcp::receiver_report(
                RECEIVER_SSRC,
                source_ssrc,
                highest_sequence,
                total_lost.min(i32::MAX as u64) as i32,
                jitter.round() as u32,
            );
            client.send_rtcp(&report);
            last_report = Instant::now();
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
        let wait = if since_picture >= patience {
            STALL_HARD_TIMEOUT
                .saturating_sub(stall.stalled_since.unwrap().elapsed())
                .min(Duration::from_secs(1))
        } else {
            READ_TIMEOUT.min(patience.saturating_sub(since_picture))
        };
        // A session feeding a camera's speaker has one more clock to honour:
        // the wait is cut so that the next talk packet is written on time even
        // while the picture keeps the loop from ever timing out. Idle, that
        // clock is the keepalive's, not the frame rate's.
        let talk_wait = talk_sender
            .as_ref()
            .map(|_| talk_gap(talk.talking_on(index)).saturating_sub(last_talk.elapsed()));
        let wait = match talk_wait {
            Some(talk_wait) => wait.min(talk_wait),
            None => wait,
        };
        match tokio::time::timeout(wait, client.wait_for_media()).await {
            Ok(result) => result?,
            Err(_) => {
                // The talk clock waking the loop is not silence on the wire.
                if talk_sender.is_some() && last_talk.elapsed() >= talk_gap(talk.talking_on(index)) {
                    continue;
                }
                // A wait that the picture deadline cut short is not silence on
                // the wire, and reporting it as such would name the wrong fault
                // and the wrong duration. The loop's own check reports that one.
                if stall.since_picture() >= patience {
                    continue;
                }
                return Err(crate::error::CoreError::rtsp(format!(
                    "no media received for {} s",
                    READ_TIMEOUT.as_secs()
                )));
            }
        }
        // Channel 0 carries RTP, channel 1 carries RTCP, on the interleaved
        // transport. UDP has no channels: `read_media` returns RTP only and
        // drains the control socket itself, so only RTP reaches here.
        let (channel, payload) = match client.read_media().await? {
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
            if audio.wants(index) {
                if let Some(output) = audio.output().filter(|output| output.is_active()) {
                    if let Some(track) = audio_decoder.as_mut() {
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
                if transport.is_udp() {
                    if let Some(previous) = last_sequence {
                        let delta = header.sequence.wrapping_sub(previous);
                        if delta == 0 || delta > 0x8000 {
                            // A duplicate, or a straggler that belongs to a
                            // picture already handed over.
                            window_reordered += 1;
                            continue;
                        }
                        if delta > 1 {
                            // A gap: the packets in between were lost, so the
                            // unit being assembled is missing part of its
                            // picture. It is written off rather than stitched
                            // together and decoded into a screenful of
                            // corruption.
                            window_lost += u64::from(delta) - 1;
                            total_lost += u64::from(delta) - 1;
                            depacketizer.invalidate();
                        }
                    }
                    last_sequence = Some(header.sequence);
                }
                window_bytes += payload.len() as u64;
                // The receiver report counts sequence numbers the way RFC 3550
                // does: the 16 bit RTP field folded with the number of times it
                // has wrapped, so a long session does not look like it restarted.
                source_ssrc = header.ssrc;
                received_packets = true;
                let candidate = (highest_sequence & 0xffff_0000) | u32::from(header.sequence);
                highest_sequence = if candidate + 0x8000 < highest_sequence {
                    candidate + 0x1_0000
                } else if candidate > highest_sequence + 0x8000 {
                    candidate.saturating_sub(0x1_0000)
                } else {
                    candidate
                };
                // How far apart this packet and the last one were sent, against
                // how far apart they arrived. RFC 3550 smooths the difference
                // into the jitter the receiver report carries.
                let now = Instant::now();
                if let Some((previous_arrival, previous_timestamp)) = last_arrival {
                    let arrived = now.saturating_duration_since(previous_arrival).as_secs_f64()
                        * clock_rate;
                    let sent = f64::from(header.timestamp.wrapping_sub(previous_timestamp) as i32);
                    let difference = (arrived - sent).abs();
                    jitter += (difference - jitter) / 16.0;
                }
                last_arrival = Some((now, header.timestamp));
                // The marker bit is set on the last packet of an access unit.
                if header.marker && header.payload_type < 192 {
                    window_frames += 1;
                    total_frames += 1;
                }
                if payload_type.is_some_and(|expected| header.payload_type != expected) {
                    window_foreign += 1;
                }
                if !announced {
                    announced = true;
                    emit(
                        events,
                        StreamEvent::State {
                            index,
                            camera_id: camera_id.to_string(),
                            state: ConnectionState::Streaming,
                            detail: format!("receiving {} stream", stream.label()),
                        },
                    );
                }

                if let Some(decoder) = decoder.as_mut() {
                    let payload = &payload[header.header_len..];
                    // A packet completes at most two pictures: the one the RTP
                    // timestamp change released, and the one its marker closed.
                    let units = depacketizer.push(&header, payload);
                    let completed_none = units.is_empty();
                    for mut unit in units {
                        window_units += 1;
                        window_unit_bytes += unit.data.len() as u64;
                        // Once per session, from the first access unit that
                        // carries a parameter set of its own: a camera whose
                        // SDP advertised none only ever says what it is here.
                        // H.264's reader, so an H.265 access unit is left out of
                        // it rather than read with H.264's layout.
                        if kind == Codec::H264 && !sps_logged {
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
                                        index,
                                        profile = info.profile,
                                        level = info.level,
                                        chroma = info.chroma,
                                        bit_depth = info.bit_depth,
                                        scaling_matrices = info.scaling_matrices,
                                        size = ?info.size,
                                        "stream parameter set"
                                    );
                                    sps_logged = true;
                                }
                            }
                        }
                        baseline_fix.observe(index, kind, &mut unit.data);
                        stall.observe(unit.keyframe, Instant::now());
                        let started = Instant::now();
                        let decoded = decoder.decode(&unit.data, unit.pts_us(), unit.keyframe);
                        slowest_decode = slowest_decode.max(started.elapsed());
                        match decoded {
                            Ok(decoded) => {
                                // A picture came out, whatever the renderer can
                                // do with it: on the platform whose frames stay
                                // in a GPU buffer there are no planes to publish,
                                // and the stream is no less alive for it.
                                if !decoded.is_empty() {
                                    let was_stalled = stall.stalled_since.is_some();
                                    stall.picture(Instant::now());
                                    if was_stalled {
                                        emit(
                                            events,
                                            StreamEvent::State {
                                                index,
                                                camera_id: camera_id.to_string(),
                                                state: ConnectionState::Streaming,
                                                detail: format!(
                                                    "receiving {} stream",
                                                    stream.label()
                                                ),
                                            },
                                        );
                                    }
                                }
                                window_decoded += decoded.len() as u64;
                                if let Some((picture_width, picture_height)) =
                                    publish_frames(frames, index, &mut sequence, decoded)
                                {
                                    width = Some(picture_width);
                                    height = Some(picture_height);
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
                                if !stall.running {
                                    window_opening += 1;
                                    if window_opening == 1 {
                                        tracing::debug!(
                                            target: "xgview::pipeline",
                                            index,
                                            %err,
                                            "no picture yet, waiting for the first key frame"
                                        );
                                    }
                                } else {
                                    decode_errors += 1;
                                    window_errors += 1;
                                    // The first rejections carry the payload
                                    // that caused them, which names the NAL
                                    // units the decoder was given.
                                    if decode_errors <= 3 {
                                        tracing::debug!(
                                            target: "xgview::pipeline",
                                            index,
                                            len = unit.data.len(),
                                            types = ?nal_types(&unit.data, kind),
                                            keyframe = unit.keyframe,
                                            au = %hex(&unit.data, 24),
                                            cached = ?depacketizer.parameter_sets(),
                                            "access unit rejected by the decoder"
                                        );
                                    }
                                    if decode_errors == 1 {
                                        tracing::warn!(
                                            target: "xgview::pipeline",
                                            index,
                                            %err,
                                            "decoding failed, waiting for the next key frame"
                                        );
                                    } else {
                                        tracing::debug!(
                                            target: "xgview::pipeline",
                                            index,
                                            errors = decode_errors,
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
                        window_dropped += 1;
                    }
                }
            }
        }

        let elapsed = window_start.elapsed();
        if elapsed >= STATS_INTERVAL {
            let seconds = elapsed.as_secs_f32().max(0.001);
            emit(
                events,
                StreamEvent::Stats {
                    index,
                    camera_id: camera_id.to_string(),
                    stream,
                    codec: codec.clone(),
                    width,
                    height,
                    hardware: uses_hardware(&decoder),
                    audio: audio_decoder.is_some(),
                    fps: window_frames as f32 / seconds,
                    bitrate_kbps: (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                    total_frames,
                },
            );
            // `units` is what the network delivered, `decoded` what came out of
            // the decoder: a frozen picture with a steady `units` blames the
            // decoder (or the renderer), a `units` that drops to zero and then
            // bursts blames the camera, the link, or this loop being starved.
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                stream = stream.as_str(),
                hardware = uses_hardware(&decoder),
                // The picture deadline this session is being held to. It is
                // learned from the stream, so a channel cut off at a number of
                // seconds that is not the one in the source reads here as the
                // group of pictures that camera turned out to have.
                deadline_s = stall.patience().as_secs(),
                units = window_units,
                unit_bytes = window_unit_bytes,
                decoded = window_decoded,
                errors = window_errors,
                opening = window_opening,
                dropped = window_dropped,
                foreign = window_foreign,
                reordered = window_reordered,
                lost = window_lost,
                slowest_decode_ms = slowest_decode.as_millis() as u64,
                fps = window_frames as f32 / seconds,
                kbps = (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                "stream window"
            );
            window_start = Instant::now();
            window_frames = 0;
            window_bytes = 0;
            window_units = 0;
            window_unit_bytes = 0;
            window_decoded = 0;
            window_errors = 0;
            window_opening = 0;
            window_dropped = 0;
            window_foreign = 0;
            window_reordered = 0;
            window_lost = 0;
            slowest_decode = Duration::ZERO;
        }
    }
}

/// Publishes the pictures a decoder produced, in order, and reports the size of
/// the last one.
///
/// `None` means the decoder produced nothing the renderer can take: it needs a
/// luma plane and an interleaved chroma plane, and a backend that hands over
/// anything else - a decoder rendering straight into a GPU buffer, say - has
/// none yet.
fn publish_frames(
    frames: &FrameStore,
    index: usize,
    sequence: &mut u64,
    decoded: Vec<DecodedFrame>,
) -> Option<(u32, u32)> {
    let mut size = None;
    for frame in decoded {
        let mut planes = frame.planes.into_iter();
        let (Some(y), Some(uv)) = (planes.next(), planes.next()) else {
            continue;
        };
        *sequence += 1;
        frames.publish(
            index,
            Arc::new(VideoFrame {
                sequence: *sequence,
                width: frame.width,
                height: frame.height,
                colorspace: frame.colorspace,
                y,
                uv,
            }),
        );
        size = Some((frame.width, frame.height));
    }
    size
}

/// One MJPEG session: one JPEG part after another, decoded as they arrive.
///
/// Nothing of [`run_session`] carries over. There is no RTP to depacketize and
/// no sequence numbers to check, no RTCP to send and no session to keep alive,
/// because the whole conversation is one HTTP response that the server keeps
/// writing pictures into. Keeping the two apart leaves the RTSP path without a
/// branch per statement, and this one without anything it does not need.
///
/// Every part is a complete JPEG and therefore a key frame, so the stream is
/// displayable from its first picture and the decoder never waits for one.
async fn run_mjpeg_session(
    endpoint: Endpoint,
    handshake_timeout: Duration,
    events: &Sender<StreamEvent>,
    frames: &FrameStore,
) -> Result<()> {
    let Endpoint { index, camera_id, uri, stream, .. } = endpoint;
    let camera_id = camera_id.as_str();

    // The response headers are the handshake here, and the client sets no
    // deadline on them: a server that accepts the connection and then says
    // nothing would hold the channel the same way a silent RTSP peer does.
    // Only the connect itself was bounded, and the per picture read below has
    // its own idle timeout - this covers the gap between the two.
    let mut client =
        match tokio::time::timeout(handshake_timeout, MjpegClient::connect(&uri)).await {
            Ok(result) => result?,
            Err(_) => {
                tracing::warn!(
                    target: "xgview::pipeline",
                    index,
                    timeout_s = handshake_timeout.as_secs(),
                    "the stream did not answer, giving up on this attempt"
                );
                return Err(crate::error::CoreError::network(format!(
                    "the stream did not answer within {} s",
                    handshake_timeout.as_secs()
                )));
            }
        };
    let mut decoder = open_decoder(
        &DecoderConfig {
            codec: Codec::Mjpeg,
            width: 0,
            height: 0,
            hardware: false,
        },
        index,
    );

    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut sequence: u64 = 0;
    let mut total_frames: u64 = 0;
    let mut window_frames: u64 = 0;
    // Pictures handed to the renderer, against `window_frames` which counts the
    // parts read: the two differing means the decoder or the renderer is where
    // the stream is being lost, not the network.
    let mut window_pictures: u64 = 0;
    let mut window_bytes: u64 = 0;
    let mut window_errors: u64 = 0;
    let mut window_start = Instant::now();
    let mut announced = false;

    emit(
        events,
        StreamEvent::Stats {
            index,
            camera_id: camera_id.to_string(),
            stream,
            codec: Some("MJPEG".to_string()),
            width,
            height,
            hardware: uses_hardware(&decoder),
            audio: false,
            fps: 0.0,
            bitrate_kbps: 0.0,
            total_frames: 0,
        },
    );

    loop {
        // The read is bounded so that a server which stops writing is noticed.
        // That is the only fault this stream has: a part either arrives or it
        // does not, and there is no sequence number to miss in between.
        let picture = match tokio::time::timeout(MJPEG_IDLE_TIMEOUT, client.next_frame()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(crate::error::CoreError::network(format!(
                    "no picture received for {} s",
                    MJPEG_IDLE_TIMEOUT.as_secs()
                )))
            }
        };

        window_frames += 1;
        total_frames += 1;
        window_bytes += picture.len() as u64;
        if !announced {
            announced = true;
            emit(
                events,
                StreamEvent::State {
                    index,
                    camera_id: camera_id.to_string(),
                    state: ConnectionState::Streaming,
                    detail: format!("receiving {} stream", stream.label()),
                },
            );
        }

        if let Some(decoder) = decoder.as_mut() {
            match decoder.decode(&picture, 0, true) {
                Ok(decoded) => {
                    window_pictures += decoded.len() as u64;
                    if let Some((picture_width, picture_height)) =
                        publish_frames(frames, index, &mut sequence, decoded)
                    {
                        width = Some(picture_width);
                        height = Some(picture_height);
                    }
                }
                // One unreadable picture is not worth a new session: the next
                // part is a self contained JPEG and needs nothing from this one.
                Err(err) => {
                    window_errors += 1;
                    tracing::debug!(
                        target: "xgview::pipeline",
                        index,
                        len = picture.len(),
                        %err,
                        "mjpeg picture rejected by the decoder"
                    );
                }
            }
        }

        let elapsed = window_start.elapsed();
        if elapsed >= STATS_INTERVAL {
            let seconds = elapsed.as_secs_f32().max(0.001);
            let fps = window_frames as f32 / seconds;
            emit(
                events,
                StreamEvent::Stats {
                    index,
                    camera_id: camera_id.to_string(),
                    stream,
                    codec: Some("MJPEG".to_string()),
                    width,
                    height,
                    hardware: uses_hardware(&decoder),
                    audio: false,
                    fps,
                    bitrate_kbps: (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                    total_frames,
                },
            );
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                stream = stream.as_str(),
                frames = window_frames,
                pictures = window_pictures,
                bytes = window_bytes,
                errors = window_errors,
                resolution = ?width.zip(height),
                fps,
                kbps = (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                "mjpeg window"
            );
            window_start = Instant::now();
            window_frames = 0;
            window_pictures = 0;
            window_bytes = 0;
            window_errors = 0;
        }
    }
}

/// Accept any server certificate — the LAN Frigate box uses a self-signed cert
/// and the user already opted in by aiming the camera at an MSE (`wss://`)
/// address.  TLS handshake signatures are still verified cryptographically so
/// the channel is not plaintext.
#[derive(Debug)]
struct NoServerCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let alg = &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, alg)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let alg = &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, alg)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Whether a WebSocket URL is Frigate's MSE endpoint rather than a standalone
/// go2rtc's.
///
/// The two serve the same MSE stream at different paths and authenticate
/// differently: Frigate fronts it at `/live/mse/api/ws` behind its own session
/// login, while go2rtc serves `/api/ws` behind its HTTP API's Basic account.
/// The path is what tells them apart - `live/mse/` is Frigate's, wherever a
/// reverse proxy has placed it.
fn is_frigate_mse(ws_url: &str) -> bool {
    let after_scheme = ws_url.split_once("://").map_or(ws_url, |(_, rest)| rest);
    let path = after_scheme.split_once('/').map_or("", |(_, path)| path);
    path.contains("live/mse/")
}

/// Log in to Frigate's web API and return the JWT token that the WebSocket
/// endpoint expects as a Bearer header.  The base URL is derived from the
/// WebSocket URL (`wss://host:port/…` → `https://host:port`).
async fn frigate_ws_login(ws_url: &str, user: &str, pass: &str, timeout: Duration) -> Result<String> {
    let (scheme, rest) = ws_url
        .split_once("://")
        .ok_or_else(|| crate::error::CoreError::network("bad WebSocket URL"))?;
    let http_scheme = match scheme {
        "wss" => "https",
        "ws" => "http",
        _ => return Err(crate::error::CoreError::network("not a WebSocket URL")),
    };
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let base_url = format!("{http_scheme}://{authority}");

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(timeout)
        .build()?;

    let response = http
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({"user": user, "password": pass}))
        .send()
        .await?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(crate::error::CoreError::network("Frigate rejected the login"));
    }
    if !status.is_success() {
        return Err(crate::error::CoreError::network(format!(
            "Frigate /api/login answered {status}"
        )));
    }

    response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            let (name, rest) = cookie.split_once('=')?;
            (name.trim() == "frigate_token")
                .then(|| rest.split(';').next().unwrap_or_default().to_string())
        })
        .ok_or_else(|| crate::error::CoreError::network("Frigate login did not return a token"))
}

/// A connected MSE WebSocket, with the handshake sent but no message read yet.
pub(crate) type MseSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens a go2rtc MSE WebSocket: builds the request with the endpoint's
/// credentials, connects (accepting a self-signed certificate), and sends the
/// MSE handshake. The caller reads the messages that follow - the JSON reply,
/// then the fMP4 segments - so the same opener serves the playback session and
/// the discovery probe.
///
/// The two servers authenticate differently: Frigate fronts its MSE stream
/// behind the same JWT login as its REST API (Basic is rejected with 401),
/// while a standalone go2rtc answers its own `/api/ws` behind the HTTP Basic
/// account its `api:` section names - see [`is_frigate_mse`].
pub(crate) async fn open_mse(
    uri: &str,
    credentials: Option<&(String, String)>,
    handshake_timeout: Duration,
) -> Result<MseSocket> {
    use futures::SinkExt;
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let mut request = uri
        .into_client_request()
        .map_err(|e| crate::error::CoreError::network(format!("bad WebSocket URL: {e}")))?;
    if let Some((user, pass)) = credentials {
        let authorization = if is_frigate_mse(uri) {
            let token = frigate_ws_login(uri, user, pass, handshake_timeout).await?;
            format!("Bearer {token}")
        } else {
            use base64::Engine;
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            format!("Basic {token}")
        };
        request.headers_mut().insert("Authorization", authorization.try_into().unwrap());
    }

    // A TLS connector that accepts self-signed certificates (Frigate's LAN box
    // uses one). For plain `ws://` no connector is needed.
    let connector = if uri.starts_with("wss://") {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| crate::error::CoreError::network(format!("rustls config error: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoServerCertVerifier))
            .with_no_client_auth();
        Some(tokio_tungstenite::Connector::Rustls(Arc::new(cfg)))
    } else {
        None
    };

    let connect = connect_async_tls_with_config(request, None, false, connector);
    let mut ws = match tokio::time::timeout(handshake_timeout, connect).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            return Err(crate::error::CoreError::network(format!("WebSocket handshake failed: {e}")));
        }
        Err(_) => {
            return Err(crate::error::CoreError::network(format!(
                "the stream did not answer within {} s",
                handshake_timeout.as_secs()
            )));
        }
    };

    // MSE handshake: tell go2rtc we want MSE mode with H.264/H.265 video.
    ws.send(Message::text(
        r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029,hvc1.1.6.L153.B0\""}"#,
    ))
    .await
    .map_err(|e| crate::error::CoreError::network(format!("WebSocket send failed: {e}")))?;

    Ok(ws)
}

/// One WebSocket (MSE) session: connect to a go2rtc MSE endpoint - Frigate's
/// `/live/mse/api/ws` or a standalone go2rtc's `/api/ws` - receive fMP4
/// segments, decode the H.264/H.265 access units inside them and publish the
/// pictures.
///
/// The protocol is go2rtc's MSE consumer: the client opens a WebSocket, sends a
/// `{"type":"mse","value":"…"}` JSON handshake, then receives binary frames
/// that are fragmented MP4 — an init segment (`ftyp`+`moov` with the codec
/// configuration) followed by media segments (`moof`+`mdat` with the samples).
/// Each sample is length-prefixed (AVCC/HVCC); the [`MseParser`] converts them
/// to Annex-B so the existing decoders consume them unchanged.
async fn run_ws_session(
    endpoint: Endpoint,
    prefer_hardware: bool,
    handshake_timeout: Duration,
    events: &Sender<StreamEvent>,
    frames: &FrameStore,
    audio: &AudioHub,
) -> Result<()> {

    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    // The audio track's decoder, when the init segment carried an audio trak.
    let mut audio_decoder: Option<(Box<dyn AudioDecoder>, AudioStreamInfo)> = None;

    let Endpoint { index, camera_id, uri, stream, credentials, .. } = endpoint;
    let camera_id_str = camera_id.as_str();

    // Open the MSE socket and leave the handshake sent; the messages that
    // follow - the JSON reply, then the fMP4 segments - are read below.
    let mut ws = open_mse(&uri, credentials.as_ref(), handshake_timeout).await?;

    let mut parser = crate::mse::MseParser::new();
    let mut decoder: Option<Box<dyn VideoDecoder>> = None;
    // The codec the init segment named, so the plain-Baseline fix can apply to
    // the media segments - and the fix itself, decided from the init's parameter
    // sets. See [`BaselineFix`].
    let mut codec: Option<Codec> = None;
    let mut baseline_fix = BaselineFix::new();
    let mut codec_label: Option<String> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut sequence: u64 = 0;
    let mut total_frames: u64 = 0;
    let mut window_frames: u64 = 0;
    let mut window_pictures: u64 = 0;
    let mut window_bytes: u64 = 0;
    let mut window_errors: u64 = 0;
    let mut window_start = Instant::now();
    let mut announced = false;

    loop {
        let msg = match tokio::time::timeout(MJPEG_IDLE_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => {
                return Err(crate::error::CoreError::network(format!(
                    "WebSocket read failed: {e}"
                )));
            }
            Ok(None) => {
                return Err(crate::error::CoreError::network(
                    "WebSocket closed by peer",
                ));
            }
            Err(_) => {
                return Err(crate::error::CoreError::network(format!(
                    "no data received for {} s",
                    MJPEG_IDLE_TIMEOUT.as_secs()
                )));
            }
        };

        let data = match msg {
            Message::Binary(data) => data,
            Message::Text(_) => continue,
            Message::Close(_) => {
                return Err(crate::error::CoreError::network(
                    "WebSocket closed by peer",
                ));
            }
            _ => continue,
        };

        window_bytes += data.len() as u64;

        let parsed = parser.parse_message(&data)?;

        // The audio track's description, when the init segment carried one:
        // open its decoder so the media segments can be fed to it below.
        if let Some(audio_init) = parsed.audio {
            let mut info = AudioStreamInfo::new(audio_init.codec, audio_init.sample_rate, audio_init.channels);
            info.extradata = audio_init.extradata.clone();
            let mut decoder = create_audio_decoder(audio_init.codec);
            match decoder.configure(&info) {
                Ok(()) => audio_decoder = Some((decoder, info)),
                Err(err) => tracing::debug!(
                    target: "xgview::pipeline",
                    index,
                    codec = %audio_init.codec,
                    error = %err,
                    "no audio backend for this MSE track, the channel stays silent"
                ),
            }
        }

        // Init segment: open the decoder and feed it the parameter sets.
        if let Some(mut init) = parsed.init {
            codec = Some(init.codec);
            codec_label = Some(match init.codec {
                Codec::H264 => "H264".to_string(),
                Codec::H265 => "H265".to_string(),
                other => other.as_str().to_string(),
            });
            width = if init.width > 0 { Some(init.width) } else { width };
            height = if init.height > 0 { Some(init.height) } else { height };
            decoder = video_decoder(init.codec, Some(init.width), Some(init.height), index, prefer_hardware);
            // The init segment's parameter sets carry the sequence parameter set,
            // so the plain-Baseline decision is taken and applied here - before
            // they are handed to the decoder, which is where libavcodec reads
            // the profile and picks the hardware path over the software one.
            baseline_fix.observe(index, init.codec, &mut init.param_sets);
            if let Some(dec) = decoder.as_mut() {
                if !init.param_sets.is_empty() {
                    if let Err(err) = dec.decode(&init.param_sets, 0, true) {
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index,
                            %err,
                            "MSE param sets rejected by decoder (non-fatal, may arrive with first IDR)",
                        );
                    }
                }
            }
            emit(
                events,
                StreamEvent::Stats {
                    index,
                    camera_id: camera_id_str.to_string(),
                    stream,
                    codec: codec_label.clone(),
                    width,
                    height,
                    hardware: uses_hardware(&decoder),
                    audio: audio_decoder.is_some(),
                    fps: 0.0,
                    bitrate_kbps: 0.0,
                    total_frames: 0,
                },
            );
        }

        // The audio units that rode along in the same segment, decoded while this
        // channel is the audible one and the sink is open.
        for sample in parsed.audio_samples {
            if audio.wants(index) {
                if let Some(output) = audio.output().filter(|output| output.is_active()) {
                    if let Some((decoder, info)) = audio_decoder.as_mut() {
                        let mut pcm: Vec<i16> = Vec::new();
                        if decoder.decode(&sample.data, &mut pcm).is_ok() && !pcm.is_empty() {
                            output.configure(info);
                            output.push(&pcm);
                        }
                    }
                }
            }
        }

        // Media segment: decode each access unit.
        for mut sample in parsed.samples {
            window_frames += 1;
            total_frames += 1;

            if !announced {
                announced = true;
                emit(
                    events,
                    StreamEvent::State {
                        index,
                        camera_id: camera_id_str.to_string(),
                        state: ConnectionState::Streaming,
                        detail: format!("receiving {} stream", stream.label()),
                    },
                );
            }

            // The same plain-Baseline fix the RTSP path applies - an in-band
            // sequence parameter set is the only place it could still be decided
            // if the init segment carried none.
            if let Some(codec) = codec {
                baseline_fix.observe(index, codec, &mut sample.data);
            }

            if let Some(dec) = decoder.as_mut() {
                match dec.decode(&sample.data, sample.pts_us, sample.keyframe) {
                    Ok(decoded) => {
                        window_pictures += decoded.len() as u64;
                        if let Some((w, h)) = publish_frames(frames, index, &mut sequence, decoded) {
                            width = Some(w);
                            height = Some(h);
                        }
                    }
                    Err(err) => {
                        window_errors += 1;
                        tracing::debug!(
                            target: "xgview::pipeline",
                            index,
                            len = sample.data.len(),
                            %err,
                            "MSE sample rejected by decoder",
                        );
                    }
                }
            }
        }

        let elapsed = window_start.elapsed();
        if elapsed >= STATS_INTERVAL {
            let seconds = elapsed.as_secs_f32().max(0.001);
            let fps = window_frames as f32 / seconds;
            emit(
                events,
                StreamEvent::Stats {
                    index,
                    camera_id: camera_id_str.to_string(),
                    stream,
                    codec: codec_label.clone(),
                    width,
                    height,
                    hardware: uses_hardware(&decoder),
                    audio: audio_decoder.is_some(),
                    fps,
                    bitrate_kbps: (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                    total_frames,
                },
            );
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                stream = stream.as_str(),
                frames = window_frames,
                pictures = window_pictures,
                bytes = window_bytes,
                errors = window_errors,
                resolution = ?width.zip(height),
                fps,
                kbps = (window_bytes as f32 * 8.0 / 1000.0) / seconds,
                "mse window",
            );
            window_start = Instant::now();
            window_frames = 0;
            window_pictures = 0;
            window_bytes = 0;
            window_errors = 0;
        }
    }
}

/// Creates the decoder for a stream's codec.
///
/// H.264, H.265 and MJPEG are wired to a backend. H.265 is decoded on the
/// desktop by FFmpeg, which reads the size and the parameter sets from the
/// bitstream, and on Android by `AMediaCodec`, which is handed the VPS, SPS and
/// PPS as `csd-0`/`csd-1`/`csd-2` and the size read out of the stream's own
/// sequence parameter set. There is no software H.265 on Android - OpenH264 is
/// H.264 alone - so a device whose platform decoder will not take an HEVC
/// stream shows no picture for it, the same placeholder an unrecognised codec
/// gets.
///
/// An unrecognised codec has no decoder anywhere, and that is not an error: the
/// session keeps running, reports its statistics and shows a placeholder rather
/// than a connection that never stops reconnecting.
///
/// Either way the depacketizer has already turned the packets into whole
/// pictures, so what reaches the decoder here is the same shape whether the
/// codec arrived as NAL units over RTSP or as JPEG fragments.
///
/// `prefer_hardware` only asks. A decoder that will not open is retried on the
/// CPU, because a preference must never cost a channel its picture. It is not
/// asked of MJPEG, whose decoder is a Rust loop over the picture whatever the
/// setting says: asking would only open a GPU context no frame ever reaches.
fn video_decoder(
    kind: Codec,
    width: Option<u32>,
    height: Option<u32>,
    index: usize,
    prefer_hardware: bool,
) -> Option<Box<dyn VideoDecoder>> {
    let supported = matches!(kind, Codec::H264 | Codec::H265 | Codec::Mjpeg);
    if !supported {
        tracing::warn!(
            target: "xgview::pipeline",
            index,
            codec = kind.as_str(),
            "no decoder for this codec, the stream will not be displayed"
        );
        return None;
    }
    // H.264 and H.265 have a hardware path; MJPEG does not.
    let prefer_hardware = prefer_hardware && matches!(kind, Codec::H264 | Codec::H265);
    let config = DecoderConfig {
        codec: kind,
        width: width.unwrap_or(0),
        height: height.unwrap_or(0),
        hardware: prefer_hardware,
    };
    if prefer_hardware {
        if let Some(decoder) = open_decoder(&config, index) {
            return Some(decoder);
        }
        tracing::warn!(
            target: "xgview::pipeline",
            index,
            "the hardware decoder did not open, decoding on the cpu instead"
        );
    }
    open_decoder(&DecoderConfig { hardware: false, ..config }, index)
}

/// Opens one decoder, reporting the reason when it will not open.
fn open_decoder(config: &DecoderConfig, index: usize) -> Option<Box<dyn VideoDecoder>> {
    let mut decoder = monitor_codec::create_decoder(config.codec);
    match decoder.configure(config) {
        Ok(()) => Some(decoder),
        Err(err) => {
            tracing::warn!(
                target: "xgview::pipeline",
                index,
                hardware = config.hardware,
                %err,
                "cannot configure the decoder"
            );
            None
        }
    }
}

/// True once a channel's decoder has proved it runs on the GPU.
fn uses_hardware(decoder: &Option<Box<dyn VideoDecoder>>) -> bool {
    decoder.as_ref().and_then(|decoder| decoder.info()).is_some_and(|info| info.hardware)
}

fn emit(events: &Sender<StreamEvent>, event: StreamEvent) {
    // A closed receiver simply means the UI has shut down.
    let _ = events.send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CameraOrigin, TileAspect};

    /// A "yes" is kept for the run and a "no" is asked again after a while; a
    /// failure is not an answer at all, so the next magnification asks again.
    #[test]
    fn a_talk_answer_is_trusted_for_as_long_as_it_is_worth() {
        let mut entry = TalkEntry::default();
        assert!(!entry.fresh(), "nothing has been asked yet");

        entry.support = TalkSupport::Supported;
        entry.answered = Some(Instant::now());
        assert!(entry.fresh(), "a camera that offered a back channel still does");

        entry.support = TalkSupport::Unsupported;
        assert!(entry.fresh(), "a fresh refusal is worth believing");
        entry.answered = Some(Instant::now() - UNSUPPORTED_TTL - Duration::from_secs(1));
        assert!(!entry.fresh(), "but it is asked again after a while");

        entry.support = TalkSupport::Unknown;
        entry.answered = None;
        assert!(!entry.fresh(), "a failed probe caches nothing");
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
}
