use std::{
    future::Future,
    net::{IpAddr, Ipv6Addr},
    pin::Pin,
    sync::Arc,
};

use forge_http::{
    BoxResponseFuture, Handler, HeaderValue, Method, Middleware, Next, Request, Response,
    StatusCode, header,
};
use serde_json::json;
use tracing::warn;

use crate::{Decision, Limit, LimitError, Limiter, key, retry_after_seconds};

/// Identifies the client of a request: a remote address, a session or user ID,
/// an API key ID, or a combination (see [`join`]).
///
/// The value is hashed by [`key`] before it reaches the limiter, so it may be
/// personal data, but it **must not be chosen by the client**: a header or
/// form field the client sets freely lets one client use unlimited keys. An
/// empty result puts the request in the shared bucket of unidentified clients
/// (it is limited, never exempt).
pub type KeyFn = Arc<dyn Fn(&Request) -> String + Send + Sync>;

/// Keys by the client address the HTTP server resolved
/// ([`Request::client_ip`], ADR 0032): the TCP peer, unless the peer is a
/// trusted proxy listed in `FORGE_HTTP_TRUSTED_PROXIES`, in which case it is
/// the first untrusted `X-Forwarded-For` entry read from the right. A client
/// that reaches the application directly cannot choose it; with no trusted
/// proxies configured it equals [`by_remote_addr`]. IPv4 addresses (and
/// IPv4-mapped IPv6 addresses) are used whole; IPv6 addresses are reduced to
/// their `/64` prefix, because one subscriber routinely controls a whole
/// `/64` and could otherwise rotate through addresses.
///
/// It is the key to use for per-client limits, including behind a reverse
/// proxy once `FORGE_HTTP_TRUSTED_PROXIES` lists it. `X-Real-IP` and
/// `Forwarded` are never read.
#[must_use]
pub fn by_client_ip(request: &Request) -> String {
    address_key(request.client_ip())
}

/// Keys by the peer address of the TCP connection, whatever the trusted
/// proxies (IPv6 reduced to its `/64` like [`by_client_ip`]). It never reads
/// a forwarding header. Behind a reverse proxy the peer is the proxy, so
/// every client shares one bucket; use [`by_client_ip`] for per-client
/// limits.
#[must_use]
pub fn by_remote_addr(request: &Request) -> String {
    address_key(request.remote_addr().ip())
}

/// An IPv4 address whole, an IPv6 address as its `/64`.
fn address_key(address: IpAddr) -> String {
    match address.to_canonical() {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            let prefix = u128::from(address) & (u128::MAX << 64);
            format!("{}/64", Ipv6Addr::from(prefix))
        }
    }
}

/// Combines several identifying values into one unambiguous key function
/// result (for example a user ID and a remote address).
#[must_use]
pub fn join(parts: &[&str]) -> String {
    key("join", parts)
}

/// Rate limit as an HTTP layer or per-route handler wrapper.
///
/// Requests are counted before the handler runs, whatever its outcome. Over
/// the limit the client gets `429`, `Retry-After` (whole seconds, rounded up)
/// and `Cache-Control: no-store`, as JSON or, when the client prefers HTML, a
/// minimal static page. If the limiter returns an error the request is
/// answered `503 rate_limit_unavailable` (**fail closed**: an attacker who can
/// make the limiter fail must not become unlimited), unless
/// [`RateLimit::fail_open`] was chosen for the route. Place it after
/// authentication when it keys by principal.
///
/// The scope names what the limit protects. Counters are identified by
/// (scope, window length, client key), so give distinct routes distinct
/// scopes when they must be counted separately and reuse a scope to share one
/// counter on purpose. `forge-http` layers run before routing, so unlike the
/// route-pattern default of other frameworks the scope is always explicit.
#[derive(Clone)]
pub struct RateLimit {
    inner: Arc<Inner>,
}

