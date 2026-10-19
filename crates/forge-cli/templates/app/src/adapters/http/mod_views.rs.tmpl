//! HTTP adapter. Handlers stay thin: decode the request, call a use case and
//! map the result to a response, usually an HTML page rendered from a view
//! model in `views` (or a `303` redirect after a form submission).

pub mod views;

use std::sync::Arc;

use forge::ComposeError;
use forge::http::{Method, Request, Response, Router};

use self::views::{Site, welcome::WelcomeIndex};

/// Registers the digested assets, the request layers every HTML page relies
/// on (Content-Security-Policy, CSRF protection with the `_method` override,
/// flash messages) and the application's routes. Forge adds `/health`,
/// `/health/live` and `/health/ready` itself.
pub fn routes(router: &mut Router) -> Result<(), ComposeError> {
    let site = Arc::new(Site::load()?);
    site.register(router).map_err(ComposeError::other)?;

    let index_site = Arc::clone(&site);
    router.route(Method::GET, "/", move |request: Request| {
        let site = Arc::clone(&index_site);
        async move { index(&site, &request) }
    })?;
    Ok(())
}

fn index(site: &Site, request: &Request) -> Response {
    forge::web::html(&WelcomeIndex {
        layout: site.layout(request, "Welcome"),
        application: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    })
}
