// forge:checksum 9088a3b7657ec3ccad881889fb10c669c49fc297dd583f002625e99610747ecd
//! Routes the generators mount. `forge generate scaffold`, `forge generate
//! resource` and `forge generate controller` write this file, and `forge
//! destroy` takes their blocks back out; each block starts with a `forge:`
//! comment. Do not edit it: the generators refuse to change it once its
//! checksum no longer matches. Mount your own routes in `compose`
//! (src/bootstrap/mod.rs), which calls this function once.

// A guard the application declares itself (a `fn protect` in
// src/bootstrap/mod.rs) replaces the generated one below, which the
// generators then leave out. Declaring one while the generated one is still
// here makes the name ambiguous, a compile error rather than a guard silently
// ignored: run the generator again.
#[allow(unused_imports)]
use super::*;
use guards::*;

/// Mounts the routes of the generated resources and controllers.
pub(super) fn generated_routes(
    composition: &mut forge::Composition<'_>,
    site: &std::sync::Arc<crate::adapters::http::views::Site>,
) -> Result<(), forge::ComposeError> {
    // forge:streams streams
    // Live updates (--broadcast), once for every broadcasting resource. On
    // PostgreSQL every replica hears every change (LISTEN/NOTIFY); production
    // needs FORGE_STREAMS_KEY (`openssl rand -base64 32`, the same everywhere).
    let streams =
        crate::adapters::http::streams::Streams::register(composition, forge::Fanout::Postgres)?;
    // forge:scaffold events
    {
        // events: tenant-scoped, at /events of the tenant the session signed in to,
        // for roles granted these permissions (the guard grants the owner role).
        let access = protect(
            composition,
            &[
                crate::application::use_cases::events::READ_PERMISSION,
                crate::application::use_cases::events::WRITE_PERMISSION,
            ],
        )?;
        let events = crate::application::use_cases::events::EventService::new(std::sync::Arc::new(
            crate::infrastructure::database::events::PgEventRepository::new(
                composition.postgres()?.clone(),
            ),
        ));
        crate::adapters::http::events::routes(
            composition.router(),
            site,
            events.clone(),
            access.clone(),
            &streams,
        )?;
        // events JSON API (/v1/events): the same session cookie and policy as the
        // pages; changes send the session's CSRF token in X-CSRF-Token.
        crate::adapters::http::events_api::routes(composition.router(), events, access)?;
    }
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
            &streams,
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

/// The guards of the generated routes.
mod guards {
    // forge:guard protect
    /// The guard of tenant-scoped routes: sign-in, the CSRF token on changes, a
    /// membership in the session's tenant, and the `owner` role (`forge setup`)
    /// granted `permissions`. Declare `fn protect` with this signature in
    /// src/bootstrap/mod.rs to grant other roles; the generators then use yours.
    pub(super) fn protect(
        composition: &mut forge::Composition<'_>,
        permissions: &[&str],
    ) -> Result<
        std::sync::Arc<
            impl crate::adapters::http::access::TenantAccess
            + crate::adapters::http::api_access::ApiTenantAccess
            + 'static,
        >,
        forge::ComposeError,
    > {
        let mut policy = forge::security::RbacPolicy::new();
        let owner = forge::security::Role::new("owner").map_err(forge::ComposeError::other)?;
        for permission in permissions {
            let permission = forge::security::Permission::new(*permission)
                .map_err(forge::ComposeError::other)?;
            policy.allow(owner.clone(), permission);
        }
        let pool = composition.postgres()?.pool().clone();
        // Sessions end after FORGE_AUTH_SESSION_IDLE_TIMEOUT without a request.
        let sessions = crate::infrastructure::auth::PostgresSessionStore::new(pool.clone())
            .with_idle_timeout(
                forge::auth::SessionIdleTimeout::new(
                    composition.config().auth.session_idle_timeout,
                )
                .map_err(forge::ComposeError::other)?,
            );
        Ok(std::sync::Arc::new(
            crate::adapters::http::access::SessionTenantAccess::new(
                std::sync::Arc::new(sessions),
                std::sync::Arc::new(crate::infrastructure::tenancy::PgMembershipStore::new(pool)),
                policy,
            ),
        ))
    }
}
