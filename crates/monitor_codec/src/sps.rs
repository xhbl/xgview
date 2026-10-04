//! Sequence parameter set fields that matter before a picture is decoded.
//!
//! A decoder is told a size before its first picture, and the size the SDP
//! advertises is not always the one the stream carries: the pipeline prepends
//! the parameter sets an SDP carried to a first picture that arrived without
//! its own, and the size then comes from the SDP rather than the camera. The
//! size is only a hint - the picture decides - but a decoder sized for 640x360
//! that is handed 640x480 makes its buffers twice, and the reported stream size
//! is wrong until the pictures arrive. So the size is taken from the sequence
//! parameter set the stream itself carries whenever one is in hand.
//!
//! The rest of the set is read for a different reason: a hardware decoder
//! screens a stream before it agrees to decode it, and what it screens is the
//! profile, the level, the chroma sampling and the bit depth. A stream it turns
//! down decodes on the CPU instead, and without these fields in the log there
//! is nothing to say why - the two look the same from the outside.

/// Sequence parameter set NAL unit type.
const NAL_SPS: u8 = 7;

/// `profile_idc` of the Baseline profile.
const PROFILE_BASELINE: u8 = 66;

/// `constraint_set1_flag`, the bit of the constraint byte that makes a Baseline
/// stream a *constrained* one.
const CONSTRAINT_SET1: u8 = 0x40;

/// The fields of a sequence parameter set that decide whether a decoder takes
/// the stream at all.
///
/// These are what a hardware decoder screens: a profile or level it does not
/// implement, a chroma sampling other than 4:2:0, more than eight bits per
/// sample, a set carrying scaling matrices. The software decoder accepts what
/// the GPU refuses, so a refused stream simply decodes on the CPU - unless
/// someone can say what the GPU objected to, which is what this is for.
#[derive(Debug, Clone, Copy)]
pub struct SpsInfo {
    /// `profile_idc`.
    pub profile: u8,
    /// `level_idc`; 51 is level 5.1.
    pub level: u8,
    /// `chroma_format_idc`: 1 is 4:2:0, 2 is 4:2:2, 3 is 4:4:4.
    pub chroma: u32,
    /// Bits per luma sample.
    pub bit_depth: u32,
    /// Whether the set carries scaling matrices - the one thing this reader
    /// stops at, and a plausible thing for a hardware decoder to refuse.
    pub scaling_matrices: bool,
    /// Picture size, absent when the fields it is built from are not in the set.
    pub size: Option<(u32, u32)>,
}

/// Reads a sequence parameter set.
///
/// `nal` is the parameter set as it arrived - the NAL header byte followed by
/// its payload, no start code. `None` covers everything this cannot start on:
/// another kind of NAL, or fewer than four bytes.
pub fn sps_info(nal: &[u8]) -> Option<SpsInfo> {
    if nal.len() < 4 || nal[0] & 0x1f != NAL_SPS {
        return None;
    }
    let mut bits = BitReader::new(&nal[1..]);
    let profile = bits.bits(8)? as u8;
    bits.bits(8)?; // constraint flags and reserved bits
    let level = bits.bits(8)? as u8;
    bits.ue()?; // seq_parameter_set_id

    // The High family carries fields the Baseline and Main ones do not.
    let mut chroma = 1;
    let mut bit_depth = 8;
    let mut scaling_matrices = false;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = bits.ue()?;
        if chroma == 3 {
            bits.bits(1)?; // separate_colour_plane_flag
        }
        bit_depth = bits.ue()? + 8; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        scaling_matrices = bits.bits(1)? == 1;
    }

    Some(SpsInfo {
        profile,
        level,
        chroma,
        bit_depth,
        scaling_matrices,
        // Matrices have a count and a length that depend on the chroma format,
        // and walking them buys nothing here: the fields above are what was
        // wanted, and everything below produces only the size.
        size: if scaling_matrices { None } else { size_from(&mut bits) },
    })
}

/// Picture size a sequence parameter set describes, in pixels.
///
/// `None` covers everything that cannot be read: another kind of NAL, a
/// truncated one, a set carrying scaling matrices, or a size of zero.
pub fn picture_size(nal: &[u8]) -> Option<(u32, u32)> {
    sps_info(nal).and_then(|info| info.size)
}

