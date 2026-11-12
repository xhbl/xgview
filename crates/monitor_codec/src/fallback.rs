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

use std::time::{Duration, Instant};

/// How long after a stream was moved to the CPU the first attempt to move it
/// back is made.
///
/// A device that ran out of hardware sessions frees one when a channel is
/// closed or the grid is made smaller, and a stream that stays on the CPU for
/// the rest of the session never notices - it is only ever offered the platform
/// decoder again if its session reconnects for some unrelated reason. Waiting
/// first keeps a wall that is genuinely full from asking on every key frame.
const UPGRADE_FIRST_WAIT: Duration = Duration::from_secs(30);

/// The longest wait between two attempts, reached by doubling.
const UPGRADE_MAX_WAIT: Duration = Duration::from_secs(300);

/// The decoder tried first, and the one that takes what it refuses.
pub struct Fallback {
    preferred: Box<dyn VideoDecoder>,
    software: Box<dyn VideoDecoder>,
    /// The configuration the stream was set up with, kept because a switch has
    /// to set the software decoder up before it can take the refused unit.
    config: Option<DecoderConfig>,
    /// Set once the switch has been made, so the first is not asked again
    /// until [`Fallback::try_upgrade`] offers the stream back.
    switched: bool,
    /// When the stream was last moved on to the CPU, or last offered back -
    /// whichever happened most recently. Nothing is offered back before
    /// [`Fallback::upgrade_wait`] has passed.
    settled_at: Option<Instant>,
    /// How long to leave the platform decoder alone after an offer it refused.
    /// Doubles with every refusal, so a wall that is full for good asks a
    /// handful of times and then stops.
    upgrade_wait: Duration,
}

impl Fallback {
    pub fn new(preferred: Box<dyn VideoDecoder>, software: Box<dyn VideoDecoder>) -> Self {
        Self {
            preferred,
            software,
            config: None,
            switched: false,
            settled_at: None,
            upgrade_wait: UPGRADE_FIRST_WAIT,
        }
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
        // Given back before the CPU one is set up. A platform decoder left open
        // but idle is still a session the next channel cannot have, and this is
        // the point where this channel stops needing it.
        self.preferred.close();
        self.software.configure(&config)?;
        self.switched = true;
        self.settled_at = Some(Instant::now());
        self.software.decode(access_unit, pts_us, keyframe)
    }

