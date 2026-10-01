//! H.264 RTP depacketization (RFC 6184) and access unit reassembly.
//!
//! A camera splits one coded picture over one or more RTP packets and sets the
//! marker bit on the last of them. Three packetizations show up in practice:
//!
//! | Payload         | Meaning                                                  |
//! |-----------------|----------------------------------------------------------|
//! | single NAL      | the payload is one complete NAL unit                      |
//! | `STAP-A` (24)   | several small NAL units, usually SPS/PPS, in one payload   |
//! | `FU-A` (28)     | one NAL unit fragmented over consecutive packets           |
//!
//! [`H264Depacketizer::push`] rebuilds the Annex-B access unit the decoder
//! expects. A picture ends where the marker bit says it does, and where the RTP
//! timestamp changes: a camera was measured marking the parameter sets that
//! accompany a picture rather than its last slice, and trusting the marker alone
//! released no picture at all from it.
//!
//! The parameter sets are cached, from the SDP `sprop-parameter-sets` attribute
//! and from the stream itself, the newest copy of each id replacing the one
//! before, and put back in front of every access unit that does not carry them.
//!
//! Repeating them per picture is what makes a live stream survive a decoder that
//! resets: a unit identical to the one already held is not read as a sequence
//! change, while a picture that arrives without its sets after the decoder threw
//! them away is rejected with `dsNoParamSets` and takes the picture with it.

use std::collections::BTreeMap;

use crate::rtsp::RtpHeader;

/// Start code prefixing every NAL unit of an Annex-B access unit.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Coded slice of a picture that is not an IDR.
pub const NAL_SLICE: u8 = 1;
/// Coded slice of an IDR picture.
pub const NAL_IDR: u8 = 5;
/// Sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// Picture parameter set.
pub const NAL_PPS: u8 = 8;
/// Supplemental enhancement information, which cameras put in front of a picture.
pub const NAL_SEI: u8 = 6;
/// Aggregation packet carrying several NAL units.
pub const NAL_STAP_A: u8 = 24;
/// Fragmentation unit with a one byte header.
pub const NAL_FU_A: u8 = 28;
/// Fragmentation unit with a two byte decoding order number.
pub const NAL_FU_B: u8 = 29;
/// Aggregation packets that interleave RTP timestamps; not used for video.
pub const NAL_STAP_B: u8 = 25;

/// Clock rate of the RTP timestamp of an H.264 stream.
pub const H264_CLOCK_RATE: i64 = 90_000;

/// Type of a NAL unit, i.e. the low five bits of its header.
pub fn nal_unit_type(header: u8) -> u8 {
    header & 0x1f
}

/// Splits a payload into the NAL units it carries.
///
/// The first unit starts at offset zero: a fragment header carried its header
/// byte, so no start code precedes it. Every later unit follows an Annex-B start
/// code the sender left in the payload. Slicing on start codes is safe inside a
/// coded picture because H.264 inserts emulation prevention bytes precisely so
/// that the sequence cannot occur there.
fn split_nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut begin = 0;
    let mut offset = 0;
    while offset + 3 <= data.len() {
        let code = if data[offset..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        if offset > begin {
            units.push(&data[begin..offset]);
        }
        begin = offset + code;
        offset = begin;
    }
    if begin < data.len() {
        units.push(&data[begin..]);
    }
    units
}

/// Id a parameter set is stored under, i.e. the `seq_parameter_set_id` of an
/// SPS or the `pic_parameter_set_id` of a PPS.
///
/// `None` for any other unit, and for a set too short to hold its id.
fn parameter_set_id(kind: u8, nal: &[u8]) -> Option<u8> {
    // The id leads the RBSP of both sets, after the three bytes of profile,
    // constraint flags and level an SPS starts with.
    let mut bit = match kind {
        NAL_SPS => 24,
        NAL_PPS => 0,
        _ => return None,
    };
    let body = nal.get(1..)?;
    let mut zeros = 0;
    while read_bit(body, bit)? == 0 {
        zeros += 1;
        bit += 1;
        if zeros > 8 {
            return None;
        }
    }
    bit += 1;
    let mut value = 0u32;
    for _ in 0..zeros {
        value = (value << 1) | u32::from(read_bit(body, bit)?);
        bit += 1;
    }
    let id = u8::try_from((1u32 << zeros) - 1 + value).ok()?;
    // The spec caps an SPS id at 31. Anything else was not a parameter set to
    // begin with - most likely a slice that happened to start with a byte whose
    // type looked like one - and caching it would put garbage in front of every
    // picture that follows.
    match kind {
        NAL_SPS if id > 31 => None,
        _ => Some(id),
    }
}

