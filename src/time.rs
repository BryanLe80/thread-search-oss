//! UTC calendar arithmetic in epoch seconds. The tool reads one timestamp format and prints one
//! date format, which does not justify a date crate.

pub const SECONDS_PER_MINUTE: i64 = 60;
pub const SECONDS_PER_HOUR: i64 = 3600;
pub const SECONDS_PER_DAY: i64 = 86_400;
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `YYYY-MM-DD HH:MM` (Howard Hinnant's `civil_from_days`).
pub fn utc(epoch: i64) -> String {
    let days = epoch.div_euclid(SECONDS_PER_DAY);
    let secs = epoch.rem_euclid(SECONDS_PER_DAY);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (hour, minute) = (secs / SECONDS_PER_HOUR, secs % SECONDS_PER_HOUR / SECONDS_PER_MINUTE);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formats_a_known_instant() {
        assert_eq!(utc(1_794_312_000), "2026-11-10 12:00");
        assert_eq!(utc(0), "1970-01-01 00:00");
    }

    #[test]
    fn days_from_civil_counts_days_from_the_epoch_and_agrees_with_utc() {
        assert_eq!(utc(-1), "1969-12-31 23:59");
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 2, 29) * SECONDS_PER_DAY, 1_709_164_800);
        assert_eq!(utc(days_from_civil(2026, 11, 10) * SECONDS_PER_DAY), "2026-11-10 00:00");
    }
}
