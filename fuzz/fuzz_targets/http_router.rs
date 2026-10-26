//! Route registration and path matching (`forge_http::Router`), including the
//! `{name...}` catch-all and the refusal of paths that could smuggle a
//! separator or a dot segment past routing.
//!
//! Input: route patterns, one per line, a NUL, then the request path.
//!
//! The pattern grammar, the precedence of static text over `{name}` over
//! `{name...}`, the refusal of invalid paths and the captures are all written
//! out a second time here, independently of the trie, and the two must agree
//! on every input. Whatever a handler is given is safe to join to a
//! directory: a capture never holds a `.` or `..` segment, a backslash or an
//! encoded separator, and a catch-all never starts with `/` or holds `//`.
#![no_main]

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use forge_http::{HeaderMap, Method, Request, Response, RouteError, Router, StatusCode, Uri};
use libfuzzer_sys::fuzz_target;

const MAX_PATTERNS: usize = 6;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Segment {
    Static(String),
    Param(String),
    Rest(String),
}

impl Segment {
    /// Static text beats a parameter, which beats a catch-all.
    fn rank(&self) -> u8 {
        match self {
            Self::Static(_) => 0,
            Self::Param(_) => 1,
            Self::Rest(_) => 2,
        }
    }
}

enum Refusal {
    Invalid,
    DuplicateName,
}

/// `[a-z_][a-z0-9_]*`.
fn param_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// The pattern grammar: an absolute path without `?` or `#`, whole-segment
/// captures only, a catch-all last, no name twice.
fn grammar(pattern: &str) -> Result<Vec<Segment>, Refusal> {
    if pattern.contains(['?', '#']) {
        return Err(Refusal::Invalid);
    }
    let rest = pattern.strip_prefix('/').ok_or(Refusal::Invalid)?;
    let mut segments: Vec<Segment> = Vec::new();
    if rest.is_empty() {
        return Ok(segments);
    }
    for raw in rest.split('/') {
        if matches!(segments.last(), Some(Segment::Rest(_))) {
            return Err(Refusal::Invalid);
        }
        let capture = raw
            .strip_prefix('{')
            .and_then(|inner| inner.strip_suffix('}'));
        let segment = match capture {
            Some(inner) => {
                let (name, catch_all) = match inner.strip_suffix("...") {
                    Some(name) => (name, true),
                    None => (inner, false),
                };
                if !param_name(name) {
                    return Err(Refusal::Invalid);
                }
                let taken = segments
                    .iter()
                    .any(|seen| matches!(seen, Segment::Param(n) | Segment::Rest(n) if n == name));
                if taken {
                    return Err(Refusal::DuplicateName);
                }
                if catch_all {
                    Segment::Rest(name.to_owned())
                } else {
                    Segment::Param(name.to_owned())
                }
            }
            None if raw.contains(['{', '}']) => return Err(Refusal::Invalid),
            None => Segment::Static(raw.to_owned()),
        };
        segments.push(segment);
    }
    Ok(segments)
}

/// Two patterns of one shape (names apart) cannot both be registered.
fn shape(segments: &[Segment]) -> Vec<(u8, &str)> {
    segments
        .iter()
        .map(|segment| match segment {
            Segment::Static(text) => (0, text.as_str()),
            Segment::Param(_) => (1, ""),
            Segment::Rest(_) => (2, ""),
        })
        .collect()
}

/// `%XX` escapes (and nothing else: `+` stays a plus) that must form UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            index += 2;
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8(out).ok()
}

/// A path that could smuggle a separator or a dot segment past routing.
fn invalid_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("%2f")
        || lower.contains("%5c")
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| matches!(percent_decode(segment).as_deref(), Some("." | "..")))
}

/// The raw captures of `pattern` for `path`, if it matches: a catch-all takes
/// the rest of the path (possibly empty after a trailing `/`, and at the root
/// for `/`).
fn matches(pattern: &[Segment], path: &str) -> Option<Vec<String>> {
    let rest = path.strip_prefix('/')?;
    let parts: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('/').collect()
    };
    let mut captures = Vec::new();
    for (index, segment) in pattern.iter().enumerate() {
        match segment {
            Segment::Static(text) => {
                if parts.get(index) != Some(&text.as_str()) {
                    return None;
                }
            }
            Segment::Param(_) => {
                let part = parts.get(index)?;
                if part.is_empty() {
                    return None;
                }
                captures.push((*part).to_owned());
            }
            Segment::Rest(_) => {
                // At least one more segment, except that `/` is the empty
                // rest of a catch-all at the root.
                if index >= parts.len() && !(index == 0 && rest.is_empty()) {
                    return None;
                }
                captures.push(parts.get(index..).unwrap_or_default().join("/"));
                return Some(captures);
            }
        }
    }
    (parts.len() == pattern.len()).then_some(captures)
}

type Seen = Arc<Mutex<Vec<Hit>>>;

