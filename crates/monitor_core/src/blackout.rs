//! The blackout schedule: when the wall should stop showing anything.
//!
//! Two things live here that the rest of the crate should not have to know
//! about - reading the local clock, and answering the one question the wall
//! asks: is one of its periods in force right now.
//!
//! The clock is read through each platform's own API rather than through a
//! date/time crate, which this workspace does not carry. It has to be the local
//! clock and not an offset worked out once: a period that says 22:00 has to
//! mean the 22:00 of the person who wrote it, and where daylight saving is
//! observed that is a different offset for half the year. `GetLocalTime` and
//! `localtime_r` both apply the rules currently in effect.

use crate::config::{Blackout, BlackoutWindow, MINUTES_PER_WEEK};

/// What the local clock says, in the pieces a viewer reads.
///
/// The weekday is counted from Monday, which is how the schedule counts, so the
/// two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// The local wall clock, or `None` on a platform this build cannot read one on.
///
/// A missing clock is not worth reporting as an error. The answer to "should
/// the wall be blank now" is simply no, and a wall that carries on showing
/// pictures is a better failure than one that blanks at an hour nobody asked
/// for.
pub fn local_now() -> Option<LocalTime> {
    imp::local_now()
}

/// Which moment of the week it is locally, as minutes from Monday 00:00.
pub fn minute_of_week() -> Option<u32> {
    let now = local_now()?;
    let minutes =
        u32::from(now.weekday) * 24 * 60 + u32::from(now.hour) * 60 + u32::from(now.minute);
    Some(minutes % MINUTES_PER_WEEK)
}

/// Whether the wall is inside one of its blackout periods right now.
pub fn active(blackout: &Blackout) -> bool {
    blackout.is_enabled() && minute_of_week().is_some_and(|now| blackout.covers(now))
}

/// The period in force right now, if any.
pub fn current(blackout: &Blackout) -> Option<&BlackoutWindow> {
    blackout.covering(minute_of_week()?)
}

/// The date as `yyyy/mm/dd`.
///
/// That order in every language, on purpose: it is the only numeric form that
/// cannot be read as another month and day, which matters on something that is
/// glanced at rather than studied.
pub fn format_date(now: &LocalTime) -> String {
    format!("{:04}/{:02}/{:02}", now.year, now.month, now.day)
}

/// The time as `hh:mm:ss`, 24 hour.
pub fn format_time(now: &LocalTime) -> String {
    format!("{:02}:{:02}:{:02}", now.hour, now.minute, now.second)
}

/// Minutes from midnight as `HH:MM`, the form the settings read them in.
pub fn format_clock(minutes: u16) -> String {
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
}

/// Reads `HH:MM` back into minutes from midnight.
pub fn parse_clock(text: &str) -> Option<u16> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let hours: u16 = hours.trim().parse().ok()?;
    let minutes: u16 = minutes.trim().parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(hours * 60 + minutes)
}

#[cfg(windows)]
mod imp {
    use super::*;

    /// `SYSTEMTIME`, in the order and of the widths `GetLocalTime` writes.
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        /// 0 = Sunday, as the Win32 API counts.
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        // Named with a leading underscore to keep its place in the layout
        // without being reported as unused.
        _milliseconds: u16,
    }

    // `kernel32` is linked by the standard library on Windows, so declaring the
    // one function needed here is cheaper than a dependency for it. Written
    // without `unsafe` on the block: this crate is edition 2021, and the
    // `unsafe extern` form would raise the effective minimum compiler past the
    // 1.80 the workspace declares.
    extern "system" {
        fn GetLocalTime(time: *mut SystemTime);
    }

    pub(super) fn local_now() -> Option<LocalTime> {
        let mut time = SystemTime::default();
        // SAFETY: the call fills the structure it is given, which is laid out
        // exactly as `SYSTEMTIME` is on every Windows target, and it has no
        // failure mode.
        unsafe { GetLocalTime(&mut time) };
        Some(LocalTime {
            year: time.year,
            month: time.month as u8,
            day: time.day as u8,
            // The API counts from Sunday; the schedule counts from Monday.
            weekday: ((time.day_of_week + 6) % 7) as u8,
            hour: time.hour as u8,
            minute: time.minute as u8,
            second: time.second as u8,
        })
    }
}

#[cfg(unix)]
mod imp {
    use super::*;

