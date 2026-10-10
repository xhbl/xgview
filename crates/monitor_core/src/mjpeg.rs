//! MJPEG over HTTP, the `multipart/x-mixed-replace` stream a Synology
//! Surveillance Station hands out as the low resolution stream of a camera.
//!
//! It is not RTSP and shares nothing with [`crate::rtsp`]: one HTTP `GET` stays
//! open and the server keeps writing parts at it, each part a complete JPEG
//! picture. The parts are framed twice over, which is the whole of the work
//! here:
//!
//! ```text
//! Transfer-Encoding: chunked          ->  de-chunked by the HTTP client
//! --boundary\r\n
//! Content-Type: image/jpeg\r\n
//! Content-Length: 44932\r\n
//! \r\n
//! <one JPEG picture>\r\n
//! --boundary\r\n ...
//! ```
//!
//! The transport chunking is left to the HTTP client, which also means an
//! `https://` NAS works without anything here knowing about TLS. The multipart
//! framing is parsed from the bytes as they arrive, because a part - and even a
//! header line - can be split across two reads and the reader has no say in
//! where the split falls.
//!
//! Pictures are JPEG, so they are handed to the decoder as they stand: a
//! standalone JPEG is the packet form libavcodec's MJPEG decoder takes, and
//! every picture is a key frame.
//!
//! # RTP/JPEG, the other way a camera sends the same codec
//!
//! RTSP can carry MJPEG too, as RFC 2435 describes it, and that path shares
//! nothing with the HTTP one beyond the codec. The RTP payload is *not* a JPEG:
//! it is the entropy coded scan of one, cut into fragments, each behind an
//! eight byte header that names its offset in the scan and the geometry of the
//! picture. Every marker a decoder needs - the quantization tables, the frame
//! header, the Huffman tables, the start of scan - has been stripped, and the
//! receiver has to put them back before there is anything to decode.
//!
//! ```text
//! 00 00 00 00 | 01 | ff | 50 | 3c         fragment offset, type, quality, blocks
//! 00 | 00 | 00 80 | <128 bytes of tables> reserved, precision, length
//! <scan data...>
//! ```
//!
//! [`JpegDepacketizer`] rebuilds the file: it reads the first fragment, builds
//! the header from the tables it carries and the geometry it announces, appends
//! every fragment in offset order and closes the picture with an `EOI` when the
//! marker bit says the last one has arrived. The Huffman tables are the ones
//! the JPEG standard fixes, because the format assumes them and never sends
//! them.

use std::time::Duration;

use crate::error::{CoreError, Result};
use crate::h264::AccessUnit;
use crate::rtsp::RtpHeader;

/// Longest header block accepted for one part.
const MAX_HEADERS: usize = 8 * 1024;
/// Longest part accepted. A part that grows past this was never terminated,
/// and buffering it further would only cost memory.
const MAX_PART: usize = 8 * 1024 * 1024;

/// An open MJPEG stream, one JPEG per part.
#[derive(Debug)]
pub struct MjpegClient {
    /// The still open response body; reading it de-chunks the transfer.
    response: reqwest::Response,
    /// Multipart framing of the body.
    parts: PartReader,
    /// Pictures handed out so far.
    frames: u64,
}

impl MjpegClient {
    /// Opens the stream and reads the multipart boundary it announced.
    pub async fn connect(url: &str) -> Result<Self> {
        // No total timeout: the response is the stream, and a timeout meant for
        // a request that ends would cut a live view off in the middle.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .danger_accept_invalid_certs(true)
            .build()?;
        let response = http.get(url).send().await?;
        if !response.status().is_success() {
            return Err(CoreError::network(format!(
                "mjpeg stream answered {} {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or("")
            )));
        }
        tracing::debug!(
            target: "xgview::mjpeg",
            status = response.status().as_u16(),
            version = ?response.version(),
            headers = ?response.headers(),
            "mjpeg response"
        );
        let boundary = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_boundary)
            .ok_or_else(|| CoreError::parse("mjpeg stream carries no multipart boundary"))?;
        tracing::debug!(
            target: "xgview::mjpeg",
            boundary = %String::from_utf8_lossy(&boundary),
            "mjpeg stream opened"
        );
        Ok(Self { response, parts: PartReader::new(boundary), frames: 0 })
    }

    /// Pictures read so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Reads until the next complete picture is in hand.
    pub async fn next_frame(&mut self) -> Result<Vec<u8>> {
        loop {
            if let Some(frame) = self.parts.take()? {
                self.frames += 1;
                return Ok(frame);
            }
            match self.response.chunk().await? {
                Some(chunk) => self.parts.push(&chunk),
                None => return Err(CoreError::network("mjpeg stream closed by the server")),
            }
            if self.parts.pending() > MAX_PART {
                return Err(CoreError::parse("mjpeg part is larger than the accepted maximum"));
            }
        }
    }
}

