//! The routing table: services, their current target, health-gated
//! switches, draining of the previous target, maintenance mode and the
//! persisted state that lets a restarted proxy resume routing.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::log::{Logger, text};
use crate::settings::{DeployRequest, DeployResult, ServiceStatus};

/// How long a client may take to send a body the proxy buffers before
/// forwarding, and how long any single read of a larger, streamed body may
/// wait for data.
pub const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting to a target may take.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The most upgraded (WebSocket) connections held open at once.
const MAX_TUNNELS: usize = 10_000;

const STATE_VERSION: i64 = 1;

/// Why a deploy was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeployError {
    /// The message, in Forge-Go's words.
    pub message: String,
    /// The target never passed its health check; the previous target keeps
    /// serving.
    pub unhealthy: bool,
}

impl std::fmt::Display for DeployError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeployError {}

impl DeployError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unhealthy: false,
        }
    }
}

/// The text of every health-check failure.
pub const ERR_UNHEALTHY: &str = "target failed its health check";

/// Routes requests to the current target of each service. Cheap to clone;
/// every clone is the same server.
#[derive(Clone, Debug)]
pub struct Server {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) logger: Logger,
    state_path: Option<PathBuf>,
    /// [`BODY_READ_TIMEOUT`]; tests shorten it.
    pub(crate) body_timeout: Mutex<Duration>,
    routes: RwLock<Routes>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    state_lock: Mutex<()>,
    /// Upgraded connections outlive the request that opened them; they are
    /// tracked here and stopped by `shutdown`.
    pub(crate) tunnels: TaskTracker,
    pub(crate) tunnel_permits: Arc<Semaphore>,
    pub(crate) shutdown: CancellationToken,
    /// HTTP-01 key authorizations, once HTTPS is enabled.
    challenges: Mutex<Option<crate::acme::Challenges>>,
    /// Signalled when a TLS service is deployed or loaded.
    tls_changed: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct Routes {
    services: BTreeMap<String, Arc<Service>>,
    hosts: HashMap<String, Arc<Service>>,
    fallback: Option<Arc<Service>>,
}

/// One routing entry.
#[derive(Debug)]
pub(crate) struct Service {
    pub(crate) name: String,
    settings: RwLock<Arc<DeployRequest>>,
    target: RwLock<Option<Arc<Target>>>,
    maintenance: AtomicBool,
}

impl Service {
    pub(crate) fn settings(&self) -> Arc<DeployRequest> {
        Arc::clone(&read(&self.settings))
    }

    pub(crate) fn target(&self) -> Option<Arc<Target>> {
        read(&self.target).clone()
    }

    pub(crate) fn maintenance(&self) -> bool {
        self.maintenance.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Occupancy {
    retired: bool,
    active: u64,
}

/// One backend container. Requests take a [`Lease`] before forwarding; once
/// retired, no new lease is granted and the request re-reads the service's
/// current target, which was swapped in before retirement.
#[derive(Debug)]
pub(crate) struct Target {
    pub(crate) addr: String,
    occupancy: watch::Sender<Occupancy>,
}

/// An in-flight request on a target; dropping it ends the request.
#[derive(Debug)]
pub(crate) struct Lease {
    target: Arc<Target>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.target.occupancy.send_modify(|state| {
            state.active = state.active.saturating_sub(1);
        });
    }
}

impl Lease {
    pub(crate) fn addr(&self) -> &str {
        &self.target.addr
    }
}

impl Target {
    fn new(addr: &str) -> Arc<Self> {
        Arc::new(Self {
            addr: addr.to_owned(),
            occupancy: watch::Sender::new(Occupancy::default()),
        })
    }

    /// A lease, unless the target was retired.
    pub(crate) fn acquire(self: &Arc<Self>) -> Option<Lease> {
        let granted = self.occupancy.send_if_modified(|state| {
            if state.retired {
                return false;
            }
            state.active += 1;
            true
        });
        granted.then(|| Lease {
            target: Arc::clone(self),
        })
    }

    /// Stops new leases.
    pub(crate) fn retire(&self) {
        self.occupancy.send_modify(|state| state.retired = true);
    }

    pub(crate) fn in_flight(&self) -> u64 {
        self.occupancy.borrow().active
    }