/// One bit of an RBSP, counting from the most significant bit of the first byte.
fn read_bit(data: &[u8], bit: usize) -> Option<u8> {
    let byte = *data.get(bit / 8)?;
    Some((byte >> (7 - bit % 8)) & 1)
}

/// A complete coded picture in the Annex-B form the decoder expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    /// RTP timestamp of the picture, on the 90 kHz H.264 clock.
    pub timestamp: u32,
    /// The picture carries an IDR slice, so it can restart the decoder.
    pub keyframe: bool,
}

impl AccessUnit {
    /// Presentation timestamp in microseconds.
    pub fn pts_us(&self) -> i64 {
        i64::from(self.timestamp) * 1_000_000 / H264_CLOCK_RATE
    }
}

/// Reassembles the H.264 access units of one RTP session.
#[derive(Debug, Default)]
pub struct H264Depacketizer {
    /// Payload type of the video track. `None` accepts every payload.
    payload_type: Option<u8>,
    /// Cached parameter sets, keyed by kind and id, SPS ahead of PPS.
    ///
    /// The decoder identifies a set by the id inside it, so the newest copy of
    /// each id replaces the one before rather than piling up beside it: two SPS
    /// that share an id and differ in content make the decoder end the access
    /// unit it is reading, which costs the picture that follows.
    parameter_sets: BTreeMap<(u8, u8), Vec<u8>>,
    /// Annex-B data of the access unit being assembled.
    frame: Vec<u8>,
    /// Timestamp of the access unit being assembled.
    frame_timestamp: Option<u32>,
    /// The access unit holds an IDR slice.
    frame_keyframe: bool,
    /// The access unit carries its own SPS.
    frame_has_sps: bool,
    /// The access unit carries its own PPS.
    frame_has_pps: bool,
    /// The access unit carries a coded slice, so it holds a picture.
    frame_has_slice: bool,
    /// NAL unit currently being reassembled from `FU-A` fragments.
    fragment: Vec<u8>,
    /// A `FU-A` sequence was started and can still be appended to.
    fragment_active: bool,
}

impl H264Depacketizer {
    /// Creates a depacketizer for the video track.
    ///
    /// `parameter_sets` are the raw NAL units of the SDP
    /// `sprop-parameter-sets` attribute, when the camera advertises them.
    pub fn new(payload_type: Option<u8>, parameter_sets: Vec<Vec<u8>>) -> Self {
        let mut depacketizer = Self { payload_type, ..Self::default() };
        for nal in parameter_sets {
            depacketizer.remember_parameter_set(&nal);
        }
        depacketizer
    }

    /// Parameter sets known so far, SPS ahead of PPS.
    pub fn parameter_sets(&self) -> impl Iterator<Item = &[u8]> {
        self.parameter_sets.values().map(Vec::as_slice)
    }

    /// Feeds one RTP packet, appending the access units it completed.
    ///
    /// A picture ends at the marker bit RFC 6184 specifies and at a change of
    /// the RTP timestamp, so one packet completes at most two access units: the
    /// picture the new timestamp released, and the one the marker closed.
    pub fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> Vec<AccessUnit> {
        if payload.is_empty() {
            return Vec::new();
        }
        if let Some(expected) = self.payload_type {
            if header.payload_type != expected {
                return Vec::new();
            }
        }

        let mut units = Vec::new();

        // A new timestamp ends the picture under construction. Releasing it
        // rather than dropping it is what keeps a camera visible that marks the
        // parameter sets of a picture instead of its last slice; a picture whose
        // middle was lost costs one concealed picture, which the encoder's next
        // IDR recovers from.
        if let Some(open) = self.frame_timestamp.filter(|open| *open != header.timestamp) {
            units.extend(self.finish(open));
        }
        self.frame_timestamp = Some(header.timestamp);

        match nal_unit_type(payload[0]) {
            NAL_FU_A | NAL_FU_B => self.push_fragment(payload),
            NAL_STAP_A => self.push_aggregate(payload),
            1..=23 => self.append_nal(payload),
            // Undefined types and the unused aggregation packets: ignoring them
            // is better than feeding the decoder a unit it cannot parse.
            _ => {}
        }

        // The marker closes the picture, but a marker that lands in the middle
        // of a fragmented NAL unit must not cut the unit short: closing now
        // would throw the fragments that have not arrived yet away with it, and
        // cameras were measured marking whichever packet of a picture carries a
        // parameter set, long before its last slice has been sent. Such a
        // picture is closed by the next timestamp instead, one frame later.
        if header.marker && !self.fragment_active {
            units.extend(self.finish(header.timestamp));
        }
        units
    }

