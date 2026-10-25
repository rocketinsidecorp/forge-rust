//! Strict `application/x-www-form-urlencoded` decoding for query strings
//! and, through [`Mode::Form`], request bodies (see `form.rs`).
//!
//! The query is split on `&` into `name=value` pairs; `+` decodes to a space
//! and `%XX` escapes must form UTF-8. Anything ambiguous is rejected rather
//! than guessed: empty segments (`a=1&&b=2`, a trailing `&`), pairs without
//! `=`, empty names and repeated names. `name=` is an empty value.
//!
//! Values are then deserialized from their text: integers, floats, `bool`
//! (`true`/`false`), `char`, strings, unit enum variants, newtypes and any
//! type deserialized from a string (such as UUIDs) work; a present parameter
//! is `Some` for `Option` fields and an absent one `None`. Sequences and
//! nested structures are not supported, nor are `#[serde(flatten)]` and
//! untagged enums (they buffer values as strings, so numbers inside them do
//! not parse). Unknown parameters are ignored unless the target type uses
//! `#[serde(deny_unknown_fields)]`; deserializing into `()` accepts only an
//! empty query.
//!
//! Form bodies use the same grammar with HTML form semantics on top: a name
//! may repeat only when its field is a sequence (`Vec<T>`), `bool` also
//! accepts the checkbox values `on` and `1` (and `0`), and an empty value is
//! `None` for an `Option` field.

use std::{borrow::Cow, collections::BTreeMap, fmt};

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess,
    Visitor, value::StrDeserializer,
};
use thiserror::Error;

use crate::encoding::percent_decode;

/// Longest parameter name echoed in a [`QueryError`], in characters.
const MAX_ECHOED_PARAMETER_CHARS: usize = 64;

/// What was wrong with a query string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum QueryErrorKind {
    /// The query is not strict `application/x-www-form-urlencoded`.
    Malformed,
    /// A parameter appears more than once.
    DuplicateParameter,
    /// A parameter the target type does not accept.
    UnknownParameter,
    /// A required parameter is absent.
    MissingParameter,
    /// A value does not parse as its field's type, or the query does not fit
    /// the target type at all.
    InvalidValue,
}

impl QueryErrorKind {
    /// A caller-safe description that never includes request input.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Malformed => "query string is not valid application/x-www-form-urlencoded",
            Self::DuplicateParameter => "query parameter must not be repeated",
            Self::UnknownParameter => "unknown query parameter",
            Self::MissingParameter => "missing required query parameter",
            Self::InvalidValue => "query parameter has an invalid value",
        }
    }
}

/// A query string that could not be decoded into the requested type.
///
/// [`QueryError::parameter`] is safe to echo to the caller; the `Display`
/// output may include decoder detail and is meant for logs.
#[derive(Debug, Error)]
#[error("{}{}", kind.message(), detail.as_deref().map(|detail| format!(": {detail}")).unwrap_or_default())]
pub struct QueryError {
    kind: QueryErrorKind,
    parameter: Option<String>,
    detail: Option<String>,
}

impl QueryError {
    pub(crate) fn new(kind: QueryErrorKind, parameter: Option<&str>) -> Self {
        Self {
            kind,
            parameter: parameter.map(echo),
            detail: None,
        }
    }

    /// What was wrong.
    #[must_use]
    pub fn kind(&self) -> QueryErrorKind {
        self.kind
    }

    /// The offending parameter name, truncated to 64 characters, when the
    /// failure concerns one parameter.
    #[must_use]
    pub fn parameter(&self) -> Option<&str> {
        self.parameter.as_deref()
    }

    /// Splits the error for conversion into another boundary error.
    pub(crate) fn into_parts(self) -> (QueryErrorKind, Option<String>, Option<String>) {
        (self.kind, self.parameter, self.detail)
    }
}

/// Bounds attacker-controlled names before they are echoed.
fn echo(parameter: &str) -> String {
    parameter.chars().take(MAX_ECHOED_PARAMETER_CHARS).collect()
}

/// Which `application/x-www-form-urlencoded` dialect is decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    /// Query strings: every name at most once, `bool` is `true`/`false`.
    Query,
    /// HTML form bodies: repeated names fill sequences, checkbox booleans,
    /// empty optional values are `None`.
    Form,
}

