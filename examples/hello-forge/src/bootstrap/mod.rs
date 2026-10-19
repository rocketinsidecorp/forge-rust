//! Composition root: selects concrete implementations and owns the process
//! lifecycle. Forge provides the runtime, configuration, structured logging,
//! health routes and graceful shutdown.

use std::process::ExitCode;

/// Runs the command selected on the command line (serve by default).
pub fn run() -> ExitCode {
    forge::App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .compose(|composition| crate::adapters::http::routes(composition.router()))
        .run()
}
