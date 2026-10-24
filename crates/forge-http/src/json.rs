//! Typed JSON request bodies: the media type and, for described types, the
//! members the document lists.

use forge_openapi::{OpenApiSchema, unknown_member};
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::{HeaderMap, HeaderValue, Request, header};

/// `application/json`, the only media type typed JSON handlers accept.
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// Why a JSON request body was refused.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JsonBodyError {
    /// The request does not carry exactly one `Content-Type:
    /// application/json` (an optional `charset` must be UTF-8).
    #[error("request body must be application/json")]
    UnsupportedMediaType,
    /// The body is not valid JSON for the target type.
    #[error("request body is not valid JSON for this endpoint")]
    Malformed(#[source] serde_json::Error),
    /// The body has a member the target type's description does not list
    /// (`additionalProperties: false`).
    #[error("request body has a member this endpoint does not list")]
    UnknownMember {
        /// Where it is, such as `address.street` or `items[2].sku`. It is
        /// the client's own text: truncate it before echoing it.
        member: String,
    },
}

/// Whether `headers` declare exactly one `application/json` body, with at
/// most a `charset` parameter whose value is UTF-8.
pub(crate) fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => is_json_media_type(value),
        _ => false,
    }
}

fn is_json_media_type(value: &HeaderValue) -> bool {
    let Ok(text) = value.to_str() else {
        return false;
    };
    let mut parts = text.split(';');
    let media_type = parts.next().unwrap_or_default().trim();
    if !media_type.eq_ignore_ascii_case(JSON_CONTENT_TYPE) {
        return false;
    }
    parts.all(|parameter| match parameter.split_once('=') {
        Some((name, value)) => {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .unwrap_or(value);
            name.trim().eq_ignore_ascii_case("charset") && value.eq_ignore_ascii_case("utf-8")
        }
        None => false,
    })
}

impl Request {
    /// Whether the request declares exactly one JSON body: `Content-Type:
    /// application/json` (case-insensitive), with at most a `charset`
    /// parameter whose value is UTF-8. [`Request::json`] does not look at
    /// it.
    #[must_use]
    pub fn is_json(&self) -> bool {
        is_json(self.headers())
    }

    /// Decodes a JSON request body into `T`, refusing a request that does
    /// not declare `application/json` (see [`Request::is_json`]). Unknown
    /// members are ignored unless `T` says `#[serde(deny_unknown_fields)]`;
    /// [`Request::json_body_strict`] refuses them too.
    pub fn json_body<T: DeserializeOwned>(&self) -> Result<T, JsonBodyError> {
        if !self.is_json() {
            return Err(JsonBodyError::UnsupportedMediaType);
        }
        serde_json::from_slice(&self.body).map_err(JsonBodyError::Malformed)
    }

