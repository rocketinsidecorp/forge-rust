use std::{
    io::{ErrorKind, Read, Write},
    net::TcpStream as StdTcpStream,
};

use tokio::{sync::oneshot, task::JoinHandle};

use super::*;

/// Upper bound for any single client wait; generous to avoid CI flakiness.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

fn request(method: Method, path: &str) -> Request {
    Request {
        method,
        uri: path.parse().expect("valid test URI"),
        headers: HeaderMap::new(),
        body: Bytes::new(),
        path_params: HashMap::new(),
        remote_addr: "127.0.0.1:4000".parse().expect("valid test address"),
        client_ip: None,
        request_id: Uuid::now_v7(),
        route: None,
        extensions: Extensions::new(),
    }
}

/// Test layer: records its name in the `x-trace` request/response
/// headers, optionally rewrites the method or answers directly.
struct Trace {
    name: &'static str,
    rewrite: Option<Method>,
    answer: Option<StatusCode>,
}

impl Middleware for Trace {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        Box::pin(async move {
            if let Some(status) = self.answer {
                return Response::empty(status);
            }
            if let Some(method) = &self.rewrite {
                request.set_method(method.clone());
            }
            request.extensions_mut().insert(self.name);
            let mut response = next.run(request).await;
            response
                .headers_mut()
                .append("x-trace", HeaderValue::from_static(self.name));
            response
        })
    }
}

fn trace(name: &'static str) -> Trace {
    Trace {
        name,
        rewrite: None,
        answer: None,
    }
}

/// Test layer: removes a `/v2` path prefix before routing.
struct StripV2;

impl Middleware for StripV2 {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        if let Some(rest) = request.uri().path().strip_prefix("/v2")
            && let Ok(uri) = rest.parse::<Uri>()
        {
            request.set_uri(uri);
        }
        next.run(request)
    }
}

#[tokio::test]
async fn a_layer_can_rewrite_the_path_the_route_table_sees() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/items", |request: Request| async move {
            Response::new(StatusCode::OK, request.uri().path().to_owned())
        })
        .expect("route registers");
    router.layer(StripV2);

    let response = router.dispatch(request(Method::GET, "/v2/items")).await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body(), &"/items");
}

#[tokio::test]
async fn layers_run_in_registration_order_before_routing() {
    let mut router = Router::new();
    router
        .route(Method::DELETE, "/items", |request: Request| async move {
            let seen = request
                .extensions()
                .get::<&'static str>()
                .copied()
                .unwrap_or("none");
            Response::new(StatusCode::OK, seen)
        })
        .expect("route registers");
    router.layer(trace("outer")).layer(Trace {
        rewrite: Some(Method::DELETE),
        ..trace("inner")
    });

    let response = router.dispatch(request(Method::POST, "/items")).await;

    // The inner layer rewrote POST to DELETE before the route table ran.
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body(), &"inner");
    let traced: Vec<_> = response
        .headers
        .get_all("x-trace")
        .iter()
        .map(|value| value.to_str().expect("ASCII"))
        .collect();
    assert_eq!(traced, ["inner", "outer"]);
    assert!(format!("{router:?}").contains("layers: 2"));
}

#[tokio::test]
async fn layers_see_unrouted_requests_and_can_answer_directly() {
    let mut router = Router::new();
    router.layer(trace("outer"));
    let not_found = router.dispatch(request(Method::GET, "/missing")).await;
    assert_eq!(not_found.status, StatusCode::NOT_FOUND);
    assert_eq!(
        not_found.headers.get("x-trace").map(HeaderValue::as_bytes),
        Some(&b"outer"[..])
    );

    router.layer(Trace {
        answer: Some(StatusCode::FORBIDDEN),
        ..trace("guard")
    });
    let refused = router.dispatch(request(Method::GET, "/missing")).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn request_form_decodes_bodies_and_reads_reserved_fields() {
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Login {
        email: String,
        #[serde(default)]
        remember: bool,
    }
    let mut form = request(Method::POST, "/session");
    form.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    form.body = Bytes::from_static(b"email=a%40example.com&remember=on&csrf_token=t&_method=PUT");

    assert!(form.has_form_body());
    let login: Login = form.form().expect("valid form");
    assert_eq!(login.email, "a@example.com");
    assert!(login.remember);
    assert_eq!(
        form.form_value(CSRF_FIELD_NAME).expect("valid"),
        Some("t".to_owned())
    );
    assert_eq!(
        form.form_value(METHOD_FIELD_NAME).expect("valid"),
        Some("PUT".to_owned())
    );

    let json = request(Method::POST, "/session");
    assert!(!json.has_form_body());
    assert_eq!(
        json.form::<Login>().expect_err("no form").kind(),
        FormErrorKind::UnsupportedMediaType
    );
}

#[tokio::test]
async fn routes_to_registered_handler() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/customers", |_| async {
            Response::new(StatusCode::OK, "customers")
        })
        .expect("route must register");

    let response = router.dispatch(request(Method::GET, "/customers")).await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body(), &"customers");
}

#[tokio::test]
async fn dynamic_route_captures_and_decodes_typed_path_parameters() {
    let mut router = Router::new();
    router
        .route(
            Method::GET,
            "/customers/{id}",
            |request: Request| async move {
                let id: Uuid = request
                    .path_param_as("id")
                    .expect("route should provide a valid UUID capture");
                Response::new(StatusCode::OK, id.to_string())
            },
        )
        .expect("dynamic route must register");

    let id = Uuid::now_v7();
    let response = router
        .dispatch(request(Method::GET, &format!("/customers/{id}")))
        .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body(), &id.to_string());
}

#[tokio::test]
async fn dynamic_route_percent_decodes_utf8_without_form_decoding_plus() {
    let mut router = Router::new();
    router
        .route(
            Method::GET,
            "/products/{slug}",
            |request: Request| async move {
                let slug: String = request.path_param_as("slug").expect("valid path capture");
                Response::new(StatusCode::OK, slug)
            },
        )
        .expect("dynamic route must register");

    let response = router
        .dispatch(request(Method::GET, "/products/caf%C3%A9%2Btea"))
        .await;

    assert_eq!(response.body(), &"café+tea");
}

