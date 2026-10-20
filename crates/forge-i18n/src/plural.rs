//! CLDR 46 cardinal plural rules, built in and data-free (ADR 0037).

use std::fmt;

use crate::locale::parents;

/// A CLDR plural category.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Category {
    /// `zero`.
    Zero,
    /// `one`.
    One,
    /// `two`.
    Two,
    /// `few`.
    Few,
    /// `many`.
    Many,
    /// `other`, which every language uses and every plural object must have.
    Other,
}

impl Category {
    /// Every category, in CLDR order.
    pub const ALL: [Self; 6] = [
        Self::Zero,
        Self::One,
        Self::Two,
        Self::Few,
        Self::Many,
        Self::Other,
    ];

    /// The key of the category in a plural object (`"one"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::One => "one",
            Self::Two => "two",
            Self::Few => "few",
            Self::Many => "many",
            Self::Other => "other",
        }
    }

    /// The category named `key`, if it is one.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.as_str() == key)
    }
}

impl fmt::Display for Category {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The CLDR plural operands of a count: `n` is its absolute value, `i` its
/// integer digits and `v` the number of visible fraction digits (0 for
/// integers). Counts beyond the `i64` range saturate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Operands {
    /// Absolute value.
    pub n: f64,
    /// Integer digits of the absolute value.
    pub i: i64,
    /// Number of visible fraction digits.
    pub v: u32,
}

impl Operands {
    /// The operands of an integer count.
    #[must_use]
    pub fn from_integer(value: i128) -> Self {
        let magnitude = i64::try_from(value.unsigned_abs()).unwrap_or(i64::MAX);
        #[allow(clippy::cast_precision_loss)] // n is the CLDR value, used only for "n = 1"
        let n = magnitude as f64;
        Self {
            n,
            i: magnitude,
            v: 0,
        }
    }

    /// The operands of a float count: `v` counts the digits of its shortest
    /// round-trip decimal form (`1.5` has one). NaN and infinities never
    /// select a singular form.
    #[must_use]
    pub fn from_float(value: f64) -> Self {
        if !value.is_finite() {
            return Self { n: 0.0, i: 0, v: 1 };
        }
        let value = value.abs();
        let text = value.to_string();
        let visible = text.split_once('.').map_or(0, |(_, fraction)| {
            u32::try_from(fraction.len()).unwrap_or(u32::MAX)
        });
        #[allow(clippy::cast_possible_truncation)] // saturating by construction
        let integer = if value < 9_223_372_036_854_775_807.0 {
            value.trunc() as i64
        } else {
            i64::MAX
        };
        Self {
            n: value,
            i: integer,
            v: visible,
        }
    }

    /// Whether the count is the integer `n` (CLDR `n = x`).
    fn is(self, n: i64) -> bool {
        self.v == 0 && self.i == n
    }

    /// The CLDR `many` of French, Spanish, Italian and Portuguese: a whole,
    /// non-zero number of millions.
    fn millions(self) -> bool {
        self.v == 0 && self.i != 0 && self.i % 1_000_000 == 0
    }
}

/// Chooses the plural category of a count. Rules for the languages Forge
/// ships are built in ([`plural_rule_for`]); add others with
/// [`Options::plural_rule`](crate::Options::plural_rule), transcribing the
/// language's rule from the CLDR plural rules chart
/// (<https://cldr.unicode.org/index/cldr-spec/plural-rules>).
pub type PluralRule = fn(Operands) -> Category;

fn slavic(o: Operands) -> Category {
    if o.v != 0 {
        return Category::Other;
    }
    let (mod10, mod100) = (o.i % 10, o.i % 100);
    if mod10 == 1 && mod100 != 11 {
        Category::One
    } else if (2..=4).contains(&mod10) && !(12..=14).contains(&mod100) {
        Category::Few
    } else {
        Category::Many
    }
}

fn only_other(_: Operands) -> Category {
    Category::Other
}

/// English, German, Dutch and the Nordic languages: "1 item", "0 items",
/// "1.0 items".
fn one_if_integer_one(o: Operands) -> Category {
    if o.i == 1 && o.v == 0 {
        Category::One
    } else {
        Category::Other
    }
}

/// `n = 1`: Turkish, Greek and Hungarian.
fn one_if_exactly_one(o: Operands) -> Category {
    #[allow(clippy::float_cmp)] // CLDR compares the exact value
    if o.n == 1.0 {
        Category::One
    } else {
        Category::Other
    }
}

fn spanish(o: Operands) -> Category {
    #[allow(clippy::float_cmp)]
    if o.n == 1.0 {
        Category::One
    } else if o.millions() {
        Category::Many
    } else {
        Category::Other
    }
}

fn italian(o: Operands) -> Category {
    if o.i == 1 && o.v == 0 {
        Category::One
    } else if o.millions() {
        Category::Many
    } else {
        Category::Other
    }
}

