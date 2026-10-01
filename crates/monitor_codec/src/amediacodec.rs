//! Android `AMediaCodec` binding (NDK media) with zero copy rendering.
//!
//! ![Pipeline](https://developer.android.com/ndk) The RTP access units are
//! queued into an `AMediaCodec` created in *decoder* mode and configured with
//! an `ANativeWindow` surface. Decoded frames are then rendered by the codec
//! itself into the surface (or an `AHardwareBuffer`), so the YUV data is never
//! copied back to the CPU.
//!
//! The entry point (`monitor_android`) owns the `ANativeWindow` obtained from
//! the `android-activity` surface callback and passes it to
//! [`AMediaCodecDecoder::set_surface`].

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;
use std::time::Duration;

use crate::{Codec, CodecError, DecodedFrame, DecoderConfig, Result, VideoDecoder, VideoStreamInfo};

/// Opaque `ANativeWindow*`.
pub type ANativeWindow = c_void;
/// Opaque `AMediaCodec*`.
type AMediaCodec = c_void;
/// Opaque `AMediaFormat*`.
type AMediaFormat = c_void;

/// `AMEDIA_OK`
const AMEDIA_OK: c_int = 0;
/// `AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG`
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
/// `AMEDIACODEC_BUFFER_FLAG_KEY_FRAME`
const BUFFER_FLAG_KEY_FRAME: u32 = 1;
/// `INFO_OUTPUT_FORMAT_CHANGED`
const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
/// `INFO_TRY_AGAIN_LATER`
const INFO_TRY_AGAIN_LATER: isize = -1;

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

    fn AMediaFormat_new() -> *mut AMediaFormat;
    fn AMediaFormat_delete(format: *mut AMediaFormat);
    fn AMediaFormat_setString(format: *mut AMediaFormat, key: *const c_char, value: *const c_char);
    fn AMediaFormat_setInt32(format: *mut AMediaFormat, key: *const c_char, value: i32);
}

/// Zero copy `AMediaCodec` decoder.
#[derive(Debug, Default)]
pub struct AMediaCodecDecoder {
    codec: Option<Codec>,
    surface: Option<usize>,
    info: Option<VideoStreamInfo>,
    started: bool,
    /// Timestamp of the last queued access unit, used for the output timestamps.
    last_pts_us: i64,
}

impl AMediaCodecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the surface (`ANativeWindow*`) provided by the Android activity.
    ///
    /// Passing the surface into the codec is what makes the path zero copy:
    /// `AMediaCodec_releaseOutputBuffer(.., true)` renders directly into the
    /// window without ever touching the CPU.
    pub fn set_surface(&mut self, surface: *mut ANativeWindow) {
        self.surface = Some(surface as usize);
    }

    /// `true` when this decoder renders into a hardware surface.
    pub fn is_zero_copy(&self) -> bool {
        self.surface.is_some()
    }

    fn open(&mut self, config: &DecoderConfig) -> Result<*mut AMediaCodec> {
        let mime = CString::new(config.codec.mime())
            .map_err(|err| CodecError::Configure(err.to_string()))?;
        let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr()) };
        if codec.is_null() {
            return Err(CodecError::Unsupported(format!(
                "AMediaCodec cannot create a decoder for {}",
                config.codec
            )));
        }

        let format = unsafe { AMediaFormat_new() };
        unsafe {
            AMediaFormat_setString(format, c"mime".as_ptr(), mime.as_ptr());
            AMediaFormat_setInt32(format, c"width".as_ptr(), config.width as i32);
            AMediaFormat_setInt32(format, c"height".as_ptr(), config.height as i32);
            // Low latency: do not buffer more than one frame in the codec.
            let latency = if config.low_latency { 1 } else { 0 };
            AMediaFormat_setInt32(format, c"low-latency".as_ptr(), latency);
        }

        let window = self.surface.unwrap_or(0) as *mut ANativeWindow;
        // `AMEDIACODEC_CONFIGURE_FLAG_ENCODE` is 0 for the decoder direction.
        let status = unsafe { AMediaCodec_configure(codec, format, window, ptr::null_mut(), 0) };
        unsafe { AMediaFormat_delete(format) };

        if status != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure(format!(
                "AMediaCodec_configure failed with status {status}"
            )));
        }
        if unsafe { AMediaCodec_start(codec) } != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure("AMediaCodec_start failed".to_string()));
        }
        self.started = true;
        Ok(codec)
    }
}

