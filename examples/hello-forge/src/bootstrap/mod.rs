//! Composition root: selects concrete implementations and owns the process
//! lifecycle. Forge provides the runtime, configuration, structured logging,
//! health routes and graceful shutdown.

mod routes_gen;

use std::process::ExitCode;

/// Runs the command selected on the command line (serve by default).
pub fn run() -> ExitCode {
    forge::App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .compose(compose)
        .run()
}

/// Wires the HTTP adapter: the translations of `config/locales`, the site
/// (digested assets and the HTML request layers) and the routes. `forge
/// generate scaffold`, `resource` and `controller` mount theirs in
/// `routes_gen.rs`, which this function calls last.
fn compose(composition: &mut forge::Composition<'_>) -> Result<(), forge::ComposeError> {
    let translator = crate::adapters::http::views::locales::translator(composition.config())
        .map_err(forge::ComposeError::other)?;
    let site = crate::adapters::http::site(composition.router(), translator)?;
    crate::adapters::http::routes(composition.router(), &site)?;
    // The routes the generators mount (src/bootstrap/routes_gen.rs).
    routes_gen::generated_routes(composition, &site)?;
    Ok(())
}
