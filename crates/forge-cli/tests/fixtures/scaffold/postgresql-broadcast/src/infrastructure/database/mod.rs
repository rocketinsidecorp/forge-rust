//! PostgreSQL infrastructure: least-privilege tenant transaction setup.

pub mod events;
pub mod posts;

use forge::tenancy::TenantContext;
use sqlx::{PgPool, Postgres, Transaction};

/// Starts a PostgreSQL transaction and installs tenant/principal settings only
/// for that transaction.
///
/// RLS policies should read these values with current_setting(..., true). The
/// third set_config argument is true, so pooled connections do not retain tenant
/// identity after commit/rollback.
pub async fn begin_tenant_transaction<'a>(
    pool: &'a PgPool,
    context: &TenantContext,
) -> Result<Transaction<'a, Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;

    sqlx::query(
        "SELECT set_config('forge.tenant_id', $1, true), \
         set_config('forge.principal_id', $2, true)",
    )
    .bind(context.tenant_id().to_string())
    .bind(context.principal_id().to_string())
    .execute(&mut *transaction)
    .await?;

    Ok(transaction)
}
