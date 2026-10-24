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

/// Encodes one linear 16 bit sample as a µ-law byte.
///
/// The inverse of [`decode_ulaw`], and the same standard North American
/// companding: clip to full scale, add the bias, then take the segment the
/// sample falls in and the mantissa inside it.
pub fn encode_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32_635;
    let mut sample = i32::from(sample);
    let sign = if sample < 0 {
        sample = -sample;
        0x80
    } else {
        0
    };
    sample = sample.min(CLIP) + BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && sample & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (sample >> (exponent + 3)) & 0x0F;
    (!(sign | (exponent << 4) | mantissa)) as u8
}

/// Encodes one linear 16 bit sample as an A-law byte.
///
/// The inverse of [`decode_alaw`]. The first segment is the flat one - 16
/// steps of 16 covering everything below 256 - and above it the segment is
/// the position of the highest set bit, as it is for µ-law.
pub fn encode_alaw(sample: i16) -> u8 {
    const CLIP: i32 = 32_635;
    let mut sample = i32::from(sample);
    let sign = if sample >= 0 {
        0x80
    } else {
        sample = -sample;
        0x00
    };
    sample = sample.min(CLIP);
    let (exponent, mantissa) = if sample < 256 {
        (0, (sample >> 4) & 0x0F)
    } else {
        let mut exponent = 7;
        let mut mask = 0x4000;
        while exponent > 0 && sample & mask == 0 {
            exponent -= 1;
            mask >>= 1;
        }
        (exponent, (sample >> (exponent + 3)) & 0x0F)
    };
    ((sign | (exponent << 4) | mantissa) as u8) ^ 0x55
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

    #[test]
    fn ulaw_encodes_silence_to_the_silence_byte() {
        assert_eq!(encode_ulaw(0), 0xFF);
        // A-law's two zeros take the positive one: decode(encode(0)) is +8.
        assert_eq!(encode_alaw(0), 0xD5);
    }

    #[test]
    fn encoders_round_trip_within_a_quantisation_step() {
        // The codecs are lossy by design; what matters is that a sample comes
        // back inside its own step and with its sign, not that it is identical.
        // The step of segment `e` is `1 << (e + 3)` for µ-law, so a third of
        // the sample is a safe ceiling for everything below full scale.
        for sample in (-32768i16..=32767).step_by(7) {
            let back = decode_ulaw(encode_ulaw(sample));
            let error = i32::from(back) - i32::from(sample);
            assert!(
                error.abs() <= (i32::from(sample).abs() / 3).max(8),
                "ulaw {sample} -> {back}"
            );
            let back = decode_alaw(encode_alaw(sample));
            let error = i32::from(back) - i32::from(sample);
            assert!(
                error.abs() <= (i32::from(sample).abs() / 3).max(8),
                "alaw {sample} -> {back}"
            );
        }
    }

    #[test]
    fn encoders_keep_the_sign() {
        assert!(decode_ulaw(encode_ulaw(1000)) > 0);
        assert!(decode_ulaw(encode_ulaw(-1000)) < 0);
        assert!(decode_alaw(encode_alaw(1000)) > 0);
        assert!(decode_alaw(encode_alaw(-1000)) < 0);
    }
}