//! Whether a codec hands its pictures over in the order they are shown in.
//!
//! A decoder left alone reorders by the presentation times it was given and
//! hands pictures over in display order. Asked for low latency it hands each one
//! over as it finishes it, which for a stream with B-frames is not that order:
//! the sum of the pictures in a GOP goes out before the ones it is shown
//! between, and a viewer sees an older picture after a newer one on every GOP -
//! a regular shudder, measured in `docs/DevMemo.md` §17.
//!
//! Which of the two a stream needs is not something the NDK can be asked, and
//! not something the SDP says, so it is measured from the pictures themselves:
//! this counts the ones that come out before their predecessors.
//!
//! Compiled everywhere so the rule can be tested where the NDK decoder cannot be
//! built - the same reason [`crate::handover`] and [`crate::surface`] are.

/// A jump back by this much is a replay seeking, not a picture out of order.
///
/// A reorder is worth a frame or two - tens or hundreds of milliseconds for the
/// frame rates a camera produces - while a seek is worth seconds. Without the
/// distinction every seek in a recording would take a session's low latency
/// away for a fault that only lasted one picture.
pub const SEEK_JUMP_US: i64 = 2_000_000;

/// How many pictures must come out before their predecessors before the stream
/// is taken to need reordering.
///
/// A stream with B-frames breaks the order on every GOP, so two is reached
/// within a few frames. One alone could be the same picture reported twice or a
/// timestamp a camera repeated, and reopening a healthy session on that would
/// cost a stall for nothing.
pub const REORDERED_LIMIT: u32 = 2;

/// What the order of a codec's output says about the stream behind it.
#[derive(Debug, Default)]
pub struct Order {
    /// The presentation time of the last picture seen.
    last_pts_us: Option<i64>,
    /// Pictures that came out before their predecessors.
    reordered: u32,
}

impl Order {
    /// Records one picture, by the presentation time the codec gave it.
    ///
    /// Answers whether this one came out before the picture ahead of it, which
    /// is the only evidence a reordering stream leaves.
    pub fn saw(&mut self, pts_us: i64) -> bool {
        let out_of_order = self
            .last_pts_us
            .is_some_and(|last| pts_us < last && last - pts_us < SEEK_JUMP_US);
        self.last_pts_us = Some(pts_us);
        if out_of_order {
            self.reordered = self.reordered.saturating_add(1);
        }
        out_of_order
    }

    /// Pictures seen out of order so far.
    pub fn reordered(&self) -> u32 {
        self.reordered
    }

    /// Whether the stream has shown enough disorder to be worth the decoder's
    /// own reordering, which costs its depth in delay.
    pub fn needs_reordering(&self) -> bool {
        self.reordered >= REORDERED_LIMIT
    }

    /// Forgets the pictures seen: a new session starts its own order.
    pub fn reset(&mut self) {
        self.last_pts_us = None;
    }

    /// Forgets everything, including what the stream was seen to do.
    ///
    /// For a new stream, whose order says nothing about the one before it - as
    /// opposed to [`Self::reset`], which is a session reopening on the same one.
    pub fn clear(&mut self) {
        self.reset();
        self.reordered = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame is 83 milliseconds, which is the twelve frame stream this was
    /// written against.
    const FRAME_US: i64 = 83_333;

    /// Pictures in display order are not out of order, however unevenly they
    /// come: a decoder that holds three and gives them back together is still
    /// handing them over in order.
    #[test]
    fn display_order_is_not_reordering() {
        let mut order = Order::default();
        for index in 0..30 {
            assert!(!order.saw(index * FRAME_US));
        }
        assert_eq!(order.reordered(), 0);
        assert!(!order.needs_reordering());
    }

    /// The order measured on a stream with B-frames: a P picture, then the two
    /// B pictures it is shown between, and so on through the GOP.
    #[test]
    fn pictures_that_come_out_before_their_predecessors_are_counted() {
        let mut order = Order::default();
        // `2, 7, 5, 4, 6, 11, 9, 8, 10`, the order `docs/DevMemo.md` §17
        // measured - the group of pictures is decoded in a different order from
        // the one it is shown in.
        for index in [2u32, 7, 5, 4, 6, 11, 9, 8, 10] {
            order.saw(index as i64 * FRAME_US);
        }
        assert_eq!(order.reordered(), 4, "5, 4, 9 and 8 each came out early");
        assert!(order.needs_reordering());
    }

    /// A seek is seconds backwards, not a frame: it must not be read as a
    /// reordering stream, or every skip in a recording would cost the session
    /// its low latency.
    #[test]
    fn a_seek_is_not_reordering() {
        let mut order = Order::default();
        order.saw(60_000_000);
        assert!(!order.saw(1_000_000));
        assert_eq!(order.reordered(), 0);
        assert!(!order.needs_reordering());
    }

    /// A session that reopens on the same stream starts its order again: the
    /// first picture of the new session can be older than the last of the old
    /// one without the stream having done anything wrong.
    #[test]
    fn a_new_session_starts_its_order_again() {
        let mut order = Order::default();
        order.saw(120 * FRAME_US);
        order.saw(121 * FRAME_US);
        order.reset();
        assert!(!order.saw(FRAME_US), "a session change read as a reorder");
        assert_eq!(order.reordered(), 0);
    }

    /// What the last stream needed says nothing about the next one.
    #[test]
    fn a_new_stream_forgets_the_last_ones_order() {
        let mut order = Order::default();
        for index in [2u32, 7, 5, 4] {
            order.saw(index as i64 * FRAME_US);
        }
        assert!(order.needs_reordering());
        order.clear();
        assert!(!order.needs_reordering());
        assert!(!order.saw(0));
    }
}