#[tokio::test]
async fn static_routes_take_precedence_over_parameters() {
    let mut router = Router::new();
    router
        .route(
            Method::GET,
            "/customers/{id}",
            |request: Request| async move {
                let id: String = request.path_param_as("id").expect("id capture");
                Response::new(StatusCode::OK, format!("customer:{id}"))
            },
        )
        .expect("parameter route must register");
    router
        .route(Method::GET, "/customers/new", |_| async {
            Response::new(StatusCode::OK, "new-customer")
        })
        .expect("static route must register");

    let response = router
        .dispatch(request(Method::GET, "/customers/new"))
        .await;

    assert_eq!(response.body(), &"new-customer");
}

#[tokio::test]
async fn dynamic_path_parameter_errors_are_distinguishable() {
    let mut router = Router::new();
    router
        .route(
            Method::GET,
            "/customers/{id}",
            |request: Request| async move {
                let result = request.path_param_as::<u64>("id");
                let status = match result {
                    Ok(_) => StatusCode::OK,
                    Err(PathParamError::Missing { .. }) => StatusCode::INTERNAL_SERVER_ERROR,
                    Err(PathParamError::Invalid { .. }) => StatusCode::BAD_REQUEST,
                };
                Response::empty(status)
            },
        )
        .expect("dynamic route must register");

    let invalid = router
        .dispatch(request(Method::GET, "/customers/not-a-number"))
        .await;
    let malformed_escape = router
        .dispatch(request(Method::GET, "/customers/%GG"))
        .await;

    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(malformed_escape.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn encoded_slash_in_a_capture_is_a_bad_request() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/files/{name}", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("dynamic route must register");

    for path in ["/files/a%2Fb", "/files/a%2fb", "/files/%2F"] {
        let response = router.dispatch(request(Method::GET, path)).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{path}");
    }
    let escaped_percent = router
        .dispatch(request(Method::GET, "/files/a%252Fb"))
        .await;
    assert_eq!(escaped_percent.status, StatusCode::OK);
}

#[tokio::test]
async fn unmatched_and_empty_segments_are_not_found() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/customers/{id}", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("dynamic route must register");

    for path in [
        "/customers",
        "/customers/",
        "/customers/1/extra",
        "/other/1",
    ] {
        let response = router.dispatch(request(Method::GET, path)).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn allow_header_lists_registered_methods_sorted() {
    let mut router = Router::new();
    for method in [Method::PUT, Method::DELETE, Method::GET] {
        router
            .route(method, "/customers/{id}", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("dynamic route must register");
    }

    let response = router.dispatch(request(Method::POST, "/customers/1")).await;

    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers[header::ALLOW], "DELETE, GET, HEAD, PUT");
}

#[tokio::test]
async fn head_is_served_by_the_get_route_with_the_head_method() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/posts/{id}", |request: Request| async move {
            Response::text(StatusCode::OK, format!("{} post", request.method()))
        })
        .expect("GET route must register");

    let response = router.dispatch(request(Method::HEAD, "/posts/7")).await;

    assert_eq!(response.status, StatusCode::OK);
    // In process the body stays; the server drops it on the wire.
    assert_eq!(response.body(), "HEAD post");
}

#[tokio::test]
async fn an_explicit_head_route_wins_over_the_get_route() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/report", |_| async {
            Response::new(StatusCode::OK, "get")
        })
        .expect("GET route must register")
        .route(Method::HEAD, "/report", |_| async {
            Response::empty(StatusCode::NO_CONTENT)
        })
        .expect("HEAD route must register");

    let response = router.dispatch(request(Method::HEAD, "/report")).await;

    assert_eq!(response.status, StatusCode::NO_CONTENT);
    let post = router.dispatch(request(Method::POST, "/report")).await;
    assert_eq!(post.headers[header::ALLOW], "GET, HEAD");
}

#[tokio::test]
async fn head_without_a_get_route_is_not_allowed() {
    let mut router = Router::new();
    router
        .route(Method::POST, "/posts", |_| async {
            Response::empty(StatusCode::CREATED)
        })
        .expect("POST route must register");

    let response = router.dispatch(request(Method::HEAD, "/posts")).await;

    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers[header::ALLOW], "POST");
}

#[tokio::test]
async fn dynamic_route_method_mismatch_reports_allowed_methods() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/customers/{id}", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("dynamic route must register");

    let response = router
        .dispatch(request(Method::POST, "/customers/123"))
        .await;

    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers[header::ALLOW], "GET, HEAD");
}

#[test]
fn ambiguous_dynamic_routes_are_rejected() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/customers/{id}", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("first parameter route must register");

    let error = router
        .route(Method::GET, "/customers/{customer_id}", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect_err("equivalent parameter route must be rejected");

    assert!(matches!(error, RouteError::Ambiguous { .. }));
}

