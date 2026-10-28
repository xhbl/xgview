//! Minimal fMP4 (fragmented MP4) parser for go2rtc's MSE WebSocket stream.
//!
//! go2rtc sends two kinds of binary WebSocket messages:
//! - Init segments (`ftyp` + `moov`): codec configuration (SPS/PPS).
//! - Media segments (`moof` + `mdat`): one or more access units with
//!   timestamps and keyframe flags.
//!
//! Each sample in `mdat` is in AVCC/HVCC format (4-byte length-prefixed NAL
//! units). The parser converts them to Annex-B format (start-code-prefixed) so
//! the existing decoders can consume them unchanged.

use crate::error::{CoreError, Result};
use monitor_codec::audio::AudioCodec;
use monitor_codec::Codec;

/// The codec and parameter sets parsed from an init segment.
pub struct MseInit {
    pub codec: Codec,
    /// Parameter sets in Annex-B format (`00 00 00 01` + SPS + `00 00 00 01` + PPS …).
    /// Feed these to the decoder before the first sample so it has the sequence
    /// and picture parameter sets it needs to decode the first IDR.
    pub param_sets: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The audio track an init segment carries (`mp4a`/`esds` → AAC).
pub struct MseAudioInit {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    /// The AudioSpecificConfig out of the `esds` box. The desktop decoder builds
    /// an ADTS header from the rate and channels and ignores it, but Android's
    /// MediaCodec wants it as its `csd-0`; empty when the segment had no esds.
    pub extradata: Vec<u8>,
}

/// One access unit extracted from a media segment.
pub struct MseSample {
    /// Annex-B formatted access unit data.
    pub data: Vec<u8>,
    /// Presentation timestamp in microseconds.
    pub pts_us: i64,
    /// Whether this is a keyframe (IDR).
    pub keyframe: bool,
}

/// One audio access unit from a media segment - a raw AAC frame, not Annex-B.
pub struct MseAudioSample {
    pub data: Vec<u8>,
    pub pts_us: i64,
}

/// What [`MseParser::parse_message`] found in one WebSocket binary frame.
pub struct ParsedMessage {
    /// Present when the message contained a `moov` box (init segment), video.
    pub init: Option<MseInit>,
    /// The audio track's description, when the init segment carried one.
    pub audio: Option<MseAudioInit>,
    /// Video access units from `moof` + `mdat` pairs (media segment).
    pub samples: Vec<MseSample>,
    /// Audio access units from the same segments, keyed off the audio track.
    pub audio_samples: Vec<MseAudioSample>,
}

/// What an init segment yields: the video track and, when present, the audio one.
struct MoovOut {
    video: Option<MseInit>,
    audio: Option<MseAudioInit>,
}

/// Whether a `trak` holds video or audio.
enum TrackKind {
    Video(MseInit),
    Audio(MseAudioInit),
}

/// One parsed `trak`: its `track_ID`, its `mdhd` timescale, and what it holds.
struct TrackDesc {
    id: u32,
    timescale: u32,
    kind: TrackKind,
}

/// Parses fMP4 init and media segments from go2rtc's MSE WebSocket.
pub struct MseParser {
    /// `track_ID` of the video / audio track, learned from the init segment.
    video_track_id: Option<u32>,
    audio_track_id: Option<u32>,
    /// Each track's own `mdhd` timescale.
    video_timescale: u32,
    audio_timescale: u32,
    nal_length_size: usize,
}

impl MseParser {
    pub fn new() -> Self {
        Self {
            video_track_id: None,
            audio_track_id: None,
            video_timescale: 90000,
            audio_timescale: 8000,
            nal_length_size: 4,
        }
    }

    /// Parses one binary WebSocket message, which may contain an init segment,
    /// a media segment, or both.
    pub fn parse_message(&mut self, data: &[u8]) -> Result<ParsedMessage> {
        let mut init = None;
        let mut audio = None;
        let mut samples = Vec::new();
        let mut audio_samples = Vec::new();
        let mut pending_run: Option<TrackRun> = None;

        for (box_type, content) in iter_boxes(data) {
            match box_type {
                b"moov" => {
                    let moov = self.parse_moov(content)?;
                    init = moov.video;
                    audio = moov.audio;
                }
                b"moof" => pending_run = Some(self.parse_moof(content, content.len() + 8)?),
                b"mdat" => {
                    if let Some(run) = pending_run.take() {
                        self.extract_samples(&run, content, &mut samples, &mut audio_samples)?;
                    }
                }
                _ => {}
            }
        }

        Ok(ParsedMessage { init, audio, samples, audio_samples })
    }