    /// Retires the target and waits up to `timeout` for its requests to
    /// finish. Reports whether they did and how many remain.
    async fn drain(&self, timeout: Duration) -> (bool, u64) {
        self.retire();
        let mut receiver = self.occupancy.subscribe();
        let idle =
            tokio::time::timeout(timeout, receiver.wait_for(|state| state.active == 0)).await;
        match idle {
            Ok(Ok(_)) => (true, 0),
            _ => {
                let remaining = self.in_flight();
                (remaining == 0, remaining)
            }
        }
    }
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Deserialize, Serialize)]
struct SavedState {
    version: i64,
    services: Vec<SavedService>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SavedService {
    settings: DeployRequest,
    maintenance: bool,
}

/// Lowers `host`, drops a port and a trailing dot.
pub(crate) fn route_key(host: &str) -> String {
    strip_port(host).trim_end_matches('.').to_ascii_lowercase()
}

/// `host` without `:port` (IPv6 brackets kept off).
pub(crate) fn strip_port(host: &str) -> &str {
    crate::settings::split_host_port(host).map_or(host, |(name, _)| name)
}

impl Server {
    /// Creates a server and loads the routing state saved at `state_path`,
    /// if any. `None` disables persistence.
    ///
    /// # Errors
    ///
    /// The state file exists but cannot be read, decoded or validated: the
    /// proxy refuses to start rather than route from a state it does not
    /// understand.
    pub fn new(logger: Logger, state_path: Option<PathBuf>) -> Result<Self, String> {
        let server = Self {
            inner: Arc::new(Inner {
                logger,
                state_path,
                body_timeout: Mutex::new(BODY_READ_TIMEOUT),
                routes: RwLock::new(Routes::default()),
                locks: Mutex::new(HashMap::new()),
                state_lock: Mutex::new(()),
                tunnels: TaskTracker::new(),
                tunnel_permits: Arc::new(Semaphore::new(MAX_TUNNELS)),
                shutdown: CancellationToken::new(),
                challenges: Mutex::new(None),
                tls_changed: tokio::sync::Notify::new(),
            }),
        };
        if let Some(path) = server.inner.state_path.clone() {
            if let Some(directory) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                crate::files::clean_up(&server.inner.logger, directory);
            }
            server.load_state(&path)?;
        }
        Ok(server)
    }

    /// Shortens the body read timeout (tests).
    #[doc(hidden)]
    pub fn set_body_timeout(&self, timeout: Duration) {
        *lock(&self.inner.body_timeout) = timeout;
    }

    pub(crate) fn body_timeout(&self) -> Duration {
        *lock(&self.inner.body_timeout)
    }

    pub(crate) fn logger(&self) -> &Logger {
        &self.inner.logger
    }

    /// Starts answering ACME HTTP-01 challenges on plain HTTP and returns
    /// the registry the certificate manager fills.
    pub(crate) fn enable_challenges(&self) -> crate::acme::Challenges {
        let mut challenges = lock(&self.inner.challenges);
        Arc::clone(challenges.get_or_insert_with(Default::default))
    }

    /// The key authorization for an HTTP-01 `token`: `None` when HTTPS is
    /// off, `Some(None)` when the token is unknown.
    pub(crate) fn challenge(&self, token: &str) -> Option<Option<String>> {
        let challenges = lock(&self.inner.challenges).clone()?;
        let answer = lock(&challenges).get(token).cloned();
        Some(answer)
    }

    /// Resolves when a TLS service was deployed or restored.
    pub(crate) async fn tls_changed(&self) {
        self.inner.tls_changed.notified().await;
    }

