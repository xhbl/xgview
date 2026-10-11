//! Keeping the viewer up.
//!
//! `--supervise` makes a process a supervisor rather than a viewer: it starts the
//! viewer as a child - itself, without that switch - waits, and starts it again
//! when it dies without having been asked to stop. Nothing about a window belongs
//! here, which is what makes it testable without starting anything: this module
//! is only the policy over the endings, and the loop that spawns and waits lives
//! where it is used (the viewer binary, and the command line entry point beside
//! it).
//!
//! The numbers are Android's, which has supervised its own restarts since before
//! this existed: `MainActivity` allows three restarts in ten minutes and then
//! leaves the viewer down, and comes back after 700 ms rather than backing off -
//! the point being to get the wall back, not to spare the cpu. What differs is
//! where the restarts are counted: the activity has to keep them in shared
//! preferences because the process that made them is gone, while a supervisor is
//! still here to remember its own.

use std::process::ExitStatus;
use std::time::{Duration, Instant};

/// The switch that makes a process a supervisor instead of a viewer.
pub const SWITCH: &str = "--supervise";

/// Exit code of a viewer that stopped because it was asked to: the window was
/// closed, or another instance holds the configuration.
pub const STOPPED: i32 = 0;

/// Exit code of a viewer that cannot run on this machine at all - there is no
/// renderer to be had here. Starting it again would only put the same question
/// twice, so a supervisor leaves it down. See `monitor_gui::CannotDraw`.
pub const CANNOT_RUN: i32 = 2;

/// How many times a viewer may be started again within [`WINDOW`].
pub const LIMIT: usize = 3;

/// The span the restarts are counted over. Older ones are forgotten, which is
/// what keeps a viewer that dies once a quarter of an hour from ever adding up.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);

/// How long to wait before starting the viewer again.
pub const DELAY: Duration = Duration::from_millis(700);

/// How a run of the viewer ended, as far as a supervisor cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Asked to stop, and it did. There is nothing left to watch.
    AskedToStop,
    /// It cannot draw here at all.
    CannotRun,
    /// It died: it crashed, or something killed it.
    Died,
}

/// What a supervisor does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Leave the viewer down, handing its exit code on to whoever is waiting.
    LeaveItDown,
    /// Start it again after this long.
    Again(Duration),
}

/// How one run of the viewer ended, from the status it exited with.
///
/// A signal on unix, an exception code on windows, or any code that is not one
/// of the two above: the viewer did not choose to go.
pub fn ending(status: ExitStatus) -> Ending {
    ending_of(status.code())
}

/// The ending an exit code means. Split out because a code is the whole of what
/// a status carries that matters here, and a status is awkward to make by hand.
fn ending_of(code: Option<i32>) -> Ending {
    match code {
        Some(STOPPED) => Ending::AskedToStop,
        Some(CANNOT_RUN) => Ending::CannotRun,
        _ => Ending::Died,
    }
}

/// The restarts that have been made, and the policy over them.
#[derive(Debug, Default)]
pub struct Record {
    restarts: Vec<Instant>,
}

impl Record {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decides what to do about a viewer that has just ended.
    pub fn decide(&mut self, ending: Ending, now: Instant) -> Next {
        match ending {
            // Neither of these is something another launch could change: one was
            // asked for, and the other is this machine's answer.
            Ending::AskedToStop | Ending::CannotRun => Next::LeaveItDown,
            Ending::Died => {
                self.forget(now);
                if self.restarts.len() >= LIMIT {
                    return Next::LeaveItDown;
                }
                self.restarts.push(now);
                Next::Again(DELAY)
            }
        }
    }

    /// How many restarts fall within the window that ends at `now`, for the log
    /// line that explains giving up.
    pub fn recent(&self, now: Instant) -> usize {
        self.restarts
            .iter()
            .filter(|at| now.duration_since(**at) < WINDOW)
            .count()
    }

    /// Drops the restarts that have fallen out of the window.
    fn forget(&mut self, now: Instant) {
        self.restarts.retain(|at| now.duration_since(*at) < WINDOW);
    }

