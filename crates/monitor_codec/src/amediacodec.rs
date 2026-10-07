//! Android `AMediaCodec` binding (NDK media).
//!
//! The RTP access units are queued into an `AMediaCodec` opened in decoder mode
//! with **no surface at all**: the codec hands every decoded picture over as a
//! byte buffer, and that buffer is walked into the padded NV12 planes the
//! renderer uploads - the same shape the desktop backend produces.
//!
//! The surface is given up on purpose. Rendering into an `AImageReader` while
//! the process also drives the wgpu surface aborts the process within seconds on
//! a SHIELD (Android 11), inside NVIDIA's sync fence handling -
//! `fdsan: ... actually owned by unique_fd` with `libnvrm_sync.so` in the stack
//! - whatever usage the reader is built with. A control app that runs the same
//! `MediaCodec -> AImageReader` path without a GPU surface in the process
//! decodes happily, and this backend with no surface at all decodes happily too.
//! `docs/DevMemo.md` §9 holds the evidence and the variants that were ruled
//! out. The price is the copy out of the codec's buffer; on this device it is
//! the difference between a picture and an abort.
//!
//! The session is opened once, from [`VideoDecoder::configure`], and lives as
//! long as the channel does. A codec rebuilt for every packet never reaches a
//! steady state, and `low-latency` means nothing to a decoder created one packet
//! ago.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;
use std::time::{Duration, Instant};

use crate::sps::picture_size;
use crate::{
    plane_stride, CodecError, ColorMatrix, ColorRange, ColorSpace, DecodedFrame, DecoderConfig,
    PixelFormat, Result, VideoDecoder, VideoStreamInfo,
};

/// Opaque `AMediaCodec*`.
type AMediaCodec = c_void;
/// Opaque `AMediaFormat*`.
type AMediaFormat = c_void;

/// `AMEDIA_OK`
const AMEDIA_OK: c_int = 0;
/// `AMEDIACODEC_BUFFER_FLAG_KEY_FRAME`
const BUFFER_FLAG_KEY_FRAME: u32 = 1;
/// `INFO_OUTPUT_FORMAT_CHANGED`
const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
/// `INFO_TRY_AGAIN_LATER`
const INFO_TRY_AGAIN_LATER: isize = -1;

/// `COLOR_FormatYUV420Flexible`: the format a decoder offers when it is given no
/// surface to render into. What it hands over is NV12 in practice, with the row
/// pitch and plane height it reports as `stride` and `slice-height`.
const COLOR_FORMAT_FLEXIBLE: i32 = 0x7F42_0888;
/// `COLOR_FormatYUV420SemiPlanar`: the same layout under its older name.
const COLOR_FORMAT_SEMI_PLANAR: i32 = 0x15;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AMediaCodecBufferInfo {
    offset: i32,
    size: i32,
    presentation_time_us: i64,
    flags: u32,
}

