// forge:checksum deb4e923c8ee0ba2693cd26480f2c89ce394e5ca1ca1c662e5c173ac3199786a
//! Routes the generators mount. `forge generate scaffold`, `forge generate
//! resource` and `forge generate controller` write this file, and `forge
//! destroy` takes their blocks back out; each block starts with a `forge:`
//! comment. Do not edit it: the generators refuse to change it once its
//! checksum no longer matches. Mount your own routes in `compose`
//! (src/bootstrap/mod.rs), which calls this function once.

/// Mounts the routes of the generated resources and controllers.
pub(super) fn generated_routes(
    composition: &mut forge::Composition<'_>,
    site: &std::sync::Arc<crate::adapters::http::views::Site>,
) -> Result<(), forge::ComposeError> {
    // forge:scaffold posts
    {
        // posts: global. DenyAll refuses every request until you decide who may
        // use them: implement GlobalAccess (src/adapters/http/access.rs).
        let posts = crate::application::use_cases::posts::PostService::new(std::sync::Arc::new(
            crate::infrastructure::database::posts::PgPostRepository::new(
                composition.postgres()?.clone(),
            ),
        ));
        crate::adapters::http::posts::routes(
            composition.router(),
            site,
            posts.clone(),
            std::sync::Arc::new(crate::adapters::http::access::DenyAll),
        )?;
        // posts JSON API (/v1/posts): global. DenyAll refuses every request until
        // you decide who may use it: implement ApiGlobalAccess
        // (src/adapters/http/api_access.rs).
        crate::adapters::http::posts_api::routes(
            composition.router(),
            posts,
            std::sync::Arc::new(crate::adapters::http::api_access::DenyAll),
        )?;
    }
    Ok(())
}
