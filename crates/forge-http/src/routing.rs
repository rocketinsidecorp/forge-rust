//! Route patterns and the segment trie behind [`Router`](crate::Router).
//!
//! A pattern is split on `/` into segments that are either static text,
//! compared byte-for-byte with the raw request path, or a `{name}` parameter
//! that captures exactly one non-empty segment. Every route of the same
//! shape (same static text, parameters in the same positions) ends at the
//! same trie node regardless of parameter names; each node holds at most one
//! endpoint per method, which is how ambiguous registrations are detected.

use std::{collections::HashMap, sync::Arc};

use http::Method;

use crate::{Handler, RouteError};

/// One parsed segment of a route pattern.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Segment {
    /// Literal text, matched against the raw (undecoded) request segment.
    Static(String),
    /// A named capture of one non-empty segment.
    Param(Arc<str>),
}

/// The text after the leading `/`, or `None` for `/` itself, which has no
/// segments; `/a/` has the segments `a` and `""`. Request paths and route
/// patterns share this rule, so a trailing slash is significant on both sides.
fn segment_text(rest: &str) -> Option<&str> {
    (!rest.is_empty()).then_some(rest)
}

/// Parses and validates a route pattern.
pub(crate) fn parse_pattern(path: &str) -> Result<Vec<Segment>, RouteError> {
    let invalid = || RouteError::InvalidPath(path.to_owned());
    if path.contains('?') || path.contains('#') {
        return Err(invalid());
    }
    let Some(rest) = path.strip_prefix('/') else {
        return Err(invalid());
    };
    let mut parsed = Vec::new();
    for raw in segment_text(rest)
        .into_iter()
        .flat_map(|text| text.split('/'))
    {
        let segment = match raw
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
        {
            Some(name) if is_param_name(name) => {
                if parsed
                    .iter()
                    .any(|seen| matches!(seen, Segment::Param(seen) if &**seen == name))
                {
                    return Err(RouteError::DuplicateParameter {
                        path: path.to_owned(),
                        name: name.to_owned(),
                    });
                }
                Segment::Param(Arc::from(name))
            }
            // Braces are reserved for whole-segment parameters.
            _ if raw.contains(['{', '}']) => return Err(invalid()),
            _ => Segment::Static(raw.to_owned()),
        };
        parsed.push(segment);
    }
    Ok(parsed)
}

/// `[a-z_][a-z0-9_]*`.
fn is_param_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// A handler registered for one method at one trie node.
pub(crate) struct Endpoint {
    pub(crate) method: Method,
    /// The pattern as registered, for error messages.
    pub(crate) path: Arc<str>,
    /// Parameter names in path order.
    pub(crate) params: Box<[Arc<str>]>,
    pub(crate) handler: Arc<dyn Handler>,
}

/// A node of the segment trie.
#[derive(Default)]
pub(crate) struct Node {
    statics: HashMap<Box<str>, Node>,
    param: Option<Box<Node>>,
    endpoints: Vec<Endpoint>,
}

impl Node {
    /// Adds an endpoint at the node for `segments`, rejecting a second
    /// endpoint for the same method and shape.
    pub(crate) fn insert(
        &mut self,
        segments: Vec<Segment>,
        endpoint: Endpoint,
    ) -> Result<(), RouteError> {
        let mut node = self;
        for segment in segments {
            node = match segment {
                Segment::Static(text) => node.statics.entry(text.into_boxed_str()).or_default(),
                Segment::Param(_) => node.param.get_or_insert_with(Box::default),
            };
        }
        if let Some(existing) = node.endpoint(&endpoint.method) {
            return Err(if existing.path == endpoint.path {
                RouteError::Duplicate {
                    method: endpoint.method,
                    path: endpoint.path.to_string(),
                }
            } else {
                RouteError::Ambiguous {
                    method: endpoint.method,
                    path: endpoint.path.to_string(),
                    existing: existing.path.to_string(),
                }
            });
        }
        node.endpoints.push(endpoint);
        Ok(())
    }

