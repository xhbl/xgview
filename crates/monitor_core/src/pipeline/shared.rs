//! What every session shares: the sinks it publishes through, the window it
//! counts its numbers in, the decoder it opens, and the one H.264 declaration a
//! hardware decoder has no mode for.

use super::*;

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
    /// Refusals before the session had shown a picture, which are its opening
    /// rather than damage - see `run_session`.
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
}

impl BaselineFix {
    pub(super) fn new() -> Self {
        Self { decided: false, apply: false }
    }

    /// Rewrites `data` in place when the stream needs it, and says whether it
    /// changed anything. Called with every access unit - and with the MSE init
    /// segment's parameter sets - so the decision is taken from the first set
    /// that arrives, whichever form the session is in.
    pub(super) fn observe(&mut self, index: usize, codec: Codec, data: &mut [u8]) -> bool {
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
