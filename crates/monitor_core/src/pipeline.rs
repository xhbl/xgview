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

use monitor_codec::sps::picture_size;
use monitor_codec::{Codec, ColorSpace, DecodedFrame, DecoderConfig, VideoDecoder};

use crate::config::ReconnectPolicy;
use crate::error::Result;
use crate::h264::{nal_unit_type, AccessUnit, H264Depacketizer, START_CODE};
use crate::mjpeg::{JpegDepacketizer, MjpegClient};
use crate::model::{is_http_url, CameraSource, ConnectionState, RtspTransport, StreamKind};
use crate::rtsp::{parse_rtp_header, MediaPacket, RtspClient, RtpHeader};
use crate::scheduler::ScheduleChange;

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
        /// Frames per second measured on RTP marker bits.
        fps: f32,
        bitrate_kbps: f32,
        total_frames: u64,
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
            | StreamEvent::Transition { index, .. } => *index,
        }
    }

    pub fn camera_id(&self) -> &str {
        match self {
            StreamEvent::State { camera_id, .. }
            | StreamEvent::Stats { camera_id, .. }
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
    Shutdown,
}

/// Handle owned by the UI thread.
#[derive(Debug)]
pub struct ChannelManager {
    commands: UnboundedSender<ChannelCommand>,
    events: Receiver<StreamEvent>,
    frames: Arc<FrameStore>,
    handle: Handle,
}

impl ChannelManager {
    /// Spawns the supervisor on the given runtime.
    ///
    /// `prefer_hardware` is the decoding preference live channels are opened
    /// with; see [`ChannelManager::set_hardware_preference`].
    pub fn spawn(handle: &Handle, policy: ReconnectPolicy, prefer_hardware: bool) -> Self {
        let (command_tx, command_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded();
        let frames = Arc::new(FrameStore::default());
        handle.spawn(supervisor(
            command_rx,
            event_tx,
            frames.clone(),
            policy,
            prefer_hardware,
            handle.clone(),
        ));
        Self { commands: command_tx, events: event_rx, frames, handle: handle.clone() }
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

    /// The runtime used by the streaming tasks (the UI uses it to spawn the
    /// discovery and import jobs as well).
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// The newest decoded picture of every channel.
    pub fn frames(&self) -> &Arc<FrameStore> {
        &self.frames
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
}

impl Endpoint {
    /// True when this is the same stream as the one already running, so that a
    /// revisit of a channel does not reopen it. The camera is not compared: the
    /// URI it contributes is, and so is the transport it is pulled with.
    fn is_same_stream(&self, other: &Endpoint) -> bool {
        self.stream == other.stream
            && self.uri == other.uri
            && self.credentials == other.credentials
            && self.transport == other.transport
    }

    /// True when the stream is an MJPEG endpoint rather than an RTSP session.
    ///
    /// A device can carry both: a Synology camera has its main stream on RTSP
    /// and its sub stream on HTTP MJPEG, so one `Endpoint` shape has to serve
    /// either and the URL is what picks the session it opens.
    fn is_mjpeg(&self) -> bool {
        is_http_url(&self.uri)
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

async fn supervisor(
    mut commands: UnboundedReceiver<ChannelCommand>,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
    policy: ReconnectPolicy,
    mut prefer_hardware: bool,
    handle: Handle,
) {
    let mut channels: HashMap<usize, ChannelRuntime> = HashMap::new();

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
                let endpoint = Endpoint {
                    index,
                    camera_id: source.id.clone(),
                    uri: source.stream_uri(stream).to_string(),
                    credentials: credentials_of(&source),
                    stream,
                    transport: source.transport,
                };
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
                    policy.clone(),
                    events.clone(),
                    frames.clone(),
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
                        policy.clone(),
                        events.clone(),
                        frames.clone(),
                    ));
                    channels.insert(index, ChannelRuntime { endpoint, task });
                }
            }
            ChannelCommand::Shutdown => break,
        }
    }

    for (_, runtime) in channels.drain() {
        runtime.task.abort();
    }
    tracing::debug!(target: "xgview::pipeline", "supervisor stopped");
}

