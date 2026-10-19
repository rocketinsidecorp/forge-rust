//! Five-field cron expressions evaluated in UTC.
//!
//! The grammar is the classic Vixie cron one, without seconds or years:
//!
//! ```text
//! minute (0-59)  hour (0-23)  day-of-month (1-31)  month (1-12 or jan-dec)  day-of-week (0-7 or sun-sat; 0 and 7 are Sunday)
//! ```
//!
//! Each field is `*`, a value, a range `a-b`, a step `*/n`, `a/n` or `a-b/n`,
//! or a comma-separated list of those. `@yearly` (`@annually`), `@monthly`,
//! `@weekly`, `@daily` (`@midnight`) and `@hourly` are accepted. As in Vixie
//! cron, when both day-of-month and day-of-week are restricted (neither starts
//! with `*`), a day matches if **either** matches.
//!
//! Evaluation is always in UTC: there are no time zones and therefore no
//! daylight-saving gaps or repeated hours.

use std::{
    fmt,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

/// Longest accepted expression, in bytes.
const MAX_EXPRESSION_LEN: usize = 256;
/// Days searched for the next/previous match. A satisfiable expression always
/// matches within nine years (29 February can be eight years apart).
const SEARCH_DAYS: i64 = 366 * 9;
const MINUTES_PER_DAY: i64 = 1440;

/// A parsed cron expression.
///
/// ```
/// use std::time::{Duration, UNIX_EPOCH};
/// use forge_jobs::Cron;
///
/// let every_quarter_hour: Cron = "*/15 * * * *".parse()?;
/// // 1970-01-01T00:07:00Z -> 00:15:00Z
/// let next = every_quarter_hour.next_after(UNIX_EPOCH + Duration::from_secs(7 * 60));
/// assert_eq!(next, Some(UNIX_EPOCH + Duration::from_secs(15 * 60)));
/// # Ok::<(), forge_jobs::CronError>(())
/// ```
#[derive(Clone, Eq, PartialEq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
    expression: Box<str>,
}

/// An invalid cron expression.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum CronError {
    /// The expression is longer than 256 bytes.
    #[error("cron expression is too long")]
    TooLong,
    /// The expression does not have exactly five fields.
    #[error("cron expression must have 5 fields, found {found}")]
    FieldCount {
        /// Number of whitespace-separated fields found.
        found: usize,
    },
    /// A `@` macro other than the supported ones.
    #[error("unknown cron macro")]
    UnknownMacro,
    /// A field is malformed or out of range.
    #[error("invalid cron {field} field")]
    InvalidField {
        /// The field: `minute`, `hour`, `day-of-month`, `month` or `day-of-week`.
        field: &'static str,
    },
    /// The expression is well formed but no date satisfies it (e.g. `0 0 30 2 *`).
    #[error("cron expression never matches")]
    NeverMatches,
}

const MONTHS: &[&str] = &[
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

struct FieldSpec {
    name: &'static str,
    min: u32,
    max: u32,
    /// Names for `min..`, matched case-insensitively.
    aliases: &'static [&'static str],
}

const MINUTE: FieldSpec = FieldSpec {
    name: "minute",
    min: 0,
    max: 59,
    aliases: &[],
};
const HOUR: FieldSpec = FieldSpec {
    name: "hour",
    min: 0,
    max: 23,
    aliases: &[],
};
const DAY: FieldSpec = FieldSpec {
    name: "day-of-month",
    min: 1,
    max: 31,
    aliases: &[],
};
const MONTH: FieldSpec = FieldSpec {
    name: "month",
    min: 1,
    max: 12,
    aliases: MONTHS,
};
const WEEKDAY: FieldSpec = FieldSpec {
    name: "day-of-week",
    min: 0,
    max: 7,
    aliases: WEEKDAYS,
};

