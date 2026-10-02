//! Platform video decoding abstraction for XGView.
//!
//! | Target        | Backend                                                    |
//! |---------------|------------------------------------------------------------|
//! | Android       | `AMediaCodec` (NDK media) decoding straight into an         |
//! |               | `ANativeWindow` surface, i.e. zero copy on the GPU path     |
//! | Windows/Linux | FFmpeg (`ffmpeg` feature) or Cisco OpenH264 (`h264`        |
//! |               | feature, on by default), falling back to the null decoder   |
//! |               | used by the UI skeleton when neither is enabled             |
//!
//! MJPEG is the exception that every target shares: it is decoded by the pure
//! Rust backend of [`jpeg`], because JPEG gains nothing from a C library and
//! `AMediaCodec` has no decoder for it at all.
//!
//! The crate never allocates GPU or CPU frames on the UI thread: a decoder is
//! owned by the channel worker task and only pushes [`DecodedFrame`] handles to
//! the renderer.

use std::fmt;

use thiserror::Error;

#[cfg(all(not(target_os = "android"), feature = "ffmpeg"))]
pub mod ffmpeg;
#[cfg(all(not(target_os = "android"), feature = "h264"))]
pub mod h264;
pub mod jpeg;
pub mod null;

#[cfg(target_os = "android")]
pub mod amediacodec;

/// Codecs supported by the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Mjpeg,
    Unknown,
}

impl Codec {
    /// Detects the codec from an SDP `rtpmap` encoding name.
    pub fn from_encoding(name: &str) -> Self {
        match name.trim().to_ascii_uppercase().as_str() {
            "H264" => Codec::H264,
            "H265" | "HEVC" => Codec::H265,
            "JPEG" | "MJPEG" => Codec::Mjpeg,
            _ => Codec::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Mjpeg => "mjpeg",
            Codec::Unknown => "unknown",
        }
    }

    /// MIME type understood by `AMediaCodec`.
    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
            Codec::Mjpeg => "video/mjpeg",
            Codec::Unknown => "video/avc",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pixel layout of a decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// YUV 4:2:0 planar (Y, U, V planes).
    Yuv420Planar,
    /// YUV 4:2:0 semi planar (Y plane + interleaved UV plane).
    Nv12,
    /// 8 bit RGBA, ready for an upload.
    Rgba,
    /// Opaque GPU buffer (AHardwareBuffer / D3D texture); no CPU access.
    Opaque,
}

/// Description of the decoded video stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoStreamInfo {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: Option<u32>,
    /// True when the decoding itself runs on dedicated hardware rather than on
    /// the CPU. The pictures may still be copied back to system memory for the
    /// renderer, so this does not promise a zero copy path.
    pub hardware: bool,
}

impl VideoStreamInfo {
    pub fn resolution(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

/// Configuration handed to a decoder before the first packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderConfig {
    pub codec: Codec,
    /// Expected width, used to size the output buffers.
    pub width: u32,
    pub height: u32,
    /// Decode straight into this surface instead of returning CPU frames
    /// (Android `ANativeWindow`, `None` for CPU decoding).
    pub surface: Option<usize>,
    /// Keep only the newest frame, dropping the ones that pile up.
    pub low_latency: bool,
    /// Run the decoding itself on the GPU. This is a preference and not a
    /// demand: a backend that finds no usable device, or that is offered no
    /// hardware format for the stream, decodes on the CPU instead of failing.
    pub hardware: bool,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            codec: Codec::H264,
            width: 1280,
            height: 720,
            surface: None,
            low_latency: true,
            hardware: true,
        }
    }
}

/// Row alignment of the planes of a [`DecodedFrame`], in bytes.
///
/// A texture upload wants every row to start on a 256 byte boundary, which is
/// the alignment `wgpu` demands of a copy into a texture. The decoders pad
/// their rows to it so the renderer can take the planes as they come, and the
/// reason it is done here rather than there is the copy it saves: a decoder
/// already walks its picture row by row to strip the stride its own frames
/// carry, and one walk can write either layout.
pub const PLANE_ROW_ALIGNMENT: usize = 256;

/// Bytes one row of a plane of `width` samples occupies, padding included.
pub const fn plane_stride(width: usize) -> usize {
    width.div_ceil(PLANE_ROW_ALIGNMENT) * PLANE_ROW_ALIGNMENT
}

/// A decoded frame produced by a decoder.
///
/// On the zero copy path only the metadata is filled in: the pixels stay in the
/// hardware buffer referenced by `buffer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Presentation timestamp in microseconds.
    pub pts_us: i64,
    pub keyframe: bool,
    /// Opaque handle to the GPU buffer (AHardwareBuffer / D3D11 texture).
    pub buffer: Option<usize>,
    /// CPU planes, empty on the zero copy path.
    ///
    /// Luma first, then the interleaved chroma of NV12. Every row is
    /// [`PLANE_ROW_ALIGNMENT`] bytes long whatever the width - see
    /// [`plane_stride`] - so the renderer can hand a plane to the GPU without
    /// walking it again.
    pub planes: Vec<Vec<u8>>,
}

impl DecodedFrame {
    /// Creates a metadata only frame, used by the zero copy backends.
    pub fn hardware(width: u32, height: u32, pts_us: i64, keyframe: bool, buffer: usize) -> Self {
        Self {
            width,
            height,
            format: PixelFormat::Opaque,
            pts_us,
            keyframe,
            buffer: Some(buffer),
            planes: Vec::new(),
        }
    }