/// Decoded `name → values` pairs, in name order.
pub(crate) type Pairs<'q> = BTreeMap<Cow<'q, str>, Vec<Cow<'q, str>>>;

/// Decodes a raw query string (the part after `?`, if any) into `T`.
pub(crate) fn decode<T: DeserializeOwned>(query: Option<&str>) -> Result<T, QueryError> {
    let pairs = parse(query.unwrap_or_default(), Mode::Query, &[])?;
    deserialize(&pairs, Mode::Query)
}

/// Deserializes already parsed pairs into `T`.
pub(crate) fn deserialize<T: DeserializeOwned>(
    pairs: &Pairs<'_>,
    mode: Mode,
) -> Result<T, QueryError> {
    T::deserialize(QueryDeserializer { pairs, mode }).map_err(DeError::into_query_error)
}

/// Splits and decodes `input` into `name → values` pairs.
///
/// In [`Mode::Query`] a repeated name is an error here; in [`Mode::Form`]
/// the values are kept in order and the target type decides. Names listed in
/// `skip` (compared after decoding) are dropped.
pub(crate) fn parse<'q>(
    input: &'q str,
    mode: Mode,
    skip: &[&str],
) -> Result<Pairs<'q>, QueryError> {
    let mut pairs = Pairs::new();
    if input.is_empty() {
        return Ok(pairs);
    }
    for segment in input.split('&') {
        let Some((raw_name, raw_value)) = segment.split_once('=') else {
            let parameter = (!segment.is_empty()).then_some(segment);
            return Err(QueryError::new(QueryErrorKind::Malformed, parameter));
        };
        if raw_name.is_empty() {
            return Err(QueryError::new(QueryErrorKind::Malformed, None));
        }
        let name = percent_decode(raw_name, true)
            .map_err(|_| QueryError::new(QueryErrorKind::Malformed, Some(raw_name)))?;
        let value = percent_decode(raw_value, true)
            .map_err(|_| QueryError::new(QueryErrorKind::Malformed, Some(&name)))?;
        if skip.contains(&name.as_ref()) {
            continue;
        }
        if mode == Mode::Query && pairs.contains_key(&name) {
            return Err(QueryError::new(
                QueryErrorKind::DuplicateParameter,
                Some(&name),
            ));
        }
        pairs.entry(name).or_default().push(value);
    }
    Ok(pairs)
}

/// Serde error carrying the parameter it concerns.
#[derive(Debug)]
struct DeError {
    kind: QueryErrorKind,
    parameter: Option<String>,
    detail: String,
}

impl DeError {
    fn new(kind: QueryErrorKind, parameter: Option<&str>, detail: impl fmt::Display) -> Self {
        Self {
            kind,
            parameter: parameter.map(echo),
            detail: detail.to_string(),
        }
    }

    /// Attributes the failure to `parameter` unless it already names one.
    fn at(mut self, parameter: &str) -> Self {
        if self.parameter.is_none() {
            self.parameter = Some(echo(parameter));
        }
        self
    }

    fn into_query_error(self) -> QueryError {
        QueryError {
            kind: self.kind,
            parameter: self.parameter,
            detail: Some(self.detail),
        }
    }
}

impl fmt::Display for DeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(QueryErrorKind::InvalidValue, None, message)
    }

    fn unknown_field(field: &str, _expected: &'static [&'static str]) -> Self {
        Self::new(
            QueryErrorKind::UnknownParameter,
            Some(field),
            "unknown field",
        )
    }

    fn missing_field(field: &'static str) -> Self {
        Self::new(
            QueryErrorKind::MissingParameter,
            Some(field),
            "missing field",
        )
    }

    fn duplicate_field(field: &'static str) -> Self {
        // Reachable through `#[serde(alias)]`: two spellings of one field.
        Self::new(
            QueryErrorKind::DuplicateParameter,
            Some(field),
            "duplicate field",
        )
    }
}

/// Deserializes the whole input as a map of parameters.
struct QueryDeserializer<'m, 'q> {
    pairs: &'m Pairs<'q>,
    mode: Mode,
}

impl<'de> Deserializer<'de> for QueryDeserializer<'_, '_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_map(Parameters {
            pairs: self.pairs.iter(),
            mode: self.mode,
            value: None,
        })
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.pairs.keys().next() {
            None => visitor.visit_unit(),
            Some(name) => Err(DeError::new(
                QueryErrorKind::UnknownParameter,
                Some(name),
                "no query parameters are accepted",
            )),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option seq tuple tuple_struct map struct enum identifier
        ignored_any
    }
}