#[link(name = "mediandk")]
extern "C" {
    fn AMediaCodec_createDecoderByType(mime_type: *const c_char) -> *mut AMediaCodec;
    fn AMediaCodec_configure(
        codec: *mut AMediaCodec,
        format: *const AMediaFormat,
        window: *mut c_void,
        crypto: *mut c_void,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_start(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_stop(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_delete(codec: *mut AMediaCodec);
    fn AMediaCodec_getInputBuffer(codec: *mut AMediaCodec, index: usize, out_size: *mut usize)
        -> *mut u8;
    fn AMediaCodec_getOutputBuffer(codec: *mut AMediaCodec, index: usize, out_size: *mut usize)
        -> *mut u8;
    fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
    fn AMediaCodec_queueInputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        offset: usize,
        size: usize,
        time_us: u64,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: *mut AMediaCodec,
        info: *mut AMediaCodecBufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_releaseOutputBuffer(codec: *mut AMediaCodec, index: usize, render: bool) -> c_int;
    fn AMediaCodec_flush(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;

    fn AMediaFormat_new() -> *mut AMediaFormat;
    fn AMediaFormat_delete(format: *mut AMediaFormat);
    fn AMediaFormat_setString(format: *mut AMediaFormat, key: *const c_char, value: *const c_char);
    fn AMediaFormat_setInt32(format: *mut AMediaFormat, key: *const c_char, value: i32);
    fn AMediaFormat_setBuffer(
        format: *mut AMediaFormat,
        key: *const c_char,
        data: *const c_void,
        size: usize,
    );
    fn AMediaFormat_getInt32(
        format: *const AMediaFormat,
        key: *const c_char,
        out: *mut i32,
    ) -> bool;
}

/// The parameter sets an Annex-B access unit carries: sequence first, then
/// picture, each without its start code.
///
/// A decoder is configured with these, and the pipeline has prepended the sets
/// it cached to every key frame by the time one reaches the decoder, so they are
/// here even when the session's `SDP` announced none.
fn parameter_sets(access_unit: &[u8]) -> (Option<&[u8]>, Option<&[u8]>) {
    let mut sps = None;
    let mut pps = None;
    let mut offset = 0;
    while offset + 3 <= access_unit.len() {
        let code = if access_unit[offset..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if access_unit[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        let start = offset + code;
        if start >= access_unit.len() {
            break;
        }
        // The NAL runs to the next start code, less the zeros a four byte one
        // leaves in front of it.
        let mut end = start;
        while end + 3 <= access_unit.len() && !access_unit[end..].starts_with(&[0, 0, 1]) {
            end += 1;
        }
        let mut nal = &access_unit[start..end];
        while nal.last() == Some(&0) {
            nal = &nal[..nal.len() - 1];
        }
        match nal.first().map(|byte| byte & 0x1f) {
            Some(7) => sps = Some(nal),
            Some(8) => pps = Some(nal),
            _ => {}
        }
        offset = end.max(offset + 1);
    }
    (sps, pps)
}

/// `AMediaCodec` decoder reading its pictures back as byte buffers.
#[derive(Debug, Default)]
pub struct AMediaCodecDecoder {
    /// The codec, alive from the first access unit to the end of the session.
    handle: Option<*mut AMediaCodec>,
    /// Configuration remembered from [`VideoDecoder::configure`], which is the
    /// only thing known before the stream is: the parameter sets the codec has
    /// to be opened with come with the first access unit.
    config: DecoderConfig,
    info: Option<VideoStreamInfo>,
    /// Colour description of the stream, read from the output format and used
    /// for every picture; the renderer's default until the codec announces its
    /// own.
    colorspace: ColorSpace,
    /// Picture size the codec announced for its output.
    width: u32,
    height: u32,
    /// Row pitch of a plane, and the number of rows the luma plane occupies.
    /// Both are the codec's to choose and are padding, not geometry.
    stride: u32,
    slice_height: u32,
    started: bool,
    /// Set once an output format this cannot read as NV12 has been reported.
    format_reported: bool,
}

// Safety: the codec is owned outright by the channel worker that opened it and
// is never touched from two threads at once; the renderer only ever sees the
// finished planes. The raw pointer is the only reason the type is not `Send` on
// its own.
unsafe impl Send for AMediaCodecDecoder {}

impl AMediaCodecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the codec session, which is given no surface and therefore decodes
    /// into byte buffers rather than rendering.
    ///
    /// `access_unit` is the first one to reach the decoder, and it is what
    /// carries the parameter sets.
    ///
    /// It is opened from there rather than from `configure` because a session
    /// whose `SDP` announced no `sprop-parameter-sets` - which is what the two
    /// streams that failed on a Qualcomm decoder had in common - leaves neither
    /// a size nor the `csd-0`/`csd-1` that decoder insists on: it refuses the
    /// configuration with `-10000` rather than reading either from the bitstream.
    fn open(&mut self, access_unit: &[u8]) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }
        let config = self.config.clone();
        let mime = CString::new(config.codec.mime())
            .map_err(|err| CodecError::Configure(err.to_string()))?;
        let (sps, pps) = parameter_sets(access_unit);
        // What the stream describes beats what the session announced, which is
        // only a hint for the buffers the decoder sizes up front.
        let size = sps
            .and_then(picture_size)
            .or(Some((config.width, config.height)).filter(|(w, h)| *w > 0 && *h > 0));

        // Low latency is asked for first and given up only if the codec refuses
        // the whole configuration over it. `ACodec` does not treat the option as
        // a hint it may drop: HiSilicon's H.264 decoder answers
        // `OMX_ErrorUndefined` to it and the configuration is lost, which the
        // NDK reports as the same meaningless `-10000` it reports for a format
        // the codec will not take at all. The public NDK has no way to ask a
        // codec whether it supports the option - `AMediaCodecInfo` is not in it
        // - so trying is what keeps the option on the decoders that do take it.
        let mut low_latency = config.low_latency;
        let codec = match Self::open_session(&mime, sps, pps, size, low_latency) {
            Ok(codec) => codec,
            Err(err) if low_latency => {
                low_latency = false;
                tracing::debug!(
                    target: "xgview::codec",
                    %err,
                    "the decoder refused low latency; opening without it"
                );
                Self::open_session(&mime, sps, pps, size, false)?
            }
            Err(err) => return Err(err),
        };

        self.handle = Some(codec);
        self.started = true;
        // The size the stream describes is the one to report, not the one the
        // session guessed at before any of it arrived.
        if let (Some((width, height)), Some(info)) = (size, self.info.as_mut()) {
            info.width = width;
            info.height = height;
        }
        tracing::debug!(
            target: "xgview::codec",
            codec = config.codec.as_str(),
            low_latency,
            "AMediaCodec session open, decoding into byte buffers"
        );
        Ok(())
    }

    /// Creates, configures and starts one codec session, handing back the codec.
    ///
    /// A codec that failed to configure is released before the error comes back,
    /// so a caller that wants to try again with a different format has nothing
    /// to clean up.
    fn open_session(
        mime: &CString,
        sps: Option<&[u8]>,
        pps: Option<&[u8]>,
        size: Option<(u32, u32)>,
        low_latency: bool,
    ) -> Result<*mut AMediaCodec> {
        let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr()) };
        if codec.is_null() {
            return Err(CodecError::Unsupported(format!(
                "AMediaCodec cannot create a decoder for {}",
                mime.to_string_lossy()
            )));
        }

        let format = unsafe { AMediaFormat_new() };
        unsafe {
            AMediaFormat_setString(format, c"mime".as_ptr(), mime.as_ptr());
            if let Some((width, height)) = size {
                AMediaFormat_setInt32(format, c"width".as_ptr(), width as i32);
                AMediaFormat_setInt32(format, c"height".as_ptr(), height as i32);
            }
            // The parameter sets themselves, which is what a decoder that will
            // not take a bare format is waiting for.
            if let Some(sps) = sps {
                AMediaFormat_setBuffer(format, c"csd-0".as_ptr(), sps.as_ptr().cast(), sps.len());
            }
            if let Some(pps) = pps {
                AMediaFormat_setBuffer(format, c"csd-1".as_ptr(), pps.as_ptr().cast(), pps.len());
            }
            // Low latency: do not buffer more than one frame in the codec.
            if low_latency {
                AMediaFormat_setInt32(format, c"low-latency".as_ptr(), 1);
            }
            AMediaFormat_setInt32(format, c"color-format".as_ptr(), COLOR_FORMAT_FLEXIBLE);
        }

        tracing::debug!(
            target: "xgview::codec",
            mime = mime.to_string_lossy().as_ref(),
            width = size.map(|(width, _)| width).unwrap_or(0),
            height = size.map(|(_, height)| height).unwrap_or(0),
            sps = sps.map(|nal| nal.len()).unwrap_or(0),
            pps = pps.map(|nal| nal.len()).unwrap_or(0),
            low_latency,
            "configuring the decoder"
        );
        // A null window is what asks for byte buffers instead of a surface.
        // `AMEDIACODEC_CONFIGURE_FLAG_ENCODE` is 0 for the decoder direction.
        let configured = unsafe { AMediaCodec_configure(codec, format, ptr::null_mut(), ptr::null_mut(), 0) };
        unsafe { AMediaFormat_delete(format) };
        if configured != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure(format!(
                "AMediaCodec_configure failed with status {configured}"
            )));
        }
        if unsafe { AMediaCodec_start(codec) } != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure("AMediaCodec_start failed".to_string()));
        }
        Ok(codec)
    }

    /// Reads the picture geometry and colour description the codec announced.
    ///
    /// Both sizes matter. The picture size is what the planes are cut to, and
    /// `stride` and `slice-height` are the padding the codec put around them:
    /// they are the codec's to choose and say nothing about the picture.
    fn read_output_format(&mut self, codec: *mut AMediaCodec) {
        let format = unsafe { AMediaCodec_getOutputFormat(codec) };
        if format.is_null() {
            return;
        }
        let mut width: i32 = 0;
        let mut height: i32 = 0;
        let mut stride: i32 = 0;
        let mut slice_height: i32 = 0;
        let mut standard: i32 = 0;
        let mut range: i32 = 0;
        let mut colour_format: i32 = 0;
        // Safety: each call only writes its out parameter, and `format` is live
        // until it is deleted below.
        let (has_width, has_height, has_stride, has_slice, has_standard, has_range, has_colour) =
            unsafe {
                (
                    AMediaFormat_getInt32(format, c"width".as_ptr(), &mut width),
                    AMediaFormat_getInt32(format, c"height".as_ptr(), &mut height),
                    AMediaFormat_getInt32(format, c"stride".as_ptr(), &mut stride),
                    AMediaFormat_getInt32(format, c"slice-height".as_ptr(), &mut slice_height),
                    AMediaFormat_getInt32(format, c"color-standard".as_ptr(), &mut standard),
                    AMediaFormat_getInt32(format, c"color-range".as_ptr(), &mut range),
                    AMediaFormat_getInt32(format, c"color-format".as_ptr(), &mut colour_format),
                )
            };
        unsafe { AMediaFormat_delete(format) };

        if has_width && width > 0 {
            self.width = width as u32;
        }
        if has_height && height > 0 {
            self.height = height as u32;
        }
        if has_stride && stride > 0 {
            self.stride = stride as u32;
        }
        if has_slice && slice_height > 0 {
            self.slice_height = slice_height as u32;
        }
        if has_standard {
            self.colorspace.matrix = match standard {
                2 | 4 => ColorMatrix::Bt601,
                6 => ColorMatrix::Bt2020,
                _ => ColorMatrix::Bt709,
            };
        }
        if has_range {
            self.colorspace.range =
                if range == 1 { ColorRange::Full } else { ColorRange::Limited };
        }
        let nv12 = !has_colour
            || colour_format == COLOR_FORMAT_FLEXIBLE
            || colour_format == COLOR_FORMAT_SEMI_PLANAR;
        if !nv12 && !self.format_reported {
            self.format_reported = true;
            tracing::warn!(
                target: "xgview::codec",
                colour_format,
                "the codec's output is not a format this reads as NV12"
            );
        }
    }

    /// Releases every output buffer the codec has ready, packing the pictures it
    /// hands over.
    ///
    /// Every buffer has to be taken and given back: in byte buffer mode a buffer
    /// the codec hands out is not returned until it is released, and there are
    /// only a handful of them.
    fn drain(&mut self, frames: &mut Vec<DecodedFrame>) {
        let Some(codec) = self.handle else {
            return;
        };
        loop {
            let mut info = AMediaCodecBufferInfo::default();
            let index = unsafe { AMediaCodec_dequeueOutputBuffer(codec, &mut info, 0) };
            match index {
                INFO_TRY_AGAIN_LATER => break,
                INFO_OUTPUT_FORMAT_CHANGED => {
                    self.read_output_format(codec);
                    continue;
                }
                index if index >= 0 => {
                    if info.size > 0 {
                        let mut capacity = 0usize;
                        let buffer = unsafe {
                            AMediaCodec_getOutputBuffer(codec, index as usize, &mut capacity)
                        };
                        if !buffer.is_null() {
                            if let Some(frame) = self.pack(
                                buffer,
                                capacity,
                                info.presentation_time_us,
                                info.flags & BUFFER_FLAG_KEY_FRAME != 0,
                            ) {
                                frames.push(frame);
                            }
                        }
                    }
                    // Nothing was rendered, so there is nothing to render now.
                    unsafe { AMediaCodec_releaseOutputBuffer(codec, index as usize, false) };
                }
                _ => break,
            }
        }
    }

    /// Turns one output buffer into the padded NV12 planes the renderer takes.
    ///
    /// `None` when the buffer is shorter than the layout its own format
    /// describes, which is a picture this cannot cut correctly and is better
    /// dropped than cut in the wrong place.
    fn pack(
        &self,
        buffer: *const u8,
        capacity: usize,
        pts_us: i64,
        keyframe: bool,
    ) -> Option<DecodedFrame> {
        let (width, height) = (self.width as usize, self.height as usize);
        if width == 0 || height == 0 || capacity == 0 {
            return None;
        }
        // Safety: the codec handed over `capacity` readable bytes at `buffer`,
        // and the slice lives only for this call - the caller releases the
        // buffer the moment this returns.
        let data = unsafe { std::slice::from_raw_parts(buffer, capacity) };
        let stride = if self.stride == 0 { width } else { self.stride as usize };
        let slice_height = if self.slice_height == 0 { height } else { self.slice_height as usize };
        let chroma_height = height.div_ceil(2);
        let padded = plane_stride(width);

        let mut y = vec![0u8; padded * height];
        for row in 0..height {
            let start = row * stride;
            let source = data.get(start..start + width)?;
            y.get_mut(row * padded..row * padded + width)?.copy_from_slice(source);
        }

        // The interleaved chroma plane follows the luma one, one chroma row per
        // two luma rows, and the codec's padding sits between them.
        let base = stride * slice_height;
        let mut uv = vec![0u8; padded * chroma_height];
        for row in 0..chroma_height {
            let start = base + row * stride;
            let source = data.get(start..start + width)?;
            uv.get_mut(row * padded..row * padded + width)?.copy_from_slice(source);
        }

        Some(DecodedFrame {
            width: width as u32,
            height: height as u32,
            format: PixelFormat::Nv12,
            colorspace: self.colorspace,
            pts_us,
            keyframe,
            buffer: None,
            planes: vec![y, uv],
        })
    }

    /// Stops and releases the codec. Safe to call twice, and called by [`Drop`]
    /// when the channel ends.
    fn close(&mut self) {
        if let Some(codec) = self.handle.take() {
            if self.started {
                unsafe { AMediaCodec_stop(codec) };
                self.started = false;
            }
            unsafe { AMediaCodec_delete(codec) };
        }
    }
}

