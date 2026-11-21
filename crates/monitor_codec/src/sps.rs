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

/// `profile_idc` values of the High family, which carry the chroma and bit
/// depth fields the Baseline and Main ones omit.
fn is_high_profile(profile: u8) -> bool {
    matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
}

/// What each H.264 level allows: its `level_idc`, the largest picture in
/// macroblocks, the most macroblocks a second, and the largest reference
/// buffer in macroblocks (`MaxDpbMbs`, which is not the same number as the
/// picture size). Table A-1, less level 1b, whose id follows no pattern and
/// which no camera here is small enough for.
const LEVELS: [(u8, u32, u32, u32); 15] = [
    (10, 99, 1485, 396),
    (11, 396, 3000, 900),
    (12, 396, 6000, 2376),
    (13, 396, 11880, 2376),
    (20, 396, 11880, 2376),
    (21, 792, 19800, 4752),
    (22, 1620, 20250, 8100),
    (30, 1620, 40500, 8100),
    (31, 3600, 108000, 18000),
    (32, 5120, 216000, 20480),
    (40, 8192, 245760, 32768),
    (41, 8192, 245760, 32768),
    (42, 8704, 522240, 34816),
    (50, 22080, 589824, 110400),
    (51, 36864, 983040, 184320),
];

/// The lowest level whose picture size and macroblock rate cover `size`.
///
/// Thirty frames a second is the ceiling a camera of this size is expected to
/// reach, and rounding up is harmless: the level only has to be one a decoder
/// will take the stream at. A stream faster than that - a 720p fifty or a 1080p
/// sixty - is therefore declared one level under what its macroblock rate would
/// ask for, which is a limit no decoder here checks and is the smaller error by
/// far next to the buffers a 4K declaration asks a decoder to set aside.
fn level_for(size: (u32, u32)) -> u8 {
    let mbs = size.0.div_ceil(16) * size.1.div_ceil(16);
    let rate = mbs * 30;
    LEVELS
        .iter()
        .find(|(_, frames, per_second, _)| *frames >= mbs && *per_second >= rate)
        .map_or(52, |(level, _, _, _)| *level)
}

/// Where a level sits in the table, which is what "two levels out" is counted
/// in.
fn level_rank(level: u8) -> Option<usize> {
    LEVELS.iter().position(|(id, _, _, _)| *id == level)
}

/// How many pictures a level lets a decoder keep as references.
///
/// `MaxDpbFrames` is `MaxDpbMbs` over the picture's macroblocks, capped at
/// sixteen. A declared `max_dec_frame_buffering` above it is a set that
/// contradicts its own level, and a decoder strict enough to check would
/// refuse the stream - so what a rewrite declares is held under it.
fn max_dpb_frames(level: u8, size: (u32, u32)) -> u32 {
    let mbs = size.0.div_ceil(16) * size.1.div_ceil(16);
    let Some(rank) = level_rank(level) else {
        return 16;
    };
    (LEVELS[rank].3 / mbs.max(1)).clamp(1, 16)
}

/// A `level_idc` two whole levels above what its pictures need, with the one
/// they do need.
///
/// Level 5.1 is a 4K declaration: 36864 macroblocks, a sixteen picture
/// reference buffer, a 240 Mbps channel. The front door camera at
/// 192.168.27.40 declares it for 640x480 - 1200 macroblocks, one reference
/// picture, 650 kbps - and a hardware decoder that sizes its output buffers
/// from the declaration holds pictures back before it releases the first,
/// which the wall shows as a tile moving in steps of four or five (see
/// `priv/docs/foscam_probe.md` §12). The pictures are the same either way; the
/// level is a statement about what a decoder must be able to do, and this one
/// asks for far more than the stream does.
///
/// Two levels, not one: a camera that rounds up to the next level is inside
/// what the specification allows and is not a fault to correct. This is also
/// why the healthy majority of streams is never touched.
pub fn overstated_level(nal: &[u8]) -> Option<(u8, u8)> {
    let declared = *nal.get(3)?;
    if nal[0] & 0x1f != NAL_SPS {
        return None;
    }
    let needed = level_for(picture_size(nal)?);
    if level_rank(declared)? < level_rank(needed)? + 2 {
        return None;
    }
    Some((declared, needed))
}

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
    if is_high_profile(profile) {
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
    size_and_refs(bits).map(|(size, _)| size)
}

