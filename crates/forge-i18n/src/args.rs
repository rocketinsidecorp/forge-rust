//! Interpolation arguments: `%{name}` values and the plural `count`.

use std::borrow::Cow;
use std::fmt;

use crate::plural::Operands;

/// The value of a `%{name}` placeholder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value<'a> {
    /// Text. In an `_html` translation it is HTML-escaped.
    Text(Cow<'a, str>),
    /// Trusted markup, interpolated as is even in an `_html` translation
    /// (Forge-Go's `template.HTML`). Never pass request input here.
    Markup(Cow<'a, str>),
}

/// A plural count: an integer or a float, with the text it interpolates as.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Count {
    operands: Operands,
    value: CountValue,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CountValue {
    Integer(i128),
    Float(f64),
}

impl Count {
    /// The CLDR operands of the count.
    #[must_use]
    pub fn operands(&self) -> Operands {
        self.operands
    }

    /// Parses a count written as text, as a `count` argument given with
    /// [`Args::with`] is: an integer, else a decimal number. `None` for
    /// anything else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Ok(integer) = text.parse::<i128>() {
            return Some(Self::from(integer));
        }
        let is_decimal = !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'+'));
        if !is_decimal {
            return None;
        }
        let value = text.parse::<f64>().ok()?;
        let mut operands = Operands::from_float(value);
        // CLDR counts the fraction digits as written: "1.50" has two.
        if let Some((_, fraction)) = text.split_once('.') {
            operands.v = u32::try_from(fraction.len()).unwrap_or(u32::MAX);
        }
        Some(Self {
            operands,
            value: CountValue::Float(value),
        })
    }
}

impl fmt::Display for Count {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.value {
            CountValue::Integer(value) => write!(formatter, "{value}"),
            CountValue::Float(value) => write!(formatter, "{value}"),
        }
    }
}

impl From<i128> for Count {
    fn from(value: i128) -> Self {
        Self {
            operands: Operands::from_integer(value),
            value: CountValue::Integer(value),
        }
    }
}

impl From<f64> for Count {
    fn from(value: f64) -> Self {
        Self {
            operands: Operands::from_float(value),
            value: CountValue::Float(value),
        }
    }
}

impl From<f32> for Count {
    fn from(value: f32) -> Self {
        // Through the shortest decimal form, so 1.1f32 counts as 1.1.
        let widened = value.to_string().parse::<f64>().unwrap_or(f64::NAN);
        Self::from(widened)
    }
}

macro_rules! count_from_integers {
    ($($ty:ty),*) => {$(
        impl From<$ty> for Count {
            fn from(value: $ty) -> Self {
                Self::from(i128::from(value))
            }
        }
    )*};
}

count_from_integers!(i8, i16, i32, i64, u8, u16, u32, u64);

impl From<isize> for Count {
    fn from(value: isize) -> Self {
        Self::from(i128::try_from(value).unwrap_or(i128::MAX))
    }
}

impl From<usize> for Count {
    fn from(value: usize) -> Self {
        Self::from(i128::try_from(value).unwrap_or(i128::MAX))
    }
}

impl From<u128> for Count {
    fn from(value: u128) -> Self {
        Self::from(i128::try_from(value).unwrap_or(i128::MAX))
    }
}

/// Numbers usable as a plural count, by value or by reference (Askama
/// passes template fields by reference).
pub trait IntoCount {
    /// The count.
    fn into_count(self) -> Count;
}

impl IntoCount for Count {
    fn into_count(self) -> Count {
        self
    }
}

macro_rules! into_count {
    ($($ty:ty),*) => {$(
        impl IntoCount for $ty {
            fn into_count(self) -> Count {
                Count::from(self)
            }
        }
        impl IntoCount for &$ty {
            fn into_count(self) -> Count {
                Count::from(*self)
            }
        }
        impl IntoCount for &&$ty {
            fn into_count(self) -> Count {
                Count::from(**self)
            }
        }
    )*};
}

into_count!(
    i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64
);

