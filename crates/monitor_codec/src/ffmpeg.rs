//! In process decoder built on FFmpeg's `libavcodec`.
//!
//! The backend is a thin shell around `avcodec_send_packet` /
//! `avcodec_receive_frame`. The access units the pipeline assembles are already
//! Annex-B, which is the packet form `libavcodec` expects, so no demuxer takes
//! part: a picture arrives as one packet and leaves as at most one frame.
//!
//! ```text
//! Annex-B access unit  ->  libavcodec  ->  libswscale  ->  NV12 DecodedFrame
//!                            (or the gpu)
//! ```
//!
//! Decoding runs on the GPU when the caller asks for it and the platform offers
//! a device: CUDA and VAAPI on Linux, Direct3D 11 and CUDA on Windows, tried in
//! that order and falling through to the one below on a machine that has no
//! driver for the one above. Either way the pictures are copied back into system
//! memory afterwards, which is the form the renderer takes. The copy is what the
//! simpler path costs; the decoding itself, which is the part that costs, is off
//! the CPU. A machine where no device opens, and a stream the chosen decoder is
//! offered no hardware format for, both leave the software decoder in place: the
//! preference is never allowed to cost a picture.
//!
//! The conversion goes through `libswscale` rather than a hand written loop
//! because it reads the range the stream announced, so a limited range camera
//! and a full range one both end up as the limited range planes the renderer
//! expects. The colour matrix itself is not applied here: the renderer runs it
//! on the GPU, once per picture and separately from the CPU work.
//!
//! Latency is bounded on purpose. The default configuration holds several
//! pictures back for frame threading and for reordering, which a live view feels
//! as a delay of a few frames, so the decoder is opened single threaded and
//! without reordering.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Once;
use std::time::{Duration, Instant};

// The crate is published as `ffmpeg-next`, and the module carrying this backend
// is already called `ffmpeg`, so the crate is brought in under a plain alias.
use ffmpeg_next as ffmpeg;

use ffmpeg::codec::context::Context as CodecContext;
use ffmpeg::codec::decoder::video::Video as DecoderContext;
use ffmpeg::codec::id::Id;
use ffmpeg::software::scaling::context::Context as Scaler;
use ffmpeg::software::scaling::flag::Flags as ScalerFlags;
use ffmpeg::util::format::pixel::Pixel;
use ffmpeg::util::frame::video::Video as Picture;
use ffmpeg::Packet;

use crate::{
    plane_stride, Codec, CodecError, ColorMatrix, ColorRange, ColorSpace, DecodedFrame,
    DecoderConfig, PixelFormat, Result, VideoDecoder, VideoStreamInfo,
};

/// Decoder backed by FFmpeg's `libavcodec`.
pub struct FfmpegDecoder {
    /// Codec the session negotiated, reported through [`VideoDecoder::info`].
    codec: Codec,
    /// Opened by [`VideoDecoder::configure`], which needs the negotiated codec.
    decoder: Option<DecoderContext>,
    /// Converts whatever pixel format the decoder produces into the RGBA the
    /// renderer takes. It is bound to the geometry it was built for, so it is
    /// rebuilt whenever the stream changes size or format.
    scaler: Option<Scaler>,
    scaler_key: Option<(Pixel, u32, u32)>,
    info: Option<VideoStreamInfo>,
    /// Timestamp of the last access unit, reused when the decoder is drained.
    last_pts_us: i64,
    frames: u64,
    /// The device this decoder was handed, when one opened.
    device: Option<&'static Device>,
    /// Set once a picture comes back in that device's format, which is the only
    /// proof the GPU is doing the work: whether the device is accepted is the
    /// decoder's decision, and it is only known at the first picture.
    hardware: bool,
    /// Where the time of the current window went; see
    /// [`FfmpegDecoder::report_transfer_cost`].
    cost: TransferCost,
    last_report: Instant,
}

/// Time spent in each step that turns a decoded picture into planes, summed
/// over the pictures of the current report.
///
/// The steps are the ones standing between the decoder and the renderer, and
/// only the first is the decoder's own work: `copy_back` brings a hardware
/// picture back into system memory, `convert` runs libswscale when the picture
/// is not already NV12, and `pack` walks the planes into the padded layout the
/// upload wants. Which of them costs is what decides whether removing the
/// copies is worth it at all.
#[derive(Debug, Default)]
struct TransferCost {
    pictures: u64,
    copy_back_us: u64,
    convert_us: u64,
    pack_us: u64,
}

