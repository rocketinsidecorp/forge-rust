//! UTC timestamps in the text forms Forge-Go writes: RFC 3339 for JSON
//! (`2026-10-19T12:00:00.5Z`), and Docker's `docker ps` `CreatedAt`
//! (`2026-10-19 12:00:00 +0000 UTC`), parsed back to the same instant.
//!
//! `forge deploy` and forge-proxy share this module so lock records, deploy
//! logs and access logs read the same in both frameworks.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// An instant with the UTC offset it was written with, to the nanosecond.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timestamp {
    /// Seconds since the Unix epoch (UTC).
    pub unix_seconds: i64,
    /// Nanoseconds within the second.
    pub nanos: u32,
    /// The offset from UTC the text carried, in seconds.
    pub offset_seconds: i32,
}

/// Go's zero `time.Time` (`0001-01-01T00:00:00Z`).
const GO_ZERO_UNIX: i64 = -62_135_596_800;

impl Timestamp {
    /// The current time in UTC.
    #[must_use]
    pub fn now() -> Self {
        Self::from_system(SystemTime::now())
    }

    /// Go's zero time, which is what an unparsable Docker timestamp becomes.
    #[must_use]
    pub const fn go_zero() -> Self {
        Self {
            unix_seconds: GO_ZERO_UNIX,
            nanos: 0,
            offset_seconds: 0,
        }
    }