/// The size the fields after the chroma ones describe.
fn size_from(bits: &mut BitReader) -> Option<(u32, u32)> {
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.bits(1)?; // delta_pic_order_always_zero_flag
            bits.se()?; // offset_for_non_ref_pic
            bits.se()?; // offset_for_top_to_bottom_field
            let cycle = bits.ue()?;
            for _ in 0..cycle {
                bits.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    bits.ue()?; // max_num_ref_frames
    bits.bits(1)?; // gaps_in_frame_num_value_allowed_flag

    let width_in_mbs = bits.ue()? + 1;
    let height_in_map_units = bits.ue()? + 1;
    let frame_mbs_only = bits.bits(1)? == 1;
    if !frame_mbs_only {
        bits.bits(1)?; // mb_adaptive_frame_field_flag
    }
    bits.bits(1)?; // direct_8x8_inference_flag

    let (mut crop_left, mut crop_right, mut crop_top, mut crop_bottom) = (0, 0, 0, 0);
    if bits.bits(1)? == 1 {
        crop_left = bits.ue()?;
        crop_right = bits.ue()?;
        crop_top = bits.ue()?;
        crop_bottom = bits.ue()?;
    }

    // The crop unit is the chroma sampling: 4:2:0, which is what every camera
    // here sends, crops one luma sample per unit horizontally and two
    // vertically, the second doubled again for a frame coded as two fields.
    let interlace = 2 - u32::from(frame_mbs_only);
    let width = width_in_mbs * 16 - (crop_left + crop_right) * 2;
    let height = interlace * height_in_map_units * 16 - (crop_top + crop_bottom) * 2 * interlace;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// The last sequence parameter set an Annex-B access unit carries, as it
/// arrived: the NAL header byte and its payload, no start code.
///
/// The bytes come back rather than the fields read out of them because two
/// callers want different things from the same set: [`sps_info`] reads what a
/// decoder screens, and [`constrain_baseline`] rewrites the one flag a
/// hardware decoder takes exception to.
pub fn access_unit_sps(data: &[u8]) -> Option<&[u8]> {
    let mut found = None;
    let mut offset = 0;
    while let Some((start, next)) = nal_at(data, offset) {
        let nal = &data[start..next];
        if !nal.is_empty() && nal[0] & 0x1f == NAL_SPS {
            found = Some(nal);
        }
        offset = next;
    }
    found
}

/// Size the sequence parameter set carried by an Annex-B access unit describes.
pub fn access_unit_size(data: &[u8]) -> Option<(u32, u32)> {
    access_unit_sps(data).and_then(picture_size)
}

/// Whether a sequence parameter set declares Baseline without the constrained
/// flag.
///
/// This is the declaration a hardware decoder refuses while the software one
/// takes. `ff_h264_get_profile` reads profile 66 as `CONSTRAINED_BASELINE` only
/// when `constraint_set1_flag` is set, and the Direct3D 11 / DXVA mode list
/// holds nothing else a profile-66 stream could match - `CONSTRAINED_BASELINE`,
/// `MAIN` and `HIGH` are the whole of it - so a plainly declared Baseline
/// stream finds no decoder to run on and falls to the CPU. See
/// [`constrain_baseline`] for what is done about it.
pub fn needs_constrained_baseline(nal: &[u8]) -> bool {
    // The constraint byte is the third of the NAL and never preceded by an
    // emulation prevention byte: `profile_idc` is 66 here, not zero.
    nal.len() >= 3
        && nal[0] & 0x1f == NAL_SPS
        && nal[1] == PROFILE_BASELINE
        && nal[2] & CONSTRAINT_SET1 == 0
}

/// Declares every Baseline sequence parameter set of an Annex-B access unit as
/// constrained, and reports how many were changed.
///
/// The flag says the stream obeys the constraints of the Main profile - no
/// arbitrary slice order, no flexible macroblock ordering, no redundant slices.
/// A camera that leaves it clear while using none of those features is
/// describing itself as something the hardware has no decoder for, and the flag
/// is the whole of the difference: the pictures themselves are untouched, and a
/// stream that did use those features could not be hardware decoded whatever
/// the flag says, which is why they are not in the mode list to begin with.
///
/// This edits what the decoder is handed, and the caller is expected to say so
/// in its log: a picture decoded from bytes the camera did not send is not
/// something to do quietly.
pub fn constrain_baseline(data: &mut [u8]) -> usize {
    let mut changed = 0;
    let mut offset = 0;
    while let Some((start, next)) = nal_at(data, offset) {
        if needs_constrained_baseline(&data[start..next]) {
            data[start + 2] |= CONSTRAINT_SET1;
            changed += 1;
        }
        offset = next;
    }
    changed
}

/// Start and end of the NAL unit beginning at or after `offset`.
///
/// The returned start is past the start code; the end is where the next start
/// code begins, or the end of the buffer.
fn nal_at(data: &[u8], offset: usize) -> Option<(usize, usize)> {
    let start = (offset..data.len().saturating_sub(3)).find(|&at| {
        data[at..].starts_with(&[0, 0, 1]) || data[at..].starts_with(&[0, 0, 0, 1])
    })?;
    let code = if data[start..].starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
    let begin = start + code;
    let end = (begin..data.len().saturating_sub(3))
        .find(|&at| data[at..].starts_with(&[0, 0, 1]) || data[at..].starts_with(&[0, 0, 0, 1]))
        .unwrap_or(data.len());
    Some((begin, end))
}

/// Reads the bit fields of a parameter set, emulation prevention removed.
struct BitReader {
    bytes: Vec<u8>,
    bit: usize,
}

impl BitReader {
    fn new(payload: &[u8]) -> Self {
        let mut bytes = Vec::with_capacity(payload.len());
        let mut zeros = 0usize;
        for &byte in payload {
            // A 0x03 behind two zero bytes is an emulation prevention byte.
            if zeros >= 2 && byte == 0x03 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            bytes.push(byte);
        }
        Self { bytes, bit: 0 }
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = *self.bytes.get(self.bit / 8)?;
            let shift = 7 - (self.bit % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.bits(1)? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        if leading == 0 {
            return Some(0);
        }
        let rest = self.bits(leading)?;
        Some((1u32 << leading) - 1 + rest)
    }

    /// Signed Exp-Golomb.
    fn se(&mut self) -> Option<i32> {
        let value = self.ue()?;
        let magnitude = ((value + 1) / 2) as i32;
        Some(if value % 2 == 1 { magnitude } else { -magnitude })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Baseline set that leaves the constrained flag clear - the declaration
    /// a hardware decoder has no mode for - is declared constrained in place.
    #[test]
    fn a_plain_baseline_set_is_declared_constrained() {
        // `00 00 00 01` start code, then the NAL: header, profile, constraints.
        let mut unit = vec![0x00, 0x00, 0x00, 0x01, 0x67, PROFILE_BASELINE, 0x00, 0x1e, 0xe9];
        assert!(
            needs_constrained_baseline(&unit[4..]),
            "the set on its own is what the check reads"
        );

        assert_eq!(constrain_baseline(&mut unit), 1);

        assert_eq!(unit[5], PROFILE_BASELINE, "the profile is not what changes");
        assert_eq!(unit[6], CONSTRAINT_SET1, "the flag is");
        assert!(!needs_constrained_baseline(&unit[4..]));
        assert_eq!(constrain_baseline(&mut unit), 0, "and a second pass is a no-op");
    }

    /// A set already declared constrained, and a set of another profile, are
    /// both left as the camera sent them.
    #[test]
    fn other_sets_are_left_alone() {
        let mut already = vec![0x00, 0x00, 0x01, 0x67, PROFILE_BASELINE, CONSTRAINT_SET1, 0x1e];
        assert!(!needs_constrained_baseline(&already[3..]));
        assert_eq!(constrain_baseline(&mut already), 0);
        assert_eq!(already[5], CONSTRAINT_SET1);

        // High profile: the mode list takes it as it stands.
        let mut high = vec![0x00, 0x00, 0x01, 0x67, 100, 0x00, 0x1e];
        assert!(!needs_constrained_baseline(&high[3..]));
        assert_eq!(constrain_baseline(&mut high), 0);
        assert_eq!(high[5], 0x00);
    }
}
