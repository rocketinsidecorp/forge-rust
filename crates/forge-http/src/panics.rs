//! Keeping the messages of request handler panics out of logs and stderr.
//!
//! A handler's panic message can quote request data (`expect` on a form
//! field, a formatted assertion over a payload), and "never expose secrets
//! in logs" is a project rule. The server logs a caught panic with its
//! source location and payload type at `error` and the message only at
//! `debug`; this module supplies the location, which only the panic hook
//! sees, and keeps the standard hook from printing the message.
//!
//! As in `forge-jobs` (`panics.rs`), the hook is installed once per process
//! and wraps the hook present at that moment: a panic raised while a request
//! is being handled inside [`scoped`] records its location and prints
//! nothing (the server's log line reports it); every other panic goes to the
//! previous hook unchanged. Install your own hook before creating a
//! [`Server`](crate::Server) if it must also see request panics.

use std::{cell::RefCell, future::Future, sync::Once};

tokio::task_local! {
    static SITE: RefCell<Option<String>>;
}

static INSTALL: Once = Once::new();

/// Installs the request panic hook (idempotent).
pub(crate) fn install() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|location| format!("{}:{}", location.file(), location.line()));
            let in_request = SITE
                .try_with(|site| {
                    if let Ok(mut site) = site.try_borrow_mut() {
                        *site = location;
                    }
                })
                .is_ok();
            if !in_request {
                previous(info);
            }
        }));
    });
}

/// Runs `future` so that a panic inside it is withheld from the hook's
/// output; returns its output and the location of the last panic raised in
/// it (`file:line`), if any.
pub(crate) async fn scoped<F: Future>(future: F) -> (F::Output, Option<String>) {
    SITE.scope(RefCell::new(None), async {
        let output = future.await;
        let site = SITE.with(|site| site.borrow_mut().take());
        (output, site)
    })
    .await
}

/// The type of a panic payload, for the log: `&str` (`panic!` with a
/// literal), `String` (a formatted message) or `other`.
pub(crate) fn payload_type(payload: &(dyn std::any::Any + Send)) -> &'static str {
    if payload.is::<&'static str>() {
        "&str"
    } else if payload.is::<String>() {
        "String"
    } else {
        "other"
    }
}