impl Cron {
    /// Parses an expression; see the [module documentation](self).
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        if expression.len() > MAX_EXPRESSION_LEN {
            return Err(CronError::TooLong);
        }
        let trimmed = expression.trim();
        let expanded = match trimmed {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other if other.starts_with('@') => return Err(CronError::UnknownMacro),
            other => other,
        };
        let fields: Vec<&str> = expanded.split_ascii_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(CronError::FieldCount {
                found: fields.len(),
            });
        };
        let weekdays = parse_field(weekday, &WEEKDAY)?;
        // Day-of-week 7 is Sunday, like 0.
        let weekdays = (weekdays | (weekdays >> 7)) & 0x7f;
        let cron = Self {
            minutes: parse_field(minute, &MINUTE)?,
            hours: narrow(parse_field(hour, &HOUR)?),
            days: narrow(parse_field(day, &DAY)?),
            months: narrow(parse_field(month, &MONTH)?),
            weekdays: narrow(weekdays),
            days_restricted: !day.starts_with('*'),
            weekdays_restricted: !weekday.starts_with('*'),
            expression: trimmed.into(),
        };
        if cron.is_satisfiable() {
            Ok(cron)
        } else {
            Err(CronError::NeverMatches)
        }
    }

    /// The first matching minute strictly after `time`, or `None` when
    /// `time` is before the Unix epoch or no match exists in range.
    #[must_use]
    pub fn next_after(&self, time: SystemTime) -> Option<SystemTime> {
        let start = floor_minute(time)?.checked_add(1)?;
        self.search_forward(start).map(minute_time)
    }

    /// The latest matching minute at or before `time`, or `None` when there
    /// is none since the Unix epoch within the search range.
    #[must_use]
    pub fn latest_at_or_before(&self, time: SystemTime) -> Option<SystemTime> {
        let start = floor_minute(time)?;
        self.search_backward(start).map(minute_time)
    }

    /// The expression as written (trimmed).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.expression
    }

    fn search_forward(&self, start: i64) -> Option<i64> {
        let first = start.div_euclid(MINUTES_PER_DAY);
        let mut from = u32::try_from(start.rem_euclid(MINUTES_PER_DAY)).ok()?;
        for day in first..first.saturating_add(SEARCH_DAYS) {
            if self.day_matches(day)
                && let Some(minute) = self.first_time_from(from)
            {
                return Some(day * MINUTES_PER_DAY + i64::from(minute));
            }
            from = 0;
        }
        None
    }

    fn search_backward(&self, start: i64) -> Option<i64> {
        let mut day = start.div_euclid(MINUTES_PER_DAY);
        let mut until = u32::try_from(start.rem_euclid(MINUTES_PER_DAY)).ok()?;
        for _ in 0..SEARCH_DAYS {
            if day < 0 {
                return None;
            }
            if self.day_matches(day)
                && let Some(minute) = self.last_time_until(until)
            {
                return Some(day * MINUTES_PER_DAY + i64::from(minute));
            }
            day -= 1;
            until = 1439;
        }
        None
    }

    /// First matching minute of the day at or after `from` (minute of day).
    fn first_time_from(&self, from: u32) -> Option<u32> {
        let (first_hour, first_minute) = (from / 60, from % 60);
        (first_hour..24)
            .filter(|hour| self.hours & (1 << hour) != 0)
            .find_map(|hour| {
                let start = if hour == first_hour { first_minute } else { 0 };
                (start..60)
                    .find(|minute| self.minutes & (1 << minute) != 0)
                    .map(|minute| hour * 60 + minute)
            })
    }

    /// Last matching minute of the day at or before `until` (minute of day).
    fn last_time_until(&self, until: u32) -> Option<u32> {
        let (last_hour, last_minute) = (until / 60, until % 60);
        (0..=last_hour)
            .rev()
            .filter(|hour| self.hours & (1 << hour) != 0)
            .find_map(|hour| {
                let end = if hour == last_hour { last_minute } else { 59 };
                (0..=end)
                    .rev()
                    .find(|minute| self.minutes & (1 << minute) != 0)
                    .map(|minute| hour * 60 + minute)
            })
    }

    fn day_matches(&self, day: i64) -> bool {
        let (_, month, day_of_month) = civil_from_days(day);
        if self.months & (1 << month) == 0 {
            return false;
        }
        let weekday = weekday(day);
        let by_day = self.days & (1 << day_of_month) != 0;
        let by_weekday = self.weekdays & (1 << weekday) != 0;
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => by_day || by_weekday,
            (true, false) => by_day,
            (false, true) => by_weekday,
            (false, false) => true,
        }
    }

    /// Whether some calendar date matches the day and month fields.
    fn is_satisfiable(&self) -> bool {
        if !self.days_restricted || self.weekdays_restricted {
            // Only the month (never empty) and day-of-week constrain the
            // date, and every month contains every weekday.
            return true;
        }
        (1..=12_u32)
            .filter(|month| self.months & (1 << month) != 0)
            .any(|month| {
                let longest = match month {
                    2 => 29,
                    4 | 6 | 9 | 11 => 30,
                    _ => 31,
                };
                (1..=longest).any(|day| self.days & (1 << day) != 0)
            })
    }
}

