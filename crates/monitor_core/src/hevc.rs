//! H.265 / HEVC RTP depacketization (RFC 7798) and access unit reassembly.
//!
//! The shape is H.264's ([`crate::h264`]) with HEVC's own numbers, and the two
//! are kept as siblings rather than merged: the payload header, the parameter
//! sets and the packet types all differ, and a shared body would be a body full
//! of `if hevc` - see DevMemo §17.
//!
//! | Payload          | Meaning                                                 |
//! |------------------|---------------------------------------------------------|
//! | single NAL       | the payload is one complete NAL unit                     |
//! | `AP` (48)        | several NAL units, each behind a two byte length         |
//! | `FU` (49)        | one NAL unit fragmented over consecutive packets         |
//!
//! Three things are HEVC's own and drive every branch here:
//!
//! * the NAL header is **two** bytes and the type sits in `(byte0 >> 1) & 0x3f`,
//!   so the H.264 one byte masks read an HEVC stream as noise;
//! * the parameter sets are three - VPS (32), SPS (33), PPS (34) - and a PPS
//!   references an SPS references a VPS, so they are replayed in that order;
//! * a picture is a key frame when a slice is an IRAP (types 16..=23), not when
//!   a single IDR type appears.
//!
//! The rest is copied from the H.264 depacketizer on purpose, because it is not
//! codec specific: a picture ends at the marker bit and at a change of the RTP
//! timestamp, the cached sets are repeated in front of every picture that does
//! not carry its own, and a gap in the sequence numbers writes off the picture
//! being assembled so a loss costs one frame rather than a screen of garbage.

use std::collections::BTreeMap;

use crate::h264::AccessUnit;
use crate::rtsp::RtpHeader;

/// Start code prefixing every NAL unit of an Annex-B access unit.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Video parameter set.
pub const NAL_VPS: u8 = 32;
/// Sequence parameter set.
pub const NAL_SPS: u8 = 33;
/// Picture parameter set.
pub const NAL_PPS: u8 = 34;
/// Aggregation packet carrying several NAL units, each length prefixed.
pub const NAL_AP: u8 = 48;
/// Fragmentation unit carrying one NAL unit over several packets.
pub const NAL_FU: u8 = 49;
/// Payload header carrying fragment alignment; not handled.
pub const NAL_PACI: u8 = 50;

/// Type of a NAL unit: the six bits between the forbidden bit and the layer id.
///
/// The header is two bytes, and the type lives in the first - `F(1) Type(6)
/// LayerId[5](1)` - so it is `(byte0 >> 1) & 0x3f`, not the H.264 `byte0 & 0x1f`.
pub fn nal_unit_type(header: u8) -> u8 {
    (header >> 1) & 0x3f
}

/// Whether a NAL type is a video coding layer slice, i.e. carries a picture.
///
/// Types 0..=31 are the slices; 32..=47 are the non-VCL units (parameter sets,
/// SEI), and 48 and up are the RTP level aggregation and fragmentation headers.
fn is_slice(kind: u8) -> bool {
    kind <= 31
}

/// Whether a slice type is an intra random access point, i.e. a key frame.
///
/// Types 16..=23 are the IRAP pictures - BLA, IDR and CRA - and they are the
/// HEVC equivalent of the H.264 IDR: the point a decoder can restart from.
fn is_irap(kind: u8) -> bool {
    (16..=23).contains(&kind)
}

/// Reassembles the H.265 access units of one RTP session.
#[derive(Debug, Default)]
pub struct HevcDepacketizer {
    /// Payload type of the video track. `None` accepts every payload.
    payload_type: Option<u8>,
    /// Cached parameter sets, keyed by NAL type: VPS, SPS, PPS.
    ///
    /// Keyed by type alone, where H.264 keys on the id inside the set as well.
    /// Reading the id out of an HEVC sequence parameter set means walking the
    /// profile-tier-level structure that precedes it, and a live camera sends
    /// one of each kind: the newest of a kind replacing the one before is
    /// enough, and it avoids a parser that would only ever guess.
    parameter_sets: BTreeMap<u8, Vec<u8>>,
    /// Annex-B data of the access unit being assembled.
    frame: Vec<u8>,
    /// Timestamp of the access unit being assembled.
    frame_timestamp: Option<u32>,
    /// The access unit holds an IRAP slice.
    frame_keyframe: bool,
    /// The access unit carries its own VPS.
    frame_has_vps: bool,
    /// The access unit carries its own SPS.
    frame_has_sps: bool,
    /// The access unit carries its own PPS.
    frame_has_pps: bool,
    /// The access unit carries a coded slice, so it holds a picture.
    frame_has_slice: bool,
    /// NAL unit currently being reassembled from `FU` fragments.
    fragment: Vec<u8>,
    /// An `FU` sequence was started and can still be appended to.
    fragment_active: bool,
    /// Packets of the access unit under assembly were lost, so it must not
    /// reach the decoder. Set by [`HevcDepacketizer::invalidate_current`].
    damaged: bool,
}

