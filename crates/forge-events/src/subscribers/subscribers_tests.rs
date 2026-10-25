//! Routing to subscribers without a database.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use serde::{Deserialize, Serialize};

use super::*;
use crate::{EventId, MAX_NAME};

#[derive(Serialize, Deserialize)]
struct Paid;

impl IntegrationEvent for Paid {
    const TYPE: &'static str = "billing.paid";
}

fn envelope(event_type: &str) -> EventEnvelope {
    EventEnvelope {
        id: EventId::new(),
        event_type: event_type.to_owned(),
        version: 1,
        tenant_id: None,
        occurred_at: forge_core::Timestamp::now(),
        attempt: 1,
        payload: "{}".to_owned(),
    }
}

/// Records its name when it handles an event and answers `outcome`.
struct Named {
    name: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
    outcome: fn() -> Result<(), PublishError>,
}

#[async_trait]
impl EventSubscriber for Named {
    async fn handle(&self, _: &EventEnvelope) -> Result<(), PublishError> {
        if let Ok(mut log) = self.log.lock() {
            log.push(self.name);
        }
        (self.outcome)()
    }
}

fn named(
    name: &'static str,
    log: &Arc<Mutex<Vec<&'static str>>>,
    outcome: fn() -> Result<(), PublishError>,
) -> Named {
    Named {
        name,
        log: Arc::clone(log),
        outcome,
    }
}

#[tokio::test]
async fn an_event_without_subscribers_is_published_and_dropped() {
    let subscribers = Subscribers::new();
    assert!(subscribers.is_empty());
    assert_eq!(
        subscribers.publish(&envelope("billing.paid")).await.ok(),
        Some(())
    );
}

#[tokio::test]
async fn every_subscriber_of_the_type_runs_in_order_and_others_do_not() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut subscribers = Subscribers::new();
    subscribers
        .subscribe::<Paid>(named("first", &log, || Ok(())))
        .expect("valid type")
        .subscribe_type("billing.refunded", named("other", &log, || Ok(())))
        .expect("valid type")
        .subscribe::<Paid>(named("second", &log, || Ok(())))
        .expect("valid type");
    assert!(!subscribers.is_empty());

    subscribers
        .publish(&envelope("billing.paid"))
        .await
        .expect("all succeed");

    assert_eq!(*log.lock().expect("lock"), ["first", "second"]);
}

#[tokio::test]
async fn one_failure_fails_the_event_after_every_subscriber_ran_and_the_first_failure_wins() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut subscribers = Subscribers::new();
    subscribers
        .subscribe::<Paid>(named("a", &log, || Ok(())))
        .expect("valid type")
        .subscribe::<Paid>(named("b", &log, || Err(PublishError::retry("b is down"))))
        .expect("valid type")
        .subscribe::<Paid>(named("c", &log, || {
            Err(PublishError::permanent("c refuses"))
        }))
        .expect("valid type");

    let failure = subscribers
        .publish(&envelope("billing.paid"))
        .await
        .expect_err("a subscriber failed");

    assert_eq!(failure.message(), "b is down");
    assert!(!failure.is_permanent());
    assert_eq!(
        *log.lock().expect("lock"),
        ["a", "b", "c"],
        "the others still ran"
    );
}

#[test]
fn invalid_event_types_are_refused_and_nothing_is_registered() {
    struct Nobody(Arc<AtomicUsize>);

    #[async_trait]
    impl EventSubscriber for Nobody {
        async fn handle(&self, _: &EventEnvelope) -> Result<(), PublishError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let mut subscribers = Subscribers::new();
    for invalid in [
        "",
        "Billing.Paid",
        "has space",
        "-leading",
        &"a".repeat(MAX_NAME + 1),
    ] {
        let error = subscribers
            .subscribe_type(invalid, Nobody(Arc::clone(&calls)))
            .expect_err(invalid);
        assert!(
            matches!(
                error,
                EventsError::InvalidName {
                    kind: "event type",
                    ..
                }
            ),
            "{error:?}"
        );
    }
    assert!(subscribers.is_empty());
}

#[test]
fn the_debug_form_lists_the_types_with_their_subscriber_counts_in_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut subscribers = Subscribers::new();
    subscribers
        .subscribe_type("b.event", named("x", &log, || Ok(())))
        .expect("valid")
        .subscribe_type("a.event", named("y", &log, || Ok(())))
        .expect("valid")
        .subscribe_type("a.event", named("z", &log, || Ok(())))
        .expect("valid");

    let debug = format!("{subscribers:?}");

    assert_eq!(
        debug,
        "Subscribers { routes: [(\"a.event\", 2), (\"b.event\", 1)] }"
    );
}
