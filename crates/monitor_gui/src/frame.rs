//! How long a frame took, and what was on it.
//!
//! A stutter is a number with a moment attached, and the moment is the part
//! that costs work to guess at: two streams on the wall, a panel drawn over
//! them, a magnified picture at full size, a call in progress. This measures the
//! frame and, when one runs past a threshold, writes one line saying how long it
//! took and what was being drawn at the time.
//!
//! Off unless `XGVIEW_FRAME_MS` names a threshold in milliseconds:
//!
//! ```text
//! XGVIEW_FRAME_MS=16 RUST_LOG=xgview::frame=info xgview
//! ```
//!
//! What it costs while off is one `Option` check per frame: the two clocks are
//! only read when a threshold is set. A line is written at most once a second,
//! and the frames swallowed in between are counted into the next one, so a wall
//! that stutters on every frame cannot drown the log it is meant to explain.
//!
//! Only the time spent inside the frame is measured - the work the viewer does
//! between one repaint and the next, not the wait for the display. That is the
//! part the viewer can do something about; a frame that is short here and still
//! arrives late is the compositor's or the GPU's.

use std::time::{Duration, Instant};

/// The threshold named by `XGVIEW_FRAME_MS`, and the bookkeeping that keeps a
/// bad run from filling the log.
pub struct FrameClock {
    /// Frames at or over this are reported.
    threshold: Duration,
    /// When the last line was written, if one ever was.
    last_report: Option<Instant>,
    /// Frames that ran long and were not reported, because one had just been.
    swallowed: u32,
}

impl FrameClock {
    /// The clock the environment asks for, if it asks for one.
    ///
    /// A missing variable, an unparsable one and a zero all mean the same thing:
    /// nobody is asking, so nothing is measured.
    pub fn from_env() -> Option<Self> {
        let ms: u64 = std::env::var("XGVIEW_FRAME_MS").ok()?.trim().parse().ok()?;
        if ms == 0 {
            return None;
        }
        Some(Self {
            threshold: Duration::from_millis(ms),
            last_report: None,
            swallowed: 0,
        })
    }

    /// The clock to hand back to [`Self::report`] when the frame is over.
    pub fn start(&self) -> Instant {
        Instant::now()
    }

    /// Whether a frame that took this long is worth a line.
    ///
    /// Separate from [`Self::report`] so the caller can build the description of
    /// what was on screen only when it is going to be used: it allocates, and a
    /// frame that is not being reported should not pay for it.
    pub fn wants(&self, took: Duration) -> bool {
        took >= self.threshold
    }

    /// Writes the line, unless one was written less than a second ago.
    pub fn report(&mut self, took: Duration, scene: &str) {
        self.swallowed += 1;
        // `map_or` rather than `is_none_or`, which the workspace's 1.80 floor
        // does not have.
        let due = self.last_report.map_or(true, |at| at.elapsed() >= Duration::from_secs(1));
        if !due {
            return;
        }
        let swallowed = std::mem::take(&mut self.swallowed);
        self.last_report = Some(Instant::now());
        tracing::info!(
            target: "xgview::frame",
            ms = took.as_secs_f64() * 1000.0,
            swallowed = swallowed - 1,
            scene,
            "a frame ran long"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock with the given threshold, as `from_env` would build one.
    fn clock(threshold: u64) -> FrameClock {
        FrameClock {
            threshold: Duration::from_millis(threshold),
            last_report: None,
            swallowed: 0,
        }
    }

    #[test]
    fn a_frame_at_the_threshold_is_wanted_and_one_under_it_is_not() {
        let clock = clock(16);
        assert!(!clock.wants(Duration::from_millis(15)));
        assert!(clock.wants(Duration::from_millis(16)));
        assert!(clock.wants(Duration::from_millis(400)));
    }

    /// The count is of the frames the line does not mention, so the frame it is
    /// about is not one of them - and a burst inside the second is not lost, it
    /// is added to the next line.
    #[test]
    fn a_run_of_long_frames_is_written_once_a_second_and_counted() {
        let mut clock = clock(16);
        clock.report(Duration::from_millis(40), "wall");
        assert_eq!(clock.swallowed, 0, "the frame that was reported is not swallowed");
        assert!(clock.last_report.is_some());

        clock.report(Duration::from_millis(40), "wall");
        clock.report(Duration::from_millis(40), "wall");
        assert_eq!(clock.swallowed, 2, "both were inside the second and are waiting");
    }
}