impl FromStr for Cron {
    type Err = CronError;

    fn from_str(expression: &str) -> Result<Self, Self::Err> {
        Self::parse(expression)
    }
}

impl fmt::Display for Cron {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.expression)
    }
}

impl fmt::Debug for Cron {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Cron")
            .field(&self.expression)
            .finish()
    }
}

/// Parses one field into a bit set of its allowed values.
fn parse_field(text: &str, spec: &FieldSpec) -> Result<u64, CronError> {
    let invalid = || CronError::InvalidField { field: spec.name };
    let mut set = 0_u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, Some(parse_number(step).ok_or_else(invalid)?)),
            None => (item, None),
        };
        let (low, high) = if range == "*" {
            (spec.min, spec.max)
        } else if let Some((low, high)) = range.split_once('-') {
            (value(low, spec)?, value(high, spec)?)
        } else {
            let low = value(range, spec)?;
            // `a/n` runs from a to the end of the field.
            (low, if step.is_some() { spec.max } else { low })
        };
        let step = step.unwrap_or(1);
        if low > high || step == 0 {
            return Err(invalid());
        }
        let mut current = low;
        while current <= high {
            set |= 1 << current;
            current += step;
        }
    }
    Ok(set)
}

fn value(text: &str, spec: &FieldSpec) -> Result<u32, CronError> {
    let parsed = parse_number(text).or_else(|| {
        spec.aliases
            .iter()
            .position(|alias| alias.eq_ignore_ascii_case(text))
            .and_then(|index| u32::try_from(index).ok())
            .map(|index| index + spec.min)
    });
    match parsed {
        Some(number) if (spec.min..=spec.max).contains(&number) => Ok(number),
        _ => Err(CronError::InvalidField { field: spec.name }),
    }
}

fn parse_number(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 2 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Narrows a bit set whose values fit the target width by construction.
fn narrow<T: TryFrom<u64> + Default>(bits: u64) -> T {
    T::try_from(bits).unwrap_or_default()
}

/// Whole minutes since the Unix epoch, rounded down; `None` before it.
pub(crate) fn floor_minute(time: SystemTime) -> Option<i64> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(seconds / 60).ok()
}

