use core::{fmt, str::FromStr};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
/// Days from 0001-01-01 to 1970-01-01 (proleptic Gregorian calendar).
const MIN_DAYS: i64 = -719_162;
/// Days from 1970-01-01 to 9999-12-31.
const MAX_DAYS: i64 = 2_932_896;

/// A UTC instant with microsecond precision, from `0001-01-01T00:00:00Z` to
/// `9999-12-31T23:59:59.999999Z`.
///
/// `Timestamp` is a Forge-owned value for domain and application code, which
/// must not depend on a database driver or date-time crate. Adapters convert
/// it to their own types through [`Timestamp::unix_micros`] and
/// [`Timestamp::from_unix_micros`].
///
/// Text forms:
///
/// - [`Display`](fmt::Display) and serde use RFC 3339 in UTC:
///   `2026-10-19T08:30:00Z`, with `.ffffff` only when there are microseconds.
/// - [`Timestamp::parse`] accepts RFC 3339 with `Z` or a `±HH:MM` offset, and
///   the offset-less `YYYY-MM-DDTHH:MM[:SS[.f]]` that an HTML
///   `<input type="datetime-local">` submits, which it reads as UTC. At most
///   six fractional digits are accepted; leap seconds are not.
/// - [`Timestamp::to_datetime_local`] renders the value such an input expects.
///
/// ```
/// use forge_core::Timestamp;
///
/// let instant: Timestamp = "2026-10-19T08:30:00+02:00".parse()?;
/// assert_eq!(instant.to_string(), "2026-10-19T06:30:00Z");
/// assert_eq!(instant.to_datetime_local(), "2026-10-19T06:30:00");
/// assert_eq!(Timestamp::parse("2026-10-19T06:30")?, instant);
/// assert!(Timestamp::parse("2026-02-30T00:00").is_err());
/// # Ok::<(), forge_core::InvalidTimestamp>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(i64);

/// A text or numeric value that is not a [`Timestamp`].
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum InvalidTimestamp {
    /// The text is not one of the accepted formats.
    #[error("timestamp must be RFC 3339 or YYYY-MM-DDTHH:MM[:SS[.ffffff]]")]
    Format,
    /// The fields form no calendar date or time of day.
    #[error("timestamp is not a valid date and time")]
    Value,
    /// The instant is outside years 0001 to 9999.
    #[error("timestamp is outside years 0001 to 9999")]
    OutOfRange,
}

impl Timestamp {
    /// `1970-01-01T00:00:00Z`.
    pub const UNIX_EPOCH: Self = Self(0);
    /// `0001-01-01T00:00:00Z`.
    pub const MIN: Self = Self(MIN_DAYS * SECONDS_PER_DAY * MICROS_PER_SECOND);
    /// `9999-12-31T23:59:59.999999Z`.
    pub const MAX: Self = Self((MAX_DAYS + 1) * SECONDS_PER_DAY * MICROS_PER_SECOND - 1);