impl HevcDepacketizer {
    /// Creates a depacketizer for the video track.
    ///
    /// `parameter_sets` are the raw NAL units the SDP advertised through
    /// `sprop-vps` / `sprop-sps` / `sprop-pps`, when the camera carries them.
    pub fn new(payload_type: Option<u8>, parameter_sets: Vec<Vec<u8>>) -> Self {
        let mut depacketizer = Self { payload_type, ..Self::default() };
        for nal in parameter_sets {
            depacketizer.remember_parameter_set(&nal);
        }
        depacketizer
    }

    /// Parameter sets known so far, VPS ahead of SPS ahead of PPS.
    pub fn parameter_sets(&self) -> impl Iterator<Item = &[u8]> {
        self.parameter_sets.values().map(Vec::as_slice)
    }

    /// Writes off the access unit being assembled.
    ///
    /// Called when a gap in the RTP sequence numbers shows that packets of the
    /// picture were lost, exactly as in H.264: a slice with a hole in it is
    /// painted as corruption and may be kept as a reference, so the unit is
    /// dropped whole and a loss costs one missing picture.
    pub fn invalidate_current(&mut self) {
        self.damaged = true;
        self.fragment.clear();
        self.fragment_active = false;
    }

    /// Feeds one RTP packet, appending the access units it completed.
    ///
    /// As in H.264 a picture ends at the marker bit and at a change of the RTP
    /// timestamp, so one packet completes at most two access units: the picture
    /// the new timestamp released, and the one the marker closed.
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

        // A new timestamp ends the picture under construction. A fragment still
        // open here is a key frame whose last end flag the sender dropped: its
        // data is complete, only the flag is missing, so it is closed as a
        // whole slice rather than thrown away.
        if let Some(open) = self.frame_timestamp.filter(|open| *open != header.timestamp) {
            if self.fragment_active && !self.fragment.is_empty() {
                let nal = std::mem::take(&mut self.fragment);
                self.fragment_active = false;
                self.append_nal(&nal);
            }
            units.extend(self.finish(open));
        }
        self.frame_timestamp = Some(header.timestamp);

        match nal_unit_type(payload[0]) {
            NAL_FU => self.push_fragment(payload),
            NAL_AP => self.push_aggregate(payload),
            // A single NAL unit packet: the payload is the unit, header and all.
            // This is where a parameter set or a whole slice arrives.
            0..=47 => self.append_nal(payload),
            // PACI and the undefined types carry no picture this reads.
            _ => {}
        }