    /// From a system time, in UTC.
    #[must_use]
    pub fn from_system(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(since) => Self {
                unix_seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                nanos: since.subsec_nanos(),
                offset_seconds: 0,
            },
            Err(before) => {
                let before = before.duration();
                let mut seconds = -i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
                let mut nanos = before.subsec_nanos();
                if nanos > 0 {
                    seconds -= 1;
                    nanos = 1_000_000_000 - nanos;
                }
                Self {
                    unix_seconds: seconds,
                    nanos,
                    offset_seconds: 0,
                }
            }
        }
    }

    /// The same instant in UTC.
    #[must_use]
    pub const fn utc(self) -> Self {
        Self {
            offset_seconds: 0,
            ..self
        }
    }

    /// Nanoseconds since the epoch, for ordering and differences.
    #[must_use]
    pub fn total_nanos(self) -> i128 {
        i128::from(self.unix_seconds) * 1_000_000_000 + i128::from(self.nanos)
    }

    /// This instant plus `duration`.
    #[must_use]
    pub fn plus(self, duration: Duration) -> Self {
        let total = self.total_nanos() + i128::try_from(duration.as_nanos()).unwrap_or(0);
        Self::from_total_nanos(total, self.offset_seconds)
    }

    fn from_total_nanos(total: i128, offset_seconds: i32) -> Self {
        let seconds = total.div_euclid(1_000_000_000);
        let nanos = total.rem_euclid(1_000_000_000);
        Self {
            unix_seconds: i64::try_from(seconds).unwrap_or(i64::MAX),
            nanos: u32::try_from(nanos).unwrap_or(0),
            offset_seconds,
        }
    }

    /// Whole milliseconds from `earlier` to `self` (negative when earlier is
    /// later).
    #[must_use]
    pub fn millis_since(self, earlier: Self) -> i64 {
        i64::try_from((self.total_nanos() - earlier.total_nanos()) / 1_000_000).unwrap_or(0)
    }

    /// Local wall-clock fields at the stored offset.
    fn fields(self) -> (i64, u32, u32, u32, u32, u32) {
        let local = self.unix_seconds + i64::from(self.offset_seconds);
        let days = local.div_euclid(86_400);
        let second_of_day = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = u32::try_from(second_of_day / 3600).unwrap_or(0);
        let minute = u32::try_from(second_of_day % 3600 / 60).unwrap_or(0);
        let second = u32::try_from(second_of_day % 60).unwrap_or(0);
        (year, month, day, hour, minute, second)
    }

    fn offset_text(self, colon: bool) -> String {
        if self.offset_seconds == 0 && colon {
            return "Z".to_owned();
        }
        let sign = if self.offset_seconds < 0 { '-' } else { '+' };
        let total = self.offset_seconds.unsigned_abs();
        let (hours, minutes) = (total / 3600, total % 3600 / 60);
        if colon {
            format!("{sign}{hours:02}:{minutes:02}")
        } else {
            format!("{sign}{hours:02}{minutes:02}")
        }
    }

    /// Go's `time.RFC3339Nano` (what `encoding/json` writes): fractional
    /// seconds without trailing zeros, `Z` for UTC.
    #[must_use]
    pub fn rfc3339_nano(self) -> String {
        let (year, month, day, hour, minute, second) = self.fields();
        let mut fraction = String::new();
        if self.nanos > 0 {
            fraction = format!(".{:09}", self.nanos);
            while fraction.ends_with('0') {
                fraction.pop();
            }
        }
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}{}",
            self.offset_text(true)
        )
    }

    /// Go's `time.RFC3339`: whole seconds.
    #[must_use]
    pub fn rfc3339(self) -> String {
        let (year, month, day, hour, minute, second) = self.fields();
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{}",
            self.offset_text(true)
        )
    }

    /// `log/slog`'s JSON time: RFC 3339 with milliseconds.
    #[must_use]
    pub fn rfc3339_millis(self) -> String {
        let (year, month, day, hour, minute, second) = self.fields();
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}{}",
            self.nanos / 1_000_000,
            self.offset_text(true)
        )
    }

    /// Go's `"2006-01-02 15:04:05 MST"`; the zone is `UTC` or the numeric
    /// offset.
    #[must_use]
    pub fn human(self) -> String {
        let (year, month, day, hour, minute, second) = self.fields();
        let zone = if self.offset_seconds == 0 {
            "UTC".to_owned()
        } else {
            self.offset_text(false)
        };
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {zone}")
    }

    /// Docker's `CreatedAt` form, `2026-10-19 12:00:00 +0000 UTC`.
    #[must_use]
    pub fn docker(self) -> String {
        let (year, month, day, hour, minute, second) = self.fields();
        let zone = if self.offset_seconds == 0 {
            "UTC".to_owned()
        } else {
            self.offset_text(false)
        };
        format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {} {zone}",
            self.offset_text(false)
        )
    }

    /// Parses RFC 3339 with optional fractional seconds (`Z` or `±hh:mm`).
    #[must_use]
    pub fn parse_rfc3339(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() < 20 || bytes[10] != b'T' && bytes[10] != b't' {
            return None;
        }
        let (date, rest) = text.split_at(10);
        let (year, month, day) = parse_date(date)?;
        let rest = &rest[1..];
        let (hour, minute, second, rest) = parse_clock(rest)?;
        let (nanos, rest) = parse_fraction(rest)?;
        let offset = match rest {
            "Z" | "z" => 0,
            _ => parse_offset(rest, true)?,
        };
        Some(Self::from_fields(
            year, month, day, hour, minute, second, nanos, offset,
        ))
    }

    /// Parses Docker's `2006-01-02 15:04:05 -0700 MST` (Go layout used by
    /// Forge-Go for `docker ps` output).
    #[must_use]
    pub fn parse_docker(text: &str) -> Option<Self> {
        let mut parts = text.split(' ');
        let date = parts.next()?;
        let clock = parts.next()?;
        let offset = parts.next()?;
        let zone = parts.next()?;
        if parts.next().is_some() || zone.is_empty() {
            return None;
        }
        let (year, month, day) = parse_date(date)?;
        let (hour, minute, second, rest) = parse_clock(clock)?;
        let (nanos, rest) = parse_fraction(rest)?;
        if !rest.is_empty() {
            return None;
        }
        let offset = parse_offset(offset, false)?;
        Some(Self::from_fields(
            year, month, day, hour, minute, second, nanos, offset,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_fields(
        year: i64,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        nanos: u32,
        offset_seconds: i32,
    ) -> Self {
        let days = days_from_civil(year, month, day);
        let local = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
        Self {
            unix_seconds: local - i64::from(offset_seconds),
            nanos,
            offset_seconds,
        }
    }
}

fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn parse_date(text: &str) -> Option<(i64, u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = i64::from(digits(&text[0..4])?);
    let month = digits(&text[5..7])?;
    let day = digits(&text[8..10])?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some((year, month, day))
}

fn parse_clock(text: &str) -> Option<(u32, u32, u32, &str)> {
    let bytes = text.as_bytes();
    if bytes.len() < 8 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    let hour = digits(&text[0..2])?;
    let minute = digits(&text[3..5])?;
    let second = digits(&text[6..8])?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second, &text[8..]))
}