/// Reassembles the parts of a multipart body from whatever bytes have arrived.
///
/// Kept apart from the HTTP client so that the framing can be tested against a
/// synthetic stream, including the case that costs the most: a part split
/// across two reads.
#[derive(Debug)]
struct PartReader {
    boundary: Vec<u8>,
    buffer: Vec<u8>,
}

impl PartReader {
    fn new(boundary: Vec<u8>) -> Self {
        Self { boundary, buffer: Vec::new() }
    }

    /// Appends whatever the last read returned.
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Bytes held back while waiting for the rest of a part.
    fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Pulls one complete part out of the buffer, `None` until it is whole.
    fn take(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(mut cursor) = find_boundary(&self.buffer, &self.boundary) else {
            return Ok(None);
        };
        // A delimiter followed by `--` closes the stream rather than opening a
        // part.
        if self.buffer[cursor..].starts_with(b"--") {
            return Err(CoreError::network("mjpeg stream ended"));
        }
        // The delimiter is followed by a line break the part header follows.
        if self.buffer[cursor..].starts_with(b"\r\n") {
            cursor += 2;
        } else if self.buffer[cursor..].starts_with(b"\n") {
            cursor += 1;
        } else {
            // The rest of the delimiter line has not arrived yet.
            return Ok(None);
        }

        let Some(headers_end) = find(&self.buffer[cursor..], b"\r\n\r\n") else {
            if self.buffer.len() - cursor > MAX_HEADERS {
                return Err(CoreError::parse("mjpeg part headers are unreasonably long"));
            }
            return Ok(None);
        };
        let Some(length) = content_length(&self.buffer[cursor..cursor + headers_end]) else {
            return Err(CoreError::parse("mjpeg part carries no Content-Length"));
        };

        let body = cursor + headers_end + 4;
        if self.buffer.len() < body + length {
            // The picture itself is still on its way.
            return Ok(None);
        }
        let frame = self.buffer[body..body + length].to_vec();
        // The part and the line break behind it are of no further use, so they
        // are dropped rather than left to be skipped on every later call.
        self.buffer.drain(..body + length);
        Ok(Some(frame))
    }
}

/// Reads the boundary parameter out of a `multipart/x-mixed-replace` content
/// type, quoted or not.
fn parse_boundary(content_type: &str) -> Option<Vec<u8>> {
    let mut parameters = content_type.split(';');
    parameters.next()?;
    for parameter in parameters {
        let Some((key, value)) = parameter.split_once('=') else { continue };
        if key.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.as_bytes().to_vec());
            }
        }
    }
    None
}

