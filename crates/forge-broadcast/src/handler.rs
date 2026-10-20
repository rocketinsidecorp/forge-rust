//! The Server-Sent Events endpoint a `<turbo-stream-source>` connects to.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use forge_core::{ErrorCategory, ForgeError};
use forge_http::{HeaderValue, Method, Request, Response, RouteError, Router, StatusCode, header};
use forge_security::PrincipalId;
use forge_tenancy::{TenantContext, TenantId};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tracing::debug;

use crate::memory::SubscriptionEnd;
use crate::{BroadcastError, Broker, Channel, STREAM_PARAMETER, Signer, Subscription};

/// How often a comment line keeps an idle stream open through proxies.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(20);
/// How long one stream lives before the browser must reconnect, which
/// re-runs authentication and authorization.
pub const DEFAULT_MAX_CONNECTION_AGE: Duration = Duration::from_secs(30 * 60);
/// The reconnection delay clients are told to use.
pub const DEFAULT_RETRY: Duration = Duration::from_secs(3);
/// Open streams of one signed-in principal (or, without one, one client
/// address).
pub const DEFAULT_MAX_PER_PRINCIPAL: usize = 16;
/// Open streams of one tenant.
pub const DEFAULT_MAX_PER_TENANT: usize = 1_000;
/// Seconds a client is told to wait after a capacity refusal.
const RETRY_AFTER_SECONDS: &str = "5";

/// Who is subscribing, as the application's [`StreamAccess`] established it.
///
/// A tenant channel is only served to a viewer built from a
/// [`TenantContext`] of that very tenant, which cannot be constructed without
/// an authenticated principal, an active membership and a policy grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamViewer {
    tenant: Option<TenantId>,
    principal: Option<PrincipalId>,
}

impl StreamViewer {
    /// A member authorized in a tenant: may read that tenant's channels (and
    /// global ones).
    #[must_use]
    pub fn member(context: &TenantContext) -> Self {
        Self {
            tenant: Some(context.tenant_id()),
            principal: Some(context.principal_id()),
        }
    }

    /// A signed-in principal outside any tenant: global channels only.
    #[must_use]
    pub fn principal(principal: PrincipalId) -> Self {
        Self {
            tenant: None,
            principal: Some(principal),
        }
    }

    /// Nobody in particular: global channels only, counted against the
    /// per-client limit by client address. Use it only for channels whose
    /// content is public.
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            tenant: None,
            principal: None,
        }
    }

    /// The viewer's authorized tenant.
    #[must_use]
    pub fn tenant(&self) -> Option<TenantId> {
        self.tenant
    }

    /// The viewer's principal.
    #[must_use]
    pub fn principal_id(&self) -> Option<PrincipalId> {
        self.principal
    }
}

/// Why [`StreamAccess`] refused a subscription. The response never says
/// which check failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamDenied {
    /// No valid session: `401`.
    Unauthenticated,
    /// Authenticated but not allowed to read the channel: `403`.
    Forbidden,
    /// An identity store failed: `503`.
    Unavailable,
}

/// The application's decision about who may read a channel, made after the
/// token was verified.
///
/// Typical rules: authenticate the session; for a tenant channel, load the
/// caller's membership of [`Channel::tenant`] and require a read permission
/// (`<name>:read`) before returning [`StreamViewer::member`]. A stream is
/// never open to everyone by default: return [`StreamViewer::anonymous`]
/// only for public global channels.
#[async_trait]
pub trait StreamAccess: Send + Sync {
    /// Who `request` is, if it may read `channel`.
    async fn authorize(
        &self,
        request: &Request,
        channel: &Channel,
    ) -> Result<StreamViewer, StreamDenied>;
}

/// The client address a request is counted by when it has no principal.
pub type ClientAddress = Arc<dyn Fn(&Request) -> String + Send + Sync>;