#[tokio::test]
async fn distinguishes_unknown_path_from_wrong_method() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/customers", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("route must register");

    let wrong_method = router.dispatch(request(Method::POST, "/customers")).await;
    let missing = router.dispatch(request(Method::GET, "/missing")).await;

    assert_eq!(wrong_method.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[test]
fn duplicate_routes_are_rejected() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/health", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("first route must register");
    let error = router
        .route(Method::GET, "/health", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect_err("duplicate route must fail");

    assert!(matches!(error, RouteError::Duplicate { .. }));
}

#[test]
fn routes_lists_every_registration_with_its_call_site() {
    let mut router = Router::new();
    let first = line!() + 2;
    router
        .route(Method::GET, "/posts/{id}", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("route must register");
    let second = line!() + 2;
    router
        .route(Method::POST, "/posts", |_| async {
            Response::empty(StatusCode::CREATED)
        })
        .expect("route must register");
    // A refused registration is not listed.
    assert!(
        router
            .route(Method::POST, "/posts", |_| async {
                Response::empty(StatusCode::OK)
            })
            .is_err()
    );

    let routes: Vec<_> = router
        .routes()
        .map(|route| {
            (
                route.method.as_str(),
                route.path,
                route.source.file(),
                route.source.line(),
            )
        })
        .collect();
    assert_eq!(
        routes,
        [
            ("GET", "/posts/{id}", file!(), first),
            ("POST", "/posts", file!(), second),
        ]
    );
}

#[test]
fn security_headers_are_applied_at_transport_boundary() {
    let outgoing = to_hyper_response(
        Response::empty(StatusCode::NO_CONTENT),
        Uuid::now_v7(),
        false,
    );

    assert_eq!(outgoing.headers()["x-content-type-options"], "nosniff");
    assert_eq!(outgoing.headers()["x-frame-options"], "DENY");
    assert!(outgoing.headers().contains_key("x-request-id"));
    // Plain HTTP: no HSTS, which browsers would ignore anyway.
    assert!(
        !outgoing
            .headers()
            .contains_key(header::STRICT_TRANSPORT_SECURITY)
    );
}

#[test]
fn hsts_is_added_when_clients_use_https_and_a_handler_value_is_kept() {
    let outgoing = to_hyper_response(Response::empty(StatusCode::OK), Uuid::now_v7(), true);
    assert_eq!(
        outgoing.headers()[header::STRICT_TRANSPORT_SECURITY],
        "max-age=31536000; includeSubDomains"
    );

    let mut preload = Response::empty(StatusCode::OK);
    preload.headers_mut().insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=63072000; includeSubDomains; preload"),
    );
    let outgoing = to_hyper_response(preload, Uuid::now_v7(), true);
    let values: Vec<_> = outgoing
        .headers()
        .get_all(header::STRICT_TRANSPORT_SECURITY)
        .iter()
        .collect();
    assert_eq!(values, ["max-age=63072000; includeSubDomains; preload"]);
}

#[tokio::test]
async fn the_server_sends_hsts_only_behind_tls_termination() {
    for (transport, expected) in [
        (forge_config::HttpTransport::Plain, None),
        (
            forge_config::HttpTransport::TrustedProxy,
            Some("strict-transport-security: max-age=31536000; includesubdomains"),
        ),
    ] {
        let mut config = test_config(Duration::from_secs(10));
        config.transport = transport;
        let server = TestServer::start(config, panicking_router()).await;
        // Every response, the routed one, a 404 and a recovered panic.
        for target in ["/health", "/missing", "/panic"] {
            let raw =
                format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let addr = server.addr;
            let response =
                tokio::task::spawn_blocking(move || exchange_blocking(addr, raw.as_bytes()))
                    .await
                    .expect("client task must not panic")
                    .to_ascii_lowercase();
            match expected {
                Some(line) => assert!(
                    response.contains(line),
                    "{transport:?} {target}: {response}"
                ),
                None => assert!(
                    !response.contains("strict-transport-security"),
                    "{transport:?} {target}: {response}"
                ),
            }
        }
        server.stop().await;
    }
}
#[test]
fn router_debug_lists_routes() {
    let mut router = Router::new();
    router
        .standard_health_routes()
        .expect("health routes must register");

    let debug = format!("{router:?}");

    assert!(debug.contains("GET /health/live"), "{debug}");
}

#[tokio::test]
async fn health_routes_return_json() {
    let mut router = Router::new();
    router
        .standard_health_routes()
        .expect("health routes must register");

    for (path, body) in [
        ("/health", r#"{"status":"ok"}"#),
        ("/health/live", r#"{"status":"live"}"#),
        ("/health/ready", r#"{"status":"ready"}"#),
    ] {
        let response = router.dispatch(request(Method::GET, path)).await;

        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert_eq!(response.body(), &body, "{path}");
        assert_eq!(
            response.headers[header::CONTENT_TYPE],
            "application/json",
            "{path}"
        );
    }
}

struct TestServer {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), ServerError>>,
}

impl TestServer {
    async fn start(config: ServerConfig, router: Router) -> Self {
        Self::start_server(Server::new(config, router)).await
    }

    async fn start_server(server: Server) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener must bind");
        let addr = listener.local_addr().expect("listener has an address");
        let (shutdown, signal) = oneshot::channel::<()>();
        let handle = tokio::spawn(server.serve(listener, async {
            // A dropped sender also means shutdown.
            let _ = signal.await;
        }));
        Self {
            addr,
            shutdown,
            handle,
        }
    }

    async fn stop(self) {
        self.shutdown
            .send(())
            .expect("server must still be waiting for shutdown");
        tokio::time::timeout(CLIENT_TIMEOUT, self.handle)
            .await
            .expect("server must stop promptly")
            .expect("server task must not panic")
            .expect("server must shut down cleanly");
    }
}

fn test_config(request_timeout: Duration) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1:0".parse().expect("valid address"),
        request_timeout,
        shutdown_grace: Duration::from_secs(30),
        max_body_bytes: 64,
        max_connections: 16,
        max_in_flight_body_bytes: 1024,
        trusted_proxies: Vec::new(),
        transport: forge_config::HttpTransport::Plain,
    }
}

fn test_router() -> Router {
    let mut router = Router::new();
    router
        .standard_health_routes()
        .expect("health routes must register");
    router
        .route(Method::POST, "/echo", |request: Request| async move {
            Response::new(StatusCode::OK, request.body().clone())
        })
        .expect("echo route must register");
    router
        .route(Method::GET, "/slow", |_| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Response::empty(StatusCode::OK)
        })
        .expect("slow route must register");
    router
}

fn connect(addr: SocketAddr) -> StdTcpStream {
    let stream = StdTcpStream::connect(addr).expect("client must connect");
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .expect("read timeout must be settable");
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .expect("write timeout must be settable");
    stream
}

/// Writes raw bytes and reads until the server closes the connection.
fn exchange_blocking(addr: SocketAddr, raw: &[u8]) -> String {
    let mut stream = connect(addr);
    stream.write_all(raw).expect("request must be written");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("server must close the connection before the client timeout");
    String::from_utf8_lossy(&response).into_owned()
}

async fn exchange(addr: SocketAddr, raw: &'static str) -> String {
    tokio::task::spawn_blocking(move || exchange_blocking(addr, raw.as_bytes()))
        .await
        .expect("client task must not panic")
}