    /// Reassembles a fragmented NAL unit (`FU-A` / `FU-B`).
    fn push_fragment(&mut self, payload: &[u8]) {
        let indicator = payload[0];
        // FU-B repeats the decoding order number on every fragment.
        let (fu_header, body) = if nal_unit_type(indicator) == NAL_FU_B {
            if payload.len() < 4 {
                return;
            }
            (payload[1], &payload[4..])
        } else {
            if payload.len() < 2 {
                return;
            }
            (payload[1], &payload[2..])
        };

        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;

        if start {
            self.fragment.clear();
            // The reconstructed header keeps the forbidden zero bit and the
            // nal_ref_idc of the fragment indicator.
            self.fragment.push((indicator & 0xe0) | (fu_header & 0x1f));
            self.fragment_active = true;
        }
        if !self.fragment_active {
            // The first fragment was lost, the rest of the unit is unusable.
            return;
        }
        self.fragment.extend_from_slice(body);
        if end {
            let nal = std::mem::take(&mut self.fragment);
            self.fragment_active = false;
            self.append_nal(&nal);
        }
    }

    /// Splits an aggregation packet (`STAP-A`) into its NAL units.
    fn push_aggregate(&mut self, payload: &[u8]) {
        let mut offset = 1;
        while offset + 2 <= payload.len() {
            let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
            offset += 2;
            if size == 0 || offset + size > payload.len() {
                // Truncated aggregation packet, keep what was already parsed.
                return;
            }
            self.append_nal(&payload[offset..offset + size]);
            offset += size;
        }
    }

    /// Appends the NAL units a payload carries to the access unit.
    ///
    /// A payload normally holds exactly one NAL unit, but one camera was measured
    /// handing a whole access unit to a single fragmented unit: the fragment
    /// header named only its first NAL, an `SEI`, and the slices followed inside
    /// the same payload, separated by start codes the camera left in it. Reading
    /// only the first byte made every such picture look like a parameter set on
    /// its own and the picture was thrown away, which showed as a slideshow that
    /// only advanced on key frames.
    fn append_nal(&mut self, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        // Splitting is right for a payload that carries a whole access unit: a
        // camera doing that names its first NAL, and the rest follow behind start
        // codes. A payload that opens with a slice is a fragment of one, and is
        // taken whole. An encoder that does not insert the emulation prevention
        // bytes the spec requires can put a sequence that looks like a start code
        // inside a slice, and cutting there hands the decoder a fragment it reads
        // as a parameter set and complains about.
        let units = match nal_unit_type(nal[0]) {
            NAL_SLICE | NAL_IDR => vec![nal],
            _ => split_nal_units(nal),
        };
        for unit in units {
            self.classify(unit);
            self.frame.extend_from_slice(&START_CODE);
            self.frame.extend_from_slice(unit);
        }
    }

    /// Records what one NAL unit contributes to the access unit under assembly.
    fn classify(&mut self, nal: &[u8]) {
        let Some(&first) = nal.first() else { return };
        match nal_unit_type(first) {
            // The two halves are tracked apart: a picture that repeats one of
            // them and not the other still needs the missing one in front of it.
            NAL_SPS => {
                self.frame_has_sps = true;
                self.remember_parameter_set(nal);
            }
            NAL_PPS => {
                self.frame_has_pps = true;
                self.remember_parameter_set(nal);
            }
            NAL_IDR => {
                self.frame_keyframe = true;
                self.frame_has_slice = true;
            }
            // Types 1 to 4 are the slices of a picture that is not an IDR: 1 is
            // a plain coded slice, 2 to 4 are its data partitions.
            NAL_SLICE..=4 => self.frame_has_slice = true,
            _ => {}
        }
    }

    /// Caches an SPS or PPS, replacing the previous copy of the same id.
    fn remember_parameter_set(&mut self, nal: &[u8]) {
        let Some(&first) = nal.first() else { return };
        let kind = nal_unit_type(first);
        let Some(id) = parameter_set_id(kind, nal) else { return };
        self.parameter_sets.insert((kind, id), nal.to_vec());
    }

