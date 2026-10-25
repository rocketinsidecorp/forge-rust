//! Query and form decoding into unit, newtype and aliased targets and the
//! errors only serde's derive can provoke.

use serde::Deserialize;

use crate::query::{Mode, QueryError, QueryErrorKind, decode, deserialize, parse};

#[derive(Debug, Deserialize, PartialEq)]
struct Paging {
    limit: u16,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Filter(Paging);

#[derive(Debug, Deserialize, PartialEq)]
struct Nothing;

#[derive(Debug, Deserialize, PartialEq)]
struct Marker;

#[derive(Debug, Deserialize, PartialEq)]
struct Id(u32);

#[derive(Debug, Deserialize, PartialEq)]
struct Flags {
    empty: (),
    marker: Marker,
    id: Id,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Aliased {
    #[serde(alias = "q")]
    query: String,
}

#[derive(Debug, Deserialize)]
struct Tagged {
    #[allow(dead_code)]
    tags: Vec<String>,
}

fn error_of<T: serde::de::DeserializeOwned + std::fmt::Debug>(query: &str) -> QueryError {
    decode::<T>(Some(query)).expect_err(query)
}

#[test]
fn unit_and_newtype_targets_decode_from_the_whole_query() {
    assert_eq!(decode::<()>(Some("")).expect("empty"), ());
    assert_eq!(decode::<Nothing>(None).expect("absent"), Nothing);
    let error = error_of::<Nothing>("a=1");
    assert_eq!(error.kind(), QueryErrorKind::UnknownParameter);
    assert_eq!(error.parameter(), Some("a"));
    assert_eq!(
        decode::<Filter>(Some("limit=5")).expect("newtype"),
        Filter(Paging { limit: 5 })
    );
}

#[test]
fn unit_and_newtype_values_decode_from_their_text() {
    assert_eq!(
        decode::<Flags>(Some("empty=&marker=&id=7")).expect("flags"),
        Flags {
            empty: (),
            marker: Marker,
            id: Id(7)
        }
    );
    let error = error_of::<Flags>("empty=x&marker=&id=7");
    assert_eq!(error.kind(), QueryErrorKind::InvalidValue);
    assert_eq!(error.parameter(), Some("empty"));
    assert!(
        error.to_string().contains("expected an empty value"),
        "{error}"
    );
    let error = error_of::<Flags>("empty=&marker=y&id=7");
    assert_eq!(error.parameter(), Some("marker"));
    let error = error_of::<Flags>("empty=&marker=&id=seven");
    assert_eq!(error.parameter(), Some("id"));
}

#[test]
fn a_field_spelled_two_ways_is_a_duplicate_parameter() {
    assert_eq!(
        decode::<Aliased>(Some("q=a")).expect("alias"),
        Aliased {
            query: "a".to_owned()
        }
    );
    let error = error_of::<Aliased>("query=a&q=b");
    assert_eq!(error.kind(), QueryErrorKind::DuplicateParameter);
    assert_eq!(error.parameter(), Some("query"));
}

#[test]
fn query_strings_have_no_sequences_but_forms_do() {
    let error = error_of::<Tagged>("tags=a");
    assert_eq!(error.kind(), QueryErrorKind::InvalidValue);
    assert_eq!(error.parameter(), Some("tags"));

    let pairs = parse("tags=a&tags=b", Mode::Form, &[]).expect("form pairs");
    let tagged: Tagged = deserialize(&pairs, Mode::Form).expect("form sequence");
    assert_eq!(tagged.tags, ["a", "b"]);
}
