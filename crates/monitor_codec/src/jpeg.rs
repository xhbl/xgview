//! MJPEG decoder built on a pure Rust JPEG decoder.
//!
//! Android is the reason this backend exists. `AMediaCodec` decodes H.264 and
//! HEVC and answers null for `video/mjpeg` on every device seen so far, and the
//! FFmpeg backend that could cover MJPEG is deliberately not part of the
//! Android build: it would put a C toolchain into a cross compilation that
//! needs none today.
//!
//! It is also the only path that produces CPU readable planes on Android. The
//! `AMediaCodec` path renders into an `ANativeWindow`, which leaves nothing for
//! the renderer to take; a decoded JPEG here is the very NV12 the renderer
//! wants, so a tile is drawn from it without anything else changing.
//!
//! One implementation serves every platform rather than two, because JPEG is
//! the one codec that gains nothing from libavcodec: it needs no negotiation,
//! no parameter sets and no hardware, and a picture is independent of the one
//! before it.
//!
//! ```text
//! one JPEG  ->  jpeg-decoder  ->  RGB (full range)  ->  NV12 (limited range)
//! ```
//!
//! A JPEG holds its colour full range; the renderer's shader expands luma from
//! `16..235` and chroma from `128 ± 112`, so the conversion at the end folds the
//! picture back into that range - the same thing libswscale does for the FFmpeg
//! backend, and the reason a picture from here and one from a camera look alike.
//!
//! Each frame is expected to be a self contained JPEG, which is what a camera
//! and the relays seen so far send: every part carries its own tables. An
//! "abbreviated" stream, whose later frames reuse the tables of the first,
//! would need the decoder to be kept between pictures and is not handled.

use jpeg_decoder::{Decoder as JpegDecoder, PixelFormat as JpegPixelFormat};

use crate::{
    Codec, CodecError, DecodedFrame, DecoderConfig, PixelFormat, Result, VideoDecoder,
    VideoStreamInfo,
};

/// Decoder for an MJPEG stream, one JPEG per picture.
#[derive(Debug, Default)]
pub struct MjpegDecoder {
    /// Description of the stream, filled in from the first picture.
    info: Option<VideoStreamInfo>,
    frames: u64,
}

impl MjpegDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pictures decoded so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

impl VideoDecoder for MjpegDecoder {
    fn name(&self) -> &'static str {
        "jpeg"
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        if config.codec != Codec::Mjpeg {
            return Err(CodecError::Unsupported(format!(
                "the jpeg decoder only takes mjpeg, not {}",
                config.codec
            )));
        }
        // Nothing to open and nothing to size: a JPEG carries its own geometry,
        // which is only known once the first one has been read, and `info`
        // reports it from then on.
        self.info = None;
        self.frames = 0;
        Ok(())
    }

    fn decode(&mut self, jpeg: &[u8], pts_us: i64, keyframe: bool) -> Result<Vec<DecodedFrame>> {
        let mut decoder = JpegDecoder::new(jpeg);
        let pixels = decoder
            .decode()
            .map_err(|err| CodecError::Decode(format!("jpeg: {err}")))?;
        let header = decoder
            .info()
            .ok_or_else(|| CodecError::Decode("jpeg carries no frame header".to_string()))?;
        let layout = Layout::of(header.pixel_format).ok_or_else(|| {
            CodecError::Unsupported(format!(
                "jpeg pixel format {:?} is not supported",
                header.pixel_format
            ))
        })?;

        let width = header.width as usize;
        let height = header.height as usize;
        let (luma, chroma) = to_nv12(&pixels, layout, width, height).ok_or_else(|| {
            CodecError::Decode("jpeg pixel buffer is shorter than its header".to_string())
        })?;

        self.frames += 1;
        if self.info.is_none() {
            self.info = Some(VideoStreamInfo {
                codec: Codec::Mjpeg,
                width: width as u32,
                height: height as u32,
                fps: None,
                // The decoding is a pure Rust loop over the picture.
                hardware: false,
            });
        }

        Ok(vec![DecodedFrame {
            width: width as u32,
            height: height as u32,
            format: PixelFormat::Nv12,
            pts_us,
            keyframe,
            buffer: None,
            planes: vec![luma, chroma],
        }])
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        // Nothing is held back: every picture leaves as it is decoded.
        Ok(Vec::new())
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.info.as_ref()
    }
}

/// How the decoder laid the pixels out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Three bytes per pixel, red first.
    Rgb,
    /// One byte per pixel.
    Gray,
}

impl Layout {
    fn of(format: JpegPixelFormat) -> Option<Self> {
        match format {
            JpegPixelFormat::RGB24 => Some(Layout::Rgb),
            JpegPixelFormat::L8 => Some(Layout::Gray),
            // Neither a camera nor a relay sends 16 bit or CMYK.
            _ => None,
        }
    }

