//! `json_render`, `html_render` and `csrf_verify`.

use std::hint::black_box;
use std::sync::Arc;

use askama::Template;
use forge::auth::CsrfToken;
use forge::core::{Timestamp, Uuid};
use forge::http::{Method, Request, Response, Router, StatusCode};
use forge::views::{
    CsrfProtection, FlashLayer, csrf_field, csrf_meta, csrf_token, csrf_verified, flash, html,
};
use forge_bench::exec::block_on;
use forge_bench::{Case, Runner};
use serde::Serialize;
use subtle::ConstantTimeEq as _;

use crate::support::{RequestParts, TOKEN, must};

pub fn cases(cases: &mut Vec<Case>) {
    json_render(cases);
    html_render(cases);
    csrf_verify(cases);
}

// json_render ----------------------------------------------------------------

/// One record as a scaffold's JSON API would show it.
#[derive(Clone, Serialize)]
struct Post {
    id: Uuid,
    title: String,
    body: String,
    version: i64,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// A page of records, as a scaffold's JSON index would list it.
#[derive(Serialize)]
struct Page {
    items: Vec<Post>,
    next_cursor: String,
}

/// `Response::json` of one record and of a page of 50 (it serializes into the
/// response body; nothing is written to a socket).
fn json_render(cases: &mut Vec<Case>) {
    fn post() -> Post {
        let now = must(Timestamp::parse("2026-10-20T12:00:00Z"), "timestamp");
        Post {
            id: Uuid::now_v7(),
            title: "Hello Forge".to_owned(),
            body: "My first post, a few sentences long so that the body is not trivially small."
                .to_owned(),
            version: 1,
            created_at: now,
            updated_at: now,
        }
    }
    cases.push(Case::new("json_render", "object", 500_000, || {
        let post = post();
        assert_eq!(
            must(Response::json(StatusCode::OK, &post), "json").status(),
            StatusCode::OK
        );
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(Response::json(StatusCode::OK, black_box(&post)).is_ok());
            }
        })
    }));
    cases.push(Case::new("json_render", "list_50", 20_000, || {
        let page = Page {
            items: vec![post(); 50],
            next_cursor: String::new(),
        };
        assert_eq!(
            must(Response::json(StatusCode::OK, &page), "json").status(),
            StatusCode::OK
        );
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(Response::json(StatusCode::OK, black_box(&page)).is_ok());
            }
        })
    }));
}

// html_render ----------------------------------------------------------------

/// A row of the page; the text contains markup that auto-escaping must handle.
struct Row {
    id: Uuid,
    title: String,
    body: &'static str,
}

fn rows() -> Vec<Row> {
    (0..50)
        .map(|index| Row {
            id: Uuid::now_v7(),
            title: format!("Post <{index}> & friends"),
            body: "Tom & Jerry say \"hello\" <b>world</b>",
        })
        .collect()
}

/// A layout with two partials, no request helpers.
#[derive(Template)]
#[template(path = "posts/plain_index.html")]
struct PlainIndex<'a> {
    rows: &'a [Row],
}

/// The page a generated app serves: the CSRF meta tag in the head, a form
/// with a token, and the flash messages.
#[derive(Template)]
#[template(path = "posts/index.html")]
struct CsrfIndex<'a> {
    rows: &'a [Row],
    csrf_meta: String,
    csrf_field: String,
    notice: Option<String>,
    alert: Option<String>,
}