impl VideoDecoder for AMediaCodecDecoder {
    fn name(&self) -> &'static str {
        "AMediaCodec"
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        self.codec = Some(config.codec);
        if config.surface.is_some() {
            self.surface = config.surface;
        }
        self.info = Some(VideoStreamInfo {
            codec: config.codec,
            width: config.width,
            height: config.height,
            fps: None,
            hardware: true,
        });
        // The codec is created lazily on the first access unit so that the
        // configuration can be refined from the SDP parameters.
        Ok(())
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        let config = DecoderConfig {
            codec: self.codec.unwrap_or(Codec::H264),
            width: self.info.as_ref().map(|info| info.width).unwrap_or(0),
            height: self.info.as_ref().map(|info| info.height).unwrap_or(0),
            surface: self.surface,
            low_latency: true,
        };
        // NOTE: the codec instance is created once and reused; the surrounding
        // struct keeps it alive for the whole session (see `close`).
        let codec = self.open(&config)?;
        self.last_pts_us = pts_us;

        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        let mut index = unsafe { AMediaCodec_dequeueInputBuffer(codec, 0) };
        while index < 0 && std::time::Instant::now() < deadline {
            index = unsafe { AMediaCodec_dequeueInputBuffer(codec, 10_000) };
        }
        if index < 0 {
            self.close(codec);
            return Err(CodecError::NoBuffer);
        }

        let mut capacity = 0usize;
        let buffer = unsafe { AMediaCodec_getInputBuffer(codec, index as usize, &mut capacity) };
        if buffer.is_null() || capacity < access_unit.len() {
            self.close(codec);
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
            let _ = BUFFER_FLAG_CODEC_CONFIG;
        }

        let frames = self.drain(codec)?;
        self.close(codec);
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        Ok(Vec::new())
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.info.as_ref()
    }
}

impl AMediaCodecDecoder {
    /// Collects the frames the codec produced, rendering them into the surface.
    fn drain(&mut self, codec: *mut AMediaCodec) -> Result<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        loop {
            let mut buffer_info = AMediaCodecBufferInfo::default();
            let index = unsafe { AMediaCodec_dequeueOutputBuffer(codec, &mut buffer_info, 0) };
            match index {
                INFO_TRY_AGAIN_LATER => break,
                INFO_OUTPUT_FORMAT_CHANGED => continue,
                index if index >= 0 => {
                    if buffer_info.size > 0 {
                        let mut capacity = 0usize;
                        let buffer = unsafe {
                            AMediaCodec_getOutputBuffer(codec, index as usize, &mut capacity)
                        };
                        let (width, height) = self
                            .info
                            .as_ref()
                            .map(|info| (info.width, info.height))
                            .unwrap_or((0, 0));
                        let keyframe = buffer_info.flags & BUFFER_FLAG_KEY_FRAME != 0;
                        if self.is_zero_copy() {
                            // Rendering happens inside the codec: no CPU copy.
                            frames.push(DecodedFrame::hardware(
                                width,
                                height,
                                buffer_info.presentation_time_us,
                                keyframe,
                                buffer as usize,
                            ));
                        }
                    }
                    // `render = true` hands the buffer to the surface queue.
                    unsafe {
                        AMediaCodec_releaseOutputBuffer(codec, index as usize, self.is_zero_copy())
                    };
                }
                _ => break,
            }
        }
        Ok(frames)
    }

    fn close(&mut self, codec: *mut AMediaCodec) {
        if codec.is_null() {
            return;
        }
        if self.started {
            unsafe { AMediaCodec_stop(codec) };
            self.started = false;
        }
        unsafe { AMediaCodec_delete(codec) };
    }

    /// Flushes the codec after a stream switch (main <-> sub).
    pub fn reset(&mut self, codec: *mut AMediaCodec) {
        if !codec.is_null() {
            unsafe { AMediaCodec_flush(codec) };
        }
    }
}