    fn parse_moov(&mut self, data: &[u8]) -> Result<MoovOut> {
        let mut video = None;
        let mut audio = None;
        for (box_type, content) in iter_boxes(data) {
            if box_type == b"trak" {
                if let Some(desc) = self.parse_trak(content)? {
                    match desc.kind {
                        TrackKind::Video(init) => {
                            self.video_track_id = Some(desc.id);
                            self.video_timescale = desc.timescale.max(1);
                            if video.is_none() {
                                video = Some(init);
                            }
                        }
                        TrackKind::Audio(init) => {
                            self.audio_track_id = Some(desc.id);
                            self.audio_timescale = desc.timescale.max(1);
                            if audio.is_none() {
                                audio = Some(init);
                            }
                        }
                    }
                }
            }
        }
        if video.is_none() && audio.is_none() {
            return Err(CoreError::network("moov had no usable trak"));
        }
        Ok(MoovOut { video, audio })
    }

    fn parse_trak(&mut self, data: &[u8]) -> Result<Option<TrackDesc>> {
        let mut track_id = None;
        let mut timescale = 90000;
        let mut mdia = None;
        for (box_type, content) in iter_boxes(data) {
            if box_type == b"tkhd" {
                track_id = Some(parse_tkhd(content));
            }
            if box_type == b"mdia" {
                mdia = Some(content);
            }
        }
        let mdia = mdia.ok_or_else(|| CoreError::network("trak had no mdia"))?;

        for (box_type, content) in iter_boxes(mdia) {
            if box_type == b"mdhd" {
                timescale = parse_mdhd(content);
            }
            if box_type == b"minf" {
                if let Some(kind) = self.parse_minf(content)? {
                    return Ok(Some(TrackDesc { id: track_id.unwrap_or(0), timescale, kind }));
                }
            }
        }
        Ok(None)
    }

    fn parse_minf(&mut self, data: &[u8]) -> Result<Option<TrackKind>> {
        for (box_type, content) in iter_boxes(data) {
            if box_type == b"stbl" {
                if let Some(kind) = self.parse_stbl(content)? {
                    return Ok(Some(kind));
                }
            }
        }
        Ok(None)
    }

    fn parse_stbl(&mut self, data: &[u8]) -> Result<Option<TrackKind>> {
        for (box_type, content) in iter_boxes(data) {
            if box_type == b"stsd" {
                if let Some(kind) = self.parse_stsd(content)? {
                    return Ok(Some(kind));
                }
            }
        }
        Ok(None)
    }

