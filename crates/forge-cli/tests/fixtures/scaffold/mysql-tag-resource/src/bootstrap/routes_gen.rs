// forge:checksum 5ce68ff31131b35a476e7dc805242b02496a26cb1167f33f0ca7a25858c22406
//! Routes the generators mount. `forge generate scaffold`, `forge generate
//! resource` and `forge generate controller` write this file, and `forge
//! destroy` takes their blocks back out; each block starts with a `forge:`
//! comment. Do not edit it: the generators refuse to change it once its
//! checksum no longer matches. Mount your own routes in `compose`
//! (src/bootstrap/mod.rs), which calls this function once.

/// Mounts the routes of the generated resources and controllers.
pub(super) fn generated_routes(
    composition: &mut forge::Composition<'_>,
    _site: &std::sync::Arc<crate::adapters::http::views::Site>,
) -> Result<(), forge::ComposeError> {
    // forge:resource tags
    {
        let tags = crate::application::use_cases::tags::TagService::new(std::sync::Arc::new(
            crate::infrastructure::database::tags::MySqlTagRepository::new(
                composition.mysql()?.clone(),
            ),
        ));
        // tags JSON API (/v1/tags): global. DenyAll refuses every request until
        // you decide who may use it: implement ApiGlobalAccess
        // (src/adapters/http/api_access.rs).
        crate::adapters::http::tags_api::routes(
            composition.router(),
            tags,
            std::sync::Arc::new(crate::adapters::http::api_access::DenyAll),
        )?;
    }
    Ok(())
}
