//! JSON log lines in the shape of Go's `log/slog` JSON handler, which is
//! what Forge-Go's forge-proxy writes to `docker logs forge-proxy`:
//! `{"time":"…","level":"INFO","msg":"request","service":"shop",…}`.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::clock::Timestamp;

/// A shared, line-atomic JSON logger.
#[derive(Clone)]
pub struct Logger {
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Logger")
    }
}

/// Log levels used by the proxy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Level {
    /// Routine events: requests, switches.
    Info,
    /// Recoverable problems: upstream errors, rejected deploys.
    Warn,
    /// Failures an operator must look at: state not saved.
    Error,
}

impl Level {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

impl Logger {
    /// Writes to `out`.
    #[must_use]
    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self {
            out: Arc::new(Mutex::new(out)),
        }
    }

    /// Discards everything.
    #[must_use]
    pub fn discard() -> Self {
        Self::new(Box::new(std::io::sink()))
    }

    /// Writes one line: time, level, msg, then `attrs` in order.
    pub fn log(&self, level: Level, message: &str, attrs: &[(&str, Value)]) {
        let mut line = String::with_capacity(160);
        line.push_str("{\"time\":");
        push_json(&mut line, &Value::String(Timestamp::now().rfc3339_millis()));
        line.push_str(",\"level\":");
        push_json(&mut line, &Value::String(level.as_str().to_owned()));
        line.push_str(",\"msg\":");
        push_json(&mut line, &Value::String(message.to_owned()));
        for (key, value) in attrs {
            line.push(',');
            push_json(&mut line, &Value::String((*key).to_owned()));
            line.push(':');
            push_json(&mut line, value);
        }
        line.push_str("}\n");
        // A poisoned lock means another writer panicked mid-line; keep
        // logging rather than lose every later line.
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }

    /// [`Level::Info`].
    pub fn info(&self, message: &str, attrs: &[(&str, Value)]) {
        self.log(Level::Info, message, attrs);
    }

    /// [`Level::Warn`].
    pub fn warn(&self, message: &str, attrs: &[(&str, Value)]) {
        self.log(Level::Warn, message, attrs);
    }

    /// [`Level::Error`].
    pub fn error(&self, message: &str, attrs: &[(&str, Value)]) {
        self.log(Level::Error, message, attrs);
    }
}

fn push_json(line: &mut String, value: &Value) {
    match serde_json::to_string(value) {
        Ok(text) => line.push_str(&text),
        Err(_) => line.push_str("null"),
    }
}

/// A `String` attribute.
#[must_use]
pub fn text(value: impl Into<String>) -> Value {
    Value::String(value.into())
}