/// How often the transfer cost is reported.
const TRANSFER_REPORT: Duration = Duration::from_secs(2);

// Safety: a decoder is owned outright by the channel worker that created it and
// is never touched from two threads at once; the renderer only ever sees the
// finished frames. The contexts are plain heap state, so moving them between
// threads is sound, and the raw pointers inside them are the only reason they
// are not `Send` on their own.
unsafe impl Send for FfmpegDecoder {}

impl Default for FfmpegDecoder {
    fn default() -> Self {
        Self {
            codec: Codec::H264,
            decoder: None,
            scaler: None,
            scaler_key: None,
            info: None,
            last_pts_us: 0,
            frames: 0,
            device: None,
            hardware: false,
            cost: TransferCost::default(),
            last_report: Instant::now(),
        }
    }
}

impl FfmpegDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pictures produced so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Pulls every picture the decoder is holding right now.
    ///
    /// `receive_frame` answers `EAGAIN` while the decoder still needs input and
    /// `Eof` once it has been drained, and both end the loop rather than
    /// counting as a failure: an access unit that carries no picture is normal,
    /// the pictures before the first key frame being the obvious example.
    fn collect(&mut self, pts_us: i64, keyframe: bool) -> Result<Vec<DecodedFrame>> {
        let codec = self.codec;
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(Vec::new());
        };

        let mut frames = Vec::new();
        let mut picture = Picture::empty();
        let mut copied_back = Picture::empty();
        let mut converted = Picture::empty();
        while decoder.receive_frame(&mut picture).is_ok() {
            // A picture the decoder left on the GPU has to be brought back into
            // system memory before anything can read it, and every device hands
            // it over as NV12 - the very layout the renderer takes. Which format
            // counts as "on the GPU" is read off the picture rather than assumed
            // from the request: accepting the device is the decoder's decision,
            // and this is where it shows.
            let copy_started = Instant::now();
            let source = match self.device {
                Some(device) if picture.format() == device.pixel => {
                    copy_back(&mut copied_back, &picture)?;
                    self.hardware = true;
                    &copied_back
                }
                _ => &picture,
            };
            self.cost.copy_back_us += copy_started.elapsed().as_micros() as u64;

            let convert_started = Instant::now();
            let ready = to_nv12(&mut self.scaler, &mut self.scaler_key, source, &mut converted)?;
            self.cost.convert_us += convert_started.elapsed().as_micros() as u64;
            // The matrix is the one the stream announced, and stays the stream's
            // whatever happens here. The range does not: a picture that went
            // through libswscale came out in the studio range the NV12 planes
            // carry, so only that case can change it.
            let colorspace = ColorSpace {
                matrix: color_matrix(picture.color_space()),
                range: if std::ptr::eq(ready, source) {
                    color_range(picture.color_range())
                } else {
                    ColorRange::Limited
                },
            };
            let pack_started = Instant::now();
            let Some(frame) = pack(ready, pts_us, keyframe, colorspace) else {
                continue;
            };
            self.cost.pack_us += pack_started.elapsed().as_micros() as u64;
            self.cost.pictures += 1;
            if self.info.is_none() {
                self.info = Some(VideoStreamInfo {
                    codec,
                    width: frame.width,
                    height: frame.height,
                    fps: None,
                    hardware: self.hardware,
                });
            }
            self.frames += 1;
            frames.push(frame);
        }
        self.report_transfer_cost();
        Ok(frames)
    }

    /// Reports where the cost of a decoded picture went, every few seconds.
    ///
    /// The three steps sit between the decoder and the renderer and the report
    /// says which of them costs. Only the first is the decoder's own work, and
    /// whether the copies are worth removing is a question this answers rather
    /// than a matter of opinion.
    fn report_transfer_cost(&mut self) {
        if self.cost.pictures == 0 || self.last_report.elapsed() < TRANSFER_REPORT {
            return;
        }
        let pictures = self.cost.pictures as f64;
        let per_picture = |micros: u64| micros as f64 / pictures / 1000.0;
        tracing::debug!(
            target: "xgview::codec",
            pictures = self.cost.pictures,
            copy_back_ms = per_picture(self.cost.copy_back_us),
            convert_ms = per_picture(self.cost.convert_us),
            pack_ms = per_picture(self.cost.pack_us),
            "where a picture's cost goes"
        );
        self.cost = TransferCost::default();
        self.last_report = Instant::now();
    }
}

