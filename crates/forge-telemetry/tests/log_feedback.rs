//! A failing log export reports the failure on standard output only: the
//! diagnostic never enters the log queue, so a broken collector cannot feed
//! itself (Forge-Go ADR 0040).

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use forge_telemetry::{
    Options, Telemetry,
    testing::{Behavior, Receiver},
};
use http::HeaderMap;
use tracing_subscriber::layer::SubscriberExt as _;

const FLUSH: Duration = Duration::from_secs(10);

/// Counts the telemetry's own export diagnostics.
struct Diagnostics(Arc<Mutex<usize>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Diagnostics {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "forge_telemetry::export" {
            *self.0.lock().expect("lock") += 1;
        }
    }
}

#[test]
fn a_failing_log_export_does_not_feed_itself() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let receiver = runtime.block_on(Receiver::start(Behavior::Always(400)));
    let mut options = Options::new("svc");
    options.logs = Some(receiver.client(HeaderMap::new(), false, FLUSH));
    let telemetry = Telemetry::new(options).expect("valid");
    let diagnostics = Arc::new(Mutex::new(0));
    let subscriber = tracing_subscriber::registry()
        .with(telemetry.log_layer().expect("layer"))
        .with(Diagnostics(Arc::clone(&diagnostics)));
    // Alone in this binary, so the subscriber can be global: the export
    // task's diagnostic then reaches the log layer whatever thread it runs on.
    tracing::subscriber::set_global_default(subscriber).expect("first subscriber");
    runtime.block_on(async {
        telemetry.start();
        tracing::info!("one line");
        telemetry.force_flush(FLUSH).await;
        telemetry.force_flush(FLUSH).await;
    });
    assert_eq!(
        *diagnostics.lock().expect("lock"),
        1,
        "the failure was reported"
    );
    assert_eq!(
        receiver.attempts(),
        1,
        "only the application's line was exported"
    );
    let stats = telemetry.log_stats();
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.exported, 0);
    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    assert!(
        text.contains("forge_telemetry_export_errors_total{signal=\"logs\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("forge_telemetry_logs_dropped_total 1"),
        "{text}"
    );
}