/// The instant at the start of `minute` minutes after the Unix epoch.
pub(crate) fn minute_time(minute: i64) -> SystemTime {
    let seconds = u64::try_from(minute).unwrap_or(0).saturating_mul(60);
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// `(year, month 1-12, day 1-31)` of a day count since 1970-01-01
/// (Howard Hinnant's `civil_from_days`).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// Day of week, Sunday = 0, of a day count since 1970-01-01 (a Thursday).
pub(crate) fn weekday(days: i64) -> u32 {
    u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(days: i64, hour: i64, minute: i64) -> SystemTime {
        minute_time(days * MINUTES_PER_DAY + hour * 60 + minute)
    }

    /// Days since the epoch of a civil date, by counting (test oracle).
    fn days_of(year: i64, month: u32, day: u32) -> i64 {
        let mut days = 0;
        loop {
            let civil = civil_from_days(days);
            if civil == (year, month, day) {
                return days;
            }
            days += 1;
        }
    }

    #[test]
    fn civil_dates_and_weekdays_match_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(weekday(0), 4, "1970-01-01 was a Thursday");
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(weekday(19_723), 1, "2024-01-01 was a Monday");
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(47_540), (2100, 2, 28));
        assert_eq!(
            civil_from_days(47_541),
            (2100, 3, 1),
            "2100 is not a leap year"
        );
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // Consecutive days never skip or repeat a date.
        let mut previous = civil_from_days(0);
        for day in 1..=(366 * 40) {
            let current = civil_from_days(day);
            assert!(current > previous, "{previous:?} -> {current:?}");
            assert_eq!(weekday(day), (weekday(day - 1) + 1) % 7);
            previous = current;
        }
    }

    #[test]
    fn fields_values_ranges_steps_lists_and_names_parse() {
        let cron = Cron::parse("5,10-12,*/20 0-23/6 1,15 JAN-mar,dec mon-fri").expect("valid");
        let minutes: Vec<u32> = (0..60).filter(|m| cron.minutes & (1 << m) != 0).collect();
        assert_eq!(minutes, [0, 5, 10, 11, 12, 20, 40]);
        let hours: Vec<u32> = (0..24).filter(|h| cron.hours & (1 << h) != 0).collect();
        assert_eq!(hours, [0, 6, 12, 18]);
        assert_eq!(cron.months, 0b1_0000_0000_1110);
        assert_eq!(cron.weekdays, 0b011_1110);
        assert!(cron.days_restricted && cron.weekdays_restricted);
        assert_eq!(Cron::parse("0 0 * * 7").expect("sunday").weekdays, 1);
        assert_eq!(
            Cron::parse("30/10 * * * *")
                .expect("a/n")
                .minutes
                .count_ones(),
            3
        );
        assert_eq!(Cron::parse("@daily").expect("macro").as_str(), "@daily");
        assert_eq!(
            Cron::parse("  0 * * * *  ").expect("trimmed").to_string(),
            "0 * * * *"
        );
    }

    #[test]
    fn malformed_expressions_are_rejected_with_the_field() {
        let cases: &[(&str, CronError)] = &[
            ("", CronError::FieldCount { found: 0 }),
            ("* * * *", CronError::FieldCount { found: 4 }),
            ("* * * * * *", CronError::FieldCount { found: 6 }),
            ("@reboot", CronError::UnknownMacro),
            ("60 * * * *", CronError::InvalidField { field: "minute" }),
            ("* 24 * * *", CronError::InvalidField { field: "hour" }),
            (
                "* * 0 * *",
                CronError::InvalidField {
                    field: "day-of-month",
                },
            ),
            ("* * * 13 *", CronError::InvalidField { field: "month" }),
            (
                "* * * * 8",
                CronError::InvalidField {
                    field: "day-of-week",
                },
            ),
            ("*/0 * * * *", CronError::InvalidField { field: "minute" }),
            ("5-1 * * * *", CronError::InvalidField { field: "minute" }),
            ("-1 * * * *", CronError::InvalidField { field: "minute" }),
            ("1,,2 * * * *", CronError::InvalidField { field: "minute" }),
            ("001 * * * *", CronError::InvalidField { field: "minute" }),
            ("* * * foo *", CronError::InvalidField { field: "month" }),
            (
                "* * * * mon/0",
                CronError::InvalidField {
                    field: "day-of-week",
                },
            ),
            ("0 0 30 2 *", CronError::NeverMatches),
            ("0 0 31 4,6,9,11 *", CronError::NeverMatches),
        ];
        for (expression, expected) in cases {
            assert_eq!(
                Cron::parse(expression).as_ref(),
                Err(expected),
                "{expression:?}"
            );
        }
        assert_eq!(Cron::parse(&"*".repeat(300)), Err(CronError::TooLong));
        // 29 February and 31-day months are satisfiable.
        assert!(Cron::parse("0 0 29 2 *").is_ok());
        assert!(
            Cron::parse("0 0 30 2 mon").is_ok(),
            "either field may match"
        );
    }

    #[test]
    fn next_and_previous_ticks() {
        let day = days_of(2026, 10, 19); // a Monday
        assert_eq!(weekday(day), 1);
        let quarter: Cron = "*/15 * * * *".parse().expect("valid");
        assert_eq!(quarter.next_after(at(day, 10, 7)), Some(at(day, 10, 15)));
        assert_eq!(quarter.next_after(at(day, 10, 15)), Some(at(day, 10, 30)));
        assert_eq!(quarter.next_after(at(day, 23, 50)), Some(at(day + 1, 0, 0)));
        assert_eq!(
            quarter.latest_at_or_before(at(day, 10, 15)),
            Some(at(day, 10, 15))
        );
        assert_eq!(
            quarter.latest_at_or_before(at(day, 10, 14)),
            Some(at(day, 10, 0))
        );
        let within = at(day, 10, 15) + Duration::from_secs(59);
        assert_eq!(quarter.latest_at_or_before(within), Some(at(day, 10, 15)));
        assert_eq!(quarter.next_after(within), Some(at(day, 10, 30)));

        let nightly: Cron = "0 3 * * *".parse().expect("valid");
        assert_eq!(nightly.next_after(at(day, 3, 0)), Some(at(day + 1, 3, 0)));
        assert_eq!(
            nightly.latest_at_or_before(at(day, 2, 59)),
            Some(at(day - 1, 3, 0))
        );

        let leap: Cron = "0 0 29 2 *".parse().expect("valid");
        let from = at(days_of(2025, 3, 1), 0, 0);
        assert_eq!(leap.next_after(from), Some(at(days_of(2028, 2, 29), 0, 0)));
        assert_eq!(
            leap.latest_at_or_before(from),
            Some(at(days_of(2024, 2, 29), 0, 0))
        );

        // Friday the 13th semantics: the 13th OR any Friday.
        let either: Cron = "0 12 13 * fri".parse().expect("valid");
        assert_eq!(either.next_after(at(day, 0, 0)), Some(at(day + 4, 12, 0)));
        let weekdays_only: Cron = "0 9 * * 1-5".parse().expect("valid");
        let saturday = day + 5;
        assert_eq!(
            weekdays_only.next_after(at(saturday, 0, 0)),
            Some(at(saturday + 2, 9, 0))
        );

        // Before the epoch there is nothing.
        assert_eq!(nightly.latest_at_or_before(at(0, 2, 0)), None);
        assert_eq!(
            nightly.next_after(UNIX_EPOCH - Duration::from_secs(1)),
            None
        );
    }

    /// Deterministic SplitMix64 for reproducible generated cases.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: u32) -> u32 {
            u32::try_from(self.next() % u64::from(bound)).expect("bounded")
        }
    }

    /// A random field: its text and the expected value set (oracle).
    fn random_field(rng: &mut Rng, min: u32, max: u32) -> (String, Vec<u32>) {
        let width = max - min + 1;
        match rng.below(5) {
            0 => ("*".to_owned(), (min..=max).collect()),
            1 => {
                let step = 1 + rng.below(width.min(15));
                (
                    format!("*/{step}"),
                    (min..=max).step_by(step as usize).collect(),
                )
            }
            2 => {
                let value = min + rng.below(width);
                (value.to_string(), vec![value])
            }
            3 => {
                let low = min + rng.below(width);
                let high = low + rng.below(max - low + 1);
                let step = 1 + rng.below(4);
                (
                    format!("{low}-{high}/{step}"),
                    (low..=high).step_by(step as usize).collect(),
                )
            }
            _ => {
                let mut values: Vec<u32> = (0..1 + rng.below(4))
                    .map(|_| min + rng.below(width))
                    .collect();
                let text = values
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                values.sort_unstable();
                values.dedup();
                (text, values)
            }
        }
    }

    #[test]
    fn generated_expressions_agree_with_a_minute_by_minute_oracle() {
        let mut rng = Rng(0x5eed_f0f6_e000_0001);
        let start_day = days_of(2027, 12, 20);
        let mut checked = 0;
        while checked < 150 {
            let fields: Vec<(String, Vec<u32>)> = vec![
                random_field(&mut rng, 0, 59),
                random_field(&mut rng, 0, 23),
                random_field(&mut rng, 1, 31),
                // Keep months dense so most cases match within the window.
                if rng.below(4) == 0 {
                    random_field(&mut rng, 1, 12)
                } else {
                    ("*".to_owned(), (1..=12).collect())
                },
                random_field(&mut rng, 0, 6),
            ];
            let expression = fields
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let Ok(cron) = Cron::parse(&expression) else {
                // Only impossible dates may be rejected.
                assert!(
                    fields[2].0 != "*" && fields[4].0.starts_with('*'),
                    "{expression}"
                );
                continue;
            };
            let set = |index: usize, value: u32| fields[index].1.contains(&value);
            let restricted = |index: usize| !fields[index].0.starts_with('*');
            let oracle = |minute: i64| {
                let day = minute.div_euclid(MINUTES_PER_DAY);
                let of_day = u32::try_from(minute.rem_euclid(MINUTES_PER_DAY)).expect("small");
                let (_, month, dom) = civil_from_days(day);
                let wd = weekday(day);
                let by_dom = set(2, dom);
                let by_dow = set(4, wd);
                let day_ok = match (restricted(2), restricted(4)) {
                    (true, true) => by_dom || by_dow,
                    (true, false) => by_dom,
                    (false, true) => by_dow,
                    (false, false) => true,
                };
                set(0, of_day % 60) && set(1, of_day / 60) && set(3, month) && day_ok
            };
            let offset = i64::from(rng.below(1440 * 20));
            let origin = start_day * MINUTES_PER_DAY + offset;
            let window = 1440 * 45;
            let expected_next = (origin + 1..origin + window).find(|m| oracle(*m));
            if let Some(expected) = expected_next {
                assert_eq!(
                    cron.next_after(minute_time(origin) + Duration::from_secs(17)),
                    Some(minute_time(expected)),
                    "next_after for {expression:?} from minute {origin}"
                );
            }
            let expected_previous = (origin - window..=origin).rev().find(|m| oracle(*m));
            if let Some(expected) = expected_previous {
                assert_eq!(
                    cron.latest_at_or_before(minute_time(origin) + Duration::from_secs(42)),
                    Some(minute_time(expected)),
                    "latest_at_or_before for {expression:?} from minute {origin}"
                );
            }
            checked += 1;
        }
    }

    #[test]
    fn arbitrary_input_never_panics() {
        let alphabet: Vec<char> = "0123456789*/,- @abcdefjmnorstuvwyADFJMNOSW\t"
            .chars()
            .collect();
        let mut rng = Rng(0xf022_0000_0000_0042);
        for _ in 0..20_000 {
            let length = rng.below(40) as usize;
            let input: String = (0..length)
                .map(|_| {
                    alphabet[rng.below(u32::try_from(alphabet.len()).expect("small")) as usize]
                })
                .collect();
            if let Ok(cron) = Cron::parse(&input) {
                // Anything accepted must be evaluable.
                let _ = cron.next_after(at(20_000, 0, 0));
                assert_eq!(Cron::parse(cron.as_str()).as_ref(), Ok(&cron));
            }
        }
    }
}
