//! Stamps the year of the build into the crate, for the about panel and the
//! command line.
//!
//! Cargo exposes no date, so the year is taken from the system clock here and
//! handed to the code as `XGVIEW_BUILD_YEAR`; `copyright_years` turns it into
//! the string that is shown. It is the year the binary was built in: a later
//! rebuild takes the year again.
//!
//! No `rerun-if-*` instruction is emitted on purpose. That leaves Cargo's
//! default in place - the script is re-run when a file of the package changes,
//! which is exactly when the crate is rebuilt - so the stamped year cannot go
//! stale while the binary it describes is unchanged.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=XGVIEW_BUILD_YEAR={}", year_of(seconds as i64));
}

/// Calendar year of a Unix timestamp, in UTC.
///
/// Howard Hinnant's `civil_from_days`: days since the epoch, shifted into
/// 400-year eras, mid-year so that February is last and the leap rule is a
/// pair of divisions. Only the year is kept.
fn year_of(unix_seconds: i64) -> i64 {
    let days = unix_seconds.div_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    if month <= 2 {
        year + 1
    } else {
        year
    }
}
