//! Android `AMediaCodec` binding (NDK media).
//!
//! The RTP access units are queued into an `AMediaCodec` created in *decoder*
//! mode whose output surface is the `ANativeWindow` of an `AImageReader`: every
//! decoded picture comes back as a `YUV_420_888` image, its planes are walked
//! into the padded NV12 layout the renderer uploads, and the colour conversion
//! stays on the GPU in the one shader every target shares.
//!
//! The session is opened once, from [`VideoDecoder::configure`], and lives as
//! long as the channel does. A codec rebuilt for every packet never reaches a
//! steady state - each frame becomes a configure / start / stop cycle - and
//! `low-latency` means nothing to a decoder that was created one packet ago.
//!
//! This is deliberately not a direct-to-`ANativeWindow` path. The grid is drawn
//! by egui through wgpu, which composites its own textures, and a codec
//! rendering into a window is a layer of its own that cannot be composited into
//! that grid. The image reader is what turns the codec's GPU output back into
//! the planes the grid can take.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;
use std::time::{Duration, Instant};

use crate::{
    plane_stride, CodecError, ColorMatrix, ColorRange, ColorSpace, DecodedFrame, DecoderConfig,
    PixelFormat, Result, VideoDecoder, VideoStreamInfo,
};

/// Opaque `ANativeWindow*`.
pub type ANativeWindow = c_void;
/// Opaque `AMediaCodec*`.
type AMediaCodec = c_void;
/// Opaque `AMediaFormat*`.
type AMediaFormat = c_void;
/// Opaque `AImageReader*`.
type AImageReader = c_void;
/// Opaque `AImage*`.
type AImage = c_void;

/// `AMEDIA_OK`
const AMEDIA_OK: c_int = 0;
/// `AMEDIACODEC_BUFFER_FLAG_KEY_FRAME`
const BUFFER_FLAG_KEY_FRAME: u32 = 1;
/// `INFO_OUTPUT_FORMAT_CHANGED`
const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
/// `INFO_TRY_AGAIN_LATER`
const INFO_TRY_AGAIN_LATER: isize = -1;