    fn stride(self) -> usize {
        match self {
            Layout::Rgb => 3,
            Layout::Gray => 1,
        }
    }
}

/// Converts decoded pixels into the two NV12 planes the renderer takes.
///
/// `None` when the buffer is shorter than the geometry it was read with. A zero
/// sized picture is refused the same way: there is nothing to convert and the
/// renderer has no texture for it.
///
/// Chroma is averaged over each two by two block rather than taken from one
/// pixel of it, which is what keeps a colour edge from stepping by a whole
/// sample.
fn to_nv12(
    pixels: &[u8],
    layout: Layout,
    width: usize,
    height: usize,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if width == 0 || height == 0 {
        return None;
    }
    let stride = layout.stride();
    if pixels.len() < width * height * stride {
        return None;
    }

    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);

    let mut luma_plane = vec![0u8; width * height];
    // Summed per chroma sample as the luma pass runs, then divided once the
    // whole block has been added up. A block at an odd edge is short a pixel,
    // which is why the count is kept rather than assumed to be four.
    let mut chroma_sums = vec![[0u32; 2]; chroma_width * chroma_height];
    let mut chroma_counts = vec![0u32; chroma_width * chroma_height];

    for row in 0..height {
        for column in 0..width {
            let offset = (row * width + column) * stride;
            let (luma, blue, red) = match layout {
                Layout::Rgb => {
                    let red = i32::from(pixels[offset]);
                    let green = i32::from(pixels[offset + 1]);
                    let blue = i32::from(pixels[offset + 2]);
                    (luma_of(red, green, blue), chroma_blue_of(red, green, blue), chroma_red_of(red, green, blue))
                }
                // A grey picture has no colour to carry: luma is the sample and
                // chroma sits at the middle of its range.
                Layout::Gray => {
                    let grey = i32::from(pixels[offset]);
                    (luma_of(grey, grey, grey), 128, 128)
                }
            };
            luma_plane[row * width + column] = clamp8(luma);

            let block = (row / 2) * chroma_width + column / 2;
            chroma_sums[block][0] += u32::from(clamp8(blue));
            chroma_sums[block][1] += u32::from(clamp8(red));
            chroma_counts[block] += 1;
        }
    }

    let mut chroma_plane = vec![0u8; chroma_width * chroma_height * 2];
    for (index, sums) in chroma_sums.iter().enumerate() {
        let count = chroma_counts[index].max(1);
        chroma_plane[index * 2] = (sums[0] / count) as u8;
        chroma_plane[index * 2 + 1] = (sums[1] / count) as u8;
    }
    Some((luma_plane, chroma_plane))
}

/// BT.601 luma at limited range, in 8 bit fixed point.
fn luma_of(red: i32, green: i32, blue: i32) -> i32 {
    ((66 * red + 129 * green + 25 * blue + 128) >> 8) + 16
}

/// BT.601 blue difference at limited range.
fn chroma_blue_of(red: i32, green: i32, blue: i32) -> i32 {
    ((-38 * red - 74 * green + 112 * blue + 128) >> 8) + 128
}

/// BT.601 red difference at limited range.
fn chroma_red_of(red: i32, green: i32, blue: i32) -> i32 {
    ((112 * red - 94 * green - 18 * blue + 128) >> 8) + 128
}

