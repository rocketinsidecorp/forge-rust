//! Durations written the way Forge-Go writes them (`time.ParseDuration` and
//! `time.Duration.String`): `500ms`, `30s`, `1m0s`, `1h30m0s`.
//!
//! `config/deploy.yml`, the `forge-proxy deploy` flags and the proxy's state
//! file use this text form, so a configuration, a command line or a state
//! volume written by either framework is read identically by the other.

use std::fmt;

/// Nanoseconds per unit.
const NANOSECOND: u64 = 1;
const MICROSECOND: u64 = 1_000 * NANOSECOND;
const MILLISECOND: u64 = 1_000 * MICROSECOND;
const SECOND: u64 = 1_000 * MILLISECOND;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;

/// A signed number of nanoseconds, like Go's `time.Duration`.
///
/// Negative values exist so that validation can report them ("must be
/// between ...") exactly as Forge-Go does, instead of failing to parse.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GoDuration(i64);

/// A duration that is not in Go's syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseDurationError {
    message: String,
}

impl fmt::Display for ParseDurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParseDurationError {}

impl GoDuration {
    /// Zero.
    pub const ZERO: Self = Self(0);

    /// From nanoseconds.
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// From milliseconds (saturating).
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis.saturating_mul(1_000_000))
    }

    /// From seconds (saturating).
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self(secs.saturating_mul(1_000_000_000))
    }

    /// The number of nanoseconds.
    #[must_use]
    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    /// Whole seconds, truncated toward zero (Go's `int(d.Seconds())`).
    #[must_use]
    pub const fn whole_seconds(self) -> i64 {
        self.0 / 1_000_000_000
    }

    /// Whether the duration is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The duration as a [`std::time::Duration`]; negative values are zero.
    #[must_use]
    pub fn to_std(self) -> std::time::Duration {
        std::time::Duration::from_nanos(u64::try_from(self.0).unwrap_or(0))
    }

    /// From a [`std::time::Duration`] (saturating at Go's maximum).
    #[must_use]
    pub fn from_std(duration: std::time::Duration) -> Self {
        Self(i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX))
    }

    /// Parses Go's duration syntax: an optional sign and a sequence of
    /// decimal numbers, each with an optional fraction and a unit (`ns`,
    /// `us`, `µs`, `μs`, `ms`, `s`, `m`, `h`), such as `300ms`, `-1.5h` or
    /// `2h45m`. A bare `0` is zero.
    ///
    /// # Errors
    ///
    /// Returns the same message as Go for malformed or overflowing text.
    pub fn parse(text: &str) -> Result<Self, ParseDurationError> {
        let invalid = || ParseDurationError {
            message: format!("time: invalid duration {}", go_quote(text)),
        };
        let mut rest = text;
        let mut negative = false;
        if let Some(first) = rest.chars().next()
            && (first == '-' || first == '+')
        {
            negative = first == '-';
            rest = &rest[1..];
        }
        if rest == "0" {
            return Ok(Self::ZERO);
        }
        if rest.is_empty() {
            return Err(invalid());
        }
        let mut total: u64 = 0;
        while !rest.is_empty() {
            let bytes = rest.as_bytes();
            if !(bytes[0] == b'.' || bytes[0].is_ascii_digit()) {
                return Err(invalid());
            }
            // The integer part.
            let before = rest.len();
            let (whole, after) = leading_int(rest).ok_or_else(invalid)?;
            rest = after;
            let has_whole = before != rest.len();
            // The fraction.
            let mut fraction: u64 = 0;
            let mut scale: f64 = 1.0;
            let mut has_fraction = false;
            if let Some(after_dot) = rest.strip_prefix('.') {
                let before = after_dot.len();
                let (value, value_scale, after) = leading_fraction(after_dot);
                fraction = value;
                scale = value_scale;
                rest = after;
                has_fraction = before != rest.len();
            }
            if !has_whole && !has_fraction {
                return Err(invalid());
            }
            // The unit runs until the next digit or dot.
            let unit_len = rest
                .char_indices()
                .find(|(_, c)| *c == '.' || c.is_ascii_digit())
                .map_or(rest.len(), |(index, _)| index);
            if unit_len == 0 {
                return Err(ParseDurationError {
                    message: format!("time: missing unit in duration {}", go_quote(text)),
                });
            }
            let unit_text = &rest[..unit_len];
            rest = &rest[unit_len..];
            let unit = match unit_text {
                "ns" => NANOSECOND,
                "us" | "\u{00b5}s" | "\u{03bc}s" => MICROSECOND,
                "ms" => MILLISECOND,
                "s" => SECOND,
                "m" => MINUTE,
                "h" => HOUR,
                _ => {
                    return Err(ParseDurationError {
                        message: format!(
                            "time: unknown unit {} in duration {}",
                            go_quote(unit_text),
                            go_quote(text)
                        ),
                    });
                }
            };
            const LIMIT: u64 = 1 << 63;
            if whole > LIMIT / unit {
                return Err(invalid());
            }
            let mut value = whole * unit;
            if fraction > 0 {
                // Go computes the fractional part in float64 the same way.
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let extra = (fraction as f64 * (unit as f64 / scale)) as u64;
                value = value.checked_add(extra).ok_or_else(invalid)?;
                if value > LIMIT {
                    return Err(invalid());
                }
            }
            total = total.checked_add(value).ok_or_else(invalid)?;
            if total > LIMIT {
                return Err(invalid());
            }
        }
        if negative {
            // -(1<<63) is representable.
            if total == 1 << 63 {
                return Ok(Self(i64::MIN));
            }
            let positive = i64::try_from(total).map_err(|_| invalid())?;
            return Ok(Self(-positive));
        }
        i64::try_from(total).map(Self).map_err(|_| invalid())
    }
}

