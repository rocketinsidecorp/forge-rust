//! The public surface of `forge-http` that the server tests do not touch:
//! request and response accessors, error messages, `Debug` output, the
//! health route descriptions, and `Server::run` on a real address.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use forge_config::{ServerConfig, TrustedProxy};
use forge_http::{
    BoxResponseFuture, CheckResponses, FormErrorKind, HeaderMap, HeaderValue, JsonError, Method,
    Middleware, Next, PathParamError, QueryErrorKind, Request, Response, Router, Server,
    ServerError, StatusCode, TrustedProxies, describe, header, health_operation,
};
use forge_openapi::{Info, Operation};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Notify, oneshot},
};
use uuid::Uuid;

fn request(method: Method, uri: &str, headers: HeaderMap, body: &'static str) -> Request {
    Request::new_for_test(
        method,
        uri.parse().expect("valid URI"),
        headers,
        Bytes::from_static(body.as_bytes()),
        SocketAddr::from(([127, 0, 0, 1], 4000)),
        Uuid::now_v7(),
    )
}

fn get(uri: &str) -> Request {
    request(Method::GET, uri, HeaderMap::new(), "")
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct Paging {
    limit: u16,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Signup {
    name: String,
}

#[test]
fn a_request_exposes_its_parts_and_decodes_json_queries_and_forms() {
    let id = Uuid::now_v7();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    let mut form = Request::new_for_test(
        Method::POST,
        "/signup?limit=5".parse().expect("URI"),
        headers,
        Bytes::from_static(b"name=Ada"),
        SocketAddr::from(([127, 0, 0, 1], 4000)),
        id,
    );
    assert_eq!(form.request_id(), id);
    assert_eq!(form.route(), None);
    assert!(form.trace_context().is_none());
    assert!(form.has_form_body());
    assert_eq!(form.form::<Signup>().expect("form").name, "Ada");
    assert_eq!(
        form.form_value("name").expect("value").as_deref(),
        Some("Ada")
    );
    assert_eq!(form.form_value("missing").expect("absent"), None);
    assert_eq!(form.query::<Paging>().expect("query"), Paging { limit: 5 });
    form.set_method(Method::PUT);
    assert_eq!(form.method(), Method::PUT);
    assert!(form.extensions().get::<u8>().is_none());
    form.extensions_mut().insert(7_u8);
    assert_eq!(form.extensions().get::<u8>(), Some(&7));

    let json = request(Method::POST, "/", HeaderMap::new(), r#"{"limit": 9}"#);
    assert_eq!(json.json::<Paging>().expect("JSON"), Paging { limit: 9 });
    let broken = request(Method::POST, "/", HeaderMap::new(), "{");
    let error = broken.json::<Paging>().expect_err("not JSON");
    assert!(matches!(error, JsonError::Decode(_)));
    assert_eq!(error.to_string(), "request body is not valid JSON");
    let error = get("/?limit=x")
        .query::<Paging>()
        .expect_err("not a number");
    assert_eq!(error.kind(), QueryErrorKind::InvalidValue);
    assert_eq!(error.parameter(), Some("limit"));
}

#[test]
fn media_types_with_unreadable_header_values_are_not_forms_or_json() {
    for raw in [&b"application/x-www-form-urlencoded\xff"[..], b"\xff"] {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_bytes(raw).expect("opaque header value"),
        );
        let request = request(Method::POST, "/", headers, "name=Ada");
        assert!(!request.has_form_body());
        assert!(!request.is_json());
    }
}

struct Failing;

impl Serialize for Failing {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("refused"))
    }
}