    /// Offers the stream back to the platform decoder after it was refused.
    ///
    /// The refusal that sends a stream to the CPU is usually a device at its
    /// limit of hardware sessions - "already holding as many as it allows",
    /// which is temporary. Closing a channel or shrinking the grid frees one,
    /// and without this the stream that was refused stays on the CPU for as
    /// long as its session lasts, because nothing else ever asks again: a
    /// session reconnects only when something goes wrong with it.
    ///
    /// Called on key frames, so the platform decoder is handed a picture it can
    /// start on rather than one it has to wait past. Attempts are spaced by
    /// [`Fallback::upgrade_wait`], which doubles every time the offer is
    /// refused, and the stream stays on the CPU throughout: a decoder that is
    /// configured and then turns out to be a shell is caught by the platform
    /// side's own starvation check and comes straight back here.
    fn try_upgrade(&mut self) -> bool {
        if !self.switched {
            return false;
        }
        let Some(config) = self.config.clone() else {
            return false;
        };
        let now = Instant::now();
        if let Some(settled) = self.settled_at {
            if now.duration_since(settled) < self.upgrade_wait {
                return false;
            }
        }
        self.settled_at = Some(now);
        match self.preferred.configure(&config) {
            Ok(()) => {
                tracing::info!(
                    target: "xgview::codec",
                    from = self.software.name(),
                    to = self.preferred.name(),
                    "the platform decoder will take the stream again, decoding on it"
                );
                // Released rather than kept: the CPU decoder is rebuilt by a
                // later switch, and holding it open would keep a copy of the
                // stream's state that nobody reads.
                self.software.close();
                self.switched = false;
                self.upgrade_wait = UPGRADE_FIRST_WAIT;
                true
            }
            Err(err) => {
                self.upgrade_wait = (self.upgrade_wait * 2).min(UPGRADE_MAX_WAIT);
                tracing::debug!(
                    target: "xgview::codec",
                    %err,
                    next_try_s = self.upgrade_wait.as_secs(),
                    "the platform decoder will not take the stream back yet"
                );
                false
            }
        }
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

    /// Handed to whichever decoder is doing the work.
    ///
    /// Not the default: a decoder given a surface returns no frames, and the
    /// session that feeds it counts pictures instead - a wrapper that answered
    /// zero here would tell every such session its stream had stalled.
    fn pictures(&self) -> u64 {
        self.active().pictures()
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        self.config = Some(config.clone());
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
                self.settled_at = Some(Instant::now());
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

    /// Hands the platform decoder back when the stream was moved off it.
    ///
    /// Named through the type so that the inherent method - the one with the
    /// state machine in it - is the one that runs.
    fn try_upgrade(&mut self) -> bool {
        Fallback::try_upgrade(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Codec, ColorSpace, PixelFormat};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn config() -> DecoderConfig {
        DecoderConfig { codec: Codec::H264, width: 0, height: 0, hardware: true }
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
    fn a_stream_both_decoders_refuse_is_an_error() {
        let mut decoder = Fallback::new(stub("first", Answer::Refused), stub("second", Answer::Refused));
        decoder.configure(&config()).unwrap();
        assert!(decoder.decode(&[0; 8], 0, true).is_err());
    }

    /// A decoder that records what the wrapper did to it, and whose answer can
    /// be changed while the stream runs - the way a device that frees a
    /// hardware session changes its mind.
    ///
    /// Counters are atomics rather than `Cell`s because a decoder is `Send`.
    struct Spy {
        name: &'static str,
        answer: Arc<AtomicU32>,
        configures: Arc<AtomicU32>,
        closed: Arc<AtomicU32>,
    }

    /// `Answer` as a number, since an atomic cannot hold the enum itself.
    const PICTURE: u32 = 0;
    const REFUSED: u32 = 1;

    fn answer_of(value: u32) -> Answer {
        match value {
            PICTURE => Answer::Picture,
            REFUSED => Answer::Refused,
            _ => Answer::Broken,
        }
    }

    impl VideoDecoder for Spy {
        fn name(&self) -> &'static str {
            self.name
        }

        fn configure(&mut self, _config: &DecoderConfig) -> Result<()> {
            self.configures.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn decode(&mut self, _unit: &[u8], _pts_us: i64, _keyframe: bool) -> Result<Vec<DecodedFrame>> {
            match answer_of(self.answer.load(Ordering::Relaxed)) {
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

        fn close(&mut self) {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A stream that was moved to the CPU, and the parts a test needs to watch.
    struct Spied {
        decoder: Fallback,
        answer: Arc<AtomicU32>,
        configures: Arc<AtomicU32>,
        closed: Arc<AtomicU32>,
    }

    fn spied() -> Spied {
        let answer = Arc::new(AtomicU32::new(REFUSED));
        let configures = Arc::new(AtomicU32::new(0));
        let closed = Arc::new(AtomicU32::new(0));
        let decoder = Fallback::new(
            Box::new(Spy {
                name: "first",
                answer: answer.clone(),
                configures: configures.clone(),
                closed: closed.clone(),
            }),
            stub("second", Answer::Picture),
        );
        Spied { decoder, answer, configures, closed }
    }

    #[test]
    fn the_decoder_left_behind_is_closed_when_the_stream_moves_to_the_cpu() {
        let mut spied = spied();
        spied.decoder.configure(&config()).unwrap();
        assert_eq!(spied.decoder.decode(&[0; 8], 0, true).unwrap().len(), 1);
        assert_eq!(spied.decoder.name(), "second");
        // A platform session held open but idle is one the next channel cannot
        // have, and this channel no longer needs it.
        assert_eq!(spied.closed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_platform_decoder_is_offered_the_stream_again_after_a_cooling_off() {
        let mut spied = spied();
        spied.decoder.configure(&config()).unwrap();
        spied.decoder.decode(&[0; 8], 0, true).unwrap();
        assert_eq!(spied.decoder.name(), "second", "refused, so the cpu decodes it");

        // Not on the key frame that follows: the stream was just moved, and a
        // wall that is genuinely full must not be asked every group.
        assert!(!spied.decoder.try_upgrade());
        assert_eq!(spied.decoder.name(), "second");

        // Once the wait has passed, the next key frame offers it back.
        spied.decoder.upgrade_wait = Duration::ZERO;
        spied.answer.store(PICTURE, Ordering::Relaxed);
        assert!(spied.decoder.try_upgrade());
        assert_eq!(spied.decoder.name(), "first");
        assert_eq!(spied.configures.load(Ordering::Relaxed), 2, "configured again before it is given the stream");
        assert_eq!(spied.decoder.decode(&[0; 8], 0, true).unwrap().len(), 1);
    }

    #[test]
    fn a_taken_offer_back_starts_the_wait_again() {
        let mut spied = spied();
        spied.decoder.configure(&config()).unwrap();
        spied.decoder.decode(&[0; 8], 0, true).unwrap();
        assert_eq!(spied.decoder.upgrade_wait, UPGRADE_FIRST_WAIT);

        spied.decoder.upgrade_wait = Duration::ZERO;
        assert!(spied.decoder.try_upgrade(), "the spy takes the stream back");
        assert_eq!(spied.decoder.upgrade_wait, UPGRADE_FIRST_WAIT, "and the wait starts over");
        assert!(!spied.decoder.try_upgrade(), "so it is not asked again straight away");
    }
}
