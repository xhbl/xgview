//! What a surface can take, and what it is given.
//!
//! A picture decoded into a surface is handed to the compositor by rendering it,
//! and the compositor shows the newest one it has. Rendering more often than the
//! display refreshes therefore costs nothing but the render, while rendering
//! less often than the stream produces pictures loses the ones in between.
//!
//! This is compiled everywhere so the policy can be tested where the NDK
//! decoder cannot be built - the same reason [`crate::surface`] is.

use std::time::{Duration, Instant};

/// The shortest gap between two pictures handed to the surface's compositor,
/// while the session is feeding faster than a display can show.
///
/// A surface shows one picture per refresh, and holds the codec's output buffers
/// until it has shown them. A session that renders every picture it decodes is
/// therefore paced by the display rather than by what it is reading: a recording
/// played at eight times its speed is pinned to sixty pictures a second - the
/// codec waits for an output buffer it has already handed over, the session sees
/// every sample arrive late, and the rate the viewer asked for is never reached.
///
/// Only that session is thinned. See [`LIVE_FEED`] for why the gap cannot be
/// measured between the handovers of a live stream.
pub const RENDER_GAP: Duration = Duration::from_millis(25);

/// The slowest feed that still counts as a stream the display can keep up with.
///
/// A live camera's main stream is eight to forty pictures a second; a replay at
/// eight times its speed feeds a recording's frames that many times faster.
/// Below this every picture is handed over and the compositor chooses. Above it,
/// handovers are thinned by [`RENDER_GAP`].
///
/// The distinction has to be made on how fast the session feeds, and not on how
/// far apart the pictures come back. A decoder does not return its output one
/// picture per frame interval: it holds two or three and gives them back
/// together, so a gap measured between handovers throws away everything but the
/// first of each burst. Measured on the Shield, forty of the sixty pictures a
/// twelve frame a second doorbell produced never reached the screen - a stream
/// that moves four times a second, while the same stream in VLC moves at twelve.
/// The feed, by contrast, is one picture per frame interval whatever the codec
/// does with it.
pub const LIVE_FEED: Duration = Duration::from_millis(30);

/// The feed rate a session is taken at before it has one.
///
/// A stream that opens is live until it proves otherwise, which costs a replay a
/// few renders at its start and nothing else.
const FEED_UNKNOWN: Duration = Duration::from_millis(100);

/// Decides whether a picture goes to the surface's compositor.
#[derive(Debug, Default)]
pub struct Handover {
    /// How fast the session is feeding, smoothed: the interval between two
    /// access units.
    feed: Duration,
    /// When the previous access unit arrived, for `feed`.
    fed_at: Option<Instant>,
    /// When the last picture was handed over, or `None` before the first one, so
    /// that a session always renders the picture it opened on however soon after
    /// it opened.
    rendered_at: Option<Instant>,
}

impl Handover {
    /// Records one access unit, which is what sets the feed rate.
    pub fn fed(&mut self, now: Instant) {
        match self.fed_at {
            Some(last) => {
                let gap = now.saturating_duration_since(last).min(Duration::from_secs(1));
                self.feed = (self.feed * 3 + gap) / 4;
            }
            None => self.feed = FEED_UNKNOWN,
        }
        self.fed_at = Some(now);
    }

    /// Whether this picture is one to hand over.
    ///
    /// A live feed is handed over whole: a surface shows the newest picture it is
    /// given, so handing it one it cannot show costs a render and no frame,
    /// while skipping one of a burst is a picture the viewer never sees at all.
    pub fn take(&mut self, now: Instant) -> bool {
        if self.feed > LIVE_FEED {
            self.rendered_at = Some(now);
            return true;
        }
        if self
            .rendered_at
            .is_some_and(|last| now.duration_since(last) < RENDER_GAP)
        {
            return false;
        }
        self.rendered_at = Some(now);
        true
    }

    /// Ends a session: the next one's rate is its own, and the gap between two
    /// sessions is not a gap within one.
    pub fn reset(&mut self) {
        self.fed_at = None;
        self.rendered_at = None;
    }

    /// The feed rate in use, for the codec's own reporting.
    pub fn feed(&self) -> Duration {
        self.feed
    }

    /// Whether pictures are being thinned rather than handed over whole.
    pub fn thinned(&self) -> bool {
        self.feed <= LIVE_FEED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live feed hands over every picture the codec returns, however closely
    /// they come back: a decoder that holds three pictures and gives them back
    /// together must not lose two of them.
    #[test]
    fn a_live_feed_hands_every_picture_over() {
        let mut handover = Handover::default();
        let start = Instant::now();
        // Twelve frames a second, one access unit per frame interval.
        for frame in 0..12u64 {
            handover.fed(start + Duration::from_micros(frame * 83_333));
        }
        assert!(!handover.thinned(), "a twelve frame feed was read as a replay");
        // The codec gives three pictures back together.
        assert!(handover.take(start + Duration::from_millis(1000)));
        assert!(handover.take(start + Duration::from_millis(1000)), "a burst was thinned");
        assert!(handover.take(start + Duration::from_millis(1000)));
    }

    /// A session reading faster than the display is still thinned: eight times a
    /// recording's speed is eight times the renders, and a surface takes sixty.
    #[test]
    fn a_feed_faster_than_the_display_is_thinned() {
        let mut handover = Handover::default();
        let start = Instant::now();
        // Fifty access units a second, which is a replay at eight times a
        // twenty-five frame stream.
        for frame in 0..20u64 {
            handover.fed(start + Duration::from_millis(frame * 20));
        }
        assert!(handover.thinned(), "a fast feed was read as live");
        let now = start + Duration::from_secs(1);
        assert!(handover.take(now));
        assert!(!handover.take(now), "a replay faster than the display rendered twice");
        // A picture a frame later is worth rendering again.
        assert!(handover.take(now + RENDER_GAP));
    }

    /// A session that opens has no rate yet, and is taken as live.
    #[test]
    fn a_session_that_has_just_opened_hands_every_picture_over() {
        let mut handover = Handover::default();
        let now = Instant::now();
        handover.fed(now);
        assert!(handover.take(now));
        assert!(handover.take(now));
    }

    /// The rate is forgotten when the session ends, so the gap between two of
    /// them is not read as a rate.
    #[test]
    fn a_new_session_starts_its_own_rate() {
        let mut handover = Handover::default();
        let start = Instant::now();
        for frame in 0..20u64 {
            handover.fed(start + Duration::from_millis(frame * 20));
        }
        assert!(handover.thinned());
        handover.reset();
        let later = start + Duration::from_secs(30);
        handover.fed(later);
        assert!(!handover.thinned(), "the gap between sessions became a frame rate");
        assert!(handover.take(later));
        assert!(handover.take(later), "the new session's first burst was thinned");
    }
}