/// The peer address of the TCP connection, IPv4 (and IPv4-mapped IPv6)
/// whole, IPv6 by its `/64` prefix (one subscriber's allocation), like the
/// rate limiter and the login throttle.
///
/// It never reads forwarded headers. Behind a reverse proxy every
/// anonymous viewer shares the proxy's slots; authenticated viewers are
/// counted by principal. Pass a resolver of trusted proxies to
/// [`EventStreams::client_address`] once the application has one.
#[must_use]
pub fn peer_address(request: &Request) -> String {
    match request.remote_addr().ip().to_canonical() {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            let prefix = u128::from(address) & (u128::MAX << 64);
            format!("{}/64", Ipv6Addr::from(prefix))
        }
    }
}

/// `GET /streams?stream=TOKEN`: the Server-Sent Events endpoint.
///
/// Checks, in order, all failing closed: exactly one `stream` parameter
/// whose token verifies (`403 stream_invalid`: forged, tampered and expired
/// tokens look alike); [`StreamAccess`] (`401`, `403 stream_forbidden`,
/// `503`); a tenant channel needs a viewer of the same tenant
/// (`403 stream_forbidden`); the per-principal and per-tenant caps
/// (`429 stream_limit` with `Retry-After`); broker capacity
/// (`503 stream_unavailable` with `Retry-After`).
///
/// The response is `text/event-stream` with `Cache-Control: no-store` and
/// `X-Accel-Buffering: no` (nginx), and no hop-by-hop headers, so it passes
/// HTTP/2 proxies unchanged. It starts with `retry: 3000` and `: connected`,
/// sends `: ping` every heartbeat, and ends when the client leaves, the
/// subscription is dropped (slow consumer) or closed (shutdown), or the
/// maximum connection age passes; the browser then reconnects, which runs
/// every check again. A client that stops reading is disconnected by the
/// HTTP server's stalled-write timeout.
pub struct EventStreams {
    broker: Arc<dyn Broker>,
    signer: Arc<Signer>,
    access: Arc<dyn StreamAccess>,
    heartbeat: Duration,
    max_connection_age: Option<Duration>,
    max_per_principal: Option<usize>,
    max_per_tenant: Option<usize>,
    client_address: ClientAddress,
    open: Arc<OpenStreams>,
}

impl fmt::Debug for EventStreams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventStreams")
            .field("heartbeat", &self.heartbeat)
            .field("max_connection_age", &self.max_connection_age)
            .field("max_per_principal", &self.max_per_principal)
            .field("max_per_tenant", &self.max_per_tenant)
            .finish_non_exhaustive()
    }
}

impl EventStreams {
    /// The endpoint over `broker`, verifying tokens with `signer` and asking
    /// `access` who may read a channel.
    #[must_use]
    pub fn new(
        broker: Arc<dyn Broker>,
        signer: Arc<Signer>,
        access: Arc<dyn StreamAccess>,
    ) -> Self {
        Self {
            broker,
            signer,
            access,
            heartbeat: DEFAULT_HEARTBEAT,
            max_connection_age: Some(DEFAULT_MAX_CONNECTION_AGE),
            max_per_principal: Some(DEFAULT_MAX_PER_PRINCIPAL),
            max_per_tenant: Some(DEFAULT_MAX_PER_TENANT),
            client_address: Arc::new(peer_address),
            open: Arc::new(OpenStreams::default()),
        }
    }

    /// Sets the heartbeat interval (positive).
    pub fn heartbeat(mut self, every: Duration) -> Result<Self, BroadcastError> {
        if every.is_zero() {
            return Err(BroadcastError::Configuration(
                "the heartbeat must be positive",
            ));
        }
        self.heartbeat = every;
        Ok(self)
    }

    /// Sets the maximum connection age; `None` disables it.
    #[must_use]
    pub fn max_connection_age(mut self, age: Option<Duration>) -> Self {
        self.max_connection_age = age;
        self
    }

    /// Sets the open-stream limit of one principal (or client address);
    /// `None` disables it.
    #[must_use]
    pub fn max_per_principal(mut self, limit: Option<usize>) -> Self {
        self.max_per_principal = limit;
        self
    }