struct Inner {
    limiter: Arc<dyn Limiter>,
    key: KeyFn,
    limit: Limit,
    scope: String,
    fail_open: bool,
    methods: Option<Vec<Method>>,
}

impl std::fmt::Debug for RateLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RateLimit")
            .field("scope", &self.inner.scope)
            .field("limit", &self.inner.limit)
            .field("fail_open", &self.inner.fail_open)
            .finish_non_exhaustive()
    }
}

impl RateLimit {
    /// Builds the limit for `scope` (non-empty).
    pub fn new(
        limiter: Arc<dyn Limiter>,
        scope: impl Into<String>,
        limit: Limit,
        key: impl Fn(&Request) -> String + Send + Sync + 'static,
    ) -> Result<Self, LimitError> {
        let scope = scope.into();
        if scope.is_empty() {
            return Err(LimitError::Configuration(
                "rate limit scope must not be empty",
            ));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                limiter,
                key: Arc::new(key),
                limit,
                scope,
                fail_open: false,
                methods: None,
            }),
        })
    }

    fn modify(self, change: impl FnOnce(&mut Inner)) -> Self {
        let inner = &self.inner;
        let mut copy = Inner {
            limiter: Arc::clone(&inner.limiter),
            key: Arc::clone(&inner.key),
            limit: inner.limit,
            scope: inner.scope.clone(),
            fail_open: inner.fail_open,
            methods: inner.methods.clone(),
        };
        change(&mut copy);
        Self {
            inner: Arc::new(copy),
        }
    }

    /// Lets requests through when the limiter returns an error (for example
    /// the database is down), after logging it. The default is to fail closed
    /// with `503`.
    #[must_use]
    pub fn fail_open(self) -> Self {
        self.modify(|inner| inner.fail_open = true)
    }

    /// Counts only requests with these methods (Rails' `only: :create`);
    /// others pass uncounted. Useful for a layer registered for the whole
    /// application.
    #[must_use]
    pub fn only_methods(self, methods: impl IntoIterator<Item = Method>) -> Self {
        let methods: Vec<Method> = methods.into_iter().collect();
        self.modify(|inner| inner.methods = Some(methods))
    }

    /// Wraps one route handler, like `rate_limit ... only: :create`.
    pub fn wrap<H: Handler>(self, handler: H) -> RateLimited<H> {
        RateLimited {
            rate: self,
            handler: Arc::new(handler),
        }
    }

    /// Counts the request and returns the response that rejects it, or `None`
    /// when it may proceed.
    pub async fn check(&self, request: &Request) -> Option<Response> {
        let inner = &self.inner;
        if let Some(methods) = &inner.methods
            && !methods.contains(request.method())
        {
            return None;
        }
        let window = inner.limit.per().as_nanos().to_string();
        let client = (inner.key)(request);
        let store_key = key(&inner.scope, &[&window, &client]);
        match inner.limiter.allow(&store_key, inner.limit).await {
            Ok(decision) if decision.allowed => None,
            Ok(decision) => Some(limited(request, &decision)),
            Err(error) if inner.fail_open => {
                warn!(%error, "rate limiter unavailable; failing open");
                None
            }
            Err(error) => {
                warn!(%error, "rate limiter unavailable; failing closed");
                Some(unavailable(request))
            }
        }
    }
}

impl Middleware for RateLimit {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        Box::pin(async move {
            match self.check(&request).await {
                Some(rejection) => rejection,
                None => next.run(request).await,
            }
        })
    }
}

/// A handler protected by a [`RateLimit`] (see [`RateLimit::wrap`]).
pub struct RateLimited<H> {
    rate: RateLimit,
    handler: Arc<H>,
}

impl<H: Handler> Handler for RateLimited<H> {
    fn call(&self, request: Request) -> Pin<Box<dyn Future<Output = Response> + Send + 'static>> {
        let rate = self.rate.clone();
        let handler = Arc::clone(&self.handler);
        Box::pin(async move {
            match rate.check(&request).await {
                Some(rejection) => rejection,
                None => handler.call(request).await,
            }
        })
    }
}