#[derive(Debug)]
struct Hit {
    route: Option<String>,
    captures: Vec<(String, Result<String, ()>)>,
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Some((patterns, path)) = text.split_once('\0') else {
        return;
    };
    let Ok(uri) = path.parse::<Uri>() else {
        return;
    };
    let patterns: Vec<&str> = patterns.split('\n').take(MAX_PATTERNS).collect();

    let seen: Seen = Seen::default();
    let mut router = Router::new();
    let mut registered: Vec<(&str, Vec<Segment>)> = Vec::new();
    for pattern in patterns {
        let names: Vec<String> = grammar(pattern)
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|segment| match segment {
                        Segment::Param(name) | Segment::Rest(name) => Some(name.clone()),
                        Segment::Static(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let hits = Arc::clone(&seen);
        let result = router.route(Method::GET, pattern, move |request: Request| {
            let hits = Arc::clone(&hits);
            let names = names.clone();
            async move {
                let captures = names
                    .iter()
                    .map(|name| {
                        (
                            name.clone(),
                            request.path_param_as::<String>(name).map_err(|_| ()),
                        )
                    })
                    .collect();
                hits.lock().expect("hits").push(Hit {
                    route: request.route().map(str::to_owned),
                    captures,
                });
                Response::empty(StatusCode::OK)
            }
        });
        match (grammar(pattern), result) {
            (Ok(segments), Ok(_)) => {
                let taken = registered
                    .iter()
                    .any(|(_, other)| shape(other) == shape(&segments));
                assert!(!taken, "{pattern:?} has the shape of an earlier route");
                registered.push((pattern, segments));
            }
            (Ok(segments), Err(error)) => {
                assert!(
                    matches!(
                        error,
                        RouteError::Duplicate { .. } | RouteError::Ambiguous { .. }
                    ),
                    "{pattern:?}: {error}"
                );
                assert!(
                    registered
                        .iter()
                        .any(|(_, other)| shape(other) == shape(&segments)),
                    "{pattern:?} refused as a conflict with nothing"
                );
            }
            (Err(refusal), Err(error)) => match refusal {
                Refusal::Invalid => {
                    assert!(
                        matches!(error, RouteError::InvalidPath(_)),
                        "{pattern:?}: {error}"
                    );
                }
                Refusal::DuplicateName => assert!(
                    matches!(error, RouteError::DuplicateParameter { .. }),
                    "{pattern:?}: {error}"
                ),
            },
            (Err(_), Ok(_)) => panic!("{pattern:?} was registered"),
        }
    }

    let request = Request::new_for_test(
        Method::GET,
        uri,
        HeaderMap::new(),
        Bytes::new(),
        SocketAddr::from(([127, 0, 0, 1], 9)),
        uuid::Uuid::nil(),
    );
    let path = request.uri().path().to_owned();
    let response = block_on(router.dispatch(request));
    let hits = std::mem::take(&mut *seen.lock().expect("hits"));

    if invalid_path(&path) {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path:?}");
        assert!(hits.is_empty(), "{path:?} reached a handler");
        return;
    }

    // The route that serves the path: of those that match, the one that has
    // static text, then a capture, then a catch-all at the first position
    // where they differ.
    let best = registered
        .iter()
        .filter_map(|(pattern, segments)| {
            matches(segments, &path).map(|captures| (pattern, segments, captures))
        })
        .min_by_key(|(_, segments, _)| segments.iter().map(Segment::rank).collect::<Vec<_>>());
    let Some((pattern, segments, captures)) = best else {
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path:?}");
        assert!(hits.is_empty());
        return;
    };
    let rest_is_unclean = matches!(segments.last(), Some(Segment::Rest(_)))
        && captures
            .last()
            .is_some_and(|rest| rest.starts_with('/') || rest.contains("//"));
    if rest_is_unclean {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path:?}");
        assert!(hits.is_empty(), "{path:?} reached a handler");
        return;
    }

    assert_eq!(response.status(), StatusCode::OK, "{pattern:?} {path:?}");
    assert_eq!(hits.len(), 1, "{pattern:?} {path:?}");
    let hit = &hits[0];
    assert_eq!(hit.route.as_deref(), Some(*pattern));
    assert_eq!(hit.captures.len(), captures.len());
    for ((name, got), raw) in hit.captures.iter().zip(&captures) {
        assert_eq!(*got, percent_decode(raw).ok_or(()), "{name}: {raw:?}");
        // What a handler may join to a directory.
        if let Ok(value) = got {
            assert!(!value.contains('\\'), "{name}: backslash in {value:?}");
            assert!(
                value.split('/').all(|part| part != "." && part != ".."),
                "{name}: dot segment in {value:?}"
            );
            let catch_all = matches!(
                segments
                    .iter()
                    .find(|s| matches!(s, Segment::Param(n) | Segment::Rest(n) if n == name)),
                Some(Segment::Rest(_))
            );
            if catch_all {
                assert!(
                    !value.starts_with('/') && !value.contains("//"),
                    "{name}: {value:?}"
                );
            } else {
                assert!(
                    !value.is_empty() && !value.contains('/'),
                    "{name}: {value:?}"
                );
            }
        }
    }
});
