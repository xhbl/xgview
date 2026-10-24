//! Where a channel's decoded audio goes, and the little hub that tells a channel
//! whether it is the one being listened to.
//!
//! The output is owned by the interface, not by the core: the desktop opens an
//! audio device through the platform API, and the core only knows the
//! [`AudioOutput`] trait. A channel worker asks the hub two questions before it
//! spends anything on audio - is this the channel the user is listening to, and
//! is the sink open - so a wall of nine grid tiles decodes exactly one
//! microphone, or none while the sound is off.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use monitor_codec::audio::AudioStreamInfo;

/// The index stored when no channel is the audible one.
pub const NO_AUDIO: usize = usize::MAX;

/// The sink a channel hands its decoded audio to.
///
/// Everything is behind `&self` because the interface owns one sink shared by
/// every channel; the audio thread of the platform API is what really drives
/// it, and `push` only copies into a ring buffer.
pub trait AudioOutput: Send + Sync {
    /// Whether the sink is open and sound is wanted right now.
    ///
    /// A channel skips decoding entirely while this is false, so a muted
    /// application costs no audio work at all.
    fn is_active(&self) -> bool;

    /// Points the open stream at a source's rate, channels and codec.
    ///
    /// Called before the samples of a stream are pushed; an implementation
    /// compares with what it already holds and reopens the device only when
    /// something really changed, so calling it per packet is cheap.
    fn configure(&self, info: &AudioStreamInfo);

    /// Hands one block of interleaved `i16` samples to the device.
    fn push(&self, samples: &[i16]);

    /// No channel is audible any more; the device may be released.
    fn stop(&self);
}

/// Shared state that tells each channel whether it is the audible one.
///
/// A channel index is the position in the enabled camera list, the same one the
/// [`crate::scheduler`] uses. The current index changes when the user zooms or
/// turns the sound off; the output is set once, when the interface opens it.
pub struct AudioHub {
    index: AtomicUsize,
    output: Mutex<Option<Arc<dyn AudioOutput>>>,
}

impl AudioHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { index: AtomicUsize::new(NO_AUDIO), output: Mutex::new(None) })
    }

    /// Installs the sink, or clears it. Called once by the interface.
    pub fn set_output(&self, output: Option<Arc<dyn AudioOutput>>) {
        match self.output.lock() {
            Ok(mut slot) => *slot = output,
            Err(poisoned) => *poisoned.into_inner() = output,
        }
    }

    /// The current sink, if one is installed.
    pub fn output(&self) -> Option<Arc<dyn AudioOutput>> {
        match self.output.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Marks `index` as the channel whose audio should play, `None` for silence.
    pub fn set_index(&self, index: Option<usize>) {
        self.index.store(index.unwrap_or(NO_AUDIO), Ordering::Relaxed);
    }

    /// The channel currently set to play, if any.
    pub fn index(&self) -> Option<usize> {
        match self.index.load(Ordering::Relaxed) {
            NO_AUDIO => None,
            index => Some(index),
        }
    }

    /// Whether the channel `index` is the one that should play its audio now.
    pub fn wants(&self, index: usize) -> bool {
        self.index.load(Ordering::Relaxed) == index
    }
}

/// Strips the RFC 3640 AU headers from an AAC RTP payload (`MPEG4-GENERIC`),
/// returning each access unit's byte range within the payload.
///
/// The payload opens with a 16 bit count of header bits, then one header per
/// access unit of `size_length + index_length` bits, then the units back to
/// back. The cameras measured all send `sizelength=13; indexlength=3`, and
/// those are what callers pass; a camera that differs takes its own fields from
/// the SDP's fmtp.
pub fn aac_access_units(
    payload: &[u8],
    size_length: u32,
    index_length: u32,
) -> Vec<std::ops::Range<usize>> {
    if payload.len() < 2 {
        return Vec::new();
    }
    let header_bits = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let header_len = (header_bits + 7) / 8;
    let per_header = (size_length + index_length) as usize;
    if per_header == 0 || header_len == 0 || header_len > payload.len() - 2 {
        return Vec::new();
    }
    let count = header_bits / per_header;
    let headers = &payload[2..2 + header_len];
    let mut ranges = Vec::with_capacity(count);
    let mut data = 2 + header_len;
    for i in 0..count {
        let size = read_bits(headers, per_header * i, size_length as usize);
        if size == 0 || data + size > payload.len() {
            return ranges;
        }
        ranges.push(data..data + size);
        data += size;
    }
    ranges
}