/// The argument list of a `printf` style call, which the platforms spell
/// differently: a plain pointer on Windows, and a pointer to the tag structure
/// of the System V calling convention on Linux.
#[cfg(windows)]
type VaList = *mut c_char;
#[cfg(not(windows))]
type VaList = *mut ffmpeg::ffi::__va_list_tag;

/// Longest libavcodec message kept, in bytes.
const LOG_LINE: usize = 1024;

/// Takes libavcodec's own messages and hands them to `tracing`.
///
/// Left to itself the library writes to `stderr`: no timestamp, no target, and
/// no way to filter the lines out. It also reports at error level conditions a
/// live camera produces routinely - a picture that cannot be decoded, a stream
/// joined between two key frames - and one camera here spends its first second
/// waiting for a key frame, which put nearly two hundred unformatted lines on
/// the console for a single session. They are passed on at debug level instead,
/// because the pipeline already counts them and warns once about the condition
/// behind them; `RUST_LOG=xgview=debug` brings back the detail for the moment it
/// is actually worth reading.
///
/// Safety: libavcodec calls this from whichever thread logged, with the format
/// string and the argument list of a `printf` call. Nothing in it may unwind
/// into C, so nothing in it panics: a message that cannot be read is dropped.
unsafe extern "C" fn forward_log(
    _context: *mut c_void,
    level: c_int,
    format: *const c_char,
    arguments: VaList,
) {
    if format.is_null() {
        return;
    }
    let mut line = [0 as c_char; LOG_LINE];
    let mut prefix = 0;
    let written = ffmpeg::ffi::av_log_format_line2(
        std::ptr::null_mut(),
        level,
        format,
        arguments,
        line.as_mut_ptr(),
        LOG_LINE as c_int,
        &mut prefix,
    );
    if written <= 0 {
        return;
    }
    let message = CStr::from_ptr(line.as_ptr()).to_string_lossy();
    let message = message.trim_end();
    // The levels are mapped onto ours so that libavcodec's own diagnoses keep
    // their weight. Its ERROR is the only place some failures are ever named -
    // a hardware decoder that will not set up says so here and nowhere else -
    // and a level below `warn` is a level a viewer never reads.
    match level {
        ffmpeg::ffi::AV_LOG_PANIC | ffmpeg::ffi::AV_LOG_FATAL => {
            tracing::error!(target: "xgview::codec", "{message}")
        }
        ffmpeg::ffi::AV_LOG_ERROR => tracing::warn!(target: "xgview::codec", "{message}"),
        ffmpeg::ffi::AV_LOG_WARNING => tracing::info!(target: "xgview::codec", "{message}"),
        // INFO and VERBOSE carry the setup detail - which adapter, which
        // decoder, what the driver answered - and are what a decoder that
        // misbehaves is diagnosed with.
        ffmpeg::ffi::AV_LOG_INFO | ffmpeg::ffi::AV_LOG_VERBOSE => {
            tracing::debug!(target: "xgview::codec", "{message}")
        }
        _ => tracing::trace!(target: "xgview::codec", "{message}"),
    }
}

/// How much of libavcodec's own logging is kept, asked for in
/// `XGVIEW_FFMPEG_LEVEL`.
///
/// `info` - the default - is where the library names a failure it cannot
/// handle. `verbose` and `debug` add the setup detail a decoder that will not
/// take the hardware path is diagnosed with: which adapter was chosen, what the
/// driver answered, which pixel format was refused. They cost a line or two per
/// decoder rather than per frame, but they are not what a healthy machine needs
/// in its log, so they are asked for rather than always on.
fn log_level() -> c_int {
    match std::env::var("XGVIEW_FFMPEG_LEVEL").unwrap_or_default().to_ascii_lowercase().as_str() {
        "trace" => ffmpeg::ffi::AV_LOG_TRACE,
        "debug" => ffmpeg::ffi::AV_LOG_DEBUG,
        "verbose" => ffmpeg::ffi::AV_LOG_VERBOSE,
        "warning" => ffmpeg::ffi::AV_LOG_WARNING,
        "error" => ffmpeg::ffi::AV_LOG_ERROR,
        _ => ffmpeg::ffi::AV_LOG_INFO,
    }
}