impl VideoDecoder for AMediaCodecDecoder {
    fn name(&self) -> &'static str {
        "AMediaCodec"
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        self.info = Some(VideoStreamInfo {
            codec: config.codec,
            width: config.width,
            height: config.height,
            fps: None,
            hardware: true,
        });
        // Remembered, not acted on: the codec session is opened on the first
        // access unit, which is the first thing that can describe the stream.
        self.config = config.clone();
        Ok(())
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        if self.handle.is_none() {
            self.open(access_unit)?;
        }
        let Some(codec) = self.handle else {
            return Err(CodecError::Configure("decoder was never configured".to_string()));
        };

        let deadline = Instant::now() + Duration::from_millis(100);
        let mut index = unsafe { AMediaCodec_dequeueInputBuffer(codec, 0) };
        while index < 0 && Instant::now() < deadline {
            index = unsafe { AMediaCodec_dequeueInputBuffer(codec, 10_000) };
        }
        if index < 0 {
            return Err(CodecError::NoBuffer);
        }

        let mut capacity = 0usize;
        let buffer = unsafe { AMediaCodec_getInputBuffer(codec, index as usize, &mut capacity) };
        if buffer.is_null() || capacity < access_unit.len() {
            // The input buffer has to be handed back or the codec runs out of
            // them; queueing it empty is how a dequeued input buffer is returned
            // unused. The picture it would have carried is lost either way.
            unsafe {
                AMediaCodec_queueInputBuffer(codec, index as usize, 0, 0, pts_us.max(0) as u64, 0);
            }
            return Err(CodecError::Decode("input buffer too small".to_string()));
        }
        unsafe {
            ptr::copy_nonoverlapping(access_unit.as_ptr(), buffer, access_unit.len());
        }

        let mut flags = 0u32;
        if keyframe {
            flags |= BUFFER_FLAG_KEY_FRAME;
        }
        unsafe {
            AMediaCodec_queueInputBuffer(
                codec,
                index as usize,
                0,
                access_unit.len(),
                pts_us.max(0) as u64,
                flags,
            );
        }

        let mut frames = Vec::new();
        self.drain(&mut frames);
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        // The end of a session: hand over whatever the codec is still holding.
        let mut frames = Vec::new();
        self.drain(&mut frames);
        Ok(frames)
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.info.as_ref()
    }
}

impl Drop for AMediaCodecDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

impl AMediaCodecDecoder {
    /// Drops the buffered pictures and restarts the codec, for a stream switch
    /// (main <-> sub) that keeps the same session.
    pub fn reset(&mut self) {
        if let Some(codec) = self.handle {
            unsafe { AMediaCodec_flush(codec) };
        }
    }
}
