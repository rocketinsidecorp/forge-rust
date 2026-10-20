//! Localized numbers and dates from Rails' formatting keys (ADR 0037):
//! `number.format.*`, `date.formats.*`, `time.formats.*`, the day and month
//! names and `time.am`/`time.pm`.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Translator;
use crate::catalog::{Node, Tree, find};
use crate::locale::parents;

/// The most decimals a number is formatted with.
const MAX_PRECISION: usize = 20;
const SECONDS_PER_DAY: i64 = 86_400;

/// The English formats used when neither the locale, its parents nor the
/// default locale define a formatting key (Rails' `en` defaults).
fn builtin_formats() -> &'static Tree {
    static FORMATS: OnceLock<Tree> = OnceLock::new();
    FORMATS.get_or_init(|| {
        let text = |value: &str| Node::Text(value.to_owned());
        let list = |values: &[Option<&str>]| {
            Node::List(
                values
                    .iter()
                    .map(|value| value.map(str::to_owned))
                    .collect(),
            )
        };
        let map = |entries: Vec<(&str, Node)>| {
            Node::Map(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), value))
                    .collect(),
            )
        };
        let names =
            |values: &[&str]| list(&values.iter().map(|value| Some(*value)).collect::<Vec<_>>());
        let months = |values: &[&str]| {
            let mut all = vec![None];
            all.extend(values.iter().map(|value| Some(*value)));
            list(&all)
        };
        let root = map(vec![
            (
                "number",
                map(vec![(
                    "format",
                    map(vec![
                        ("separator", text(".")),
                        ("delimiter", text(",")),
                        ("precision", Node::Number(2.0)),
                    ]),
                )]),
            ),
            (
                "date",
                map(vec![
                    (
                        "formats",
                        map(vec![
                            ("default", text("%Y-%m-%d")),
                            ("short", text("%b %d")),
                            ("long", text("%B %d, %Y")),
                        ]),
                    ),
                    (
                        "day_names",
                        names(&[
                            "Sunday",
                            "Monday",
                            "Tuesday",
                            "Wednesday",
                            "Thursday",
                            "Friday",
                            "Saturday",
                        ]),
                    ),
                    (
                        "abbr_day_names",
                        names(&["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]),
                    ),
                    (
                        "month_names",
                        months(&[
                            "January",
                            "February",
                            "March",
                            "April",
                            "May",
                            "June",
                            "July",
                            "August",
                            "September",
                            "October",
                            "November",
                            "December",
                        ]),
                    ),
                    (
                        "abbr_month_names",
                        months(&[
                            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
                            "Nov", "Dec",
                        ]),
                    ),
                ]),
            ),
            (
                "time",
                map(vec![
                    (
                        "formats",
                        map(vec![
                            ("default", text("%a, %d %b %Y %H:%M:%S %z")),
                            ("short", text("%d %b %H:%M")),
                            ("long", text("%B %d, %Y %H:%M")),
                        ]),
                    ),
                    ("am", text("am")),
                    ("pm", text("pm")),
                ]),
            ),
        ]);
        match root {
            Node::Map(tree) => tree,
            _ => Tree::new(),
        }
    })
}

/// A number to format: an integer or a float.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Number {
    /// Any integer type.
    Integer(i128),
    /// `f32` or `f64`.
    Float(f64),
}

/// Numbers usable with [`Translator::format_number`], by value or by
/// reference (Askama passes template fields by reference).
pub trait IntoNumber {
    /// The number.
    fn into_number(self) -> Number;
}

impl IntoNumber for Number {
    fn into_number(self) -> Number {
        self
    }
}

macro_rules! into_number {
    ($variant:ident: $($ty:ty),*) => {$(
        impl IntoNumber for $ty {
            fn into_number(self) -> Number {
                into_number!(@convert $variant self)
            }
        }
        impl IntoNumber for &$ty {
            fn into_number(self) -> Number {
                (*self).into_number()
            }
        }
        impl IntoNumber for &&$ty {
            fn into_number(self) -> Number {
                (**self).into_number()
            }
        }
    )*};
    (@convert Integer $value:ident) => {
        Number::Integer(i128::try_from($value).unwrap_or(i128::MAX))
    };
    (@convert Float $value:ident) => {
        Number::Float(f64::from($value))
    };
}