/// Initialises the FFmpeg runtime.
///
/// Every channel calls this and only the first call does anything, but the
/// initialisation itself is not something to run twice from two threads at once.
fn init() -> Result<()> {
    static INIT: Once = Once::new();
    let mut failure = None;
    INIT.call_once(|| {
        if let Err(err) = ffmpeg::init() {
            failure = Some(err.to_string());
            return;
        }
        // Safety: both are plain global settings of the library, and this runs
        // once, before any decoder exists. The callback is installed rather than
        // left at the default because the default writes to `stderr`; the level
        // is what bounds how much reaches it.
        unsafe {
            ffmpeg::ffi::av_log_set_level(log_level());
            ffmpeg::ffi::av_log_set_callback(Some(forward_log));
        }
    });
    match failure {
        Some(message) => Err(CodecError::Configure(format!("ffmpeg: {message}"))),
        None => Ok(()),
    }
}

/// That picture in the layout the renderer takes.
///
/// A decoder that already produces NV12 - which a hardware decoder does once its
/// picture has been read back - is handed on untouched. Everything else is
/// converted, the converter being rebuilt whenever the geometry or the format
/// changes because it is bound to both.
///
/// The conversion goes through `libswscale` rather than a hand written loop
/// because it reads the range the stream announced, so a limited range camera
/// and a full range one both end up as the limited range planes the renderer
/// expects. The colour matrix is deliberately not applied here: the renderer
/// runs it on the GPU, once per picture and separately from the CPU work.
fn to_nv12<'a>(
    scaler: &mut Option<Scaler>,
    key: &mut Option<(Pixel, u32, u32)>,
    picture: &'a Picture,
    converted: &'a mut Picture,
) -> Result<&'a Picture> {
    if picture.format() == Pixel::NV12 {
        return Ok(picture);
    }
    let wanted = (picture.format(), picture.width(), picture.height());
    if *key != Some(wanted) {
        let (format, width, height) = wanted;
        tracing::debug!(
            target: "xgview::codec",
            ?format,
            width,
            height,
            "building the ffmpeg plane converter"
        );
        *scaler = Some(
            Scaler::get(format, width, height, Pixel::NV12, width, height, ScalerFlags::BILINEAR)
                .map_err(|err| CodecError::Decode(format!("ffmpeg converter: {err}")))?,
        );
        *key = Some(wanted);
    }
    scaler
        .as_mut()
        .expect("the converter was just built")
        .run(picture, converted)
        .map_err(|err| CodecError::Decode(format!("ffmpeg converter: {err}")))?;
    Ok(converted)
}

/// One way of decoding on the GPU.
struct Device {
    /// What libavcodec is asked for before the decoder is opened.
    kind: ffmpeg::ffi::AVHWDeviceType,
    /// The pixel format that device hands its pictures over in, as the format
    /// negotiation callback names it.
    picture: ffmpeg::ffi::AVPixelFormat,
    /// The same format in the wrapper's vocabulary rather than the raw one, for
    /// recognising it on a picture that came back from the decoder.
    pixel: Pixel,
    /// What the decoder calls itself while this device is the one in use.
    name: &'static str,
}

#[cfg(windows)]
const D3D11VA: Device = Device {
    kind: ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
    picture: ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
    pixel: Pixel::D3D11,
    name: "ffmpeg + d3d11va",
};
#[cfg(any(windows, target_os = "linux"))]
const CUDA: Device = Device {
    kind: ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
    picture: ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
    pixel: Pixel::CUDA,
    name: "ffmpeg + cuda",
};
#[cfg(target_os = "linux")]
const VAAPI: Device = Device {
    kind: ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
    picture: ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
    pixel: Pixel::VAAPI,
    name: "ffmpeg + vaapi",
};