    /// The endpoint registered for `method` at this node.
    pub(crate) fn endpoint(&self, method: &Method) -> Option<&Endpoint> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.method == *method)
    }

    /// The endpoint that serves `method` at this node: the one registered
    /// for it, else, for `HEAD`, the `GET` endpoint (RFC 9110 §9.3.2: a
    /// server that supports `GET` supports `HEAD`; the server sends the
    /// headers without the body). An explicit `HEAD` route wins.
    pub(crate) fn serving(&self, method: &Method) -> Option<&Endpoint> {
        self.endpoint(method).or_else(|| {
            if *method == Method::HEAD {
                self.endpoint(&Method::GET)
            } else {
                None
            }
        })
    }

    /// Methods served at this node, sorted and comma-separated, for the
    /// `Allow` header: the registered ones, plus `HEAD` wherever `GET` is
    /// registered.
    pub(crate) fn allow(&self) -> String {
        let mut methods: Vec<&str> = self
            .endpoints
            .iter()
            .map(|endpoint| endpoint.method.as_str())
            .collect();
        if methods.contains(&Method::GET.as_str()) && !methods.contains(&Method::HEAD.as_str()) {
            methods.push(Method::HEAD.as_str());
        }
        methods.sort_unstable();
        methods.join(", ")
    }

    /// Resolves a raw request path to the node of the route that serves it,
    /// pushing the raw captured segments onto `captures` in path order.
    ///
    /// At each position a static child is tried before the parameter child;
    /// the parameter child is only tried when the static branch yields no
    /// route. Only nodes with at least one endpoint match, so method
    /// selection happens after the path has been resolved. The search only
    /// descends into existing trie nodes, so its depth is bounded by the
    /// longest registered pattern, not by the request.
    pub(crate) fn find<'n, 'p>(
        &'n self,
        path: &'p str,
        captures: &mut Vec<&'p str>,
    ) -> Option<&'n Self> {
        let rest = path.strip_prefix('/')?;
        self.find_in(segment_text(rest), captures)
    }

    fn find_in<'n, 'p>(
        &'n self,
        remaining: Option<&'p str>,
        captures: &mut Vec<&'p str>,
    ) -> Option<&'n Self> {
        let Some(remaining) = remaining else {
            return (!self.endpoints.is_empty()).then_some(self);
        };
        let (segment, tail) = match remaining.split_once('/') {
            Some((segment, tail)) => (segment, Some(tail)),
            None => (remaining, None),
        };
        if let Some(child) = self.statics.get(segment)
            && let Some(found) = child.find_in(tail, captures)
        {
            return Some(found);
        }
        if let Some(child) = &self.param
            && !segment.is_empty()
        {
            captures.push(segment);
            if let Some(found) = child.find_in(tail, captures) {
                return Some(found);
            }
            captures.pop();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(path: &str) -> Result<Vec<Segment>, RouteError> {
        parse_pattern(path)
    }

    #[test]
    fn patterns_split_into_static_and_parameter_segments() {
        assert_eq!(pattern("/"), Ok(Vec::new()));
        assert_eq!(
            pattern("/products/{id}"),
            Ok(vec![
                Segment::Static("products".to_owned()),
                Segment::Param(Arc::from("id")),
            ])
        );
        assert_eq!(
            pattern("/a/"),
            Ok(vec![
                Segment::Static("a".to_owned()),
                Segment::Static(String::new()),
            ])
        );
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        for path in [
            "",
            "products",
            "/a?b",
            "/a#b",
            "/{}",
            "/{Id}",
            "/{1d}",
            "/{id-x}",
            "/{id",
            "/id}",
            "/file.{ext}",
            "/{a}{b}",
            "/{{id}}",
        ] {
            assert_eq!(
                pattern(path),
                Err(RouteError::InvalidPath(path.to_owned())),
                "{path:?}"
            );
        }
    }

    #[test]
    fn parameter_names_must_be_unique() {
        assert_eq!(
            pattern("/a/{id}/b/{id}"),
            Err(RouteError::DuplicateParameter {
                path: "/a/{id}/b/{id}".to_owned(),
                name: "id".to_owned(),
            })
        );
        assert!(pattern("/a/{_}/{a_1}").is_ok());
    }
}