/// French, and Portuguese as CLDR's `pt` (Brazilian): 0 and 1 are singular.
fn zero_or_one_singular(o: Operands) -> Category {
    if o.i == 0 || o.i == 1 {
        Category::One
    } else if o.millions() {
        Category::Many
    } else {
        Category::Other
    }
}

fn polish(o: Operands) -> Category {
    if o.v != 0 {
        return Category::Other;
    }
    let (mod10, mod100) = (o.i % 10, o.i % 100);
    if o.i == 1 {
        Category::One
    } else if (2..=4).contains(&mod10) && !(12..=14).contains(&mod100) {
        Category::Few
    } else {
        Category::Many
    }
}

fn czech(o: Operands) -> Category {
    if o.v != 0 {
        Category::Many
    } else if o.i == 1 {
        Category::One
    } else if (2..=4).contains(&o.i) {
        Category::Few
    } else {
        Category::Other
    }
}

fn arabic(o: Operands) -> Category {
    if o.v != 0 {
        return Category::Other;
    }
    let mod100 = o.i % 100;
    match o.i {
        0 => Category::Zero,
        1 => Category::One,
        2 => Category::Two,
        _ if (3..=10).contains(&mod100) => Category::Few,
        _ if mod100 >= 11 => Category::Many,
        _ => Category::Other,
    }
}

fn hebrew(o: Operands) -> Category {
    if o.is(1) || (o.i == 0 && o.v != 0) {
        Category::One
    } else if o.is(2) {
        Category::Two
    } else {
        Category::Other
    }
}

/// The CLDR 46 cardinal rules Forge ships, by canonical locale.
fn builtin(locale: &str) -> Option<PluralRule> {
    Some(match locale {
        "en" | "de" | "nl" | "sv" | "nb" | "fi" | "et" => one_if_integer_one,
        "tr" | "el" | "hu" => one_if_exactly_one,
        "ja" | "zh" | "ko" | "vi" | "th" | "id" | "ms" => only_other,
        "es" => spanish,
        "it" => italian,
        // "pt" is Brazilian Portuguese in CLDR.
        "fr" | "pt" => zero_or_one_singular,
        // European Portuguese: only 1 is singular.
        "pt-PT" => italian,
        "ru" | "uk" => slavic,
        "pl" => polish,
        "cs" | "sk" => czech,
        "ar" => arabic,
        "he" => hebrew,
        _ => return None,
    })
}

/// The built-in rule of a canonical locale, trying its parents (`pt-BR`
/// uses `pt`, `pt-PT` has its own). Locales without one use
/// [`Category::Other`] for every count.
#[must_use]
pub fn plural_rule_for(locale: &str) -> PluralRule {
    rule_in(&[], locale)
}

pub(crate) fn rule_in(custom: &[(String, PluralRule)], locale: &str) -> PluralRule {
    for candidate in parents(locale) {
        if let Some((_, rule)) = custom.iter().find(|(name, _)| name == candidate) {
            return *rule;
        }
        if let Some(rule) = builtin(candidate) {
            return rule;
        }
    }
    only_other
}