#[tokio::test]
async fn responses_describe_themselves_and_report_serialization_failures() {
    let json = Response::json(StatusCode::CREATED, &Paging { limit: 1 }).expect("JSON");
    assert_eq!(
        json.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(json.body().as_ref(), br#"{"limit":1}"#);
    let debug = format!("{json:?}");
    assert!(
        debug.starts_with("Response { status: 201, headers: ") && debug.contains("body: b"),
        "{debug}"
    );

    let streamed = Response::stream(StatusCode::OK, 0_u8, |count| async move {
        (count < 2).then(|| (Bytes::from(format!("{count}")), count + 1))
    });
    assert!(format!("{streamed:?}").contains("body: \"<stream>\""));
    let mut streamed = streamed;
    let mut chunks = Vec::new();
    while let Some(chunk) = streamed.next_chunk().await {
        chunks.push(chunk);
    }
    assert_eq!(chunks, [Bytes::from_static(b"0"), Bytes::from_static(b"1")]);

    let error = Response::json(StatusCode::OK, &Failing).expect_err("serialization fails");
    assert!(matches!(error, JsonError::Encode(_)));
    assert_eq!(error.to_string(), "response could not be serialized");

    assert_eq!(
        PathParamError::Missing {
            name: "id".to_owned()
        }
        .name(),
        "id"
    );
    assert_eq!(
        PathParamError::Invalid {
            name: "slug".to_owned()
        }
        .name(),
        "slug"
    );
}

#[test]
fn every_error_kind_has_a_caller_safe_message() {
    let messages: Vec<&str> = [
        FormErrorKind::UnsupportedMediaType,
        FormErrorKind::Malformed,
        FormErrorKind::TooManyFields,
        FormErrorKind::DuplicateField,
        FormErrorKind::UnknownField,
        FormErrorKind::MissingField,
        FormErrorKind::InvalidValue,
    ]
    .into_iter()
    .map(FormErrorKind::message)
    .collect();
    assert_eq!(
        messages,
        [
            "request body must be application/x-www-form-urlencoded (UTF-8)",
            "form body is not valid application/x-www-form-urlencoded",
            "form body has too many fields",
            "form field must not be repeated",
            "unknown form field",
            "missing required form field",
            "form field has an invalid value",
        ]
    );
}

#[test]
fn trusted_proxies_list_their_networks() {
    let networks = [
        "10.0.0.0/8".parse::<TrustedProxy>().expect("network"),
        "fd00::/16".parse::<TrustedProxy>().expect("network"),
    ];
    let trusted = TrustedProxies::new(networks);
    assert_eq!(format!("{trusted:?}"), r#"["10.0.0.0/8", "fd00::/16"]"#);
    assert_eq!(trusted.proxies().len(), 2);
    assert_eq!(format!("{:?}", TrustedProxies::default()), "[]");
}

/// A layer that records how `Next` prints.
struct Printer(Arc<Mutex<String>>);

impl Middleware for Printer {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        if let Ok(mut text) = self.0.lock() {
            *text = format!("{next:?}");
        }
        next.run(request)
    }
}

#[tokio::test]
async fn routers_answer_health_checks_and_unmatched_requests()
-> Result<(), Box<dyn std::error::Error>> {
    let mut router = Router::new();
    let printed = Arc::new(Mutex::new(String::new()));
    router.layer(Printer(Arc::clone(&printed)));
    router.standard_health_routes()?;
    router.route(Method::POST, "/posts", |_| async {
        Response::empty(StatusCode::CREATED)
    })?;
    for (path, body) in [
        ("/health", r#"{"status":"ok"}"#),
        ("/health/live", r#"{"status":"live"}"#),
        ("/health/ready", r#"{"status":"ready"}"#),
    ] {
        let response = router.dispatch(get(path)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.body().as_ref(), body.as_bytes(), "{path}");
    }
    assert_eq!(
        router.dispatch(get("/nothing")).await.status(),
        StatusCode::NOT_FOUND
    );
    let wrong = router.dispatch(get("/posts")).await;
    assert_eq!(wrong.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        wrong
            .headers()
            .get(header::ALLOW)
            .and_then(|v| v.to_str().ok()),
        Some("POST")
    );
    assert!(
        printed.lock().expect("lock").contains("remaining_layers"),
        "{printed:?}"
    );
    assert!(format!("{router:?}").contains("GET /health/live"));
    assert_eq!(router.routes().len(), 4);
    Ok(())
}

#[tokio::test]
async fn the_ready_check_description_adds_the_failure_response_to_the_document()
-> Result<(), Box<dyn std::error::Error>> {
    let mut router = Router::new();
    let ready = describe(
        |_| async { Response::empty(StatusCode::SERVICE_UNAVAILABLE) },
        health_operation("/health/ready", true),
    );
    assert!(format!("{ready:?}").starts_with("Described { operation: "));
    router.route(Method::GET, "/health/ready", ready)?;
    router.route(
        Method::GET,
        "/events",
        describe(
            |_| async { Response::stream(StatusCode::OK, (), |()| async { None }) },
            Operation::new()
                .summary("Events")
                .response(forge_openapi::Response::empty(200)),
        ),
    )?;
    let document = router.openapi(&Info::new("app", "0.1.0"))?;
    let value = document.as_value();
    let responses = &value["paths"]["/health/ready"]["get"]["responses"];
    assert_eq!(responses["503"]["description"], "A check failed");
    assert!(
        value["components"]["schemas"]["HealthReport"].is_object(),
        "{value}"
    );
    // Streamed responses are never checked; the layer is not shown in the
    // report either.
    let failures = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&failures);
    let layer = CheckResponses::new(document, move |failure| {
        if let Ok(mut list) = recorded.lock() {
            list.push(failure.to_string());
        }
    });
    assert!(format!("{layer:?}").starts_with("CheckResponses"));
    router.layer(layer);
    let response = router.dispatch(get("/events")).await;
    assert!(response.is_stream());
    assert!(failures.lock().expect("lock").is_empty());
    Ok(())
}

// --- Server::run ------------------------------------------------------------------

fn config(bind: SocketAddr) -> ServerConfig {
    ServerConfig {
        bind,
        request_timeout: Duration::from_secs(5),
        shutdown_grace: Duration::from_millis(100),
        ..ServerConfig::default()
    }
}

async fn status_of(address: SocketAddr, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(address).await.ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .ok()?;
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response)).await;
    String::from_utf8_lossy(&response)
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
}

async fn eventually_serving(address: SocketAddr, path: &str) -> Option<u16> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if let Some(status) = status_of(address, path).await {
            return Some(status);
        }
        // real-clock: polls a real TCP listener or connection, which a fake clock cannot advance, until a deadline
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// A running `Server::run`, as `run_on_a_free_address` starts it.
struct Running {
    address: SocketAddr,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), ServerError>>,
}

/// Runs `Server::run` on a loopback address nobody had bound yet. `run` binds
/// the address itself and reports nothing, so a free one is probed first, and
/// another process may take it between the probe and the bind. That loss is a
/// bind error that ends the task before it served: the next attempt probes a
/// fresh address instead of failing the test.
async fn run_on_a_free_address() -> Running {
    for _ in 0..10 {
        let address = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            probe.local_addr().expect("address")
        };
        let mut router = Router::new();
        router.standard_health_routes().expect("health routes");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            Server::new(config(address), router)
                .run(async {
                    let _ = stopped.await;
                })
                .await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !task.is_finished() && tokio::time::Instant::now() < deadline {
            if status_of(address, "/health/live").await == Some(200) && !task.is_finished() {
                return Running {
                    address,
                    stop,
                    task,
                };
            }
            // real-clock: polls a real TCP listener or connection, which a fake clock cannot advance, until a deadline
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let outcome = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("a run that did not serve ends")
            .expect("task");
        assert!(
            matches!(outcome, Err(ServerError::Bind(_))),
            "only a lost bind race may end the run before it serves: {outcome:?}"
        );
    }
    panic!("no free address could be bound in ten attempts");
}

#[tokio::test]
async fn run_binds_the_configured_address_and_a_taken_one_is_an_error() {
    // A taken address, held for the whole check: the bind error is reported,
    // not swallowed.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = taken.local_addr().expect("address");
    let error = Server::new(config(address), Router::new())
        .run(std::future::pending())
        .await
        .expect_err("the address is in use");
    assert!(matches!(error, ServerError::Bind(_)), "{error:?}");
    drop(taken);

    // A free one: `run` serves there until the shutdown future resolves.
    let running = run_on_a_free_address().await;
    assert_eq!(
        eventually_serving(running.address, "/health/live").await,
        Some(200)
    );
    let _ = running.stop.send(());
    let outcome = tokio::time::timeout(Duration::from_secs(10), running.task)
        .await
        .expect("stops in time");
    outcome.expect("task").expect("serves cleanly");
}

#[tokio::test]
async fn shutdown_aborts_a_handler_that_outlives_the_grace_period() {
    let started = Arc::new(Notify::new());
    let announced = Arc::clone(&started);
    let mut router = Router::new();
    router
        .route(Method::GET, "/stuck", move |_| {
            let announced = Arc::clone(&announced);
            async move {
                announced.notify_one();
                std::future::pending::<Response>().await
            }
        })
        .expect("route");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        Server::new(config(address), router)
            .serve(listener, async {
                let _ = stopped.await;
            })
            .await
    });
    let mut client = TcpStream::connect(address).await.expect("connect");
    client
        .write_all(b"GET /stuck HTTP/1.1\r\nhost: test\r\n\r\n")
        .await
        .expect("request");
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the handler started");
    let _ = stop.send(());
    // The handler never answers: the grace period ends and the connection
    // is aborted, so the server returns and the client sees the end.
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("stops in time");
    outcome.expect("task").expect("serves cleanly");
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), client.read_to_end(&mut rest))
        .await
        .expect("the client sees the connection end in time")
        .expect("the aborted connection reads to its end");
}