    /// [`Request::json_body`] for a type with an OpenAPI description: a
    /// member the description does not list is refused with
    /// [`JsonBodyError::UnknownMember`], so decoding accepts what the
    /// document publishes (`additionalProperties: false`) and no more.
    /// A `#[serde(alias)]` is not a listed member.
    pub fn json_body_strict<T: DeserializeOwned + OpenApiSchema + 'static>(
        &self,
    ) -> Result<T, JsonBodyError> {
        if !self.is_json() {
            return Err(JsonBodyError::UnsupportedMediaType);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&self.body).map_err(JsonBodyError::Malformed)?;
        if let Some(member) = unknown_member::<T>(&value) {
            return Err(JsonBodyError::UnknownMember { member });
        }
        serde_json::from_value(value).map_err(JsonBodyError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde::Deserialize;
    use uuid::Uuid;

    use super::*;
    use crate::{Method, Uri};

    #[allow(dead_code)]
    #[derive(Debug, Deserialize, forge_openapi::OpenApiSchema)]
    #[openapi(crate = "forge_openapi")]
    struct Address {
        city: String,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize, forge_openapi::OpenApiSchema)]
    #[openapi(crate = "forge_openapi")]
    struct Input {
        name: String,
        address: Address,
        tags: Vec<Address>,
        #[serde(default)]
        note: Option<String>,
        #[serde(default, alias = "old")]
        other: Option<String>,
    }

    fn request(content_type: Option<&str>, body: &str) -> Request {
        request_with(content_type.as_slice(), body)
    }

    fn request_with(content_types: &[&str], body: &str) -> Request {
        let mut headers = HeaderMap::new();
        for value in content_types {
            headers.append(
                header::CONTENT_TYPE,
                HeaderValue::from_str(value).expect("header"),
            );
        }
        Request::new_for_test(
            Method::POST,
            Uri::from_static("/x"),
            headers,
            Bytes::copy_from_slice(body.as_bytes()),
            "127.0.0.1:4000".parse().expect("address"),
            Uuid::now_v7(),
        )
    }

    const JSON: Option<&str> = Some("application/json");
    const OK: &str = r#"{"name":"a","address":{"city":"x"},"tags":[{"city":"y"}]}"#;

    #[test]
    fn the_media_type_is_exactly_application_json() {
        for accepted in [
            "application/json",
            "Application/JSON",
            "application/json; charset=utf-8",
            "application/json;charset=\"UTF-8\"",
        ] {
            assert!(request(Some(accepted), OK).is_json(), "{accepted}");
        }
        for refused in [
            "text/plain",
            "application/x-www-form-urlencoded",
            "application/jsonx",
            "application/problem+json",
            "application/json; charset=latin1",
            "application/json; boundary=x",
            "",
        ] {
            let request = request(Some(refused), OK);
            assert!(!request.is_json(), "{refused}");
            assert!(matches!(
                request.json_body::<Input>(),
                Err(JsonBodyError::UnsupportedMediaType)
            ));
        }
        let missing = request(None, OK);
        assert!(!missing.is_json());
        assert!(matches!(
            missing.json_body_strict::<Input>(),
            Err(JsonBodyError::UnsupportedMediaType)
        ));
        // Two Content-Type headers are ambiguous.
        assert!(!request_with(&["application/json", "application/json"], OK).is_json());
    }

    #[test]
    fn lenient_decoding_ignores_unknown_members_strict_decoding_refuses_them() {
        let body = r#"{"name":"a","extra":1,"address":{"city":"x"},"tags":[]}"#;
        assert!(request(JSON, body).json_body::<Input>().is_ok());
        match request(JSON, body).json_body_strict::<Input>() {
            Err(JsonBodyError::UnknownMember { member }) => assert_eq!(member, "extra"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_members_are_found_at_any_depth() {
        let nested = r#"{"name":"a","address":{"city":"x","zip":"1"},"tags":[]}"#;
        let in_list =
            r#"{"name":"a","address":{"city":"x"},"tags":[{"city":"y"},{"city":"z","k":0}]}"#;
        for (body, want) in [(nested, "address.zip"), (in_list, "tags[1].k")] {
            match request(JSON, body).json_body_strict::<Input>() {
                Err(JsonBodyError::UnknownMember { member }) => assert_eq!(member, want),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn valid_bodies_pass_and_other_failures_stay_malformed() {
        let input = request(JSON, OK)
            .json_body_strict::<Input>()
            .expect("valid");
        assert_eq!(input.name, "a");
        let with_null = r#"{"name":"a","address":{"city":"x"},"tags":[],"note":null}"#;
        assert!(request(JSON, with_null).json_body_strict::<Input>().is_ok());
        for body in [
            "",
            "{",
            r#"{"name":1}"#,
            r#"{"address":{"city":"x"}}"#,
            "[]",
        ] {
            assert!(
                matches!(
                    request(JSON, body).json_body_strict::<Input>(),
                    Err(JsonBodyError::Malformed(_))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn an_alias_is_not_a_listed_member() {
        let body = r#"{"name":"a","address":{"city":"x"},"tags":[],"old":"v"}"#;
        assert!(request(JSON, body).json_body::<Input>().is_ok());
        assert!(matches!(
            request(JSON, body).json_body_strict::<Input>(),
            Err(JsonBodyError::UnknownMember { .. })
        ));
    }
}