fn clamp8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes a JPEG decoder would have handed over for these pixels.
    fn rgb_bytes(pixels: &[[u8; 3]]) -> Vec<u8> {
        pixels.iter().flatten().copied().collect()
    }

    #[test]
    fn white_and_black_land_on_the_range_the_shader_expands() {
        let white = rgb_bytes(&[[255, 255, 255]; 4]);
        let (luma, chroma) = to_nv12(&white, Layout::Rgb, 2, 2).unwrap();
        // Limited range white is 235, not 255: the shader multiplies by
        // 255/219 after subtracting 16, so full white has to arrive as 235.
        assert_eq!(luma, vec![235u8; 4]);
        assert_eq!(chroma, vec![128u8; 2]);

        let black = rgb_bytes(&[[0, 0, 0]; 4]);
        let (luma, chroma) = to_nv12(&black, Layout::Rgb, 2, 2).unwrap();
        assert_eq!(luma, vec![16u8; 4]);
        assert_eq!(chroma, vec![128u8; 2]);
    }

    #[test]
    fn each_block_keeps_the_colour_it_was_given() {
        // Four by two, so the chroma plane is two blocks of one colour each:
        // the average of a block must not smear one colour into the other.
        let red = [255u8, 0, 0];
        let blue = [0u8, 0, 255];
        let pixels = [red, red, blue, blue, red, red, blue, blue];
        let (_, chroma) = to_nv12(&rgb_bytes(&pixels), Layout::Rgb, 4, 2).unwrap();
        assert_eq!(
            chroma,
            vec![
                clamp8(chroma_blue_of(255, 0, 0)),
                clamp8(chroma_red_of(255, 0, 0)),
                clamp8(chroma_blue_of(0, 0, 255)),
                clamp8(chroma_red_of(0, 0, 255)),
            ]
        );
        // The two colours really do differ, or the assertion above would hold
        // for a conversion that ignored colour altogether.
        assert_ne!(chroma[0], chroma[2]);
    }

    #[test]
    fn a_block_averages_the_pixels_it_covers() {
        // A two by two picture is a single chroma sample, so half red and half
        // blue has to come out as the average of the two, not as either one.
        let pixels = [[255u8, 0, 0], [255, 0, 0], [0, 0, 255], [0, 0, 255]];
        let (_, chroma) = to_nv12(&rgb_bytes(&pixels), Layout::Rgb, 2, 2).unwrap();
        let averages = |first: i32, second: i32| ((first + second) / 2) as u8;
        assert_eq!(
            chroma[0],
            averages(clamp8(chroma_blue_of(255, 0, 0)).into(), clamp8(chroma_blue_of(0, 0, 255)).into())
        );
        assert_eq!(
            chroma[1],
            averages(clamp8(chroma_red_of(255, 0, 0)).into(), clamp8(chroma_red_of(0, 0, 255)).into())
        );
    }

    #[test]
    fn a_grey_picture_gets_a_neutral_chroma_plane() {
        let grey = vec![128u8; 4];
        let (luma, chroma) = to_nv12(&grey, Layout::Gray, 2, 2).unwrap();
        assert_eq!(luma, vec![126u8; 4]);
        assert_eq!(chroma, vec![128u8; 2]);
    }

    #[test]
    fn an_odd_size_still_covers_its_last_partial_block() {
        // Three by three: the chroma plane is two by two, and the blocks of the
        // last row and column average the three pixels they have rather than
        // four, two of which do not exist.
        let pixels = vec![255u8; 3 * 3 * 3];
        let (luma, chroma) = to_nv12(&pixels, Layout::Rgb, 3, 3).unwrap();
        assert_eq!(luma.len(), 9);
        assert_eq!(chroma.len(), 8);
        assert_eq!(chroma, vec![128u8; 8]);
    }

    #[test]
    fn a_short_buffer_is_refused() {
        assert!(to_nv12(&[0u8; 3], Layout::Rgb, 2, 2).is_none());
        assert!(to_nv12(&[], Layout::Gray, 0, 4).is_none());
    }

    #[test]
    fn rejects_a_codec_that_is_not_mjpeg() {
        let mut decoder = MjpegDecoder::new();
        let config = DecoderConfig { codec: Codec::H264, ..Default::default() };
        assert!(decoder.configure(&config).is_err());
    }

    /// The decoder end to end against a real JPEG.
    ///
    /// Point `XGVIEW_TEST_JPEG` at a file produced by, for example,
    ///
    /// ```text
    /// curl --max-time 2 -o frame.jpg 'http://nas:5000/webapi/entry.cgi?...&format=mjpeg'
    /// ```
    ///
    /// It is what a Synology MJPEG endpoint streams, minus the HTTP and the
    /// multipart framing.
    #[test]
    fn decodes_a_real_jpeg() {
        let Ok(path) = std::env::var("XGVIEW_TEST_JPEG") else {
            return;
        };
        let jpeg = std::fs::read(path).expect("read the jpeg");

        let mut decoder = MjpegDecoder::new();
        decoder
            .configure(&DecoderConfig { codec: Codec::Mjpeg, ..Default::default() })
            .expect("configure the decoder");
        let frames = decoder.decode(&jpeg, 0, true).expect("decode the jpeg");
        assert_eq!(frames.len(), 1, "one jpeg is one picture");

        let frame = &frames[0];
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(frame.format, PixelFormat::Nv12);
        assert_eq!(frame.planes.len(), 2);
        let (luma, chroma) = (&frame.planes[0], &frame.planes[1]);
        assert_eq!(luma.len(), (frame.width * frame.height) as usize);
        assert_eq!(chroma.len(), luma.len() / 2);
        // A picture that decoded to nothing but the black level would satisfy
        // every length above and still be wrong.
        let first = luma[0];
        assert!(
            luma.iter().any(|value| *value != first),
            "the luma plane is a flat colour, which a camera picture never is"
        );
        assert_eq!(decoder.frames(), 1);
        assert_eq!(decoder.info().map(|info| info.codec), Some(Codec::Mjpeg));
    }
}