/// `AIMAGE_FORMAT_YUV_420_888`: the one format every decoder can output and
/// every reader can hand over as planes.
const AIMAGE_FORMAT_YUV_420_888: c_int = 0x23;
/// `AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN`: the planes are read on the CPU.
const AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN: u64 = 3;
/// Images the reader holds. Two cover reading the newest while the codec fills
/// the next; a deeper queue only adds latency to a live view.
const READER_IMAGES: i32 = 2;
/// Reader size used when neither the stream nor the configuration named one.
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
/// Access units a session waits for one carrying a sequence parameter set
/// before it settles for the size the SDP guessed.
const PENDING_LIMIT: u32 = 20;

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
        window: *mut ANativeWindow,
        crypto: *mut c_void,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_start(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_stop(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_delete(codec: *mut AMediaCodec);
    fn AMediaCodec_getInputBuffer(codec: *mut AMediaCodec, index: usize, out_size: *mut usize)
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
    fn AMediaFormat_getInt32(
        format: *const AMediaFormat,
        key: *const c_char,
        out: *mut i32,
    ) -> bool;

    fn AImageReader_newWithUsage(
        width: i32,
        height: i32,
        format: i32,
        usage: u64,
        max_images: i32,
        reader: *mut *mut AImageReader,
    ) -> c_int;
    fn AImageReader_getWindow(
        reader: *mut AImageReader,
        window: *mut *mut ANativeWindow,
    ) -> c_int;
    fn AImageReader_acquireLatestImage(
        reader: *mut AImageReader,
        image: *mut *mut AImage,
    ) -> c_int;
    fn AImageReader_delete(reader: *mut AImageReader);

    fn AImage_getWidth(image: *const AImage, width: *mut i32) -> c_int;
    fn AImage_getHeight(image: *const AImage, height: *mut i32) -> c_int;
    fn AImage_getPlaneData(
        image: *const AImage,
        plane: c_int,
        data: *mut *mut u8,
        length: *mut c_int,
    ) -> c_int;
    fn AImage_getPlaneRowStride(image: *const AImage, plane: c_int, stride: *mut i32) -> c_int;
    fn AImage_getPlanePixelStride(image: *const AImage, plane: c_int, stride: *mut i32) -> c_int;
    fn AImage_delete(image: *mut AImage);
}


/// `AMediaCodec` decoder reading its pictures back through an `AImageReader`.
#[derive(Debug, Default)]
pub struct AMediaCodecDecoder {
    /// The codec, alive from the first access unit to the end of the session.
    handle: Option<*mut AMediaCodec>,
    /// The reader backing the codec's output surface; its lifetime is tied to
    /// the codec's, which must not outlive it.
    reader: Option<*mut AImageReader>,
    info: Option<VideoStreamInfo>,
    /// Colour description of the stream, read from the output format and used
    /// for every picture; the renderer's default until the codec announces its
    /// own.
    colorspace: ColorSpace,
    /// What the session was configured with. The codec is opened on the first
    /// access unit, which is where the size the reader needs comes from.
    config: Option<DecoderConfig>,
    /// Access units seen while waiting for one that carries a size.
    pending: u32,
    /// Set once a picture whose planes could not be read has been reported.
    pack_reported: bool,
    started: bool,
    /// Timestamp of the last queued access unit, used for the output timestamps.
    last_pts_us: i64,
}

// Safety: the codec and the reader are owned outright by the channel worker
// that opened them and are never touched from two threads at once; the renderer
// only ever sees the finished planes. The raw pointers are the only reason the
// type is not `Send` on its own.
unsafe impl Send for AMediaCodecDecoder {}

impl AMediaCodecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the codec session and the image reader behind it.
    ///
    /// The reader is built to `size`, which the caller read out of the stream:
    /// the decoder renders a picture at its own size whatever the format asks
    /// for, and a reader of any other size cannot be read back at all. The
    /// reader's window is the codec's output surface, so the reader is created
    /// first and outlives the codec.
    fn open(&mut self, config: &DecoderConfig, size: (u32, u32)) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }
        let mime = CString::new(config.codec.mime())
            .map_err(|err| CodecError::Configure(err.to_string()))?;
        let (width, height) = size;

        // The reader is the codec's output surface. Only the CPU read usage is
        // asked for: a buffer created with `GPU_SAMPLED_IMAGE` is handed to the
        // GPU alone, and `AImage_getPlaneData` then fails with
        // `AMEDIA_IMGREADER_CANNOT_LOCK_IMAGE` - the planes cannot be read at
        // all, and the channel shows nothing. The decoder takes the surface
        // either way.
        let mut reader: *mut AImageReader = ptr::null_mut();
        let status = unsafe {
            AImageReader_newWithUsage(
                width as i32,
                height as i32,
                AIMAGE_FORMAT_YUV_420_888,
                AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
                READER_IMAGES,
                &mut reader,
            )
        };
        if status != AMEDIA_OK || reader.is_null() {
            return Err(CodecError::Configure(format!(
                "AImageReader_newWithUsage failed with status {status}"
            )));
        }

        let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr()) };
        if codec.is_null() {
            unsafe { AImageReader_delete(reader) };
            return Err(CodecError::Unsupported(format!(
                "AMediaCodec cannot create a decoder for {}",
                config.codec
            )));
        }

        let format = unsafe { AMediaFormat_new() };
        unsafe {
            AMediaFormat_setString(format, c"mime".as_ptr(), mime.as_ptr());
            AMediaFormat_setInt32(format, c"width".as_ptr(), width as i32);
            AMediaFormat_setInt32(format, c"height".as_ptr(), height as i32);
            // Low latency: do not buffer more than one frame in the codec.
            let latency = if config.low_latency { 1 } else { 0 };
            AMediaFormat_setInt32(format, c"low-latency".as_ptr(), latency);
        }

        let mut window: *mut ANativeWindow = ptr::null_mut();
        let window_status = unsafe { AImageReader_getWindow(reader, &mut window) };
        if window_status != AMEDIA_OK || window.is_null() {
            unsafe {
                AMediaFormat_delete(format);
                AMediaCodec_delete(codec);
                AImageReader_delete(reader);
            }
            return Err(CodecError::Configure(format!(
                "AImageReader_getWindow failed with status {window_status}"
            )));
        }
        // `AMEDIACODEC_CONFIGURE_FLAG_ENCODE` is 0 for the decoder direction.
        let configured = unsafe { AMediaCodec_configure(codec, format, window, ptr::null_mut(), 0) };
        unsafe { AMediaFormat_delete(format) };
        if configured != AMEDIA_OK {
            unsafe {
                AMediaCodec_delete(codec);
                AImageReader_delete(reader);
            }
            return Err(CodecError::Configure(format!(
                "AMediaCodec_configure failed with status {configured}"
            )));
        }
        if unsafe { AMediaCodec_start(codec) } != AMEDIA_OK {
            unsafe {
                AMediaCodec_delete(codec);
                AImageReader_delete(reader);
            }
            return Err(CodecError::Configure("AMediaCodec_start failed".to_string()));
        }

        self.handle = Some(codec);
        self.reader = Some(reader);
        self.started = true;
        tracing::debug!(
            target: "xgview::codec",
            width,
            height,
            "AMediaCodec session open, decoding into an image reader"
        );
        Ok(())
    }

    /// Stops and releases the codec and the reader. Safe to call twice, and
    /// called by [`Drop`] when the channel ends.
    fn close(&mut self) {
        if let Some(codec) = self.handle.take() {
            if self.started {
                unsafe { AMediaCodec_stop(codec) };
                self.started = false;
            }
            unsafe { AMediaCodec_delete(codec) };
        }
        if let Some(reader) = self.reader.take() {
            unsafe { AImageReader_delete(reader) };
        }
    }

    /// Releases every output buffer the codec has ready, then reads the newest
    /// image the reader holds.
    ///
    /// `render = true` is what queues a decoded picture to the reader's surface;
    /// without it nothing ever reaches the image. Every ready buffer is released
    /// so the codec can keep going, but only the newest image is read: a grid
    /// that falls behind should skip pictures, not queue them.
    fn drain(&mut self, frames: &mut Vec<DecodedFrame>) {
        let Some(codec) = self.handle else {
            return;
        };
        let mut pts_us = self.last_pts_us;
        let mut keyframe = false;
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
                    pts_us = info.presentation_time_us;
                    keyframe = info.flags & BUFFER_FLAG_KEY_FRAME != 0;
                    unsafe { AMediaCodec_releaseOutputBuffer(codec, index as usize, true) };
                }
                _ => break,
            }
        }
        self.collect(frames, pts_us, keyframe);
    }

    /// Appends the newest image of the reader, if one is ready.
    fn collect(&mut self, frames: &mut Vec<DecodedFrame>, pts_us: i64, keyframe: bool) {
        let Some(reader) = self.reader else {
            return;
        };
        let mut image: *mut AImage = ptr::null_mut();
        let status = unsafe { AImageReader_acquireLatestImage(reader, &mut image) };
        if status != AMEDIA_OK || image.is_null() {
            return;
        }
        if let Some((width, height, y, uv)) = pack_image(image) {
            frames.push(DecodedFrame {
                width,
                height,
                format: PixelFormat::Nv12,
                colorspace: self.colorspace,
                pts_us,
                keyframe,
                buffer: None,
                planes: vec![y, uv],
            });
        } else if !self.pack_reported {
            self.pack_reported = true;
            tracing::warn!(
                target: "xgview::codec",
                "the picture planes cannot be read back, the tile stays blank"
            );
        }
        unsafe { AImage_delete(image) };
    }

    /// Reads the colour description the codec announced for its output.
    ///
    /// The keys are the `MediaFormat` ones: `color-standard` (1 BT.709, 2 and 4
    /// BT.601, 6 BT.2020) and `color-range` (1 full, 2 limited). A codec that
    /// reports neither leaves the default in place.
    fn read_output_format(&mut self, codec: *mut AMediaCodec) {
        let format = unsafe { AMediaCodec_getOutputFormat(codec) };
        if format.is_null() {
            return;
        }
        let mut standard: i32 = 0;
        let mut range: i32 = 0;
        // Safety: each call only writes its out parameter, and `format` is live
        // until it is deleted below.
        let (has_standard, has_range) = unsafe {
            (
                AMediaFormat_getInt32(format, c"color-standard".as_ptr(), &mut standard),
                AMediaFormat_getInt32(format, c"color-range".as_ptr(), &mut range),
            )
        };
        unsafe { AMediaFormat_delete(format) };
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
        // The session is opened on the first access unit rather than here: the
        // image reader has to be built to the size the stream really carries,
        // and that size travels in the sequence parameter set the first key
        // frame brings, not in the SDP.
        self.config = Some(config.clone());
        Ok(())
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        if self.handle.is_none() {
            let Some(config) = self.config.clone() else {
                return Err(CodecError::Configure("decoder was never configured".to_string()));
            };
            match crate::sps::access_unit_size(access_unit) {
                Some(size) => self.open(&config, size)?,
                None if self.pending < PENDING_LIMIT => {
                    // The picture cannot decode before its parameter set, and
                    // the reader cannot be built without a size. It waits for
                    // the key frame that carries one rather than guessing.
                    self.pending += 1;
                    return Ok(Vec::new());
                }
                None => {
                    tracing::debug!(
                        target: "xgview::codec",
                        "no sequence parameter set arrived, sizing the reader from the sdp"
                    );
                    let size = if config.width > 0 && config.height > 0 {
                        (config.width, config.height)
                    } else {
                        (DEFAULT_WIDTH, DEFAULT_HEIGHT)
                    };
                    self.open(&config, size)?;
                }
            }
        }
        let Some(codec) = self.handle else {
            return Err(CodecError::Configure("the codec did not open".to_string()));
        };
        self.last_pts_us = pts_us;

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
            // them; queueing it empty is how a dequeued input buffer is
            // returned unused. The picture it would have carried is lost either
            // way, and the session survives to decode the next one.
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
        // The end of a session: hand over whatever image the reader is holding.
        let mut frames = Vec::new();
        self.collect(&mut frames, self.last_pts_us, false);
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

/// One plane of an `AImage`, as the reader handed it over.
struct Plane {
    data: *const u8,
    len: usize,
    row_stride: usize,
    pixel_stride: usize,
}

/// Reads one plane's pointer, length and strides out of an `AImage`.
fn plane_of(image: *const AImage, index: c_int) -> Option<Plane> {
    let mut data: *mut u8 = ptr::null_mut();
    let mut len: c_int = 0;
    let mut row_stride: c_int = 0;
    let mut pixel_stride: c_int = 0;
    // Safety: `image` is live for this call, and each function only writes its
    // out parameters.
    let ok = unsafe {
        AImage_getPlaneData(image, index, &mut data, &mut len) == AMEDIA_OK
            && AImage_getPlaneRowStride(image, index, &mut row_stride) == AMEDIA_OK
            && AImage_getPlanePixelStride(image, index, &mut pixel_stride) == AMEDIA_OK
    };
    if !ok || data.is_null() || len <= 0 {
        return None;
    }
    Some(Plane {
        data,
        len: len as usize,
        row_stride: row_stride.max(0) as usize,
        pixel_stride: pixel_stride.max(1) as usize,
    })
}

/// Reads one `YUV_420_888` image into the padded NV12 planes the renderer takes.
///
/// Luma is copied row by row into a plane padded to the upload alignment. The
/// two chroma planes are interleaved into the single UV plane NV12 wants, which
/// covers both shapes a reader hands them over in: planar (`pixel_stride` 1,
/// I420) and already interleaved (`pixel_stride` 2, NV12/NV21).
fn pack_image(image: *const AImage) -> Option<(u32, u32, Vec<u8>, Vec<u8>)> {
    let mut width: i32 = 0;
    let mut height: i32 = 0;
    // Safety: `image` is live for this call, and both functions only write their
    // out parameter.
    unsafe {
        AImage_getWidth(image, &mut width);
        AImage_getHeight(image, &mut height);
    }
    if width <= 0 || height <= 0 {
        return None;
    }
    let (width, height) = (width as usize, height as usize);
    let stride = plane_stride(width);

    let luma = plane_of(image, 0)?;
    let mut y = vec![0u8; stride * height];
    copy_luma(&luma, width, height, stride, &mut y)?;

    let u = plane_of(image, 1)?;
    let v = plane_of(image, 2)?;
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let mut uv = vec![0u8; stride * chroma_height];
    copy_chroma(&u, &v, chroma_width, chroma_height, stride, &mut uv)?;

    Some((width as u32, height as u32, y, uv))
}

/// Copies the luma plane, stripping its row stride and padding every row.
fn copy_luma(plane: &Plane, width: usize, height: usize, stride: usize, out: &mut [u8]) -> Option<()> {
    // Safety: the plane's pointer and length come from the reader and stay live
    // for as long as the image does, which is the whole call.
    let data = unsafe { std::slice::from_raw_parts(plane.data, plane.len) };
    for row in 0..height {
        let base = row * plane.row_stride;
        let dst = out.get_mut(row * stride..row * stride + width)?;
        if plane.pixel_stride == 1 {
            let end = base.checked_add(width)?;
            dst.copy_from_slice(data.get(base..end)?);
        } else {
            for (x, byte) in dst.iter_mut().enumerate() {
                let at = base.checked_add(x * plane.pixel_stride)?;
                *byte = *data.get(at)?;
            }
        }
    }
    Some(())
}

/// Copies the two chroma planes into one interleaved UV plane.
fn copy_chroma(
    u: &Plane,
    v: &Plane,
    width: usize,
    height: usize,
    stride: usize,
    out: &mut [u8],
) -> Option<()> {
    let (u_stride, u_pixel) = (u.row_stride, u.pixel_stride);
    let (v_stride, v_pixel) = (v.row_stride, v.pixel_stride);
    // Safety: both pointers and lengths come from the reader and stay live for
    // as long as the image does, which is the whole call.
    let (u, v) = unsafe {
        (
            std::slice::from_raw_parts(u.data, u.len),
            std::slice::from_raw_parts(v.data, v.len),
        )
    };
    for row in 0..height {
        let dst = out.get_mut(row * stride..row * stride + width * 2)?;
        let u_base = row * u_stride;
        let v_base = row * v_stride;
        for x in 0..width {
            let at_u = u_base.checked_add(x * u_pixel)?;
            let at_v = v_base.checked_add(x * v_pixel)?;
            *dst.get_mut(2 * x)? = *u.get(at_u)?;
            *dst.get_mut(2 * x + 1)? = *v.get(at_v)?;
        }
    }
    Some(())
}