/// The categories a rule gives the integers 0 to 1000: the forms a
/// translator must provide. The `many` of whole millions (French, Spanish,
/// Italian, Portuguese) is left out: it falls back to `other`.
pub(crate) fn used_categories(rule: PluralRule) -> Vec<Category> {
    let mut seen = [false; 6];
    for n in 0..=1000 {
        let category = rule(Operands::from_integer(n));
        if let Some(index) = Category::ALL.iter().position(|c| *c == category) {
            seen[index] = true;
        }
    }
    Category::ALL
        .into_iter()
        .zip(seen)
        .filter_map(|(category, used)| used.then_some(category))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Count;

    /// The built-in rules against CLDR 46 samples (Forge-Go's table).
    #[test]
    fn rules_match_cldr_samples() {
        use Category::{Few as Fw, Many as M, One as O, Other as F, Two as Tw, Zero as Z};
        let i = |value: i128| Count::from(value);
        let f = |value: f64| Count::from(value);
        let tables: Vec<(&str, Vec<(Count, Category)>)> = vec![
            (
                "en",
                vec![
                    (i(0), F),
                    (i(1), O),
                    (i(2), F),
                    (i(-1), O),
                    (f(1.0), O),
                    (f(1.5), F),
                    (i(21), F),
                    (Count::from(1_u8), O),
                ],
            ),
            ("de", vec![(i(1), O), (i(0), F), (f(1.5), F)]),
            (
                "es",
                vec![
                    (i(0), F),
                    (i(1), O),
                    (f(1.0), O),
                    (i(2), F),
                    (i(1_000_000), M),
                    (i(2_000_000), M),
                    (i(1_000_001), F),
                    (f(1.5), F),
                ],
            ),
            (
                "fr",
                vec![
                    (i(0), O),
                    (i(1), O),
                    (f(1.5), O),
                    (i(2), F),
                    (i(1_000_000), M),
                    (f(2.5), F),
                ],
            ),
            (
                "it",
                vec![(i(0), F), (i(1), O), (f(1.5), F), (i(1_000_000), M)],
            ),
            (
                "pt",
                vec![
                    (i(0), O),
                    (i(1), O),
                    (f(1.5), O),
                    (i(2), F),
                    (i(1_000_000), M),
                ],
            ),
            ("pt-BR", vec![(i(0), O), (i(1), O), (i(2), F)]),
            (
                "pt-PT",
                vec![
                    (i(0), F),
                    (i(1), O),
                    (f(1.5), F),
                    (i(2), F),
                    (i(1_000_000), M),
                ],
            ),
            (
                "ru",
                vec![
                    (i(1), O),
                    (i(21), O),
                    (i(101), O),
                    (i(11), M),
                    (i(111), M),
                    (i(2), Fw),
                    (i(3), Fw),
                    (i(4), Fw),
                    (i(22), Fw),
                    (i(12), M),
                    (i(14), M),
                    (i(0), M),
                    (i(5), M),
                    (i(10), M),
                    (i(25), M),
                    (i(100), M),
                    (f(1.5), F),
                    (i(-21), O),
                ],
            ),
            (
                "uk",
                vec![(i(1), O), (i(2), Fw), (i(5), M), (i(11), M), (f(0.5), F)],
            ),
            (
                "pl",
                vec![
                    (i(1), O),
                    (i(2), Fw),
                    (i(3), Fw),
                    (i(4), Fw),
                    (i(22), Fw),
                    (i(12), M),
                    (i(13), M),
                    (i(14), M),
                    (i(0), M),
                    (i(5), M),
                    (i(21), M),
                    (i(11), M),
                    (i(112), M),
                    (i(102), Fw),
                    (f(1.5), F),
                ],
            ),
            (
                "cs",
                vec![
                    (i(1), O),
                    (i(2), Fw),
                    (i(4), Fw),
                    (i(5), F),
                    (i(0), F),
                    (f(1.5), M),
                ],
            ),
            (
                "ar",
                vec![
                    (i(0), Z),
                    (i(1), O),
                    (i(2), Tw),
                    (i(3), Fw),
                    (i(10), Fw),
                    (i(103), Fw),
                    (i(110), Fw),
                    (i(11), M),
                    (i(99), M),
                    (i(111), M),
                    (i(100), F),
                    (i(101), F),
                    (i(102), F),
                    (f(1.5), F),
                ],
            ),
            (
                "he",
                vec![(i(1), O), (i(2), Tw), (i(3), F), (f(0.5), O), (i(0), F)],
            ),
            ("tr", vec![(i(1), O), (f(1.0), O), (i(2), F), (i(0), F)]),
            ("ja", vec![(i(0), F), (i(1), F), (i(2), F)]),
            ("zh", vec![(i(1), F)]),
            ("zh-Hant", vec![(i(1), F)]),
            ("ko", vec![(i(1), F)]),
            // Unknown languages: always other.
            ("xx", vec![(i(0), F), (i(1), F)]),
        ];
        for (locale, samples) in tables {
            let rule = plural_rule_for(locale);
            for (count, want) in samples {
                assert_eq!(rule(count.operands()), want, "{locale}: {count:?}");
            }
        }
    }

    #[test]
    fn categories_translators_must_provide() {
        use Category::{Few, Many, One, Other, Two, Zero};
        for (locale, want) in [
            ("en", vec![One, Other]),
            ("pt", vec![One, Other]),
            // Whole millions fall back to other.
            ("es", vec![One, Other]),
            ("ru", vec![One, Few, Many]),
            ("pl", vec![One, Few, Many]),
            ("ar", vec![Zero, One, Two, Few, Many, Other]),
            ("ja", vec![Other]),
        ] {
            assert_eq!(used_categories(plural_rule_for(locale)), want, "{locale}");
        }
    }

    #[test]
    fn operands_saturate_and_ignore_non_finite_values() {
        let huge = Operands::from_integer(i128::MAX);
        assert_eq!(huge.i, i64::MAX);
        let nan = Operands::from_float(f64::NAN);
        assert_eq!((nan.i, nan.v), (0, 1));
        let float = Operands::from_float(-12.25);
        assert_eq!((float.i, float.v), (12, 2));
        assert_eq!(Operands::from_float(1e300).i, i64::MAX);
    }

    #[test]
    fn categories_round_trip_their_keys() {
        for category in Category::ALL {
            assert_eq!(Category::from_key(category.as_str()), Some(category));
        }
        assert_eq!(Category::from_key("several"), None);
    }
}