    /// Closes the access unit under construction.
    fn finish(&mut self, timestamp: u32) -> Option<AccessUnit> {
        self.fragment.clear();
        self.fragment_active = false;

        let mut data = std::mem::take(&mut self.frame);
        let keyframe = std::mem::take(&mut self.frame_keyframe);
        let has_sps = std::mem::take(&mut self.frame_has_sps);
        let has_pps = std::mem::take(&mut self.frame_has_pps);
        let has_slice = std::mem::take(&mut self.frame_has_slice);

        // A unit that holds no slice is not a picture. Several cameras send SPS
        // and PPS as their own marker terminated packets, and handing those to
        // the decoder only earns a `dsNoParamSets` error: the sets are already
        // cached for the picture that follows.
        if !has_slice {
            return None;
        }

        // The cached sets are repeated in front of every picture that does not
        // bring them: the SPS whenever either half is missing, since it has to be
        // read before any PPS that references it, and the PPS only when the
        // picture brings none of its own.
        //
        // A decoder keys the sets by the id inside the unit and drops what it
        // holds whenever it resets - after a damaged picture, or when the
        // sequence changes - and it then rejects every picture that arrives
        // without its sets with `dsNoParamSets`, taking the picture down with it.
        // Repeating them per picture is what makes the stream recover on its own.
        // It costs some fifty bytes per picture, and a unit identical to the one
        // the decoder already holds is not read as a sequence change.
        let sps_missing = !has_sps;
        let pps_missing = !has_pps;
        if sps_missing || pps_missing {
            // A picture that needs sets the stream has not delivered yet cannot
            // decode. Every picture between the start of a session and its first
            // key frame is one of those on a camera whose SDP carries no sets,
            // and handing them over earns a rejected frame and two lines of
            // decoder noise each: they wait for the key frame instead.
            if self.parameter_sets.is_empty() {
                return None;
            }
            let mut with_parameter_sets = Vec::with_capacity(data.len() + 64);
            for nal in self.parameter_sets.values() {
                let is_sps = nal_unit_type(nal[0]) == NAL_SPS;
                if (is_sps && (sps_missing || pps_missing)) || (!is_sps && pps_missing) {
                    with_parameter_sets.extend_from_slice(&START_CODE);
                    with_parameter_sets.extend_from_slice(nal);
                }
            }
            with_parameter_sets.extend_from_slice(&data);
            data = with_parameter_sets;
        }

        Some(AccessUnit { data, timestamp, keyframe })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(payload_type: u8, timestamp: u32, marker: bool) -> RtpHeader {
        RtpHeader {
            version: 2,
            padding: false,
            extension: false,
            marker,
            payload_type,
            sequence: 1,
            timestamp,
            ssrc: 0,
            csrc_count: 0,
            header_len: 12,
        }
    }

    /// NAL unit with the given type and a recognisable body.
    fn nal(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut nal = vec![0x60 | kind];
        nal.extend_from_slice(body);
        nal
    }

    /// The byte an id takes at the head of a parameter set, in Exp-Golomb form.
    fn ue(value: u8) -> u8 {
        let code = u16::from(value) + 1;
        let bits = 16 - code.leading_zeros();
        (code as u8) << (8 - (2 * bits - 1))
    }

    /// SPS carrying the given id.
    fn sps(id: u8, tail: &[u8]) -> Vec<u8> {
        // Profile, constraint flags, level, then the id and whatever follows.
        let mut nal = vec![0x67, 0x42, 0x00, 0x1e, ue(id)];
        nal.extend_from_slice(tail);
        nal
    }

    /// PPS carrying the given id.
    fn pps(id: u8, tail: &[u8]) -> Vec<u8> {
        let mut nal = vec![0x68, ue(id), ue(0)];
        nal.extend_from_slice(tail);
        nal
    }

    /// Annex-B concatenation of the given NAL units.
    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut data = Vec::new();
        for nal in nals {
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(nal);
        }
        data
    }

    /// The one access unit the packet completed.
    fn completed(depacketizer: &mut H264Depacketizer, header: &RtpHeader, payload: &[u8]) -> AccessUnit {
        let mut units = depacketizer.push(header, payload);
        assert_eq!(units.len(), 1, "the packet must complete exactly one access unit");
        units.pop().expect("one access unit")
    }