/// Reads from a keep-alive connection until `marker` has been received.
fn read_until(stream: &mut StdTcpStream, marker: &str) -> String {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !String::from_utf8_lossy(&received).contains(marker) {
        let read = stream.read(&mut buffer).expect("response must arrive");
        assert!(read > 0, "connection closed before {marker:?} arrived");
        received.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&received).into_owned()
}

/// Answers with the client address the first layer saw.
struct SeenByFirstLayer;

impl Middleware for SeenByFirstLayer {
    fn handle<'a>(&'a self, mut request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        let seen = request.client_ip();
        request.extensions_mut().insert(seen);
        next.run(request)
    }
}

fn client_ip_router() -> Router {
    let mut router = Router::new();
    router.layer(SeenByFirstLayer);
    router
        .route(Method::GET, "/ip", |request: Request| async move {
            let seen = request.extensions().get::<IpAddr>().copied();
            Response::new(
                StatusCode::OK,
                format!("first-layer={seen:?} handler={}", request.client_ip()),
            )
        })
        .expect("ip route must register");
    router
}

#[tokio::test]
async fn server_resolves_the_client_from_trusted_proxies_only_before_every_layer() {
    let raw = "GET /ip HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 1.2.3.4, 203.0.113.9\r\nX-Real-IP: 198.51.100.1\r\nConnection: close\r\n\r\n";

    // The test client connects from 127.0.0.1; trusting it believes the
    // header, and the first layer already sees the resolved client.
    let mut trusting = test_config(Duration::from_secs(10));
    trusting.trusted_proxies = vec!["127.0.0.0/8".parse().expect("valid network")];
    let server = TestServer::start(trusting, client_ip_router()).await;
    let response = exchange(server.addr, raw).await;
    assert!(
        response.contains("first-layer=Some(203.0.113.9) handler=203.0.113.9"),
        "{response}"
    );
    server.stop().await;

    // Negative: the default trusts nobody, so a client that connects
    // directly cannot choose its address with any forwarding header.
    let server = TestServer::start(test_config(Duration::from_secs(10)), client_ip_router()).await;
    let response = exchange(server.addr, raw).await;
    assert!(
        response.contains("first-layer=Some(127.0.0.1) handler=127.0.0.1"),
        "{response}"
    );
    server.stop().await;

    // Negative: a trusted network that does not contain the peer.
    let mut elsewhere = test_config(Duration::from_secs(10));
    elsewhere.trusted_proxies = vec!["10.0.0.0/8".parse().expect("valid network")];
    let server = TestServer::start(elsewhere, client_ip_router()).await;
    let response = exchange(server.addr, raw).await;
    assert!(response.contains("handler=127.0.0.1"), "{response}");
    server.stop().await;
}

#[tokio::test]
async fn serve_answers_liveness_and_stops_on_shutdown() {
    let server = TestServer::start(test_config(Duration::from_secs(10)), test_router()).await;

    let response = exchange(
        server.addr,
        "GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: application/json\r\n"),
        "{response}"
    );
    assert!(response.ends_with(r#"{"status":"live"}"#), "{response}");
    server.stop().await;
}

#[tokio::test]
async fn slow_handler_times_out_with_gateway_timeout() {
    let server = TestServer::start(test_config(Duration::from_millis(200)), test_router()).await;

    let response = exchange(
        server.addr,
        "GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 504 "), "{response}");
    server.stop().await;
}

#[tokio::test]
async fn request_timeout_covers_slow_body() {
    let server = TestServer::start(test_config(Duration::from_millis(200)), test_router()).await;

    // Declares ten body bytes but sends two, then stalls.
    let response = exchange(
        server.addr,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\
         Connection: close\r\n\r\nab",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 504 "), "{response}");
    server.stop().await;
}

#[tokio::test]
async fn slow_headers_close_the_connection() {
    let server = TestServer::start(test_config(Duration::from_millis(200)), test_router()).await;

    // The header block is never terminated.
    let response = exchange(server.addr, "GET /health HTTP/1.1\r\nHost: localhost\r\n").await;

    assert!(!response.contains(" 200 "), "{response}");
    server.stop().await;
}

