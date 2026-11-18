//! What every session shares: the sinks it publishes through, the window it
//! counts its numbers in, the decoder it opens, and the one H.264 declaration a
//! hardware decoder has no mode for.

use super::*;

use std::sync::OnceLock;

/// The client every request to Frigate's own port goes out on.
///
/// One for the process rather than one per request. A recording is asked for
/// again on every seek, and a client built per request starts from nothing: no
/// pooled connection and no TLS session to resume, so each scrub paid a
/// handshake before it could even ask. The certificate is Frigate's own - its
/// port serves a self-signed one - and that is the same answer for every
/// request, which is why it can be decided once, here.
///
/// No overall timeout: a window of a recording is meant to take minutes to play
/// out, and the response is read for as long as the viewer watches it. What
/// does need a deadline - a login, a handshake - sets one on its own request.
pub(super) fn frigate_http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("a client built from constants")
    })
}

/// Where a session puts what it produces: the events that mark a change, and
/// the two stores that keep the newest of what has no history worth keeping.
///
/// Carried as one argument because a session writes to these three and to
/// nothing else, and because three of them among nine positional parameters is
/// a signature read by counting.
#[derive(Clone, Copy)]
pub(super) struct Sinks<'a> {
    pub(super) events: &'a Sender<StreamEvent>,
    pub(super) frames: &'a FrameStore,
    pub(super) stats: &'a StatsStore,
    /// Set when the channel this session is running has been given to another
    /// one, or parked.
    ///
    /// `abort` cancels a task at its next await point and nowhere else, and a
    /// task inside a decoder call is not there for a long time - long enough for
    /// the session it was replaced by to give up and die, leaving the survivor
    /// running with nothing to publish to. So the session is asked to stop
    /// instead, and checks between packets: a few milliseconds, not a minute.
    pub(super) stop: &'a AtomicBool,
}