/// Yields each parameter name, then its values.
struct Parameters<'m, 'q> {
    pairs: std::collections::btree_map::Iter<'m, Cow<'q, str>, Vec<Cow<'q, str>>>,
    mode: Mode,
    value: Option<(&'m str, &'m [Cow<'q, str>])>,
}

impl<'de> MapAccess<'de> for Parameters<'_, '_> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        let Some((name, values)) = self.pairs.next() else {
            return Ok(None);
        };
        self.value = Some((name, values));
        let key: StrDeserializer<'_, DeError> = name.as_ref().into_deserializer();
        seed.deserialize(key)
            .map(Some)
            .map_err(|error| error.at(name))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let Some((name, values)) = self.value.take() else {
            return Err(de::Error::custom("value requested before its name"));
        };
        seed.deserialize(ValueDeserializer {
            values,
            mode: self.mode,
        })
        .map_err(|error| error.at(name))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.pairs.len())
    }
}

/// Deserializes the decoded value(s) of one name, parsing them as whatever
/// type is requested. Every type except a sequence needs exactly one value.
struct ValueDeserializer<'m, 'q> {
    values: &'m [Cow<'q, str>],
    mode: Mode,
}

impl ValueDeserializer<'_, '_> {
    /// The only value; a repeated name is an error for non-sequence fields.
    fn single(&self) -> Result<&str, DeError> {
        match self.values {
            [value] => Ok(value),
            [] => Err(de::Error::custom("parameter has no value")),
            _ => Err(DeError::new(
                QueryErrorKind::DuplicateParameter,
                None,
                "parameter must not be repeated",
            )),
        }
    }

    fn parse<T>(&self) -> Result<T, DeError>
    where
        T: std::str::FromStr,
        T::Err: fmt::Display,
    {
        self.single()?.parse().map_err(de::Error::custom)
    }
}

macro_rules! parse_value {
    ($($method:ident => $visit:ident,)*) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
                visitor.$visit(self.parse()?)
            }
        )*
    };
}

impl<'de> Deserializer<'de> for ValueDeserializer<'_, '_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_str(self.single()?)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match (self.single()?, self.mode) {
            ("true", _) | ("on" | "1", Mode::Form) => visitor.visit_bool(true),
            ("false", _) | ("0", Mode::Form) => visitor.visit_bool(false),
            (_, Mode::Query) => Err(de::Error::custom("expected `true` or `false`")),
            (_, Mode::Form) => Err(de::Error::custom(
                "expected `true`, `on` or `1`, or `false` or `0`",
            )),
        }
    }

    parse_value! {
        deserialize_i8 => visit_i8,
        deserialize_i16 => visit_i16,
        deserialize_i32 => visit_i32,
        deserialize_i64 => visit_i64,
        deserialize_i128 => visit_i128,
        deserialize_u8 => visit_u8,
        deserialize_u16 => visit_u16,
        deserialize_u32 => visit_u32,
        deserialize_u64 => visit_u64,
        deserialize_u128 => visit_u128,
        deserialize_f32 => visit_f32,
        deserialize_f64 => visit_f64,
        deserialize_char => visit_char,
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        // A single blank form input (`age=`) is an absent optional value; a
        // repeated field is an optional sequence.
        if self.mode == Mode::Form && matches!(self.values, [value] if value.is_empty()) {
            return visitor.visit_none();
        }
        visitor.visit_some(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.mode {
            Mode::Form => visitor.visit_seq(Values {
                values: self.values.iter(),
                mode: self.mode,
            }),
            // Query strings have no sequences; report the type mismatch.
            Mode::Query => visitor.visit_str(self.single()?),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.single()?.is_empty() {
            visitor.visit_unit()
        } else {
            Err(de::Error::custom("expected an empty value"))
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let variant: StrDeserializer<'_, DeError> = self.single()?.into_deserializer();
        visitor.visit_enum(variant)
    }

    /// A field the target type does not declare is skipped however often it
    /// repeats; only `deny_unknown_fields` refuses it.
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        str string bytes byte_buf tuple tuple_struct map struct identifier
    }
}

/// The values of a repeated form field, one element each.
struct Values<'m, 'q> {
    values: std::slice::Iter<'m, Cow<'q, str>>,
    mode: Mode,
}

impl<'de> SeqAccess<'de> for Values<'_, '_> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        let Some(value) = self.values.next() else {
            return Ok(None);
        };
        seed.deserialize(ValueDeserializer {
            values: std::slice::from_ref(value),
            mode: self.mode,
        })
        .map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.values.len())
    }
}