/// The size those fields describe, and the reference buffer they ask for.
///
/// `max_num_ref_frames` comes back because a VUI has to name a
/// `max_dec_frame_buffering` no smaller than it, and this is where it is read.
fn size_and_refs(bits: &mut BitReader) -> Option<((u32, u32), u32)> {
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
    let refs = bits.ue()?; // max_num_ref_frames
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
    Some(((width, height), refs))
}

/// What a sequence parameter set's `bitstream_restriction_flag` block declares
/// a decoder may do, which is what decides whether it holds pictures back and
/// how far it may let a vector reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restriction {
    /// `log2_max_mv_length_horizontal`: the base-2 logarithm of the longest
    /// horizontal vector, with sixteen meaning "no limit".
    pub mv_horizontal: u32,
    /// The same vertically.
    pub mv_vertical: u32,
    /// `max_num_reorder_frames`: how many pictures the decoder may hold before
    /// releasing one. Zero is a stream that decodes in display order.
    pub reorder: u32,
    /// `max_dec_frame_buffering`: how many pictures it may keep as references.
    pub buffering: u32,
}

/// The restriction block of a sequence parameter set, if it carries one.
///
/// `None` covers a set this cannot walk, one with no VUI, and one whose
/// `bitstream_restriction_flag` is clear: nothing is declared and there is
/// nothing to read.
pub fn restriction(nal: &[u8]) -> Option<Restriction> {
    restriction_in_vui(&mut walk(nal)?.bits)
}

/// A sequence parameter set walked up to its `vui_parameters_present_flag`,
/// and what it said on the way - the reference buffer it asks for, the level it
/// declares, and the picture its macroblocks describe.
///
/// The three are read together because a rewrite needs all of them: a buffer
/// the level cannot hold is not a declaration a strict decoder will take.
struct Walk {
    bits: BitReader,
    refs: u32,
    level: u8,
    size: (u32, u32),
}

