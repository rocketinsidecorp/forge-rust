//! The log span exporter writes each ended span as one event, and a failing
//! final metric push is counted and reported on the telemetry's own target.
//! Alone in this binary so one global subscriber sees the export tasks, which
//! run on other threads.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use forge_telemetry::{
    Options, SpanKind, SpanSink, Telemetry,
    testing::{Behavior, Receiver},
};
use http::HeaderMap;
use tracing_subscriber::layer::SubscriberExt as _;

const FLUSH: Duration = Duration::from_secs(10);

/// Collects the target and fields of every event.
struct Events(Arc<Mutex<Vec<String>>>);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields(format!("{} ", event.metadata().target()));
        event.record(&mut fields);
        self.0.lock().expect("lock").push(fields.0);
    }
}

#[test]
fn spans_go_to_the_log_and_a_failed_final_metric_push_is_reported() {
    let events = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(Events(Arc::clone(&events))),
    )
    .expect("first subscriber");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let receiver = runtime.block_on(Receiver::start(Behavior::Always(400)));
    let mut options = Options::new("svc");
    options.spans = Some(SpanSink::Log);
    options.metrics = Some(receiver.client(HeaderMap::new(), false, FLUSH));
    options.metrics_interval = Duration::from_secs(3600);
    let telemetry = Telemetry::new(options).expect("valid");
    {
        let _guard = runtime.enter();
        telemetry.start();
    }
    let tracer = telemetry.tracer("test");
    let parent = tracer.start("parent", SpanKind::Server, None);
    let child = tracer.start("child", SpanKind::Internal, Some(parent.context()));
    child.end();
    parent.end();
    runtime.block_on(async {
        telemetry.force_flush(FLUSH).await;
        telemetry
            .shutdown(tokio::time::Instant::now() + FLUSH)
            .await
            .expect("shuts down in time");
    });

    let events = events.lock().expect("lock");
    let spans: Vec<&String> = events
        .iter()
        .filter(|event| event.contains("name=child") || event.contains("name=parent"))
        .collect();
    assert_eq!(spans.len(), 2, "{events:?}");
    let child = spans
        .iter()
        .find(|event| event.contains("name=child"))
        .expect("child");
    assert!(
        child.contains("trace_id=") && child.contains("duration_ms="),
        "{child}"
    );
    assert!(
        !child.contains("parent_span_id= "),
        "the child names its parent: {child}"
    );
    let parent = spans
        .iter()
        .find(|event| event.contains("name=parent"))
        .expect("parent");
    assert!(
        parent.contains("parent_span_id= ") || parent.contains("parent_span_id=\"\""),
        "{parent}"
    );
    // The 400s (the flush's push and the final snapshot) are counted, not retried.
    let deadline = Instant::now() + FLUSH;
    while !events.iter().any(|event| {
        event.contains("forge_telemetry::export") && event.contains("final metric export failed")
    }) {
        assert!(Instant::now() < deadline, "{events:?}");
        std::thread::yield_now();
    }
    assert_eq!(receiver.attempts(), 2);
    let text = forge_telemetry::prometheus::render(telemetry.metrics());
    assert!(
        text.contains("forge_telemetry_export_errors_total{signal=\"metrics\"} 2"),
        "{text}"
    );
    drop(receiver);
    runtime.shutdown_timeout(Duration::from_secs(1));
}