    /// Sets the open-stream limit of one tenant; `None` disables it.
    #[must_use]
    pub fn max_per_tenant(mut self, limit: Option<usize>) -> Self {
        self.max_per_tenant = limit;
        self
    }

    /// Replaces how anonymous viewers are counted ([`peer_address`] by
    /// default), for example with a trusted-proxy resolver. The value must
    /// not be chosen by the client.
    #[must_use]
    pub fn client_address(mut self, resolve: ClientAddress) -> Self {
        self.client_address = resolve;
        self
    }

    /// Registers `GET path`.
    pub fn register(self, router: &mut Router, path: &str) -> Result<(), RouteError> {
        let streams = Arc::new(self);
        router.route(Method::GET, path, move |request: Request| {
            let streams = Arc::clone(&streams);
            async move { streams.serve(&request).await }
        })?;
        Ok(())
    }

    /// Answers one request (what [`EventStreams::register`] routes).
    pub async fn serve(&self, request: &Request) -> Response {
        let Some(token) = single_stream_parameter(request) else {
            return refusal(
                StatusCode::FORBIDDEN,
                "stream_invalid",
                "stream is invalid or expired",
            );
        };
        let channel = match self.signer.verify(token) {
            Ok(channel) => channel,
            Err(reason) => {
                debug!(reason = %reason, "event stream refused");
                return refusal(
                    StatusCode::FORBIDDEN,
                    "stream_invalid",
                    "stream is invalid or expired",
                );
            }
        };
        let viewer = match self.access.authorize(request, &channel).await {
            Ok(viewer) => viewer,
            Err(StreamDenied::Unauthenticated) => {
                return refusal(
                    StatusCode::UNAUTHORIZED,
                    "unauthenticated",
                    "authentication required",
                );
            }
            Err(StreamDenied::Forbidden) => return forbidden(),
            Err(StreamDenied::Unavailable) => return busy("service temporarily unavailable"),
        };
        // Fail-closed tenancy: a tenant channel only for a viewer authorized
        // in that tenant, whatever the access policy returned.
        if let Some(tenant) = channel.tenant()
            && viewer.tenant != Some(tenant)
        {
            return forbidden();
        }

        let principal_key = match viewer.principal {
            Some(principal) => format!("principal:{principal}"),
            None => format!("ip:{}", (self.client_address)(request)),
        };
        let Some(principal_slot) = self.open.acquire(principal_key, self.max_per_principal) else {
            return limited();
        };
        let tenant_slot = match viewer.tenant {
            Some(tenant) => {
                match self
                    .open
                    .acquire(format!("tenant:{tenant}"), self.max_per_tenant)
                {
                    Some(slot) => Some(slot),
                    None => return limited(),
                }
            }
            None => None,
        };
        let subscription = match self.broker.subscribe(&channel) {
            Ok(subscription) => subscription,
            Err(BroadcastError::TooManySubscribers | BroadcastError::Closed) => {
                return busy("too many open streams; try again shortly");
            }
            Err(error) => {
                debug!(error = %error, "event stream subscription failed");
                return busy("service temporarily unavailable");
            }
        };
        self.stream(Live {
            subscription,
            heartbeat: heartbeat(self.heartbeat),
            expires: self.max_connection_age.map(|age| Instant::now() + age),
            connected: false,
            _slots: (principal_slot, tenant_slot),
        })
    }

    fn stream(&self, live: Live) -> Response {
        let mut response = Response::stream(StatusCode::OK, live, |mut live| async move {
            let chunk = live.next_chunk().await?;
            Some((chunk, live))
        });
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
        response
    }
}

/// The state of one open stream, owned by its response body.
struct Live {
    subscription: Subscription,
    heartbeat: Interval,
    expires: Option<Instant>,
    connected: bool,
    /// Released when the stream ends.
    _slots: (Slot, Option<Slot>),
}