#[tokio::test]
async fn complete_body_is_delivered_to_handler() {
    let server = TestServer::start(test_config(Duration::from_secs(10)), test_router()).await;

    let response = exchange(
        server.addr,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\
         Connection: close\r\n\r\nhello",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(response.ends_with("\r\n\r\nhello"), "{response}");
    server.stop().await;
}

#[tokio::test]
async fn oversized_bodies_are_rejected() {
    let server = TestServer::start(test_config(Duration::from_secs(10)), test_router()).await;

    let declared = exchange(
        server.addr,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000\r\n\
         Connection: close\r\n\r\n",
    )
    .await;
    assert!(declared.starts_with("HTTP/1.1 413 "), "{declared}");
    // The media type the OpenAPI document declares for the refusal.
    assert!(
        declared
            .to_ascii_lowercase()
            .contains("content-type: text/plain; charset=utf-8"),
        "{declared}"
    );

    // An 80-byte chunk exceeds the 64-byte limit without a Content-Length.
    let chunked = exchange(
        server.addr,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n50\r\n\
         01234567890123456789012345678901234567890123456789012345678901234567890123456789\
         \r\n0\r\n\r\n",
    )
    .await;
    assert!(chunked.starts_with("HTTP/1.1 413 "), "{chunked}");
    server.stop().await;
}

#[tokio::test]
async fn connection_limit_applies_backpressure() {
    let mut config = test_config(Duration::from_secs(10));
    config.max_connections = 1;
    let server = TestServer::start(config, test_router()).await;
    let addr = server.addr;

    tokio::task::spawn_blocking(move || {
        // The first client keeps its connection (and the only permit) open.
        let mut holder = connect(addr);
        holder
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("request must be written");
        let first = read_until(&mut holder, r#"{"status":"ok"}"#);
        assert!(first.starts_with("HTTP/1.1 200 "), "{first}");

        // The second client sits in the listen backlog and gets no answer.
        let mut waiting = connect(addr);
        waiting
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("request must be written");
        waiting
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("read timeout must be settable");
        let mut probe = [0_u8; 1];
        let error = waiting
            .read(&mut probe)
            .expect_err("saturated server must not answer");
        assert!(
            matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
            "{error}"
        );

        // Releasing the first connection frees the permit.
        drop(holder);
        waiting
            .set_read_timeout(Some(CLIENT_TIMEOUT))
            .expect("read timeout must be settable");
        let mut second = Vec::new();
        waiting
            .read_to_end(&mut second)
            .expect("second client must be served once a permit is free");
        let second = String::from_utf8_lossy(&second);
        assert!(second.starts_with("HTTP/1.1 200 "), "{second}");
    })
    .await
    .expect("client task must not panic");

    server.stop().await;
}

#[tokio::test]
async fn shutdown_interrupts_saturated_server_and_closes_idle_connections() {
    let mut config = test_config(Duration::from_secs(10));
    config.max_connections = 1;
    let server = TestServer::start(config, test_router()).await;
    let addr = server.addr;

    // Hold the only permit with an idle keep-alive connection.
    let holder = tokio::task::spawn_blocking(move || {
        let mut holder = connect(addr);
        holder
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("request must be written");
        read_until(&mut holder, r#"{"status":"ok"}"#);
        holder
    })
    .await
    .expect("client task must not panic");

    // Grace is 30 s; `stop` requires completion well before that.
    server.stop().await;
    drop(holder);
}

/// A streamed body of `count` numbered lines, one every `pause`.
fn counting(count: u8, pause: Duration) -> Response {
    Response::stream(StatusCode::OK, 0_u8, move |sent| async move {
        if sent == count {
            return None;
        }
        tokio::time::sleep(pause).await;
        Some((Bytes::from(format!("line {sent}\n")), sent + 1))
    })
}

#[tokio::test]
async fn streamed_bodies_are_read_chunk_by_chunk_in_process() {
    let mut buffered = Response::new(StatusCode::OK, "whole");
    assert!(!buffered.is_stream());
    assert_eq!(buffered.next_chunk().await.as_deref(), Some(&b"whole"[..]));
    assert_eq!(buffered.next_chunk().await, None);

    let mut streamed = counting(3, Duration::ZERO);
    assert!(streamed.is_stream());
    assert!(streamed.body().is_empty());
    assert!(format!("{streamed:?}").contains("<stream>"));
    let mut chunks = Vec::new();
    while let Some(chunk) = streamed.next_chunk().await {
        chunks.push(String::from_utf8_lossy(&chunk).into_owned());
    }
    assert_eq!(chunks, ["line 0\n", "line 1\n", "line 2\n"]);
    assert_eq!(streamed.next_chunk().await, None);
}

#[tokio::test]
async fn empty_chunks_are_skipped() {
    let mut response = Response::stream(StatusCode::OK, 0_u8, |step| async move {
        match step {
            0 | 2 => Some((Bytes::new(), step + 1)),
            1 => Some((Bytes::from_static(b"x"), step + 1)),
            _ => None,
        }
    });

    assert_eq!(response.next_chunk().await.as_deref(), Some(&b"x"[..]));
    assert_eq!(response.next_chunk().await, None);
}

#[test]
fn a_step_that_only_returns_empty_chunks_does_not_starve_other_tasks() {
    // On a single-threaded runtime, a body that spun without yielding
    // would keep the other task from ever running. The verdict comes
    // through a channel so that a regression fails instead of hanging.
    let (verdict, outcome) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime must build");
        runtime.block_on(async {
            let mut empty =
                Response::stream(StatusCode::OK, (), |()| async { Some((Bytes::new(), ())) });
            let reader = tokio::spawn(async move { empty.next_chunk().await });
            let other = tokio::spawn(async { "ran" });
            let ran = other.await.expect("the other task must not panic");
            reader.abort();
            let _ = verdict.send(ran);
        });
    });

    assert_eq!(
        outcome.recv_timeout(CLIENT_TIMEOUT),
        Ok("ran"),
        "an always-empty body must yield to other tasks"
    );
}

#[tokio::test]
async fn streamed_bodies_are_sent_with_chunked_encoding() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/count", |_| async {
            counting(3, Duration::from_millis(20))
        })
        .expect("route must register");
    let server = TestServer::start(test_config(Duration::from_secs(10)), router).await;

    let response = exchange(
        server.addr,
        "GET /count HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "{response}"
    );
    for line in ["line 0", "line 1", "line 2"] {
        assert!(response.contains(line), "{response}");
    }
    server.stop().await;
}

/// Records whether the value was dropped.
struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A router whose `GET /flood` streams an endless body far larger than
/// the socket buffers; `dropped` turns true when the body is dropped.
fn flood(dropped: &Arc<std::sync::atomic::AtomicBool>) -> Router {
    let mut router = Router::new();
    let flag = Arc::clone(dropped);
    router
        .route(Method::GET, "/flood", move |_| {
            let guard = DropFlag(Arc::clone(&flag));
            async move {
                Response::stream(StatusCode::OK, guard, |guard| async move {
                    Some((Bytes::from(vec![b'x'; 64 * 1024]), guard))
                })
            }
        })
        .expect("route must register");
    router
}

#[test]
fn the_write_stall_guard_is_the_request_timeout_but_at_least_30_seconds() {
    assert_eq!(MIN_WRITE_STALL_TIMEOUT, Duration::from_secs(30));
    for (request_timeout, guard) in [(1, 30), (29, 30), (30, 30), (31, 31), (120, 120)] {
        assert_eq!(
            write_stall_timeout(
                Duration::from_secs(request_timeout),
                MIN_WRITE_STALL_TIMEOUT
            ),
            Duration::from_secs(guard),
            "{request_timeout}"
        );
    }
}

