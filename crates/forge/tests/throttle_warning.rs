//! The production warning for attempt throttles that count in one process
//! (Forge-Go's `auth.warnProcessLocalThrottle`).

use std::cell::RefCell;
use std::io;
use std::sync::{Arc, Once};

use async_trait::async_trait;
use forge::Composition;
use forge::auth::{
    LoginThrottle, MemoryLoginThrottle, PgLoginThrottle, ThrottleConfig, ThrottleDecision,
    ThrottleError, ThrottleKey,
};
use forge::config::{AppConfig, Environment};
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