impl Live {
    async fn next_chunk(&mut self) -> Option<Bytes> {
        if !self.connected {
            self.connected = true;
            return Some(Bytes::from(format!(
                "retry: {}\n: connected\n\n",
                DEFAULT_RETRY.as_millis()
            )));
        }
        let expires = self.expires;
        let expired = async move {
            match expires {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            received = self.subscription.next() => match received {
                Ok(message) => Some(message.event()),
                Err(SubscriptionEnd::Closed) => None,
                Err(SubscriptionEnd::SlowConsumer) => {
                    debug!("event stream ended: subscriber too slow");
                    None
                }
            },
            _ = self.heartbeat.tick() => Some(Bytes::from_static(b": ping\n\n")),
            () = expired => None,
        }
    }
}

fn heartbeat(every: Duration) -> Interval {
    let mut interval = tokio::time::interval_at(Instant::now() + every, every);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

/// The value of the only `stream` query parameter, if there is exactly one.
fn single_stream_parameter(request: &Request) -> Option<&str> {
    let mut found = None;
    for pair in request.uri().query().unwrap_or_default().split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == STREAM_PARAMETER {
            if found.is_some() {
                return None;
            }
            found = Some(value);
        }
    }
    found
}

fn refusal(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    let category = match status {
        StatusCode::UNAUTHORIZED => ErrorCategory::Unauthenticated,
        StatusCode::FORBIDDEN => ErrorCategory::Forbidden,
        StatusCode::TOO_MANY_REQUESTS => ErrorCategory::RateLimited,
        _ => ErrorCategory::Infrastructure,
    };
    let failure = ForgeError::new(category, code, message);
    Response::json(status, &failure.report()).unwrap_or_else(|_| Response::empty(status))
}

fn forbidden() -> Response {
    refusal(
        StatusCode::FORBIDDEN,
        "stream_forbidden",
        "stream is not available to this account",
    )
}

fn limited() -> Response {
    let mut response = refusal(
        StatusCode::TOO_MANY_REQUESTS,
        "stream_limit",
        "too many open streams for this account",
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_static(RETRY_AFTER_SECONDS),
    );
    response
}

fn busy(message: &'static str) -> Response {
    let mut response = refusal(
        StatusCode::SERVICE_UNAVAILABLE,
        "stream_unavailable",
        message,
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_static(RETRY_AFTER_SECONDS),
    );
    response
}

/// Open streams per key. Entries are removed at zero, so the map only holds
/// keys with open streams, which the broker's subscriber cap bounds.
#[derive(Debug, Default)]
struct OpenStreams {
    counts: Mutex<HashMap<String, usize>>,
}

impl OpenStreams {
    /// Takes a slot of `key` unless `limit` is reached; `None` limit means
    /// unlimited (the slot is still released).
    fn acquire(self: &Arc<Self>, key: String, limit: Option<usize>) -> Option<Slot> {
        let mut counts = self.counts.lock().ok()?;
        let count = counts.entry(key.clone()).or_insert(0);
        if limit.is_some_and(|limit| *count >= limit) {
            if *count == 0 {
                counts.remove(&key);
            }
            return None;
        }
        *count += 1;
        Some(Slot {
            key,
            open: Arc::clone(self),
        })
    }
}

/// One taken slot, released on drop.
struct Slot {
    key: String,
    open: Arc<OpenStreams>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.open.counts.lock()
            && let Some(count) = counts.get_mut(&self.key)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use forge_auth::{Session, UnixTimestamp};
    use forge_core::{Cancellation, Uuid};
    use forge_http::HeaderMap;
    use forge_security::{Permission, RbacPolicy, Role};
    use forge_tenancy::Membership;

    use super::*;
    use crate::{MemoryBroker, MemoryOptions, Message};

    const KEY: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    /// Stands in for the application's authentication and permission check:
    /// `x-test-tenant` makes the caller an authorized member of that tenant,
    /// `x-test-principal` a signed-in principal, `x-test-anonymous` nobody in
    /// particular; without them the caller is signed out. Only the `posts`
    /// channel may be read.
    struct HeaderAccess;