        // The marker closes the picture, but a marker that lands inside a
        // fragmented NAL unit must not cut it short: such a picture is closed by
        // the next timestamp instead, one frame later.
        if header.marker && !self.fragment_active {
            units.extend(self.finish(header.timestamp));
        }
        units
    }

    /// Reassembles a fragmented NAL unit (`FU`).
    ///
    /// The payload is the two byte `PayloadHdr` (type 49), one `FU header`
    /// (`S(1) E(1) FuType(6)`) and the fragment. The reconstructed unit keeps
    /// the forbidden bit, the layer id and the temporal id of the header it was
    /// cut from, and takes its type from `FuType` - which is the whole point of
    /// the scheme: the fragments say which NAL unit they rebuild.
    fn push_fragment(&mut self, payload: &[u8]) {
        if payload.len() < 3 {
            return;
        }
        let fu_header = payload[2];
        let body = &payload[3..];
        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;

        if start {
            self.fragment.clear();
            // F and the top bit of the layer id sit in byte0's low bit and top
            // bit; everything else about the type comes from FuType.
            let fu_type = fu_header & 0x3f;
            self.fragment.push((payload[0] & 0x81) | (fu_type << 1));
            self.fragment.push(payload[1]);
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

    /// Splits an aggregation packet (`AP`) into its NAL units.
    ///
    /// Each unit is preceded by a two byte big-endian length, and the units are
    /// complete NAL units with their own two byte headers - no start codes.
    fn push_aggregate(&mut self, payload: &[u8]) {
        let mut offset = 2;
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

    /// Appends one complete NAL unit to the access unit under assembly.
    ///
    /// Every caller hands over exactly one unit - a single NAL packet is one, an
    /// `AP` names each with a length, an `FU` is rebuilt into one - so unlike
    /// H.264 there is nothing to split on start codes here.
    fn append_nal(&mut self, nal: &[u8]) {
        if nal.len() < 2 {
            return;
        }
        self.classify(nal);
        self.frame.extend_from_slice(&START_CODE);
        self.frame.extend_from_slice(nal);
    }

    /// Records what one NAL unit contributes to the access unit under assembly.
    fn classify(&mut self, nal: &[u8]) {
        match nal_unit_type(nal[0]) {
            NAL_VPS => {
                self.frame_has_vps = true;
                self.remember_parameter_set(nal);
            }
            NAL_SPS => {
                self.frame_has_sps = true;
                self.remember_parameter_set(nal);
            }
            NAL_PPS => {
                self.frame_has_pps = true;
                self.remember_parameter_set(nal);
            }
            kind if is_irap(kind) => {
                self.frame_keyframe = true;
                self.frame_has_slice = true;
            }
            kind if is_slice(kind) => self.frame_has_slice = true,
            _ => {}
        }
    }

    /// Caches a VPS, SPS or PPS, replacing the previous copy of the same kind.
    fn remember_parameter_set(&mut self, nal: &[u8]) {
        if nal.len() < 2 {
            return;
        }
        let kind = nal_unit_type(nal[0]);
        if matches!(kind, NAL_VPS | NAL_SPS | NAL_PPS) {
            self.parameter_sets.insert(kind, nal.to_vec());
        }
    }

    /// Closes the access unit under construction.
    fn finish(&mut self, timestamp: u32) -> Option<AccessUnit> {
        self.fragment.clear();
        self.fragment_active = false;

        let mut data = std::mem::take(&mut self.frame);
        let keyframe = std::mem::take(&mut self.frame_keyframe);
        let has_vps = std::mem::take(&mut self.frame_has_vps);
        let has_sps = std::mem::take(&mut self.frame_has_sps);
        let has_pps = std::mem::take(&mut self.frame_has_pps);
        let has_slice = std::mem::take(&mut self.frame_has_slice);
        let damaged = std::mem::take(&mut self.damaged);

        // A unit that holds no slice is not a picture. Cameras send the sets as
        // their own marker terminated packets, and handing those to the decoder
        // only earns an error: they are already cached for the picture after.
        if !has_slice {
            return None;
        }

        // A unit that lost packets never reaches the decoder: see
        // `invalidate_current`.
        if damaged {
            return None;
        }

        // The cached sets are replayed in front of every picture that does not
        // bring them, in VPS, SPS, PPS order - a PPS references an SPS which
        // references a VPS, so a decoder needs them in that order. A decoder
        // drops what it holds whenever it resets - after a damaged picture, or
        // when the sequence changes - and then rejects every picture that
        // arrives without its sets, taking the picture down with it; repeating
        // them is what makes the stream recover on its own.
        //
        // Every unit here reaches the decoder as `HevcDepacketizer` assembled
        // it, so unlike H.264 the decoder is never also handed the units the
        // camera sent separately - this is the only place the sets are added.
        if !(has_vps && has_sps && has_pps) {
            // A picture that needs sets the stream has not delivered yet cannot
            // decode. Every picture between the start of a session and its first
            // key frame is one of those on a camera whose SDP carries no sets.
            if self.parameter_sets.is_empty() {
                return None;
            }
            let mut with_parameter_sets = Vec::with_capacity(data.len() + 128);
            for nal in self.parameter_sets.values() {
                with_parameter_sets.extend_from_slice(&START_CODE);
                with_parameter_sets.extend_from_slice(nal);
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

    /// A NAL unit with the given type and a recognisable body, two byte header.
    fn nal(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut unit = vec![kind << 1, 0x01];
        unit.extend_from_slice(body);
        unit
    }

    /// An access unit in Annex-B, as the depacketizer is expected to hand it on.
    fn annex_b(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut data = Vec::new();
        for nal in nals {
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(nal);
        }
        data
    }

    /// The three parameter sets a picture needs, in the order the SDP lists
    /// them when it advertises the HEVC attributes separately.
    fn sets() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (nal(NAL_VPS, &[0xa0]), nal(NAL_SPS, &[0xb0]), nal(NAL_PPS, &[0xc0]))
    }

    /// Feeds one access unit's worth of NAL units on their own packets, the
    /// last one carrying the marker that closes the picture.
    fn feed(depacketizer: &mut HevcDepacketizer, timestamp: u32, nals: &[&[u8]]) -> Vec<AccessUnit> {
        let (last, rest) = nals.split_last().expect("at least one NAL unit");
        let mut units = Vec::new();
        for unit in rest {
            units.extend(depacketizer.push(&header(96, timestamp, false), unit));
        }
        units.extend(depacketizer.push(&header(96, timestamp, true), last));
        units
    }

    #[test]
    fn reads_the_two_byte_header_type() {
        assert_eq!(nal_unit_type(0x40), 32, "VPS");
        assert_eq!(nal_unit_type(0x42), 33, "SPS");
        assert_eq!(nal_unit_type(0x44), 34, "PPS");
        assert_eq!(nal_unit_type(0x26), 19, "IDR_W_RADL");
        // The H.264 mask would read this SPS as type 2, which is the whole
        // reason the two depacketizers are not one.
        assert_ne!(nal_unit_type(0x42) & 0x1f, nal_unit_type(0x42));
    }

    /// The marker closes one picture, and a VCL slice that is not an IRAP is
    /// not a key frame.
    #[test]
    fn closes_a_picture_on_the_marker() {
        let (vps, sps, pps) = sets();
        let slice = nal(1, &[1, 2, 3, 4]); // TRAIL_R, not a key frame
        let mut depacketizer = HevcDepacketizer::new(Some(96), Vec::new());
        let units = feed(&mut depacketizer, 100, &[&vps, &sps, &pps, &slice]);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, annex_b(&[vps, sps, pps, slice]));
        assert_eq!(units[0].timestamp, 100);
        assert!(!units[0].keyframe);
    }

    #[test]
    fn an_irap_slice_is_a_key_frame() {
        let (vps, sps, pps) = sets();
        let slice = nal(19, &[9, 9]); // IDR_W_RADL
        let mut depacketizer = HevcDepacketizer::new(Some(96), Vec::new());
        let units = feed(&mut depacketizer, 7, &[&vps, &sps, &pps, &slice]);
        assert_eq!(units.len(), 1);
        assert!(units[0].keyframe);
    }

    /// A picture that arrives without its sets gets the cached ones replayed in
    /// VPS, SPS, PPS order - whatever order the SDP listed them in.
    #[test]
    fn replays_cached_sets_in_vps_sps_pps_order() {
        let (vps, sps, pps) = sets();
        // The SDP the camera sent at 192.168.17.87 lists them pps, sps, vps.
        let depacketizer_sets = vec![pps.clone(), sps.clone(), vps.clone()];
        let mut depacketizer = HevcDepacketizer::new(Some(96), depacketizer_sets);

        let slice = nal(19, &[1, 2]);
        let units = feed(&mut depacketizer, 1, &[&slice]);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, annex_b(&[vps, sps, pps, slice]));
        assert!(units[0].keyframe);
    }

    /// A picture that carries its own sets is not given them a second time -
    /// even when the SDP advertised them too.
    #[test]
    fn a_picture_with_its_own_sets_is_left_alone() {
        let (vps, sps, pps) = sets();
        let slice = nal(19, &[1, 2]);
        let advertised = vec![vps.clone(), sps.clone(), pps.clone()];
        let mut depacketizer = HevcDepacketizer::new(Some(96), advertised);
        let units = feed(&mut depacketizer, 1, &[&vps, &sps, &pps, &slice]);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, annex_b(&[vps, sps, pps, slice]), "no duplicate sets");
    }

    /// A NAL unit fragmented over three `FU` packets is stitched back with its
    /// header's type restored from `FuType`.
    #[test]
    fn reassembles_a_fragmented_unit() {
        let (vps, sps, pps) = sets();
        let slice = nal(19, &[1, 2, 3, 4, 5, 6, 7, 8]); // IDR of ten bytes
        let body = &slice[2..]; // past the two byte header
        let fu = |s: bool, e: bool, part: &[u8]| {
            let mut payload = vec![
                NAL_FU << 1,                             // PayloadHdr: type 49
                0x01,
                ((s as u8) << 7) | ((e as u8) << 6) | 19, // S, E, FuType = 19
            ];
            payload.extend_from_slice(part);
            payload
        };
        let split = [&body[..4], &body[4..7], &body[7..]];

        let mut depacketizer = HevcDepacketizer::new(Some(96), Vec::new());
        for unit in [&vps, &sps, &pps] {
            assert!(depacketizer.push(&header(96, 5, false), unit).is_empty());
        }
        assert!(depacketizer.push(&header(96, 5, false), &fu(true, false, split[0])).is_empty());
        assert!(depacketizer.push(&header(96, 5, false), &fu(false, false, split[1])).is_empty());
        let units = depacketizer.push(&header(96, 5, true), &fu(false, true, split[2]));
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, annex_b(&[vps, sps, pps, slice]));
        assert!(units[0].keyframe, "FuType 19 is an IRAP");
    }

    /// An `AP` carries several NAL units, each behind a two byte length.
    #[test]
    fn splits_an_aggregation_packet() {
        let (vps, sps, pps) = sets();
        let slice = nal(19, &[1, 2, 3]);
        let mut payload = vec![NAL_AP << 1, 0x01];
        for unit in [&vps, &sps, &pps, &slice] {
            payload.extend_from_slice(&(unit.len() as u16).to_be_bytes());
            payload.extend_from_slice(unit);
        }
        let mut depacketizer = HevcDepacketizer::new(Some(96), Vec::new());
        let units = depacketizer.push(&header(96, 3, true), &payload);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, annex_b(&[vps, sps, pps, slice]));
        assert!(units[0].keyframe);
    }

    /// The parameters the SDP advertised reach the decoder in front of a
    /// picture that arrives without them, which is how a camera that lists them
    /// separately is configured.
    #[test]
    fn a_parameter_set_packet_is_not_a_picture() {
        let mut depacketizer = HevcDepacketizer::new(Some(96), Vec::new());
        let sps = nal(NAL_SPS, &[0xb0]);
        // A marker terminated SPS on its own carries no slice: no picture.
        let units = depacketizer.push(&header(96, 1, true), &sps);
        assert!(units.is_empty());
        // ... but it is cached, and replayed before the picture that follows.
        let slice = nal(19, &[1]);
        let units = depacketizer.push(&header(96, 2, true), &slice);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, annex_b(&[sps, slice]));
    }

    /// A picture the sequence numbers say was holed is written off whole - the
    /// same unit would have decoded had the packets all arrived.
    #[test]
    fn a_damaged_picture_is_dropped() {
        let (vps, sps, pps) = sets();
        let mut depacketizer = HevcDepacketizer::new(Some(96), vec![vps, sps, pps]);
        let slice = nal(1, &[1, 2, 3]);
        depacketizer.push(&header(96, 9, false), &slice);
        depacketizer.invalidate_current();
        let units = depacketizer.push(&header(96, 9, true), &nal(1, &[4, 5]));
        assert!(units.is_empty(), "the holed picture never reaches the decoder");
    }

    /// A timestamp change closes the open picture, as it does for H.264, so a
    /// camera that marks its parameter sets rather than its last slice still
    /// releases a picture.
    #[test]
    fn a_timestamp_change_closes_the_picture() {
        let (vps, sps, pps) = sets();
        let mut depacketizer = HevcDepacketizer::new(Some(96), vec![vps, sps, pps]);
        depacketizer.push(&header(96, 1, false), &nal(1, &[1]));
        let units = depacketizer.push(&header(96, 2, false), &nal(1, &[2]));
        assert_eq!(units.len(), 1, "the picture of timestamp 1 was released");
        assert_eq!(units[0].timestamp, 1);
    }

    #[test]
    fn ignores_another_payload_type() {
        let mut depacketizer = HevcDepacketizer::new(Some(96), Vec::new());
        let units = depacketizer.push(&header(97, 1, true), &nal(19, &[1]));
        assert!(units.is_empty());
    }

    /// A PACI packet carries no NAL unit to reassemble and is ignored.
    #[test]
    fn ignores_a_paci_packet() {
        let mut depacketizer = HevcDepacketizer::new(Some(96), Vec::new());
        let paci = vec![NAL_PACI << 1, 0x01, 0x00, 0x00];
        let units = depacketizer.push(&header(96, 1, true), &paci);
        assert!(units.is_empty());
    }
}