    /// The current time of the system clock, truncated to microseconds and
    /// clamped to the supported range.
    #[must_use]
    pub fn now() -> Self {
        let micros = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_micros()).unwrap_or(i64::MAX),
            Err(before) => {
                i64::try_from(before.duration().as_micros()).map_or(i64::MIN, |micros| -micros)
            }
        };
        Self(micros.clamp(Self::MIN.0, Self::MAX.0))
    }

    /// Builds a timestamp from microseconds since the Unix epoch.
    pub const fn from_unix_micros(micros: i64) -> Result<Self, InvalidTimestamp> {
        if micros < Self::MIN.0 || micros > Self::MAX.0 {
            return Err(InvalidTimestamp::OutOfRange);
        }
        Ok(Self(micros))
    }

    /// Microseconds since the Unix epoch (negative before 1970).
    #[must_use]
    pub const fn unix_micros(self) -> i64 {
        self.0
    }

    /// Parses RFC 3339 (`Z` or `±HH:MM`) or an offset-less
    /// `YYYY-MM-DDTHH:MM[:SS[.ffffff]]` read as UTC.
    pub fn parse(text: &str) -> Result<Self, InvalidTimestamp> {
        let bytes = text.as_bytes();
        if bytes.len() < 16 || bytes.len() > 32 {
            return Err(InvalidTimestamp::Format);
        }
        let year = digits(bytes, 0, 4)?;
        expect(bytes, 4, b'-')?;
        let month = digits(bytes, 5, 2)?;
        expect(bytes, 7, b'-')?;
        let day = digits(bytes, 8, 2)?;
        if !matches!(bytes[10], b'T' | b't') {
            return Err(InvalidTimestamp::Format);
        }
        let hour = digits(bytes, 11, 2)?;
        expect(bytes, 13, b':')?;
        let minute = digits(bytes, 14, 2)?;
        let mut position = 16;
        let mut second = 0;
        let mut micros = 0;
        if bytes.get(position) == Some(&b':') {
            second = digits(bytes, position + 1, 2)?;
            position += 3;
            if bytes.get(position) == Some(&b'.') {
                let start = position + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let count = end - start;
                if count == 0 || count > 6 {
                    return Err(InvalidTimestamp::Format);
                }
                micros = digits(bytes, start, count)? * 10_i64.pow((6 - count) as u32);
                position = end;
            }
        }
        let offset_seconds = match &bytes[position..] {
            [] => 0,
            [b'Z' | b'z'] => 0,
            [sign @ (b'+' | b'-'), ..] if bytes.len() - position == 6 => {
                expect(bytes, position + 3, b':')?;
                let hours = digits(bytes, position + 1, 2)?;
                let minutes = digits(bytes, position + 4, 2)?;
                if hours > 23 || minutes > 59 {
                    return Err(InvalidTimestamp::Value);
                }
                let offset = hours * 3_600 + minutes * 60;
                if *sign == b'+' { offset } else { -offset }
            }
            _ => return Err(InvalidTimestamp::Format),
        };

        if year < 1
            || !(1..=12).contains(&month)
            || day < 1
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err(InvalidTimestamp::Value);
        }
        let seconds = days_from_civil(year, month, day) * SECONDS_PER_DAY
            + hour * 3_600
            + minute * 60
            + second
            - offset_seconds;
        Self::from_unix_micros(seconds * MICROS_PER_SECOND + micros)
    }

    /// `YYYY-MM-DDTHH:MM:SS[.ffffff]` in UTC, the value format of an HTML
    /// `<input type="datetime-local">`.
    #[must_use]
    pub fn to_datetime_local(self) -> String {
        let (year, month, day, hour, minute, second, micros) = self.fields();
        let mut text = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
        if micros != 0 {
            text.push_str(&format!(".{micros:06}"));
        }
        text
    }

    fn fields(self) -> (i64, i64, i64, i64, i64, i64, i64) {
        let seconds = self.0.div_euclid(MICROS_PER_SECOND);
        let micros = self.0.rem_euclid(MICROS_PER_SECOND);
        let days = seconds.div_euclid(SECONDS_PER_DAY);
        let of_day = seconds.rem_euclid(SECONDS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        (
            year,
            month,
            day,
            of_day / 3_600,
            of_day % 3_600 / 60,
            of_day % 60,
            micros,
        )
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}Z", self.to_datetime_local())
    }
}

impl FromStr for Timestamp {
    type Err = InvalidTimestamp;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TimestampVisitor;

        impl de::Visitor<'_> for TimestampVisitor {
            type Value = Timestamp;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an RFC 3339 timestamp")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Timestamp::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(TimestampVisitor)
    }
}

fn digits(bytes: &[u8], start: usize, count: usize) -> Result<i64, InvalidTimestamp> {
    let slice = bytes
        .get(start..start + count)
        .ok_or(InvalidTimestamp::Format)?;
    slice.iter().try_fold(0_i64, |value, byte| {
        if byte.is_ascii_digit() {
            Ok(value * 10 + i64::from(byte - b'0'))
        } else {
            Err(InvalidTimestamp::Format)
        }
    })
}

fn expect(bytes: &[u8], index: usize, expected: u8) -> Result<(), InvalidTimestamp> {
    if bytes.get(index) == Some(&expected) {
        Ok(())
    } else {
        Err(InvalidTimestamp::Format)
    }
}

const fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Timestamp {
        match Timestamp::parse(text) {
            Ok(value) => value,
            Err(error) => panic!("{text}: {error}"),
        }
    }

    #[test]
    fn parses_rfc3339_and_datetime_local() {
        assert_eq!(parse("1970-01-01T00:00:00Z"), Timestamp::UNIX_EPOCH);
        assert_eq!(parse("1970-01-01T00:00"), Timestamp::UNIX_EPOCH);
        assert_eq!(parse("1970-01-01t01:00:00+01:00"), Timestamp::UNIX_EPOCH);
        assert_eq!(parse("1969-12-31T23:00:00-01:00"), Timestamp::UNIX_EPOCH);
        assert_eq!(
            parse("2000-03-01T00:00:00Z").unix_micros(),
            951_868_800_000_000
        );
        assert_eq!(
            parse("2026-10-19T08:30:00.5Z").unix_micros() % 1_000_000,
            500_000
        );
        assert_eq!(parse("2024-02-29T12:00:00.000001Z").unix_micros() % 10, 1);
        assert_eq!(parse("0001-01-01T00:00:00Z"), Timestamp::MIN);
        assert_eq!(parse("9999-12-31T23:59:59.999999Z"), Timestamp::MAX);
    }

    #[test]
    fn rejects_malformed_and_impossible_values() {
        for text in [
            "",
            "2026-10-19",
            "2026-10-19 08:30",
            "2026-10-19T8:30",
            "2026-10-19T08:30:00.",
            "2026-10-19T08:30:00.1234567Z",
            "2026-10-19T08:30:00+0200",
            "2026-10-19T08:30:00 Z",
            "+2026-10-19T08:30",
            "2026-10-19T08:30Zjunk",
        ] {
            assert_eq!(
                Timestamp::parse(text),
                Err(InvalidTimestamp::Format),
                "{text}"
            );
        }
        for text in [
            "2026-02-29T00:00",
            "2026-13-01T00:00",
            "2026-00-01T00:00",
            "2026-04-31T00:00",
            "2026-10-19T24:00",
            "2026-10-19T23:60",
            "2026-10-19T23:59:60Z",
            "0000-01-01T00:00",
            "2026-10-19T00:00+24:00",
        ] {
            assert_eq!(
                Timestamp::parse(text),
                Err(InvalidTimestamp::Value),
                "{text}"
            );
        }
        assert_eq!(
            Timestamp::parse("0001-01-01T00:00:00+00:01"),
            Err(InvalidTimestamp::OutOfRange)
        );
        assert_eq!(
            Timestamp::from_unix_micros(Timestamp::MAX.unix_micros() + 1),
            Err(InvalidTimestamp::OutOfRange)
        );
    }

    #[test]
    fn formats_round_trip_across_the_range() {
        for text in [
            "0001-01-01T00:00:00Z",
            "1600-02-29T12:34:56Z",
            "1969-12-31T23:59:59.999999Z",
            "1970-01-01T00:00:00Z",
            "2000-02-29T00:00:00.100000Z",
            "2100-03-01T00:00:00Z",
            "9999-12-31T23:59:59.999999Z",
        ] {
            assert_eq!(parse(text).to_string(), text);
        }
        assert_eq!(
            parse("2026-10-19T08:30").to_datetime_local(),
            "2026-10-19T08:30:00"
        );
        let mut day = MIN_DAYS;
        while day <= MAX_DAYS {
            let (year, month, date) = civil_from_days(day);
            assert_eq!(days_from_civil(year, month, date), day);
            day += 97;
        }
    }

    #[test]
    fn now_is_in_range_and_serde_uses_rfc3339() {
        let now = Timestamp::now();
        assert!(now > parse("2020-01-01T00:00:00Z"));
        let json = serde_json::to_string(&parse("2026-10-19T08:30:00Z")).unwrap_or_default();
        assert_eq!(json, "\"2026-10-19T08:30:00Z\"");
        let back: Result<Timestamp, _> = serde_json::from_str(&json);
        assert_eq!(back.ok(), Some(parse("2026-10-19T08:30:00Z")));
        assert!(serde_json::from_str::<Timestamp>("\"yesterday\"").is_err());
    }
}