#[tokio::test]
async fn a_short_request_timeout_does_not_disconnect_a_slow_reader() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The request timeout is far shorter than the pause below, but the
    // stalled-write guard keeps its 30 second floor.
    let server = TestServer::start(test_config(Duration::from_millis(100)), flood(&dropped)).await;
    let addr = server.addr;

    let (paused, received) = tokio::task::spawn_blocking(move || {
        let mut stream = connect(addr);
        stream
            .write_all(b"GET /flood HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("request must be written");
        // Not reading: the socket buffers fill and the server's writes
        // stall for ten request timeouts.
        std::thread::sleep(Duration::from_secs(1));
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut received = 0;
        while received < 4 << 20 {
            let read = stream.read(&mut buffer).expect("the stream must stay open");
            assert_ne!(read, 0, "the server must not end the stream");
            received += read;
        }
        (stream, received)
    })
    .await
    .expect("client task must not panic");

    assert!(received >= 4 << 20);
    assert!(
        !dropped.load(std::sync::atomic::Ordering::SeqCst),
        "a slow but live reader must keep its stream"
    );
    drop(paused);
    server.stop().await;
}

#[tokio::test]
async fn a_client_that_stops_reading_a_stream_is_disconnected() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut server = Server::new(test_config(Duration::from_millis(300)), flood(&dropped));
    // Without the 30 second floor, so the test does not wait for it.
    server.min_write_stall = Duration::ZERO;
    let server = TestServer::start_server(server).await;
    let addr = server.addr;

    // The client sends its request and never reads the response.
    let client = tokio::task::spawn_blocking(move || {
        let mut stream = connect(addr);
        stream
            .write_all(b"GET /flood HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("request must be written");
        stream
    })
    .await
    .expect("client task must not panic");

    let deadline = tokio::time::Instant::now() + CLIENT_TIMEOUT;
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a stalled stream must be dropped"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(client);
    server.stop().await;
}

/// The response head (status line and headers) and everything after it.
fn split_head(response: &str) -> (&str, &str) {
    response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header block in {response:?}"))
}

