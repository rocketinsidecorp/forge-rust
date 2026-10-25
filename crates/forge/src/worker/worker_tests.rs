//! Worker lifecycle failures: a panicking worker, and a queue stopped with
//! items left.

use super::*;
use crate::test_support::capture;

#[tokio::test]
async fn a_panicking_worker_is_reported_and_does_not_stop_the_others() {
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let panicking = Worker {
        name: "panicking",
        run: Box::new(|_| Box::pin(async { panic!("worker bug") })),
    };
    let steady = Worker {
        name: "steady",
        run: Box::new(move |mut shutdown| {
            Box::pin(async move {
                shutdown.requested().await;
                let _ = finished_tx.send(());
            })
        }),
    };
    let (captured, guard) = capture();

    RunningWorkers::start(vec![panicking, steady]).stop().await;
    drop(guard);

    finished_rx
        .await
        .expect("the other worker saw the shutdown");
    let log = captured.text();
    assert!(log.contains("worker failed"), "{log}");
}

#[tokio::test]
async fn items_left_in_a_stopped_queue_are_dropped_and_counted() {
    let (queue, worker) = queue("mail", 4, |_: u8| async {});
    queue.submit(1).expect("queued");
    queue.submit(2).expect("queued");
    queue.submit(3).expect("queued");
    // Shutdown was requested before the worker ran: it takes nothing.
    let (_stop, receiver) = watch::channel(true);
    let (captured, guard) = capture();

    (worker.run)(Shutdown(receiver)).await;
    drop(guard);

    let log = captured.text();
    assert!(log.contains("work queue stopped with items left"), "{log}");
    assert!(log.contains("dropped=3") && log.contains("mail"), "{log}");
    assert_eq!(queue.submit(4), Err(SubmitError::Stopped { queue: "mail" }));
    assert_eq!(
        SubmitError::Stopped { queue: "mail" }.to_string(),
        "work queue mail has stopped"
    );
}

#[tokio::test]
async fn a_dropped_lifecycle_counts_as_shutdown() {
    let (stop, receiver) = watch::channel(false);
    let mut shutdown = Shutdown(receiver);
    drop(stop);
    // The sender is gone: waiting must not hang.
    shutdown.requested().await;
    assert!(!shutdown.is_requested());
}
