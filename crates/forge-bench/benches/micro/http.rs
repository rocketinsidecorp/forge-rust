//! `router_match`, `middleware_chain`, `http_telemetry` and the
//! `request_build` reference rows.

use std::hint::black_box;
use std::sync::Arc;

use forge::http::{
    BoxResponseFuture, Method, Middleware, Next, Request, Router, ServerInstrumentation, StatusCode,
};
use forge::telemetry::{Options, SpanSink, Telemetry};
use forge_bench::exec::block_on;
use forge_bench::{Case, Runner};

use crate::support::{RequestParts, must, no_content};

pub fn cases(cases: &mut Vec<Case>) {
    router_match(cases);
    middleware_chain(cases);
    http_telemetry(cases);
    request_build(cases);
}

// router_match ---------------------------------------------------------------

/// `Router::dispatch` for a request that matches the middle route of a table
/// of 1, 10, 100 or 1000 routes, answered by a handler that does nothing.
///
/// `static` is `/static/rN/item`, `param` is `/param/rN/{id}` and
/// `wildcard` is the catch-all `/files/rN/{path...}` matched by
/// `/files/rN/docs/2026/report.txt`, as in Forge-Go. `escaped` is `param` with
/// a percent-escaped capture (`/param/rN/a%20b`), the one path shape whose
/// invalid-path check has to decode a segment.
fn router_match(cases: &mut Vec<Case>) {
    for (variant, pattern, target) in [
        ("static", "/static/r{}/item", "/static/r{}/item"),
        ("param", "/param/r{}/{id}", "/param/r{}/42"),
        (
            "wildcard",
            "/files/r{}/{path...}",
            "/files/r{}/docs/2026/report.txt",
        ),
        // A percent-escaped capture: the invalid-path rule decodes it.
        ("escaped", "/param/r{}/{id}", "/param/r{}/a%20b"),
    ] {
        for routes in [1_usize, 10, 100, 1000] {
            let name: &'static str = Box::leak(format!("{variant}/{routes}").into_boxed_str());
            cases.push(Case::new("router_match", name, 1_000_000, move || {
                let mut router = Router::new();
                for index in 0..routes {
                    must(
                        router.route(
                            Method::GET,
                            pattern.replace("{}", &index.to_string()),
                            no_content,
                        ),
                        "route",
                    );
                }
                let parts = RequestParts::new(
                    Method::GET,
                    &target.replace("{}", &(routes / 2).to_string()),
                );
                assert_eq!(
                    block_on(router.dispatch(parts.build())).status(),
                    StatusCode::NO_CONTENT
                );
                Box::new(move |iterations| {
                    for _ in 0..iterations {
                        black_box(block_on(router.dispatch(black_box(parts.build()))));
                    }
                })
            }));
        }
    }
}

// middleware_chain -----------------------------------------------------------

/// A layer that only calls the rest of the chain.
struct Passthrough;

impl Middleware for Passthrough {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        next.run(request)
    }
}

/// A layer that also stores a value in the request's extensions, as most real
/// layers do. `http::Extensions` is keyed by type, so each depth has its own
/// type (`N`), where Forge-Go's context keys differ by value.
struct WithValue<const N: usize>;

#[derive(Clone)]
struct Stored<const N: usize>(#[allow(dead_code)] usize);

impl<const N: usize> Middleware for WithValue<N> {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        request.extensions_mut().insert(Stored::<N>(N));
        next.run(request)
    }
}

fn add_value_layer(router: &mut Router, index: usize) {
    match index {
        0 => router.layer(WithValue::<0>),
        1 => router.layer(WithValue::<1>),
        2 => router.layer(WithValue::<2>),
        3 => router.layer(WithValue::<3>),
        4 => router.layer(WithValue::<4>),
        5 => router.layer(WithValue::<5>),
        6 => router.layer(WithValue::<6>),
        7 => router.layer(WithValue::<7>),
        8 => router.layer(WithValue::<8>),
        _ => router.layer(WithValue::<9>),
    };
}

/// The request path of the server for one `GET /users/{id}` with N layers
/// added by `Router::layer`, in the default telemetry mode (enabled, nothing
/// exported), like Forge-Go's chain. See [`serve`] for what is, and what is
/// not, in that path.
fn middleware_chain(cases: &mut Vec<Case>) {
    for kind in ["passthrough", "context"] {
        for depth in [0_usize, 1, 5, 10] {
            let name: &'static str = Box::leak(format!("{kind}/{depth}").into_boxed_str());
            cases.push(Case::new("middleware_chain", name, 500_000, move || {
                let mut router = users_router();
                for index in 0..depth {
                    if kind == "passthrough" {
                        router.layer(Passthrough);
                    } else {
                        add_value_layer(&mut router, index);
                    }
                }
                let telemetry = must(Telemetry::new(Options::new("bench")), "telemetry");
                let instrumentation = must(telemetry.http_server(), "http instrumentation");
                server_loop(router, Some(instrumentation), Some(telemetry))
            }));
        }
    }
}

// http_telemetry -------------------------------------------------------------