const LIMITED_PAGE: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>Too many requests</title></head>\n<body><h1>Too many requests</h1><p>You have made too many requests. Please wait and try again.</p></body></html>\n";

fn limited(request: &Request, decision: &Decision) -> Response {
    let mut response = if prefers_html(request) {
        let mut page = Response::new(StatusCode::TOO_MANY_REQUESTS, LIMITED_PAGE);
        page.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        page
    } else {
        error_json(
            request,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many requests",
        )
    };
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from(retry_after_seconds(decision.retry_after)),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn unavailable(request: &Request) -> Response {
    let mut response = error_json(
        request,
        StatusCode::SERVICE_UNAVAILABLE,
        "rate_limit_unavailable",
        "the service is temporarily unavailable",
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error_json(request: &Request, status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "code": code,
            "message": message,
            "request_id": request.request_id().to_string(),
        }
    });
    Response::json(status, &body).unwrap_or_else(|_| Response::new(status, message.to_owned()))
}

/// Whether the client asks for a page: `text/html` (or XHTML) is listed and
/// not after `application/json` or a wildcard-only preference.
fn prefers_html(request: &Request) -> bool {
    let Some(accept) = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    for item in accept.split(',') {
        let media = item
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match media.as_str() {
            "text/html" | "application/xhtml+xml" => return true,
            "application/json" | "*/*" => return false,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use async_trait::async_trait;
    use forge_http::{HeaderMap, Router, TrustedProxies, Uri};
    use uuid::Uuid;

    use super::*;
    use crate::{MemoryLimiter, MemoryOptions};

    fn request(method: Method, path: &str, peer: &str, headers: &[(&str, &str)]) -> Request {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        Request::new_for_test(
            method,
            path.parse::<Uri>().expect("uri"),
            map,
            Default::default(),
            peer.parse::<SocketAddr>().expect("peer"),
            Uuid::now_v7(),
        )
    }

    fn memory() -> Arc<MemoryLimiter> {
        Arc::new(MemoryLimiter::new(MemoryOptions::default()).expect("limiter"))
    }

    fn two_per_minute() -> Limit {
        Limit::new(2, Duration::from_secs(60)).expect("limit")
    }

    fn router(rate: RateLimit, hits: Arc<AtomicUsize>) -> Router {
        let mut router = Router::new();
        router.layer(rate);
        router
            .route(Method::POST, "/comments", move |_request: Request| {
                let hits = Arc::clone(&hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    Response::new(StatusCode::CREATED, "ok")
                }
            })
            .expect("route");
        router
    }

    #[tokio::test]
    async fn limits_requests_and_answers_429_with_retry_after() {
        let hits = Arc::new(AtomicUsize::new(0));
        let rate = RateLimit::new(
            memory(),
            "comments.create",
            two_per_minute(),
            by_remote_addr,
        )
        .expect("rate limit");
        let router = router(rate, Arc::clone(&hits));
        let post = || request(Method::POST, "/comments", "203.0.113.7:5000", &[]);

        assert_eq!(router.dispatch(post()).await.status(), StatusCode::CREATED);
        assert_eq!(router.dispatch(post()).await.status(), StatusCode::CREATED);
        let limited = router.dispatch(post()).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry: u64 = limited.headers()[header::RETRY_AFTER]
            .to_str()
            .expect("ascii")
            .parse()
            .expect("integer");
        assert!((1..=60).contains(&retry), "Retry-After {retry}");
        assert_eq!(limited.headers()[header::CACHE_CONTROL], "no-store");
        let body: serde_json::Value = serde_json::from_slice(limited.body()).expect("json body");
        assert_eq!(body["error"]["code"], "rate_limited");
        assert!(body["error"]["request_id"].is_string());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the handler never saw the limited request"
        );

        // Another client is unaffected.
        let other = request(Method::POST, "/comments", "203.0.113.8:5000", &[]);
        assert_eq!(router.dispatch(other).await.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn browsers_get_a_static_html_page() {
        let rate = RateLimit::new(
            memory(),
            "s",
            Limit::new(1, Duration::from_secs(60)).expect("limit"),
            by_remote_addr,
        )
        .expect("rate limit");
        let router = router(rate, Arc::new(AtomicUsize::new(0)));
        let get = |accept| {
            request(
                Method::POST,
                "/comments",
                "203.0.113.7:1",
                &[("accept", accept)],
            )
        };
        router.dispatch(get("text/html")).await;
        let page = router
            .dispatch(get("text/html,application/xhtml+xml,*/*;q=0.8"))
            .await;
        assert_eq!(page.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            page.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert!(page.headers().contains_key(header::RETRY_AFTER));
        assert_eq!(page.headers()[header::CACHE_CONTROL], "no-store");
        let json = router.dispatch(get("application/json, text/html")).await;
        assert_eq!(
            json.headers()[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn forwarded_headers_cannot_buy_a_fresh_bucket() {
        let hits = Arc::new(AtomicUsize::new(0));
        let rate =
            RateLimit::new(memory(), "s", two_per_minute(), by_remote_addr).expect("rate limit");
        let router = router(rate, Arc::clone(&hits));
        let mut statuses = Vec::new();
        for i in 0..5 {
            let spoof = format!("198.51.100.{i}");
            let response = router
                .dispatch(request(
                    Method::POST,
                    "/comments",
                    "203.0.113.7:5000",
                    &[
                        ("x-forwarded-for", &spoof),
                        ("x-real-ip", &spoof),
                        ("forwarded", &format!("for={spoof}")),
                    ],
                ))
                .await;
            statuses.push(response.status());
        }
        assert_eq!(
            statuses,
            vec![
                StatusCode::CREATED,
                StatusCode::CREATED,
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::TOO_MANY_REQUESTS,
            ]
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    fn behind_proxy(peer: &str, forwarded_for: &str) -> Request {
        let mut request = request(
            Method::POST,
            "/comments",
            peer,
            &[("x-forwarded-for", forwarded_for)],
        );
        trusted().apply(&mut request);
        request
    }

    fn trusted() -> TrustedProxies {
        TrustedProxies::new(["172.18.0.0/16".parse().expect("valid network")])
    }

    #[tokio::test]
    async fn clients_behind_a_trusted_proxy_get_their_own_buckets() {
        let hits = Arc::new(AtomicUsize::new(0));
        let rate =
            RateLimit::new(memory(), "s", two_per_minute(), by_client_ip).expect("rate limit");
        let router = router(rate, Arc::clone(&hits));
        let status = |request| {
            let router = &router;
            async move { router.dispatch(request).await.status() }
        };
        for _ in 0..2 {
            assert_eq!(
                status(behind_proxy("172.18.0.2:5000", "203.0.113.9")).await,
                StatusCode::CREATED
            );
        }
        assert_eq!(
            status(behind_proxy("172.18.0.2:5000", "203.0.113.9")).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // Forged entries left of the proxy's own entry change nothing.
        for forged in ["1.2.3.4, 203.0.113.9", "172.18.0.9, 203.0.113.9"] {
            assert_eq!(
                status(behind_proxy("172.18.0.2:5000", forged)).await,
                StatusCode::TOO_MANY_REQUESTS,
                "{forged}"
            );
        }
        // Another client behind the same proxy has its own bucket.
        assert_eq!(
            status(behind_proxy("172.18.0.2:5000", "198.51.100.7")).await,
            StatusCode::CREATED
        );
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn direct_clients_cannot_choose_their_client_ip_bucket() {
        let rate =
            RateLimit::new(memory(), "s", two_per_minute(), by_client_ip).expect("rate limit");
        let router = router(rate, Arc::new(AtomicUsize::new(0)));
        let mut statuses = Vec::new();
        for i in 0..4 {
            // The peer is not a trusted proxy: its header is ignored.
            let response = router
                .dispatch(behind_proxy("203.0.113.7:5000", &format!("198.51.100.{i}")))
                .await;
            statuses.push(response.status());
        }
        assert_eq!(
            statuses,
            [
                StatusCode::CREATED,
                StatusCode::CREATED,
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::TOO_MANY_REQUESTS,
            ]
        );
    }

    #[test]
    fn client_ip_keys_group_ipv6_by_64_and_keep_the_peer_key_the_peer() {
        // Rotating inside one /64 behind the proxy is one client.
        assert_eq!(
            by_client_ip(&behind_proxy("172.18.0.2:5000", "2001:db8:1:2::1")),
            by_client_ip(&behind_proxy("172.18.0.2:5000", "2001:db8:1:2:ffff::9")),
        );
        assert_eq!(
            by_client_ip(&behind_proxy("172.18.0.2:5000", "2001:db8:1:2::1")),
            "2001:db8:1:2::/64"
        );
        assert_ne!(
            by_client_ip(&behind_proxy("172.18.0.2:5000", "2001:db8:1:2::1")),
            by_client_ip(&behind_proxy("172.18.0.2:5000", "2001:db8:1:3::1")),
        );
        // The peer key still means the peer, whatever the trusted proxies.
        assert_eq!(
            by_remote_addr(&behind_proxy("172.18.0.2:5000", "203.0.113.9")),
            "172.18.0.2"
        );
        // Without a resolver the client is the peer.
        let direct = request(
            Method::GET,
            "/",
            "[::ffff:203.0.113.7]:80",
            &[("x-forwarded-for", "198.51.100.1")],
        );
        assert_eq!(by_client_ip(&direct), "203.0.113.7");
    }

    #[test]
    fn remote_address_keys_use_the_peer_and_group_ipv6_by_64() {
        let key_of = |peer: &str| by_remote_addr(&request(Method::GET, "/", peer, &[]));
        assert_eq!(key_of("203.0.113.7:80"), "203.0.113.7");
        assert_eq!(key_of("[::ffff:203.0.113.7]:80"), "203.0.113.7");
        assert_eq!(
            key_of("[2001:db8:1:2:aaaa:bbbb:cccc:dddd]:80"),
            key_of("[2001:db8:1:2:1111:2222:3333:4444]:80"),
            "one /64 is one client"
        );
        assert_eq!(key_of("[2001:db8:1:2:aaaa::1]:80"), "2001:db8:1:2::/64");
        assert_ne!(
            key_of("[2001:db8:1:2::1]:80"),
            key_of("[2001:db8:1:3::1]:80")
        );
    }

    #[tokio::test]
    async fn empty_key_is_limited_not_exempt() {
        let rate =
            RateLimit::new(memory(), "s", two_per_minute(), |_| String::new()).expect("rate limit");
        let router = router(rate, Arc::new(AtomicUsize::new(0)));
        let mut codes = Vec::new();
        for peer in ["203.0.113.1:1", "203.0.113.2:1", "203.0.113.3:1"] {
            codes.push(
                router
                    .dispatch(request(Method::POST, "/comments", peer, &[]))
                    .await
                    .status(),
            );
        }
        assert_eq!(
            codes[2],
            StatusCode::TOO_MANY_REQUESTS,
            "unidentified clients share one bucket"
        );
    }

    struct Failing;

    #[async_trait]
    impl Limiter for Failing {
        async fn allow(&self, _key: &str, _limit: Limit) -> Result<Decision, LimitError> {
            Err(LimitError::Store("database password=hunter2 down".into()))
        }
    }

    #[tokio::test]
    async fn fails_closed_with_503_and_open_only_on_request() {
        let hits = Arc::new(AtomicUsize::new(0));
        let closed = RateLimit::new(Arc::new(Failing), "s", two_per_minute(), by_remote_addr)
            .expect("rate limit");
        let response = router(closed, Arc::clone(&hits))
            .dispatch(request(Method::POST, "/comments", "203.0.113.7:1", &[]))
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let text = String::from_utf8_lossy(response.body());
        assert!(text.contains("rate_limit_unavailable"));
        assert!(
            !text.contains("hunter2"),
            "the cause is never sent to the client"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        let open = RateLimit::new(Arc::new(Failing), "s", two_per_minute(), by_remote_addr)
            .expect("rate limit")
            .fail_open();
        let response = router(open, Arc::clone(&hits))
            .dispatch(request(Method::POST, "/comments", "203.0.113.7:1", &[]))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn scopes_separate_or_share_counters_and_methods_filter() {
        let limiter = memory();
        let limit = Limit::new(1, Duration::from_secs(60)).expect("limit");
        let a = RateLimit::new(limiter.clone(), "a", limit, by_remote_addr).expect("a");
        let b = RateLimit::new(limiter.clone(), "b", limit, by_remote_addr).expect("b");
        let a_again = RateLimit::new(limiter.clone(), "a", limit, by_remote_addr).expect("a2");
        let req = || request(Method::POST, "/x", "203.0.113.7:1", &[]);
        assert!(a.check(&req()).await.is_none());
        assert!(
            b.check(&req()).await.is_none(),
            "another scope counts separately"
        );
        assert!(
            a_again.check(&req()).await.is_some(),
            "the same scope shares one counter"
        );

        let only_post = RateLimit::new(limiter, "p", limit, by_remote_addr)
            .expect("p")
            .only_methods([Method::POST]);
        for _ in 0..5 {
            assert!(
                only_post
                    .check(&request(Method::GET, "/x", "203.0.113.7:1", &[]))
                    .await
                    .is_none()
            );
        }
        assert!(only_post.check(&req()).await.is_none());
        assert!(only_post.check(&req()).await.is_some());
    }

    #[tokio::test]
    async fn wrapped_handlers_are_limited_per_route() {
        let rate = RateLimit::new(
            memory(),
            "s",
            Limit::new(1, Duration::from_secs(60)).expect("limit"),
            by_remote_addr,
        )
        .expect("rate limit");
        let wrapped = rate.wrap(|_request: Request| async { Response::new(StatusCode::OK, "ok") });
        let call = || wrapped.call(request(Method::GET, "/", "203.0.113.7:1", &[]));
        assert_eq!(call().await.status(), StatusCode::OK);
        assert_eq!(call().await.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn empty_scope_is_refused() {
        assert!(matches!(
            RateLimit::new(memory(), "", two_per_minute(), by_remote_addr),
            Err(LimitError::Configuration(_))
        ));
    }

    #[tokio::test]
    async fn keys_are_hashed_before_the_limiter() {
        struct Recording(std::sync::Mutex<Vec<String>>);
        #[async_trait]
        impl Limiter for Recording {
            async fn allow(&self, key: &str, limit: Limit) -> Result<Decision, LimitError> {
                self.0.lock().expect("lock").push(key.to_owned());
                Ok(Decision::allowed(limit.n() - 1))
            }
        }
        let recording = Arc::new(Recording(std::sync::Mutex::new(Vec::new())));
        let rate = RateLimit::new(recording.clone(), "login", two_per_minute(), by_remote_addr)
            .expect("rate limit");
        assert!(
            rate.check(&request(Method::POST, "/", "203.0.113.7:1", &[]))
                .await
                .is_none()
        );
        let keys = recording.0.lock().expect("lock");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].len(), 64);
        assert!(!keys[0].contains("203.0.113"));
    }
}
