//! The little of RTCP (RFC 3550) a viewer has to send.
//!
//! A receiver is expected to report back what it received. Cameras differ in how
//! much they care: some stream regardless, one FOSCAM sub stream was measured
//! sending a single key frame and then nothing but per picture `SEI` until a
//! receiver report arrived, which a player shows as a slideshow whose timecode
//! jumps. Sending the report costs one small packet every few seconds.

/// Payload type of a receiver report.
pub const RECEIVER_REPORT: u8 = 201;

/// Number of bytes a receiver report with one report block occupies.
pub const RECEIVER_REPORT_LEN: usize = 32;

/// Builds a receiver report carrying a single report block.
///
/// `highest_sequence` is the extended highest sequence number received, i.e. the
/// 16 bit RTP sequence number with the count of its cycles folded into the high
/// half. `packets_lost` is signed and counts what the stream never delivered,
/// which over TCP is normally zero.
pub fn receiver_report(
    receiver_ssrc: u32,
    source_ssrc: u32,
    highest_sequence: u32,
    packets_lost: i32,
    jitter: u32,
) -> [u8; RECEIVER_REPORT_LEN] {
    let mut report = [0u8; RECEIVER_REPORT_LEN];
    // Version 2, no padding, no reception report count beyond the one block.
    report[0] = 0x80 | 1;
    report[1] = RECEIVER_REPORT;
    // Length in 32 bit words, minus one, covers the header and the block.
    report[2..4].copy_from_slice(&7u16.to_be_bytes());
    report[4..8].copy_from_slice(&receiver_ssrc.to_be_bytes());
    report[8..12].copy_from_slice(&source_ssrc.to_be_bytes());
    // The fraction lost stays zero: a TCP stream arrives in order and whole, so
    // the only loss to report is a gap in the sequence numbers, which the
    // cumulative count below already covers.
    let lost = packets_lost.clamp(-0x80_0000, 0x7f_ffff).to_be_bytes();
    report[13..16].copy_from_slice(&lost[1..4]);
    report[16..20].copy_from_slice(&highest_sequence.to_be_bytes());
    report[20..24].copy_from_slice(&jitter.to_be_bytes());
    // No sender report has been seen, so the timestamp of the last one and the
    // delay since it have nothing to report and stay zero.
    report[24..32].fill(0);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_well_formed() {
        let report = receiver_report(0x1111_2222, 0x3333_4444, 65_537, 0, 0);
        assert_eq!(report.len(), RECEIVER_REPORT_LEN);
        assert_eq!(report[0], 0x81, "version 2 with one report block");
        assert_eq!(report[1], RECEIVER_REPORT);
        assert_eq!(u16::from_be_bytes([report[2], report[3]]), 7);
        assert_eq!(&report[4..8], &0x1111_2222u32.to_be_bytes());
        assert_eq!(&report[8..12], &0x3333_4444u32.to_be_bytes());
        assert_eq!(&report[16..20], &65_537u32.to_be_bytes());
    }

    #[test]
    fn the_lost_count_is_signed_and_twenty_four_bits_wide() {
        // Byte 12 is the fraction lost, the cumulative count is the three bytes
        // after it.
        let negative = receiver_report(0, 0, 0, -1, 0);
        assert_eq!(negative[12], 0);
        assert_eq!(&negative[13..16], &[0xff, 0xff, 0xff]);

        let saturation = receiver_report(0, 0, 0, i32::MAX, 0);
        assert_eq!(&saturation[13..16], &[0x7f, 0xff, 0xff]);
    }
}