/// The cost of telemetry on the same request path:
///
/// - `baseline`: `Router::dispatch` alone, no telemetry code at all;
/// - `disabled`: the server path of [`serve`] with `FORGE_TELEMETRY_ENABLED=false`,
///   which creates no instrumentation, so it is `baseline` plus reading the
///   status (Forge-Go's two figures also match by construction);
/// - `enabled_no_export`: instrumented, spans recorded and sampled, no exporter;
/// - `enabled_exporting`: the same with the `log` span sink running on a
///   Tokio runtime: every finished span is queued, batched and handed to the
///   exporter task, which writes one tracing event per span (no subscriber is
///   installed, so nothing is printed). Forge-Go's counterpart exports to a
///   discarding exporter. An OTLP export over the network is not measured.
fn http_telemetry(cases: &mut Vec<Case>) {
    cases.push(Case::new("http_telemetry", "baseline", 300_000, || {
        let router = users_router();
        let parts = RequestParts::new(Method::GET, "/users/42");
        Box::new(move |iterations| {
            for _ in 0..iterations {
                black_box(block_on(router.dispatch(black_box(parts.build()))).status());
            }
        })
    }));
    cases.push(Case::new("http_telemetry", "disabled", 300_000, || {
        server_loop(users_router(), None, None)
    }));
    cases.push(Case::new(
        "http_telemetry",
        "enabled_no_export",
        300_000,
        || {
            let telemetry = must(Telemetry::new(Options::new("bench")), "telemetry");
            let instrumentation = must(telemetry.http_server(), "http instrumentation");
            server_loop(users_router(), Some(instrumentation), Some(telemetry))
        },
    ));
    cases.push(Case::new(
        "http_telemetry",
        "enabled_exporting",
        300_000,
        || {
            let runtime = must(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_time()
                    .build(),
                "tokio runtime",
            );
            let mut options = Options::new("bench");
            options.spans = Some(SpanSink::Log);
            // The queue Forge-Go's benchmark uses, so a burst is not dropped.
            options.max_queue = 1 << 16;
            let telemetry = {
                let _entered = runtime.enter();
                let telemetry = must(Telemetry::new(options), "telemetry");
                telemetry.start();
                telemetry
            };
            let instrumentation = must(telemetry.http_server(), "http instrumentation");
            let mut run = server_loop(users_router(), Some(instrumentation), Some(telemetry));
            // The runtime lives as long as the loop that exports on it.
            Box::new(move |iterations| {
                run(iterations);
                black_box(&runtime);
            })
        },
    ));
}

fn users_router() -> Router {
    let mut router = Router::new();
    must(
        router.route(Method::GET, "/users/{id}", no_content),
        "route",
    );
    router
}

/// What `forge-http`'s server does around `Router::dispatch` for one request
/// that is not a network concern: start the request's telemetry, dispatch
/// through the layers and the route table, finish the telemetry with the
/// route and status. `Server::handle_request` itself is private and sits on
/// hyper, so this restates those steps; request parsing, the body limit, the
/// request timeout, panic catching and the log span are not included.
fn serve(
    router: &Router,
    instrumentation: Option<&ServerInstrumentation>,
    parts: &RequestParts,
) -> StatusCode {
    let request = parts.build();
    let telemetry = instrumentation.map(|instrumentation| {
        instrumentation.start(request.method(), http_version(), request.headers())
    });
    let status = block_on(router.dispatch(request)).status();
    if let Some(telemetry) = telemetry {
        telemetry.finish(Some("/users/{id}"), status.as_u16());
    }
    status
}

fn http_version() -> http::Version {
    http::Version::HTTP_11
}

/// The timed loop of [`serve`] for `GET /users/42`; `telemetry` is kept alive
/// with it.
fn server_loop(
    router: Router,
    instrumentation: Option<ServerInstrumentation>,
    telemetry: Option<Telemetry>,
) -> Runner {
    let router = Arc::new(router);
    let parts = RequestParts::new(Method::GET, "/users/42");
    assert_eq!(
        serve(&router, instrumentation.as_ref(), &parts),
        StatusCode::NO_CONTENT
    );
    Box::new(move |iterations| {
        for _ in 0..iterations {
            black_box(serve(&router, instrumentation.as_ref(), black_box(&parts)));
        }
        black_box(&telemetry);
    })
}

// request_build --------------------------------------------------------------

/// Reference rows: building (and dropping) the request every iteration, which
/// the scenarios that dispatch pay inside their figures.
fn request_build(cases: &mut Vec<Case>) {
    cases.push(
        Case::new("request_build", "get", 2_000_000, || {
            let parts = RequestParts::new(Method::GET, "/users/42");
            Box::new(move |iterations| {
                for _ in 0..iterations {
                    black_box(black_box(&parts).build());
                }
            })
        })
        .reference(),
    );
    cases.push(
        Case::new("request_build", "post_form", 1_000_000, || {
            let parts = RequestParts::new(Method::POST, "/posts")
                .named_header(
                    "cookie",
                    &format!("__Host-forge_csrf={}", crate::support::TOKEN),
                )
                .named_header("x-csrf-token", crate::support::TOKEN)
                .named_header("content-type", "application/x-www-form-urlencoded")
                .body(format!(
                    "csrf_token={}&title=Hello+Forge",
                    crate::support::TOKEN
                ));
            Box::new(move |iterations| {
                for _ in 0..iterations {
                    black_box(black_box(&parts).build());
                }
            })
        })
        .reference(),
    );
}
