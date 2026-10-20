//! Connection pool gauges with the OpenTelemetry database client names
//! Forge-Go uses: `db.client.connection.count` (by state `idle`/`used`) and
//! `db.client.connection.max`, labelled with `db.system.name` and
//! `db.client.connection.pool.name`. The pool is read when metrics are
//! collected, never per query.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::{Telemetry, metrics::MetricError};

/// `db.client.connection.count`.
pub const METRIC_CONNECTION_COUNT: &str = "db.client.connection.count";
/// `db.client.connection.max`.
pub const METRIC_CONNECTION_MAX: &str = "db.client.connection.max";

/// A pool's state at one instant.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PoolStats {
    /// Open connections, idle and in use.
    pub open: u32,
    /// Idle connections.
    pub idle: u32,
    /// The pool's limit.
    pub max: u32,
}

/// Stops observing a pool when dropped (or with [`PoolObservation::stop`]),
/// and sets its connection counts to zero.
#[must_use = "dropping the observation stops reading the pool"]
pub struct PoolObservation {
    stop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl std::fmt::Debug for PoolObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PoolObservation")
            .field("active", &self.stop.is_some())
            .finish()
    }
}

impl PoolObservation {
    /// Stops observing now.
    pub fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

impl Drop for PoolObservation {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

impl Telemetry {
    /// Observes a connection pool: `system` is `db.system.name`
    /// (`postgresql`, `mysql`, `sqlite`), `pool` the pool name (`default`
    /// unless an application opens several), `stats` reads the pool. Give
    /// each open pool its own name; stop the observation when the pool
    /// closes.
    pub fn observe_pool(
        &self,
        system: &'static str,
        pool: &str,
        stats: impl Fn() -> PoolStats + Send + Sync + 'static,
    ) -> Result<PoolObservation, MetricError> {
        let registry = self.metrics();
        let count = registry.gauge(
            METRIC_CONNECTION_COUNT,
            "Connections in the pool, by state (idle or used).",
            "{connection}",
            &[
                "db.system.name",
                "db.client.connection.pool.name",
                "db.client.connection.state",
            ],
        )?;
        let max = registry.gauge(
            METRIC_CONNECTION_MAX,
            "Maximum number of connections the pool allows.",
            "{connection}",
            &["db.system.name", "db.client.connection.pool.name"],
        )?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let key = format!(
            "forge.db.pool/{system}/{pool}/{}",
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let name: Arc<str> = Arc::from(pool);
        let active = Arc::new(AtomicBool::new(true));
        {
            let (count, max, name, active) = (
                count.clone(),
                max.clone(),
                Arc::clone(&name),
                Arc::clone(&active),
            );
            registry.observe(&key, move || {
                if !active.load(Ordering::Acquire) {
                    return;
                }
                let stats = stats();
                let used = stats.open.saturating_sub(stats.idle);
                count.set(f64::from(stats.idle), &[system, &name, "idle"]);
                count.set(f64::from(used), &[system, &name, "used"]);
                max.set(f64::from(stats.max), &[system, &name]);
            });
        }
        let registry = registry.clone();
        Ok(PoolObservation {
            stop: Some(Box::new(move || {
                active.store(false, Ordering::Release);
                registry.unobserve(&key);
                count.set(0.0, &[system, &name, "idle"]);
                count.set(0.0, &[system, &name, "used"]);
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::Options;

    #[test]
    fn pools_are_read_on_collection_and_zeroed_when_closed() {
        let telemetry = Telemetry::new(Options::new("svc")).expect("valid");
        let state = Arc::new(Mutex::new(PoolStats {
            open: 7,
            idle: 3,
            max: 10,
        }));
        let shared = Arc::clone(&state);
        let observation = telemetry
            .observe_pool("postgresql", "default", move || {
                *shared.lock().expect("lock")
            })
            .expect("registers");
        let read = |telemetry: &Telemetry| {
            let collected = telemetry.metrics().collect();
            let count = collected
                .iter()
                .find(|m| m.name == METRIC_CONNECTION_COUNT)
                .expect("count")
                .points
                .iter()
                .map(|point| (point.labels[2].1.clone(), point.value))
                .collect::<Vec<_>>();
            let max = collected
                .iter()
                .find(|m| m.name == METRIC_CONNECTION_MAX)
                .expect("max")
                .points[0]
                .value;
            (count, max)
        };
        let (count, max) = read(&telemetry);
        assert_eq!(count, [("idle".to_owned(), 3.0), ("used".to_owned(), 4.0)]);
        assert_eq!(max, 10.0);
        *state.lock().expect("lock") = PoolStats {
            open: 2,
            idle: 2,
            max: 10,
        };
        assert_eq!(read(&telemetry).0[1].1, 0.0);
        observation.stop();
        let (count, _) = read(&telemetry);
        assert_eq!(count, [("idle".to_owned(), 0.0), ("used".to_owned(), 0.0)]);
    }
}
