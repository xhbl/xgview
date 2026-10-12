//! A decoder that hands the stream to a second one when the first will not take
//! it.
//!
//! Android is why this exists. The platform decodes H.264 through
//! `AMediaCodec`, and that will not take every stream a camera offers: a profile
//! its decoders do not implement, or a device already holding as many hardware
//! sessions as it allows. A tile that goes dark for want of a decoder is worse
//! than a tile decoded on the CPU, so the platform's decoder is tried first and
//! Cisco OpenH264 takes what it refuses.
//!
//! **The switch is made on a refusal, not on an error.** A decoder that cannot
//! take the stream says so with [`CodecError::Unsupported`] or
//! [`CodecError::Configure`]; a stream that breaks later reports
//! [`CodecError::Decode`], which is the stream's trouble rather than the
//! decoder's and is left to the pipeline's own reconnection. Only the first two
//! are read as "this decoder cannot take it".
//!
//! The unit that was refused is the one the fallback is handed, because it is
//! the one carrying the parameter sets a decoder needs before anything will
//! decode: it is the unit the platform decoder refused to open on, and the
//! software one opens on it instead.

use crate::{CodecError, DecodedFrame, DecoderConfig, Result, VideoDecoder, VideoStreamInfo};

/// The decoder tried first, and the one that takes what it refuses.
pub struct Fallback {
    preferred: Box<dyn VideoDecoder>,
    software: Box<dyn VideoDecoder>,
    /// The configuration the stream was set up with, kept because a switch has
    /// to set the software decoder up before it can take the refused unit.
    config: Option<DecoderConfig>,
    /// Set once the switch has been made, so the first is not asked again.
    switched: bool,
}

impl Fallback {
    pub fn new(preferred: Box<dyn VideoDecoder>, software: Box<dyn VideoDecoder>) -> Self {
        Self { preferred, software, config: None, switched: false }
    }

    /// The decoder in use right now.
    fn active(&self) -> &dyn VideoDecoder {
        if self.switched {
            self.software.as_ref()
        } else {
            self.preferred.as_ref()
        }
    }

    fn active_mut(&mut self) -> &mut dyn VideoDecoder {
        if self.switched {
            self.software.as_mut()
        } else {
            self.preferred.as_mut()
        }
    }

    /// Moves the stream to the software decoder, handing it the unit that was
    /// refused.
    fn switch(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        let Some(config) = self.config.clone() else {
            return Err(CodecError::Configure(
                "the decoder was never configured".to_string(),
            ));
        };
        tracing::info!(
            target: "xgview::codec",
            from = self.preferred.name(),
            to = self.software.name(),
            "the platform decoder refused the stream, decoding on the cpu instead"
        );
        self.software.configure(&config)?;
        self.switched = true;
        self.software.decode(access_unit, pts_us, keyframe)
    }
}

/// Whether an error says "this decoder cannot take the stream" rather than "the
/// stream broke".
fn refused_the_stream(err: &CodecError) -> bool {
    matches!(err, CodecError::Unsupported(_) | CodecError::Configure(_))
}

impl VideoDecoder for Fallback {
    fn name(&self) -> &'static str {
        self.active().name()
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        self.config = Some(config.clone());
        // Asked not to decode in hardware, the software decoder is taken
        // straight away and the platform's is never opened. The pair is a policy
        // over the two decoders, and this is the other side of it: the fallback
        // the setting asks for rather than the one a refusal forces.
        if !config.hardware {
            tracing::info!(
                target: "xgview::codec",
                decoder = self.software.name(),
                "hardware decoding is switched off, decoding on the cpu"
            );
            self.software.configure(config)?;
            self.switched = true;
            return Ok(());
        }
        match self.preferred.configure(config) {
            Ok(()) => Ok(()),
            // A refusal here is one about the codec rather than the stream -
            // the platform has no decoder for it at all - and there is nothing
            // to wait for: the fallback is set up now.
            Err(err) if refused_the_stream(&err) => {
                tracing::info!(
                    target: "xgview::codec",
                    from = self.preferred.name(),
                    to = self.software.name(),
                    "the platform decoder cannot take this codec, using the cpu one"
                );
                self.software.configure(config)?;
                self.switched = true;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        if self.switched {
            return self.software.decode(access_unit, pts_us, keyframe);
        }
        match self.preferred.decode(access_unit, pts_us, keyframe) {
            Ok(frames) => Ok(frames),
            Err(err) if refused_the_stream(&err) => {
                // The unit that was refused is the one that carries the
                // parameter sets, so it is passed on rather than dropped.
                self.switch(access_unit, pts_us, keyframe)
            }
            Err(err) => Err(err),
        }
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        self.active_mut().flush()
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.active().info()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Codec, ColorSpace, PixelFormat};

    fn config() -> DecoderConfig {
        DecoderConfig { codec: Codec::H264, width: 0, height: 0, low_latency: true, hardware: true }
    }

    fn frame() -> DecodedFrame {
        DecodedFrame {
            width: 2,
            height: 2,
            format: PixelFormat::Nv12,
            colorspace: ColorSpace::default(),
            pts_us: 0,
            keyframe: false,
            buffer: None,
            planes: vec![vec![0; 4], vec![0; 4]],
        }
    }

    /// A decoder that answers the same way every time, so a test can say what
    /// each half of the pair does without a real stream.
    struct Stub {
        name: &'static str,
        /// What `configure` answers with, `Ok(())` when it is `None`.
        refuses_at_configure: bool,
        /// What `decode` answers with: a picture, a refusal, or a stream that
        /// broke.
        answer: Answer,
    }

    #[derive(Clone, Copy)]
    enum Answer {
        Picture,
        Refused,
        Broken,
    }

    impl VideoDecoder for Stub {
        fn name(&self) -> &'static str {
            self.name
        }

        fn configure(&mut self, _config: &DecoderConfig) -> Result<()> {
            if self.refuses_at_configure {
                return Err(CodecError::Unsupported("no decoder for this codec".to_string()));
            }
            Ok(())
        }

        fn decode(&mut self, _unit: &[u8], _pts_us: i64, _keyframe: bool) -> Result<Vec<DecodedFrame>> {
            match self.answer {
                Answer::Picture => Ok(vec![frame()]),
                Answer::Refused => Err(CodecError::Unsupported("no".to_string())),
                Answer::Broken => Err(CodecError::Decode("a damaged picture".to_string())),
            }
        }

        fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
            Ok(Vec::new())
        }