/// The named arguments of a translation.
///
/// ```
/// use forge_i18n::Args;
///
/// let args = Args::new().with("name", "Ana").count(3);
/// assert_eq!(args.len(), 2);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Args<'a> {
    values: Vec<(Cow<'a, str>, Value<'a>)>,
    count: Option<Count>,
}

impl<'a> Args<'a> {
    /// No arguments.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `%{name}` as text (escaped in `_html` translations). A `count`
    /// given this way also selects the plural form when it is a number.
    #[must_use]
    pub fn with(mut self, name: impl Into<Cow<'a, str>>, value: impl fmt::Display) -> Self {
        self.push(name.into(), Value::Text(Cow::Owned(value.to_string())));
        self
    }

    /// Adds `%{name}` as trusted markup: interpolated without escaping in
    /// `_html` translations. Only pass markup your own code built.
    #[must_use]
    pub fn with_markup(
        mut self,
        name: impl Into<Cow<'a, str>>,
        markup: impl Into<Cow<'a, str>>,
    ) -> Self {
        self.push(name.into(), Value::Markup(markup.into()));
        self
    }

    /// Sets `count`: it selects the plural form and interpolates as
    /// `%{count}`.
    #[must_use]
    pub fn count(mut self, count: impl IntoCount) -> Self {
        let count = count.into_count();
        self.count = Some(count);
        self.push(
            Cow::Borrowed("count"),
            Value::Text(Cow::Owned(count.to_string())),
        );
        self
    }

    fn push(&mut self, name: Cow<'a, str>, value: Value<'a>) {
        if let Some(entry) = self
            .values
            .iter_mut()
            .find(|(existing, _)| *existing == name)
        {
            entry.1 = value;
        } else {
            self.values.push((name, value));
        }
    }

    /// The number of arguments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether there are no arguments.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Value<'a>> {
        self.values
            .iter()
            .find_map(|(existing, value)| (existing == name).then_some(value))
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.values.iter().map(|(name, _)| name.as_ref())
    }

    /// The plural count: the one set with [`Args::count`], else a numeric
    /// `count` text. `Err` names a `count` that is not a number.
    pub(crate) fn plural_count(&self) -> Result<Option<Count>, String> {
        if let Some(count) = self.count {
            return Ok(Some(count));
        }
        match self.get("count") {
            None => Ok(None),
            Some(Value::Text(text) | Value::Markup(text)) => Count::parse(text)
                .map(Some)
                .ok_or_else(|| "count is text, not a number".to_owned()),
        }
    }
}

/// Whether `name` can be a placeholder name: ASCII letters, digits and
/// `_`, not starting with a digit, at most 64 bytes.
pub(crate) fn is_argument_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_parse_integers_and_decimals_as_written() {
        let parsed = Count::parse("1.50").map(|count| count.operands());
        assert_eq!(parsed.map(|o| (o.i, o.v)), Some((1, 2)));
        assert_eq!(Count::parse("-3").map(|c| c.to_string()), Some("-3".into()));
        assert!(Count::parse("many").is_none());
        assert!(Count::parse("inf").is_none());
        assert_eq!(Count::from(1.5_f32).to_string(), "1.5");
        assert_eq!(Count::from(2.0).to_string(), "2");
    }

    #[test]
    fn references_are_counts_too() {
        let value = 7_i64;
        let reference = &value;
        assert_eq!(reference.into_count().to_string(), "7");
        assert_eq!((&reference).into_count().to_string(), "7");
    }

    #[test]
    fn a_name_given_twice_keeps_the_last_value() {
        let args = Args::new().with("name", "a").with("name", "b");
        assert_eq!(args.len(), 1);
        assert_eq!(args.get("name"), Some(&Value::Text("b".into())));
    }

    #[test]
    fn argument_names_are_identifiers() {
        for good in ["count", "name_2", "_x", "N"] {
            assert!(is_argument_name(good), "{good}");
        }
        for bad in ["", "2x", "a b", "a-b", "é", &"x".repeat(65)] {
            assert!(!is_argument_name(bad), "{bad}");
        }
    }
}
