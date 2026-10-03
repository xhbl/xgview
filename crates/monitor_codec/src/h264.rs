//! In process H.264 software decoder built on Cisco OpenH264.
//!
//! The backend is compiled from source by `openh264-sys2`, so a plain
//! `cargo run` works on a machine that only has a Rust toolchain installed.
//! Decoding is synchronous and bounded: one access unit in, at most one picture
//! out, which keeps the latency of a live view bounded and predictable.
//!
//! ```text
//! Annex-B access unit  ->  OpenH264  ->  RGBA DecodedFrame
//! ```
//!
//! The frames are taken from the decoder directly rather than through the
//! wrapper's `decode`, because a picture the decoder concealed is reported with
//! an error state and the wrapper throws it away together with the error.

use std::ffi::c_void;
use std::ptr::{addr_of_mut, null_mut};
use std::slice;

use openh264::decoder::Decoder as OpenH264Decoder;
use openh264::formats::YUVSource;
use openh264_sys2::{
    DECODER_OPTION_ERROR_CON_IDC, ERROR_CON_SLICE_COPY_CROSS_IDR_FREEZE_RES_CHANGE, SBufferInfo,
};

use crate::{
    plane_stride, Codec, CodecError, ColorSpace, DecodedFrame, DecoderConfig, PixelFormat, Result,
    VideoDecoder, VideoStreamInfo,
};

/// H.264 decoder backed by OpenH264.
#[derive(Default)]
pub struct H264Decoder {
    /// Created by [`VideoDecoder::configure`]: building it needs the negotiated
    /// codec, and `create_decoder` cannot fail.
    decoder: Option<OpenH264Decoder>,
    info: Option<VideoStreamInfo>,
    /// Timestamp of the last access unit, reused when the decoder is drained.
    last_pts_us: i64,
    frames: u64,
}

impl H264Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pictures produced so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

/// Wraps one I420 picture as an NV12 frame.
///
/// The planes are strided: `stride` bytes separate the start of two rows, and
/// only the leading `width` (or `width / 2`) bytes of a row hold pixels. NV12
/// keeps the two chroma halves interleaved rather than apart, so the chroma is
/// repacked and nothing else: the colour matrix belongs to the renderer, which
/// runs it on the GPU for the whole grid at once instead of here, per picture,
/// per channel. The rows of the frame written here are padded the way the
/// upload wants them, so this one walk over the picture is the only one.
fn to_frame(
    (y_plane, u_plane, v_plane): (&[u8], &[u8], &[u8]),
    strides: (usize, usize, usize),
    (width, height): (usize, usize),
    pts_us: i64,
    keyframe: bool,
) -> Option<DecodedFrame> {
    if width == 0 || height == 0 || u_plane.is_empty() || v_plane.is_empty() {
        return None;
    }
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let padded = plane_stride(width);

    let mut luma = vec![0u8; padded * height];
    for row in 0..height {
        let source = &y_plane[row * strides.0..];
        luma[row * padded..row * padded + width].copy_from_slice(&source[..width]);
    }

    // One byte of U followed by one of V covers two luma samples, so a chroma
    // row is as long as a luma row and there are half as many of them.
    let mut chroma = vec![0u8; padded * chroma_height];
    for row in 0..chroma_height {
        let (u_row, v_row) = (&u_plane[row * strides.1..], &v_plane[row * strides.2..]);
        for column in 0..chroma_width {
            chroma[row * padded + column * 2] = u_row[column];
            chroma[row * padded + column * 2 + 1] = v_row[column];
        }
    }

    Some(DecodedFrame {
        width: width as u32,
        height: height as u32,
        format: PixelFormat::Nv12,
        // OpenH264 hands the planes over as they were coded and exposes no
        // colour description to ask, so the renderer's default is reported.
        colorspace: ColorSpace::default(),
        pts_us,
        keyframe,
        buffer: None,
        planes: vec![luma, chroma],
    })
}

/// Turns on the decoder's error concealment.
///
/// A damaged picture is then repaired or repeated instead of failing. The mode
/// matters more than it looks: with concealment disabled the decoder treats any
/// failure as a lost parameter set and refuses every picture that is not a key
/// frame until the next IDR arrives, so a live stream that hiccups once never
/// comes back.
fn conceal_errors(decoder: &mut OpenH264Decoder) -> Result<()> {
    let mut mode = ERROR_CON_SLICE_COPY_CROSS_IDR_FREEZE_RES_CHANGE;
    // Safety: the option reads one `int` through the pointer, which a mutable
    // local provides, and says nothing about the state of the session.
    let status = unsafe {
        decoder
            .raw_api()
            .set_option(DECODER_OPTION_ERROR_CON_IDC, addr_of_mut!(mode).cast::<c_void>())
    };
    if status != 0 {
        return Err(CodecError::Configure(format!(
            "openh264 refused the error concealment mode: {status}"
        )));
    }
    Ok(())
}

