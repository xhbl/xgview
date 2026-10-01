//! In process decoder built on FFmpeg's `libavcodec`.
//!
//! The backend is a thin shell around `avcodec_send_packet` /
//! `avcodec_receive_frame`. The access units the pipeline assembles are already
//! Annex-B, which is the packet form `libavcodec` expects, so no demuxer takes
//! part: a picture arrives as one packet and leaves as at most one frame.
//!
//! ```text
//! Annex-B access unit  ->  libavcodec  ->  libswscale  ->  NV12 DecodedFrame
//! ```
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

use std::sync::Once;

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
    Codec, CodecError, DecodedFrame, DecoderConfig, PixelFormat, Result, VideoDecoder, VideoStreamInfo,
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
}

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
        while decoder.receive_frame(&mut picture).is_ok() {
            let mut nv12 = Picture::empty();
            scale_into(&mut self.scaler, &mut self.scaler_key, &picture, &mut nv12)?;
            let Some(frame) = pack(&nv12, pts_us, keyframe) else {
                continue;
            };
            if self.info.is_none() {
                self.info = Some(VideoStreamInfo {
                    codec,
                    width: frame.width,
                    height: frame.height,
                    fps: None,
                    hardware: false,
                });
            }
            self.frames += 1;
            frames.push(frame);
        }
        Ok(frames)
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
        }
    });
    match failure {
        Some(message) => Err(CodecError::Configure(format!("ffmpeg: {message}"))),
        None => Ok(()),
    }
}

/// Builds the converter for the picture at hand when needed, then runs it.
///
/// The output is NV12: the layout a hardware decoder also produces, and the one
/// the renderer takes. The colour matrix is deliberately not applied here. Going
/// from the camera's plane format to NV12 is a range conversion and a copy per
/// row, and the renderer turns the planes into colour once, on the GPU, for every
/// tile at the same time.
fn scale_into(
    scaler: &mut Option<Scaler>,
    key: &mut Option<(Pixel, u32, u32)>,
    picture: &Picture,
    nv12: &mut Picture,
) -> Result<()> {
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
        .run(picture, nv12)
        .map_err(|err| CodecError::Decode(format!("ffmpeg converter: {err}")))
}

/// Copies the two planes of an NV12 picture out of their strided buffers.
fn pack(nv12: &Picture, pts_us: i64, keyframe: bool) -> Option<DecodedFrame> {
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
        pts_us,
        keyframe,
        buffer: None,
        planes: vec![luma, chroma],
    })
}

/// Copies the leading `columns` bytes of `rows` rows out of one strided plane.
///
/// A decoder aligns every row to a stride that is usually wider than the
/// picture, so the rows are copied one at a time and the padding is left behind.
fn copy_plane(frame: &Picture, plane: usize, columns: usize, rows: usize) -> Option<Vec<u8>> {
    let stride = frame.stride(plane);
    let data = frame.data(plane);
    let mut buffer = Vec::with_capacity(columns * rows);
    for row in 0..rows {
        let start = row * stride;
        buffer.extend_from_slice(data.get(start..start + columns)?);
    }
    Some(buffer)
}

impl VideoDecoder for FfmpegDecoder {
    fn name(&self) -> &'static str {
        "ffmpeg"
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

        let decoder = context
            .decoder()
            .video()
            .map_err(|err| CodecError::Configure(format!("ffmpeg: {err}")))?;

        self.codec = config.codec;
        self.decoder = Some(decoder);
        self.scaler = None;
        self.scaler_key = None;
        self.info = None;
        self.frames = 0;
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
        DecoderConfig { codec, width: 0, height: 0, surface: None, low_latency: true }
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
                let samples = frame.width as usize * frame.height as usize;
                assert_eq!(frame.planes[0].len(), samples, "one luma byte per pixel");
                assert_eq!(frame.planes[1].len(), samples / 2, "one chroma pair per two pixels");
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