fn walk(nal: &[u8]) -> Option<Walk> {
    if nal.len() < 4 || nal[0] & 0x1f != NAL_SPS {
        return None;
    }
    let mut bits = BitReader::new(&nal[1..]);
    let profile = bits.bits(8)? as u8;
    bits.bits(8)?; // constraint flags and reserved bits
    let level = bits.bits(8)? as u8;
    bits.ue()?; // seq_parameter_set_id
    if is_high_profile(profile) {
        let chroma = bits.ue()?;
        if chroma == 3 {
            bits.bits(1)?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.bits(1)? == 1 {
            // Scaling matrices are a list per size, and walking them buys
            // nothing here: a set that carries them keeps the set it sent.
            return None;
        }
    }
    // The same walk the size reader does, and for the same reason: the fields
    // between are in the way and their lengths are known.
    let (size, refs) = size_and_refs(&mut bits)?;
    Some(Walk { bits, refs, level, size })
}

/// The restriction block of a VUI.
///
/// `bits` is expected to sit on `vui_parameters_present_flag`, which is where
/// [`size_from`] leaves a reader.
fn restriction_in_vui(bits: &mut BitReader) -> Option<Restriction> {
    if bits.bits(1)? == 0 {
        return None; // no VUI at all
    }
    if bits.bits(1)? == 1 {
        // aspect_ratio_info_present_flag
        if bits.bits(8)? == 255 {
            // Extended_SAR carries the ratio itself
            bits.skip(16)?;
            bits.skip(16)?;
        }
    }
    if bits.bits(1)? == 1 {
        bits.bits(1)?; // overscan_appropriate_flag
    }
    if bits.bits(1)? == 1 {
        // video_signal_type_present_flag
        bits.skip(3)?; // video_format
        bits.bits(1)?; // video_full_range_flag
        if bits.bits(1)? == 1 {
            bits.skip(24)?; // colour_primaries, transfer, matrix
        }
    }
    if bits.bits(1)? == 1 {
        // chroma_loc_info_present_flag
        bits.ue()?;
        bits.ue()?;
    }
    if bits.bits(1)? == 1 {
        // timing_info_present_flag
        bits.skip(32)?; // num_units_in_tick
        bits.skip(32)?; // time_scale
        bits.bits(1)?; // fixed_frame_rate_flag
    }
    // An HRD is a count and then a table per count, and none of the streams
    // here carry one. Rather than walk it to reach a field this reader wants,
    // the set keeps what the camera sent - which is the safe direction: the
    // stream is decoded as it always was.
    if bits.bits(1)? == 1 {
        return None; // nal_hrd_parameters_present_flag
    }
    if bits.bits(1)? == 1 {
        return None; // vcl_hrd_parameters_present_flag
    }
    bits.bits(1)?; // pic_struct_present_flag
    if bits.bits(1)? == 0 {
        return None; // bitstream_restriction_flag, and with it the field
    }
    bits.bits(1)?; // motion_vectors_over_pic_boundaries_flag
    bits.ue()?; // max_bytes_per_pic_denom
    bits.ue()?; // max_bits_per_mb_denom
    let mv_horizontal = bits.ue()?; // log2_max_mv_length_horizontal
    let mv_vertical = bits.ue()?; // log2_max_mv_length_vertical
    let reorder = bits.ue()?; // max_num_reorder_frames
    let buffering = bits.ue()?; // max_dec_frame_buffering
    Some(Restriction { mv_horizontal, mv_vertical, reorder, buffering })
}

/// Declares that this stream has nothing to reorder, and returns the set as it
/// should have been sent.
///
/// One form, and deliberately so: a set with no VUI at all gets a minimal one -
/// every optional block declined, then `bitstream_restriction_flag` set with a
/// reorder depth of zero and a reference buffer no deeper than the pictures use.
/// A set that already carries a VUI is left exactly as the camera sent it,
/// because a set that has spoken has said what it wants, and a stream that
/// declares a reorder depth is a stream that reorders - forcing that to zero
/// would show its pictures out of order.
///
/// This is the field a decoder reads to decide how many pictures it may hold
/// before it releases one, and a stream of I and P pictures that declares
/// nothing leaves the decoder to its own default - which, on the hardware in
/// `priv/docs/foscam_probe.md` §12, is four to six pictures. The tile then
/// moves in steps of a third of a second while every counter on the reading
/// side says the stream is arriving evenly.
///
/// The rewrite changes the set's length - the field is Exp-Golomb and zero is
/// one bit - so the whole set is spliced and the emulation prevention bytes
/// put back. `None` means the set was left alone: one that already carries a
/// VUI, or one this cannot walk.
///
/// This edits what the decoder is handed, and the caller is expected to say so
/// in its log, for the same reason [`constrain_baseline`] is.
pub fn declare_no_reorder(nal: &[u8]) -> Option<Vec<u8>> {
    let Walk { mut bits, refs, level, size } = walk(nal)?;
    let rbsp = without_emulation(&nal[1..]);
    // The stop bit closes the set, and every bit after it is padding that
    // belongs to the byte it lands in. Splicing up to the stop bit and letting
    // the packer pad the new length is what keeps the ending honest.
    let stop = trailing_stop(&rbsp)?;
    let vui_at = bits.bit;
    if bits.bits(1)? == 1 {
        return None; // it has a VUI of its own, and has said what it wants
    }
    // Every optional block declined, then the restriction this is for. A VUI
    // that says nothing else is the smallest one that can say this, and nothing
    // in it contradicts what the camera sent.
    let mut out = BitPacker::with_capacity(rbsp.len() + 8);
    out.copy_bits(&rbsp, 0, vui_at);
    out.push_bit(true); // vui_parameters_present_flag
    out.push_bit(false); // aspect_ratio_info_present_flag
    out.push_bit(false); // overscan_info_present_flag
    out.push_bit(false); // video_signal_type_present_flag
    out.push_bit(false); // chroma_loc_info_present_flag
    out.push_bit(false); // timing_info_present_flag
    out.push_bit(false); // nal_hrd_parameters_present_flag
    out.push_bit(false); // vcl_hrd_parameters_present_flag
    out.push_bit(false); // pic_struct_present_flag
    out.push_bit(true); // bitstream_restriction_flag
    out.push_bit(true); // motion_vectors_over_pic_boundaries_flag
    out.ue(0); // max_bytes_per_pic_denom, which yields to no limit
    out.ue(0); // max_bits_per_mb_denom, likewise
    // Sixteen is the "unrestricted" value of these two. Zero, which is what the
    // smallest encoding would give, says a vector never reaches past a quarter
    // of a sample - true of nothing that moves, and a limit a decoder strict
    // enough to size its vector buffer from it would find the stream breaking.
    out.ue(16); // log2_max_mv_length_horizontal
    out.ue(16); // log2_max_mv_length_vertical
    out.ue(0); // max_num_reorder_frames
    // The buffer has to hold every reference picture, and the set said how many
    // those are - but never more than its own level allows a decoder to hold,
    // or the declaration contradicts the set it is written into.
    let buffering = refs.max(1).min(max_dpb_frames(level, size));
    out.ue(buffering); // max_dec_frame_buffering
    out.copy_bits(&rbsp, vui_at + 1, stop + 1);
    Some(rebuild(nal[0], out.finish()))
}

/// Whether a sequence parameter set carries a VUI of its own.
///
/// A set that carries one has declared what it wants; a set that carries none
/// has left the decoder to its default. The difference decides whether a
/// rewrite has anything to add.
pub fn has_vui(nal: &[u8]) -> Option<bool> {
    let mut walk = walk(nal)?;
    Some(walk.bits.bits(1)? == 1)
}

/// Declares every sequence parameter set of an Annex-B access unit as having
/// nothing to reorder, and reports how many were changed.
///
/// The buffer is rebuilt rather than edited because the sets change length -
/// see [`declare_no_reorder`]. A unit with no set in it is left as it was.
pub fn declare_no_reorder_in_unit(data: &mut Vec<u8>) -> usize {
    let mut changed = 0;
    let mut rebuilt = Vec::with_capacity(data.len() + 32);
    let mut copied = 0;
    let mut offset = 0;
    while let Some((start, end)) = nal_at(data, offset) {
        if let Some(set) = declare_no_reorder(&data[start..end]) {
            rebuilt.extend_from_slice(&data[copied..start]);
            rebuilt.extend_from_slice(&set);
            copied = end;
            changed += 1;
        }
        offset = end;
    }
    if changed > 0 {
        rebuilt.extend_from_slice(&data[copied..]);
        *data = rebuilt;
    }
    changed
}

/// A rewritten set: the NAL header the camera sent, then the payload with the
/// emulation prevention bytes put back.
fn rebuild(header: u8, rbsp: Vec<u8>) -> Vec<u8> {
    let emulated = with_emulation(&rbsp);
    let mut set = Vec::with_capacity(emulated.len() + 1);
    set.push(header);
    set.extend_from_slice(&emulated);
    set
}

/// Position of the last set bit of an RBSP: the one that closes it.
///
/// `rbsp_trailing_bits` is a single one followed by zeroes, so the last one in
/// the buffer is that bit and everything after it is padding.
fn trailing_stop(rbsp: &[u8]) -> Option<usize> {
    let total = rbsp.len() * 8;
    (0..total).rev().find(|&at| (rbsp[at / 8] >> (7 - at % 8)) & 1 == 1)
}

/// A payload with its emulation prevention bytes removed.
fn without_emulation(payload: &[u8]) -> Vec<u8> {
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
    bytes
}

/// The same bytes put back the way a decoder expects to read them: a 0x03
/// inserted wherever the bitstream would otherwise show a start code.
fn with_emulation(rbsp: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(rbsp.len() + rbsp.len() / 8 + 4);
    let mut zeros = 0usize;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 0x03 {
            bytes.push(0x03);
            zeros = 0;
        }
        bytes.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    bytes
}

/// Writes bits into a buffer, a bit at a time.
///
/// The counterpart of [`BitReader`], and it exists for one job: splicing a
/// shorter Exp-Golomb value into a set, which moves every bit after it.
struct BitPacker {
    bytes: Vec<u8>,
    bit: usize,
}

impl BitPacker {
    fn with_capacity(bytes: usize) -> Self {
        Self { bytes: Vec::with_capacity(bytes), bit: 0 }
    }

    fn push_bit(&mut self, one: bool) {
        if self.bit % 8 == 0 {
            self.bytes.push(0);
        }
        if one {
            let index = self.bit / 8;
            self.bytes[index] |= 1 << (7 - (self.bit % 8));
        }
        self.bit += 1;
    }

    /// Copies the bits of `src` from `from` up to but not including `to`.
    fn copy_bits(&mut self, src: &[u8], from: usize, to: usize) {
        for at in from..to {
            let byte = src.get(at / 8).copied().unwrap_or(0);
            self.push_bit((byte >> (7 - at % 8)) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb: `value + 1` in binary, behind that many zeroes
    /// minus one.
    fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let width = 64 - code.leading_zeros();
        for _ in 1..width {
            self.push_bit(false);
        }
        for shift in (0..width).rev() {
            self.push_bit((code >> shift) & 1 == 1);
        }
    }

    /// The bytes written, with the last one padded to a byte with zeroes -
    /// which is what `rbsp_trailing_bits` asks for.
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

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
    for flag in profile_flags.iter_mut().take(max_sub_layers_minus1 as usize) {
        *flag = bits.bits(1)? == 1;
    }
    for flag in level_flags.iter_mut().take(max_sub_layers_minus1 as usize) {
        *flag = bits.bits(1)? == 1;
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

/// Brings every sequence parameter set of an Annex-B access unit whose level
/// is far above its pictures down to the level they need, and reports how many
/// were changed.
///
/// Each set is re-read and re-decided, not matched against a decision taken
/// earlier in the session: a set that has since changed its pictures is a
/// different question, and rewriting it on the strength of what the first set
/// declared would put a 4K stream's 5.1 down to the level of a 640x480 one.
/// [`overstated_level`] is what the answer comes from.
///
/// `level_idc` is the fourth byte of the set and is never preceded by an
/// emulation prevention byte - that takes two zero bytes in a row, and
/// `profile_idc` is not one of them - so this is an edit in place and the set
/// keeps its length. The pictures are untouched: what changes is the statement
/// about the decoder they ask for. See [`overstated_level`] for why one is
/// rewritten at all.
pub fn relabel_level(data: &mut [u8]) -> usize {
    let mut changed = 0;
    let mut offset = 0;
    while let Some((start, next)) = nal_at(data, offset) {
        if let Some((_, needed)) = overstated_level(&data[start..next]) {
            data[start + 3] = needed;
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
        Self { bytes: without_emulation(payload), bit: 0 }
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
        let magnitude = value.div_ceil(2) as i32;
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

    /// The front door camera's own set, as it arrives: High profile, level
    /// 5.1, 640x480, no VUI. Level 5.1 is a 4K declaration and the pictures are
    /// a quarter of that, which is what makes a decoder hold them back.
    const DOORBELL_SPS: [u8; 12] = [
        0x67, 0x64, 0x00, 0x33, 0xac, 0x15, 0x14, 0xa0, 0xa0, 0x3d, 0x90, 0x00,
    ];

    #[test]
    fn a_level_far_above_the_pictures_is_brought_down_to_what_they_need() {
        assert_eq!(picture_size(&DOORBELL_SPS), Some((640, 480)));
        assert_eq!(
            overstated_level(&DOORBELL_SPS),
            Some((51, 30)),
            "level 5.1 for a 640x480 stream is five levels out"
        );
    }

    /// The set as the SDP hands it over: the same bytes without the trailing
    /// zero the extradata dump carried, which is what the session reads.
    #[test]
    fn the_set_as_the_session_receives_it_is_read_the_same_way() {
        let from_sdp = &DOORBELL_SPS[..11];
        assert_eq!(picture_size(from_sdp), Some((640, 480)));
        assert_eq!(overstated_level(from_sdp), Some((51, 30)));
    }

    /// The set keeps its length when it is relabelled, because the level is a
    /// byte of its own - and only that byte changes.
    #[test]
    fn relabelling_reaches_only_the_level_byte() {
        let mut unit = Vec::new();
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(&DOORBELL_SPS);
        unit.extend_from_slice(&[0, 0, 1, 0x68, 0xee, 0x3c, 0xb0]);
        let before = unit.len();

        assert_eq!(relabel_level(&mut unit), 1);

        assert_eq!(unit.len(), before, "the set kept its length");
        assert_eq!(unit[7], 30, "the level byte is the one that moved");
        assert_eq!(unit[6], 0x00, "and the constraint byte is beside it");
        assert_eq!(unit[5], 0x64, "the profile is untouched");
        assert_eq!(relabel_level(&mut unit), 0, "a second pass is a no-op");
    }

    /// The rewrite decides from the set in front of it, not from one it saw
    /// earlier: a unit holding two sets brings each of them down to its own
    /// level instead of applying one answer to both.
    #[test]
    fn each_set_is_brought_down_to_its_own_level() {
        // 1280x720 declared at level 4.1, two ranks above the 3.1 its size
        // needs at thirty frames, beside the doorbell's 640x480 at level 5.1.
        // The set carries no timing VUI, so nothing here says what rate the
        // camera actually sends; the rewrite only reads the pictures.
        let big = [
            0x67, 0x42, 0x00, 0x29, 0xe2, 0x90, 0x0a, 0x00, 0xb7, 0x60, 0x2d, 0xc0, 0x40, 0x40,
            0x69, 0x07, 0x89, 0x11, 0x50,
        ];
        let mut unit = Vec::new();
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(&DOORBELL_SPS[..11]);
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(&big);

        assert_eq!(relabel_level(&mut unit), 2, "both sets overstated their level");

        let (small_at, small_end) = nal_at(&unit, 0).expect("the doorbell's set");
        assert_eq!(unit[small_at + 3], 30, "640x480 comes down to what it needs");
        let (big_at, _) = nal_at(&unit, small_end).expect("the 720p set");
        assert_eq!(unit[big_at + 3], 31, "720p comes down to its own, not the doorbell's");
    }

    /// A level caps the buffer a set may declare, which is why the rewrite asks
    /// before it writes.
    #[test]
    fn a_level_caps_the_buffer_a_set_may_declare() {
        assert_eq!(max_dpb_frames(30, (640, 480)), 6);
        assert_eq!(max_dpb_frames(40, (1920, 1080)), 4);
        assert_eq!(max_dpb_frames(51, (3840, 2160)), 5);
    }

    /// A set that declares a level its pictures fit in is left as the camera
    /// sent it - one level out is inside what the specification allows.
    #[test]
    fn a_level_the_pictures_fit_in_is_left_alone() {
        // The same camera through the other table: level 3.0 at 640x480.
        let mut fits = DOORBELL_SPS;
        fits[3] = 30;
        assert_eq!(overstated_level(&fits), None);

        // One level up is not a fault to correct either.
        let mut one_out = DOORBELL_SPS;
        one_out[3] = 31;
        assert_eq!(overstated_level(&one_out), None);
    }

    /// An H.265 set, and a buffer with no set in it, are not read as H.264's.
    #[test]
    fn a_set_of_another_kind_has_no_level_to_relabel() {
        assert_eq!(overstated_level(&HEVC_SPS), None);
        assert_eq!(overstated_level(&DOORBELL_SPS[..3]), None, "truncated");
        assert_eq!(overstated_level(&[0x65, 0x64, 0x00, 0x33]), None, "not an SPS");
    }

    /// The splice works on unescaped bits and puts the emulation prevention
    /// bytes back on the way out, so the two halves have to be exact inverses.
    /// The doorbell's own set has no escaped triple, which is why this is tested
    /// on bytes that do.
    #[test]
    fn emulation_prevention_round_trips() {
        let escaped =
            [0x67, 0x64, 0x00, 0x33, 0xac, 0x00, 0x00, 0x03, 0x01, 0xa0, 0x00, 0x00, 0x03, 0x00];
        let plain = [0x67, 0x64, 0x00, 0x33, 0xac, 0x00, 0x00, 0x01, 0xa0, 0x00, 0x00, 0x00];
        assert_eq!(without_emulation(&escaped), plain);
        assert_eq!(with_emulation(&plain), escaped);
    }

    /// The set the doorbell sends carries no VUI, which is why its decoder is
    /// left to its own default. The rewrite adds the smallest VUI that can say
    /// "nothing to reorder", and the pictures come through it unchanged.
    #[test]
    fn a_set_with_no_vui_gains_one_that_declares_no_reorder() {
        let sent = &DOORBELL_SPS[..11];
        assert_eq!(has_vui(sent), Some(false), "the camera says nothing");
        assert_eq!(restriction(sent), None, "and declares no restriction");

        let rewritten = declare_no_reorder(sent).expect("a set with no VUI gets one");

        assert_eq!(
            picture_size(&rewritten),
            Some((640, 480)),
            "the pictures are described exactly as before"
        );
        assert_eq!(has_vui(&rewritten), Some(true));
        assert_eq!(
            restriction(&rewritten),
            Some(Restriction {
                mv_horizontal: 16,
                mv_vertical: 16,
                reorder: 0,
                buffering: 1,
            }),
            "no reordering, one reference picture, and vectors left unrestricted"
        );
        assert_eq!(overstated_level(&rewritten).map(|(_, level)| level), Some(30), "level kept");
        assert!(rewritten.len() > sent.len(), "a VUI costs bytes");
        assert_eq!(rewritten[0], sent[0], "the NAL header is the camera's");
        assert_eq!(rewritten[1], sent[1], "so is the profile");
        assert_eq!(declare_no_reorder(&rewritten), None, "a second pass is a no-op");
    }

    /// A set that carries a VUI has said what it wants, and a set that declares
    /// a reorder depth is a stream that reorders: narrowing it to zero would
    /// show those pictures out of order. It is left exactly as it arrived.
    #[test]
    fn a_set_that_has_spoken_is_left_alone() {
        let spoken = declare_no_reorder(&DOORBELL_SPS[..11]).expect("rewritten once");
        assert_eq!(declare_no_reorder(&spoken), None);
        assert_eq!(
            restriction(&spoken).map(|restriction| restriction.reorder),
            Some(0),
            "what it declares is what was written, not something written again"
        );
    }

    /// Rewriting the set inside an access unit leaves the rest of the unit -
    /// the picture the decoder is about to be handed - as it was.
    #[test]
    fn a_unit_keeps_its_pictures_when_its_set_is_rewritten() {
        let mut unit = Vec::new();
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(&DOORBELL_SPS[..11]);
        unit.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0xb0]);
        unit.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);

        assert_eq!(declare_no_reorder_in_unit(&mut unit), 1);

        let set = access_unit_sps(&unit).expect("the set is still there");
        assert_eq!(picture_size(set), Some((640, 480)));
        assert_eq!(has_vui(set), Some(true));
        assert!(
            unit.ends_with(&[0, 0, 0, 1, 0x65, 0x88, 0x84]),
            "the picture behind the set is untouched"
        );
        assert_eq!(declare_no_reorder_in_unit(&mut unit), 0, "a second pass changes nothing");
    }
}