#[cfg(test)]
mod display_tests;
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde::Deserialize;
    use uuid::Uuid;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Paging {
        limit: Option<u16>,
        after: Option<Uuid>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "lowercase")]
    enum Order {
        Asc,
        Desc,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Search {
        q: String,
        #[serde(default)]
        exact: bool,
        order: Option<Order>,
        min: Option<i64>,
        ratio: Option<f64>,
    }

    fn failure<T: DeserializeOwned + fmt::Debug>(query: &str) -> (QueryErrorKind, Option<String>) {
        let error = decode::<T>(Some(query)).expect_err(query);
        (error.kind(), error.parameter().map(str::to_owned))
    }

    fn kind_at(kind: QueryErrorKind, parameter: &str) -> (QueryErrorKind, Option<String>) {
        (kind, Some(parameter.to_owned()))
    }

    #[test]
    fn absent_and_empty_queries_decode_to_defaults() {
        let expected = Paging {
            limit: None,
            after: None,
        };
        assert_eq!(decode::<Paging>(None).expect("absent"), expected);
        assert_eq!(decode::<Paging>(Some("")).expect("empty"), expected);
        decode::<()>(None).expect("unit accepts an absent query");
        decode::<()>(Some("")).expect("unit accepts an empty query");
    }

    #[test]
    fn typed_values_are_parsed_from_text() {
        let id = Uuid::now_v7();
        let paging = decode::<Paging>(Some(&format!("after={id}&limit=25"))).expect("valid");
        assert_eq!(
            paging,
            Paging {
                limit: Some(25),
                after: Some(id),
            }
        );

        let search = decode::<Search>(Some("q=red+mug%21&exact=true&order=desc&min=-5&ratio=0.5"))
            .expect("valid");
        assert_eq!(
            search,
            Search {
                q: "red mug!".to_owned(),
                exact: true,
                order: Some(Order::Desc),
                min: Some(-5),
                ratio: Some(0.5),
            }
        );
    }

    #[test]
    fn empty_values_are_empty_strings() {
        let search = decode::<Search>(Some("q=")).expect("empty value");
        assert_eq!(search.q, "");
        assert_eq!(
            failure::<Paging>("limit="),
            kind_at(QueryErrorKind::InvalidValue, "limit")
        );
    }

    #[test]
    fn malformed_queries_are_rejected() {
        for (query, parameter) in [
            ("limit", Some("limit")),
            ("limit=1&", None),
            ("&limit=1", None),
            ("limit=1&&after=2", None),
            ("=1", None),
            ("li%6=1", Some("li%6")),
            ("limit=%zz", Some("limit")),
            ("limit=%FF", Some("limit")),
            ("lim%C3it=1", Some("lim%C3it")),
        ] {
            assert_eq!(
                failure::<Paging>(query),
                (QueryErrorKind::Malformed, parameter.map(str::to_owned)),
                "{query:?}"
            );
        }
    }

    #[test]
    fn duplicates_are_rejected_after_decoding() {
        for query in [
            "limit=1&limit=2",
            "limit=1&li%6Dit=2",
            "limit=1&after=x&limit=1",
        ] {
            assert_eq!(
                failure::<Paging>(query),
                kind_at(QueryErrorKind::DuplicateParameter, "limit"),
                "{query:?}"
            );
        }
    }

    #[test]
    fn unknown_parameters_follow_deny_unknown_fields() {
        assert_eq!(
            failure::<Paging>("page=2"),
            kind_at(QueryErrorKind::UnknownParameter, "page")
        );
        // Without `deny_unknown_fields` extra parameters are ignored.
        let search = decode::<Search>(Some("q=x&page=2")).expect("lenient");
        assert_eq!(search.q, "x");
        assert_eq!(
            failure::<()>("page=2"),
            kind_at(QueryErrorKind::UnknownParameter, "page")
        );
    }

    #[test]
    fn missing_and_invalid_values_name_the_parameter() {
        assert_eq!(
            failure::<Search>("exact=true"),
            kind_at(QueryErrorKind::MissingParameter, "q")
        );
        for (query, parameter) in [
            ("limit=65536", "limit"),
            ("limit=-1", "limit"),
            ("limit=ten", "limit"),
            ("limit=+1", "limit"),
            ("after=not-a-uuid", "after"),
        ] {
            assert_eq!(
                failure::<Paging>(query),
                kind_at(QueryErrorKind::InvalidValue, parameter),
                "{query:?}"
            );
        }
        for (query, parameter) in [
            ("q=x&exact=yes", "exact"),
            ("q=x&order=up", "order"),
            ("q=x&min=99999999999999999999", "min"),
        ] {
            assert_eq!(
                failure::<Search>(query),
                kind_at(QueryErrorKind::InvalidValue, parameter),
                "{query:?}"
            );
        }
    }

    #[test]
    fn echoed_parameter_names_are_bounded() {
        let long = "é".repeat(1_000);
        let (_, parameter) = failure::<Paging>(&format!("{long}=1"));
        assert_eq!(parameter.map(|name| name.chars().count()), Some(64));
    }

    #[test]
    fn non_struct_targets_are_rejected_without_a_parameter() {
        assert_eq!(failure::<u32>("a=1"), (QueryErrorKind::InvalidValue, None));
    }

    #[test]
    fn display_is_for_logs_and_kind_message_is_caller_safe() {
        let error = decode::<Paging>(Some("limit=secret-input")).expect_err("invalid");
        assert!(
            error
                .to_string()
                .starts_with(QueryErrorKind::InvalidValue.message())
        );
        for kind in [
            QueryErrorKind::Malformed,
            QueryErrorKind::DuplicateParameter,
            QueryErrorKind::UnknownParameter,
            QueryErrorKind::MissingParameter,
            QueryErrorKind::InvalidValue,
        ] {
            assert!(!kind.message().is_empty());
        }
    }

    /// Minimal form encoder for round-trip tests.
    fn encode(text: &str) -> String {
        let mut encoded = String::new();
        for byte in text.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                    encoded.push(char::from(byte));
                }
                b' ' => encoded.push('+'),
                other => encoded.push_str(&format!("%{other:02X}")),
            }
        }
        encoded
    }

    #[test]
    fn arbitrary_names_and_values_round_trip() {
        // Deterministic pseudo-random strings drawn from a hostile alphabet.
        let alphabet: Vec<char> = "aZ09 +&=%/?#é€😀\u{0}\n-._~".chars().collect();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..500 {
            let mut expected = HashMap::new();
            let count = usize::try_from(next() % 5).unwrap_or(0);
            for index in 0..count {
                let length = usize::try_from(next() % 8).unwrap_or(0);
                let mut name: String = (0..length)
                    .map(|_| alphabet[usize::try_from(next()).unwrap_or(0) % alphabet.len()])
                    .collect();
                // Names must be non-empty and unique.
                name.push_str(&index.to_string());
                let length = usize::try_from(next() % 8).unwrap_or(0);
                let value: String = (0..length)
                    .map(|_| alphabet[usize::try_from(next()).unwrap_or(0) % alphabet.len()])
                    .collect();
                expected.insert(name, value);
            }
            let query = expected
                .iter()
                .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
                .collect::<Vec<_>>()
                .join("&");

            let decoded = decode::<HashMap<String, String>>(Some(&query))
                .unwrap_or_else(|error| panic!("round {round}: {query:?}: {error}"));

            assert_eq!(decoded, expected, "round {round}: {query:?}");
        }
    }

    #[test]
    fn integers_round_trip_at_their_limits() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Limits {
            a: u8,
            b: i8,
            c: u64,
            d: i64,
            e: u128,
            f: i128,
            g: char,
        }
        let query = format!(
            "a={}&b={}&c={}&d={}&e={}&f={}&g=%C3%A9",
            u8::MAX,
            i8::MIN,
            u64::MAX,
            i64::MIN,
            u128::MAX,
            i128::MIN
        );
        assert_eq!(
            decode::<Limits>(Some(&query)).expect("limits"),
            Limits {
                a: u8::MAX,
                b: i8::MIN,
                c: u64::MAX,
                d: i64::MIN,
                e: u128::MAX,
                f: i128::MIN,
                g: 'é',
            }
        );
    }
}
