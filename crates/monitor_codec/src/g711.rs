//! G.711 companding, the µ-law and A-law codecs a camera sends as `PCMU` /
//! `PCMA` (static payload types 0 and 8).
//!
//! Both are one byte per sample, so decoding is a byte to sample map and the
//! whole thing is a couple of dozen instructions - which is why it lives here as
//! a table built from the standard algorithm rather than as a call into FFmpeg
//! or the platform decoder. Every target uses it: 8 kHz mono G.711 is the lowest
//! common denominator across the camera wall, and the Android `MediaCodec`
//! coverage for `audio/g711-mlaw` / `audio/g711-alaw` is uneven enough not to be
//! relied on.

/// Decodes one µ-law byte to a linear 16 bit sample.
///
/// The standard North American companding: the byte is complemented, the top
/// bit is the sign, the next three are a segment and the low four a mantissa
/// inside it. `0xFF` and `0x7F` both land on silence, which the tests pin down.
pub fn decode_ulaw(byte: u8) -> i16 {
    // Segment offsets: the base a mantissa is placed on within each of the eight
    // logarithmic segments.
    const EXP_LUT: [i32; 8] = [0, 132, 396, 924, 1980, 4092, 8316, 16764];
    let inverted = !byte;
    let sign = inverted & 0x80;
    let exponent = ((inverted >> 4) & 0x07) as usize;
    let mantissa = (inverted & 0x0F) as i32;
    let sample = EXP_LUT[exponent] + (mantissa << (exponent as i32 + 3));
    if sign != 0 {
        -sample as i16
    } else {
        sample as i16
    }
}

/// Decodes one A-law byte to a linear 16 bit sample.
///
/// The European companding, used by everything that is not North American. It
/// has no exact zero: the two bytes around silence decode to `-8` and `+8`.
pub fn decode_alaw(byte: u8) -> i16 {
    let a_val = byte ^ 0x55;
    // The mantissa occupies the high nibble of the linear value, and the
    // segment decides how far the whole value is shifted up from there.
    let mut t = ((a_val & 0x0F) as i32) << 4;
    let segment = ((a_val & 0x70) >> 4) as i32;
    match segment {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= segment - 1;
        }
    }
    if a_val & 0x80 != 0 {
        t as i16
    } else {
        -t as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulaw_silence_is_zero() {
        assert_eq!(decode_ulaw(0xFF), 0);
        assert_eq!(decode_ulaw(0x7F), 0);
    }

    #[test]
    fn ulaw_extremes_are_symmetric() {
        // The most positive and most negative µ-law samples, symmetric about 0.
        assert_eq!(decode_ulaw(0x80), 32124);
        assert_eq!(decode_ulaw(0x00), -32124);
    }

    #[test]
    fn alaw_silence_is_plus_minus_eight() {
        // A-law has no exact zero; these are the two samples next to it.
        assert_eq!(decode_alaw(0xD5), 8);
        assert_eq!(decode_alaw(0x55), -8);
    }

    #[test]
    fn alaw_full_scale_is_symmetric() {
        // The A-law code toggles the even bits (`^ 0x55`), so the largest byte
        // is not the loudest sample; these two are the ends of the range, and
        // they must stay symmetric about silence.
        assert_eq!(decode_alaw(0xAA), 32256);
        assert_eq!(decode_alaw(0x2A), -32256);
    }

    #[test]
    fn alaw_segments_rise_within_a_segment() {
        // Inside segment 0 the step is 16 per mantissa count, so the raw code
        // (mantissa order is toggled) still walks up in the decoded value.
        assert_eq!(decode_alaw(0xD5), 8);
        assert_eq!(decode_alaw(0xD4), 24);
        assert_eq!(decode_alaw(0xD7), 40);
        assert_eq!(decode_alaw(0xD6), 56);
    }
}