/// An error followed by every cause under it, outermost first.
///
/// `reqwest` reports the stage a request failed at and keeps the reason in the
/// source chain, so the stage on its own - "error decoding response body" -
/// says nothing about whether the peer closed, the transport reset or a
/// timeout fired.
pub(super) fn describe(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(" <- ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Publishes the pictures a decoder produced, in order, and reports the size of
/// the last one.
///
/// `None` means the decoder produced nothing the renderer can take: it needs a
/// luma plane and an interleaved chroma plane, and a backend that hands over
/// anything else - a decoder rendering straight into a GPU buffer, say - has
/// none yet.
pub(super) fn publish_frames(
    frames: &FrameStore,
    index: usize,
    sequence: &mut u64,
    decoded: Vec<DecodedFrame>,
    at: Option<f64>,
) -> Option<(u32, u32)> {
    let mut size = None;
    let mut undrawable = 0usize;
    // What one of them looked like, so the line says which shape the renderer
    // could not take rather than only how many there were.
    let mut shape: Option<(u32, u32, usize)> = None;
    for frame in decoded {
        let plane_count = frame.planes.len();
        let mut planes = frame.planes.into_iter();
        let (Some(y), Some(uv)) = (planes.next(), planes.next()) else {
            // The decoder produced a picture the renderer cannot take - a
            // backend that renders into a GPU buffer of its own has no planes.
            // Nothing is published, and the tile keeps whatever it had.
            undrawable += 1;
            if shape.is_none() {
                shape = Some((frame.width, frame.height, plane_count));
            }
            continue;
        };
        *sequence += 1;
        frames.publish(
            index,
            Arc::new(VideoFrame {
                sequence: *sequence,
                at,
                width: frame.width,
                height: frame.height,
                colorspace: frame.colorspace,
                y,
                uv,
            }),
        );
        size = Some((frame.width, frame.height));
    }
    if undrawable > 0 {
        tracing::debug!(
            target: "xgview::pipeline",
            index,
            pictures = undrawable,
            shape = ?shape,
            "pictures that cannot be drawn"
        );
    }
    size
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
pub(super) fn video_decoder(
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
pub(super) fn open_decoder(config: &DecoderConfig, index: usize) -> Option<Box<dyn VideoDecoder>> {
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
pub(super) fn uses_hardware(decoder: &Option<Box<dyn VideoDecoder>>) -> bool {
    decoder.as_ref().and_then(|decoder| decoder.info()).is_some_and(|info| info.hardware)
}

/// The counters of one statistics window.
///
/// A live view that freezes and then runs to catch up is either a starving
/// network or a lagging decoder, and only these numbers tell the two apart -
/// which is why they are counted per window rather than over the session.
///
/// Every session publishes the same sample - a rate, a bitrate and the pictures
/// since it started - but each counts what its own protocol can lose. An RTSP
/// session knows about access units it could not assemble and packets that
/// arrived twice; a JPEG stream has no sequence number to miss; an MSE one is a
/// sequence of fragments with no packets at all. What a session does not count
/// stays zero, and its own log line is the one that does not print it.
#[derive(Debug, Default)]
pub(super) struct WindowCounters {
    /// Pictures the window closed.
    pub(super) frames: u64,
    /// Bytes the network delivered.
    pub(super) bytes: u64,
    /// Access units assembled, and their bytes. Compared against `bytes` the two
    /// say whether the pictures arrive whole: assembled bytes far below what
    /// arrived mean fragments are being thrown away.
    pub(super) units: u64,
    pub(super) unit_bytes: u64,
    /// Pictures the decoder produced, and the ones it refused.
    ///
    /// Counted against `frames`, which is what arrived: the two differing means
    /// the decoder or the renderer is where the stream is being lost, not the
    /// link.
    pub(super) decoded: u64,
    pub(super) errors: u64,
    /// Decode calls that put no pixels in the renderer's hands.
    ///
    /// A decoder scanning into a surface of its own is the ordinary case, and
    /// such a call is one of these too: it hands its picture over by rendering
    /// it. What separates the two is `undrawn_frames`, which counts the
    /// pictures behind those calls only when there were some - so a climb in
    /// this with that one still at zero is a surface, and a climb in both is
    /// pictures that were decoded and then went nowhere.
    ///
    /// Not the complement of `drawn`, and not a count of handovers: a handover
    /// can be both, when the decode that produced nothing is the one during
    /// which a picture held from before went to the wall. Each counts its own
    /// side of that.
    pub(super) undrawn_calls: u64,
    /// The pictures behind those calls, which tells an empty decode from a
    /// decode whose output had no planes to draw.
    pub(super) undrawn_frames: u64,
    /// Calls that did put a picture in the renderer's hands.
    pub(super) drawn: u64,
    /// Refusals before the session had shown a picture, which are its opening
    /// rather than damage - see `on_video` in `rtsp_session`.
    pub(super) opening: u64,
    /// Markers that assembled no access unit, and packets that do not carry the
    /// negotiated payload type. A channel whose pictures never assemble is
    /// either fragmenting them in a way the depacketizer does not recognise or
    /// sending them under a second payload type, and the two are
    /// indistinguishable without these counters.
    pub(super) dropped: u64,
    pub(super) foreign: u64,
    /// Duplicate or late packets, and the ones the sequence numbers say never
    /// arrived. Only a UDP session has either, so both stay zero on TCP.
    pub(super) reordered: u64,
    pub(super) lost: u64,
    /// The slowest single decode of the window.
    pub(super) slowest: Duration,
}

impl WindowCounters {
    /// Keeps the slowest decode the window has seen.
    pub(super) fn decode_took(&mut self, took: Duration) {
        self.slowest = self.slowest.max(took);
    }

    /// Pictures per second over `seconds`.
    pub(super) fn fps(&self, seconds: f32) -> f32 {
        self.frames as f32 / seconds
    }

    /// Kilobits per second over `seconds`.
    pub(super) fn kbps(&self, seconds: f32) -> f32 {
        (self.bytes as f32 * 8.0 / 1000.0) / seconds
    }
}

/// One statistics window: what has been counted since the last sample, and when
/// the next one is due.
///
/// The clock and the arithmetic are the whole of this type - every session
/// publishes the same sample by the same sum - while each logs its own counters
/// under its own names, because what an RTSP session can lose and what a JPEG
/// one can are not the same fault.
#[derive(Debug)]
pub(super) struct StatsWindow {
    pub(super) counters: WindowCounters,
    started: Instant,
}

impl StatsWindow {
    pub(super) fn new() -> Self {
        Self { counters: WindowCounters::default(), started: Instant::now() }
    }

    /// Whether a sample is due.
    pub(super) fn due(&self) -> bool {
        self.started.elapsed() >= STATS_INTERVAL
    }

    /// How long the window that is closing lasted, floored so that a sample
    /// taken right after the previous one cannot divide by zero.
    pub(super) fn seconds(&self) -> f32 {
        self.started.elapsed().as_secs_f32().max(0.001)
    }

    /// Ends the window: hands back what it counted and starts the next one.
    ///
    /// The counters come back rather than being read first, so that the sample
    /// published and the line logged describe the same window - and so that the
    /// reset cannot be forgotten, which it could be in three places at once.
    pub(super) fn close(&mut self) -> WindowCounters {
        self.started = Instant::now();
        std::mem::take(&mut self.counters)
    }
}

/// Says, once per session, that pictures are arriving.
///
/// Every session reported the same transition and each wrote it out - the flag,
/// the event and the sentence. The flag is what keeps it to one; a session that
/// recovers from a stall says so again from where it noticed, which is why this
/// is called where a picture has just been produced rather than from a state
/// machine.
pub(super) fn announce_streaming(
    events: &Sender<StreamEvent>,
    index: usize,
    camera_id: &str,
    stream: StreamKind,
    announced: &mut bool,
) {
    if *announced {
        return;
    }
    *announced = true;
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

pub(super) fn emit(events: &Sender<StreamEvent>, event: StreamEvent) {
    // A closed receiver simply means the UI has shut down.
    let _ = events.send(event);
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
pub(super) struct BaselineFix {
    decided: bool,
    apply: bool,
    /// Whether a set may be rewritten past the baseline flag at all.
    ///
    /// Only where a wrong answer can be taken back. The level and the reorder
    /// declaration both change what a decoder is configured with, and a decoder
    /// keeps that for as long as it lives: leaving a set alone is the only way
    /// back. A live session can reopen its decoder on the set the camera sends
    /// (see [`Self::retract`]); a recording and the MSE path cannot, because
    /// what they feed after the first parameter sets carries none, so a decoder
    /// reopened there would have nothing to configure from. They get the
    /// baseline flag and nothing else - narrow, and the direction that cannot
    /// make a stream worse.
    rewrite: bool,
    /// The level this stream declares and the one its pictures need, when the
    /// declaration is far above it. See [`sps::overstated_level`].
    level: Option<(u8, u8)>,
    /// Whether to declare that this stream has nothing to reorder.
    ///
    /// Taken for a stream that carries no VUI: a set that says nothing leaves
    /// the decoder to its own default, and its default is to hold pictures -
    /// four or five on the hardware in `priv/docs/foscam_probe.md` §12 - before
    /// releasing the first. The main stream of that doorbell is the case a
    /// level-based gate missed: 2560x1920 declared at level 5.1 is an honest
    /// declaration, and the pictures still arrived in bursts of four.
    ///
    /// A set that carries a VUI is left to say what it wants, and a stream that
    /// turns out to reorder takes the declaration back - see [`Self::retract`].
    no_reorder: bool,
    /// Whether a stream that reordered after the declaration was made has had it
    /// taken back, so the log says it once.
    retracted: bool,
}

impl BaselineFix {
    /// For a session whose decoder can be reopened mid-stream: a live one, which
    /// the next access unit's own parameter sets configure.
    pub(super) fn new_live() -> Self {
        Self { rewrite: true, ..Self::new() }
    }

    /// For a session whose decoder cannot: a recording and the MSE path both
    /// feed samples without parameter sets after the first.
    pub(super) fn new() -> Self {
        Self {
            decided: false,
            apply: false,
            rewrite: false,
            level: None,
            no_reorder: false,
            retracted: false,
        }
    }

    /// Stops declaring that this stream has nothing to reorder, and answers
    /// whether it did - the caller uses that to reopen the decoder, which is
    /// the only way the declaration leaves one that was configured from it.
    ///
    /// Called when the decoder reports that the stream hands its pictures over
    /// out of order, which is the one thing the declaration would be wrong
    /// about: a stream that reorders must be allowed to. The set the next
    /// access unit carries is then left as the camera sent it, and the decoder
    /// the session reopens for itself is configured from that one - see
    /// `AMediaCodecDecoder::restart`.
    pub(super) fn retract(&mut self, index: usize) -> bool {
        if !self.no_reorder || self.retracted {
            return false;
        }
        self.no_reorder = false;
        self.retracted = true;
        tracing::info!(
            target: "xgview::pipeline",
            index,
            "the stream reorders its pictures, so the declaration that it does \
             not is taken back"
        );
        true
    }

    /// Rewrites `data` when the stream needs it, and says whether it changed
    /// anything. Called with every access unit, so the decision is taken from
    /// the first set that arrives.
    pub(super) fn observe(&mut self, index: usize, codec: Codec, data: &mut Vec<u8>) -> bool {
        if codec == Codec::H264 && !self.decided {
            if let Some(nal) = sps::access_unit_sps(data) {
                self.apply = sps::needs_constrained_baseline(nal);
                self.level = self.rewrite.then(|| sps::overstated_level(nal)).flatten();
                // The gate is the set's own silence, not its level: a camera
                // that declares a level its pictures fit in still says nothing
                // about reordering, and that is the field the decoder fills in
                // with a buffer of its own.
                self.no_reorder = self.rewrite && sps::has_vui(nal) == Some(false);
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
                if let Some((declared, needed)) = self.level {
                    tracing::info!(
                        target: "xgview::pipeline",
                        index,
                        declared,
                        needed,
                        "relabelling a stream that declares a level far above its \
                         pictures, which is what makes a decoder hold them back"
                    );
                }
                if self.no_reorder {
                    tracing::info!(
                        target: "xgview::pipeline",
                        index,
                        "declaring that this stream has nothing to reorder, which \
                         its own parameter set left unsaid"
                    );
                }
            }
        }
        // Both rewrites re-read the set they are given and decide again, rather
        // than applying the answer taken above to whatever set arrives later: a
        // session whose camera changed its pictures mid-stream would otherwise
        // have the new set brought down to the old one's level, and a camera
        // that started sending B-frames would have them reordered away. `level`
        // and `no_reorder` only say whether this stream is one worth walking at
        // all, and what to call it in the log.
        let mut changed = self.apply && sps::constrain_baseline(data) > 0;
        if self.level.is_some() {
            changed |= sps::relabel_level(data) > 0;
        }
        if self.no_reorder {
            changed |= sps::declare_no_reorder_in_unit(data) > 0;
        }
        changed
    }
}
