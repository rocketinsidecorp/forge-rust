//! Composition root: selects concrete implementations and owns the process
//! lifecycle. Forge provides the runtime, configuration, structured logging,
//! health routes and graceful shutdown.

use std::process::ExitCode;

/// Runs the command selected on the command line (serve by default).
pub fn run() -> ExitCode {
    forge::App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .compose(compose)
        .run()
}

/// Wires the HTTP adapter: the site (digested assets and the HTML request
/// layers) and the routes. `forge generate scaffold` prints the lines that
/// wire each generated resource at the end of this function.
fn compose(composition: &mut forge::Composition<'_>) -> Result<(), forge::ComposeError> {
    let site = crate::adapters::http::site(composition.router())?;
    crate::adapters::http::routes(composition.router(), &site)?;
    Ok(())
}