/// Consumes leading decimal digits. `None` on overflow.
fn leading_int(text: &str) -> Option<(u64, &str)> {
    let mut value: u64 = 0;
    let mut consumed = 0;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            break;
        }
        if value > (1 << 63) / 10 {
            return None;
        }
        value = value * 10 + u64::from(byte - b'0');
        if value > 1 << 63 {
            return None;
        }
        consumed += 1;
    }
    Some((value, &text[consumed..]))
}

/// Consumes leading digits of a fraction, ignoring precision beyond what
/// fits, as Go does.
fn leading_fraction(text: &str) -> (u64, f64, &str) {
    let mut value: u64 = 0;
    let mut scale: f64 = 1.0;
    let mut overflow = false;
    let mut consumed = 0;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            break;
        }
        consumed += 1;
        if overflow {
            continue;
        }
        if value > (i64::MAX as u64) / 10 {
            overflow = true;
            continue;
        }
        let next = value * 10 + u64::from(byte - b'0');
        if next > 1 << 63 {
            overflow = true;
            continue;
        }
        value = next;
        scale *= 10.0;
    }
    (value, scale, &text[consumed..])
}

/// Go's `strconv.Quote` for the ASCII text durations are made of.
fn go_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            c if c.is_control() => {
                quoted.push_str(&format!("\\x{:02x}", u32::from(c)));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

impl fmt::Display for GoDuration {
    /// Go's `time.Duration.String`: `0s`, `1.5µs`, `500ms`, `2s`, `1m0s`,
    /// `1h30m0s`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.0 < 0;
        let mut value = self.0.unsigned_abs();
        // Built back to front, like Go.
        let mut out: Vec<char> = Vec::with_capacity(32);
        if value < SECOND {
            if value == 0 {
                return formatter.write_str("0s");
            }
            out.push('s');
            let precision = if value < MICROSECOND {
                out.push('n');
                0
            } else if value < MILLISECOND {
                out.push('\u{00b5}');
                3
            } else {
                out.push('m');
                6
            };
            value = push_fraction(&mut out, value, precision);
            push_int(&mut out, value);
        } else {
            out.push('s');
            value = push_fraction(&mut out, value, 9);
            push_int(&mut out, value % 60);
            value /= 60;
            if value > 0 {
                out.push('m');
                push_int(&mut out, value % 60);
                value /= 60;
                if value > 0 {
                    out.push('h');
                    push_int(&mut out, value);
                }
            }
        }
        if negative {
            out.push('-');
        }
        let text: String = out.into_iter().rev().collect();
        formatter.write_str(&text)
    }
}

/// Pushes (reversed) the fraction of `value / 10^precision` without
/// trailing zeros, and returns the integer part.
fn push_fraction(out: &mut Vec<char>, mut value: u64, precision: u32) -> u64 {
    let mut print = false;
    for _ in 0..precision {
        let digit = value % 10;
        print = print || digit != 0;
        if print {
            out.push(char::from(b'0' + u8::try_from(digit).unwrap_or(0)));
        }
        value /= 10;
    }
    if print {
        out.push('.');
    }
    value
}

/// Pushes (reversed) the decimal digits of `value`.
fn push_int(out: &mut Vec<char>, mut value: u64) {
    if value == 0 {
        out.push('0');
        return;
    }
    while value > 0 {
        out.push(char::from(b'0' + u8::try_from(value % 10).unwrap_or(0)));
        value /= 10;
    }
}

