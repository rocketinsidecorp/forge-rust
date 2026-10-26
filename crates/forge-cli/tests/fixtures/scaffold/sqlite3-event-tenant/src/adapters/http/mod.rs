//! HTTP adapter. Handlers stay thin: decode the request, call a use case and
//! map the result to a response, usually an HTML page rendered from a view
//! model in `views` (or a `303` redirect after a form submission).

pub mod access;
pub mod api_access;
pub mod events;
pub mod events_api;
pub mod status;
pub mod views;

use std::sync::Arc;

use forge::ComposeError;
use forge::http::{Method, Request, Response, Router};
use forge::i18n::Translator;

use self::views::{Site, welcome::WelcomeIndex};

/// Whether the welcome page links to the sign-in pages.
const SIGN_IN: bool = false;

/// Loads the site and registers the digested assets and the request layers
/// every HTML page relies on (the request's locale, Content-Security-Policy,
/// CSRF protection with the `_method` override, flash messages). Every page
/// renders its layout from the returned site; `translator` is
/// `views::locales::translator(config)`.
pub fn site(router: &mut Router, translator: Translator) -> Result<Arc<Site>, ComposeError> {
    let site = Arc::new(Site::load(translator)?);
    site.register(router).map_err(ComposeError::other)?;
    Ok(site)
}

/// Registers the application's routes. Forge adds `/health`, `/health/live`
/// and `/health/ready` itself.
pub fn routes(router: &mut Router, site: &Arc<Site>) -> Result<(), ComposeError> {
    let index_site = Arc::clone(site);
    router.route(Method::GET, "/", move |request: Request| {
        let site = Arc::clone(&index_site);
        async move { index(&site, &request) }
    })?;
    status::routes(router)?;
    Ok(())
}

fn index(site: &Site, request: &Request) -> Response {
    forge::web::html(&WelcomeIndex {
        layout: site.layout(request),
        logo: site.logo(),
        application: env!("CARGO_PKG_NAME"),
        sign_in: SIGN_IN,
    })
}