/// Decodes one access unit, keeping a picture the decoder concealed.
///
/// Returns `Ok(None)` when the unit held no picture, which is also what the
/// decoder answers while it is waiting for a key frame.
fn decode_picture(
    decoder: &mut OpenH264Decoder,
    access_unit: &[u8],
    pts_us: i64,
    keyframe: bool,
) -> Result<Option<DecodedFrame>> {
    let mut planes = [null_mut::<u8>(); 3];
    let mut buffer = SBufferInfo::default();
    // Safety: the decoder writes at most three plane pointers and one buffer
    // description into the two locals, and the access unit outlives the call.
    let state = unsafe {
        decoder.raw_api().decode_frame_no_delay(
            access_unit.as_ptr(),
            access_unit.len() as i32,
            planes.as_mut_ptr(),
            addr_of_mut!(buffer),
        )
    };
    if buffer.iBufferStatus != 1 || planes[0].is_null() {
        // The unit held no picture. A state other than clean means it was also
        // damaged, which the next key frame repairs.
        if state == 0 {
            return Ok(None);
        }
        return Err(CodecError::Decode(format!("openh264: state {state}")));
    }

    // Safety: the decoder reports the geometry of the picture it just filled,
    // and keeps the planes alive until the next call.
    let frame = unsafe {
        let system = buffer.UsrData.sSystemBuffer;
        let (width, height) = (system.iWidth as usize, system.iHeight as usize);
        let (y_stride, uv_stride) = (system.iStride[0] as usize, system.iStride[1] as usize);
        to_frame(
            (
                slice::from_raw_parts(planes[0], y_stride * height),
                slice::from_raw_parts(planes[1], uv_stride * height.div_ceil(2)),
                slice::from_raw_parts(planes[2], uv_stride * height.div_ceil(2)),
            ),
            (y_stride, uv_stride, uv_stride),
            (width, height),
            pts_us,
            keyframe,
        )
    };

    // A concealed picture is a picture: the state carries the concealment flag,
    // and reporting it as an error would drop the picture with it.
    Ok(frame)
}

impl VideoDecoder for H264Decoder {
    fn name(&self) -> &'static str {
        "openh264"
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        if config.codec != Codec::H264 {
            return Err(CodecError::Unsupported(format!(
                "openh264 decodes H.264 only, got {}",
                config.codec
            )));
        }
        let mut decoder = OpenH264Decoder::new()
            .map_err(|err| CodecError::Configure(format!("openh264: {err}")))?;
        conceal_errors(&mut decoder)?;
        self.decoder = Some(decoder);
        self.info = None;
        self.frames = 0;
        Ok(())
    }

    fn decode(&mut self, access_unit: &[u8], pts_us: i64, keyframe: bool) -> Result<Vec<DecodedFrame>> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| CodecError::Configure("decoder is not configured".to_string()))?;
        self.last_pts_us = pts_us;

        // An access unit is a whole picture, so a damaged one only costs that
        // picture: the next key frame recovers, and the stream never restarts.
        let Some(frame) = decode_picture(decoder, access_unit, pts_us, keyframe)? else {
            return Ok(Vec::new());
        };
        self.info = Some(VideoStreamInfo {
            codec: Codec::H264,
            width: frame.width,
            height: frame.height,
            fps: None,
            hardware: false,
        });
        self.frames += 1;
        Ok(vec![frame])
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(Vec::new());
        };
        let pts_us = self.last_pts_us;
        let remaining = decoder
            .flush_remaining()
            .map_err(|err| CodecError::Decode(format!("openh264: {err}")))?;
        let mut frames = Vec::with_capacity(remaining.len());
        for yuv in &remaining {
            if let Some(frame) =
                to_frame((yuv.y(), yuv.u(), yuv.v()), yuv.strides(), yuv.dimensions(), pts_us, false)
            {
                frames.push(frame);
            }
        }
        self.frames += frames.len() as u64;
        Ok(frames)
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.info.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DecoderConfig {
        DecoderConfig {
            codec: Codec::H264,
            width: 0,
            height: 0,
            low_latency: true,
            hardware: false,
        }
    }

    #[test]
    fn rejects_other_codecs() {
        let mut decoder = H264Decoder::new();
        let mut config = config();
        config.codec = Codec::H265;
        assert!(decoder.configure(&config).is_err());
    }

    #[test]
    fn decoding_before_configuring_is_an_error() {
        let mut decoder = H264Decoder::new();
        assert!(decoder.decode(&[0, 0, 0, 1, 0x65], 0, true).is_err());
    }

    #[test]
    fn ignores_rubbish_without_producing_frames() {
        let mut decoder = H264Decoder::new();
        decoder.configure(&config()).unwrap();
        // Not a bitstream: the picture is dropped, the session survives.
        assert!(decoder.decode(&[0xff; 16], 0, false).is_err() || decoder.frames() == 0);
    }

    /// Decodes a real bitstream when one is provided, e.g.
    ///
    /// ```text
    /// ffmpeg -f lavfi -i testsrc=size=160x120:rate=5:duration=0.4 \
    ///     -c:v libx264 -profile:v baseline -pix_fmt yuv420p -f h264 tiny.h264
    /// set XGVIEW_TEST_H264=tiny.h264
    /// ```
    ///
    /// The fixture is optional so that the suite stays self contained, but the
    /// end to end path (SPS/PPS, IDR, P frames, RGBA conversion) is identical to
    /// the one used against a camera.
    #[test]
    fn decodes_a_real_bitstream() {
        let Ok(path) = std::env::var("XGVIEW_TEST_H264") else {
            return;
        };
        let data = std::fs::read(path).expect("read the bitstream");
        let mut decoder = H264Decoder::new();
        decoder.configure(&config()).unwrap();
        let mut frames = 0;
        let mut size = None;
        for packet in openh264::nal_units(&data) {
            let keyframe = matches!(packet.get(3).map(|byte| byte & 0x1f), Some(5));
            let Ok(produced) = decoder.decode(packet, 0, keyframe) else { continue };
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
}