    /// Stops upgraded connections and waits for them to end.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        self.inner.tunnels.close();
        self.inner.tunnels.wait().await;
    }

    fn load_state(&self, path: &Path) -> Result<(), String> {
        let content = match std::fs::read(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("read proxy state: {error}")),
        };
        let state: SavedState = serde_json::from_slice(&content)
            .map_err(|error| format!("decode proxy state {}: {error}", path.display()))?;
        if state.version != STATE_VERSION {
            return Err(format!(
                "proxy state {} has unsupported version {}",
                path.display(),
                state.version
            ));
        }
        let mut routes = write(&self.inner.routes);
        for saved in state.services {
            let mut settings = saved.settings;
            let name = settings.service.clone();
            settings
                .normalize()
                .map_err(|error| format!("proxy state service {:?}: {error}", name))?;
            check_hosts(&routes, &settings.service, &settings.hosts)
                .map_err(|error| format!("proxy state: {error}"))?;
            let service = Arc::new(Service {
                name: settings.service.clone(),
                target: RwLock::new(Some(Target::new(&settings.target))),
                settings: RwLock::new(Arc::new(settings)),
                maintenance: AtomicBool::new(saved.maintenance),
            });
            routes.services.insert(service.name.clone(), service);
            reindex(&mut routes);
        }
        self.inner.tls_changed.notify_one();
        Ok(())
    }

    /// Writes the routing table atomically (temporary file, fsync, rename,
    /// directory fsync) with 0600 permissions.
    fn save_state(&self) -> Result<(), String> {
        let Some(path) = self.inner.state_path.as_deref() else {
            return Ok(());
        };
        let state = {
            let routes = read(&self.inner.routes);
            SavedState {
                version: STATE_VERSION,
                services: routes
                    .services
                    .values()
                    .map(|service| {
                        let mut settings = (*service.settings()).clone();
                        if let Some(target) = service.target() {
                            settings.target.clone_from(&target.addr);
                        }
                        SavedService {
                            settings,
                            maintenance: service.maintenance(),
                        }
                    })
                    .collect(),
            }
        };
        let mut content = serde_json::to_vec_pretty(&state).map_err(|e| e.to_string())?;
        content.push(b'\n');
        let _guard = lock(&self.inner.state_lock);
        let written = crate::files::write_atomically(path, &content)
            .map_err(|error| format!("write proxy state: {error}"))?;
        crate::files::report(&self.inner.logger, path, written);
        Ok(())
    }

    pub(crate) fn route(&self, host: &str) -> Option<Arc<Service>> {
        let key = route_key(host);
        let routes = read(&self.inner.routes);
        routes
            .hosts
            .get(&key)
            .cloned()
            .or_else(|| routes.fallback.clone())
    }

    /// Whether `host` belongs to a service with TLS enabled; certificates
    /// are issued only for those hosts.
    #[must_use]
    pub fn tls_host(&self, host: &str) -> bool {
        let routes = read(&self.inner.routes);
        routes
            .hosts
            .get(&host.to_ascii_lowercase())
            .is_some_and(|service| service.settings().tls)
    }

    /// The host names of every TLS service.
    #[must_use]
    pub fn tls_hosts(&self) -> Vec<String> {
        let routes = read(&self.inner.routes);
        let mut hosts: Vec<String> = routes
            .hosts
            .iter()
            .filter(|(_, service)| service.settings().tls)
            .map(|(host, _)| host.clone())
            .collect();
        hosts.sort();
        hosts
    }

    fn service_lock(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = lock(&self.inner.locks);
        Arc::clone(locks.entry(name.to_owned()).or_default())
    }

    /// Registers `req.target`, waits until it passes its health check
    /// `req.healthy_count` times in a row, switches the service to it
    /// atomically and drains the previous target for up to
    /// `req.drain_timeout`. If the health check does not pass within
    /// `req.deploy_timeout`, the previous target keeps serving and the
    /// error is [`DeployError::unhealthy`].
    ///
    /// # Errors
    ///
    /// An invalid request, a host routed to another service, or a target
    /// that never became healthy.
    pub async fn deploy(&self, mut req: DeployRequest) -> Result<DeployResult, DeployError> {
        req.normalize().map_err(DeployError::invalid)?;
        let service_lock = self.service_lock(&req.service);
        let guard = service_lock.lock().await;
        check_hosts(&read(&self.inner.routes), &req.service, &req.hosts)
            .map_err(DeployError::invalid)?;
        let checks = match self.wait_healthy(&req).await {
            Ok(checks) => checks,
            Err(error) => {
                self.inner.logger.warn(
                    "deploy rejected",
                    &[
                        ("service", text(&req.service)),
                        ("target", text(&req.target)),
                        ("error", text(&error.message)),
                    ],
                );
                return Err(error);
            }
        };
        let next = Target::new(&req.target);
        let previous = {
            let mut routes = write(&self.inner.routes);
            check_hosts(&routes, &req.service, &req.hosts).map_err(DeployError::invalid)?;
            let service = Arc::clone(routes.services.entry(req.service.clone()).or_insert_with(
                || {
                    Arc::new(Service {
                        name: req.service.clone(),
                        settings: RwLock::new(Arc::new(req.clone())),
                        target: RwLock::new(None),
                        maintenance: AtomicBool::new(false),
                    })
                },
            ));
            *write(&service.settings) = Arc::new(req.clone());
            let previous = write(&service.target).replace(next);
            reindex(&mut routes);
            previous
        };
        let mut result = DeployResult {
            service: req.service.clone(),
            target: req.target.clone(),
            drained: true,
            health_checks: checks,
            ..DeployResult::default()
        };
        if let Err(error) = self.save_state_blocking().await {
            self.inner
                .logger
                .error("saving proxy state failed", &[("error", text(&error))]);
            result.state_save_failed = error;
        }
        drop(guard);
        if req.tls {
            self.inner.tls_changed.notify_one();
        }
        self.inner.logger.info(
            "target switched",
            &[
                ("service", text(&req.service)),
                ("target", text(&req.target)),
            ],
        );
        if let Some(previous) = previous {
            result.previous.clone_from(&previous.addr);
            let (drained, remaining) = previous.drain(req.drain_timeout.to_std()).await;
            result.drained = drained;
            result.in_flight = i64::try_from(remaining).unwrap_or(i64::MAX);
            self.inner.logger.info(
                "previous target drained",
                &[
                    ("service", text(&req.service)),
                    ("target", text(&previous.addr)),
                    ("drained", json!(drained)),
                    ("in_flight", json!(remaining)),
                ],
            );
        }
        Ok(result)
    }

    async fn save_state_blocking(&self) -> Result<(), String> {
        let server = self.clone();
        match tokio::task::spawn_blocking(move || server.save_state()).await {
            Ok(result) => result,
            Err(error) => Err(format!("write proxy state: {error}")),
        }
    }

    async fn wait_healthy(&self, req: &DeployRequest) -> Result<i64, DeployError> {
        let deadline = Instant::now() + req.deploy_timeout.to_std();
        let mut consecutive = 0;
        let mut checks: i64 = 0;
        let mut last = "no check completed".to_owned();
        loop {
            checks += 1;
            let probe_deadline = deadline.min(Instant::now() + req.health_timeout.to_std());
            let outcome = tokio::select! {
                result = tokio::time::timeout_at(probe_deadline, probe(req)) => {
                    result.unwrap_or_else(|_| Err("request failed".to_owned()))
                }
                () = self.inner.shutdown.cancelled() => Err("proxy shutting down".to_owned()),
            };
            match outcome {
                Ok(()) => {
                    consecutive += 1;
                    if consecutive >= req.healthy_count {
                        return Ok(checks);
                    }
                }
                Err(error) => {
                    consecutive = 0;
                    // A probe cut short by the deploy deadline says nothing
                    // new; keep the previous failure for the message.
                    if Instant::now() < deadline || last == "no check completed" {
                        last = error;
                    }
                }
            }
            let wake = Instant::now() + req.health_interval.to_std();
            let unhealthy = || DeployError {
                message: format!(
                    "{ERR_UNHEALTHY}: {} not healthy within {} ({checks} checks, last: {last})",
                    req.target, req.deploy_timeout
                ),
                unhealthy: true,
            };
            if wake >= deadline || self.inner.shutdown.is_cancelled() {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => {}
                    () = self.inner.shutdown.cancelled() => {}
                }
                return Err(unhealthy());
            }
            tokio::select! {
                () = tokio::time::sleep_until(wake) => {}
                () = self.inner.shutdown.cancelled() => return Err(unhealthy()),
            }
        }
    }

    /// Turns the maintenance page on or off for a service.
    ///
    /// # Errors
    ///
    /// The service is unknown, or the state could not be saved.
    pub async fn set_maintenance(&self, name: &str, on: bool) -> Result<(), String> {
        let service = read(&self.inner.routes).services.get(name).cloned();
        let Some(service) = service else {
            return Err(format!(
                "unknown service {}",
                crate::settings::go_quote(name)
            ));
        };
        service.maintenance.store(on, Ordering::SeqCst);
        self.save_state_blocking().await
    }

    /// Unregisters a service and drains its target.
    ///
    /// # Errors
    ///
    /// The service is unknown, or the state could not be saved.
    pub async fn remove(&self, name: &str, drain_timeout: Duration) -> Result<(), String> {
        let service_lock = self.service_lock(name);
        let guard = service_lock.lock().await;
        let previous = {
            let mut routes = write(&self.inner.routes);
            let Some(service) = routes.services.remove(name) else {
                return Err(format!(
                    "unknown service {}",
                    crate::settings::go_quote(name)
                ));
            };
            reindex(&mut routes);
            // Requests routed before the removal see no target (503)
            // instead of a retired one.
            write(&service.target).take()
        };
        let saved = self.save_state_blocking().await;
        drop(guard);
        if let Some(previous) = previous {
            previous.drain(drain_timeout).await;
        }
        saved
    }

    /// The services, sorted by name.
    #[must_use]
    pub fn status(&self) -> Vec<ServiceStatus> {
        let routes = read(&self.inner.routes);
        routes
            .services
            .values()
            .map(|service| {
                let settings = service.settings();
                let target = service.target();
                ServiceStatus {
                    service: service.name.clone(),
                    hosts: settings.hosts.clone(),
                    tls: settings.tls,
                    target: target.as_ref().map(|t| t.addr.clone()).unwrap_or_default(),
                    maintenance: service.maintenance(),
                    in_flight: target
                        .as_ref()
                        .map_or(0, |t| i64::try_from(t.in_flight()).unwrap_or(i64::MAX)),
                    health_path: settings.health_path.clone(),
                }
            })
            .collect()
    }

    /// Retires the current target of `service` without a successor (tests
    /// of the window between routing and acquiring).
    #[doc(hidden)]
    pub fn retire_current_target(&self, service: &str) {
        if let Some(target) = read(&self.inner.routes)
            .services
            .get(service)
            .and_then(|service| service.target())
        {
            target.retire();
        }
    }
}