    fn parse_stsd(&mut self, data: &[u8]) -> Result<Option<TrackKind>> {
        // stsd: 1 byte version + 3 bytes flags + 4 bytes entry_count, then entries.
        if data.len() < 8 {
            return Ok(None);
        }
        let entries = &data[8..];
        for (box_type, content) in iter_boxes(entries) {
            match box_type {
                b"avc1" | b"avc3" => {
                    if let Some(init) = self.parse_avc_entry(content)? {
                        return Ok(Some(TrackKind::Video(init)));
                    }
                }
                b"hvc1" | b"hev1" => {
                    if let Some(init) = self.parse_hevc_entry(content)? {
                        return Ok(Some(TrackKind::Video(init)));
                    }
                }
                b"mp4a" => {
                    if let Some(init) = self.parse_mp4a_entry(content) {
                        return Ok(Some(TrackKind::Audio(init)));
                    }
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Parses an `mp4a` (AAC) sample entry for its rate, channel count and the
    /// `esds` box.
    ///
    /// AudioSampleEntry is 6 reserved + 2 data-reference + version/revision/
    /// vendor, then `channelcount` at offset 16, then `samplerate` at offset 24
    /// as a 16.16 fixed point, then its child boxes (the AAC `esds` among them)
    /// start at offset 28. The esds AudioSpecificConfig goes out as `extradata`;
    /// the desktop decoder builds an ADTS header from the rate and channels
    /// instead, but Android's MediaCodec asks for it as `csd-0`.
    fn parse_mp4a_entry(&mut self, data: &[u8]) -> Option<MseAudioInit> {
        if data.len() < 28 {
            return None;
        }
        let channels = u16::from_be_bytes([data[16], data[17]]) as u8;
        let rate = u32::from_be_bytes([data[24], data[25], data[26], data[27]]) >> 16;
        let mut extradata = Vec::new();
        for (box_type, content) in iter_boxes(&data[28..]) {
            if box_type == b"esds" {
                extradata = parse_esds(content).unwrap_or_default();
                break;
            }
        }
        Some(MseAudioInit {
            codec: AudioCodec::Aac,
            sample_rate: rate.max(1),
            channels: channels.max(1),
            extradata,
        })
    }

    /// Parses an `avc1`/`avc3` sample entry to find the `avcC` box.
    fn parse_avc_entry(&mut self, data: &[u8]) -> Result<Option<MseInit>> {
        // VisualSampleEntry: 6 reserved + 2 data_ref_index + 16 pre_defined
        // + 4 width + 4 height + ... then nested boxes.
        // The avcC box starts after the fixed 78-byte VisualSampleEntry header.
        if data.len() < 78 {
            return Ok(None);
        }
        let width = u16::from_be_bytes(data[24..26].try_into().unwrap()) as u32;
        let height = u16::from_be_bytes(data[26..28].try_into().unwrap()) as u32;
        for (box_type, content) in iter_boxes(&data[78..]) {
            if box_type == b"avcC" {
                let (param_sets, nal_len) = parse_avcc(content)?;
                self.nal_length_size = nal_len;
                return Ok(Some(MseInit {
                    codec: Codec::H264,
                    param_sets,
                    width,
                    height,
                }));
            }
        }
        Ok(None)
    }

    /// Parses an `hvc1`/`hev1` sample entry to find the `hvcC` box.
    fn parse_hevc_entry(&mut self, data: &[u8]) -> Result<Option<MseInit>> {
        if data.len() < 78 {
            return Ok(None);
        }
        let width = u16::from_be_bytes(data[24..26].try_into().unwrap()) as u32;
        let height = u16::from_be_bytes(data[26..28].try_into().unwrap()) as u32;
        for (box_type, content) in iter_boxes(&data[78..]) {
            if box_type == b"hvcC" {
                let (param_sets, nal_len) = parse_hvcc(content)?;
                self.nal_length_size = nal_len;
                return Ok(Some(MseInit {
                    codec: Codec::H265,
                    param_sets,
                    width,
                    height,
                }));
            }
        }
        Ok(None)
    }

    fn parse_moof(&self, data: &[u8], moof_size: usize) -> Result<TrackRun> {
        for (box_type, content) in iter_boxes(data) {
            if box_type == b"traf" {
                if let Some(mut run) = self.parse_traf(content)? {
                    run.moof_size = moof_size;
                    return Ok(run);
                }
            }
        }
        Err(CoreError::network("moof had no usable traf"))
    }

    fn parse_traf(&self, data: &[u8]) -> Result<Option<TrackRun>> {
        let mut track_id: u32 = 0;
        let mut base_dts: u64 = 0;
        let mut trun: Option<TrunData> = None;
        let mut default_sample_duration: u32 = 0;
        let mut default_sample_size: u32 = 0;
        let mut default_sample_flags: u32 = 0;

        for (box_type, content) in iter_boxes(data) {
            match box_type {
                b"tfhd" => {
                    let (id, dur, size, flags) = parse_tfhd(content);
                    track_id = id;
                    default_sample_duration = dur;
                    default_sample_size = size;
                    default_sample_flags = flags;
                }
                b"tfdt" => base_dts = parse_tfdt(content),
                b"trun" => trun = Some(parse_trun(content)?),
                _ => {}
            }
        }

        let trun = trun.ok_or_else(|| CoreError::network("traf had no trun"))?;
        Ok(Some(TrackRun {
            track_id,
            base_dts,
            trun,
            default_sample_duration,
            default_sample_size,
            default_sample_flags,
            moof_size: 0,
        }))
    }

    fn extract_samples(
        &self,
        run: &TrackRun,
        mdat: &[u8],
        out: &mut Vec<MseSample>,
        audio_out: &mut Vec<MseAudioSample>,
    ) -> Result<()> {
        let trun = &run.trun;
        // Audio samples are raw access units; video ones need the Annex-B
        // conversion. The track the `tfhd` named decides which, and each track
        // keeps its own timescale.
        let is_audio = self.audio_track_id == Some(run.track_id);
        let timescale = if is_audio { self.audio_timescale.max(1) } else { self.video_timescale.max(1) } as u64;

        // trun.data_offset is from the start of the moof box. The mdat payload
        // begins at moof_start + moof_size + 8 (mdat header), so the offset
        // into the payload is data_offset - moof_size - 8.
        let base_offset = match trun.data_offset {
            Some(v) => (v as i64 - run.moof_size as i64 - 8).max(0) as usize,
            None => 0,
        };

        let mut dts = run.base_dts;
        let mut offset = base_offset;

        // The run can name more samples than the segment carries, and the
        // count is a u32 from the wire: walking all of it is four billion turns
        // of arithmetic for a box that holds none. Stop where the samples stop
        // - the per-sample table bounds the walk while it lasts, a non-zero
        // default size bounds it by what `mdat` can hold, and with neither
        // there is nothing left to read.
        let declared = trun.sample_count as usize;
        let walk = if trun.sizes.len() >= declared {
            declared
        } else if run.default_sample_size > 0 {
            declared.min(mdat.len() / run.default_sample_size as usize + 1)
        } else {
            trun.sizes.len()
        };

        for i in 0..walk {
            let duration = trun.durations.get(i).copied().unwrap_or(run.default_sample_duration) as u64;
            let size = trun.sizes.get(i).copied().unwrap_or(run.default_sample_size) as usize;
            let comp_offset = trun.composition_offsets.get(i).copied().unwrap_or(0) as i64;
            let flags = trun.sample_flags.get(i).copied().unwrap_or(run.default_sample_flags);

            let pts = dts as i64 + comp_offset;
            let pts_us = pts * 1_000_000 / timescale as i64;
            let keyframe = (flags >> 24) & 0x03 == 0;

            if offset + size <= mdat.len() && size > 0 {
                let sample = &mdat[offset..offset + size];
                if is_audio {
                    audio_out.push(MseAudioSample { data: sample.to_vec(), pts_us });
                } else if let Some(annexb) = avcc_to_annexb(sample, self.nal_length_size) {
                    out.push(MseSample { data: annexb, pts_us, keyframe });
                }
            }

            offset += size;
            dts += duration;
        }

        Ok(())
    }
}

impl Default for MseParser {
    fn default() -> Self {
        Self::new()
    }
}

// ── Box iteration ───────────────────────────────────────────────────────────

/// Iterates over top-level ISO BMFF boxes in `data`, yielding `(type, content)`
/// where `content` is the box payload (header stripped).
fn iter_boxes(data: &[u8]) -> BoxIter<'_> {
    BoxIter { data, pos: 0 }
}

struct BoxIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = (&'a [u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let remaining = self.data.len().checked_sub(self.pos)?;
        if remaining < 8 {
            return None;
        }
        let size = u32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().ok()?) as usize;
        let box_type: &[u8; 4] = self.data[self.pos + 4..self.pos + 8].try_into().ok()?;

        let (header_size, box_size) = if size == 1 {
            if remaining < 16 {
                return None;
            }
            // The 64-bit size can exceed usize on a 32-bit target; if it
            // does, the box cannot fit in the slice we hold, so stop.
            let large = u64::from_be_bytes(
                self.data[self.pos + 8..self.pos + 16].try_into().ok()?,
            );
            let large = usize::try_from(large).ok()?;
            (16, large)
        } else if size == 0 {
            (8, remaining)
        } else {
            (8, size)
        };

        // All arithmetic is checked: on a 32-bit target `self.pos + box_size`
        // can wrap, and a wrapped value would slice past the end and panic.
        let end = self.pos.checked_add(box_size)?;
        if end > self.data.len() || box_size < header_size {
            return None;
        }
        let content = &self.data[self.pos + header_size..end];
        self.pos = end;
        Some((box_type, content))
    }
}

// ── avcC / hvcC parsing ────────────────────────────────────────────────────

/// Parses an `avcC` box payload into Annex-B parameter sets and the NAL length
/// size.
fn parse_avcc(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    if data.len() < 7 {
        return Err(CoreError::network("avcC too short"));
    }
    let nal_length_size = (data[4] & 0x03) as usize + 1;
    let mut out = Vec::new();

    // SPSs
    let num_sps = (data[5] & 0x1F) as usize;
    let mut pos = 6;
    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            break;
        }
        let len = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        if pos + len > data.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }

