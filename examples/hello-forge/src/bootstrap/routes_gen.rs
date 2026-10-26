// forge:checksum 1f2854e4162b1b52a4f6ec867b8fce570f20335d11a05f1e63a7feccb7d2d140
//! Routes the generators mount. `forge generate scaffold`, `forge generate
//! resource` and `forge generate controller` write this file, and `forge
//! destroy` takes their blocks back out; each block starts with a `forge:`
//! comment. Do not edit it: the generators refuse to change it once its
//! checksum no longer matches. Mount your own routes in `compose`
//! (src/bootstrap/mod.rs), which calls this function once.

/// Mounts the routes of the generated resources and controllers.
pub(super) fn generated_routes(
    _composition: &mut forge::Composition<'_>,
    _site: &std::sync::Arc<crate::adapters::http::views::Site>,
) -> Result<(), forge::ComposeError> {
    Ok(())
}