into_number!(Integer: i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);
into_number!(Float: f32, f64);

/// An instant with the UTC offset (and zone name) it is shown in, for
/// [`Translator::format_time`]. A framework-owned value, so formatting
/// needs no date library: convert from yours with
/// [`DateTime::from_unix`] (chrono: `DateTime::from_unix(value.timestamp())`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DateTime {
    unix: i64,
    offset: i32,
    zone: Option<String>,
}

/// The calendar fields of a [`DateTime`] in its offset.
#[derive(Clone, Copy, Debug)]
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    /// 0 = Sunday.
    weekday: u32,
    /// 1 to 366.
    yearday: u32,
}

impl DateTime {
    /// The instant `seconds` after the Unix epoch, shown in UTC.
    #[must_use]
    pub fn from_unix(seconds: i64) -> Self {
        Self {
            unix: seconds,
            offset: 0,
            zone: None,
        }
    }

    /// A system time, shown in UTC (whole seconds).
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        let seconds = match time.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            Err(before) => {
                let duration = before.duration();
                let mut seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
                if duration.subsec_nanos() > 0 {
                    seconds = seconds.saturating_add(1);
                }
                seconds.saturating_neg()
            }
        };
        Self::from_unix(seconds)
    }

    /// A UTC calendar date and time; `None` when a field is out of range.
    #[must_use]
    pub fn from_utc(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        let valid = (1..=12).contains(&month)
            && day >= 1
            && day <= days_in_month(i64::from(year), month)
            && hour < 24
            && minute < 60
            && second < 60;
        if !valid {
            return None;
        }
        let days = days_from_civil(i64::from(year), month, day);
        let seconds = i64::from(hour * 3600 + minute * 60 + second);
        Some(Self::from_unix(days * SECONDS_PER_DAY + seconds))
    }

    /// The same instant shown at `offset_seconds` east of UTC, with an
    /// optional zone abbreviation for `%Z` (ASCII letters, digits, `+`,
    /// `-` and `_`, at most 16 bytes). `None` for offsets of a day or more
    /// or an invalid abbreviation.
    #[must_use]
    pub fn with_offset(mut self, offset_seconds: i32, zone: Option<&str>) -> Option<Self> {
        if offset_seconds.unsigned_abs() >= 86_400 {
            return None;
        }
        if let Some(zone) = zone {
            let valid = !zone.is_empty()
                && zone.len() <= 16
                && zone
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'_'));
            if !valid {
                return None;
            }
        }
        self.offset = offset_seconds;
        self.zone = zone.map(str::to_owned);
        Some(self)
    }

    /// Seconds since the Unix epoch.
    #[must_use]
    pub fn unix_seconds(&self) -> i64 {
        self.unix
    }

    fn civil(&self) -> Civil {
        let local = self.unix.saturating_add(i64::from(self.offset));
        let days = local.div_euclid(SECONDS_PER_DAY);
        let seconds = local.rem_euclid(SECONDS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        let yearday = days - days_from_civil(year, 1, 1) + 1;
        // Clamped by construction: the remainder is below a day.
        let seconds = u32::try_from(seconds).unwrap_or(0);
        Civil {
            year,
            month,
            day,
            hour: seconds / 3600,
            minute: seconds / 60 % 60,
            second: seconds % 60,
            weekday: u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0),
            yearday: u32::try_from(yearday).unwrap_or(1),
        }
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
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

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl Translator {
    /// A formatting value from the locale, its parents, the default locale
    /// and its parents, then the built-in English formats. Formatting data
    /// always falls back, in every environment.
    fn data(&self, locale: &str, key: &str) -> Option<&Node> {
        parents(locale)
            .chain(parents(&self.default_locale))
            .find_map(|candidate| self.catalog.get(candidate).and_then(|tree| find(tree, key)))
            .or_else(|| find(builtin_formats(), key))
    }

    fn data_text(&self, locale: &str, key: &str) -> &str {
        match self.data(locale, key) {
            Some(Node::Text(text)) => text,
            _ => "",
        }
    }

    fn data_name(&self, locale: &str, key: &str, index: u32) -> &str {
        let index = usize::try_from(index).unwrap_or(usize::MAX);
        name_at(self.data(locale, key), index)
            .or_else(|| name_at(find(builtin_formats(), key), index))
            .unwrap_or_default()
    }

    fn default_precision(&self, locale: &str) -> usize {
        match self.data(locale, "number.format.precision") {
            Some(Node::Number(value)) if value.fract() == 0.0 && (0.0..=20.0).contains(value) => {
                // A whole number from 0 to 20.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let precision = *value as usize;
                precision
            }
            _ => 2,
        }
    }

    /// Writes `value` with the locale's `number.format.separator` and
    /// `number.format.delimiter` (thousands grouping). Integers get no
    /// decimals unless `precision` is given; floats get `precision`
    /// decimals, or `number.format.precision` when it is `None` (at most
    /// 20; exact binary ties round to even).
    ///
    /// ```
    /// # let translator = forge_i18n::Translator::from_files(&[("en.json", r#"{"en":{}}"#)], forge_i18n::Options::new())?;
    /// assert_eq!(translator.format_number("en", 1234567.891, None), "1,234,567.89");
    /// assert_eq!(translator.format_number("en", 12, Some(2)), "12.00");
    /// # Ok::<(), forge_i18n::LoadError>(())
    /// ```
    #[must_use]
    pub fn format_number(
        &self,
        locale: &str,
        value: impl IntoNumber,
        precision: Option<u32>,
    ) -> String {
        let locale = self.match_locale(locale);
        let precision = precision.map(|p| {
            usize::try_from(p)
                .unwrap_or(MAX_PRECISION)
                .min(MAX_PRECISION)
        });
        let digits = match value.into_number() {
            Number::Integer(integer) => match precision {
                Some(places) if places > 0 => format!("{integer}.{}", "0".repeat(places)),
                _ => integer.to_string(),
            },
            Number::Float(float) => {
                if float.is_nan() {
                    return "NaN".to_owned();
                }
                if float.is_infinite() {
                    return if float > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
                }
                let places = precision.unwrap_or_else(|| self.default_precision(locale));
                format!("{float:.places$}")
            }
        };
        self.group_digits(locale, &digits)
    }

    /// Localizes `-1234567.50` as `-1,234,567.50` (en) or `-1.234.567,50`
    /// (pt-BR).
    fn group_digits(&self, locale: &str, digits: &str) -> String {
        let separator = self.data_text(locale, "number.format.separator");
        let delimiter = self.data_text(locale, "number.format.delimiter");
        let (sign, digits) = match digits.strip_prefix('-') {
            Some(rest) => ("-", rest),
            None => ("", digits),
        };
        let (whole, fraction) = match digits.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (digits, None),
        };
        let mut out = String::with_capacity(digits.len() + whole.len() / 3 * delimiter.len() + 2);
        out.push_str(sign);
        for (index, character) in whole.char_indices() {
            if index > 0 && (whole.len() - index) % 3 == 0 {
                out.push_str(delimiter);
            }
            out.push(character);
        }
        if let Some(fraction) = fraction {
            out.push_str(separator);
            out.push_str(fraction);
        }
        out
    }

    /// Formats `value` with a strftime pattern from the locale: `"short"`
    /// uses `time.formats.short` (empty means `"default"`), and a format
    /// containing a dot is a full key such as `"date.formats.long"`; an
    /// unknown format uses `time.formats.default`. Directives: `%Y %y %m
    /// %d %e %j %H %I %M %S %p %P %A %a %B %b %z %Z %%` (`%-d` removes
    /// padding); names come from `date.day_names`, `date.abbr_day_names`,
    /// `date.month_names` and `date.abbr_month_names`, `%p` from `time.am`
    /// and `time.pm`.
    #[must_use]
    pub fn format_time(&self, locale: &str, value: &DateTime, format: &str) -> String {
        let locale = self.match_locale(locale);
        let format = if format.is_empty() { "default" } else { format };
        let key = if format.contains('.') {
            format.to_owned()
        } else {
            format!("time.formats.{format}")
        };
        let mut pattern = self.data_text(locale, &key);
        if pattern.is_empty() {
            pattern = self.data_text(locale, "time.formats.default");
        }
        self.strftime(locale, value, pattern)
    }

    fn strftime(&self, locale: &str, value: &DateTime, pattern: &str) -> String {
        let civil = value.civil();
        let mut out = String::with_capacity(pattern.len() * 2);
        let mut characters = pattern.chars().peekable();
        while let Some(character) = characters.next() {
            if character != '%' || characters.peek().is_none() {
                out.push(character);
                continue;
            }
            let mut pad = true;
            let mut directive = characters.next().unwrap_or('%');
            if directive == '-'
                && let Some(next) = characters.next()
            {
                pad = false;
                directive = next;
            }
            let number = |out: &mut String, value: i64, width: usize| {
                let text = value.to_string();
                if pad && text.len() < width {
                    out.push_str(&"0".repeat(width - text.len()));
                }
                out.push_str(&text);
            };
            match directive {
                'Y' => out.push_str(&civil.year.to_string()),
                // Two digits of the year, never negative (year -5 is "95", as in C).
                'y' => number(&mut out, civil.year.rem_euclid(100), 2),
                'm' => number(&mut out, i64::from(civil.month), 2),
                'd' => number(&mut out, i64::from(civil.day), 2),
                'e' => {
                    if pad && civil.day < 10 {
                        out.push(' ');
                    }
                    out.push_str(&civil.day.to_string());
                }
                'j' => number(&mut out, i64::from(civil.yearday), 3),
                'H' => number(&mut out, i64::from(civil.hour), 2),
                'I' => {
                    let hour = match civil.hour % 12 {
                        0 => 12,
                        hour => hour,
                    };
                    number(&mut out, i64::from(hour), 2);
                }
                'M' => number(&mut out, i64::from(civil.minute), 2),
                'S' => number(&mut out, i64::from(civil.second), 2),
                'p' | 'P' => {
                    let key = if civil.hour >= 12 {
                        "time.pm"
                    } else {
                        "time.am"
                    };
                    let marker = self.data_text(locale, key);
                    if directive == 'p' {
                        out.push_str(&marker.to_uppercase());
                    } else {
                        out.push_str(&marker.to_lowercase());
                    }
                }
                'A' => out.push_str(self.data_name(locale, "date.day_names", civil.weekday)),
                'a' => out.push_str(self.data_name(locale, "date.abbr_day_names", civil.weekday)),
                'B' => out.push_str(self.data_name(locale, "date.month_names", civil.month)),
                'b' => out.push_str(self.data_name(locale, "date.abbr_month_names", civil.month)),
                'z' => out.push_str(&offset_text(value.offset)),
                'Z' => match (&value.zone, value.offset) {
                    (Some(zone), _) => out.push_str(zone),
                    (None, 0) => out.push_str("UTC"),
                    (None, offset) => out.push_str(&offset_text(offset)),
                },
                '%' => out.push('%'),
                other => {
                    out.push('%');
                    if !pad {
                        out.push('-');
                    }
                    out.push(other);
                }
            }
        }
        out
    }
}