/// Reads `length` big-endian bits starting at bit `start`.
fn read_bits(data: &[u8], start: usize, length: usize) -> usize {
    let mut value = 0usize;
    for bit in start..start + length {
        value = (value << 1) | ((data[bit / 8] >> (7 - bit % 8)) & 1) as usize;
    }
    value
}

/// The RFC 3640 AU-header field widths of an AAC stream, from its SDP `fmtp`.
///
/// The `MPEG4-GENERIC` payload puts a `Size` field and an `Index` field in
/// front of every access unit, and their bit widths are the `sizelength=` and
/// `indexlength=` parameters of the fmtp. The cameras measured all send 13 and
/// 3, and those are the fallback here - a conformant camera names its own, and
/// RFC 3640 does not set a useful default for the size field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacRtpParams {
    pub size_length: u32,
    pub index_length: u32,
}

impl AacRtpParams {
    /// Reads the widths out of an SDP `fmtp` body, e.g.
    /// `config=1408; mode=AAC-hbr; sizelength=13; indexlength=3`.
    pub fn from_fmtp(fmtp: Option<&str>) -> Self {
        let mut params = Self { size_length: 13, index_length: 3 };
        let Some(fmtp) = fmtp else { return params };
        for part in fmtp.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("sizelength=") {
                if let Ok(size) = value.trim().parse() {
                    params.size_length = size;
                }
            } else if let Some(value) = part.strip_prefix("indexlength=") {
                if let Ok(index) = value.trim().parse() {
                    params.index_length = index;
                }
            }
        }
        params
    }
}

impl std::fmt::Debug for AudioHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioHub")
            .field("index", &self.index())
            .field("output", &self.output().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(bool);

    impl AudioOutput for Fake {
        fn is_active(&self) -> bool {
            self.0
        }
        fn configure(&self, _info: &AudioStreamInfo) {}
        fn push(&self, _samples: &[i16]) {}
        fn stop(&self) {}
    }

    #[test]
    fn the_hub_names_one_channel_at_a_time() {
        let hub = AudioHub::new();
        assert_eq!(hub.index(), None);
        assert!(!hub.wants(0));

        hub.set_index(Some(3));
        assert_eq!(hub.index(), Some(3));
        assert!(hub.wants(3));
        assert!(!hub.wants(0));

        hub.set_index(None);
        assert_eq!(hub.index(), None);
        assert!(!hub.wants(3));
    }

    #[test]
    fn the_output_is_installed_and_cleared() {
        let hub = AudioHub::new();
        assert!(hub.output().is_none());
        let output: Arc<dyn AudioOutput> = Arc::new(Fake(true));
        hub.set_output(Some(output.clone()));
        assert!(hub.output().is_some());
        hub.set_output(None);
        assert!(hub.output().is_none());
    }

    #[test]
    fn aac_payload_splits_into_access_units() {
        // Two 2 byte units, each headered by a 13 bit size and a 3 bit index.
        // header_bits = 2 headers x 16 bits = 32.
        let payload = [0x00, 0x20, 0x00, 0x10, 0x00, 0x11, 0xAA, 0xBB, 0xCC, 0xDD];
        let ranges = aac_access_units(&payload, 13, 3);
        assert_eq!(ranges, vec![6..8, 8..10]);
        assert_eq!(&payload[ranges[0].clone()], &[0xAA, 0xBB]);
        assert_eq!(&payload[ranges[1].clone()], &[0xCC, 0xDD]);
    }

    #[test]
    fn aac_payload_that_is_too_short_yields_nothing() {
        assert!(aac_access_units(&[], 13, 3).is_empty());
        assert!(aac_access_units(&[0x00], 13, 3).is_empty());
    }

    #[test]
    fn fmtp_carries_the_rfc3640_field_widths() {
        let fmtp = Some("config=1408; mode=AAC-hbr; sizelength=13; indexlength=3");
        let params = AacRtpParams::from_fmtp(fmtp);
        assert_eq!(
            params,
            AacRtpParams { size_length: 13, index_length: 3 }
        );
    }

    #[test]
    fn fmtp_without_the_widths_falls_back_to_the_measured_ones() {
        let params = AacRtpParams::from_fmtp(Some("config=1408; mode=AAC-hbr"));
        assert_eq!(
            params,
            AacRtpParams { size_length: 13, index_length: 3 }
        );
        assert_eq!(AacRtpParams::from_fmtp(None).size_length, 13);
    }
}