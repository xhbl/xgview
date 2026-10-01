//! Fallback decoder used when no platform decoder backend is available.
//!
//! It consumes the elementary stream and reports statistics so that the rest of
//! the pipeline (scheduler, reconnection, UI states) can be developed and
//! tested without a codec. Frames are intentionally not produced.

use crate::{
    Codec, DecodedFrame, DecoderConfig, Result, VideoDecoder, VideoStreamInfo,
};

/// Statistics of the placeholder decoder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NullStats {
    pub packets: u64,
    pub bytes: u64,
    pub keyframes: u64,
}

/// A decoder that validates the pipeline without decoding anything.
#[derive(Debug, Default)]
pub struct NullDecoder {
    config: Option<DecoderConfig>,
    info: Option<VideoStreamInfo>,
    stats: NullStats,
}

impl NullDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> &NullStats {
        &self.stats
    }

    /// Number of access units received so far.
    pub fn packets(&self) -> u64 {
        self.stats.packets
    }
}

impl VideoDecoder for NullDecoder {
    fn name(&self) -> &'static str {
        "null"
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        self.config = Some(config.clone());
        if config.width > 0 && config.height > 0 {
            self.info = Some(VideoStreamInfo {
                codec: config.codec,
                width: config.width,
                height: config.height,
                fps: None,
                hardware: false,
            });
        }
        Ok(())
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        _pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        self.stats.packets += 1;
        self.stats.bytes += access_unit.len() as u64;
        if keyframe {
            self.stats.keyframes += 1;
        }
        Ok(Vec::new())
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        Ok(Vec::new())
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.info.as_ref()
    }
}

/// Convenience helper: codec of the configured decoder.
impl NullDecoder {
    pub fn codec(&self) -> Option<Codec> {
        self.config.as_ref().map(|config| config.codec)
    }
}