/// The name at `index` of a list of names.
fn name_at(node: Option<&Node>, index: usize) -> Option<&str> {
    match node {
        Some(Node::List(list)) => list.get(index).and_then(|name| name.as_deref()),
        _ => None,
    }
}

/// `-0300`: the sign, hours and minutes of an offset.
fn offset_text(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    format!("{sign}{:02}{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_conversion_round_trips() {
        for days in [-719_468, -1, 0, 1, 10_957, 20_517, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days, "{days}");
        }
        let leap = DateTime::from_utc(2024, 2, 29, 23, 59, 59).map(|t| t.civil());
        assert_eq!(leap.map(|c| (c.yearday, c.weekday)), Some((60, 4)));
        assert!(DateTime::from_utc(2023, 2, 29, 0, 0, 0).is_none());
        assert!(DateTime::from_utc(2023, 13, 1, 0, 0, 0).is_none());
        assert_eq!(DateTime::from_unix(0).civil().weekday, 4);
        let before = DateTime::from_system_time(UNIX_EPOCH - std::time::Duration::from_millis(1));
        assert_eq!(before.unix_seconds(), -1);
    }

    #[test]
    fn offsets_and_zones_are_validated() {
        let utc = DateTime::from_unix(0);
        assert!(utc.clone().with_offset(86_400, None).is_none());
        assert!(utc.clone().with_offset(0, Some("a b")).is_none());
        assert!(utc.with_offset(-3 * 3600, Some("BRT")).is_some());
        assert_eq!(offset_text(-3 * 3600), "-0300");
        assert_eq!(offset_text(5 * 3600 + 1800), "+0530");
    }
}
