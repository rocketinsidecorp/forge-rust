// forge:checksum ee802c308bc05d0c7906e1639fc801ffd93f35f86cd4c1292a63bb949aabe883
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
    // forge:resource products
    {
        let products =
            crate::application::use_cases::products::ProductService::new(std::sync::Arc::new(
                crate::infrastructure::database::products::PgProductRepository::new(
                    composition.postgres()?.clone(),
                ),
            ));
        // products JSON API (/v1/products): tenant-scoped. The bearer token of
        // POST /auth/login names the tenant; RBAC denies every role `policy` does
        // not name: the `owner` role (`forge setup` gives it to the first member) may
        // read and write.
        let mut policy = forge::security::RbacPolicy::new();
        let owner = forge::security::Role::new("owner").map_err(forge::ComposeError::other)?;
        for permission in [
            crate::application::use_cases::products::READ_PERMISSION,
            crate::application::use_cases::products::WRITE_PERMISSION,
        ] {
            let permission =
                forge::security::Permission::new(permission).map_err(forge::ComposeError::other)?;
            policy.allow(owner.clone(), permission);
        }
        let pool = composition.postgres()?.pool().clone();
        // Tokens end after FORGE_AUTH_SESSION_IDLE_TIMEOUT without a request.
        let bearer_sessions =
            crate::infrastructure::bearer_sessions::PostgresBearerSessionStore::new(pool.clone())
                .with_idle_timeout(
                    forge::auth::SessionIdleTimeout::new(
                        composition.config().auth.session_idle_timeout,
                    )
                    .map_err(forge::ComposeError::other)?,
                );
        crate::adapters::http::products_api::routes(
            composition.router(),
            products,
            std::sync::Arc::new(crate::adapters::http::api_access::BearerTenantAccess::new(
                std::sync::Arc::new(bearer_sessions),
                std::sync::Arc::new(crate::infrastructure::tenancy::PgMembershipStore::new(pool)),
                policy,
            )),
        )?;
    }
    Ok(())
}
