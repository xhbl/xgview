//! Audio decoding abstraction for XGView.
//!
//! The video side of the pipeline decodes on the platform's decoder, the audio
//! side does not need one: G.711 is a byte to sample map (see [`crate::g711`])
//! and the desktop already links FFmpeg, which carries the AAC and Opus
//! decoders. Android adds `AMediaCodec` for AAC in a later phase.
//!
//! Decoding is push based like the video [`crate::VideoDecoder`]: a channel
//! worker owns the decoder and hands it one access unit at a time - an RTP
//! payload after its depacketiser - and takes interleaved `i16` samples back.
//! The samples are handed to whatever output the UI installed; nothing on the
//! UI thread ever touches the decoder.

use crate::g711;

/// Audio codecs the pipeline knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// G.711 µ-law, the static RTP payload type 0.
    Pcmu,
    /// G.711 A-law, the static RTP payload type 8.
    Pcma,
    /// AAC, carried as `MPEG4-GENERIC` (RFC 3640) with a `config=` in the SDP.
    Aac,
    /// Opus (RFC 7587). Not seen on the cameras measured so far.
    Opus,
    /// Something the caller could not name.
    Unknown,
}

impl AudioCodec {
    /// Detects the codec from an SDP `rtpmap` encoding name or a static payload
    /// type the cameras leave unnamed.
    pub fn from_encoding(name: &str) -> Self {
        match name.trim().to_ascii_uppercase().as_str() {
            "PCMU" | "MULAW" | "G711U" => AudioCodec::Pcmu,
            "PCMA" | "ALAW" | "G711A" => AudioCodec::Pcma,
            "MPEG4-GENERIC" | "AAC" | "MP4A-LATM" => AudioCodec::Aac,
            "OPUS" => AudioCodec::Opus,
            _ => AudioCodec::Unknown,
        }
    }

