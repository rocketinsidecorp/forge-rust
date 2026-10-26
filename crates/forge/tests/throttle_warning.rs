//! The production warning for attempt throttles that count in one process
//! (Forge-Go's `auth.warnProcessLocalThrottle`), and the agreement of the
//! session idle timeout's configuration with its domain type.

use std::cell::RefCell;
use std::io;
use std::sync::{Arc, Once};
use std::time::Duration;

use async_trait::async_trait;
use forge::Composition;
use forge::auth::{
    LoginThrottle, MemoryLoginThrottle, PgLoginThrottle, SessionIdleTimeout, ThrottleConfig,
    ThrottleDecision, ThrottleError, ThrottleKey,
};
use forge::config::{AppConfig, DEFAULT_SESSION_IDLE_TIMEOUT, Environment};
use forge::http::Router;

/// One process-wide subscriber writes into a per-thread buffer (the pattern
/// of `forge-i18n/tests/translator.rs`): with a global subscriber every
/// callsite is always interesting, whatever other tests run in parallel.
fn logs_of(run: impl FnOnce()) -> String {
    thread_local! {
        static CAPTURE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }
    struct ThreadBuffer;
    impl io::Write for ThreadBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            CAPTURE.with(|capture| {
                if let Some(buffer) = capture.borrow_mut().as_mut() {
                    buffer.extend_from_slice(bytes);
                }
            });
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadBuffer)
            .with_ansi(false)
            .without_time()
            .finish();
        // Nothing else in this test binary installs a global subscriber.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
    run();
    let bytes = CAPTURE
        .with(|capture| capture.borrow_mut().take())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A throttle every replica shares, standing for `PgLoginThrottle` or a
/// custom shared store.
struct SharedStub;

#[async_trait]
impl LoginThrottle for SharedStub {
    async fn attempt(&self, _: &ThrottleKey) -> Result<ThrottleDecision, ThrottleError> {
        Ok(ThrottleDecision::Allowed)
    }

    async fn success(&self, _: &ThrottleKey) -> Result<(), ThrottleError> {
        Ok(())
    }
}

/// In production, a sign-in, two-factor or password-reset throttle that
/// counts in memory logs exactly one warning that explains itself; a shared
/// throttle, or another environment, logs nothing. A Tokio test only
/// because a lazy pool needs a runtime; the current-thread runtime keeps the
/// per-thread capture.
#[tokio::test]
async fn process_local_throttles_warn_in_production_only() {
    let memory: Arc<dyn LoginThrottle> =
        Arc::new(MemoryLoginThrottle::new(ThrottleConfig::default()).expect("valid config"));
    // A lazy pool never connects: building the throttle needs no database.
    let pool = sqlx::PgPool::connect_lazy("postgres://u:p@127.0.0.1:1/db").expect("lazy pool");
    let postgres: Arc<dyn LoginThrottle> =
        Arc::new(PgLoginThrottle::new(pool, ThrottleConfig::default()).expect("valid config"));
    let shared: Arc<dyn LoginThrottle> = Arc::new(SharedStub);

    for environment in [
        Environment::Development,
        Environment::Test,
        Environment::Production,
    ] {
        let config = AppConfig {
            environment,
            ..AppConfig::default()
        };
        for service in ["sign-in", "two-factor", "password-reset"] {
            for (name, throttle) in [
                ("memory", &memory),
                ("postgres", &postgres),
                ("shared", &shared),
            ] {
                let mut router = Router::new();
                let composition = Composition::for_testing(&config, &mut router);
                let logs = logs_of(|| {
                    composition.warn_if_process_local_throttle(service, throttle.as_ref());
                });
                let warned = environment == Environment::Production && name == "memory";
                assert_eq!(
                    logs.matches("WARN").count(),
                    usize::from(warned),
                    "{service} with the {name} throttle in {environment:?}:\n{logs}"
                );
                if warned {
                    for expected in [
                        format!("auth: {service} attempts are throttled in this process only"),
                        "forge::auth::MemoryLoginThrottle".to_owned(),
                        "every restart or deploy resets it".to_owned(),
                        "forge::auth::PgLoginThrottle".to_owned(),
                        format!("service=\"{service}\""),
                        "environment=\"production\"".to_owned(),
                    ] {
                        assert!(logs.contains(&expected), "{expected}:\n{logs}");
                    }
                }
            }
        }
    }
}

/// `FORGE_AUTH_SESSION_IDLE_TIMEOUT`'s default and bounds are the ones the
/// session store accepts, so every configured value builds a store.
#[test]
fn the_configured_idle_timeout_is_one_the_session_store_accepts() {
    assert_eq!(
        DEFAULT_SESSION_IDLE_TIMEOUT,
        SessionIdleTimeout::DEFAULT.get()
    );
    assert_eq!(
        AppConfig::default().auth.session_idle_timeout,
        SessionIdleTimeout::default().get()
    );
    let lookup = |value: &'static str| {
        move |key: &str| (key == "FORGE_AUTH_SESSION_IDLE_TIMEOUT").then(|| value.to_owned())
    };
    for (value, bound) in [
        ("5m", SessionIdleTimeout::MIN),
        ("720h", SessionIdleTimeout::MAX),
    ] {
        let config = AppConfig::from_lookup(lookup(value)).expect("in bounds");
        assert_eq!(config.auth.session_idle_timeout, bound);
        assert!(SessionIdleTimeout::new(config.auth.session_idle_timeout).is_ok());
    }
    for value in ["4m59s", "720h1s"] {
        assert!(AppConfig::from_lookup(lookup(value)).is_err(), "{value}");
    }
    assert!(SessionIdleTimeout::new(SessionIdleTimeout::MIN - Duration::from_secs(1)).is_err());
    assert!(SessionIdleTimeout::new(SessionIdleTimeout::MAX + Duration::from_secs(1)).is_err());
}
