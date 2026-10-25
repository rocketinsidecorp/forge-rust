//! Telemetry in the process lifecycle: what `build` makes of the
//! configuration, the metrics listener's warning, the pool observation and
//! the bounded shutdown.

use std::ffi::OsString;

use forge_config::{ConfigError, SecretString};

use super::*;
use crate::test_support::{capture, warnings};

fn config(vars: &[(&str, &str)]) -> Result<AppConfig, ConfigError> {
    AppConfig::from_vars(
        vars.iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    )
}

fn service_version(telemetry: &Telemetry) -> Option<String> {
    let resource = format!("{:?}", telemetry.resource());
    resource.contains("service.version").then_some(resource)
}

#[test]
fn telemetry_is_built_from_the_configuration_unless_disabled() {
    let enabled = config(&[]).expect("config");
    let telemetry = build(&enabled, "demo", "1.2.3")
        .expect("builds")
        .expect("enabled by default");
    let resource = service_version(&telemetry).expect("a known version is reported");
    assert!(
        resource.contains("1.2.3") && resource.contains("demo"),
        "{resource}"
    );

    // An application that set no version reports none.
    let unversioned = build(&enabled, "demo", crate::app::UNKNOWN_VERSION)
        .expect("builds")
        .expect("enabled");
    assert_eq!(service_version(&unversioned), None);

    let disabled = config(&[("FORGE_TELEMETRY_ENABLED", "false")]).expect("config");
    assert!(build(&disabled, "demo", "1.2.3").expect("builds").is_none());
}

#[test]
fn an_open_metrics_listener_is_warned_about_outside_production_only() {
    let open = [
        ("FORGE_TELEMETRY_METRICS_ADDR", "0.0.0.0:9464"),
        ("FORGE_BIND", "127.0.0.1:3000"),
    ];
    let log = warnings(|| warn_public_metrics(&config(&open).expect("config")));
    assert!(log.contains("FORGE_TELEMETRY_METRICS_ADDR"), "{log}");
    assert!(log.contains("0.0.0.0:9464"), "{log}");

    // Loopback, confirmed private networks, no listener and disabled
    // telemetry have nothing to warn about.
    for vars in [
        vec![("FORGE_TELEMETRY_METRICS_ADDR", "127.0.0.1:9464")],
        vec![
            ("FORGE_TELEMETRY_METRICS_ADDR", "0.0.0.0:9464"),
            ("FORGE_TELEMETRY_METRICS_PUBLIC", "true"),
        ],
        vec![
            ("FORGE_TELEMETRY_METRICS_ADDR", "0.0.0.0:9464"),
            ("FORGE_TELEMETRY_ENABLED", "false"),
        ],
        vec![],
    ] {
        let log = warnings(|| warn_public_metrics(&config(&vars).expect("config")));
        assert_eq!(log, "", "{vars:?}");
    }
    let production = config(&[
        ("FORGE_ENV", "production"),
        ("FORGE_TELEMETRY_METRICS_ADDR", "0.0.0.0:9464"),
        ("FORGE_TELEMETRY_METRICS_PUBLIC", "true"),
    ])
    .expect("config");
    assert_eq!(warnings(|| warn_public_metrics(&production)), "");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn the_pool_is_observed_until_the_observation_stops() {
    let telemetry = Telemetry::new(forge_telemetry::Options::new("demo")).expect("telemetry");
    let database = forge_db::SqliteDatabase::connect(&SecretString::new("sqlite::memory:"))
        .await
        .expect("memory database");
    let handle = DatabaseHandle::Sqlite(database.clone());

    let observation = observe_database(&telemetry, &handle).expect("observed");
    let scrape = render(telemetry.metrics());
    assert!(scrape.contains("db_client_connection_count"), "{scrape}");
    assert!(scrape.contains(r#"db_system_name="sqlite""#), "{scrape}");
    assert!(scrape.contains("db_client_connection_max"), "{scrape}");

    // Each observation is its own; stopping one leaves the instruments.
    assert!(observe_database(&telemetry, &handle).is_some());
    observation.stop();

    // Instruments of the same names registered as something else: the
    // application still runs, only without its pool metrics.
    let taken = Telemetry::new(forge_telemetry::Options::new("demo")).expect("telemetry");
    taken
        .metrics()
        .counter("db.client.connection.count", "taken", "1", &[])
        .expect("registers first");
    let log = warnings(|| assert!(observe_database(&taken, &handle).is_none()));
    assert!(
        log.contains("connection pool metrics are not recorded"),
        "{log}"
    );
    database.close().await;
}

#[tokio::test]
async fn a_shutdown_that_misses_its_deadline_is_reported_not_fatal() {
    let vars = [
        ("FORGE_TELEMETRY_EXPORTER", "otlp"),
        // Nothing listens here: the final export cannot finish in time.
        ("FORGE_TELEMETRY_OTLP_ENDPOINT", "http://127.0.0.1:1"),
        ("FORGE_TELEMETRY_OTLP_ALLOW_PLAINTEXT", "true"),
    ];
    let config = config(&vars).expect("config");
    let telemetry = build(&config, "demo", "1.0.0")
        .expect("builds")
        .expect("enabled");
    telemetry.start();
    let (captured, guard) = capture();

    shutdown(Some(telemetry), Instant::now()).await;
    drop(guard);

    let log = captured.text();
    assert!(log.contains("not fully exported"), "{log}");
    assert!(log.contains("metrics"), "{log}");
    // Without telemetry there is nothing to stop.
    shutdown(None, Instant::now()).await;
}
