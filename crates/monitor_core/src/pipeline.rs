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

use monitor_codec::{Codec, DecoderConfig, VideoDecoder};

use crate::config::ReconnectPolicy;
use crate::error::Result;
use crate::h264::{nal_unit_type, H264Depacketizer, START_CODE};
use crate::model::{CameraSource, ConnectionState, StreamKind};
use crate::rtsp::{parse_rtp_header, RtspClient};
use crate::scheduler::ScheduleChange;

/// Time without any RTP payload after which the session is considered dead.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
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
    /// Luma plane, `width * height` bytes.
    pub y: Vec<u8>,
    /// Interleaved chroma, `width * height / 2` bytes: one byte of U and one of V
    /// for every two luma samples.
    pub uv: Vec<u8>,
}

/// The newest picture of every channel.
///
/// A channel publishes and the renderer reads, both without ever blocking. A
/// repaint that falls behind therefore skips the pictures it did not see
/// instead of queueing them, which is what keeps a 4x4 grid of 25 fps streams
/// from building an unbounded backlog of megabytes.
#[derive(Debug, Default)]
pub struct FrameStore {
    frames: Mutex<HashMap<usize, Arc<VideoFrame>>>,
}

impl FrameStore {
    fn publish(&self, index: usize, frame: Arc<VideoFrame>) {
        if let Ok(mut frames) = self.frames.lock() {
            frames.insert(index, frame);
        }
    }