    pub(super) fn local_now() -> Option<LocalTime> {
        // SAFETY: `localtime_r` writes the `tm` it is handed and reads the
        // `time_t` it is given, and both results are checked before use.
        unsafe {
            let mut seconds: libc::time_t = 0;
            if libc::time(&mut seconds) == -1 {
                return None;
            }
            let mut broken_down: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&seconds, &mut broken_down).is_null() {
                return None;
            }
            Some(LocalTime {
                // `tm_year` counts from 1900 and `tm_mon` from January.
                year: (broken_down.tm_year + 1900) as u16,
                month: (broken_down.tm_mon + 1) as u8,
                day: broken_down.tm_mday as u8,
                // The C library counts from Sunday; the schedule counts from
                // Monday.
                weekday: (broken_down.tm_wday + 6).rem_euclid(7) as u8,
                hour: broken_down.tm_hour as u8,
                minute: broken_down.tm_min as u8,
                second: broken_down.tm_sec as u8,
            })
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use super::*;

    /// A platform this build has no clock for. The schedule stays off, and the
    /// blank shows nothing rather than a guess at the time.
    pub(super) fn local_now() -> Option<LocalTime> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EVERY_DAY;

    /// Minute of the week for a day counted from Monday, as the schedule does.
    fn at(day: u32, hour: u32, minute: u32) -> u32 {
        day * 24 * 60 + hour * 60 + minute
    }

    fn night() -> BlackoutWindow {
        BlackoutWindow { enabled: true, days: EVERY_DAY, start: 22 * 60, end: 7 * 60 }
    }

    #[test]
    fn a_night_covers_the_evening_it_starts_in_and_the_morning_after() {
        let window = night();
        // Monday 21:59 is before it, 22:00 is the first minute.
        assert!(!window.covers(at(0, 21, 59)));
        assert!(window.covers(at(0, 22, 0)));
        // It runs through midnight into the next morning.
        assert!(window.covers(at(0, 23, 59)));
        assert!(window.covers(at(1, 0, 0)));
        assert!(window.covers(at(1, 6, 59)));
        // …and stops at 07:00.
        assert!(!window.covers(at(1, 7, 0)));
        assert!(!window.covers(at(1, 15, 0)));
        // Tuesday evening brings a period of its own.
        assert!(window.covers(at(1, 23, 0)));
    }

    #[test]
    fn a_night_started_on_sunday_runs_into_monday() {
        // The wrap at the end of the week is the one a naive implementation
        // gets wrong: Sunday is the last day, not a day before Monday.
        let sunday = BlackoutWindow { enabled: true, days: 0b0100_0000, start: 22 * 60, end: 7 * 60 };
        assert!(sunday.covers(at(6, 23, 0)));
        assert!(sunday.covers(at(0, 3, 0)));
        assert!(!sunday.covers(at(0, 8, 0)));
        assert!(!sunday.covers(at(5, 23, 0)));
    }

    #[test]
    fn a_window_only_applies_to_the_days_it_starts_on() {
        // Weekdays only, bit 0..4 are Monday to Friday.
        let noon = BlackoutWindow { enabled: true, days: 0b0001_1111, start: 12 * 60, end: 13 * 60 };
        assert!(noon.covers(at(0, 12, 30)));
        assert!(noon.covers(at(4, 12, 30)));
        assert!(!noon.covers(at(5, 12, 30)));
        assert!(!noon.covers(at(4, 13, 30)));
    }

    #[test]
    fn an_end_equal_to_the_start_lasts_a_full_day_from_the_start() {
        let all_day = BlackoutWindow { enabled: true, days: 0b0000_0001, start: 9 * 60, end: 9 * 60 };
        assert_eq!(all_day.duration(), 24 * 60);
        // Monday 09:00 through Tuesday 09:00, and nothing before it: the period
        // is measured from its start time, not from midnight.
        assert!(!all_day.covers(at(0, 8, 59)));
        assert!(all_day.covers(at(0, 9, 0)));
        assert!(all_day.covers(at(0, 23, 59)));
        assert!(all_day.covers(at(1, 8, 59)));
        assert!(!all_day.covers(at(1, 9, 0)));
    }

    #[test]
    fn an_unchecked_entry_is_kept_but_ignored() {
        let mut window = night();
        window.enabled = false;
        assert!(!window.covers(at(0, 23, 0)));
    }

    #[test]
    fn no_entry_means_the_wall_is_never_blanked() {
        let blackout = Blackout::default();
        assert!(blackout.is_empty());
        assert!(!blackout.covers(at(0, 23, 0)));
        assert!(!active(&blackout));
    }

    #[test]
    fn the_period_in_force_is_the_one_covering_now() {
        let blackout = Blackout { entries: vec![night()], ..Blackout::default() };
        assert!(blackout.covers(at(0, 23, 30)));
        assert_eq!(blackout.covering(at(0, 23, 30)).map(|window| window.end), Some(7 * 60));
        assert!(blackout.covering(at(0, 12, 0)).is_none());
        // Any of several entries may be the one in force.
        let morning = BlackoutWindow { enabled: true, days: EVERY_DAY, start: 6 * 60, end: 8 * 60 };
        let both = Blackout { entries: vec![night(), morning], ..Blackout::default() };
        assert!(both.covers(at(0, 6, 30)));
        assert!(both.covers(at(0, 23, 0)));
    }

    #[test]
    fn the_clock_is_read_somewhere_inside_the_week() {
        // A weak claim, but it catches a weekday or an hour counted the wrong
        // way, which is the shape of mistake this arithmetic invites.
        if let Some(minute) = minute_of_week() {
            assert!(minute < MINUTES_PER_WEEK);
        }
    }

    #[test]
    fn the_local_clock_is_a_moment_that_could_be_now() {
        // The same weak claim for the fields the mark prints, so that a wrong
        // base - `tm_year` from 1900, or `tm_mon` from January - is caught
        // rather than shown on a dark wall nobody is looking at.
        if let Some(now) = local_now() {
            assert!(now.year >= 2024, "{now:?}");
            assert!((1..=12).contains(&now.month), "{now:?}");
            assert!((1..=31).contains(&now.day), "{now:?}");
            assert!(now.weekday < 7, "{now:?}");
            assert!(now.hour < 24, "{now:?}");
            assert!(now.minute < 60, "{now:?}");
            assert!(now.second < 60, "{now:?}");
        }
    }

    #[test]
    fn the_clock_reads_as_a_date_and_a_time() {
        let now = LocalTime {
            year: 2026,
            month: 10,
            day: 6,
            weekday: 1,
            hour: 19,
            minute: 4,
            second: 7,
        };
        assert_eq!(format_date(&now), "2026/10/06");
        assert_eq!(format_time(&now), "19:04:07");
    }

    #[test]
    fn a_switched_off_schedule_never_blanks() {
        // Only the negative direction can be asserted here: `active` reads the
        // real clock, so whether a period covers "now" is not ours to choose.
        let blackout = Blackout { enabled: false, ..Blackout::default() };
        assert!(!active(&blackout));
    }

    #[test]
    fn the_period_reported_is_the_one_that_ends_last() {
        // Two periods in force at once, as they are a union. The wall stays
        // black until the last of them lets go, so that is the one to report -
        // and the order they were written in must not decide it.
        let short =
            BlackoutWindow { enabled: true, days: EVERY_DAY, start: 21 * 60 + 55, end: 7 * 60 };
        let long = BlackoutWindow { enabled: true, days: EVERY_DAY, start: 20 * 60, end: 9 * 60 + 20 };
        let both = Blackout { entries: vec![short, long], ..Blackout::default() };
        assert!(both.covers(at(0, 22, 0)));
        assert_eq!(both.covering(at(0, 22, 0)).map(|window| window.end), Some(9 * 60 + 20));

        let mut reversed = both.clone();
        reversed.entries.reverse();
        assert_eq!(reversed.covering(at(0, 22, 0)).map(|window| window.end), Some(9 * 60 + 20));
    }

    #[test]
    fn a_clock_is_written_and_read_back_the_way_it_was_stored() {
        assert_eq!(format_clock(0), "00:00");
        assert_eq!(format_clock(7 * 60 + 5), "07:05");
        assert_eq!(format_clock(22 * 60), "22:00");
        assert_eq!(format_clock(23 * 60 + 59), "23:59");
        assert_eq!(parse_clock("22:00"), Some(22 * 60));
        assert_eq!(parse_clock(" 7:05 "), Some(7 * 60 + 5));
        assert_eq!(parse_clock("24:00"), None);
        assert_eq!(parse_clock("22:60"), None);
        assert_eq!(parse_clock("22"), None);
        assert_eq!(parse_clock(""), None);
    }
}
