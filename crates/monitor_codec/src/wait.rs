//! How long a decoder waits for its own buffers before giving a picture up.
//!
//! A hardware codec holds the input buffers it was given until it has worked
//! through them, so a decoder that feeds it faster than it decodes has to wait
//! for one to come back. Giving the picture up instead is expensive: H.264
//! cannot be resumed past a picture the codec never saw, so the channel stays
//! blank until the next key frame - seconds later - and a run of them is what
//! the session's stall detector reads as a frozen stream.
//!
//! So the ceiling grows when the codec is falling behind and shrinks back when
//! it is not. It is a ceiling and not a delay: a buffer that is there is taken
//! on the first, non-blocking try, so a machine that keeps up never waits at
//! all and this never shows up in its behaviour.

use std::time::Duration;

/// The shortest wait, and what a decoder that keeps up needs.
///
/// A buffer that has to be waited for at all is a codec that is behind; 100 ms
/// is long enough for the frame it is working on to be finished by any codec
/// that can carry the stream at its rate.
pub const WAIT_MIN: Duration = Duration::from_millis(100);

/// The longest wait.
///
/// About a picture on the slowest hardware that still decodes - measured at
/// 400 ms for a 5 MP stream on a Mali-G310 - and no longer, so a codec that has
/// stopped answering altogether gives the picture up rather than blocking the
/// channel for a second.
pub const WAIT_MAX: Duration = Duration::from_millis(500);

/// How long to wait next time, given how long this one took.
///
/// Growth is doubling and happens *before* a picture is lost: a wait that used
/// half the ceiling is a codec that will not have a buffer in time the next
/// time round. Shrinking is a quarter at a time and only when the wait was
/// nowhere near the ceiling, so a stream that has settled stays settled instead
/// of oscillating around the ceiling it needs.
pub fn next_wait(current: Duration, took: Duration, got_a_buffer: bool) -> Duration {
    let doubled = (current * 2).min(WAIT_MAX);
    if !got_a_buffer || took >= current / 2 {
        doubled
    } else if took < current / 4 {
        (current - current / 4).max(WAIT_MIN)
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_codec_that_keeps_up_waits_the_floor_and_no_more() {
        // A buffer on the first try: nothing to wait for, and no reason to
        // keep a ceiling that has already shrunk to the floor.
        assert_eq!(next_wait(WAIT_MIN, Duration::ZERO, true), WAIT_MIN);
        assert_eq!(next_wait(WAIT_MIN * 4, Duration::from_millis(1), true), WAIT_MIN * 3);
    }

    #[test]
    fn a_wait_that_used_half_the_ceiling_doubles_it() {
        let current = Duration::from_millis(200);
        let took = Duration::from_millis(100);
        assert_eq!(next_wait(current, took, true), Duration::from_millis(400));
    }

    #[test]
    fn a_lost_picture_doubles_the_ceiling() {
        let current = Duration::from_millis(150);
        assert_eq!(next_wait(current, current, false), Duration::from_millis(300));
    }

    #[test]
    fn the_ceiling_never_passes_its_bound() {
        assert_eq!(next_wait(WAIT_MAX, WAIT_MAX, false), WAIT_MAX);
        assert_eq!(next_wait(WAIT_MAX, Duration::from_secs(5), true), WAIT_MAX);
        assert_eq!(next_wait(WAIT_MIN, Duration::from_secs(5), true), WAIT_MIN * 2);
    }

    #[test]
    fn a_wait_in_between_leaves_the_ceiling_alone() {
        let current = Duration::from_millis(400);
        let took = Duration::from_millis(150);
        assert_eq!(next_wait(current, took, true), current);
    }
}