    pub fn is_hardware(&self) -> bool {
        self.format == PixelFormat::Opaque
    }
}

/// Decoder errors.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("decoder not supported on this platform: {0}")]
    Unsupported(String),
    #[error("decoder configuration failed: {0}")]
    Configure(String),
    #[error("decoder failure: {0}")]
    Decode(String),
    #[error("no output buffer available")]
    NoBuffer,
}

/// Result alias of the codec crate.
pub type Result<T, E = CodecError> = std::result::Result<T, E>;

/// Decoder backend.
pub trait VideoDecoder: Send {
    /// Backend name, e.g. `AMediaCodec` or `openh264`.
    fn name(&self) -> &'static str;

    /// Prepares the decoder for the negotiated stream.
    fn configure(&mut self, config: &DecoderConfig) -> Result<()>;

    /// Pushes one access unit and returns the frames it produced.
    fn decode(&mut self, access_unit: &[u8], pts_us: i64, keyframe: bool)
        -> Result<Vec<DecodedFrame>>;

    /// Drains the frames still buffered by the decoder.
    fn flush(&mut self) -> Result<Vec<DecodedFrame>>;

    /// Stream description once known.
    fn info(&self) -> Option<&VideoStreamInfo>;
}

/// What the current target can actually do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderCapabilities {
    /// A hardware decoding path exists in this build for this platform. Whether
    /// a given stream ends up on it is only known once decoding starts, and
    /// [`VideoStreamInfo::hardware`] is where that answer is reported.
    pub hardware: bool,
    /// Hardware and software decoding can be chosen between. False where the
    /// only decoder in the build is a hardware one, which is the case on
    /// Android: preferring hardware there is not a setting, it is the only way
    /// the platform decodes, and offering the choice would be a lie.
    pub selectable: bool,
    /// Backend that will be used by [`create_decoder`].
    pub backend: &'static str,
}

/// Reports the capabilities of the current platform.
pub fn capabilities() -> DecoderCapabilities {
    #[cfg(target_os = "android")]
    {
        DecoderCapabilities { hardware: true, selectable: false, backend: "AMediaCodec" }
    }
    #[cfg(all(not(target_os = "android"), feature = "ffmpeg", any(windows, target_os = "linux")))]
    {
        DecoderCapabilities { hardware: true, selectable: true, backend: "ffmpeg" }
    }
    #[cfg(all(
        not(target_os = "android"),
        feature = "ffmpeg",
        not(any(windows, target_os = "linux"))
    ))]
    {
        DecoderCapabilities { hardware: false, selectable: true, backend: "ffmpeg" }
    }
    #[cfg(all(not(target_os = "android"), not(feature = "ffmpeg"), feature = "h264"))]
    {
        DecoderCapabilities { hardware: false, selectable: true, backend: "openh264" }
    }
    #[cfg(all(not(target_os = "android"), not(feature = "ffmpeg"), not(feature = "h264")))]
    {
        DecoderCapabilities { hardware: false, selectable: false, backend: "null" }
    }
}

/// Creates the decoder of the current platform for a codec.
///
/// The codec is named because the backends do not all cover the same ones.
/// MJPEG is answered by the pure Rust decoder whatever the target: JPEG asks
/// for no negotiation and offers nothing to a hardware decoder, and
/// `AMediaCodec` has no decoder for it at all, so one implementation of it
/// beats two.
///
/// The returned decoder is owned by a channel worker; it is never touched by
/// the UI thread.
pub fn create_decoder(codec: Codec) -> Box<dyn VideoDecoder> {
    if codec == Codec::Mjpeg {
        return Box::new(jpeg::MjpegDecoder::new());
    }
    #[cfg(target_os = "android")]
    {
        Box::new(amediacodec::AMediaCodecDecoder::new())
    }
    #[cfg(all(not(target_os = "android"), feature = "ffmpeg"))]
    {
        Box::new(ffmpeg::FfmpegDecoder::new())
    }
    #[cfg(all(not(target_os = "android"), not(feature = "ffmpeg"), feature = "h264"))]
    {
        Box::new(h264::H264Decoder::new())
    }
    #[cfg(all(not(target_os = "android"), not(feature = "ffmpeg"), not(feature = "h264")))]
    {
        Box::new(null::NullDecoder::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_encoding_names() {
        assert_eq!(Codec::from_encoding("H264"), Codec::H264);
        assert_eq!(Codec::from_encoding("hevc"), Codec::H265);
        assert_eq!(Codec::from_encoding("MP4V-ES"), Codec::Unknown);
        assert_eq!(Codec::H265.mime(), "video/hevc");
    }

    #[test]
    fn null_decoder_counts_packets() {
        let mut decoder = null::NullDecoder::new();
        decoder.configure(&DecoderConfig::default()).unwrap();
        let frames = decoder.decode(&[0u8; 32], 0, true).unwrap();
        assert_eq!(frames.len(), 0);
        assert_eq!(decoder.packets(), 1);
        assert_eq!(decoder.name(), "null");
    }

    #[test]
    fn capabilities_describe_the_backend() {
        assert!(!capabilities().backend.is_empty());
    }
}
