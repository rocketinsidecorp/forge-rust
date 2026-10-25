//! Capturing what the lifecycle logs, for tests of warnings and errors.

use std::{
    io,
    sync::{Arc, Mutex, PoisonError},
};

use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;

/// What a subscriber wrote.
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

impl io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Records warnings and errors of the current thread until the guard drops.
pub(crate) fn capture() -> (Captured, DefaultGuard) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    (captured, tracing::subscriber::set_default(subscriber))
}

/// The warnings and errors `work` logs.
pub(crate) fn warnings(work: impl FnOnce()) -> String {
    let (captured, guard) = capture();
    work();
    drop(guard);
    captured.text()
}
