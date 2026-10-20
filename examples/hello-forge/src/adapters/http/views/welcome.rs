//! The welcome page served at `/`.

use askama::Template;

use super::Layout;

/// View model of `templates/welcome/index.html`.
#[derive(Template)]
#[template(path = "welcome/index.html")]
pub struct WelcomeIndex<'a> {
    pub layout: Layout<'a>,
    /// Digested URL of `assets/forge-rust-logo.png`.
    pub logo: &'a str,
    pub application: &'a str,
    pub version: &'a str,
}