    // PPSs
    if pos < data.len() {
        let num_pps = data[pos] as usize;
        pos += 1;
        for _ in 0..num_pps {
            if pos + 2 > data.len() {
                break;
            }
            let len = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + len > data.len() {
                break;
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }
    }

    Ok((out, nal_length_size))
}

/// Parses an `hvcC` box payload into Annex-B parameter sets and the NAL length
/// size.
fn parse_hvcc(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    if data.len() < 23 {
        return Err(CoreError::network("hvcC too short"));
    }
    // byte 21: constantFrameRate(2) + numTemporalLayers(3) + temporalIdNested(1) + lengthSizeMinusOne(2)
    let nal_length_size = (data[21] & 0x03) as usize + 1;
    let num_arrays = data[22] as usize;
    let mut out = Vec::new();
    let mut pos = 23;

    for _ in 0..num_arrays {
        if pos + 3 > data.len() {
            break;
        }
        pos += 1; // array_completeness + NAL_unit_type
        let num_nalus = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        for _ in 0..num_nalus {
            if pos + 2 > data.len() {
                break;
            }
            let len = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + len > data.len() {
                break;
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }
    }

    Ok((out, nal_length_size))
}

// ── AVCC → Annex-B ──────────────────────────────────────────────────────────

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Converts one sample from AVCC/HVCC format (length-prefixed NAL units) to
/// Annex-B format (start-code-prefixed). Returns `None` if the data is too
/// short or the lengths do not line up.
fn avcc_to_annexb(data: &[u8], length_size: usize) -> Option<Vec<u8>> {
    if length_size == 0 || data.len() < length_size {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut pos = 0;
    while pos + length_size <= data.len() {
        let mut nalu_len = 0usize;
        for i in 0..length_size {
            nalu_len = (nalu_len << 8) | data[pos + i] as usize;
        }
        pos += length_size;
        if pos + nalu_len > data.len() || nalu_len == 0 {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[pos..pos + nalu_len]);
        pos += nalu_len;
    }
    if out.is_empty() { None } else { Some(out) }
}

// ── trun / tfhd / tfdt parsing ──────────────────────────────────────────────

struct TrackRun {
    track_id: u32,
    base_dts: u64,
    trun: TrunData,
    default_sample_duration: u32,
    default_sample_size: u32,
    default_sample_flags: u32,
    moof_size: usize,
}

struct TrunData {
    sample_count: u32,
    data_offset: Option<i32>,
    durations: Vec<u32>,
    sizes: Vec<u32>,
    composition_offsets: Vec<i32>,
    sample_flags: Vec<u32>,
}

fn full_box_header(data: &[u8]) -> Option<(u8, u32, &[u8])> {
    if data.len() < 4 {
        return None;
    }
    let version = data[0];
    let flags = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    Some((version, flags, &data[4..]))
}

/// The AudioSpecificConfig (the `DecoderSpecificInfo` descriptor of an `esds`
/// box), or `None` when the box does not carry one.
fn parse_esds(data: &[u8]) -> Option<Vec<u8>> {
    let (_, _, body) = full_box_header(data)?;
    find_decoder_specific_info(body)
}

/// Finds the first `DecoderSpecificInfo` (tag `0x05`) descriptor in `data`,
/// descending into the `ES_Descriptor` (`0x03`) and `DecoderConfigDescriptor`
/// (`0x04`) containers that hold the rest of the tree.
fn find_decoder_specific_info(mut data: &[u8]) -> Option<Vec<u8>> {
    while !data.is_empty() {
        let (tag, body, rest) = next_descriptor(data)?;
        if tag == 0x05 {
            return Some(body.to_vec());
        }
        if tag == 0x03 {
            // ES_ID (2) + flags (1), then the optional dependsOn / URL / OCR
            // fields the flags promise, then the child descriptors.
            let mut pos = 3;
            let flags = *body.get(2)?;
            if flags & 0x80 != 0 {
                pos += 2;
            }
            if flags & 0x40 != 0 {
                let url_len = *body.get(pos)? as usize;
                pos += 1 + url_len;
            }
            if flags & 0x20 != 0 {
                pos += 2;
            }
            if let Some(found) = find_decoder_specific_info(body.get(pos..)?) {
                return Some(found);
            }
        } else if tag == 0x04 {
            // objectTypeIndication (1) + streamType (1) + bufferSizeDB (3)
            // + maxBitrate (4) + avgBitrate (4), then the child descriptors.
            if let Some(found) = find_decoder_specific_info(body.get(13..)?) {
                return Some(found);
            }
        }
        data = rest;
    }
    None
}

/// Reads one descriptor from the front of `data`: its tag, the 1-4 byte varint
/// length, the body, and the slice after the descriptor.
fn next_descriptor(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
    let mut pos = 1;
    let mut len = 0usize;
    for _ in 0..4 {
        let byte = *data.get(pos)?;
        pos += 1;
        len = (len << 7) | (byte & 0x7F) as usize;
        if byte & 0x80 == 0 {
            break;
        }
    }
    let body = data.get(pos..pos + len)?;
    Some((tag, body, &data[pos + len..]))
}

fn parse_trun(data: &[u8]) -> Result<TrunData> {
    let (_, flags, body) = full_box_header(data)
        .ok_or_else(|| CoreError::network("trun too short"))?;

    if body.len() < 4 {
        return Err(CoreError::network("trun has no sample_count"));
    }
    let sample_count = u32::from_be_bytes(body[0..4].try_into().unwrap());
    let mut pos = 4;

    let data_offset = if flags & 0x000001 != 0 {
        if pos + 4 > body.len() {
            return Err(CoreError::network("trun data_offset truncated"));
        }
        let v = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        pos += 4;
        Some(v)
    } else {
        None
    };

    if flags & 0x000004 != 0 {
        // first_sample_flags — skip (we use per-sample or default).
        if pos + 4 > body.len() {
            return Err(CoreError::network("trun first_sample_flags truncated"));
        }
        pos += 4;
    }

    let has_dur = flags & 0x000100 != 0;
    let has_size = flags & 0x000200 != 0;
    let has_comp = flags & 0x000400 != 0;
    let has_flags = flags & 0x000800 != 0;
    // ISO 14496-12: composition_time_offset is always 32 bits. Version 1
    // makes it signed, not 64-bit - the old code read 8 bytes on v1 and
    // shifted every field after it.

    // A run with no per-sample field carries nothing to read. Returning here
    // rather than walking to `sample_count` matters: the count is a u32 from
    // the wire, and four billion turns of an empty loop freeze the pipeline
    // for seconds on a box of a dozen bytes.
    let bytes_per_sample = (has_dur as usize + has_size as usize + has_flags as usize
        + has_comp as usize) * 4;
    if bytes_per_sample == 0 {
        return Ok(TrunData {
            sample_count,
            data_offset,
            durations: Vec::new(),
            sizes: Vec::new(),
            composition_offsets: Vec::new(),
            sample_flags: Vec::new(),
        });
    }

    // A malicious or broken trun can name millions of samples in a body that
    // holds only a handful: `with_capacity` would allocate before the loop's
    // own bounds check ever runs, and the process would abort. Cap each Vec to
    // what the remaining body could possibly contain. `saturating_sub` is belt
    // and braces - the optional fields above are bounds-checked - because an
    // underflow here would wrap to a huge cap and undo the whole point.
    let max_samples = body.len().saturating_sub(pos) / bytes_per_sample;
    let cap = (sample_count as usize).min(max_samples);

    let mut durations = Vec::with_capacity(if has_dur { cap } else { 0 });
    let mut sizes = Vec::with_capacity(if has_size { cap } else { 0 });
    let mut composition_offsets = Vec::with_capacity(if has_comp { cap } else { 0 });
    let mut sample_flags = Vec::with_capacity(if has_flags { cap } else { 0 });

    for _ in 0..sample_count {
        if has_dur {
            if pos + 4 > body.len() { break; }
            durations.push(u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()));
            pos += 4;
        }
        if has_size {
            if pos + 4 > body.len() { break; }
            sizes.push(u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()));
            pos += 4;
        }
        if has_comp {
            if pos + 4 > body.len() { break; }
            // Always 32 bits: v0 is unsigned, v1 is signed, but the bit
            // pattern is the same and `u32 as i32` preserves it.
            let raw = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
            composition_offsets.push(raw as i32);
            pos += 4;
        }
        if has_flags {
            if pos + 4 > body.len() { break; }
            sample_flags.push(u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()));
            pos += 4;
        }
    }