        fn info(&self) -> Option<&VideoStreamInfo> {
            None
        }
    }

    fn stub(name: &'static str, answer: Answer) -> Box<dyn VideoDecoder> {
        Box::new(Stub { name, refuses_at_configure: false, answer })
    }

    #[test]
    fn a_stream_the_first_decoder_takes_is_decoded_by_it() {
        let mut decoder = Fallback::new(stub("first", Answer::Picture), stub("second", Answer::Picture));
        decoder.configure(&config()).unwrap();
        assert_eq!(decoder.name(), "first");
        assert_eq!(decoder.decode(&[0; 8], 0, true).unwrap().len(), 1);
        assert_eq!(decoder.name(), "first", "a decoder that takes the stream keeps it");
    }

    #[test]
    fn a_stream_the_first_decoder_refuses_goes_to_the_second() {
        let mut decoder = Fallback::new(stub("first", Answer::Refused), stub("second", Answer::Picture));
        decoder.configure(&config()).unwrap();
        assert_eq!(decoder.name(), "first", "nothing has been decoded yet");
        assert_eq!(decoder.decode(&[0; 8], 0, true).unwrap().len(), 1);
        assert_eq!(decoder.name(), "second");
        // And it stays there: the first is not asked again.
        assert_eq!(decoder.decode(&[0; 8], 0, false).unwrap().len(), 1);
        assert_eq!(decoder.name(), "second");
    }

    #[test]
    fn a_broken_stream_is_not_taken_for_a_refusal() {
        let mut decoder = Fallback::new(stub("first", Answer::Broken), stub("second", Answer::Picture));
        decoder.configure(&config()).unwrap();
        assert!(decoder.decode(&[0; 8], 0, false).is_err());
        assert_eq!(decoder.name(), "first", "a stream that broke is the pipeline's to restart");
    }

    #[test]
    fn a_codec_the_first_decoder_has_no_decoder_for_switches_before_anything_is_decoded() {
        let mut decoder = Fallback::new(
            Box::new(Stub { name: "first", refuses_at_configure: true, answer: Answer::Picture }),
            stub("second", Answer::Picture),
        );
        decoder.configure(&config()).unwrap();
        assert_eq!(decoder.name(), "second");
        assert_eq!(decoder.decode(&[0; 8], 0, true).unwrap().len(), 1);
    }

    #[test]
    fn the_software_decoder_is_taken_when_hardware_is_not_asked_for() {
        let mut decoder = Fallback::new(stub("first", Answer::Picture), stub("second", Answer::Picture));
        let mut asked_for_software = config();
        asked_for_software.hardware = false;
        decoder.configure(&asked_for_software).unwrap();
        assert_eq!(decoder.name(), "second");
        assert_eq!(decoder.decode(&[0; 8], 0, true).unwrap().len(), 1);
        assert_eq!(decoder.name(), "second", "and it stays there");
    }

    #[test]
    fn a_stream_both_decoders_refuse_is_an_error() {
        let mut decoder = Fallback::new(stub("first", Answer::Refused), stub("second", Answer::Refused));
        decoder.configure(&config()).unwrap();
        assert!(decoder.decode(&[0; 8], 0, true).is_err());
    }
}