    /// A depacketizer whose decoder already holds a set of each kind, the state a
    /// session is in once its first key frame has arrived. Returns the sets too,
    /// so a test can spell out the prefix they end up in front of.
    fn running(sps_body: &[u8], pps_body: &[u8]) -> (H264Depacketizer, Vec<u8>, Vec<u8>) {
        let sps = sps(0, sps_body);
        let pps = pps(0, pps_body);
        let depacketizer = H264Depacketizer::new(Some(96), vec![sps.clone(), pps.clone()]);
        (depacketizer, sps, pps)
    }

    #[test]
    fn reassembles_a_single_nal_access_unit() {
        let (mut depacketizer, sps, pps) = running(&[0x11], &[0x22]);
        let idr = nal(NAL_IDR, &[1, 2, 3]);
        // A picture small enough to fit one packet carries the marker on it.
        let unit = completed(&mut depacketizer, &header(96, 9000, true), &idr);
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &idr]));
        assert!(unit.keyframe);
        assert_eq!(unit.pts_us(), 100_000);
    }

    #[test]
    fn ignores_other_payload_types() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        assert!(depacketizer.push(&header(97, 0, true), &nal(NAL_IDR, &[1])).is_empty());
    }

    #[test]
    fn reassembles_a_fragmented_nal_unit() {
        let (mut depacketizer, sps, pps) = running(&[0x11], &[0x22]);
        let body = [0xaa; 10];
        // FU-A: indicator (F | NRI | 28), FU header (S | E | type).
        let start = [0x7c, 0x80 | NAL_IDR, body[0], body[1]];
        let middle = [[0x7c, NAL_IDR].as_slice(), &body[2..6]].concat();
        let end = [[0x7c, 0x40 | NAL_IDR].as_slice(), &body[6..]].concat();

        assert!(depacketizer.push(&header(96, 42, false), &start).is_empty());
        assert!(depacketizer.push(&header(96, 42, false), &middle).is_empty());
        let unit = completed(&mut depacketizer, &header(96, 42, true), &end);

        let prefix = annex_b(&[&sps, &pps]);
        assert!(unit.data.starts_with(&prefix), "the cached sets lead the picture");
        let idr = &unit.data[prefix.len()..];
        assert_eq!(idr.len(), START_CODE.len() + 1 + body.len());
        assert_eq!(&idr[..4], &START_CODE);
        // The reconstructed NAL header keeps the NRI of the indicator.
        assert_eq!(idr[4], 0x60 | NAL_IDR);
        assert_eq!(&idr[5..], &body);
        assert!(unit.keyframe);
    }

    #[test]
    fn drops_a_fragment_sequence_whose_head_was_lost() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        let middle = [0x7c, NAL_IDR, 1, 2];
        assert!(depacketizer.push(&header(96, 7, false), &middle).is_empty());
        assert!(depacketizer.push(&header(96, 7, true), &middle).is_empty());
    }

    #[test]
    fn expands_an_aggregation_packet() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        let sps = sps(0, &[0x11, 0x22]);
        let pps = pps(0, &[0x33]);
        let slice = nal(NAL_SLICE, &[0x44]);

        let mut payload = vec![0x78];
        for unit in [&sps, &pps, &slice] {
            payload.extend_from_slice(&(unit.len() as u16).to_be_bytes());
            payload.extend_from_slice(unit);
        }
        let unit = completed(&mut depacketizer, &header(96, 5, true), &payload);

        assert_eq!(unit.data, annex_b(&[&sps, &pps, &slice]));
        assert!(!unit.keyframe);
        assert_eq!(depacketizer.parameter_sets().count(), 2);
    }

    #[test]
    fn drops_a_unit_that_holds_no_picture() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        let sps = sps(0, &[0x11]);
        let pps = pps(0, &[0x22]);

        // Cameras that send SPS and PPS as their own marker terminated packets
        // must not reach the decoder: a parameter set alone is not a picture.
        assert!(depacketizer.push(&header(96, 1, true), &sps).is_empty());
        assert!(depacketizer.push(&header(96, 2, true), &pps).is_empty());
        assert_eq!(depacketizer.parameter_sets().count(), 2);

        // The sets are cached, so the IDR that follows still decodes with them.
        let idr = nal(NAL_IDR, &[0x01]);
        let unit = completed(&mut depacketizer, &header(96, 3, true), &idr);
        assert!(unit.keyframe);
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &idr]));
    }

    #[test]
    fn injects_the_cached_parameter_sets_before_an_idr() {
        let sps = sps(0, &[0x11]);
        let pps = pps(0, &[0x22]);
        let mut depacketizer = H264Depacketizer::new(Some(96), vec![sps.clone(), pps.clone()]);

        // The IDR arrives alone, without repeating SPS/PPS.
        let idr = nal(NAL_IDR, &[0x01]);
        let unit = completed(&mut depacketizer, &header(96, 1, true), &idr);
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &idr]));
    }

    #[test]
    fn injects_the_cached_parameter_sets_before_an_inter_picture() {
        let sps = sps(0, &[0x11]);
        let pps = pps(0, &[0x22]);
        let mut depacketizer = H264Depacketizer::new(Some(96), vec![sps.clone(), pps.clone()]);

        // A camera that sends its sets once, with the key frame, and leaves every
        // other picture without them.
        let idr = nal(NAL_IDR, &[0x01]);
        let unit = completed(&mut depacketizer, &header(96, 1, true), &idr);
        assert!(unit.keyframe);
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &idr]));

        // The inter picture is completed the same way. Repeating a set the
        // decoder already holds is free, while a picture that arrives without it
        // after the decoder dropped what it held is rejected outright.
        let slice = nal(NAL_SLICE, &[0x02]);
        let unit = completed(&mut depacketizer, &header(96, 2, true), &slice);
        assert!(!unit.keyframe);
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &slice]));
    }

    #[test]
    fn completes_a_picture_that_brings_only_half_of_the_pair() {
        let cached_sps = sps(0, &[0x11]);
        let cached_pps = pps(0, &[0x22]);
        let mut depacketizer =
            H264Depacketizer::new(Some(96), vec![cached_sps.clone(), cached_pps.clone()]);

        // A camera that carries a PPS of its own and leaves the SPS out: the
        // cached SPS goes in front, while the PPS the picture brought survives
        // untouched.
        let carried = pps(1, &[0x33]);
        let slice = nal(NAL_SLICE, &[0x44]);
        assert!(depacketizer.push(&header(96, 1, false), &carried).is_empty());
        let unit = completed(&mut depacketizer, &header(96, 1, true), &slice);

        assert_eq!(unit.data, annex_b(&[&cached_sps, &carried, &slice]));
    }

    #[test]
    fn keeps_the_newest_copy_of_each_parameter_set() {
        let first_sps = sps(0, &[0x11]);
        let second_sps = sps(0, &[0x33]);
        let other_sps = sps(1, &[0x44]);
        let pps = pps(0, &[0x22]);
        let mut depacketizer = H264Depacketizer::new(
            Some(96),
            vec![first_sps.clone(), pps.clone(), other_sps.clone()],
        );

        // A second SPS under the id of the first replaces it: two units that
        // share an id and differ in content make the decoder end the access unit
        // it is reading, which costs the picture that follows.
        assert!(depacketizer.push(&header(96, 1, false), &second_sps).is_empty());
        assert_eq!(depacketizer.parameter_sets().count(), 3);

        let slice = nal(NAL_IDR, &[0x55]);
        let unit = completed(&mut depacketizer, &header(96, 2, true), &slice);
        assert_eq!(unit.data, annex_b(&[&second_sps, &other_sps, &pps, &slice]));
    }

    #[test]
    fn keeps_the_parameter_sets_carried_by_the_stream() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        let sps = sps(0, &[0x11]);
        let pps = pps(0, &[0x22]);
        // First access unit carries SPS/PPS in band.
        let mut payload = vec![0x78];
        for unit in [&sps, &pps] {
            payload.extend_from_slice(&(unit.len() as u16).to_be_bytes());
            payload.extend_from_slice(unit);
        }
        // The unit holds no slice, so it is dropped, but the sets are cached.
        assert!(depacketizer.push(&header(96, 1, true), &payload).is_empty());

        // A later IDR without parameter sets gets them from the cache.
        let idr = nal(NAL_IDR, &[0x01]);
        let unit = completed(&mut depacketizer, &header(96, 2, true), &idr);
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &idr]));
    }

    #[test]
    fn picks_the_slices_out_of_an_access_unit_packed_into_one_fragment_sequence() {
        let (mut depacketizer, sps, pps) = running(&[0x11], &[0x22]);
        // One FOSCAM stream hands a whole access unit to a single fragmented
        // unit: the fragment header names the leading `SEI`, and the slices
        // follow inside the same payload behind start codes of their own.
        let sei_body = [9u8, 9];
        let sei = nal(NAL_SEI, &sei_body);
        let first = nal(NAL_SLICE, &[1, 1, 1]);
        let second = nal(NAL_SLICE, &[2, 2, 2]);
        // The first unit carries neither a start code nor its own header byte:
        // the fragment header supplied the byte. Only the units behind it are
        // preceded by a start code.
        let mut packed = sei_body.to_vec();
        for unit in [&first, &second] {
            packed.extend_from_slice(&START_CODE);
            packed.extend_from_slice(unit);
        }

        let split = packed.len() / 2;
        let head = [[0x7c, 0x80 | NAL_SEI].as_slice(), &packed[..split]].concat();
        let tail = [[0x7c, 0x40 | NAL_SEI].as_slice(), &packed[split..]].concat();
        assert!(depacketizer.push(&header(96, 1, false), &head).is_empty());
        let unit = completed(&mut depacketizer, &header(96, 1, true), &tail);
        assert!(!unit.keyframe, "the slices are not IDR slices");
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &sei, &first, &second]));
    }

    #[test]
    fn holds_pictures_back_until_the_parameter_sets_have_arrived() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        // A camera whose SDP carries no sets: every picture between the start of
        // the session and the first key frame references sets that do not exist
        // yet. They are worth neither a decode nor the two lines of decoder noise
        // a rejection costs, so they are held back rather than handed over.
        let slice = nal(NAL_SLICE, &[0x01]);
        assert!(depacketizer.push(&header(96, 1, true), &slice).is_empty());
        assert!(depacketizer.push(&header(96, 2, true), &slice).is_empty());

        // The key frame brings the sets in band. The unit holds no slice and is
        // dropped, but the sets are cached.
        let sps = sps(0, &[0x11]);
        let pps = pps(0, &[0x22]);
        let mut aggregate = vec![0x78];
        for unit in [&sps, &pps] {
            aggregate.extend_from_slice(&(unit.len() as u16).to_be_bytes());
            aggregate.extend_from_slice(unit);
        }
        assert!(depacketizer.push(&header(96, 3, true), &aggregate).is_empty());

        // Pictures flow again, with the sets put back in front of them.
        let unit = completed(&mut depacketizer, &header(96, 4, true), &slice);
        assert_eq!(unit.data, annex_b(&[&sps, &pps, &slice]));
    }

    #[test]
    fn releases_the_picture_a_timestamp_change_ends() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        // A camera that marks the parameter sets of a picture and leaves the
        // slice unmarked: without the timestamp nothing would ever be released.
        let sps = sps(0, &[0x11]);
        let pps = pps(0, &[0x22]);
        assert!(depacketizer.push(&header(96, 1, true), &sps).is_empty());
        assert!(depacketizer.push(&header(96, 1, true), &pps).is_empty());

        let slice = nal(NAL_SLICE, &[1, 2, 3]);
        assert!(depacketizer.push(&header(96, 1, false), &slice).is_empty());

        let next = nal(NAL_SLICE, &[4, 5, 6]);
        let released = depacketizer.push(&header(96, 2, false), &next);
        assert_eq!(released.len(), 1);
        // The sets the camera marked were cached rather than carried, and they
        // are put back in front of the picture the timestamp released.
        assert_eq!(released[0].data, annex_b(&[&sps, &pps, &slice]));
    }

    #[test]
    fn a_marker_inside_a_fragmented_unit_does_not_cut_the_picture() {
        let mut depacketizer = H264Depacketizer::new(Some(96), Vec::new());
        let body = [0xaa; 6];
        // The camera marks the packet that carries the parameter set, which
        // arrives in the middle of the fragmentation of the picture's slice.
        let first = [[0x7c, 0x80 | NAL_IDR].as_slice(), &body[..2]].concat();
        let sps = sps(0, &[0x11]);
        let last = [[0x7c, 0x40 | NAL_IDR].as_slice(), &body[2..]].concat();

        assert!(depacketizer.push(&header(96, 1, false), &first).is_empty());
        assert!(depacketizer.push(&header(96, 1, true), &sps).is_empty());
        assert!(depacketizer.push(&header(96, 1, false), &last).is_empty());

        // The picture is released whole by the next timestamp: the fragments
        // that followed the marker are still part of it.
        let released = depacketizer.push(&header(96, 2, false), &[0x7c, 0x80 | NAL_SLICE, 9]);
        assert_eq!(released.len(), 1);
        let idr = [[0x60 | NAL_IDR].as_slice(), &body[..]].concat();
        // The picture brought an SPS and no PPS, so the cached SPS is repeated
        // in front of it: a set identical to the one the decoder already holds
        // is not read as a sequence change.
        assert_eq!(released[0].data, annex_b(&[&sps, &sps, &idr]));
        assert!(released[0].keyframe);
    }

    /// Splits an Annex-B bitstream into NAL units, start codes removed.
    fn split_annex_b(data: &[u8]) -> Vec<Vec<u8>> {
        let mut units = Vec::new();
        let mut begin: Option<usize> = None;
        let mut offset = 0;
        while offset + 3 <= data.len() {
            let code = if data[offset..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if data[offset..].starts_with(&[0, 0, 1]) {
                3
            } else {
                offset += 1;
                continue;
            };
            if let Some(start) = begin {
                units.push(data[start..offset].to_vec());
            }
            offset += code;
            begin = Some(offset);
        }
        if let Some(start) = begin {
            units.push(data[start..].to_vec());
        }
        units.retain(|unit| !unit.is_empty());
        units
    }

    /// Packetizes one NAL unit as `FU-A` fragments, marking the last payload.
    fn fragment(nal: &[u8]) -> Vec<(Vec<u8>, bool)> {
        const MAX_PAYLOAD: usize = 1200;
        if nal.len() <= MAX_PAYLOAD {
            return vec![(nal.to_vec(), true)];
        }
        let body = &nal[1..];
        let mut packets = Vec::new();
        let mut offset = 0;
        while offset < body.len() {
            let len = (body.len() - offset).min(MAX_PAYLOAD - 2);
            let last = offset + len == body.len();
            let mut payload = vec![
                (nal[0] & 0xe0) | NAL_FU_A,
                if offset == 0 { 0x80 } else { 0 }
                    | if last { 0x40 } else { 0 }
                    | (nal[0] & 0x1f),
            ];
            payload.extend_from_slice(&body[offset..offset + len]);
            packets.push((payload, last));
            offset += len;
        }
        packets
    }

    /// The offline end to end check: a real bitstream is repacketized as RTP,
    /// depacketized back into access units and decoded.
    ///
    /// Point `XGVIEW_TEST_H264` at a file produced by, for example,
    ///
    /// ```text
    /// ffmpeg -f lavfi -i testsrc=size=160x120:rate=5:duration=0.4 \
    ///     -c:v libx264 -profile:v baseline -pix_fmt yuv420p -f h264 tiny.h264
    /// ```
    ///
    /// It is what a camera streams, minus the RTSP and TCP layers.
    #[test]
    fn decodes_a_bitstream_repacketized_as_rtp() {
        let Ok(path) = std::env::var("XGVIEW_TEST_H264") else {
            return;
        };
        let data = std::fs::read(path).expect("read the bitstream");
        let units = split_annex_b(&data);
        assert!(units.len() >= 2, "the fixture must hold several NAL units");

        let parameter_sets = units
            .iter()
            .filter(|unit| matches!(nal_unit_type(unit[0]), NAL_SPS | NAL_PPS))
            .cloned()
            .collect();
        let mut depacketizer = H264Depacketizer::new(Some(96), parameter_sets);

        let mut decoder = monitor_codec::create_decoder();
        let config = monitor_codec::DecoderConfig {
            codec: monitor_codec::Codec::H264,
            width: 0,
            height: 0,
            surface: None,
            low_latency: true,
        };
        decoder.configure(&config).expect("configure the decoder");

        let mut timestamp = 0u32;
        let mut frames = 0;
        for unit in &units {
            timestamp += 3_600; // one picture every 1/25 s on the 90 kHz clock
            for (payload, last) in fragment(unit) {
                for access_unit in depacketizer.push(&header(96, timestamp, last), &payload) {
                    let decoded = decoder
                        .decode(&access_unit.data, access_unit.pts_us(), access_unit.keyframe)
                        .expect("decode the access unit");
                    frames += decoded.len();
                }
            }
        }

        assert!(frames >= 2, "the repacketized stream must decode, got {frames} picture(s)");
    }
}