    /// Names the codec after the RFC 3551 static payload type, for a track whose
    /// SDP carries no `rtpmap`.
    pub fn from_static_payload_type(payload_type: u8) -> Option<Self> {
        match payload_type {
            0 => Some(AudioCodec::Pcmu),
            8 => Some(AudioCodec::Pcma),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioCodec::Pcmu => "pcmu",
            AudioCodec::Pcma => "pcma",
            AudioCodec::Aac => "aac",
            AudioCodec::Opus => "opus",
            AudioCodec::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for AudioCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Description of the audio stream, everything a decoder needs to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamInfo {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    /// Codec setup bytes carried out of band: the AAC AudioSpecificConfig from
    /// the SDP's `config=`, empty for G.711.
    pub extradata: Vec<u8>,
}

impl AudioStreamInfo {
    pub fn new(codec: AudioCodec, sample_rate: u32, channels: u8) -> Self {
        Self { codec, sample_rate, channels, extradata: Vec::new() }
    }

    /// Number of samples in one 20 ms block, the G.711 packetisation.
    pub fn samples_per_20ms(&self) -> usize {
        (self.sample_rate as usize / 50).max(1)
    }
}

/// Audio decoder backend.
pub trait AudioDecoder: Send {
    /// Backend name, e.g. `g711` or `ffmpeg`.
    fn name(&self) -> &'static str;

    /// Prepares the decoder for the negotiated stream.
    fn configure(&mut self, info: &AudioStreamInfo) -> crate::Result<()>;

    /// Decodes one access unit and appends interleaved `i16` samples to `out`.
    ///
    /// The payload is what the depacketiser produced: the raw G.711 bytes, or
    /// the AAC access units once their RFC 3640 headers are stripped. Nothing
    /// of the RTP header is left in it.
    fn decode(&mut self, access_unit: &[u8], out: &mut Vec<i16>) -> crate::Result<()>;

    /// Drains samples the decoder still buffers. A no-op for G.711.
    fn flush(&mut self, out: &mut Vec<i16>) -> crate::Result<()>;

    /// Stream description once configured.
    fn info(&self) -> Option<&AudioStreamInfo>;
}

/// Pure Rust G.711 decoder, used on every target.
pub struct G711Decoder {
    info: Option<AudioStreamInfo>,
    alaw: bool,
}

impl G711Decoder {
    pub fn new() -> Self {
        Self { info: None, alaw: false }
    }
}

impl Default for G711Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDecoder for G711Decoder {
    fn name(&self) -> &'static str {
        "g711"
    }

    fn configure(&mut self, info: &AudioStreamInfo) -> crate::Result<()> {
        self.alaw = matches!(info.codec, AudioCodec::Pcma);
        self.info = Some(info.clone());
        Ok(())
    }

    fn decode(&mut self, access_unit: &[u8], out: &mut Vec<i16>) -> crate::Result<()> {
        out.reserve(access_unit.len());
        if self.alaw {
            out.extend(access_unit.iter().map(|&b| g711::decode_alaw(b)));
        } else {
            out.extend(access_unit.iter().map(|&b| g711::decode_ulaw(b)));
        }
        Ok(())
    }

    fn flush(&mut self, _out: &mut Vec<i16>) -> crate::Result<()> {
        Ok(())
    }

    fn info(&self) -> Option<&AudioStreamInfo> {
        self.info.as_ref()
    }
}

/// A decoder for a codec this build cannot yet take.
///
/// It fails loudly once, when configured, so the caller can drop the audio
/// session and leave the picture alone - the optional and non-fatal rule.
pub struct UnsupportedAudioDecoder {
    codec: AudioCodec,
}

impl UnsupportedAudioDecoder {
    pub fn new(codec: AudioCodec) -> Self {
        Self { codec }
    }
}

impl AudioDecoder for UnsupportedAudioDecoder {
    fn name(&self) -> &'static str {
        "unsupported"
    }

    fn configure(&mut self, _info: &AudioStreamInfo) -> crate::Result<()> {
        Err(crate::CodecError::Unsupported(format!("audio codec {}", self.codec)))
    }

    fn decode(&mut self, _access_unit: &[u8], _out: &mut Vec<i16>) -> crate::Result<()> {
        Ok(())
    }

    fn flush(&mut self, _out: &mut Vec<i16>) -> crate::Result<()> {
        Ok(())
    }

    fn info(&self) -> Option<&AudioStreamInfo> {
        None
    }
}

/// Creates the audio decoder of the current platform for a codec.
///
/// G.711 is answered by the pure Rust decoder on every target. AAC and Opus
/// have no backend yet in this phase - the desktop FFmpeg path comes next - so
/// they come back as an [`UnsupportedAudioDecoder`] rather than a panic.
pub fn create_audio_decoder(codec: AudioCodec) -> Box<dyn AudioDecoder> {
    // The compressed codecs go to FFmpeg where it is linked, and to
    // `AMediaCodec` on Android; either way `configure` fails loudly once and the
    // caller leaves the channel silent if the backend is absent.
    #[cfg(all(not(target_os = "android"), feature = "ffmpeg"))]
    if matches!(codec, AudioCodec::Aac | AudioCodec::Opus) {
        return Box::new(crate::ffmpeg_audio::FfmpegAudioDecoder::new());
    }
    #[cfg(target_os = "android")]
    if codec == AudioCodec::Aac {
        return Box::new(crate::amediacodec::AmcAudioDecoder::new());
    }
    match codec {
        AudioCodec::Pcmu | AudioCodec::Pcma => Box::new(G711Decoder::new()),
        AudioCodec::Aac | AudioCodec::Opus | AudioCodec::Unknown => {
            Box::new(UnsupportedAudioDecoder::new(codec))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_audio_encodings() {
        assert_eq!(AudioCodec::from_encoding("PCMU"), AudioCodec::Pcmu);
        assert_eq!(AudioCodec::from_encoding("pcma"), AudioCodec::Pcma);
        assert_eq!(AudioCodec::from_encoding("MPEG4-GENERIC"), AudioCodec::Aac);
        assert_eq!(AudioCodec::from_encoding("opus"), AudioCodec::Opus);
        assert_eq!(AudioCodec::from_encoding("G722"), AudioCodec::Unknown);
        assert_eq!(AudioCodec::from_static_payload_type(0), Some(AudioCodec::Pcmu));
        assert_eq!(AudioCodec::from_static_payload_type(8), Some(AudioCodec::Pcma));
        assert_eq!(AudioCodec::from_static_payload_type(96), None);
    }

    #[test]
    fn g711_decodes_one_sample_per_byte() {
        let mut decoder = create_audio_decoder(AudioCodec::Pcmu);
        decoder.configure(&AudioStreamInfo::new(AudioCodec::Pcmu, 8000, 1)).unwrap();
        let mut out = Vec::new();
        decoder.decode(&[0xFF, 0x7F, 0x80], &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 32124]);
        assert_eq!(decoder.name(), "g711");
    }

    #[test]
    fn alaw_decoder_uses_the_a_law_table() {
        let mut decoder = create_audio_decoder(AudioCodec::Pcma);
        decoder.configure(&AudioStreamInfo::new(AudioCodec::Pcma, 8000, 1)).unwrap();
        let mut out = Vec::new();
        decoder.decode(&[0xD5, 0x55], &mut out).unwrap();
        assert_eq!(out, vec![8, -8]);
    }

    #[test]
    fn unsupported_codec_fails_at_configure() {
        let mut decoder = create_audio_decoder(AudioCodec::Unknown);
        let err = decoder.configure(&AudioStreamInfo::new(AudioCodec::Unknown, 16000, 1));
        assert!(err.is_err());
    }

    #[test]
    #[cfg(all(not(target_os = "android"), feature = "ffmpeg"))]
    fn aac_decodes_on_the_desktop() {
        // The doorbell's description: AAC-LC, 16 kHz, mono (`config=1408`).
        let mut decoder = create_audio_decoder(AudioCodec::Aac);
        let mut info = AudioStreamInfo::new(AudioCodec::Aac, 16000, 1);
        info.extradata = vec![0x14, 0x08];
        assert!(decoder.configure(&info).is_ok());
    }

    #[test]
    fn samples_per_20ms_matches_the_rate() {
        assert_eq!(AudioStreamInfo::new(AudioCodec::Pcmu, 8000, 1).samples_per_20ms(), 160);
        assert_eq!(AudioStreamInfo::new(AudioCodec::Aac, 16000, 1).samples_per_20ms(), 320);
    }
}