/// The devices to try, best first.
///
/// The order is a preference between devices and not a requirement on any of
/// them: they are opened in turn, and the first that opens is used. A machine
/// without the top choice - no NVIDIA driver, no Direct3D 11 - decodes on the
/// one below it, and a machine with none of them decodes on the CPU.
///
/// NVIDIA leads on Linux because VAAPI is a second hand path there: the vendor
/// driver does not implement it, and reaching it takes a bridging package. On
/// Windows Direct3D 11 leads instead. It serves every vendor including NVIDIA,
/// drives the same decoding hardware and asks for nothing beyond the graphics
/// driver, so CUDA has nothing to win by standing in front of it.
#[cfg(windows)]
const DEVICES: &[Device] = &[D3D11VA, CUDA];
#[cfg(target_os = "linux")]
const DEVICES: &[Device] = &[CUDA, VAAPI];
#[cfg(not(any(windows, target_os = "linux")))]
const DEVICES: &[Device] = &[];

/// Opens the first device of [`DEVICES`] that will have us.
///
/// `None` means none of them opened, which is not a failure: the software
/// decoder takes the stream and the picture keeps flowing. Asking for hardware
/// is a preference, and a machine that cannot honour it is not broken.
///
/// The device name is left to the platform: a null name asks Direct3D for its
/// default adapter, CUDA for the primary device, and VAAPI for the first DRM
/// render node it can open.
fn open_device(wanted: bool) -> Option<(&'static Device, *mut ffmpeg::ffi::AVBufferRef)> {
    if !wanted {
        return None;
    }
    // Kept for the warning below: which device failed, and with what, is the
    // whole of the answer when no device opens.
    let mut last: Option<(&'static Device, std::ffi::c_int)> = None;
    for device in DEVICES {
        let mut handle: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
        // Safety: the call only writes the out parameter, and a null device
        // name asks for whichever device the type resolves to by default.
        let code = unsafe {
            ffmpeg::ffi::av_hwdevice_ctx_create(
                &mut handle,
                device.kind,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if code >= 0 && !handle.is_null() {
            tracing::debug!(
                target: "xgview::codec",
                device = device.name,
                "device opened for the hardware decoder"
            );
            return Some((device, handle));
        }
        // A missing driver, or missing hardware. That is ordinary on a machine
        // that has neither, so only the whole list failing is worth warning
        // about: a machine with one device out of two is not a problem to
        // report, it is the reason for the list.
        tracing::debug!(
            target: "xgview::codec",
            device = device.name,
            code,
            "device turned the hardware decoder down"
        );
        last = Some((device, code));
    }
    if let Some((device, code)) = last {
        tracing::warn!(
            target: "xgview::codec",
            device = device.name,
            code,
            "no device for the hardware decoder, decoding on the cpu"
        );
    }
    None
}

/// Picks the format a decoder outputs.
///
/// The decoder lists what it can produce, most preferred first. A hardware
/// format is taken when one is on offer, and the decoder's own first choice
/// otherwise, so that a stream libavcodec will not decode on the GPU still
/// decodes on the CPU rather than failing the open.
unsafe extern "C" fn prefer_hardware(
    _context: *mut ffmpeg::ffi::AVCodecContext,
    formats: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    let mut index = 0;
    loop {
        let format = *formats.add(index);
        if format == ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            break;
        }
        // A decoder only offers the formats the device it was given can
        // produce, so whichever of ours appears on the list is the one to take.
        if DEVICES.iter().any(|device| device.picture == format) {
            return format;
        }
        index += 1;
    }
    *formats
}

/// Copies a picture the decoder left on the GPU back into system memory.
///
/// The destination is an empty frame: the transfer fills in its size and its
/// pixel format from the source, and both Direct3D 11 and VAAPI hand it over as
/// NV12.
fn copy_back(destination: &mut Picture, source: &Picture) -> Result<()> {
    // Safety: both are live frames owned by this call, and this is the
    // documented way to bring a hardware picture back to the CPU.
    let code = unsafe {
        ffmpeg::ffi::av_hwframe_transfer_data(destination.as_mut_ptr(), source.as_ptr(), 0)
    };
    if code < 0 {
        return Err(CodecError::Decode(format!(
            "ffmpeg: cannot read the picture back from the gpu ({code})"
        )));
    }
    Ok(())
}

/// Copies the two planes of an NV12 picture out of their strided buffers.
fn pack(
    nv12: &Picture,
    pts_us: i64,
    keyframe: bool,
    colorspace: ColorSpace,
) -> Option<DecodedFrame> {
    let width = nv12.width() as usize;
    let height = nv12.height() as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let luma = copy_plane(nv12, 0, width, height)?;
    // The interleaved chroma plane spends one byte on U and one on V for every
    // two luma samples, so its rows are as long as the luma rows and there are
    // half as many of them.
    let chroma = copy_plane(nv12, 1, width, height.div_ceil(2))?;
    Some(DecodedFrame {
        width: width as u32,
        height: height as u32,
        format: PixelFormat::Nv12,
        colorspace,
        pts_us,
        keyframe,
        buffer: None,
        planes: vec![luma, chroma],
    })
}

/// The matrix coefficients libavcodec reported for a picture.
///
/// The standard family does not matter to the shader, only the numbers do, so
/// the ones that share a matrix are grouped. Anything unrecognised takes the
/// renderer's default, which is what the picture would have been decoded with
/// before the stream could be asked.
fn color_matrix(space: ffmpeg::color::Space) -> ColorMatrix {
    use ffmpeg::color::Space;
    match space {
        Space::BT470BG | Space::SMPTE170M | Space::SMPTE240M | Space::FCC => ColorMatrix::Bt601,
        Space::BT2020NCL | Space::BT2020CL => ColorMatrix::Bt2020,
        _ => ColorMatrix::Bt709,
    }
}

/// The range libavcodec reported for a picture.
fn color_range(range: ffmpeg::color::Range) -> ColorRange {
    match range {
        ffmpeg::color::Range::JPEG => ColorRange::Full,
        _ => ColorRange::Limited,
    }
}

/// Copies the leading `columns` bytes of `rows` rows out of one strided plane,
/// laying them out as the renderer takes them.
///
/// A decoder aligns every row to a stride of its own, usually wider than the
/// picture, so the rows are copied one at a time. The destination pads in turn:
/// one walk over the picture writes the layout the upload wants, which is one
/// walk fewer than writing it twice.
fn copy_plane(frame: &Picture, plane: usize, columns: usize, rows: usize) -> Option<Vec<u8>> {
    let stride = frame.stride(plane);
    let data = frame.data(plane);
    let padded = plane_stride(columns);
    let mut buffer = vec![0u8; padded * rows];
    for row in 0..rows {
        let start = row * stride;
        let end = row * padded + columns;
        buffer[row * padded..end].copy_from_slice(data.get(start..start + columns)?);
    }
    Some(buffer)
}

impl VideoDecoder for FfmpegDecoder {
    fn name(&self) -> &'static str {
        // Named after the device this decoder was given rather than the one the
        // caller asked for: a decoder that fell back is a software one, whatever
        // the configuration says. Whether pictures really come off it is a
        // separate question, and `hardware` is where the answer is kept.
        self.device.map_or("ffmpeg", |device| device.name)
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        init()?;
        let id = match config.codec {
            Codec::H264 => Id::H264,
            Codec::H265 => Id::HEVC,
            Codec::Mjpeg => Id::MJPEG,
            Codec::Unknown => {
                return Err(CodecError::Unsupported(format!(
                    "ffmpeg has no decoder for {}",
                    config.codec
                )))
            }
        };
        let codec = ffmpeg::codec::decoder::find(id)
            .ok_or_else(|| CodecError::Unsupported(format!("no ffmpeg decoder for {}", config.codec)))?;
        let mut context = CodecContext::new_with_codec(codec);

        // The size the SDP guessed only sizes the decoder's buffers up front;
        // the stream's own parameter sets correct it on the first key frame.
        if config.width > 0 && config.height > 0 {
            // Safety: `as_mut_ptr` hands out the decoder context the wrapper
            // owns and has not opened yet, so nothing else can observe it.
            unsafe {
                let raw = context.as_mut_ptr();
                (*raw).width = config.width as i32;
                (*raw).height = config.height as i32;
            }
        }

        // One thread and no reordering, so a picture leaves in the order it
        // arrived and without waiting for the ones behind it.
        // Safety: same context, still unopened, same reasoning as above.
        unsafe {
            let raw = context.as_mut_ptr();
            (*raw).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*raw).thread_count = 1;
            (*raw).thread_type = 0;
        }

        // A hardware decoder needs its device, and its own say over which format
        // it may output, in place before the context is opened: that call is
        // where libavcodec chooses between the software and the hardware
        // implementation of the codec.
        let device = open_device(config.hardware);
        if let Some((_device, handle)) = device {
            // Safety: the context is still unopened, so nothing else can observe
            // it. The reference is handed over to the context, which releases it
            // when it is freed.
            unsafe {
                let raw = context.as_mut_ptr();
                (*raw).hw_device_ctx = handle;
                (*raw).get_format = Some(prefer_hardware);
            }
        }

        // Three outcomes have to be told apart here, because they mean three
        // different things about the machine and only one of them is a limit
        // nobody can lift:
        //
        // * no device at all - `open_device` has already said so, and the codec
        //   opens on the CPU;
        // * a device, and the codec opens: the GPU is decoding;
        // * a device, and the codec refuses. That refusal is the driver
        //   declining a session - its count of concurrent decoders is full, or
        //   it will not decode this stream - and it is worth naming, because
        //   "cannot configure the decoder" on its own reads like a bad stream.
        let decoder = match context.decoder().video() {
            Ok(decoder) => decoder,
            Err(err) => {
                if let Some((device, _)) = device {
                    tracing::warn!(
                        target: "xgview::codec",
                        device = device.name,
                        %err,
                        "the device opened but the decoder was refused: the \
                         driver declined the session, which is what its limit on \
                         concurrent hardware decoders looks like"
                    );
                }
                return Err(CodecError::Configure(format!("ffmpeg: {err}")));
            }
        };

        self.codec = config.codec;
        self.decoder = Some(decoder);
        self.scaler = None;
        self.scaler_key = None;
        self.info = None;
        self.frames = 0;
        self.device = device.map(|(device, _)| device);
        self.hardware = false;
        Ok(())
    }

    fn decode(&mut self, access_unit: &[u8], pts_us: i64, keyframe: bool) -> Result<Vec<DecodedFrame>> {
        self.last_pts_us = pts_us;
        // The unit is handed over as it stands. An Annex-B access unit is
        // exactly the packet form `libavcodec` decodes, and the packet needs no
        // flags: the decoder reads the picture boundaries, the key frames and
        // the parameter sets from the NAL units themselves.
        let packet = Packet::copy(access_unit);
        {
            let decoder = self
                .decoder
                .as_mut()
                .ok_or_else(|| CodecError::Configure("decoder is not configured".to_string()))?;
            decoder
                .send_packet(&packet)
                .map_err(|err| CodecError::Decode(format!("ffmpeg: {err}")))?;
        }
        self.collect(pts_us, keyframe)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        if let Some(decoder) = self.decoder.as_mut() {
            decoder
                .send_eof()
                .map_err(|err| CodecError::Decode(format!("ffmpeg: {err}")))?;
        }
        let pts_us = self.last_pts_us;
        self.collect(pts_us, false)
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.info.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(codec: Codec) -> DecoderConfig {
        DecoderConfig { codec, width: 0, height: 0, low_latency: true, hardware: false }
    }

    #[test]
    fn rejects_a_codec_it_cannot_map() {
        let mut decoder = FfmpegDecoder::new();
        assert!(decoder.configure(&config(Codec::Unknown)).is_err());
    }

    #[test]
    fn decoding_before_configuring_is_an_error() {
        let mut decoder = FfmpegDecoder::new();
        assert!(decoder.decode(&[0, 0, 0, 1, 0x65], 0, true).is_err());
    }

    #[test]
    fn ignores_rubbish_without_producing_frames() {
        let mut decoder = FfmpegDecoder::new();
        decoder.configure(&config(Codec::H264)).unwrap();
        // Not a bitstream: whatever comes back, no picture does, and the session
        // survives to be fed the next key frame.
        let produced = decoder.decode(&[0xff; 16], 0, false);
        assert!(produced.is_err() || produced.unwrap().is_empty());
    }

    /// Decodes a real bitstream when one is provided, e.g.
    ///
    /// ```text
    /// ffmpeg -f lavfi -i testsrc=size=160x120:rate=5:duration=0.4 \
    ///     -c:v libx264 -profile:v baseline -pix_fmt yuv420p -f h264 tiny.h264
    /// set XGVIEW_TEST_H264=tiny.h264
    /// ```
    #[test]
    fn decodes_a_real_bitstream() {
        let Ok(path) = std::env::var("XGVIEW_TEST_H264") else {
            return;
        };
        let data = std::fs::read(path).expect("read the bitstream");
        let mut decoder = FfmpegDecoder::new();
        decoder.configure(&config(Codec::H264)).unwrap();
        let mut frames = 0;
        let mut size = None;
        for unit in annex_b_units(&data) {
            let keyframe = first_nal_type(&unit) == 5;
            let Ok(produced) = decoder.decode(&unit, 0, keyframe) else {
                continue;
            };
            for frame in produced {
                assert_eq!(frame.format, PixelFormat::Nv12);
                let (width, height) = (frame.width as usize, frame.height as usize);
                // The rows are padded for the upload, so a plane is longer than
                // the picture in it.
                assert_eq!(frame.planes[0].len(), plane_stride(width) * height, "luma rows");
                assert_eq!(
                    frame.planes[1].len(),
                    plane_stride(width) * height.div_ceil(2),
                    "chroma rows"
                );
                assert!(
                    frame.planes[0].iter().any(|sample| *sample != frame.planes[0][0]),
                    "the luma plane must carry a picture and not a flat field"
                );
                size = Some((frame.width, frame.height));
                frames += 1;
            }
        }
        assert!(frames > 0, "the bitstream must yield at least one picture");
        assert_eq!(size, Some((160, 120)));
    }

    /// Asking for hardware must never cost the picture. The decoder either runs
    /// on the GPU or falls back to the CPU, and either way the bitstream has to
    /// come out as pictures.
    #[test]
    fn decodes_a_real_bitstream_with_hardware_requested() {
        let Ok(path) = std::env::var("XGVIEW_TEST_H264") else {
            return;
        };
        let data = std::fs::read(path).expect("read the bitstream");
        let mut decoder = FfmpegDecoder::new();
        decoder
            .configure(&DecoderConfig { hardware: true, ..config(Codec::H264) })
            .expect("configure the hardware decoder");
        let mut frames = 0;
        for unit in annex_b_units(&data) {
            let keyframe = first_nal_type(&unit) == 5;
            let Ok(produced) = decoder.decode(&unit, 0, keyframe) else {
                continue;
            };
            for frame in produced {
                assert_eq!(frame.format, PixelFormat::Nv12, "both paths hand over NV12");
                frames += 1;
            }
        }
        assert!(frames > 0, "the bitstream must yield at least one picture");
    }

    /// Splits an Annex-B bitstream into NAL units, keeping their start codes:
    /// `libavcodec` expects the packet in that very form.
    fn annex_b_units(data: &[u8]) -> Vec<Vec<u8>> {
        let mut starts = Vec::new();
        let mut offset = 0;
        while offset + 3 <= data.len() {
            if data[offset..].starts_with(&[0, 0, 0, 1]) {
                starts.push(offset);
                offset += 4;
            } else if data[offset..].starts_with(&[0, 0, 1]) {
                starts.push(offset);
                offset += 3;
            } else {
                offset += 1;
            }
        }
        let mut units = Vec::new();
        for (position, start) in starts.iter().enumerate() {
            let end = starts.get(position + 1).copied().unwrap_or(data.len());
            let unit = &data[*start..end];
            // A start code followed by nothing is not a unit.
            if unit.len() > 4 {
                units.push(unit.to_vec());
            }
        }
        units
    }

    /// Type of the NAL unit a start code prefixed slice opens with.
    fn first_nal_type(unit: &[u8]) -> u8 {
        let offset = if unit.starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
        unit.get(offset).map(|byte| byte & 0x1f).unwrap_or(0)
    }
}
