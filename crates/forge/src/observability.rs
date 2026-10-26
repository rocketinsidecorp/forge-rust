//! Telemetry in the process lifecycle (ADR 0033): built from
//! `FORGE_TELEMETRY_*` before logging is installed, started inside the
//! runtime, given to the server, the database pool and the job runtime, and
//! shut down last, after the other parts stopped, so their spans and the
//! final metric snapshot are exported.

use std::{net::SocketAddr, time::Duration};

use forge_config::{AppConfig, Environment, ServerConfig};
use forge_http::{Method, Response, Router, Server, StatusCode, header};
use forge_telemetry::{
    Telemetry, TelemetryError,
    pool::PoolObservation,
    prometheus::{PROMETHEUS_CONTENT_TYPE, render},
};
use tokio::time::Instant;
use tracing::warn;

use crate::database::DatabaseHandle;

/// Connections the metrics listener accepts at once.
const METRICS_MAX_CONNECTIONS: usize = 16;
/// Request deadline of the metrics listener.
const METRICS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Pool name of the application's database.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
const DEFAULT_POOL: &str = "default";

/// The telemetry `config` describes, `None` when disabled. `version` is the
/// application's (`"unknown"` when it set none).
pub(crate) fn build(
    config: &AppConfig,
    name: &str,
    version: &str,
) -> Result<Option<Telemetry>, TelemetryError> {
    let version = (version != crate::app::UNKNOWN_VERSION).then_some(version);
    Telemetry::from_config(&config.telemetry, config.environment, name, version)
}

/// Warns about an unauthenticated metrics listener reachable from the
/// network outside production (production refuses it at configuration).
pub(crate) fn warn_public_metrics(config: &AppConfig) {
    let telemetry = &config.telemetry;
    if let Some(address) = telemetry.metrics_addr
        && telemetry.enabled
        && !address.ip().is_loopback()
        && !telemetry.metrics_public
        && config.environment != Environment::Production
    {
        warn!(
            %address,
            "FORGE_TELEMETRY_METRICS_ADDR is not a loopback address and the metrics listener has no authentication; set FORGE_TELEMETRY_METRICS_PUBLIC=true if only a private network can reach it (production refuses to start)"
        );
    }
}

/// Reads the pool of `database` at each collection; stopped when the
/// returned observation is dropped (before the pool closes).
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub(crate) fn observe_database(
    telemetry: &Telemetry,
    database: &DatabaseHandle,
) -> Option<PoolObservation> {
    // The SQLx pool's own counters: open and idle connections and the limit.
    macro_rules! observe {
        ($system:literal, $database:expr) => {{
            let pool = $database.pool().clone();
            telemetry.observe_pool($system, DEFAULT_POOL, move || {
                forge_telemetry::pool::PoolStats {
                    open: pool.size(),
                    idle: u32::try_from(pool.num_idle()).unwrap_or(u32::MAX),
                    max: pool.options().get_max_connections(),
                }
            })
        }};
    }
    let observed = match *database {
        #[cfg(feature = "postgres")]
        DatabaseHandle::Postgres(ref database) => observe!("postgresql", database),
        #[cfg(feature = "mysql")]
        DatabaseHandle::MySql(ref database) => observe!("mysql", database),
        #[cfg(feature = "sqlite")]
        DatabaseHandle::Sqlite(ref database) => observe!("sqlite", database),
    };
    match observed {
        Ok(observation) => Some(observation),
        Err(error) => {
            warn!(%error, "connection pool metrics are not recorded");
            None
        }
    }
}

/// Without a database backend there is no pool to observe.
#[cfg(not(any(feature = "postgres", feature = "mysql", feature = "sqlite")))]
pub(crate) fn observe_database(
    _telemetry: &Telemetry,
    database: &DatabaseHandle,
) -> Option<PoolObservation> {
    match *database {}
}

/// The separate Prometheus listener (`FORGE_TELEMETRY_METRICS_ADDR`): plain
/// HTTP, `GET`/`HEAD /metrics` only, never on the application router.
pub(crate) fn metrics_server(telemetry: &Telemetry, bind: SocketAddr) -> Server {
    let registry = telemetry.metrics().clone();
    let mut router = Router::new();
    for method in [Method::GET, Method::HEAD] {
        let registry = registry.clone();
        // The table is fresh, so the two registrations cannot conflict.
        let _ = router.route(method, "/metrics", move |_| {
            let registry = registry.clone();
            async move {
                let mut response = Response::new(StatusCode::OK, render(&registry));
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static(PROMETHEUS_CONTENT_TYPE),
                );
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                response
            }
        });
    }
    let config = ServerConfig {
        bind,
        request_timeout: METRICS_REQUEST_TIMEOUT,
        shutdown_grace: Duration::from_secs(1),
        max_body_bytes: 1024,
        max_connections: METRICS_MAX_CONNECTIONS,
        max_in_flight_body_bytes: 1024 * METRICS_MAX_CONNECTIONS,
        trusted_proxies: Vec::new(),
        // A private plain-HTTP listener scraped by Prometheus.
        transport: forge_config::HttpTransport::Plain,
    };
    Server::new(config, router)
}

/// Flushes and stops the telemetry before `deadline`; failures are reported
/// on standard output, the only channel left.
pub(crate) async fn shutdown(telemetry: Option<Telemetry>, deadline: Instant) {
    if let Some(telemetry) = telemetry
        && let Err(error) = telemetry.shutdown(deadline).await
    {
        warn!(%error, "telemetry was not fully exported before the shutdown deadline");
    }
}

#[cfg(test)]
mod observability_tests;