    fn member_of(tenant: TenantId) -> TenantContext {
        let principal = PrincipalId::new();
        let reader = Role::new("reader").expect("valid role");
        let permission = Permission::new("posts:read").expect("valid permission");
        let mut policy = RbacPolicy::new();
        policy.allow(reader.clone(), permission.clone());
        let authenticated = Session::new(
            principal,
            UnixTimestamp::from_secs(10),
            UnixTimestamp::from_secs(20),
        )
        .expect("valid session")
        .authenticate(UnixTimestamp::from_secs(11))
        .expect("authenticates");
        let membership = Membership::active(tenant, principal, vec![reader]);
        TenantContext::authorize(authenticated, &membership, &policy, &permission)
            .expect("authorized")
    }

    #[async_trait]
    impl StreamAccess for HeaderAccess {
        async fn authorize(
            &self,
            request: &Request,
            channel: &Channel,
        ) -> Result<StreamViewer, StreamDenied> {
            let header = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let viewer = if let Some(tenant) = header("x-test-tenant") {
                let tenant = tenant.parse().map_err(|_| StreamDenied::Forbidden)?;
                StreamViewer::member(&member_of(tenant))
            } else if let Some(principal) = header("x-test-principal") {
                StreamViewer::principal(principal.parse().map_err(|_| StreamDenied::Forbidden)?)
            } else if header("x-test-anonymous").is_some() {
                StreamViewer::anonymous()
            } else if header("x-test-unavailable").is_some() {
                return Err(StreamDenied::Unavailable);
            } else {
                return Err(StreamDenied::Unauthenticated);
            };
            if channel.name() == "posts" {
                Ok(viewer)
            } else {
                Err(StreamDenied::Forbidden)
            }
        }
    }

    struct Fixture {
        router: Router,
        broker: MemoryBroker,
        signer: Arc<Signer>,
        shutdown: Cancellation,
    }

    fn fixture(configure: impl FnOnce(EventStreams) -> EventStreams) -> Fixture {
        let shutdown = Cancellation::new();
        let broker = MemoryBroker::new(MemoryOptions::default(), &shutdown).expect("broker");
        let signer = Arc::new(Signer::new(KEY, &[]).expect("signer"));
        let streams = EventStreams::new(
            Arc::new(broker.clone()),
            Arc::clone(&signer),
            Arc::new(HeaderAccess),
        );
        let mut router = Router::new();
        configure(streams)
            .register(&mut router, "/streams")
            .expect("registers");
        Fixture {
            router,
            broker,
            signer,
            shutdown,
        }
    }

    impl Fixture {
        fn url(&self, channel: &Channel) -> String {
            self.signer.source_url("/streams", channel).expect("signs")
        }

        async fn get(&self, uri: &str, headers: &[(&str, String)], peer: &str) -> Response {
            let mut map = HeaderMap::new();
            for (name, value) in headers {
                map.insert(
                    forge_http::header::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                    HeaderValue::from_str(value).expect("value"),
                );
            }
            let request = Request::new_for_test(
                Method::GET,
                uri.parse().expect("uri"),
                map,
                Bytes::new(),
                peer.parse::<SocketAddr>().expect("peer"),
                Uuid::now_v7(),
            );
            self.router.dispatch(request).await
        }

        async fn member(&self, uri: &str, tenant: TenantId) -> Response {
            self.get(uri, &[("x-test-tenant", tenant.to_string())], "127.0.0.1:1")
                .await
        }
    }

    async fn chunk(response: &mut Response) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(5), response.next_chunk())
            .await
            .expect("the stream must answer")
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn code(response: &Response) -> String {
        let report: serde_json::Value =
            serde_json::from_slice(response.body()).expect("a JSON report");
        report["code"].as_str().unwrap_or_default().to_owned()
    }

