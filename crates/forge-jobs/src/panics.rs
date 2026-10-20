//! Keeping panic messages of job attempts off stderr.
//!
//! The standard panic hook prints the panic message, and a handler's panic
//! message can quote job data (`expect` on a payload field, a formatted
//! assertion). The worker already records only "the job panicked" in the
//! database and logs; this module covers the hook.
//!
//! The panic hook is process-global, so Forge does not replace and later
//! restore it (restoring could discard a hook another component installs in
//! the meantime). [`redact_attempt_panics`] installs, once per process, a
//! hook that wraps the hook present at that moment: a panic raised inside a
//! [`redacted`] scope (every job attempt and outbox publication) prints only
//! its source location; every other panic goes to the previous hook
//! unchanged. Install your own hook before starting the job runtime if it
//! must also see attempt panics, or not at all.

use std::{future::Future, io::Write, sync::Once};

tokio::task_local! {
    static REDACTED: ();
}

static INSTALL: Once = Once::new();

/// Installs the redacting panic hook (idempotent). [`Jobs::run`](crate::Jobs::run)
/// and the outbox dispatcher call it; call it earlier to make sure it wraps
/// the hook you want for other panics.
pub fn redact_attempt_panics() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if REDACTED.try_with(|()| ()).is_ok() {
                let location = info
                    .location()
                    .map(|location| format!(" at {}:{}", location.file(), location.line()))
                    .unwrap_or_default();
                // Best effort: stderr is the last-resort channel.
                drop(writeln!(
                    std::io::stderr().lock(),
                    "a job attempt or event publication panicked{location}; \
                     the panic message is withheld because it may contain job data"
                ));
            } else {
                previous(info);
            }
        }));
    });
}

/// Runs `future` so that its panics are reported without their message by
/// the hook of [`redact_attempt_panics`].
pub async fn redacted<F: Future>(future: F) -> F::Output {
    REDACTED.scope((), future).await
}