/// A page of 50 rows in a layout with partials and HTML auto-escaping
/// (compile-time Askama templates, the generated applications' engine).
/// `layout_partial_csrf` goes through the CSRF and flash layers and a router,
/// like the real page; `layout_partial` is `forge::views::html` alone.
fn html_render(cases: &mut Vec<Case>) {
    cases.push(Case::new("html_render", "layout_partial", 10_000, || {
        let rows = rows();
        let page = html(&PlainIndex { rows: &rows });
        assert_eq!(page.status(), StatusCode::OK);
        assert!(String::from_utf8_lossy(page.body()).contains("Post &#60;7&#62; &#38; friends"));
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(html(&PlainIndex {
                    rows: black_box(&rows),
                }));
            }
        })
    }));
    cases.push(Case::new(
        "html_render",
        "layout_partial_csrf",
        10_000,
        || {
            let rows = Arc::new(rows());
            let mut router = Router::new();
            router.layer(FlashLayer::new());
            router.layer(CsrfProtection::new());
            must(
                router.route(Method::GET, "/posts", move |request: Request| {
                    let rows = Arc::clone(&rows);
                    async move {
                        let token = csrf_token(&request);
                        let flashes = flash(&request);
                        html(&CsrfIndex {
                            rows: &rows,
                            csrf_meta: token.as_ref().map(csrf_meta).unwrap_or_default(),
                            csrf_field: token.as_ref().map(csrf_field).unwrap_or_default(),
                            notice: flashes.notice().map(str::to_owned),
                            alert: flashes.alert().map(str::to_owned),
                        })
                    }
                }),
                "route",
            );
            let parts = RequestParts::new(Method::GET, "/posts")
                .named_header("cookie", &format!("__Host-forge_csrf={TOKEN}"));
            let page = block_on(router.dispatch(parts.build()));
            assert_eq!(page.status(), StatusCode::OK);
            assert!(String::from_utf8_lossy(page.body()).contains(TOKEN));
            Box::new(move |iterations| {
                for _ in 0..iterations {
                    black_box(block_on(router.dispatch(black_box(parts.build()))));
                }
            })
        },
    ));
}

// csrf_verify ----------------------------------------------------------------

/// - `match`: the constant-time comparison of two canonical tokens that
///   `forge-views` makes (`subtle::ConstantTimeEq` on the 64 bytes; the
///   function itself is private, so this is the same expression);
/// - `header_post`: a POST through `CsrfProtection` with the token in the
///   `x-csrf-token` header;
/// - `form_post`: the same with the token in the `csrf_token` field of the
///   form body (parsed on every request).
fn csrf_verify(cases: &mut Vec<Case>) {
    cases.push(Case::new("csrf_verify", "match", 2_000_000, || {
        let cookie = must(CsrfToken::new(TOKEN), "token");
        let presented = cookie.clone();
        assert!(bool::from(
            cookie
                .expose()
                .as_bytes()
                .ct_eq(presented.expose().as_bytes())
        ));
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(bool::from(
                    black_box(cookie.expose().as_bytes())
                        .ct_eq(black_box(presented.expose().as_bytes())),
                ));
            }
        })
    }));
    cases.push(Case::new("csrf_verify", "header_post", 200_000, || {
        let parts = RequestParts::new(Method::POST, "/posts")
            .named_header("cookie", &format!("__Host-forge_csrf={TOKEN}"))
            .named_header("x-csrf-token", TOKEN);
        post_loop(&parts)
    }));
    cases.push(Case::new("csrf_verify", "form_post", 100_000, || {
        let parts = RequestParts::new(Method::POST, "/posts")
            .named_header("cookie", &format!("__Host-forge_csrf={TOKEN}"))
            .named_header("content-type", "application/x-www-form-urlencoded")
            .body(format!(
                "csrf_token={TOKEN}&title=Hello+Forge&body=My+first+post"
            ));
        post_loop(&parts)
    }));
}

/// A router whose `POST /posts` answers `204` only when `CsrfProtection`
/// verified the request, and the loop that posts `parts` to it.
fn post_loop(parts: &RequestParts) -> Runner {
    let mut router = Router::new();
    router.layer(CsrfProtection::new());
    must(
        router.route(Method::POST, "/posts", |request: Request| async move {
            if csrf_verified(&request).is_some() {
                Response::empty(StatusCode::NO_CONTENT)
            } else {
                Response::empty(StatusCode::FORBIDDEN)
            }
        }),
        "route",
    );
    assert_eq!(
        block_on(router.dispatch(parts.build())).status(),
        StatusCode::NO_CONTENT
    );
    let parts = parts.clone();
    Box::new(move |iterations| {
        for _ in 0..iterations {
            black_box(block_on(router.dispatch(black_box(parts.build()))));
        }
    })
}
