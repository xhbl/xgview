//! The compressed audio codecs - AAC above all - through FFmpeg's `libavcodec`.
//!
//! G.711 needs none of this (see [`crate::g711`]); this backend covers what the
//! doorbell and the MSE path send. The RTP stream carries raw AAC, and the
//! decoder is fed each access unit wrapped in an ADTS frame built here, so the
//! SDP's `config=` never has to reach `libavcodec` as extradata - the frame
//! header says the same things.

use ffmpeg_next as ffmpeg;

use ffmpeg::codec::context::Context as CodecContext;
use ffmpeg::codec::decoder::audio::Audio as AudioContext;
use ffmpeg::codec::id::Id;
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg::util::format::sample::{Sample, Type as SampleType};
use ffmpeg::util::frame::audio::Audio as Frame;
use ffmpeg::Packet;

use crate::audio::{AudioCodec, AudioDecoder, AudioStreamInfo};
use crate::{CodecError, Result};

/// A decoder backed by `libavcodec`'s audio decoders.
pub struct FfmpegAudioDecoder {
    info: Option<AudioStreamInfo>,
    decoder: Option<AudioContext>,
    /// Converts the decoder's format (AAC comes back planar float) into packed
    /// s16, at the stream's own rate - the sink does the rate conversion.
    resampler: Option<Resampler>,
    /// The ADTS prefix built from the stream description, reused per frame.
    adts_prefix: [u8; 7],
}

// Safety: the decoder is owned outright by the channel worker that opened it and
// is never touched from two threads at once; the interface only ever receives the
// finished samples. The raw pointers inside the context are the only reason it is
// not `Send` on its own.
unsafe impl Send for FfmpegAudioDecoder {}

impl FfmpegAudioDecoder {
    pub fn new() -> Self {
        Self { info: None, decoder: None, resampler: None, adts_prefix: [0; 7] }
    }
}

impl Default for FfmpegAudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDecoder for FfmpegAudioDecoder {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    fn configure(&mut self, info: &AudioStreamInfo) -> Result<()> {
        ffmpeg::init().map_err(|err| CodecError::Configure(format!("ffmpeg init: {err}")))?;
        let id = match info.codec {
            AudioCodec::Aac => Id::AAC,
            AudioCodec::Opus => Id::OPUS,
            AudioCodec::Pcmu => Id::PCM_MULAW,
            AudioCodec::Pcma => Id::PCM_ALAW,
            AudioCodec::Unknown => {
                return Err(CodecError::Unsupported("unknown audio codec".to_string()))
            }
        };
        let codec = ffmpeg::codec::decoder::find(id)
            .ok_or_else(|| CodecError::Unsupported(format!("no ffmpeg decoder for {}", info.codec)))?;
        let context = CodecContext::new_with_codec(codec);
        // Nothing is set on the context: the ADTS wrapper of every frame carries
        // the rate, channels and profile, so the decoder learns them from the
        // stream itself.
        let decoder = context
            .decoder()
            .audio()
            .map_err(|err| CodecError::Configure(format!("ffmpeg: {err}")))?;
        self.adts_prefix = adts_prefix(info);
        self.decoder = Some(decoder);
        self.resampler = None;
        self.info = Some(info.clone());
        Ok(())
    }

    fn decode(&mut self, access_unit: &[u8], out: &mut Vec<i16>) -> Result<()> {
        let is_aac = self.info.as_ref().map(|info| info.codec) == Some(AudioCodec::Aac);
        // Wrap the raw AAC frame in ADTS: the decoder reads the configuration
        // from the frame header, so the SDP's `config=` need not be extradata.
        let packet = if is_aac {
            let mut frame = Vec::with_capacity(access_unit.len() + 7);
            frame.extend_from_slice(&adts_frame(&self.adts_prefix, access_unit.len()));
            frame.extend_from_slice(access_unit);
            Packet::copy(&frame)
        } else {
            Packet::copy(access_unit)
        };
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| CodecError::Configure("audio decoder not configured".into()))?;
        decoder.send_packet(&packet).map_err(|err| CodecError::Decode(format!("ffmpeg: {err}")))?;
        let mut frame = Frame::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            convert_frame(&mut self.resampler, &frame, out)?;
        }
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<i16>) -> Result<()> {
        let decoder = match self.decoder.as_mut() {
            Some(decoder) => decoder,
            None => return Ok(()),
        };
        decoder.send_eof().map_err(|err| CodecError::Decode(format!("ffmpeg: {err}")))?;
        let mut frame = Frame::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            convert_frame(&mut self.resampler, &frame, out)?;
        }
        Ok(())
    }

    fn info(&self) -> Option<&AudioStreamInfo> {
        self.info.as_ref()
    }
}

