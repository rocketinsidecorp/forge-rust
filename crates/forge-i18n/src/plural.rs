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
    /// round-trip decimal form (`1.5` has one). NaN and infinities have no
    /// plural form of their own: [`plural_rule_for`] answers
    /// [`Category::Other`] for them in every locale.
    #[must_use]
    pub fn from_float(value: f64) -> Self {
        if !value.is_finite() {
            // No rule on n counts NaN as 0, 1 or 2.
            return Self {
                n: f64::NAN,
                i: 0,
                v: 1,
            };
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

/// `n = 1`: Norwegian, Turkish, Greek and Hungarian ("1.0" is singular
/// too).
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

/// CLDR 46 writes the Arabic rule on n, the whole value, so "0.0", "1.0"
/// and "103.0" count as the integers they equal: zero n = 0, one n = 1, two
/// n = 2, few n % 100 = 3..10, many n % 100 = 11..99. A range matches whole
/// numbers only, so 3.5 is `other`.
fn arabic(o: Operands) -> Category {
    #[allow(clippy::float_cmp)] // a whole number is its own truncation; NaN is not
    if o.n != o.n.trunc() {
        return Category::Other;
    }
    // `i` holds the integer digits, so n % 100 stays exact past 2^53,
    // where the float n is not.
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
const BUILTIN: &[(&str, PluralRule)] = &[
    ("en", one_if_integer_one),
    ("de", one_if_integer_one),
    ("nl", one_if_integer_one),
    ("sv", one_if_integer_one),
    ("fi", one_if_integer_one),
    ("et", one_if_integer_one),
    // n = 1: "1.0" is singular.
    ("nb", one_if_exactly_one),
    ("tr", one_if_exactly_one),
    ("el", one_if_exactly_one),
    ("hu", one_if_exactly_one),
    ("ja", only_other),
    ("zh", only_other),
    ("ko", only_other),
    ("vi", only_other),
    ("th", only_other),
    ("id", only_other),
    ("ms", only_other),
    ("es", spanish),
    ("it", italian),
    // "pt" is Brazilian Portuguese in CLDR.
    ("fr", zero_or_one_singular),
    ("pt", zero_or_one_singular),
    // European Portuguese: only 1 is singular.
    ("pt-PT", italian),
    ("ru", slavic),
    ("uk", slavic),
    ("pl", polish),
    ("cs", czech),
    ("sk", czech),
    ("ar", arabic),
    ("he", hebrew),
];

fn builtin(locale: &str) -> Option<PluralRule> {
    BUILTIN
        .iter()
        .find_map(|(name, rule)| (*name == locale).then_some(*rule))
}

/// The built-in rule of a canonical locale, trying its parents (`pt-BR`
/// uses `pt`, `pt-PT` has its own). Locales without one use
/// [`Category::Other`] for every count. NaN and infinite counts are
/// [`Category::Other`] in every locale.
#[must_use = "the rule is only useful when it is called with operands"]
pub fn plural_rule_for(locale: &str) -> impl Fn(Operands) -> Category + Copy + use<> {
    rule_in(&[], locale)
}

/// The rule of `locale`, a custom one first, behind the guard that makes
/// NaN and infinity `other` whatever the rule says: rules keyed on the
/// integer digits (French, Portuguese, Hebrew) would otherwise call NaN
/// singular, and a custom rule gets no say either.
pub(crate) fn rule_in(
    custom: &[(String, PluralRule)],
    locale: &str,
) -> impl Fn(Operands) -> Category + Copy + use<> {
    let rule = parents(locale)
        .find_map(|candidate| {
            custom
                .iter()
                .find_map(|(name, rule)| (name == candidate).then_some(*rule))
                .or_else(|| builtin(candidate))
        })
        .unwrap_or(only_other);
    move |operands: Operands| {
        if operands.n.is_finite() {
            rule(operands)
        } else {
            Category::Other
        }
    }
}

/// The categories a rule gives the integers 0 to 1000: the forms a
/// translator must provide. The `many` of whole millions (French, Spanish,
/// Italian, Portuguese) is left out: it falls back to `other`.
pub(crate) fn used_categories(rule: impl Fn(Operands) -> Category) -> Vec<Category> {
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
                    // Past 2^53, where a float n % 100 is not exact.
                    (i(100_000_000_000_000_003), Fw),
                ],
            ),
            (
                "nb",
                vec![(i(1), O), (f(1.0), O), (i(0), F), (i(2), F), (f(1.5), F)],
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

    /// The CLDR 46 samples with visible fraction digits ("1.0", "0.00"),
    /// which a float cannot carry, so the operands are written out (Forge-Go's
    /// `TestPluralRulesDecimalSamples`).
    #[test]
    fn rules_match_cldr_decimal_samples() {
        use Category::{Few as Fw, Many as M, One as O, Other as F, Two as Tw, Zero as Z};
        #[allow(clippy::cast_possible_truncation)] // the samples are small, non-negative
        let dec = |n: f64, v: u32| Operands { n, i: n as i64, v };
        type Sample = (&'static str, Operands, Category);
        let tables: Vec<(&str, Vec<Sample>)> = vec![
            // ar: zero n = 0; one n = 1; two n = 2; few n % 100 = 3..10;
            // many n % 100 = 11..99.
            (
                "ar",
                vec![
                    ("0", dec(0.0, 0), Z),
                    ("0.0", dec(0.0, 1), Z),
                    ("0.00", dec(0.0, 2), Z),
                    ("1", dec(1.0, 0), O),
                    ("1.0", dec(1.0, 1), O),
                    ("1.00", dec(1.0, 2), O),
                    ("2", dec(2.0, 0), Tw),
                    ("2.0", dec(2.0, 1), Tw),
                    ("2.00", dec(2.0, 2), Tw),
                    ("3", dec(3.0, 0), Fw),
                    ("3.0", dec(3.0, 1), Fw),
                    ("10.0", dec(10.0, 1), Fw),
                    ("103.0", dec(103.0, 1), Fw),
                    ("110.00", dec(110.0, 2), Fw),
                    ("1003.0", dec(1003.0, 1), Fw),
                    ("11", dec(11.0, 0), M),
                    ("11.0", dec(11.0, 1), M),
                    ("26.00", dec(26.0, 2), M),
                    ("99.0", dec(99.0, 1), M),
                    ("111.0", dec(111.0, 1), M),
                    ("1011.0", dec(1011.0, 1), M),
                    ("100", dec(100.0, 0), F),
                    ("100.0", dec(100.0, 1), F),
                    ("101.0", dec(101.0, 1), F),
                    ("102.00", dec(102.0, 2), F),
                    ("200.0", dec(200.0, 1), F),
                    ("1000.0", dec(1000.0, 1), F),
                    ("0.1", dec(0.1, 1), F),
                    ("0.9", dec(0.9, 1), F),
                    ("1.1", dec(1.1, 1), F),
                    ("1.7", dec(1.7, 1), F),
                    ("2.5", dec(2.5, 1), F),
                    ("3.5", dec(3.5, 1), F),
                    ("10.1", dec(10.1, 1), F),
                    ("11.5", dec(11.5, 1), F),
                    ("100.1", dec(100.1, 1), F),
                    ("103.5", dec(103.5, 1), F),
                    ("1000.1", dec(1000.1, 1), F),
                ],
            ),
            // n = 1 languages: "1.0" is singular, unlike "i = 1 and v = 0".
            (
                "nb",
                vec![
                    ("1.0", dec(1.0, 1), O),
                    ("1.00", dec(1.0, 2), O),
                    ("0.0", dec(0.0, 1), F),
                    ("1.5", dec(1.5, 1), F),
                ],
            ),
            ("tr", vec![("1.0", dec(1.0, 1), O), ("1.5", dec(1.5, 1), F)]),
            ("el", vec![("1.0", dec(1.0, 1), O), ("2.0", dec(2.0, 1), F)]),
            ("hu", vec![("1.0", dec(1.0, 1), O), ("0.5", dec(0.5, 1), F)]),
            (
                "es",
                vec![
                    ("1.0", dec(1.0, 1), O),
                    ("1.5", dec(1.5, 1), F),
                    ("1000000.0", dec(1e6, 1), F),
                ],
            ),
            // i = 1 and v = 0 languages: "1.0" is plural.
            (
                "en",
                vec![
                    ("1.0", dec(1.0, 1), F),
                    ("1.00", dec(1.0, 2), F),
                    ("0.0", dec(0.0, 1), F),
                ],
            ),
            ("de", vec![("1.0", dec(1.0, 1), F)]),
            ("nl", vec![("1.0", dec(1.0, 1), F)]),
            ("sv", vec![("1.0", dec(1.0, 1), F)]),
            ("fi", vec![("1.0", dec(1.0, 1), F)]),
            ("et", vec![("1.0", dec(1.0, 1), F)]),
            (
                "it",
                vec![("1.0", dec(1.0, 1), F), ("1000000.0", dec(1e6, 1), F)],
            ),
            ("pt-PT", vec![("1.0", dec(1.0, 1), F)]),
            // i = 0..1 languages.
            (
                "fr",
                vec![
                    ("0.0", dec(0.0, 1), O),
                    ("1.0", dec(1.0, 1), O),
                    ("1.5", dec(1.5, 1), O),
                    ("2.0", dec(2.0, 1), F),
                ],
            ),
            (
                "pt",
                vec![
                    ("0.0", dec(0.0, 1), O),
                    ("1.0", dec(1.0, 1), O),
                    ("2.0", dec(2.0, 1), F),
                ],
            ),
            // v = 0 only: decimals are "other".
            (
                "ru",
                vec![
                    ("1.0", dec(1.0, 1), F),
                    ("2.0", dec(2.0, 1), F),
                    ("5.0", dec(5.0, 1), F),
                ],
            ),
            ("uk", vec![("1.0", dec(1.0, 1), F)]),
            ("pl", vec![("1.0", dec(1.0, 1), F), ("2.0", dec(2.0, 1), F)]),
            (
                "cs",
                vec![
                    ("1.0", dec(1.0, 1), M),
                    ("2.0", dec(2.0, 1), M),
                    ("0.5", dec(0.5, 1), M),
                ],
            ),
            ("sk", vec![("1.0", dec(1.0, 1), M)]),
            // he: i = 1 and v = 0 or i = 0 and v != 0; i = 2 and v = 0.
            (
                "he",
                vec![
                    ("1.0", dec(1.0, 1), F),
                    ("2.0", dec(2.0, 1), F),
                    ("0.0", dec(0.0, 1), O),
                    ("0.5", dec(0.5, 1), O),
                ],
            ),
            ("ja", vec![("1.0", dec(1.0, 1), F)]),
        ];
        for (locale, samples) in tables {
            let rule = plural_rule_for(locale);
            for (text, operands, want) in samples {
                assert_eq!(rule(operands), want, "{locale}: {text}");
            }
        }
    }

    /// NaN and infinity are never zero, one or two in any locale: for the
    /// built-in rules (even those keyed on the integer digits), for
    /// locales without one and for custom rules.
    #[test]
    fn non_finite_counts_are_other_in_every_locale() {
        let always_one: PluralRule = |_| Category::One;
        let custom = [("cy".to_owned(), always_one)];
        let non_finite = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        let locales = BUILTIN
            .iter()
            .map(|(locale, _)| *locale)
            .chain(["xx", "pt-BR", "cy"]);
        let check = |locale: &str, rule: &dyn Fn(Operands) -> Category| {
            for count in non_finite {
                assert_eq!(
                    rule(Operands::from_float(count)),
                    Category::Other,
                    "{locale}: {count}"
                );
                assert_eq!(
                    rule(Count::from(count).operands()),
                    Category::Other,
                    "{locale}: Count {count}"
                );
            }
            // Operands built by hand are covered too.
            let infinite = Operands {
                n: f64::INFINITY,
                i: 0,
                v: 0,
            };
            assert_eq!(rule(infinite), Category::Other, "{locale}: hand-built");
        };
        for locale in locales {
            check(locale, &plural_rule_for(locale));
            check(locale, &rule_in(&custom, locale));
        }
        // The custom rule still decides finite counts.
        assert_eq!(
            rule_in(&custom, "cy")(Operands::from_integer(1)),
            Category::One
        );
    }

    #[test]
    fn every_built_in_locale_has_a_canonical_unique_name() {
        let mut seen = std::collections::BTreeSet::new();
        for (locale, _) in BUILTIN {
            assert_eq!(
                crate::locale::canonical_locale(locale).as_deref(),
                Ok(*locale)
            );
            assert!(seen.insert(*locale), "{locale} is listed twice");
            assert!(builtin(locale).is_some(), "{locale}");
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
        assert!(nan.n.is_nan());
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