fn check_hosts(routes: &Routes, name: &str, hosts: &[String]) -> Result<(), String> {
    for host in hosts {
        if let Some(owner) = routes.hosts.get(host)
            && owner.name != name
        {
            return Err(format!(
                "host {host} is already routed to service {}",
                owner.name
            ));
        }
    }
    if hosts.is_empty()
        && let Some(fallback) = &routes.fallback
        && fallback.name != name
    {
        return Err(format!(
            "service {} already receives requests for unmatched hosts",
            fallback.name
        ));
    }
    Ok(())
}

fn reindex(routes: &mut Routes) {
    routes.hosts.clear();
    routes.fallback = None;
    for service in routes.services.values() {
        let settings = service.settings();
        if settings.hosts.is_empty() {
            routes.fallback = Some(Arc::clone(service));
        }
        for host in &settings.hosts {
            routes.hosts.insert(host.clone(), Arc::clone(service));
        }
    }
}

/// One health check: `GET http://TARGET/PATH` with the service's first host
/// as `Host`, no redirects followed, any 2xx passes.
async fn probe(req: &DeployRequest) -> Result<(), String> {
    let failed = || "request failed".to_owned();
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&req.target))
        .await
        .map_err(|_| failed())?
        .map_err(|_| failed())?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
            .await
            .map_err(|_| failed())?;
    let host = req
        .hosts
        .first()
        .cloned()
        .unwrap_or_else(|| req.target.clone());
    let request = hyper::Request::get(req.health_path.as_str())
        .header(hyper::header::HOST, host)
        .header(hyper::header::USER_AGENT, "forge-proxy")
        .header(hyper::header::CONNECTION, "close")
        .body(Empty::new())
        .map_err(|_| failed())?;
    let exchange = async {
        let response = sender.send_request(request).await.map_err(|_| failed())?;
        let status = response.status();
        // Read (and discard) at most 4 KiB of the body.
        let mut body = response.into_body();
        let mut read = 0_usize;
        while read < 4096 {
            match body.frame().await {
                Some(Ok(frame)) => {
                    read += frame.data_ref().map_or(0, Bytes::len);
                }
                _ => break,
            }
        }
        if !status.is_success() {
            return Err(format!("status {}", status.as_u16()));
        }
        Ok(())
    };
    tokio::pin!(connection);
    tokio::pin!(exchange);
    tokio::select! {
        result = &mut exchange => result,
        // The connection ended; the exchange resolves next.
        _ = &mut connection => exchange.await,
    }
}