/// Hands a frame to the caller as packed s16, converting through libswresample
/// when the decoder produced anything else (AAC comes back planar float).
fn convert_frame(resampler: &mut Option<Resampler>, frame: &Frame, out: &mut Vec<i16>) -> Result<()> {
    let channels = frame.channels().max(1) as usize;
    if matches!(frame.format(), Sample::I16(SampleType::Packed)) {
        let bytes = frame.data(0);
        let len = frame.samples() * channels;
        // Safety: a packed s16 frame is exactly that many samples in data(0).
        let slice = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const i16, len) };
        out.extend_from_slice(slice);
        return Ok(());
    }
    let resampler = match resampler {
        Some(resampler) => resampler,
        slot => {
            let created = Resampler::get(
                frame.format(),
                frame.channel_layout(),
                frame.rate(),
                Sample::I16(SampleType::Packed),
                frame.channel_layout(),
                frame.rate(),
            )
            .map_err(|err| CodecError::Decode(format!("audio resampler: {err}")))?;
            *slot = Some(created);
            slot.as_mut().unwrap()
        }
    };
    let mut resampled = Frame::empty();
    resampler.run(frame, &mut resampled).map_err(|err| CodecError::Decode(format!("audio resample: {err}")))?;
    let bytes = resampled.data(0);
    let len = resampled.samples() * channels;
    // Safety: same packed s16 reasoning as above.
    let slice = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const i16, len) };
    out.extend_from_slice(slice);
    Ok(())
}

/// The 7 byte ADTS prefix for the stream, from its description.
///
/// The fields the decoder reads are the profile, the rate index and the channel
/// count; the frame length is written per frame by [`adts_frame`].
fn adts_prefix(info: &AudioStreamInfo) -> [u8; 7] {
    // The AudioSpecificConfig, when the SDP carried one, names all three.
    let asc = &info.extradata;
    let (profile, rate_index, channels) = if asc.len() >= 2 {
        let object_type = (asc[0] >> 3) & 0x1F;
        let rate_index = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
        let channels = (asc[1] >> 3) & 0x0F;
        (object_type.saturating_sub(1), rate_index, channels)
    } else {
        (1, rate_index(info.sample_rate), info.channels)
    };
    let channels = channels.max(1).min(7);
    [
        0xFF,
        0xF1,
        (profile << 6) | (rate_index << 2) | ((channels >> 2) & 0x01),
        (channels << 6) & 0xC0,
        0,
        0x1F,
        0xFC,
    ]
}

/// The MPEG-4 sampling frequency index of a rate.
fn rate_index(rate: u32) -> u8 {
    match rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        7350 => 12,
        _ => 8,
    }
}

/// Writes the per frame length into the prefix, producing a full ADTS header.
fn adts_frame(prefix: &[u8; 7], payload_len: usize) -> [u8; 7] {
    let frame_length = payload_len + 7;
    let mut frame = *prefix;
    frame[3] |= ((frame_length >> 11) & 0x03) as u8;
    frame[4] = (frame_length >> 3) as u8;
    frame[5] = ((frame_length & 0x07) as u8) << 5 | 0x1F;
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adts_frame_carries_its_length() {
        // LC profile, 16 kHz (index 8), mono: `config=1408` is the doorbell's.
        let mut info = AudioStreamInfo::new(AudioCodec::Aac, 16000, 1);
        info.extradata = vec![0x14, 0x08];
        let prefix = adts_prefix(&info);
        assert_eq!(prefix[2], (1 << 6) | (8 << 2) | 0); // profile 1, rate 8, mono
        assert_eq!(prefix[3] & 0xC0, 0x40);
        let frame = adts_frame(&prefix, 100);
        assert_eq!(frame[0], 0xFF);
        assert_eq!(frame[1], 0xF1);
        let length =
            (((frame[3] as usize) & 0x03) << 11) | ((frame[4] as usize) << 3) | ((frame[5] as usize) >> 5);
        assert_eq!(length, 107);
    }
}