fn parse_fraction(text: &str) -> Option<(u32, &str)> {
    let Some(rest) = text.strip_prefix('.') else {
        return Some((0, text));
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let mut digits_text = rest[..end].to_owned();
    digits_text.truncate(9);
    while digits_text.len() < 9 {
        digits_text.push('0');
    }
    Some((digits_text.parse().ok()?, &rest[end..]))
}

fn parse_offset(text: &str, colon: bool) -> Option<i32> {
    let bytes = text.as_bytes();
    let sign = match bytes.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = if colon {
        if bytes.len() != 6 || bytes[3] != b':' {
            return None;
        }
        (digits(&text[1..3])?, digits(&text[4..6])?)
    } else {
        if bytes.len() != 5 {
            return None;
        }
        (digits(&text[1..3])?, digits(&text[3..5])?)
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * i32::try_from(hours * 3600 + minutes * 60).ok()?)
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day number since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_rfc3339() {
        let stamp = Timestamp {
            unix_seconds: 1_792_411_200, // 2026-10-19T12:00:00Z
            nanos: 0,
            offset_seconds: 0,
        };
        assert_eq!(stamp.rfc3339_nano(), "2026-10-19T12:00:00Z");
        assert_eq!(stamp.rfc3339(), "2026-10-19T12:00:00Z");
        assert_eq!(stamp.human(), "2026-10-19 12:00:00 UTC");
        assert_eq!(stamp.docker(), "2026-10-19 12:00:00 +0000 UTC");
        let fractional = Timestamp {
            nanos: 500_000_000,
            ..stamp
        };
        assert_eq!(fractional.rfc3339_nano(), "2026-10-19T12:00:00.5Z");
        assert_eq!(fractional.rfc3339_millis(), "2026-10-19T12:00:00.500Z");
        assert_eq!(
            Timestamp::parse_rfc3339("2026-10-19T12:00:00.5Z"),
            Some(fractional)
        );
        let offset = Timestamp::parse_rfc3339("2026-10-19T14:00:00+02:00").expect("valid");
        assert_eq!(offset.unix_seconds, stamp.unix_seconds);
        assert_eq!(offset.rfc3339(), "2026-10-19T14:00:00+02:00");
        assert_eq!(Timestamp::parse_docker(&stamp.docker()), Some(stamp));
        let docker = Timestamp::parse_docker("2026-10-19 13:00:00 +0100 CET").expect("valid");
        assert_eq!(docker.unix_seconds, stamp.unix_seconds);
        for bad in [
            "",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-19 12:00:00Z",
        ] {
            assert_eq!(Timestamp::parse_rfc3339(bad), None, "{bad}");
        }
        assert_eq!(Timestamp::parse_docker("yesterday"), None);
        assert_eq!(Timestamp::go_zero().rfc3339_nano(), "0001-01-01T00:00:00Z");
    }

    #[test]
    fn civil_round_trip() {
        for days in [-719_162_i64, -1, 0, 1, 11_016, 20_745, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
    }
}
