//! Process-wide `tracing` subscriber installation.

use std::io::IsTerminal;

use forge_config::{LogConfig, LogFormat};
use forge_telemetry::LogLayer;
use thiserror::Error;
use tracing_subscriber::{
    EnvFilter, Layer as _, filter::ParseError, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

/// `FORGE_LOG` could not be parsed as filter directives.
///
/// `ParseError`'s message already embeds its own cause, so it is displayed
/// inline rather than exposed as a source to avoid repeating it.
#[derive(Debug, Error)]
#[error("invalid value for FORGE_LOG: {filter:?}: {reason}")]
pub(crate) struct InvalidLogFilter {
    filter: String,
    reason: ParseError,
}

/// Installs the global subscriber writing to standard output and, when
/// `export` is given (`FORGE_TELEMETRY_LOGS_EXPORTER=otlp`, ADR 0033), also
/// queueing every line for OTLP export. `FORGE_LOG` filters both: the
/// exporter never receives a line standard output does not print.
///
/// If a global subscriber is already installed (for example by a test
/// harness embedding the application) it is kept and this call succeeds.
pub(crate) fn init(config: &LogConfig, export: Option<LogLayer>) -> Result<(), InvalidLogFilter> {
    install(config, export, false)
}

/// Like [`init`], writing to standard error: for commands whose standard
/// output is data, such as `openapi`.
pub(crate) fn init_stderr(config: &LogConfig) -> Result<(), InvalidLogFilter> {
    install(config, None, true)
}

fn install(
    config: &LogConfig,
    export: Option<LogLayer>,
    stderr: bool,
) -> Result<(), InvalidLogFilter> {
    let filter = EnvFilter::try_new(&config.filter).map_err(|reason| InvalidLogFilter {
        filter: config.filter.clone(),
        reason,
    })?;
    let output = match (config.format, stderr) {
        (LogFormat::Json, false) => tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .boxed(),
        (LogFormat::Json, true) => tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .boxed(),
        (LogFormat::Text, false) => tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(std::io::stdout().is_terminal())
            .boxed(),
        (LogFormat::Text, true) => tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr)
            .boxed(),
    };
    let installed = tracing_subscriber::registry()
        .with(output)
        .with(export)
        .with(filter)
        .try_init();
    // `try_init` fails only when a global subscriber already exists; keep it.
    drop(installed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_filter_is_rejected() {
        let config = LogConfig {
            filter: "my_app=loudest".to_owned(),
            format: LogFormat::Text,
        };

        let error = init(&config, None).expect_err("invalid level must be rejected");

        assert!(error.to_string().contains("FORGE_LOG"));
    }

    #[test]
    fn repeated_initialization_does_not_panic() {
        for format in [LogFormat::Json, LogFormat::Text] {
            let config = LogConfig {
                filter: "info,forge=debug".to_owned(),
                format,
            };
            init(&config, None).expect("valid filter must initialize");
        }
    }
}
