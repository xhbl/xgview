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
//!
//! H.264 and H.265 both have a sequence parameter set to read; the two put
//! their fields in different places, so the readers are kept apart and named
//! after the codec - [`picture_size`] against [`hevc_picture_size`].

use crate::Codec;

/// Sequence parameter set NAL unit type.
const NAL_SPS: u8 = 7;
/// Picture parameter set NAL unit type.
const NAL_PPS: u8 = 8;

/// H.265's parameter sets: video, sequence and picture, in the order a decoder
/// reads them.
const HEVC_NAL_VPS: u8 = 32;
const HEVC_NAL_SPS: u8 = 33;
const HEVC_NAL_PPS: u8 = 34;

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

/// Type of a NAL unit taken from the first byte of an H.265 header.
///
/// The header is two bytes, and the type sits in the six bits between the
/// forbidden bit and the layer id.
fn hevc_nal_type(header: u8) -> u8 {
    (header >> 1) & 0x3f
}

/// Picture size an H.265 sequence parameter set describes, in pixels.
///
/// `nal` is the set as it arrived - the two byte NAL header and its payload, no
/// start code. `None` covers everything this cannot read: another kind of NAL,
/// a truncated set, or a size of zero.
///
/// This is what Android needs and the desktop does not: FFmpeg reads the size
/// from the bitstream, while `AMediaCodec` is built to a size before its first
/// picture and turns every one down when that size is not the stream's
/// (`-30003`, DevMemo §9).
pub fn hevc_picture_size(nal: &[u8]) -> Option<(u32, u32)> {
    if nal.len() < 3 || hevc_nal_type(nal[0]) != HEVC_NAL_SPS {
        return None;
    }
    // Past the two byte header: sps_video_parameter_set_id (4 bits),
    // sps_max_sub_layers_minus1 (3) and sps_temporal_id_nesting_flag (1).
    let mut bits = BitReader::new(&nal[2..]);
    bits.bits(4)?;
    let max_sub_layers_minus1 = bits.bits(3)?;
    bits.bits(1)?;
    skip_profile_tier_level(&mut bits, true, max_sub_layers_minus1)?;

    bits.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = bits.ue()?;
    if chroma_format_idc == 3 {
        bits.bits(1)?; // separate_colour_plane_flag
    }
    let coded_width = bits.ue()?; // pic_width_in_luma_samples
    let coded_height = bits.ue()?; // pic_height_in_luma_samples

    // The conformance window trims the coded picture down to what is shown.
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if bits.bits(1)? == 1 {
        left = bits.ue()?;
        right = bits.ue()?;
        top = bits.ue()?;
        bottom = bits.ue()?;
    }

    // The offsets are counted in chroma samples, so they scale by the chroma
    // sampling: 4:2:0 crops two luma samples per unit, 4:2:2 two across and one
    // down, 4:4:4 one and one. Monochrome has no window to apply.
    let (sub_width, sub_height) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = coded_width.checked_sub(sub_width * (left + right))?;
    let height = coded_height.checked_sub(sub_height * (top + bottom))?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// Steps over an H.265 `profile_tier_level` structure.
///
/// A size reader has no use for any of it, but it sits between the header and
/// the fields that do matter, so it has to be stepped over exactly - and its
/// length depends on how many sub-layers the set declares.
fn skip_profile_tier_level(
    bits: &mut BitReader,
    profile_present: bool,
    max_sub_layers_minus1: u32,
) -> Option<()> {
    if profile_present {
        // general_profile_space(2) general_tier_flag(1) general_profile_idc(5),
        // general_profile_compatibility_flag[32], four source flags, 44
        // reserved bits, then general_level_idc(8) below.
        bits.skip(2)?;
        bits.skip(1)?;
        bits.skip(5)?;
        bits.skip(32)?;
        bits.skip(4)?; // the four source / constraint flags
        bits.skip(44)?; // the reserved bits
    }
    bits.skip(8)?; // general_level_idc

    // Every sub-layer's two present-flags come first, all of them, and only
    // then the layers the flags describe.
    let mut profile_flags = [false; 8];
    let mut level_flags = [false; 8];
    for layer in 0..max_sub_layers_minus1 as usize {
        profile_flags[layer] = bits.bits(1)? == 1;
    }
    for layer in 0..max_sub_layers_minus1 as usize {
        level_flags[layer] = bits.bits(1)? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        // reserved_zero_2bits for the layers the set does not describe.
        for _ in max_sub_layers_minus1..8 {
            bits.skip(2)?;
        }
    }
    for layer in 0..max_sub_layers_minus1 as usize {
        if profile_flags[layer] {
            bits.skip(88)?; // sub_layer_profile_*
        }
        if level_flags[layer] {
            bits.skip(8)?; // sub_layer_level_idc
        }
    }
    Some(())
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

/// The parameter sets an Annex-B access unit carries, in the order a decoder
/// reads them, with the picture size the sequence parameter set describes.
///
/// H.264 carries two - SPS then PPS - and H.265 three, a VPS ahead of its SPS
/// and PPS. That is the order `AMediaCodec` is configured with (`csd-0`,
/// `csd-1`, `csd-2`), and each set comes back as it arrived in the stream: the
/// NAL header and its payload, no start code and no trailing zero bytes, which
/// is the form the platform decoders take.
///
/// The pipeline prepends these same sets to every picture, so the first key
/// frame carries them even on a camera whose SDP advertised none.
pub fn decode_parameter_sets(
    codec: Codec,
    access_unit: &[u8],
) -> (Vec<Vec<u8>>, Option<(u32, u32)>) {
    let hevc = codec == Codec::H265;
    let nal_type: fn(u8) -> u8 = if hevc { hevc_nal_type } else { |byte| byte & 0x1f };
    let (vps_type, sps_type, pps_type) = if hevc {
        (Some(HEVC_NAL_VPS), HEVC_NAL_SPS, HEVC_NAL_PPS)
    } else {
        (None, NAL_SPS, NAL_PPS)
    };

    let (mut vps, mut sps, mut pps) = (None, None, None);
    let mut offset = 0;
    while let Some((start, next)) = nal_at(access_unit, offset) {
        let mut nal = &access_unit[start..next];
        // A four byte start code leaves a zero byte on the end of the set
        // before it, which is not part of the set.
        while nal.last() == Some(&0) {
            nal = &nal[..nal.len() - 1];
        }
        if let Some(&first) = nal.first() {
            let kind = nal_type(first);
            if Some(kind) == vps_type {
                vps = Some(nal);
            } else if kind == sps_type {
                sps = Some(nal);
            } else if kind == pps_type {
                pps = Some(nal);
            }
        }
        offset = next;
    }

    let size = sps.and_then(|set| if hevc { hevc_picture_size(set) } else { picture_size(set) });
    let sets = [vps, sps, pps]
        .into_iter()
        .flatten()
        .map(|set| set.to_vec())
        .collect();
    (sets, size)
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

    /// Steps over `count` bits without reading them.
    ///
    /// Used for the fields only in the way of the ones being read - a run wider
    /// than a `u32` among them. The end has to lie inside the set, so a
    /// truncated one fails here rather than reading on.
    fn skip(&mut self, count: u32) -> Option<()> {
        let end = self.bit.checked_add(count as usize)?;
        if end > 0 {
            self.bytes.get((end - 1) / 8)?;
        }
        self.bit = end;
        Some(())
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

    /// The three parameter sets the H.265 camera at 192.168.17.87 advertised in
    /// its SDP (`sprop-vps` / `sprop-sps` / `sprop-pps`), base64 decoded.
    const HEVC_VPS: [u8; 24] = [
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x78, 0x99, 0xc0, 0x90,
    ];
    const HEVC_SPS: [u8; 62] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x78, 0xa0, 0x03, 0xc0, 0x80, 0x11, 0x07, 0xcb, 0x96, 0x67, 0x4a, 0x42, 0x11,
        0x91, 0xbf, 0x50, 0xc0, 0x5a, 0x80, 0x80, 0x80, 0x82, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00,
        0x00, 0x03, 0x00, 0x3c, 0xc0, 0x0b, 0xbc, 0xa2, 0x00, 0x01, 0x6e, 0x36, 0x00, 0x00, 0x7a,
        0x12, 0x02,
    ];
    const HEVC_PPS: [u8; 7] = [0x44, 0x01, 0xc0, 0xa5, 0x7c, 0x0c, 0xc9];

    /// The camera's own set describes a 1920x1080 picture: 1920x1088 coded, with
    /// the conformance window trimming the last eight rows. The size is the
    /// point of it - `AMediaCodec` turns down every picture whose size is not
    /// this before it is even configured.
    #[test]
    fn reads_the_size_out_of_an_h265_sequence_parameter_set() {
        assert_eq!(hevc_picture_size(&HEVC_SPS), Some((1920, 1080)));
    }

    /// A video or picture parameter set is not a sequence parameter set, and a
    /// set with nothing in it says nothing.
    #[test]
    fn an_h265_set_of_another_kind_has_no_size() {
        assert_eq!(hevc_picture_size(&HEVC_VPS), None);
        assert_eq!(hevc_picture_size(&HEVC_PPS), None);
        assert_eq!(hevc_picture_size(&HEVC_SPS[..6]), None, "truncated");
        assert_eq!(hevc_picture_size(&[]), None);
    }

    /// The sets come back in the order a decoder reads them, whatever order the
    /// access unit carried them in, with the size from the sequence one.
    #[test]
    fn an_h265_access_unit_yields_vps_sps_pps() {
        // A picture whose sets are listed pps, sps, vps - the order the
        // camera's SDP used, and the reverse of what the decoder wants.
        let mut unit = Vec::new();
        for set in [&HEVC_PPS[..], &HEVC_SPS[..], &HEVC_VPS[..], &[0x26, 0x01, 0xaa]] {
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(set);
        }
        let (sets, size) = decode_parameter_sets(Codec::H265, &unit);
        assert_eq!(sets, vec![HEVC_VPS.to_vec(), HEVC_SPS.to_vec(), HEVC_PPS.to_vec()]);
        assert_eq!(size, Some((1920, 1080)));
    }

    /// H.264 still yields its two, SPS ahead of PPS, with its own size - the
    /// sets the rtsp tests carry, which describe 1280x720.
    #[test]
    fn an_h264_access_unit_yields_sps_pps() {
        let sps = [
            0x67, 0x42, 0x00, 0x29, 0xe2, 0x90, 0x0a, 0x00, 0xb7, 0x60, 0x2d, 0xc0, 0x40, 0x40,
            0x69, 0x07, 0x89, 0x11, 0x50,
        ];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let mut unit = Vec::new();
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(&sps);
        unit.extend_from_slice(&[0, 0, 1]);
        unit.extend_from_slice(&pps);
        let (sets, size) = decode_parameter_sets(Codec::H264, &unit);
        assert_eq!(sets, vec![sps.to_vec(), pps.to_vec()], "SPS then PPS");
        assert_eq!(size, Some((1280, 720)));
    }
}