    /// [`decide`](Self::decide), with the words for what it decided in the log.
    ///
    /// The decision is the same wherever a supervisor runs, so it is said in one
    /// place rather than in each of them; the caller does the starting and the
    /// waiting.
    pub fn watch(&mut self, status: ExitStatus, now: Instant) -> Next {
        let ending = ending(status);
        let next = self.decide(ending, now);
        match next {
            Next::LeaveItDown => {
                // Only a death can have reached the limit - the other two
                // endings are the viewer saying that it is finished.
                if ending == Ending::Died {
                    tracing::error!(
                        target: "xgview",
                        "started the viewer again {} times within {} minutes; leaving it down",
                        self.recent(now),
                        WINDOW.as_secs() / 60
                    );
                }
            }
            Next::Again(delay) => {
                tracing::error!(
                    target: "xgview",
                    code = ?status.code(),
                    "the viewer stopped; starting it again in {} ms",
                    delay.as_millis()
                );
            }
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A viewer that stopped when it was asked to, or could not run, is left
    /// alone - whatever the supervisor has seen before.
    #[test]
    fn a_viewer_that_stopped_or_cannot_run_is_not_started_again() {
        let mut record = Record::new();
        let now = Instant::now();
        assert_eq!(record.decide(Ending::AskedToStop, now), Next::LeaveItDown);
        assert_eq!(record.decide(Ending::CannotRun, now), Next::LeaveItDown);
        assert_eq!(record.recent(now), 0, "neither counts as a restart");
    }

    /// A death is answered by starting it again, at once rather than by backing
    /// off: the wall being up matters more than the cpu.
    #[test]
    fn a_death_is_answered_by_starting_it_again() {
        let mut record = Record::new();
        assert_eq!(record.decide(Ending::Died, Instant::now()), Next::Again(DELAY));
        assert!(DELAY < Duration::from_secs(1), "the wall comes back quickly");
    }

    /// Three restarts are allowed; the fourth death within the window is the end
    /// of it. Leaving it down is the whole point of the limit - a viewer that
    /// cannot come up must not be started for ever.
    #[test]
    fn the_fourth_death_within_the_window_leaves_it_down() {
        let mut record = Record::new();
        let start = Instant::now();
        for restart in 0..LIMIT {
            let now = start + Duration::from_secs(restart as u64);
            assert_eq!(
                record.decide(Ending::Died, now),
                Next::Again(DELAY),
                "restart {restart} is within the limit of {LIMIT}"
            );
        }
        let now = start + Duration::from_secs(LIMIT as u64);
        assert_eq!(record.decide(Ending::Died, now), Next::LeaveItDown);
        assert_eq!(record.recent(now), LIMIT, "and it can say how many it was");
    }

    /// The window is what makes the limit livable: deaths far enough apart are
    /// forgotten, so a viewer that dies every quarter of an hour is started
    /// again every time, for ever.
    #[test]
    fn deaths_beyond_the_window_are_forgotten() {
        let mut record = Record::new();
        let mut now = Instant::now();
        for _ in 0..10 {
            assert_eq!(record.decide(Ending::Died, now), Next::Again(DELAY));
            now += WINDOW;
        }
        assert_eq!(record.recent(now), 0, "nothing is left inside the window");
    }

    /// The two codes the contract names, and everything else.
    #[test]
    fn the_exit_codes_the_viewer_uses_are_read_as_they_are_meant() {
        assert_eq!(ending_of(Some(STOPPED)), Ending::AskedToStop);
        assert_eq!(ending_of(Some(CANNOT_RUN)), Ending::CannotRun);
        assert_eq!(ending_of(Some(1)), Ending::Died, "an error is a death");
        assert_eq!(ending_of(Some(101)), Ending::Died, "a panic is a death");
        assert_eq!(ending_of(Some(-1073741819)), Ending::Died, "a windows access violation");
        assert_eq!(ending_of(None), Ending::Died, "a signal on unix");
    }
}