/// `Content-Length` of a part header block, case insensitively.
fn content_length(headers: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(headers).ok()?;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        if key.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Index just past a `--boundary` delimiter standing on a line of its own.
///
/// The delimiter is not searched for anywhere: a JPEG body can hold any byte
/// sequence, so only an occurrence at the start of a line counts as framing.
fn find_boundary(buffer: &[u8], boundary: &[u8]) -> Option<usize> {
    let mut marker = Vec::with_capacity(boundary.len() + 2);
    marker.extend_from_slice(b"--");
    marker.extend_from_slice(boundary);

    let mut from = 0;
    while let Some(offset) = find(&buffer[from..], &marker) {
        let index = from + offset;
        let on_its_own_line = index == 0 || buffer[index - 2..index] == b"\r\n"[..];
        if on_its_own_line {
            return Some(index + marker.len());
        }
        from = index + 1;
    }
    None
}

/// First occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

// ------------------------------------------------------------------ RTP/JPEG

/// Segment markers the RTP payload leaves out, so they have to be written back.
const MARKER_SOI: u8 = 0xd8;
const MARKER_EOI: u8 = 0xd9;
const MARKER_APP0: u8 = 0xe0;
const MARKER_DQT: u8 = 0xdb;
const MARKER_DHT: u8 = 0xc4;
const MARKER_SOF0: u8 = 0xc0;
const MARKER_SOS: u8 = 0xda;
const MARKER_DRI: u8 = 0xdd;

/// Bytes of the header one RTP/JPEG fragment starts with.
const FRAGMENT_HEADER: usize = 8;

/// Standard quantization tables of the JPEG specification, section K.1, in
/// zig-zag order: luminance first, then chrominance.
///
/// They are the base a stream scales when its quality field names a table
/// instead of carrying one.
const STANDARD_QUANTIZERS: [u8; 128] = [
    16, 11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35,
    37, 29, 40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80, 109,
    81, 87, 95, 98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99, //
    17, 18, 18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

// The standard Huffman tables, section K.3.3. `bits` is 1-based the way the
// standard prints it: `bits[n]` counts the codes that are `n` bits long, and
// index 0 is never read.
const BITS_DC_LUMINANCE: [u8; 17] = [0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const BITS_DC_CHROMINANCE: [u8; 17] = [0, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const BITS_AC_LUMINANCE: [u8; 17] = [0, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const BITS_AC_CHROMINANCE: [u8; 17] = [0, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const VALUE_DC_LUMINANCE: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const VALUE_AC_LUMINANCE: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
    0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
    0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
    0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
    0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
    0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
    0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
    0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
    0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
    0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
    0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];
const VALUE_AC_CHROMINANCE: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
    0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
    0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
    0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
    0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
    0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
    0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
    0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
    0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
    0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
    0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

/// Reassembles the JPEG pictures of one RTP/JPEG (RFC 2435) session.
///
/// A picture is complete when the marker bit says so, so one packet completes
/// at most one picture. The fragments have to arrive in order and without a
/// gap: the format numbers them by offset, and a hole would leave a decoder
/// reading entropy coded data that has lost its place, which is why a picture
/// with a gap in it is dropped whole rather than stitched together.
#[derive(Debug, Default)]
pub struct JpegDepacketizer {
    /// Payload type of the video track. `None` accepts every payload.
    payload_type: Option<u8>,
    /// Header of the picture under assembly, built from its first fragment.
    header: Vec<u8>,
    /// Scan data of the picture under assembly.
    scan: Vec<u8>,
    /// RTP timestamp of the picture under assembly.
    timestamp: Option<u32>,
    /// Tables of the last picture that carried them, for one whose first
    /// fragment announces tables it does not repeat.
    carried_tables: Option<(u8, Vec<u8>)>,
}

impl JpegDepacketizer {
    /// Creates a depacketizer for the video track.
    pub fn new(payload_type: Option<u8>) -> Self {
        Self { payload_type, ..Self::default() }
    }

    /// Writes off the picture being assembled, after a gap in the RTP sequence
    /// numbers.
    pub fn invalidate(&mut self) {
        self.header.clear();
        self.scan.clear();
        self.timestamp = None;
    }

    /// Feeds one RTP packet, returning the picture its marker completed.
    pub fn push(&mut self, header: &RtpHeader, payload: &[u8]) -> Vec<AccessUnit> {
        if let Some(expected) = self.payload_type {
            if header.payload_type != expected {
                return Vec::new();
            }
        }
        if payload.len() < FRAGMENT_HEADER {
            return Vec::new();
        }

        let offset = u32::from_be_bytes([0, payload[1], payload[2], payload[3]]) as usize;
        let mut kind = payload[4];
        let quality = payload[5];
        let blocks_wide = payload[6];
        let blocks_high = payload[7];
        let mut body = &payload[FRAGMENT_HEADER..];

        // Bit 6 of the type byte says the fragment carries the restart interval
        // it was encoded with, in the two bytes that follow the fragment header
        // and the two after them that are unused. It has to be written back
        // into the file, because a decoder that meets restart markers without
        // being told the interval loses its place in the scan.
        let mut restart_interval = 0u16;
        if kind & 0x40 != 0 {
            if body.len() < 4 {
                return Vec::new();
            }
            restart_interval = u16::from_be_bytes([body[0], body[1]]);
            body = &body[4..];
            kind &= !0x40;
        }
        // Only the two YCbCr layouts are built here; anything else would need
        // sampling factors and component counts this does not write.
        if kind > 1 {
            tracing::debug!(target: "xgview::mjpeg", kind, "unsupported rtp/jpeg type, dropping the packet");
            return Vec::new();
        }

        if offset == 0 {
            let Some((tables, consumed)) = self.tables(quality, body) else {
                return Vec::new();
            };
            body = &body[consumed..];
            // A picture still open here lost its own end, so it is written off
            // before this one takes its place.
            self.header =
                jpeg_header(kind, blocks_wide, blocks_high, restart_interval, &tables);
            self.scan.clear();
            self.timestamp = Some(header.timestamp);
        } else {
            // A fragment that arrives before its first one, or carrying the
            // timestamp of another picture, belongs to something that was lost.
            if self.timestamp != Some(header.timestamp) {
                self.invalidate();
                return Vec::new();
            }
        }

        if offset != self.scan.len() {
            // Fragments in between never arrived, so what is held cannot be
            // decoded.
            self.invalidate();
            return Vec::new();
        }
        self.scan.extend_from_slice(body);

        if !header.marker {
            return Vec::new();
        }
        let mut data = std::mem::take(&mut self.header);
        data.extend_from_slice(&self.scan);
        data.extend_from_slice(&[0xff, MARKER_EOI]);
        let timestamp = self.timestamp.take().unwrap_or(header.timestamp);
        self.scan.clear();
        vec![AccessUnit { data, timestamp, keyframe: true }]
    }

    /// Quantization tables a picture is to be decoded with, and how many bytes
    /// of the fragment they occupied.
    ///
    /// A quality field of 127 or less names one of the standard tables at that
    /// quality; 128 and up means the first fragment carries the tables itself,
    /// or, when its length is zero, that the ones of an earlier picture are
    /// still in use.
    fn tables(&mut self, quality: u8, body: &[u8]) -> Option<(Vec<u8>, usize)> {
        if quality <= 127 {
            if quality == 0 || quality > 99 {
                return None;
            }
            let factor = u32::from(quality);
            let scale = if quality < 50 { 5000 / factor } else { 200 - factor * 2 };
            let tables = STANDARD_QUANTIZERS
                .iter()
                .map(|base| (((u32::from(*base) * scale + 50) / 100).clamp(1, 255)) as u8)
                .collect();
            return Some((tables, 0));
        }

        if body.len() < 4 {
            return None;
        }
        // One reserved byte, then the precision of the coefficients and the
        // length of the tables that follow.
        let precision = body[1];
        let length = usize::from(u16::from_be_bytes([body[2], body[3]]));
        if precision != 0 {
            tracing::debug!(target: "xgview::mjpeg", precision, "only 8 bit tables are built");
        }

        if length == 0 {
            return match &self.carried_tables {
                Some((carried, tables)) if *carried == quality => Some((tables.clone(), 4)),
                _ => None,
            };
        }
        if length % 64 != 0 || length > 4 * 64 || body.len() < 4 + length {
            return None;
        }
        let tables = body[4..4 + length].to_vec();
        if quality != 255 {
            self.carried_tables = Some((quality, tables.clone()));
        }
        Some((tables, 4 + length))
    }
}

/// Builds the JPEG header an RTP/JPEG payload omits.
///
/// `blocks_wide` and `blocks_high` are the picture's size in eight pixel
/// blocks, which is how the fragment header carries it. `kind` is the layout
/// the stream announced: 0 is sampled 4:2:2 and 1 is 4:2:0, which is what the
/// chroma sampling factors below encode. `restart_interval` is zero unless the
/// stream said its scan is cut by restart markers.
fn jpeg_header(
    kind: u8,
    blocks_wide: u8,
    blocks_high: u8,
    restart_interval: u16,
    tables: &[u8],
) -> Vec<u8> {
    let table_count = tables.len() / 64;
    let width = u16::from(blocks_wide) * 8;
    let height = u16::from(blocks_high) * 8;
    let mut out = Vec::with_capacity(640);

    // SOI and the JFIF frame every decoder expects to find.
    out.extend_from_slice(&[0xff, MARKER_SOI, 0xff, MARKER_APP0, 0x00, 16]);
    out.extend_from_slice(b"JFIF\0\x01\x02\x00\x00\x01\x00\x01\x00\x00");

    // DRI, only when the scan has restart markers for it to describe.
    if restart_interval != 0 {
        out.extend_from_slice(&[0xff, MARKER_DRI, 0x00, 4]);
        out.extend_from_slice(&restart_interval.to_be_bytes());
    }

    // DQT, one entry per table the stream carried.
    out.extend_from_slice(&[0xff, MARKER_DQT]);
    out.extend_from_slice(&(2 + table_count as u16 * 65).to_be_bytes());
    for index in 0..table_count {
        out.push(index as u8);
        out.extend_from_slice(&tables[index * 64..index * 64 + 64]);
    }

    // DHT, always the four standard tables: the format assumes them.
    out.extend_from_slice(&[0xff, MARKER_DHT, 0x00, 0x00]);
    let length_at = out.len() - 2;
    let mut length = 2;
    // The class is the high nibble and the table id the low one: DC luminance,
    // DC chrominance, AC luminance, AC chrominance.
    length += push_huffman_table(&mut out, 0x00, &BITS_DC_LUMINANCE, &VALUE_DC_LUMINANCE);
    length += push_huffman_table(&mut out, 0x01, &BITS_DC_CHROMINANCE, &VALUE_DC_LUMINANCE);
    length += push_huffman_table(&mut out, 0x10, &BITS_AC_LUMINANCE, &VALUE_AC_LUMINANCE);
    length += push_huffman_table(&mut out, 0x11, &BITS_AC_CHROMINANCE, &VALUE_AC_CHROMINANCE);
    out[length_at..length_at + 2].copy_from_slice(&(length as u16).to_be_bytes());

    // SOF0: three components, the luma one sampled as the stream announced and
    // the two chroma ones at half of it.
    out.extend_from_slice(&[0xff, MARKER_SOF0, 0x00, 17, 8]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.push(3);
    out.extend_from_slice(&[1, (2 << 4) | if kind == 0 { 1 } else { 2 }, 0]);
    let chroma_table = if table_count == 2 { 1 } else { 0 };
    out.extend_from_slice(&[2, 0x11, chroma_table]);
    out.extend_from_slice(&[3, 0x11, chroma_table]);

    // SOS, naming the same tables the frame header did.
    out.extend_from_slice(&[0xff, MARKER_SOS, 0x00, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    out
}

/// Appends one Huffman table and returns how many bytes it took.
fn push_huffman_table(out: &mut Vec<u8>, id: u8, bits: &[u8; 17], values: &[u8]) -> usize {
    out.push(id);
    // `bits` counts from one: index 0 is never used.
    let mut count = 0;
    for length in bits.iter().skip(1) {
        count += usize::from(*length);
        out.push(*length);
    }
    out.extend_from_slice(&values[..count]);
    count + 17
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG header and trailer around some body, which is all the reader
    /// promises to see of a picture.
    fn jpeg(body: usize) -> Vec<u8> {
        let mut picture = vec![0xff, 0xd8];
        picture.extend(std::iter::repeat(0x42).take(body));
        picture.extend_from_slice(&[0xff, 0xd9]);
        picture
    }

    /// One part, framed the way Surveillance Station frames it.
    fn part(boundary: &str, picture: &[u8]) -> Vec<u8> {
        let mut bytes = format!("--{boundary}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", picture.len()).into_bytes();
        bytes.extend_from_slice(picture);
        bytes.extend_from_slice(b"\r\n");
        bytes
    }

    #[test]
    fn reads_the_boundary_out_of_a_content_type() {
        assert_eq!(
            parse_boundary("multipart/x-mixed-replace;boundary=myboundary").as_deref(),
            Some(b"myboundary".as_slice())
        );
        // Real servers also put a space after the semicolon, or quote the value.
        assert_eq!(
            parse_boundary("multipart/x-mixed-replace; boundary=\"--abc\"").as_deref(),
            Some(b"--abc".as_slice())
        );
        assert_eq!(parse_boundary("multipart/x-mixed-replace"), None);
    }

    #[test]
    fn reads_one_picture_per_part() {
        let first = jpeg(16);
        let second = jpeg(32);
        let mut bytes = part("myboundary", &first);
        bytes.extend_from_slice(&part("myboundary", &second));

        let mut reader = PartReader::new(b"myboundary".to_vec());
        reader.push(&bytes);
        assert_eq!(reader.take().unwrap().as_deref(), Some(first.as_slice()));
        assert_eq!(reader.take().unwrap().as_deref(), Some(second.as_slice()));
        assert_eq!(reader.take().unwrap(), None);
    }

    #[test]
    fn waits_for_a_part_split_across_two_reads() {
        let picture = jpeg(64);
        let bytes = part("myboundary", &picture);
        // The split falls inside the header block, which is the case that
        // forces the reader to hold bytes back rather than parse what it has.
        let split = 40;
        let mut reader = PartReader::new(b"myboundary".to_vec());
        reader.push(&bytes[..split]);
        assert_eq!(reader.take().unwrap(), None);
        reader.push(&bytes[split..]);
        assert_eq!(reader.take().unwrap().as_deref(), Some(picture.as_slice()));
    }

    #[test]
    fn an_image_is_never_mistaken_for_a_boundary() {
        // Delimiter bytes in the middle of a line are picture data, however
        // much the tail of them looks like a part. Only a delimiter at the
        // start of a line is framing, and reading this as one would hand the
        // decoder four bytes of body as a picture of its own.
        let mut reader = PartReader::new(b"myboundary".to_vec());
        reader.push(b"picture data --myboundary\r\nContent-Length: 4\r\n\r\nabcd");
        assert_eq!(reader.take().unwrap(), None);
    }

    #[test]
    fn a_delimiter_that_closes_the_stream_is_not_a_part() {
        let mut bytes = part("myboundary", &jpeg(8));
        bytes.extend_from_slice(b"--myboundary--\r\n");
        let mut reader = PartReader::new(b"myboundary".to_vec());
        reader.push(&bytes);
        assert!(reader.take().unwrap().is_some());
        assert!(reader.take().is_err());
    }

    fn rtp(payload_type: u8, timestamp: u32, marker: bool) -> RtpHeader {
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

    /// A first fragment as a real 640x480 stream writes it: offset 0, layout 1,
    /// a dynamic table of two entries, 80 by 60 blocks. The four bytes of the
    /// table block - reserved, precision, length - are the part that has to be
    /// read before the tables themselves.
    fn first_fragment(scan: &[u8]) -> Vec<u8> {
        let mut payload = vec![0x00, 0x00, 0x00, 0x00, 0x01, 0xff, 0x50, 0x3c];
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x80]);
        payload.extend_from_slice(&[0x10u8; 128]);
        payload.extend_from_slice(scan);
        payload
    }

    /// A later fragment, numbered from where the first one left the scan.
    fn later_fragment(offset: u32, scan: &[u8]) -> Vec<u8> {
        let bytes = offset.to_be_bytes();
        let mut payload = vec![0x00, bytes[1], bytes[2], bytes[3], 0x01, 0xff, 0x50, 0x3c];
        payload.extend_from_slice(scan);
        payload
    }

    #[test]
    fn rebuilds_a_jpeg_from_the_fragments_of_one_picture() {
        let mut depacketizer = JpegDepacketizer::new(Some(96));
        assert!(depacketizer.push(&rtp(96, 7, false), &first_fragment(&[0x01; 10])).is_empty());

        let units = depacketizer.push(&rtp(96, 7, true), &later_fragment(10, &[0x02; 6]));
        assert_eq!(units.len(), 1, "the marker closes the picture");
        let jpeg = &units[0].data;
        assert!(units[0].keyframe, "every jpeg is a key frame");
        assert_eq!(units[0].timestamp, 7);

        assert_eq!(&jpeg[..2], &[0xff, 0xd8], "the file opens with an SOI");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xff, 0xd9], "and closes with an EOI");
        // The frame header names the geometry the fragment header announced:
        // 80 by 60 blocks of eight pixels, height before width.
        let sof = find(jpeg, &[0xff, 0xc0]).expect("a frame header");
        assert_eq!(&jpeg[sof + 5..sof + 9], &[0x01, 0xe0, 0x02, 0x80]);
        // Both quantization tables came over, and the Huffman tables that the
        // format assumes were written in.
        let dqt = find(jpeg, &[0xff, 0xdb]).expect("quantization tables");
        assert_eq!(u16::from_be_bytes([jpeg[dqt + 2], jpeg[dqt + 3]]), 2 + 2 * 65);
        assert!(find(jpeg, &[0xff, 0xc4]).is_some(), "huffman tables");
        assert!(find(jpeg, &[0xff, 0xda]).is_some(), "start of scan");
        // And the scan data of both fragments is in the picture, in order.
        assert!(find(jpeg, &[0x01; 10]).is_some());
        assert!(find(jpeg, &[0x02; 6]).is_some());
    }

    #[test]
    fn writes_a_restart_interval_only_when_the_stream_declared_one() {
        let plain = jpeg_header(1, 80, 60, 0, &[0x10u8; 128]);
        assert!(find(&plain, &[0xff, 0xdd]).is_none(), "no restart markers, no DRI");

        // A stream that says its scan is cut by restart markers has to have the
        // interval written back, or a decoder meeting an `RSTn` has no idea
        // where it is.
        let cut = jpeg_header(1, 80, 60, 4, &[0x10u8; 128]);
        let dri = find(&cut, &[0xff, 0xdd, 0x00, 0x04]).expect("a restart interval");
        assert_eq!(&cut[dri + 4..dri + 6], &[0x00, 0x04]);
    }

    #[test]
    fn writes_the_four_standard_huffman_tables() {
        let header = jpeg_header(1, 80, 60, 0, &[0x10u8; 128]);
        let dht = find(&header, &[0xff, 0xc4]).expect("huffman tables");
        let mut at = dht + 4;
        // The table id carries its class in the high nibble: DC luminance, DC
        // chrominance, AC luminance, AC chrominance. A wrong nibble here is a
        // JPEG no decoder will read.
        for expected in [0x00u8, 0x01, 0x10, 0x11] {
            assert_eq!(header[at], expected, "huffman table id at {at}");
            let values: usize = header[at + 1..at + 17].iter().map(|n| usize::from(*n)).sum();
            at += 17 + values;
        }
        // The segment length covers exactly those four tables.
        let length = usize::from(u16::from_be_bytes([header[dht + 2], header[dht + 3]]));
        assert_eq!(dht + 2 + length, at);
    }

    #[test]
    fn a_gap_in_the_fragments_throws_the_picture_away() {
        let mut depacketizer = JpegDepacketizer::new(Some(96));
        assert!(depacketizer.push(&rtp(96, 7, false), &first_fragment(&[0x01; 10])).is_empty());

        // The next fragment is numbered where the one after it should have
        // been, so the bytes in between never arrived.
        assert!(depacketizer.push(&rtp(96, 7, true), &later_fragment(20, &[0x02; 6])).is_empty());
    }

    #[test]
    fn a_fragment_of_another_picture_is_dropped() {
        let mut depacketizer = JpegDepacketizer::new(Some(96));
        assert!(depacketizer.push(&rtp(96, 7, false), &first_fragment(&[0x01; 10])).is_empty());

        // Same offset, different timestamp: a new picture began and this
        // fragment belongs to the one before it.
        assert!(depacketizer.push(&rtp(96, 8, true), &later_fragment(10, &[0x02; 6])).is_empty());
    }

    #[test]
    fn a_quality_field_rebuilds_the_standard_tables() {
        // 75 is a quality, not a table id: the tables are the standard ones
        // scaled by it, and the picture still comes out whole.
        let mut depacketizer = JpegDepacketizer::new(Some(96));
        let payload = vec![0x00, 0x00, 0x00, 0x00, 0x01, 75, 0x50, 0x3c, 0x11, 0x22];
        let units = depacketizer.push(&rtp(96, 1, true), &payload);
        assert_eq!(units.len(), 1);

        let jpeg = &units[0].data;
        let dqt = find(jpeg, &[0xff, 0xdb]).expect("quantization tables");
        assert_eq!(u16::from_be_bytes([jpeg[dqt + 2], jpeg[dqt + 3]]), 2 + 2 * 65);
        // The first luma value of the standard table at quality 75.
        let scale = 200 - 75 * 2;
        assert_eq!(jpeg[dqt + 5], ((16 * scale + 50) / 100) as u8);
    }

    #[test]
    fn a_part_without_a_length_is_rejected() {
        let mut reader = PartReader::new(b"myboundary".to_vec());
        reader.push(b"--myboundary\r\nContent-Type: image/jpeg\r\n\r\n\xff\xd8");
        assert!(reader.take().is_err());
    }
}
