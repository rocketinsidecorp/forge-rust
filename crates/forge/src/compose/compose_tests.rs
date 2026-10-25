//! What a composition hook can register: workers, queues, telemetry, jobs
//! and the outbox.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::*;

#[tokio::test]
async fn workers_and_queues_registered_by_a_hook_run_and_stop_with_the_lifecycle() {
    let config = AppConfig::default();
    let mut router = Router::new();
    let mut composition = Composition::for_testing(&config, &mut router);
    let stopped = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stopped);
    composition.worker("watcher", move |mut shutdown| async move {
        shutdown.requested().await;
        seen.store(true, Ordering::SeqCst);
    });
    let handled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&handled);
    let queue = composition.work_queue("mail", 8, move |amount: usize| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(amount, Ordering::SeqCst);
        }
    });
    assert!(format!("{composition:?}").contains("workers: 2"));

    let running = composition.start_workers();
    queue.submit(5).expect("queued");
    queue.clone().submit(6).expect("a clone shares the queue");
    for _ in 0..1000 {
        if handled.load(Ordering::SeqCst) == 11 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(handled.load(Ordering::SeqCst), 11);
    running.stop().await;
    assert!(stopped.load(Ordering::SeqCst));
    assert!(
        queue.submit(1).is_err(),
        "the queue stopped with its worker"
    );
}

#[test]
fn the_composition_exposes_the_configuration_router_and_telemetry() {
    let config = AppConfig::default();
    let mut router = Router::new();
    let mut composition = Composition::for_testing(&config, &mut router);
    assert!(composition.telemetry().is_none());
    assert_eq!(composition.config().environment, config.environment);
    composition
        .router()
        .route(forge_http::Method::GET, "/composed", |_| async {
            forge_http::Response::empty(forge_http::StatusCode::NO_CONTENT)
        })
        .expect("route");

    let telemetry =
        forge_telemetry::Telemetry::new(forge_telemetry::Options::new("demo")).expect("telemetry");
    let composition = composition.with_telemetry(telemetry);
    assert!(composition.telemetry().is_some());
    assert!(format!("{composition:?}").contains("/composed"));
}

#[test]
fn undeclared_subsystems_say_which_declaration_is_missing() {
    let config = AppConfig::default();
    let mut router = Router::new();
    let mut composition = Composition::for_testing(&config, &mut router);
    #[cfg(feature = "jobs")]
    {
        let error = composition.jobs().expect_err("jobs are not declared");
        assert!(error.to_string().contains("App::jobs"), "{error}");
    }
    #[cfg(feature = "events")]
    {
        let error = composition.events().expect_err("events are not declared");
        assert!(error.to_string().contains("App::events"), "{error}");
    }
    let _ = &mut composition;
}

#[cfg(feature = "jobs")]
#[test]
fn a_testing_composition_can_be_given_the_job_registry() {
    let config = AppConfig::default();
    let mut router = Router::new();
    let mut jobs = forge_jobs::Jobs::new();
    let composition = Composition::for_testing(&config, &mut router).with_jobs(&mut jobs);
    let mut composition = composition;

    assert!(composition.jobs().expect("attached").is_empty());
}

#[cfg(feature = "events")]
#[test]
fn a_testing_composition_can_be_given_the_outbox_setup() {
    let config = AppConfig::default();
    let mut router = Router::new();
    let mut events = crate::background::EventsSetup::default();
    let mut composition = Composition::for_testing(&config, &mut router).with_events(&mut events);

    let setup = composition.events().expect("attached");
    setup.settings(forge_events::DispatcherSettings::default());
    setup.publish_to(forge_events::Subscribers::new());
    let debug = format!("{:?}", composition.events().expect("attached"));
    assert!(debug.contains("publisher: true"), "{debug}");
}