    Ok(TrunData {
        sample_count,
        data_offset,
        durations,
        sizes,
        composition_offsets,
        sample_flags,
    })
}

fn parse_tfhd(data: &[u8]) -> (u32, u32, u32, u32) {
    let (_, flags, body) = match full_box_header(data) {
        Some(t) => t,
        None => return (0, 0, 0, 0),
    };

    if body.len() < 4 {
        return (0, 0, 0, 0);
    }
    // The `track_ID` is the first field, before the flagged optional ones.
    // The optional fields appear in flag-bit order: base_data_offset (0x01,
    // 8 bytes), sample_description_index (0x02, 4 bytes). The old code skipped
    // 0x02 but not 0x01, so a tfhd that carried a base_data_offset shifted
    // every field after it.
    let track_id = u32::from_be_bytes(body[0..4].try_into().unwrap());
    let mut pos = 4;

    if flags & 0x000001 != 0 {
        pos += 8;
    }
    if flags & 0x000002 != 0 {
        pos += 4;
    }

    let mut dur = 0u32;
    let mut size = 0u32;
    let mut sflags = 0u32;

    if flags & 0x000008 != 0 {
        if pos + 4 <= body.len() {
            dur = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        }
        pos += 4;
    }
    if flags & 0x000010 != 0 {
        if pos + 4 <= body.len() {
            size = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        }
        pos += 4;
    }
    if flags & 0x000020 != 0 {
        if pos + 4 <= body.len() {
            sflags = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        }

    }

    (track_id, dur, size, sflags)
}