/// Reconnection state machine of one channel.
async fn run_channel(
    endpoint: Endpoint,
    prefer_hardware: bool,
    policy: ReconnectPolicy,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
) {
    let index = endpoint.index;
    let mut attempt: u32 = 0;
    // Reason of the last failure, kept so that the countdown does not hide it:
    // a rejected password or an unreachable device is the only explanation the
    // user has, and the grid tile truncates the detail line.
    let mut failure = "connection attempt".to_string();

    loop {
        attempt += 1;
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
        // two transports have nothing in common beyond the backoff around them,
        // so the endpoint picks the one it needs rather than a single session
        // branching on it statement by statement.
        let started = Instant::now();
        let outcome = if endpoint.is_mjpeg() {
            run_mjpeg_session(endpoint.clone(), &events, &frames).await
        } else {
            run_session(endpoint.clone(), prefer_hardware, &events, &frames).await
        };
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

        if !policy.should_retry(attempt) {
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

        let delay = policy.delay_for(attempt);
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

/// NAL unit types of an Annex-B access unit, in order.
///
/// A rejected picture is named by what it holds: a slice that arrived without
/// its parameter sets reads as `[7, 1]` or `[8, 1]`, while a complete picture
/// that the decoder merely has no reference for reads as `[7, 8, 1]`.
fn nal_types(data: &[u8]) -> Vec<u8> {
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
            types.push(nal_unit_type(first));
        }
    }
    types
}

/// Type and length of every cached parameter set.
fn sets(depacketizer: &H264Depacketizer) -> Vec<(u8, usize)> {
    depacketizer
        .parameter_sets()
        .map(|nal| (nal_unit_type(nal[0]), nal.len()))
        .collect()
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
/// loop around it is the same for both: a packet goes in, zero or more complete
/// pictures come out. Only these two codecs reach a decoder, so only these two
/// reassemblies are needed.
enum Depacketizer {
    /// H.264, the NAL units of an access unit spread over RTP packets.
    H264(H264Depacketizer),
    /// MJPEG, the fragments of one JPEG scan spread over RTP packets.
    Jpeg(JpegDepacketizer),
}

impl Depacketizer {
    fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> Vec<AccessUnit> {
        match self {
            Depacketizer::H264(depacketizer) => depacketizer.push(header, payload),
            Depacketizer::Jpeg(depacketizer) => depacketizer.push(header, payload),
        }
    }

    /// Writes off the picture being assembled, after a gap in the RTP sequence
    /// numbers.
    fn invalidate(&mut self) {
        match self {
            Depacketizer::H264(depacketizer) => depacketizer.invalidate_current(),
            Depacketizer::Jpeg(depacketizer) => depacketizer.invalidate(),
        }
    }

    /// Cached parameter sets, which only an H.264 stream has: a JPEG carries
    /// its tables in the stream rather than in a cache.
    fn parameter_sets(&self) -> Vec<(u8, usize)> {
        match self {
            Depacketizer::H264(depacketizer) => sets(depacketizer),
            Depacketizer::Jpeg(_) => Vec::new(),
        }
    }
}

/// One RTSP session: negotiation then packet pumping.
async fn run_session(
    endpoint: Endpoint,
    prefer_hardware: bool,
    events: &Sender<StreamEvent>,
    frames: &FrameStore,
) -> Result<()> {
    let Endpoint { index, camera_id, uri, credentials, stream, transport } = endpoint;
    let camera_id = camera_id.as_str();
    let uri = uri.as_str();
    let mut client = RtspClient::connect_with_auth(uri, credentials.clone()).await?;
    // A server may take one transport and refuse the other, which is worth a
    // second attempt rather than a failed session: go2rtc answers every UDP
    // `SETUP` with 461 on purpose, so that a client asking for UDP first falls
    // back, and a preference must never cost a channel its picture. The session
    // is opened afresh rather than patched, because the refused attempt left
    // the old one half set up.
    let mut transport = transport;
    let tracks = match client.start_with_transport(transport).await {
        Ok(tracks) => tracks,
        Err(crate::error::CoreError::Transport(_)) if transport.is_udp() => {
            tracing::debug!(
                target: "xgview::pipeline",
                index,
                "the server turned udp down, reopening the session over tcp"
            );
            client = RtspClient::connect_with_auth(uri, credentials).await?;
            transport = RtspTransport::Tcp;
            client.start_with_transport(transport).await?
        }
        Err(err) => return Err(err),
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

    // `a=framesize` is optional, but a decoder has to be sized before its first
    // picture arrives: Android builds its image reader to that size, and a
    // reader that does not match the stream cannot be read back at all. The
    // sequence parameter set the SDP advertises carries the real size, so it is
    // read from there when the SDP itself did not name one.
    if width.is_none() || height.is_none() {
        if let Some((sps_width, sps_height)) =
            parameter_sets.iter().find_map(|nal| picture_size(nal))
        {
            width = width.or(Some(sps_width));
            height = height.or(Some(sps_height));
        }
    }

    let mut decoder = video_decoder(kind, width, height, index, prefer_hardware);
    // Each codec arrives in its own shape over RTP: an H.264 access unit spread
    // over NAL units, or the fragments of a single JPEG scan.
    let mut depacketizer = match kind {
        Codec::Mjpeg => Depacketizer::Jpeg(JpegDepacketizer::new(payload_type)),
        _ => Depacketizer::H264(H264Depacketizer::new(payload_type, parameter_sets)),
    };
    let mut sequence: u64 = 0;
    let mut decode_errors: u64 = 0;

    tracing::debug!(
        target: "xgview::pipeline",
        index,
        codec = ?codec,
        resolution = ?video.and_then(|track| track.resolution()),
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
    // How long this session may go without a picture, learned from the stream
    // itself: see `StallDeadline`.
    let mut stall = StallDeadline::new();

    loop {
        // A channel with no decoder has no picture to expect - it shows a
        // placeholder - and is left out of this entirely.
        let patience = stall.patience();
        let since_picture = stall.since_picture();
        if decoder.is_some() && since_picture >= patience {
            return Err(crate::error::CoreError::rtsp(format!(
                "no picture decoded for {} s",
                since_picture.as_secs()
            )));
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
            // a viewer shows as a slideshow whose timecode jumps.
            let report = crate::rtcp::receiver_report(RECEIVER_SSRC, source_ssrc, highest_sequence, 0, 0);
            client.send_rtcp(&report).await?;
            last_report = Instant::now();
        }

        // Waiting no longer than the picture deadline allows, so that a session
        // gone quiet is noticed on time rather than at the next packet. The wait
        // is a peek at the transport and the packet is only read once something
        // has arrived: a read cancelled part way through a packet takes the bytes
        // it has already consumed with it, and every packet after them is then
        // read at the wrong offset, which costs the session where the wait would
        // only have cost a wait.
        let wait = READ_TIMEOUT.min(patience.saturating_sub(since_picture));
        match tokio::time::timeout(wait, client.wait_for_media()).await {
            Ok(result) => result?,
            Err(_) => {
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
        };

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
                    for unit in units {
                        window_units += 1;
                        window_unit_bytes += unit.data.len() as u64;
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
                                    stall.picture(Instant::now());
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
                                            types = ?nal_types(&unit.data),
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
    events: &Sender<StreamEvent>,
    frames: &FrameStore,
) -> Result<()> {
    let Endpoint { index, camera_id, uri, stream, .. } = endpoint;
    let camera_id = camera_id.as_str();

    let mut client = MjpegClient::connect(&uri).await?;
    let mut decoder = open_decoder(
        &DecoderConfig {
            codec: Codec::Mjpeg,
            width: 0,
            height: 0,
            low_latency: true,
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

/// Creates the decoder for a stream's codec.
///
/// H.264 and MJPEG are wired to a backend; HEVC and an unrecognised codec are
/// not, and that is not an error: the session keeps running, reports its
/// statistics and shows a placeholder, which is what an `H265` camera should
/// look like rather than a connection that never stops reconnecting.
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
    if !matches!(kind, Codec::H264 | Codec::Mjpeg) {
        tracing::warn!(
            target: "xgview::pipeline",
            index,
            codec = kind.as_str(),
            "no decoder for this codec, the stream will not be displayed"
        );
        return None;
    }
    let prefer_hardware = prefer_hardware && kind == Codec::H264;
    let config = DecoderConfig {
        codec: kind,
        width: width.unwrap_or(0),
        height: height.unwrap_or(0),
        low_latency: true,
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
            username: None,
            password: None,
            enabled: true,
            tags: Vec::new(),
            origin: CameraOrigin::Manual,
            transport: RtspTransport::default(),
            aspect: TileAspect::default(),
            main_profile: None,
            sub_profile: None,
        }
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
        let manager = ChannelManager::spawn(&handle, policy, false);
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