#[tokio::test]
async fn head_answers_with_the_get_headers_and_no_body() {
    let mut router = test_router();
    router
        .route(Method::GET, "/ticks", |_| async {
            counting(3, Duration::from_millis(1))
        })
        .expect("stream route must register")
        .route(Method::HEAD, "/empty", |_| async {
            Response::empty(StatusCode::OK)
        })
        .expect("HEAD route must register");
    let server = TestServer::start(test_config(Duration::from_secs(10)), router).await;

    let response = exchange(
        server.addr,
        "HEAD /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (head, body) = split_head(&response);
    let lower = head.to_ascii_lowercase();
    assert!(head.starts_with("HTTP/1.1 200 "), "{response}");
    // The length of the GET body ({"status":"ok"}), which is not sent.
    assert!(lower.contains("content-length: 15"), "{response}");
    assert!(
        lower.contains("content-type: application/json"),
        "{response}"
    );
    assert!(lower.contains("x-request-id: "), "{response}");
    assert_eq!(body, "", "{response}");

    // A streamed GET: headers only, no length, and the stream is not sent.
    let response = exchange(
        server.addr,
        "HEAD /ticks HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (head, body) = split_head(&response);
    assert!(head.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(
        !head.to_ascii_lowercase().contains("content-length"),
        "{response}"
    );
    assert_eq!(body, "", "{response}");

    // An empty body adds no length (hyper adds none for HEAD either).
    let response = exchange(
        server.addr,
        "HEAD /empty HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (head, body) = split_head(&response);
    assert!(head.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(
        !head.to_ascii_lowercase().contains("content-length"),
        "{response}"
    );
    assert_eq!(body, "", "{response}");

    // A route without GET still refuses HEAD and lists what it serves.
    let response = exchange(
        server.addr,
        "HEAD /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (head, body) = split_head(&response);
    assert!(head.starts_with("HTTP/1.1 405 "), "{response}");
    assert!(
        head.to_ascii_lowercase().contains("allow: post"),
        "{response}"
    );
    assert_eq!(body, "", "{response}");
    server.stop().await;
}

#[tokio::test]
async fn a_keep_alive_connection_continues_cleanly_after_head() {
    let server = TestServer::start(test_config(Duration::from_secs(10)), test_router()).await;
    let addr = server.addr;

    tokio::task::spawn_blocking(move || {
        let mut stream = connect(addr);
        stream
            .write_all(
                b"HEAD /health/live HTTP/1.1\r\nHost: localhost\r\n\r\n\
                  GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .expect("requests must be written");
        let mut received = Vec::new();
        stream
            .read_to_end(&mut received)
            .expect("server must close after the second response");
        let text = String::from_utf8_lossy(&received);
        // Two responses, and only the second carries the body.
        assert_eq!(text.matches("HTTP/1.1 200 ").count(), 2, "{text}");
        assert_eq!(text.matches(r#"{"status":"live"}"#).count(), 1, "{text}");
        assert!(text.ends_with(r#"{"status":"live"}"#), "{text}");
    })
    .await
    .expect("client task must not panic");
    server.stop().await;
}

/// A log sink for one test.
#[derive(Clone, Default)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut log) = self.0.lock() {
            log.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    fn text(&self) -> String {
        self.0
            .lock()
            .map(|log| String::from_utf8_lossy(&log).into_owned())
            .unwrap_or_default()
    }
}

fn panicking_router() -> Router {
    let mut router = test_router();
    router
        .route(Method::GET, "/panic", |_| async {
            panic!("secret-detail 4242");
        })
        .expect("panic route must register")
        .route(Method::POST, "/panic", |_| async {
            panic!("with a body");
        })
        .expect("panic route must register");
    router
}

// Current-thread runtime: the server's tasks run on this thread, so the
// test's log subscriber sees them.
#[tokio::test(flavor = "current_thread")]
async fn a_handler_panic_is_a_500_with_the_request_id_and_is_logged() {
    let log = CapturedLog::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::start(test_config(Duration::from_secs(10)), panicking_router()).await;
    let addr = server.addr;

    let text = tokio::task::spawn_blocking(move || {
        let mut stream = connect(addr);
        // The connection survives the panic: a second request follows.
        stream
            .write_all(
                b"GET /panic HTTP/1.1\r\nHost: localhost\r\n\r\n\
                  GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .expect("requests must be written");
        let mut received = Vec::new();
        stream
            .read_to_end(&mut received)
            .expect("server must answer both requests");
        String::from_utf8_lossy(&received).into_owned()
    })
    .await
    .expect("client task must not panic");

    let (first, rest) = text
        .split_once("HTTP/1.1 200 ")
        .unwrap_or_else(|| panic!("no second response in {text:?}"));
    assert!(first.starts_with("HTTP/1.1 500 "), "{text}");
    let lower = first.to_ascii_lowercase();
    assert!(
        lower.contains("content-type: text/plain; charset=utf-8"),
        "{text}"
    );
    assert!(lower.contains("x-content-type-options: nosniff"), "{text}");
    assert!(first.ends_with("\r\n\r\ninternal server error"), "{text}");
    assert!(
        !text.contains("secret-detail"),
        "the panic message leaked: {text}"
    );
    assert!(rest.ends_with(r#"{"status":"ok"}"#), "{text}");
    let request_id = lower
        .lines()
        .find_map(|line| line.strip_prefix("x-request-id: "))
        .unwrap_or_else(|| panic!("no x-request-id in {text:?}"))
        .trim()
        .to_owned();
    server.stop().await;

    let logged = log.text();
    let line = logged
        .lines()
        .find(|line| line.contains("request handler panicked"))
        .unwrap_or_else(|| panic!("panic not logged: {logged}"));
    // Logged with the request id, the message kept server-side.
    assert!(line.contains(&request_id), "{line}");
    assert!(line.contains("secret-detail 4242"), "{line}");
    assert!(line.contains("ERROR"), "{line}");
}

/// A server with a 64-byte body limit and a `budget`-byte in-flight body
/// budget, and a handle on that budget.
async fn budget_server(budget: usize, request_timeout: Duration) -> (TestServer, Arc<Semaphore>) {
    let mut config = test_config(request_timeout);
    config.max_in_flight_body_bytes = budget;
    let server = Server::new(config, panicking_router());
    let available = Arc::clone(&server.app.body_budget.0);
    (TestServer::start_server(server).await, available)
}

/// Waits until the budget has exactly `bytes` free.
async fn budget_reaches(budget: &Semaphore, bytes: usize) {
    let deadline = tokio::time::Instant::now() + CLIENT_TIMEOUT;
    while budget.available_permits() != bytes {
        assert!(
            tokio::time::Instant::now() < deadline,
            "budget stuck at {} free, expected {bytes}",
            budget.available_permits()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn exchange_owned(addr: SocketAddr, raw: String) -> String {
    tokio::task::spawn_blocking(move || exchange_blocking(addr, raw.as_bytes()))
        .await
        .expect("client task must not panic")
}

#[tokio::test]
async fn content_length_bodies_are_charged_as_they_arrive_and_refused_with_503() {
    let (server, budget) = budget_server(64, Duration::from_secs(10)).await;
    let addr = server.addr;

    // A trickling upload declares 60 bytes and holds the 40 it sent.
    let mut holder = tokio::task::spawn_blocking(move || {
        let mut stream = connect(addr);
        stream
            .write_all(
                format!(
                    "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 60\r\n\
                     Connection: close\r\n\r\n{}",
                    "a".repeat(40)
                )
                .as_bytes(),
            )
            .expect("partial body must be written");
        stream
    })
    .await
    .expect("client task must not panic");
    budget_reaches(&budget, 24).await;

    // 40 more bytes do not fit in the 24 left (one write, so the server
    // has read them all when it refuses).
    let refused = exchange_owned(
        addr,
        format!(
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 40\r\n\
             Connection: close\r\n\r\n{}",
            "b".repeat(40)
        ),
    )
    .await;
    let lower = refused.to_ascii_lowercase();
    assert!(refused.starts_with("HTTP/1.1 503 "), "{refused}");
    assert!(lower.contains("retry-after: 5\r\n"), "{refused}");
    assert!(lower.contains("x-request-id: "), "{refused}");
    assert!(refused.ends_with("server busy, retry later"), "{refused}");
    budget_reaches(&budget, 24).await;

    // A body that fits is served meanwhile.
    let fits = exchange_owned(
        addr,
        format!(
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 20\r\n\
             Connection: close\r\n\r\n{}",
            "x".repeat(20)
        ),
    )
    .await;
    assert!(fits.starts_with("HTTP/1.1 200 "), "{fits}");

    // The first upload completes and returns its share.
    let completed = tokio::task::spawn_blocking(move || {
        holder
            .write_all("c".repeat(20).as_bytes())
            .expect("rest of the body must be written");
        let mut response = Vec::new();
        holder
            .read_to_end(&mut response)
            .expect("response must arrive");
        String::from_utf8_lossy(&response).into_owned()
    })
    .await
    .expect("client task must not panic");
    assert!(completed.starts_with("HTTP/1.1 200 "), "{completed}");
    assert!(
        completed.ends_with(&format!("{}{}", "a".repeat(40), "c".repeat(20))),
        "{completed}"
    );
    budget_reaches(&budget, 64).await;
    server.stop().await;
}

#[tokio::test]
async fn a_declared_but_unsent_body_holds_no_budget() {
    let (server, budget) = budget_server(64, Duration::from_secs(10)).await;
    let addr = server.addr;

    // Several clients declare the largest body and send none of it.
    let holders = tokio::task::spawn_blocking(move || {
        (0..4)
            .map(|_| {
                let mut stream = connect(addr);
                stream
                    .write_all(
                        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 64\r\n\
                          Connection: close\r\n\r\n",
                    )
                    .expect("headers must be written");
                stream
            })
            .collect::<Vec<_>>()
    })
    .await
    .expect("client task must not panic");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(budget.available_permits(), 64);

    // A small form post and a body of the whole budget still succeed.
    for size in [1, 64] {
        let response = exchange_owned(
            addr,
            format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {size}\r\n\
                 Connection: close\r\n\r\n{}",
                "y".repeat(size)
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 "), "{size}: {response}");
    }
    drop(holders);
    budget_reaches(&budget, 64).await;
    server.stop().await;
}

#[tokio::test]
async fn trickling_chunked_uploads_are_charged_as_they_arrive_and_released_on_disconnect() {
    let (server, budget) = budget_server(64, Duration::from_secs(10)).await;
    let addr = server.addr;

    // A chunked upload sends 40 bytes and stalls without ending the body.
    let holder = tokio::task::spawn_blocking(move || {
        let mut stream = connect(addr);
        stream
            .write_all(
                format!(
                    "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\
                     Connection: close\r\n\r\n28\r\n{}\r\n",
                    "a".repeat(40)
                )
                .as_bytes(),
            )
            .expect("first chunk must be written");
        stream
    })
    .await
    .expect("client task must not panic");
    budget_reaches(&budget, 24).await;

    // A second chunked upload fits with its first 16 bytes, not its next 16
    // (one write, so the server has read everything when it refuses).
    let refused = exchange_owned(
        addr,
        format!(
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n10\r\n{0}\r\n10\r\n{0}\r\n",
            "b".repeat(16)
        ),
    )
    .await;
    assert!(refused.starts_with("HTTP/1.1 503 "), "{refused}");
    assert!(
        refused.to_ascii_lowercase().contains("retry-after: 5\r\n"),
        "{refused}"
    );
    budget_reaches(&budget, 24).await;

    // The stalled client leaves: its share comes back.
    drop(holder);
    budget_reaches(&budget, 64).await;
    let served = exchange_owned(
        addr,
        format!(
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n28\r\n{}\r\n0\r\n\r\n",
            "d".repeat(40)
        ),
    )
    .await;
    assert!(served.starts_with("HTTP/1.1 200 "), "{served}");
    assert!(served.ends_with(&"d".repeat(40)), "{served}");
    budget_reaches(&budget, 64).await;
    server.stop().await;
}

#[tokio::test]
async fn the_body_budget_is_returned_after_a_panic_a_timeout_and_a_rejection() {
    let (server, budget) = budget_server(64, Duration::from_millis(300)).await;
    let addr = server.addr;

    let panicked = exchange(
        addr,
        "POST /panic HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\
         Connection: close\r\n\r\n0123456789",
    )
    .await;
    assert!(panicked.starts_with("HTTP/1.1 500 "), "{panicked}");
    budget_reaches(&budget, 64).await;

    // Declares 40 bytes, sends 2, stalls past the request timeout.
    let timed_out = exchange(
        addr,
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 40\r\n\
         Connection: close\r\n\r\nab",
    )
    .await;
    assert!(timed_out.starts_with("HTTP/1.1 504 "), "{timed_out}");
    budget_reaches(&budget, 64).await;

    // Over the body limit after 40 bytes were charged.
    let too_large = exchange_owned(
        addr,
        format!(
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n28\r\n{0}\r\n28\r\n{0}\r\n",
            "e".repeat(40)
        ),
    )
    .await;
    assert!(too_large.starts_with("HTTP/1.1 413 "), "{too_large}");
    budget_reaches(&budget, 64).await;
    server.stop().await;
}

#[test]
fn a_charge_larger_than_u32_is_taken_in_steps() {
    let budget = BodyBudget::new(usize::MAX);
    let mut held = BodyCharge::default();
    let big = (u32::MAX as usize) * 2 + 7;
    assert!(budget.charge(&mut held, big));
    assert!(budget.charge(&mut held, 3));
    assert_eq!(held.0.len(), 3);
    assert_eq!(
        budget.0.available_permits(),
        Semaphore::MAX_PERMITS - big - 3
    );
    drop(held);
    assert_eq!(budget.0.available_permits(), Semaphore::MAX_PERMITS);

    let small = BodyBudget::new(10);
    let mut held = BodyCharge::default();
    assert!(small.charge(&mut held, 0));
    assert!(held.0.is_empty());
    assert!(small.charge(&mut held, 6));
    assert!(small.charge(&mut held, 1));
    assert_eq!(held.0.len(), 1, "small charges merge into one permit");
    assert!(!small.charge(&mut held, 4));
    assert_eq!(small.0.available_permits(), 3);
    drop(held);
    assert_eq!(small.0.available_permits(), 10);
}

/// Moves `/v2/...` to `/...` in its rewrite step and records it.
struct RewriteV2;

impl Middleware for RewriteV2 {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        next.run(request)
    }

    fn rewrite(&self, request: &mut Request) {
        if let Some(rest) = request.uri().path().strip_prefix("/v2/") {
            let uri = format!("/{rest}").parse().expect("valid rewritten URI");
            request.set_uri(uri);
            request.extensions_mut().insert("rewritten");
        }
    }
}

/// A path guard: refuses `/admin` and everything under it.
struct DenyAdmin;

impl Middleware for DenyAdmin {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        let path = request.uri().path();
        if path == "/admin" || path.starts_with("/admin/") {
            return Box::pin(async { Response::empty(StatusCode::FORBIDDEN) });
        }
        next.run(request)
    }
}

#[tokio::test]
async fn rewrites_run_before_every_layer_wherever_they_are_registered() {
    let mut router = Router::new();
    router
        .route(Method::GET, "/admin", |_| async {
            Response::new(StatusCode::OK, "admin")
        })
        .expect("admin route must register")
        .route(Method::GET, "/posts", |request: Request| async move {
            let rewritten = request.extensions().get::<&str>().copied();
            Response::new(StatusCode::OK, rewritten.unwrap_or("as sent"))
        })
        .expect("posts route must register");
    // The guard comes first; the rewriting layer after it.
    router.layer(DenyAdmin).layer(RewriteV2);

    for (target, status, body) in [
        ("/admin", StatusCode::FORBIDDEN, ""),
        ("/v2/admin", StatusCode::FORBIDDEN, ""),
        ("/v2/posts", StatusCode::OK, "rewritten"),
        ("/posts", StatusCode::OK, "as sent"),
    ] {
        let response = router.dispatch(request(Method::GET, target)).await;
        assert_eq!(response.status, status, "{target}");
        assert_eq!(response.body(), body, "{target}");
    }
}
