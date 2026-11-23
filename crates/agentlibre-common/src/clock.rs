//! The clock of agl (AGL-121, D17): the time kept is UTC, and the
//! calendar is the days-to-civil algorithm by Howard Hinnant. The local
//! zones come with cron as their own question.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    utc(seconds)
}

/// Today's calendar date, `YYYY-MM-DD`, from the system clock.
pub fn today() -> String {
    utc_now()[..10].to_owned()
}

/// Formats Unix `seconds` as UTC (the days-to-civil algorithm by Howard
/// Hinnant).
fn utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[test]
fn utc_times() {
    assert_eq!(utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(utc(1_760_205_731), "2025-10-11T18:02:11Z");
    assert_eq!(utc(4_102_444_799), "2099-12-31T23:59:59Z");
}

