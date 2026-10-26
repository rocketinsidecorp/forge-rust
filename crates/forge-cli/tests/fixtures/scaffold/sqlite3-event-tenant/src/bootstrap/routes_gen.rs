// forge:checksum 0ce99c2ab7c5f77f9dcfe782776ce2d8907300a2600df345b272c35ed7cd0618
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
    // forge:scaffold events
    {
        // events: tenant-scoped. SQLite has no Forge session or membership persistence
        // yet (ADR 0021), so DenyAll refuses every request until you provide
        // SessionStore and MembershipStore implementations for SessionTenantAccess.
        let events = crate::application::use_cases::events::EventService::new(std::sync::Arc::new(
            crate::infrastructure::database::events::SqliteEventRepository::new(
                composition.sqlite()?.clone(),
            ),
        ));
        crate::adapters::http::events::routes(
            composition.router(),
            site,
            events.clone(),
            std::sync::Arc::new(crate::adapters::http::access::DenyAll),
        )?;
        // events JSON API (/v1/events): tenant-scoped. SQLite has no Forge
        // bearer sessions or memberships yet (ADR 0021), so DenyAll refuses every
        // request until you implement ApiTenantAccess (src/adapters/http/api_access.rs).
        crate::adapters::http::events_api::routes(
            composition.router(),
            events,
            std::sync::Arc::new(crate::adapters::http::api_access::DenyAll),
        )?;
    }
    Ok(())
}