impl serde::Serialize for GoDuration {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for GoDuration {
    /// Only strings are accepted, as in Forge-Go's state and control JSON.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = GoDuration;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("duration must be a string such as \"5s\"")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<GoDuration, E> {
                GoDuration::parse(value)
                    .map_err(|_| E::custom(format!("invalid duration {}", go_quote(value))))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_go() {
        // Cases from Go's time package tests.
        let cases: &[(&str, i64)] = &[
            ("0", 0),
            ("5s", 5 * SECOND as i64),
            ("30s", 30 * SECOND as i64),
            ("1478s", 1478 * SECOND as i64),
            ("-5s", -5 * SECOND as i64),
            ("+5s", 5 * SECOND as i64),
            ("-0", 0),
            ("+0", 0),
            ("5.0s", 5 * SECOND as i64),
            ("5.6s", 5 * SECOND as i64 + 600 * MILLISECOND as i64),
            ("5.s", 5 * SECOND as i64),
            (".5s", 500 * MILLISECOND as i64),
            ("1.0s", SECOND as i64),
            ("1.00s", SECOND as i64),
            ("1.004s", SECOND as i64 + 4 * MILLISECOND as i64),
            ("1.0040s", SECOND as i64 + 4 * MILLISECOND as i64),
            ("100.00100s", 100 * SECOND as i64 + MILLISECOND as i64),
            ("10ns", 10),
            ("11us", 11 * MICROSECOND as i64),
            ("12\u{00b5}s", 12 * MICROSECOND as i64),
            ("12\u{03bc}s", 12 * MICROSECOND as i64),
            ("13ms", 13 * MILLISECOND as i64),
            ("14s", 14 * SECOND as i64),
            ("15m", 15 * MINUTE as i64),
            ("16h", 16 * HOUR as i64),
            ("3h30m", 3 * HOUR as i64 + 30 * MINUTE as i64),
            (
                "10.5s4m",
                4 * MINUTE as i64 + 10 * SECOND as i64 + 500 * MILLISECOND as i64,
            ),
            (
                "-2m3.4s",
                -(2 * MINUTE as i64 + 3 * SECOND as i64 + 400 * MILLISECOND as i64),
            ),
            ("1h2m3s4ms5us6ns", 3_723_004_005_006),
            ("39h9m14.425s", 140_954_425 * MILLISECOND as i64),
            ("52763797000ns", 52_763_797_000),
            ("0.3333333333333333333h", 20 * MINUTE as i64),
            ("9007199254740993ns", 9_007_199_254_740_993),
            ("9223372036854775807ns", i64::MAX),
            ("9223372036854775.807us", i64::MAX),
            ("-9223372036854775808ns", i64::MIN),
            ("0.100000000000000000000h", 6 * MINUTE as i64),
            ("1m0s", MINUTE as i64),
        ];
        for (text, nanos) in cases {
            assert_eq!(
                GoDuration::parse(text),
                Ok(GoDuration(*nanos)),
                "parse({text:?})"
            );
        }
    }

    #[test]
    fn rejects_like_go() {
        for text in [
            "",
            "3",
            "-",
            "s",
            ".",
            "-.",
            ".s",
            "+.s",
            "1d",
            "\u{85}\u{85}",
            "\u{ffff}",
            "9223372036854775810ns",
            "9223372036854775808ns",
            "3000000h",
            "5 s",
            "1e3s",
            "soon",
        ] {
            assert!(
                GoDuration::parse(text).is_err(),
                "parse({text:?}) succeeded"
            );
        }
        assert_eq!(
            GoDuration::parse("soon").unwrap_err().to_string(),
            "time: invalid duration \"soon\""
        );
        assert_eq!(
            GoDuration::parse("3").unwrap_err().to_string(),
            "time: missing unit in duration \"3\""
        );
        assert_eq!(
            GoDuration::parse("1d").unwrap_err().to_string(),
            "time: unknown unit \"d\" in duration \"1d\""
        );
    }

    #[test]
    fn formats_like_go() {
        let cases: &[(i64, &str)] = &[
            (0, "0s"),
            (1, "1ns"),
            (1_100, "1.1\u{00b5}s"),
            (2_200_000, "2.2ms"),
            (3_300_000_000, "3.3s"),
            (4 * MINUTE as i64 + 5 * SECOND as i64, "4m5s"),
            (4 * MINUTE as i64 + 5_001 * MILLISECOND as i64, "4m5.001s"),
            (
                5 * HOUR as i64 + 6 * MINUTE as i64 + 7_001 * MILLISECOND as i64,
                "5h6m7.001s",
            ),
            (8 * MINUTE as i64 + 1, "8m0.000000001s"),
            (i64::MAX, "2562047h47m16.854775807s"),
            (i64::MIN, "-2562047h47m16.854775808s"),
            (-1, "-1ns"),
            (MINUTE as i64, "1m0s"),
            (HOUR as i64, "1h0m0s"),
            (500 * MILLISECOND as i64, "500ms"),
            (30 * SECOND as i64, "30s"),
        ];
        for (nanos, text) in cases {
            assert_eq!(GoDuration(*nanos).to_string(), *text, "format({nanos})");
        }
    }

    #[test]
    fn round_trips_through_text_and_json() {
        for text in ["500ms", "2s", "1m0s", "1h0m0s", "1.5s", "250\u{00b5}s"] {
            let parsed = GoDuration::parse(text).expect("valid");
            assert_eq!(parsed.to_string(), text);
            let json = serde_json::to_string(&parsed).expect("serializes");
            assert_eq!(json, format!("\"{text}\""));
            let back: GoDuration = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back, parsed);
        }
        assert!(serde_json::from_str::<GoDuration>("5").is_err());
        assert!(serde_json::from_str::<GoDuration>("\"5\"").is_err());
    }
}