    /// Newest picture of a channel, `None` until the first one is decoded.
    pub fn latest(&self, index: usize) -> Option<Arc<VideoFrame>> {
        self.frames.lock().ok()?.get(&index).cloned()
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

struct ChannelRuntime {
    camera_id: String,
    stream: StreamKind,
    uri: String,
    credentials: Option<(String, String)>,
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
                let uri = source.stream_uri(stream).to_string();
                let credentials = credentials_of(&source);
                if let Some(existing) = channels.get(&index) {
                    if existing.stream == stream
                        && existing.uri == uri
                        && existing.credentials == credentials
                    {
                        continue;
                    }
                }
                if let Some(existing) = channels.remove(&index) {
                    existing.task.abort();
                }
                let camera_id = source.id.clone();
                if previous_stream.is_some() {
                    let _ = events.send(StreamEvent::Transition {
                        index,
                        camera_id: camera_id.clone(),
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
                    index,
                    camera_id.clone(),
                    uri.clone(),
                    credentials.clone(),
                    stream,
                    prefer_hardware,
                    policy.clone(),
                    events.clone(),
                    frames.clone(),
                ));
                channels.insert(
                    index,
                    ChannelRuntime { camera_id, stream, uri, credentials, task },
                );
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
                    let ChannelRuntime { camera_id, stream, uri, credentials, task } = runtime;
                    task.abort();
                    let task = handle.spawn(run_channel(
                        index,
                        camera_id.clone(),
                        uri.clone(),
                        credentials.clone(),
                        stream,
                        prefer_hardware,
                        policy.clone(),
                        events.clone(),
                        frames.clone(),
                    ));
                    channels.insert(
                        index,
                        ChannelRuntime { camera_id, stream, uri, credentials, task },
                    );
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
    index: usize,
    camera_id: String,
    uri: String,
    credentials: Option<(String, String)>,
    stream: StreamKind,
    prefer_hardware: bool,
    policy: ReconnectPolicy,
    events: Sender<StreamEvent>,
    frames: Arc<FrameStore>,
) {
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
                camera_id: camera_id.clone(),
                state,
                detail: format!("#{attempt} {failure}"),
            },
        );

        match run_session(
            index,
            &camera_id,
            &uri,
            credentials.clone(),
            stream,
            prefer_hardware,
            &events,
            &frames,
        )
        .await
        {
            Ok(()) => failure = "stream closed by peer".to_string(),
            Err(err) => {
                // A failing session is the only clue the user gets about a
                // rejected password or an unreachable device, so it is
                // reported at `warn` rather than hidden behind `debug`.
                tracing::warn!(target: "xgview::pipeline", index, attempt, %err, "session failed");
                failure = err.to_string();
            }
        }

        if !policy.should_retry(attempt) {
            emit(
                &events,
                StreamEvent::State {
                    index,
                    camera_id: camera_id.clone(),
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
                camera_id: camera_id.clone(),
                state: ConnectionState::Reconnecting,
                detail: format!("{failure} | retry in {} ms", delay.as_millis()),
            },
        );
        tokio::time::sleep(delay).await;
    }
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

/// One RTSP session: negotiation then packet pumping.
async fn run_session(
    index: usize,
    camera_id: &str,
    uri: &str,
    credentials: Option<(String, String)>,
    stream: StreamKind,
    prefer_hardware: bool,
    events: &Sender<StreamEvent>,
    frames: &FrameStore,
) -> Result<()> {
    let mut client = RtspClient::connect_with_auth(uri, credentials).await?;
    let tracks = client.start().await?;
    let video = tracks.iter().find(|track| track.is_video());
    let codec = video.and_then(|track| track.encoding.clone());
    // The SDP only guesses the resolution: `a=framesize` is optional, so the
    // real size is taken from the first decoded picture.
    let mut width = video.and_then(|track| track.width);
    let mut height = video.and_then(|track| track.height);
    let payload_type = video.and_then(|track| track.payload_type);
    let parameter_sets = video.map(|track| track.parameter_sets.clone()).unwrap_or_default();
    let kind = Codec::from_encoding(codec.as_deref().unwrap_or_default());

    let mut decoder = h264_decoder(kind, width, height, index, prefer_hardware);
    let mut depacketizer = H264Depacketizer::new(payload_type, parameter_sets);
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
        sets = ?sets(&depacketizer),
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
    // Marker terminated packets that assembled no access unit, and packets that
    // do not carry the negotiated payload type. A channel whose pictures never
    // assemble is either fragmenting them in a way the depacketizer does not
    // recognise or sending them under a second payload type, and the two are
    // indistinguishable without these counters.
    let mut window_dropped: u64 = 0;
    let mut window_foreign: u64 = 0;
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

    loop {
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
            client.send_interleaved(1, &report).await?;
            last_report = Instant::now();
        }

        let packet = tokio::time::timeout(READ_TIMEOUT, client.read_interleaved()).await;
        let (channel, payload) = match packet {
            Ok(result) => result?,
            Err(_) => {
                return Err(crate::error::CoreError::rtsp(format!(
                    "no media received for {} s",
                    READ_TIMEOUT.as_secs()
                )))
            }
        };

        // Channel 0 carries RTP, channel 1 carries RTCP.
        if channel == 0 {
            if let Some(header) = parse_rtp_header(&payload) {
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
                        let started = Instant::now();
                        let decoded = decoder.decode(&unit.data, unit.pts_us(), unit.keyframe);
                        slowest_decode = slowest_decode.max(started.elapsed());
                        match decoded {
                            Ok(decoded) => {
                                window_decoded += decoded.len() as u64;
                                for frame in decoded {
                                    // A luma plane and an interleaved chroma
                                    // plane. A backend that hands over anything
                                    // else - a decoder rendering straight into a
                                    // GPU buffer, say - has nothing the renderer
                                    // can take yet.
                                    let mut planes = frame.planes.into_iter();
                                    let (Some(y), Some(uv)) = (planes.next(), planes.next()) else {
                                        continue;
                                    };
                                    width = Some(frame.width);
                                    height = Some(frame.height);
                                    sequence += 1;
                                    frames.publish(
                                        index,
                                        Arc::new(VideoFrame {
                                            sequence,
                                            width: frame.width,
                                            height: frame.height,
                                            y,
                                            uv,
                                        }),
                                    );
                                }
                            }
                            // One damaged picture is not worth a new session:
                            // the encoder recovers at its next IDR.
                            Err(err) => {
                                decode_errors += 1;
                                window_errors += 1;
                                // The first rejections carry the payload that
                                // caused them, which names the NAL units the
                                // decoder was given.
                                if decode_errors <= 3 {
                                    tracing::debug!(
                                        target: "xgview::pipeline",
                                        index,
                                        len = unit.data.len(),
                                        types = ?nal_types(&unit.data),
                                        keyframe = unit.keyframe,
                                        au = %hex(&unit.data, 24),
                                        cached = ?sets(&depacketizer),
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
                units = window_units,
                unit_bytes = window_unit_bytes,
                decoded = window_decoded,
                errors = window_errors,
                dropped = window_dropped,
                foreign = window_foreign,
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
            window_dropped = 0;
            window_foreign = 0;
            slowest_decode = Duration::ZERO;
        }
    }
}

/// Creates the decoder for a negotiated codec.
///
/// Only H.264 is wired to a backend today. An unsupported codec is not an
/// error: the session keeps running, reports its statistics and shows a
/// placeholder, which is what a `H265` or `MJPEG` camera should look like rather
/// than a connection that never stops reconnecting.
///
/// `prefer_hardware` only asks. A decoder that will not open is retried on the
/// CPU, because a preference must never cost a channel its picture.
fn h264_decoder(
    kind: Codec,
    width: Option<u32>,
    height: Option<u32>,
    index: usize,
    prefer_hardware: bool,
) -> Option<Box<dyn VideoDecoder>> {
    if kind != Codec::H264 {
        tracing::warn!(
            target: "xgview::pipeline",
            index,
            codec = kind.as_str(),
            "no decoder for this codec, the stream will not be displayed"
        );
        return None;
    }
    let config = DecoderConfig {
        codec: kind,
        width: width.unwrap_or(0),
        height: height.unwrap_or(0),
        surface: None,
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
    let mut decoder = monitor_codec::create_decoder();
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
    use crate::model::CameraOrigin;

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
            main_profile: None,
            sub_profile: None,
        }
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