    #[tokio::test]
    async fn a_member_receives_the_tenant_channel_as_server_sent_events() {
        let fixture = fixture(|streams| streams);
        let tenant = TenantId::new();
        let channel = Channel::for_tenant(tenant, "posts").expect("valid");

        let mut response = fixture.member(&fixture.url(&channel), tenant).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.is_stream());
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(
            header("content-type").as_deref(),
            Some("text/event-stream; charset=utf-8")
        );
        assert_eq!(header("cache-control").as_deref(), Some("no-store"));
        assert_eq!(header("x-accel-buffering").as_deref(), Some("no"));
        // No hop-by-hop headers: HTTP/2 proxies forbid them.
        for hop in ["connection", "keep-alive", "transfer-encoding", "upgrade"] {
            assert_eq!(header(hop), None, "{hop}");
        }
        assert_eq!(
            chunk(&mut response).await.as_deref(),
            Some("retry: 3000\n: connected\n\n")
        );
        crate::publish(
            &fixture.broker,
            &channel,
            &[forge_views::TurboStreamAction::remove(
                forge_views::DomId::new("post_1").expect("valid"),
            )],
        )
        .await
        .expect("publishes");
        assert_eq!(
            chunk(&mut response).await.as_deref(),
            Some(
                "data: <turbo-stream action=\"remove\" target=\"post_1\"></turbo-stream>\n\
                 data: \n\n"
            )
        );
    }

    #[tokio::test]
    async fn forged_tampered_missing_and_expired_tokens_get_one_answer() {
        let fixture = fixture(|streams| streams);
        let tenant = TenantId::new();
        let channel = Channel::for_tenant(tenant, "posts").expect("valid");
        let url = fixture.url(&channel);
        let token = url.split_once("stream=").expect("param").1.to_owned();
        let expired = Signer::new(KEY, &[])
            .expect("signer")
            .with_clock(|| std::time::UNIX_EPOCH + Duration::from_secs(1_000))
            .source_url("/streams", &channel)
            .expect("signs");
        let foreign = Signer::new(b"fedcba9876543210fedcba9876543210", &[])
            .expect("signer")
            .source_url("/streams", &channel)
            .expect("signs");

        let mut bodies = Vec::new();
        for uri in [
            "/streams".to_owned(),
            "/streams?stream=".to_owned(),
            "/streams?stream=forged".to_owned(),
            format!("/streams?stream={token}x"),
            format!("/streams?stream={token}&stream={token}"),
            expired,
            foreign,
        ] {
            let response = fixture.member(&uri, tenant).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(code(&response), "stream_invalid", "{uri}");
            bodies.push(response.body().clone());
        }
        bodies.dedup();
        assert_eq!(bodies.len(), 1, "every refusal reads the same");
        assert_eq!(fixture.broker.subscribers(), 0);
    }

    #[tokio::test]
    async fn another_tenant_never_subscribes_to_a_tenant_channel() {
        let fixture = fixture(|streams| streams);
        let owner = TenantId::new();
        let channel = Channel::for_tenant(owner, "posts").expect("valid");
        let url = fixture.url(&channel);

        let other = fixture.member(&url, TenantId::new()).await;
        let principal = fixture
            .get(
                &url,
                &[("x-test-principal", PrincipalId::new().to_string())],
                "127.0.0.1:1",
            )
            .await;
        let anonymous = fixture
            .get(&url, &[("x-test-anonymous", "1".to_owned())], "127.0.0.1:1")
            .await;

        for response in [other, principal, anonymous] {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(code(&response), "stream_forbidden");
        }
        assert_eq!(fixture.broker.subscribers(), 0);
    }

    #[tokio::test]
    async fn the_access_policy_decides_first() {
        let fixture = fixture(|streams| streams);
        let tenant = TenantId::new();
        let posts = fixture.url(&Channel::for_tenant(tenant, "posts").expect("valid"));
        let invoices = fixture.url(&Channel::for_tenant(tenant, "invoices").expect("valid"));

        let signed_out = fixture.get(&posts, &[], "127.0.0.1:1").await;
        assert_eq!(signed_out.status(), StatusCode::UNAUTHORIZED);
        let unpermitted = fixture.member(&invoices, tenant).await;
        assert_eq!(unpermitted.status(), StatusCode::FORBIDDEN);
        assert_eq!(code(&unpermitted), "stream_forbidden");
        let unavailable = fixture
            .get(
                &posts,
                &[("x-test-unavailable", "1".to_owned())],
                "127.0.0.1:1",
            )
            .await;
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn open_streams_are_capped_per_principal_and_per_tenant() {
        let fixture = fixture(|streams| streams.max_per_principal(Some(1)).max_per_tenant(Some(2)));
        let tenant = TenantId::new();
        let channel = Channel::for_tenant(tenant, "posts").expect("valid");
        let url = fixture.url(&channel);
        let principal = PrincipalId::new();
        let as_principal = [("x-test-principal", principal.to_string())];
        let global = fixture.url(&Channel::global("posts").expect("valid"));

        let first = fixture.get(&global, &as_principal, "127.0.0.1:1").await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = fixture.get(&global, &as_principal, "127.0.0.1:1").await;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(code(&second), "stream_limit");
        assert_eq!(
            second
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some("5")
        );
        drop(first);
        let again = fixture.get(&global, &as_principal, "127.0.0.1:1").await;
        assert_eq!(
            again.status(),
            StatusCode::OK,
            "closing a stream frees its slot"
        );

        // Two members of one tenant fill its cap; a third is refused.
        let one = fixture.member(&url, tenant).await;
        let two = fixture.member(&url, tenant).await;
        let three = fixture.member(&url, tenant).await;
        assert_eq!(one.status(), StatusCode::OK);
        assert_eq!(two.status(), StatusCode::OK);
        assert_eq!(three.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(fixture.broker.subscribers(), 3);
    }

    #[tokio::test]
    async fn anonymous_viewers_are_counted_per_address_and_ipv6_per_64() {
        let fixture = fixture(|streams| streams.max_per_principal(Some(1)));
        let url = fixture.url(&Channel::global("posts").expect("valid"));
        let anonymous = [("x-test-anonymous", "1".to_owned())];

        let first = fixture.get(&url, &anonymous, "[2001:db8:1:2::1]:1").await;
        assert_eq!(first.status(), StatusCode::OK);
        // Another address of the same /64 is the same client.
        let same = fixture
            .get(&url, &anonymous, "[2001:db8:1:2:ffff::9]:2")
            .await;
        assert_eq!(same.status(), StatusCode::TOO_MANY_REQUESTS);
        let other = fixture.get(&url, &anonymous, "[2001:db8:1:3::1]:1").await;
        assert_eq!(other.status(), StatusCode::OK);
        let ipv4 = fixture.get(&url, &anonymous, "192.0.2.1:1").await;
        assert_eq!(ipv4.status(), StatusCode::OK);
        // An IPv4-mapped IPv6 peer is its IPv4 address.
        let mapped = fixture.get(&url, &anonymous, "[::ffff:192.0.2.1]:1").await;
        assert_eq!(mapped.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn peer_addresses_group_ipv6_by_64() {
        let request = |peer: &str| {
            Request::new_for_test(
                Method::GET,
                "/".parse().expect("uri"),
                HeaderMap::new(),
                Bytes::new(),
                peer.parse().expect("peer"),
                Uuid::now_v7(),
            )
        };
        assert_eq!(peer_address(&request("192.0.2.7:80")), "192.0.2.7");
        assert_eq!(
            peer_address(&request("[2001:db8:1:2:aaaa::1]:80")),
            "2001:db8:1:2::/64"
        );
        assert_eq!(peer_address(&request("[::ffff:192.0.2.7]:80")), "192.0.2.7");
    }

    #[tokio::test]
    async fn a_full_broker_answers_unavailable() {
        let shutdown = Cancellation::new();
        let broker = MemoryBroker::new(
            MemoryOptions {
                max_subscribers: 1,
                ..MemoryOptions::default()
            },
            &shutdown,
        )
        .expect("broker");
        let signer = Arc::new(Signer::new(KEY, &[]).expect("signer"));
        let streams = EventStreams::new(
            Arc::new(broker.clone()),
            Arc::clone(&signer),
            Arc::new(HeaderAccess),
        );
        let channel = Channel::global("posts").expect("valid");
        let url = signer.source_url("/streams", &channel).expect("signs");
        let request = |principal: PrincipalId| {
            let mut headers = HeaderMap::new();
            headers.insert(
                "x-test-principal",
                HeaderValue::from_str(&principal.to_string()).expect("value"),
            );
            Request::new_for_test(
                Method::GET,
                url.parse().expect("uri"),
                headers,
                Bytes::new(),
                "127.0.0.1:1".parse().expect("peer"),
                Uuid::now_v7(),
            )
        };

        let open = streams.serve(&request(PrincipalId::new())).await;
        let full = streams.serve(&request(PrincipalId::new())).await;

        assert_eq!(open.status(), StatusCode::OK);
        assert_eq!(full.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code(&full), "stream_unavailable");
        assert!(full.headers().contains_key("retry-after"));
        drop(open);
        shutdown.cancel();
        let closed = streams.serve(&request(PrincipalId::new())).await;
        assert_eq!(closed.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn shutdown_ends_open_streams() {
        let fixture = fixture(|streams| streams);
        let tenant = TenantId::new();
        let channel = Channel::for_tenant(tenant, "posts").expect("valid");
        let mut response = fixture.member(&fixture.url(&channel), tenant).await;
        assert!(chunk(&mut response).await.is_some(), "connected");

        fixture.shutdown.cancel();

        assert_eq!(chunk(&mut response).await, None);
        drop(response);
        assert_eq!(fixture.broker.subscribers(), 0);
    }

    #[tokio::test]
    async fn heartbeats_keep_the_stream_alive_until_its_maximum_age() {
        let fixture = fixture(|streams| {
            streams
                .heartbeat(Duration::from_millis(100))
                .expect("positive")
                .max_connection_age(Some(Duration::from_millis(250)))
        });
        let tenant = TenantId::new();
        let channel = Channel::for_tenant(tenant, "posts").expect("valid");
        let mut response = fixture.member(&fixture.url(&channel), tenant).await;

        assert!(chunk(&mut response).await.is_some(), "connected");
        assert_eq!(chunk(&mut response).await.as_deref(), Some(": ping\n\n"));
        assert_eq!(chunk(&mut response).await.as_deref(), Some(": ping\n\n"));
        // At the maximum age the stream ends and the browser reconnects.
        assert_eq!(chunk(&mut response).await, None);
        drop(response);
        assert_eq!(fixture.broker.subscribers(), 0);
        assert!(
            EventStreams::new(
                Arc::new(fixture.broker.clone()),
                Arc::clone(&fixture.signer),
                Arc::new(HeaderAccess)
            )
            .heartbeat(Duration::ZERO)
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_slow_subscriber_stream_ends() {
        let shutdown = Cancellation::new();
        let broker = MemoryBroker::new(
            MemoryOptions {
                buffer: 1,
                ..MemoryOptions::default()
            },
            &shutdown,
        )
        .expect("broker");
        let signer = Arc::new(Signer::new(KEY, &[]).expect("signer"));
        let streams = EventStreams::new(
            Arc::new(broker.clone()),
            Arc::clone(&signer),
            Arc::new(HeaderAccess),
        );
        let channel = Channel::global("posts").expect("valid");
        let mut headers = HeaderMap::new();
        headers.insert("x-test-anonymous", HeaderValue::from_static("1"));
        let request = Request::new_for_test(
            Method::GET,
            signer
                .source_url("/streams", &channel)
                .expect("signs")
                .parse()
                .expect("uri"),
            headers,
            Bytes::new(),
            "127.0.0.1:1".parse().expect("peer"),
            Uuid::now_v7(),
        );
        let mut response = streams.serve(&request).await;
        assert!(chunk(&mut response).await.is_some(), "connected");

        for index in 0..3 {
            broker
                .publish(&channel, Message::new(format!("m{index}")))
                .await
                .expect("never blocks");
        }

        assert_eq!(chunk(&mut response).await.as_deref(), Some("data: m0\n\n"));
        assert_eq!(chunk(&mut response).await, None, "dropped as too slow");
        assert_eq!(broker.dropped(), 1);
    }
}
