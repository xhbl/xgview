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

/// Sequence parameter set NAL unit type.
const NAL_SPS: u8 = 7;

/// Picture size a sequence parameter set describes, in pixels.
///
/// `nal` is the parameter set as it arrived - the NAL header byte followed by
/// its payload, no start code. `None` covers everything that cannot be read:
/// another kind of NAL, a truncated one, a profile whose fields this does not
/// walk (a High profile with scaling matrices), or a size of zero.
pub fn picture_size(nal: &[u8]) -> Option<(u32, u32)> {
    if nal.len() < 4 || nal[0] & 0x1f != NAL_SPS {
        return None;
    }
    let mut bits = BitReader::new(&nal[1..]);
    let profile = bits.bits(8)?;
    bits.bits(8)?; // constraint flags and reserved bits
    bits.bits(8)?; // level_idc
    bits.ue()?; // seq_parameter_set_id

    // The High family carries fields the Baseline and Main ones do not.
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            bits.bits(1)?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.bits(1)? == 1 {
            // Scaling matrices. Their count and length depend on the chroma
            // format, and walking them buys nothing here: a stream that carries
            // them simply gets no size from this.
            return None;
        }
    }

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
    let frame_mbs_only = bits.bits(1)?;
    if frame_mbs_only == 0 {
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
    let interlace = 2 - frame_mbs_only;
    let width = width_in_mbs * 16 - (crop_left + crop_right) * 2;
    let height = interlace * height_in_map_units * 16 - (crop_top + crop_bottom) * 2 * interlace;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// Size the sequence parameter set carried by an Annex-B access unit describes.
///
/// The last one is taken: the pipeline may prepend the sets it holds to a
/// picture that arrived without its own, and the stream's own set follows them.
pub fn access_unit_size(data: &[u8]) -> Option<(u32, u32)> {
    let mut size = None;
    let mut offset = 0;
    while let Some((start, next)) = nal_at(data, offset) {
        let nal = &data[start..next];
        if !nal.is_empty() && nal[0] & 0x1f == NAL_SPS {
            if let Some(found) = picture_size(nal) {
                size = Some(found);
            }
        }
        offset = next;
    }
    size
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
