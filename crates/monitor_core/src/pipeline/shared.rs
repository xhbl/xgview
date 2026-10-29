//! What every session shares: where it publishes, what it opens to decode,
//! and the one H.264 declaration a hardware decoder has no mode for.

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