fn parse_tfdt(data: &[u8]) -> u64 {
    let (version, _, body) = match full_box_header(data) {
        Some(t) => t,
        None => return 0,
    };
    if version == 1 && body.len() >= 8 {
        u64::from_be_bytes(body[0..8].try_into().unwrap())
    } else if body.len() >= 4 {
        u32::from_be_bytes(body[0..4].try_into().unwrap()) as u64
    } else {
        0
    }
}

/// Reads the `track_ID` out of a `tkhd` box.
///
/// Version 0: creation(4) + modification(4) + track_ID(4) — track_ID at
/// body[8..12].  Version 1: creation(8) + modification(8) + track_ID(4) —
/// track_ID at body[16..20]. The old code read body[12..16], which is the
/// reserved field after track_ID in v0 and was never correct for v1.
fn parse_tkhd(data: &[u8]) -> u32 {
    let (version, _, body) = match full_box_header(data) {
        Some(t) => t,
        None => return 0,
    };
    if version == 1 && body.len() >= 20 {
        u32::from_be_bytes(body[16..20].try_into().unwrap())
    } else if version == 0 && body.len() >= 12 {
        u32::from_be_bytes(body[8..12].try_into().unwrap())
    } else {
        0
    }
}

fn parse_mdhd(data: &[u8]) -> u32 {
    let (version, _, body) = match full_box_header(data) {
        Some(t) => t,
        None => return 90000,
    };
    // version 0: creation(4) + modification(4) + timescale(4) + duration(4)
    // version 1: creation(8) + modification(8) + timescale(4) + duration(8)
    let ts_pos = if version == 1 { 16 } else { 8 };
    if body.len() >= ts_pos + 4 {
        u32::from_be_bytes(body[ts_pos..ts_pos + 4].try_into().unwrap())
    } else {
        90000
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// An `esds` box content (FullBox header + ES descriptor tree) whose
    /// DecoderSpecificInfo is the two bytes `[0x14, 0x08]`.
    fn esds_box_content() -> Vec<u8> {
        let mut esds = vec![0x00, 0x00, 0x00, 0x00]; // FullBox version + flags
        esds.extend_from_slice(&[0x03, 0x16]); // ES_Descriptor, length 22
        esds.extend_from_slice(&[0x00, 0x01, 0x00]); // ES_ID = 1, flags = 0
        esds.extend_from_slice(&[0x04, 0x11]); // DecoderConfigDescriptor, length 17
        esds.extend_from_slice(&[0x40, 0x15]); // objectTypeIndication, streamType
        esds.extend_from_slice(&[0x00, 0x00, 0x00]); // bufferSizeDB
        esds.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // maxBitrate
        esds.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // avgBitrate
        esds.extend_from_slice(&[0x05, 0x02]); // DecoderSpecificInfo, length 2
        esds.extend_from_slice(&[0x14, 0x08]); // the AudioSpecificConfig
        esds
    }

    #[test]
    fn esds_yields_the_audio_specific_config() {
        assert_eq!(parse_esds(&esds_box_content()).unwrap(), vec![0x14, 0x08]);
        // A box without a DecoderSpecificInfo yields nothing.
        let mut bare = vec![0x00, 0x00, 0x00, 0x00];
        bare.extend_from_slice(&[0x03, 0x03, 0x00, 0x01, 0x00]);
        assert_eq!(parse_esds(&bare), None);
        assert_eq!(parse_esds(&[]), None);
    }

    #[test]
    fn mp4a_entry_carries_the_esds_as_extradata() {
        // A 28-byte AudioSampleEntry (mono, 44100 Hz) followed by its esds box.
        let mut entry = Vec::new();
        entry.extend_from_slice(&[0; 6]); // reserved
        entry.extend_from_slice(&[0x00, 0x01]); // data_reference_index
        entry.extend_from_slice(&[0; 8]); // version, revision, vendor
        entry.extend_from_slice(&[0x00, 0x01]); // channelcount
        entry.extend_from_slice(&[0x00, 0x10]); // samplesize
        entry.extend_from_slice(&[0; 4]); // pre_defined
        entry.extend_from_slice(&[0xAC, 0x44, 0x00, 0x00]); // samplerate 16.16
        let esds = esds_box_content();
        entry.extend_from_slice(&(8 + esds.len() as u32).to_be_bytes());
        entry.extend_from_slice(b"esds");
        entry.extend_from_slice(&esds);
        let mut parser = MseParser::new();
        let init = parser.parse_mp4a_entry(&entry).unwrap();
        assert_eq!(init.sample_rate, 44100);
        assert_eq!(init.channels, 1);
        assert_eq!(init.extradata, vec![0x14, 0x08]);
    }

    #[test]
    fn avcc_to_annexb_converts_length_prefix_to_start_code() {
        // Two NAL units: [7, 0x42, 0x00, 0x1e] (SPS) and [8, 0x06] (PPS),
        // each prefixed with a 4-byte big-endian length.
        let avcc = [
            0, 0, 0, 4, 0x67, 0x42, 0x00, 0x1e,
            0, 0, 0, 2, 0x68, 0x06,
        ];
        let annexb = avcc_to_annexb(&avcc, 4).unwrap();
        assert_eq!(annexb, [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e,
            0, 0, 0, 1, 0x68, 0x06,
        ]);
    }

    #[test]
    fn avcc_to_annexb_handles_3_byte_length() {
        // 3-byte length prefix: 0x000002 = 2 bytes of NAL data.
        let avcc = [0, 0, 2, 0x67, 0x42];
        let annexb = avcc_to_annexb(&avcc, 3).unwrap();
        assert_eq!(annexb, [0, 0, 0, 1, 0x67, 0x42]);
    }

    #[test]
    fn avcc_to_annexb_rejects_truncated() {
        assert_eq!(avcc_to_annexb(&[], 4), None);
        assert_eq!(avcc_to_annexb(&[0, 0, 0, 10, 1, 2], 4), None);
    }

    #[test]
    fn parse_avcc_extracts_sps_and_pps() {
        // Minimal avcC: version 1, profile 66, compat 0, level 30,
        // lengthSizeMinusOne = 3 (→ 4), 1 SPS of 3 bytes, 1 PPS of 1 byte.
        let sps = [0x67u8, 0x42, 0x1e];
        let pps = [0x68u8];
        let mut avcc = Vec::new();
        avcc.push(1); // configurationVersion
        avcc.push(0x42); // profile
        avcc.push(0x00); // compat
        avcc.push(0x1e); // level
        avcc.push(0xFF); // 0xFC | 3 → lengthSize = 4
        avcc.push(0xE1); // 0xE0 | 1 → 1 SPS
        avcc.extend_from_slice(&3u16.to_be_bytes());
        avcc.extend_from_slice(&sps);
        avcc.push(1); // 1 PPS
        avcc.extend_from_slice(&1u16.to_be_bytes());
        avcc.extend_from_slice(&pps);

        let (params, nal_len) = parse_avcc(&avcc).unwrap();
        assert_eq!(nal_len, 4);
        assert_eq!(params, [
            0, 0, 0, 1, 0x67, 0x42, 0x1e,
            0, 0, 0, 1, 0x68,
        ]);
    }

    #[test]
    fn iter_boxes_walks_top_level_boxes() {
        // Two boxes: "ftyp" (8 bytes total, 0 content) and "moov" (12 bytes, 4 content).
        let data = [
            0, 0, 0, 8, b'f', b't', b'y', b'p',
            0, 0, 0, 12, b'm', b'o', b'o', b'v', 1, 2, 3, 4,
        ];
        let boxes: Vec<_> = iter_boxes(&data).collect();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].0, b"ftyp");
        assert!(boxes[0].1.is_empty());
        assert_eq!(boxes[1].0, b"moov");
        assert_eq!(boxes[1].1, &[1, 2, 3, 4]);
    }

    #[test]
    fn trun_with_a_truncated_first_sample_flags_is_refused() {
        // version 0, flags 0x000004: first_sample_flags present, but the body
        // holds nothing after sample_count. Advancing `pos` past the end would
        // underflow the capacity computation.
        let data = [0u8, 0x00, 0x00, 0x04, 0, 0, 0, 1];
        assert!(parse_trun(&data).is_err());
    }

    #[test]
    fn trun_capacity_is_capped_by_the_body() {
        // flags 0x000100: durations present. sample_count claims ten million
        // while the body holds none, so the Vec must stay near-empty instead
        // of reserving forty megabytes for a lie.
        let mut data = vec![0u8, 0x00, 0x01, 0x00];
        data.extend_from_slice(&10_000_000u32.to_be_bytes());
        let trun = parse_trun(&data).unwrap();
        assert_eq!(trun.sample_count, 10_000_000);
        assert!(trun.durations.is_empty());
        assert!(trun.durations.capacity() <= 4);
    }

    #[test]
    fn trun_with_no_per_sample_field_does_not_walk_the_count() {
        // flags 0x000001: data_offset only, nothing per sample. The declared
        // count is four billion, and the old loop spun on an empty body for
        // seconds - a twelve-byte box freezing the pipeline.
        let mut data = vec![0u8, 0x00, 0x00, 0x01];
        data.extend_from_slice(&u32::MAX.to_be_bytes());
        data.extend_from_slice(&0i32.to_be_bytes());
        let start = std::time::Instant::now();
        let trun = parse_trun(&data).unwrap();
        assert_eq!(trun.sample_count, u32::MAX);
        assert!(trun.durations.is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_run_naming_more_samples_than_it_carries_does_not_walk_them_all() {
        // The same count, one layer up: no per-sample size, no default size,
        // an empty mdat. Nothing can come out, so nothing should be walked.
        let run = TrackRun {
            track_id: 1,
            base_dts: 0,
            trun: TrunData {
                sample_count: u32::MAX,
                data_offset: None,
                durations: Vec::new(),
                sizes: Vec::new(),
                composition_offsets: Vec::new(),
                sample_flags: Vec::new(),
            },
            default_sample_duration: 0,
            default_sample_size: 0,
            default_sample_flags: 0,
            moof_size: 0,
        };
        let parser = MseParser::new();
        let mut samples = Vec::new();
        let mut audio = Vec::new();
        let start = std::time::Instant::now();
        parser
            .extract_samples(&run, &[], &mut samples, &mut audio)
            .unwrap();
        assert!(samples.is_empty());
        assert!(audio.is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn tkhd_reads_track_id_in_both_versions() {
        // Version 0: creation(4) + modification(4) + track_ID(4).
        let mut v0 = vec![0u8, 0, 0, 0];
        v0.extend_from_slice(&[0u8; 8]);
        v0.extend_from_slice(&7u32.to_be_bytes());
        assert_eq!(parse_tkhd(&v0), 7);

        // Version 1: creation(8) + modification(8) + track_ID(4).
        let mut v1 = vec![1u8, 0, 0, 0];
        v1.extend_from_slice(&[0u8; 16]);
        v1.extend_from_slice(&9u32.to_be_bytes());
        assert_eq!(parse_tkhd(&v1), 9);

        // A version-1 box too short to hold the field is not read as if it
        // were version 0.
        let mut short = vec![1u8, 0, 0, 0];
        short.extend_from_slice(&[0u8; 12]);
        assert_eq!(parse_tkhd(&short), 0);
    }
}
