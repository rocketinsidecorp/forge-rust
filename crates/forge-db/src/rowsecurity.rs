//! Refusing a PostgreSQL serving role that could escape row-level security
//! (ADR 0011, ADR 0044; Forge-Go's `postgres.RequireRowSecurity`, its ADR
//! 0046).

use forge_config::Environment;
use sqlx::{PgConnection, Pool, Postgres, Row as _, pool::PoolOptions};

use crate::{Database, DbError};

/// Describes the login role (`session_user`) and the role in effect
/// (`current_user`): their own `SUPERUSER`, `BYPASSRLS`, `CREATEROLE` and
/// `REPLICATION` attributes; membership in another role with `SUPERUSER`,
/// `BYPASSRLS` or `REPLICATION` (attributes are not inherited, but a member
/// can `SET ROLE`); membership in a predefined role that reaches the server's
/// files or programs (`pg_execute_server_program`, `pg_write_server_files`,
/// `pg_read_server_files`, a path to superuser or to every tenant's data);
/// whether it owns, or is a member of the owner of, a table of this
/// database with row-level security enabled (an owner can switch `FORCE` off
/// with `ALTER TABLE`; `pg_has_role(u, u)` is true, so direct ownership
/// counts too); whether it, or a role it is a member of, holds `TRUNCATE`,
/// `REFERENCES` or `TRIGGER` on such a table (`has_table_privilege` counts
/// `PUBLIC` and inherited grants; the membership join adds roles it can only
/// `SET ROLE` to, and `has_any_column_privilege` adds a column-level
/// `REFERENCES` grant); and whether it, or a role it is a member of, holds
/// any privilege on a partition without row-level security whose ancestors
/// (`pg_partition_ancestors`) include a table with it. Every row also
/// carries both names, so a role in effect that has no row still counts.
/// The predicates are Forge-Go's `roleEscapeQuery`.
///
/// It runs with `search_path` pinned to `pg_catalog` (see
/// [`check_connection`]), so the operators (`=`, `IN`, `<>`)
/// resolve to PostgreSQL's own, never to an operator, function or view a
/// `search_path` setting in the URL or on the role would put first; catalog
/// names and functions are also qualified, as defense in depth.
const ROLE_ESCAPE_QUERY: &str = "\
SELECT u.rolname, session_user, current_user, u.rolsuper, u.rolbypassrls, u.rolcreaterole,
       u.rolreplication,
       EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
               WHERE (r.rolsuper OR r.rolbypassrls OR r.rolreplication) AND r.oid <> u.oid
                 AND pg_catalog.pg_has_role(u.oid, r.oid, 'MEMBER')),
       EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
               WHERE r.rolname IN ('pg_execute_server_program', 'pg_write_server_files',
                                   'pg_read_server_files')
                 AND pg_catalog.pg_has_role(u.oid, r.oid, 'MEMBER')),
       EXISTS (SELECT 1 FROM pg_catalog.pg_class c
               WHERE c.relrowsecurity
                 AND pg_catalog.pg_has_role(u.oid, c.relowner, 'MEMBER')),
       EXISTS (SELECT 1 FROM pg_catalog.pg_class c, pg_catalog.pg_roles r
               WHERE c.relrowsecurity
                 AND pg_catalog.pg_has_role(u.oid, r.oid, 'MEMBER')
                 AND (pg_catalog.has_table_privilege(r.oid, c.oid, 'TRUNCATE, REFERENCES, TRIGGER')
                      OR pg_catalog.has_any_column_privilege(r.oid, c.oid, 'REFERENCES'))),
       EXISTS (SELECT 1 FROM pg_catalog.pg_class c, pg_catalog.pg_roles r
               WHERE c.relispartition AND NOT c.relrowsecurity
                 AND EXISTS (SELECT 1 FROM pg_catalog.pg_partition_ancestors(c.oid) a
                             JOIN pg_catalog.pg_class p ON p.oid = a.relid
                             WHERE p.relrowsecurity)
                 AND pg_catalog.pg_has_role(u.oid, r.oid, 'MEMBER')
                 AND (pg_catalog.has_table_privilege(r.oid, c.oid,
                          'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
                      OR pg_catalog.has_any_column_privilege(r.oid, c.oid, 'SELECT, INSERT, UPDATE, REFERENCES')))
FROM pg_catalog.pg_roles u
WHERE u.rolname IN (session_user, current_user)";

/// The fix every refusal names.
const FIX: &str = "set FORGE_DATABASE_URL to log in directly as a NOSUPERUSER NOBYPASSRLS \
     NOCREATEROLE NOREPLICATION runtime role that owns no table and has only data \
     privileges, and migrate with FORGE_MIGRATION_DATABASE_URL (docs/database.md, \
     \"Database roles\")";

impl Database<Postgres> {
    /// Refuses, in every environment except development and test (so in
    /// production), a pool whose connections could escape row-level
    /// security. Both the role `FORGE_DATABASE_URL` logs in as
    /// (`session_user`) and the role it acts as (`current_user`) are checked,
    /// and the call fails with [`DbError::RowSecurityBypass`] when either
    ///
    /// - is a superuser or has `BYPASSRLS` or `REPLICATION` (logical decoding
    ///   reads every tenant's changes),
    /// - is a member of such a role (it can `SET ROLE` to it),
    /// - has `CREATEROLE` (before PostgreSQL 16 it can grant itself such a
    ///   membership),
    /// - is a member of `pg_execute_server_program`, `pg_write_server_files`
    ///   or `pg_read_server_files` (paths to superuser or to the data files),
    /// - owns, or is a member of the owner of, a table of this database with
    ///   row-level security enabled (an owner can `ALTER TABLE ... NO FORCE
    ///   ROW LEVEL SECURITY`),
    /// - holds `TRUNCATE`, `REFERENCES` (on the table or on any of its
    ///   columns) or `TRIGGER` on such a table, itself, through `PUBLIC` or
    ///   through a role it is a member of (none of the three is subject to
    ///   row-level security: `TRUNCATE` empties every tenant's rows, a
    ///   foreign key probes them and a trigger sees them; the usual cause is
    ///   a `GRANT ALL`), or
    /// - holds any privilege on a partition that has no row-level security
    ///   of its own but belongs to a partitioned table with it (querying the
    ///   partition directly skips the parent's policies),
    ///
    /// or when the two differ: a role setting (`options=-c role=...`,
    /// `ALTER ROLE ... SET role`) makes the session act as another role, and
    /// `SET ROLE NONE` returns it to the login role, so only a login as the
    /// serving role itself passes. Membership counts whatever its `INHERIT`
    /// and `SET` options: a role granted `WITH INHERIT FALSE, SET FALSE` is
    /// refused all the same (it fails closed; do not grant the serving role
    /// such memberships). Any failure to run the check (an unreachable
    /// server, a catalog that does not describe the login role) is an error
    /// too: it fails closed. In development and test it does nothing, so
    /// local tools may connect as an administrator.
    ///
    /// Every pool already runs this check unless its
    /// [`PoolSettings`](crate::PoolSettings) name development or test or opt
    /// out; this method runs it on demand, on a connection of its own (so a
    /// lazy pool's refusal is reported at once, not as a pool timeout).
    /// `serve`, `jobs work` and the `jobs` operator commands of an
    /// application declared with `forge::App::postgres` call it before using
    /// the pool. Serve as a `NOSUPERUSER NOBYPASSRLS NOCREATEROLE` runtime
    /// role that owns no table and has only data privileges, and migrate as
    /// a separate role (`FORGE_MIGRATION_DATABASE_URL`, ADR 0011).
    ///
    /// The role query runs in a `READ ONLY` transaction with `SET LOCAL
    /// search_path = pg_catalog`; the connection's own `search_path` is back
    /// after the rollback.
    pub async fn require_row_security(&self, environment: Environment) -> Result<(), DbError> {
        if matches!(environment, Environment::Development | Environment::Test) {
            return Ok(());
        }
        // The pool's own connection settings, without its hooks: a lazy pool
        // that checks each new connection would retry a refused one until
        // its acquire timeout and report only the timeout.
        let probe = PoolOptions::<Postgres>::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(self.pool().options().get_acquire_timeout())
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_lazy_with((*self.pool().connect_options()).clone());
        let result = check_pool(&probe).await;
        probe.close().await;
        result
    }
}

/// Runs the check on a connection of `pool`.
pub(crate) async fn check_pool(pool: &Pool<Postgres>) -> Result<(), DbError> {
    let mut connection = pool.acquire().await?;
    check_connection(&mut connection).await
}

/// Runs [`ROLE_ESCAPE_QUERY`] on `connection` in a read-only transaction
/// whose `search_path` is `pg_catalog` alone and judges the rows with
/// [`judge`]. Any failure, including starting the transaction or pinning the
/// path, is returned.
pub(crate) async fn check_connection(connection: &mut PgConnection) -> Result<(), DbError> {
    let mut transaction = sqlx::Connection::begin_with(connection, "BEGIN READ ONLY").await?;
    sqlx::query("SET LOCAL search_path = pg_catalog")
        .execute(&mut *transaction)
        .await?;
    let rows = sqlx::query(ROLE_ESCAPE_QUERY)
        .fetch_all(&mut *transaction)
        .await?
        .iter()
        .map(|row| {
            Ok(RoleRow {
                role: row.try_get(0)?,
                session_user: row.try_get(1)?,
                current_user: row.try_get(2)?,
                superuser: row.try_get(3)?,
                bypass_rls: row.try_get(4)?,
                create_role: row.try_get(5)?,
                replication: row.try_get(6)?,
                member_of_bypassing_role: row.try_get(7)?,
                member_of_server_file_role: row.try_get(8)?,
                owns_row_security_table: row.try_get(9)?,
                holds_unconfined_privilege: row.try_get(10)?,
                reaches_unprotected_partition: row.try_get(11)?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    // Read only: nothing to keep, and a failed rollback says nothing about
    // the role. SQLx then queues another rollback for the connection's next
    // use, so the pinned search_path never reaches a later query.
    drop(transaction.rollback().await);
    judge(&rows)
}

/// One row of [`ROLE_ESCAPE_QUERY`].
#[derive(Clone, Debug, Default)]
struct RoleRow {
    role: String,
    session_user: String,
    current_user: String,
    superuser: bool,
    bypass_rls: bool,
    create_role: bool,
    replication: bool,
    member_of_bypassing_role: bool,
    member_of_server_file_role: bool,
    owns_row_security_table: bool,
    holds_unconfined_privilege: bool,
    reaches_unprotected_partition: bool,
}

impl RoleRow {
    /// Why this role could escape row-level security, if it could.
    fn escape(&self) -> Option<&'static str> {
        if self.superuser {
            Some("is a superuser")
        } else if self.bypass_rls {
            Some("has BYPASSRLS")
        } else if self.create_role {
            Some("has CREATEROLE")
        } else if self.replication {
            Some("has REPLICATION")
        } else if self.member_of_bypassing_role {
            Some("can SET ROLE to a superuser, BYPASSRLS or REPLICATION role")
        } else if self.member_of_server_file_role {
            Some(
                "is a member of pg_execute_server_program, pg_write_server_files or \
                 pg_read_server_files",
            )
        } else if self.owns_row_security_table {
            Some("owns, or can SET ROLE to the owner of, a table with row-level security")
        } else if self.holds_unconfined_privilege {
            Some(
                "holds TRUNCATE, REFERENCES or TRIGGER, which row-level security does not \
                 confine, on a table with row-level security, itself, through PUBLIC or through \
                 a role it belongs to (REVOKE TRUNCATE, REFERENCES, TRIGGER ON ALL TABLES IN \
                 SCHEMA public FROM each of them)",
            )
        } else if self.reaches_unprotected_partition {
            Some(
                "holds a privilege on a partition without row-level security of a table with \
                 row-level security, so querying the partition directly skips the parent's \
                 policies (ENABLE and FORCE ROW LEVEL SECURITY with the same policies on every \
                 partition, or revoke the privileges on it)",
            )
        } else {
            None
        }
    }
}

/// Accepts only a login role that is the role in effect and could not
/// escape row-level security. It fails closed: no row for the login role
/// (`pg_roles` always holds `session_user`, so an empty answer means a
/// catalog that cannot be read as expected, never a harmless role) or rows
/// that disagree on the session's roles are refused too.
fn judge(rows: &[RoleRow]) -> Result<(), DbError> {
    let refuse = |what: String| {
        Err(DbError::RowSecurityBypass {
            reason: format!("refusing to serve outside development and test {what}; {FIX}"),
        })
    };
    let Some(first) = rows.first() else {
        return refuse(
            "because the role FORGE_DATABASE_URL logs in as was not found in pg_roles".to_owned(),
        );
    };
    let (login, acting) = (&first.session_user, &first.current_user);
    if rows.len() > 2
        || rows
            .iter()
            .any(|row| row.session_user != *login || row.current_user != *acting)
    {
        return refuse("because pg_roles described the session's roles inconsistently".to_owned());
    }
    let Some(login_row) = rows.iter().find(|row| row.role == *login) else {
        return refuse(
            "because the role FORGE_DATABASE_URL logs in as was not found in pg_roles".to_owned(),
        );
    };
    // The login role first: it is what `SET ROLE NONE` returns to.
    let acting_row = rows.iter().find(|row| row.role == *acting);
    for row in std::iter::once(login_row).chain(acting_row) {
        if let Some(reason) = row.escape() {
            return refuse(format!("as PostgreSQL role {:?}, which {reason}", row.role));
        }
    }
    if login != acting {
        return refuse(format!(
            "as PostgreSQL role {login:?} acting as {acting:?} (a role setting such as \
             options=-c role=), which SET ROLE NONE undoes"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(role: &str) -> RoleRow {
        RoleRow {
            role: role.to_owned(),
            session_user: role.to_owned(),
            current_user: role.to_owned(),
            ..RoleRow::default()
        }
    }

    fn refused(rows: &[RoleRow], expected: &str) {
        match judge(rows) {
            Err(DbError::RowSecurityBypass { reason }) => {
                assert!(reason.contains(expected), "{reason}");
                assert!(reason.contains("FORGE_DATABASE_URL"), "{reason}");
                assert!(reason.contains("FORGE_MIGRATION_DATABASE_URL"), "{reason}");
            }
            other => panic!("{rows:?}: {other:?}, want a refusal naming {expected:?}"),
        }
    }

    #[test]
    fn a_plain_login_role_acting_as_itself_passes() {
        assert!(judge(&[row("app_runtime")]).is_ok());
    }

    #[test]
    fn no_row_fails_closed() {
        refused(&[], "was not found in pg_roles");
        // Rows for another role only: the login role is still missing.
        let mut other = row("someone");
        other.session_user = "app_runtime".to_owned();
        other.current_user = "someone".to_owned();
        refused(&[other], "was not found in pg_roles");
    }

    #[test]
    fn every_escape_is_refused_with_the_role_named() {
        for (change, reason) in [
            (
                (|row: &mut RoleRow| row.superuser = true) as fn(&mut RoleRow),
                "is a superuser",
            ),
            (|row| row.bypass_rls = true, "has BYPASSRLS"),
            (|row| row.create_role = true, "has CREATEROLE"),
            (|row| row.replication = true, "has REPLICATION"),
            (|row| row.member_of_bypassing_role = true, "can SET ROLE"),
            (
                |row| row.member_of_server_file_role = true,
                "pg_execute_server_program",
            ),
            (
                |row| row.owns_row_security_table = true,
                "owner of, a table with row-level security",
            ),
            (
                |row| row.holds_unconfined_privilege = true,
                "holds TRUNCATE, REFERENCES or TRIGGER, which row-level security does not confine",
            ),
            (
                |row| row.reaches_unprotected_partition = true,
                "holds a privilege on a partition without row-level security",
            ),
        ] {
            let mut escaping = row("app_runtime");
            change(&mut escaping);
            refused(&[escaping], reason);
            refused(
                &[row("x")].map(|mut r| {
                    change(&mut r);
                    r
                }),
                "\"x\"",
            );
        }
    }

    #[test]
    fn acting_as_another_role_is_refused_and_the_login_role_is_judged_first() {
        let (mut login, mut acting) = (row("postgres"), row("app_runtime"));
        for row in [&mut login, &mut acting] {
            row.session_user = "postgres".to_owned();
            row.current_user = "app_runtime".to_owned();
        }
        refused(
            &[acting.clone(), login.clone()],
            "\"postgres\" acting as \"app_runtime\"",
        );
        login.superuser = true;
        refused(
            &[acting.clone(), login.clone()],
            "role \"postgres\", which is a superuser",
        );
        // The role in effect's own escape is named when the login is plain.
        login.superuser = false;
        acting.bypass_rls = true;
        refused(
            &[login.clone(), acting.clone()],
            "role \"app_runtime\", which has BYPASSRLS",
        );
        // A role in effect without a row still counts as a switch.
        acting.bypass_rls = false;
        refused(&[login], "acting as");
    }

    #[test]
    fn inconsistent_rows_fail_closed() {
        let mut stray = row("other");
        stray.session_user = "other".to_owned();
        refused(&[row("app_runtime"), stray], "inconsistently");
        refused(
            &[row("app_runtime"), row("app_runtime"), row("app_runtime")],
            "inconsistently",
        );
    }

    #[test]
    fn role_names_are_quoted_so_they_cannot_forge_log_lines() {
        let mut hostile = row("evil\nINFO forged");
        hostile.superuser = true;
        refused(&[hostile], r#""evil\nINFO forged""#);
    }
}

/// Against a real server (`FORGE_TEST_DATABASE_URL`, a superuser), with the
/// roles of [`RlsTestDatabase`](crate::testing::RlsTestDatabase).
#[cfg(all(test, feature = "testing"))]
mod database_tests {
    use forge_config::{Environment, SecretString};
    use forge_tenancy::TenantId;
    use sqlx::{AssertSqlSafe, Postgres};
    use uuid::Uuid;

    use crate::{
        Database, DbError, PoolSettings,
        testing::{RlsTestDatabase, test_settings},
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("tests/migrations/postgres-rls");

    const SERVED: [Environment; 1] = [Environment::Production];

    fn admin_url() -> Result<String, Box<dyn std::error::Error>> {
        Ok(std::env::var("FORGE_TEST_DATABASE_URL")?)
    }

    /// `url` with `options=-c <setting>` (libpq's spelling) added.
    fn with_setting(url: &str, setting: &str) -> SecretString {
        let separator = if url.contains('?') { '&' } else { '?' };
        let encoded = setting.replace('=', "%3D").replace(',', "%2C");
        SecretString::new(format!("{url}{separator}options=-c%20{encoded}"))
    }

    /// Opens a pool on `url` and runs the check for `environment`. The pool
    /// itself is a test pool (no check of its own), so this is the explicit
    /// check alone.
    async fn check(url: &SecretString, environment: Environment) -> Result<(), DbError> {
        let database = Database::<Postgres>::connect_with(url, test_settings()).await?;
        let result = database.require_row_security(environment).await;
        database.close().await;
        result
    }

    #[track_caller]
    fn assert_refused(name: &str, result: Result<(), DbError>, reason: &str, secret: &str) {
        match result {
            Err(error @ DbError::RowSecurityBypass { .. }) => {
                let message = error.to_string();
                assert!(message.contains(reason), "{name}: {message}");
                assert!(message.contains("FORGE_DATABASE_URL"), "{name}: {message}");
                assert!(!message.contains(secret), "{name} leaks the URL: {message}");
                assert_eq!(error.code(), "row_security_bypass");
            }
            other => panic!("{name}: {other:?}, want a refusal naming {reason:?}"),
        }
    }

    async fn exec(database: &Database<Postgres>, statement: String) -> TestResult {
        sqlx::query(AssertSqlSafe(statement))
            .execute(database.pool())
            .await?;
        Ok(())
    }

    /// The user-info of `url`, which no error may contain.
    fn credentials(url: &str) -> Result<String, Box<dyn std::error::Error>> {
        url.split_once("://")
            .and_then(|(_, rest)| rest.split_once('@'))
            .map(|(credentials, _)| credentials.to_owned())
            .ok_or_else(|| "a URL without credentials".into())
    }

    /// PostgreSQL's `42501`, also raised for a row that violates a row-level
    /// security policy.
    fn insufficient_privilege(error: &sqlx::Error) -> bool {
        matches!(error, sqlx::Error::Database(error) if error.code().as_deref() == Some("42501"))
    }

    async fn insert(
        database: &Database<Postgres>,
        tenant: TenantId,
        row: TenantId,
    ) -> Result<(), DbError> {
        let mut transaction = database.begin().await?;
        transaction.set_tenant(tenant).await?;
        sqlx::query("INSERT INTO notes (id, tenant_id, body) VALUES ($1, $2, 'x')")
            .bind(Uuid::now_v7())
            .bind(row)
            .execute(transaction.connection())
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn count(
        database: &Database<Postgres>,
        tenant: Option<TenantId>,
    ) -> Result<i64, DbError> {
        let mut transaction = database.begin().await?;
        if let Some(tenant) = tenant {
            transaction.set_tenant(tenant).await?;
        }
        let count = sqlx::query_scalar("SELECT count(*) FROM notes")
            .fetch_one(transaction.connection())
            .await?;
        transaction.rollback().await?;
        Ok(count)
    }

    /// The runtime role is what an application serves with: row-level
    /// security confines it to the transaction's tenant even with no tenant
    /// predicate in the SQL, it sees nothing without a tenant, and it holds
    /// no DDL privilege with which to switch the policy off.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn the_runtime_role_is_confined_by_row_level_security() -> TestResult {
        let test = RlsTestDatabase::create([&MIGRATIONS]).await?;
        let runtime = test.runtime();
        let (tenant_a, tenant_b) = (TenantId::new(), TenantId::new());
        insert(runtime, tenant_a, tenant_a).await?;
        insert(runtime, tenant_b, tenant_b).await?;
        insert(runtime, tenant_b, tenant_b).await?;

        assert_eq!(count(runtime, Some(tenant_a)).await?, 1);
        assert_eq!(count(runtime, Some(tenant_b)).await?, 2);
        assert_eq!(count(runtime, None).await?, 0, "no tenant: nothing visible");
        // The owner is subject to the policy too (FORCE); the admin, a
        // superuser, is not.
        assert_eq!(count(test.migrator(), Some(tenant_a)).await?, 1);
        assert_eq!(count(test.admin(), None).await?, 3);

        let foreign = insert(runtime, tenant_a, tenant_b).await;
        assert!(
            matches!(foreign, Err(DbError::Query(ref error)) if insufficient_privilege(error)),
            "tenant A inserting a row of tenant B: {foreign:?}"
        );

        for statement in [
            "CREATE TABLE intruder (id int)",
            "CREATE TEMPORARY TABLE scratch (id int)",
            "ALTER TABLE notes ADD COLUMN intruder text",
            "ALTER TABLE notes DISABLE ROW LEVEL SECURITY",
            "ALTER TABLE notes NO FORCE ROW LEVEL SECURITY",
            "DROP POLICY notes_tenant_isolation ON notes",
            "TRUNCATE notes",
            "DROP TABLE notes",
        ] {
            let result = sqlx::query(statement).execute(runtime.pool()).await;
            assert!(
                result.as_ref().is_err_and(insufficient_privilege),
                "the runtime role ran {statement}: {result:?}"
            );
        }
        assert_eq!(count(test.admin(), None).await?, 3);
        test.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn require_row_security_refuses_bypassing_roles_outside_development_and_test()
    -> TestResult {
        let test = RlsTestDatabase::create([&MIGRATIONS]).await?;
        let runtime_url = test.runtime_url().clone();
        let runtime = test.runtime_role().to_owned();
        let secret = credentials(runtime_url.expose())?;
        let admin = test.admin();

        for environment in SERVED {
            check(&runtime_url, environment).await?;
        }
        // The pool itself still works after the check (search_path is back).
        assert_eq!(count(test.runtime(), None).await?, 0);

        // The test server's own user, on a fresh database.
        let own = crate::testing::TestDatabase::<Postgres>::create_empty().await?;
        let own_url = own.url().expose().to_owned();
        if is_superuser(own.database()).await? {
            let own_secret = credentials(&own_url)?;
            for environment in SERVED {
                assert_refused(
                    "superuser",
                    check(own.url(), environment).await,
                    "is a superuser",
                    &own_secret,
                );
            }
            // Logged in as the superuser but acting as the runtime role:
            // current_user alone looks harmless, SET ROLE NONE would not be.
            let separator = if own_url.contains('?') { '&' } else { '?' };
            for url in [
                with_setting(&own_url, &format!("role={runtime}")),
                SecretString::new(format!("{own_url}{separator}options[role]={runtime}")),
            ] {
                assert_refused(
                    "superuser acting as the runtime role",
                    check(&url, Environment::Production).await,
                    "is a superuser",
                    &own_secret,
                );
            }
        }
        for environment in [Environment::Development, Environment::Test] {
            check(own.url(), environment).await?;
        }
        own.cleanup().await?;

        let role = format!("\"{runtime}\"");
        exec(admin, format!("ALTER ROLE {role} BYPASSRLS")).await?;
        assert_refused(
            "BYPASSRLS",
            check(&runtime_url, Environment::Production).await,
            "has BYPASSRLS",
            &secret,
        );
        check(&runtime_url, Environment::Development).await?;
        exec(admin, format!("ALTER ROLE {role} NOBYPASSRLS")).await?;

        exec(admin, format!("ALTER ROLE {role} CREATEROLE")).await?;
        assert_refused(
            "CREATEROLE",
            check(&runtime_url, Environment::Production).await,
            "has CREATEROLE",
            &secret,
        );
        exec(admin, format!("ALTER ROLE {role} NOCREATEROLE")).await?;

        // Logical decoding reads every tenant's changes (wal_level=logical).
        exec(admin, format!("ALTER ROLE {role} REPLICATION")).await?;
        assert_refused(
            "REPLICATION",
            check(&runtime_url, Environment::Production).await,
            "has REPLICATION",
            &secret,
        );
        exec(admin, format!("ALTER ROLE {role} NOREPLICATION")).await?;

        // Membership, direct or through another role, in a role that is a
        // superuser or has BYPASSRLS or REPLICATION: the member can SET ROLE
        // to it.
        let suffix = Uuid::now_v7().simple().to_string();
        let names = ["bypass", "super", "middle", "replication", "plain"]
            .map(|kind| format!("forge_test_{kind}_{suffix}"));
        let _dropped = Roles(names.to_vec());
        let [bypass, superuser_role, middle, replication, plain] =
            names.clone().map(|name| format!("\"{name}\""));
        {
            exec(admin, format!("CREATE ROLE {bypass} NOLOGIN BYPASSRLS")).await?;
            exec(
                admin,
                format!("CREATE ROLE {superuser_role} NOLOGIN SUPERUSER"),
            )
            .await?;
            exec(admin, format!("CREATE ROLE {middle} NOLOGIN")).await?;
            exec(
                admin,
                format!("CREATE ROLE {replication} NOLOGIN REPLICATION"),
            )
            .await?;
            for (granted, member, what) in [
                (&bypass, &role, "direct BYPASSRLS"),
                (&superuser_role, &role, "direct superuser"),
                (&middle, &role, "inherited BYPASSRLS"),
                (&replication, &role, "direct REPLICATION"),
            ] {
                exec(admin, format!("GRANT {granted} TO {member}")).await?;
                if granted == &middle {
                    exec(admin, format!("GRANT {bypass} TO {middle}")).await?;
                }
                assert_refused(
                    what,
                    check(&runtime_url, Environment::Production).await,
                    "can SET ROLE to a superuser, BYPASSRLS or REPLICATION role",
                    &secret,
                );
                exec(admin, format!("REVOKE {granted} FROM {member}")).await?;
            }
            // PostgreSQL 16 grants without inheriting the privileges: the
            // member can still SET ROLE (and `SET FALSE` is refused too,
            // conservatively: membership alone counts).
            let version: i32 =
                sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
                    .fetch_one(admin.pool())
                    .await?;
            if version >= 160_000 {
                for option in ["INHERIT FALSE", "INHERIT FALSE, SET FALSE"] {
                    exec(admin, format!("GRANT {bypass} TO {role} WITH {option}")).await?;
                    assert_refused(
                        option,
                        check(&runtime_url, Environment::Production).await,
                        "can SET ROLE to a superuser, BYPASSRLS or REPLICATION role",
                        &secret,
                    );
                    exec(admin, format!("REVOKE {bypass} FROM {role}")).await?;
                }
            }
        }

        // Predefined roles that reach the server's programs and files: a
        // path to superuser, or to every tenant's data files.
        for predefined in [
            "pg_execute_server_program",
            "pg_write_server_files",
            "pg_read_server_files",
        ] {
            exec(admin, format!("GRANT {predefined} TO {role}")).await?;
            assert_refused(
                predefined,
                check(&runtime_url, Environment::Production).await,
                "is a member of pg_execute_server_program, pg_write_server_files or \
                 pg_read_server_files",
                &secret,
            );
            exec(admin, format!("REVOKE {predefined} FROM {role}")).await?;
        }

        // The owner of a table with row-level security can switch FORCE off:
        // being it, or able to SET ROLE to it, is refused.
        let migrator = format!("\"{}\"", test.migrator_role());
        let owner = "owns, or can SET ROLE to the owner of, a table with row-level security";
        exec(admin, format!("GRANT {migrator} TO {role}")).await?;
        assert_refused(
            "member of the table owner",
            check(&runtime_url, Environment::Production).await,
            owner,
            &secret,
        );
        exec(admin, format!("REVOKE {migrator} FROM {role}")).await?;
        exec(admin, format!("ALTER TABLE notes OWNER TO {role}")).await?;
        assert_refused(
            "direct owner",
            check(&runtime_url, Environment::Production).await,
            owner,
            &secret,
        );
        exec(admin, format!("ALTER TABLE notes OWNER TO {migrator}")).await?;
        // Owning a table without row-level security is not an escape.
        exec(admin, "CREATE TABLE plain_owned (id int)".to_owned()).await?;
        exec(admin, format!("ALTER TABLE plain_owned OWNER TO {role}")).await?;
        check(&runtime_url, Environment::Production).await?;
        exec(admin, "DROP TABLE plain_owned".to_owned()).await?;

        // Two unprivileged roles, but the session acts as another role than
        // the one it logged in as: only a login as the serving role passes.
        let [.., plain_name] = &names;
        exec(admin, format!("CREATE ROLE {plain} NOLOGIN")).await?;
        exec(admin, format!("GRANT {plain} TO {role}")).await?;
        assert_refused(
            "runtime role acting as another plain role",
            check(
                &with_setting(runtime_url.expose(), &format!("role={plain_name}")),
                Environment::Production,
            )
            .await,
            "acting as",
            &secret,
        );
        // The same through a role setting rather than the URL.
        exec(
            admin,
            format!("ALTER ROLE {role} SET role = '{plain_name}'"),
        )
        .await?;
        assert_refused(
            "ALTER ROLE ... SET role",
            check(&runtime_url, Environment::Production).await,
            "acting as",
            &secret,
        );
        exec(admin, format!("ALTER ROLE {role} RESET role")).await?;
        exec(admin, format!("REVOKE {plain} FROM {role}")).await?;

        check(&runtime_url, Environment::Production).await?;
        test.cleanup().await?;
        Ok(())
    }

    /// TRUNCATE, REFERENCES and TRIGGER are not subject to row-level
    /// security: holding any of them on a table with it, by any path, is
    /// refused. The same grants on a table without row-level security are
    /// harmless and accepted.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn require_row_security_refuses_privileges_row_level_security_does_not_confine()
    -> TestResult {
        let test = RlsTestDatabase::create([&MIGRATIONS]).await?;
        let runtime_url = test.runtime_url().clone();
        let secret = credentials(runtime_url.expose())?;
        let admin = test.admin();
        let role = format!("\"{}\"", test.runtime_role());
        let unconfined = "holds TRUNCATE, REFERENCES or TRIGGER, which row-level security does \
                          not confine, on a table with row-level security";
        let suffix = Uuid::now_v7().simple().to_string();
        let middle_name = format!("forge_test_grants_{suffix}");
        let _dropped = Roles(vec![middle_name.clone()]);
        let middle = format!("\"{middle_name}\"");
        exec(admin, format!("CREATE ROLE {middle} NOLOGIN")).await?;

        let mut cases = vec![
            (
                "TRUNCATE",
                format!("GRANT TRUNCATE ON notes TO {role}"),
                format!("REVOKE TRUNCATE ON notes FROM {role}"),
            ),
            (
                "REFERENCES",
                format!("GRANT REFERENCES ON notes TO {role}"),
                format!("REVOKE REFERENCES ON notes FROM {role}"),
            ),
            (
                "TRIGGER",
                format!("GRANT TRIGGER ON notes TO {role}"),
                format!("REVOKE TRIGGER ON notes FROM {role}"),
            ),
            (
                "column REFERENCES",
                format!("GRANT REFERENCES (tenant_id) ON notes TO {role}"),
                format!("REVOKE REFERENCES (tenant_id) ON notes FROM {role}"),
            ),
            (
                "ALL",
                format!("GRANT ALL ON notes TO {role}"),
                format!("REVOKE TRUNCATE, REFERENCES, TRIGGER ON notes FROM {role}"),
            ),
            (
                "PUBLIC",
                "GRANT TRUNCATE ON notes TO PUBLIC".to_owned(),
                "REVOKE TRUNCATE ON notes FROM PUBLIC".to_owned(),
            ),
            (
                "inherited",
                format!("GRANT TRUNCATE ON notes TO {middle}; GRANT {middle} TO {role}"),
                format!("REVOKE {middle} FROM {role}; REVOKE TRUNCATE ON notes FROM {middle}"),
            ),
        ];
        let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
            .fetch_one(admin.pool())
            .await?;
        if version >= 160_000 {
            // A membership that only allows SET ROLE: the privileges are one
            // statement away.
            cases.push((
                "SET-only membership",
                format!("GRANT TRIGGER ON notes TO {middle}; GRANT {middle} TO {role} WITH INHERIT FALSE"),
                format!("REVOKE {middle} FROM {role}; REVOKE TRIGGER ON notes FROM {middle}"),
            ));
        }
        for (name, grant, revoke) in cases {
            for statement in grant.split("; ") {
                exec(admin, statement.to_owned()).await?;
            }
            assert_refused(
                name,
                check(&runtime_url, Environment::Production).await,
                unconfined,
                &secret,
            );
            for statement in revoke.split("; ") {
                exec(admin, statement.to_owned()).await?;
            }
            check(&runtime_url, Environment::Production).await?;
        }

        // A table without row-level security: every privilege is accepted.
        exec(
            admin,
            "CREATE TABLE plain_lookup (id int PRIMARY KEY)".to_owned(),
        )
        .await?;
        exec(admin, format!("GRANT ALL ON plain_lookup TO {role}")).await?;
        exec(admin, "GRANT TRUNCATE ON plain_lookup TO PUBLIC".to_owned()).await?;
        check(&runtime_url, Environment::Production).await?;
        test.cleanup().await?;
        Ok(())
    }

    /// A partition without row-level security of a table with it is a way
    /// around the parent's policies: any privilege on it is refused, and
    /// protecting the partition makes the role acceptable again.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn require_row_security_refuses_an_unprotected_partition_of_a_protected_table()
    -> TestResult {
        let test = RlsTestDatabase::create([&MIGRATIONS]).await?;
        let runtime_url = test.runtime_url().clone();
        let secret = credentials(runtime_url.expose())?;
        let admin = test.admin();
        let role = format!("\"{}\"", test.runtime_role());
        for statement in [
            "CREATE TABLE events (tenant_id uuid NOT NULL, body text) PARTITION BY HASH (tenant_id)",
            "ALTER TABLE events ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE events FORCE ROW LEVEL SECURITY",
            "CREATE POLICY events_tenant_isolation ON events USING \
             (tenant_id = NULLIF(current_setting('forge.tenant_id', true), '')::uuid)",
            "CREATE TABLE events_0 PARTITION OF events FOR VALUES WITH (MODULUS 1, REMAINDER 0)",
        ] {
            exec(admin, statement.to_owned()).await?;
        }
        exec(admin, format!("GRANT SELECT ON events TO {role}")).await?;
        // Through the parent only: accepted.
        check(&runtime_url, Environment::Production).await?;
        for (name, grant, revoke) in [
            (
                "table privilege",
                format!("GRANT SELECT ON events_0 TO {role}"),
                format!("REVOKE SELECT ON events_0 FROM {role}"),
            ),
            (
                "column privilege",
                format!("GRANT SELECT (body) ON events_0 TO {role}"),
                format!("REVOKE SELECT (body) ON events_0 FROM {role}"),
            ),
            (
                "PUBLIC",
                "GRANT SELECT ON events_0 TO PUBLIC".to_owned(),
                "REVOKE SELECT ON events_0 FROM PUBLIC".to_owned(),
            ),
        ] {
            exec(admin, grant).await?;
            assert_refused(
                name,
                check(&runtime_url, Environment::Production).await,
                "holds a privilege on a partition without row-level security",
                &secret,
            );
            exec(admin, revoke).await?;
        }
        exec(admin, format!("GRANT SELECT ON events_0 TO {role}")).await?;
        exec(
            admin,
            "ALTER TABLE events_0 ENABLE ROW LEVEL SECURITY".to_owned(),
        )
        .await?;
        check(&runtime_url, Environment::Production).await?;
        test.cleanup().await?;
        Ok(())
    }

    /// Every pool checks its role unless its settings name development or
    /// test or opt out; no environment at all is checked. An eager pool is
    /// refused when it connects, a lazy one on every connection it opens.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn every_pool_checks_its_role_unless_development_test_or_the_opt_out() -> TestResult {
        let Some((own, url)) = superuser_database().await? else {
            return Ok(());
        };
        let url = SecretString::new(url);
        let secret = credentials(url.expose())?;
        let short = |settings: PoolSettings| {
            settings.with_acquire_timeout(std::time::Duration::from_secs(2))
        };
        for settings in [
            PoolSettings::default(),
            PoolSettings::default().with_environment(Environment::Production),
            PoolSettings::from_config(&forge_config::DatabaseConfig::default()),
        ] {
            match Database::<Postgres>::connect_with(&url, settings).await {
                Err(error) => assert_refused("eager pool", Err(error), "is a superuser", &secret),
                Ok(database) => {
                    database.close().await;
                    panic!("an eager superuser pool was accepted with {settings:?}");
                }
            }
            // A lazy pool connects on first use, and refuses that connection.
            let lazy = Database::<Postgres>::connect_lazy_with(&url, short(settings))?;
            let refused = lazy.ping().await;
            assert!(
                matches!(refused, Err(DbError::PoolTimeout)),
                "a lazy superuser pool answered: {refused:?}"
            );
            // The explicit check names the reason at once.
            assert_refused(
                "lazy pool, explicit check",
                lazy.require_row_security(Environment::Production).await,
                "is a superuser",
                &secret,
            );
            lazy.close().await;
        }
        for settings in [
            PoolSettings::default().with_environment(Environment::Development),
            test_settings(),
            PoolSettings::default()
                .with_environment(Environment::Production)
                .allow_row_security_bypass(),
        ] {
            Database::<Postgres>::connect_with(&url, settings)
                .await?
                .close()
                .await;
            let lazy = Database::<Postgres>::connect_lazy_with(&url, short(settings))?;
            lazy.ping().await?;
            lazy.close().await;
        }
        own.cleanup().await?;

        // The runtime role passes in every pool, eager and lazy.
        let test = RlsTestDatabase::create([&MIGRATIONS]).await?;
        let settings = PoolSettings::default().with_environment(Environment::Production);
        let eager = Database::<Postgres>::connect_with(test.runtime_url(), settings).await?;
        assert_eq!(count(&eager, None).await?, 0);
        eager.close().await;
        let lazy = Database::<Postgres>::connect_lazy_with(test.runtime_url(), settings)?;
        assert_eq!(count(&lazy, None).await?, 0);
        lazy.close().await;
        test.cleanup().await?;
        Ok(())
    }

    /// Run by [`pgoptions_never_reaches_a_pool_but_the_migrator`] in a child
    /// process whose environment sets `PGOPTIONS`, `PGAPPNAME` and `PGTZ`;
    /// a no-op anywhere else.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn child_with_session_variables() -> TestResult {
        let Ok(tenant) = std::env::var("FORGE_TEST_CHILD_TENANT") else {
            return Ok(());
        };
        let url = SecretString::new(admin_url()?);
        // The serving pool refuses to open, eager or lazy, in every
        // environment: SQLx would give each session the default tenant.
        for settings in [
            test_settings(),
            PoolSettings::default().allow_row_security_bypass(),
        ] {
            match Database::<Postgres>::connect_with(&url, settings).await {
                Err(DbError::Configuration { reason }) => {
                    assert!(reason.contains("PGOPTIONS is set"), "{reason}");
                    assert!(!reason.contains(&tenant), "{reason}");
                }
                Err(other) => panic!("{other:?}"),
                Ok(database) => {
                    // Without the refusal: the session has a default tenant.
                    let leaked: Option<String> = sqlx::query_scalar(
                        "SELECT NULLIF(current_setting('forge.tenant_id', true), '')",
                    )
                    .fetch_one(database.pool())
                    .await?;
                    database.close().await;
                    panic!("the serving pool opened; its sessions' default tenant: {leaked:?}");
                }
            }
            assert!(matches!(
                Database::<Postgres>::connect_lazy_with(&url, settings),
                Err(DbError::Configuration { .. })
            ));
        }
        // The migrator keeps the driver's behavior: the variables reach it.
        let migrator = Database::<Postgres>::connect_with(
            &url,
            test_settings().with_environment_session(true),
        )
        .await?;
        let (seen, name, zone): (Option<String>, String, String) = sqlx::query_as(
            "SELECT NULLIF(current_setting('forge.tenant_id', true), ''), \
             current_setting('application_name'), current_setting('TimeZone')",
        )
        .fetch_one(migrator.pool())
        .await?;
        migrator.close().await;
        assert_eq!(
            seen.as_deref(),
            Some(tenant.as_str()),
            "PGOPTIONS reaches the migrator"
        );
        assert_eq!(name, "forge-test-pgappname");
        // SQLx never reads PGTZ: every session is UTC.
        assert_eq!(zone, "UTC");
        Ok(())
    }

    /// `PGOPTIONS='-c forge.tenant_id=...'` would give every pooled session a
    /// default tenant, so a query outside a tenant transaction would read
    /// that tenant's rows instead of none: no pool but the migrator's opens
    /// while it is set. The variables are set for a child process (the test
    /// binary itself), never in this one.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn pgoptions_never_reaches_a_pool_but_the_migrator() -> TestResult {
        admin_url()?;
        let tenant = TenantId::new().to_string();
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "rowsecurity::database_tests::child_with_session_variables",
                "--include-ignored",
                "--test-threads=1",
            ])
            .env("FORGE_TEST_CHILD_TENANT", &tenant)
            .env("PGOPTIONS", format!("-c forge.tenant_id={tenant}"))
            .env("PGAPPNAME", "forge-test-pgappname")
            .env("PGTZ", "America/Sao_Paulo")
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Without PGOPTIONS the serving pool takes its name from the URL, else
        // `forge`, never from PGAPPNAME.
        let url = admin_url()?;
        let database =
            Database::<Postgres>::connect_with(&SecretString::new(url), test_settings()).await?;
        let name: String = sqlx::query_scalar("SELECT current_setting('application_name')")
            .fetch_one(database.pool())
            .await?;
        database.close().await;
        assert_eq!(name, "forge");
        Ok(())
    }

    /// A server that cannot be reached is refused, not waved through.
    #[tokio::test]
    async fn an_unreachable_server_fails_closed() -> TestResult {
        let database = Database::<Postgres>::connect_lazy_with(
            &SecretString::new("postgres://app_runtime:secret@127.0.0.1:1/shop"),
            crate::PoolSettings::default()
                .with_acquire_timeout(std::time::Duration::from_millis(200)),
        )?;
        let refused = database.require_row_security(Environment::Production).await;
        assert!(refused.is_err(), "{refused:?}");
        assert!(!format!("{refused:?}").contains("secret"));
        // Development and test do not connect at all.
        database
            .require_row_security(Environment::Development)
            .await?;
        database.require_row_security(Environment::Test).await?;
        Ok(())
    }

    async fn is_superuser(database: &Database<Postgres>) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user")
            .fetch_one(database.pool())
            .await
    }

    /// A fresh database as the test server's user, when that is a superuser.
    async fn superuser_database()
    -> Result<Option<(crate::testing::TestDatabase<Postgres>, String)>, Box<dyn std::error::Error>>
    {
        let test = crate::testing::TestDatabase::<Postgres>::create_empty().await?;
        if !is_superuser(test.database()).await? {
            test.cleanup().await?;
            return Ok(None);
        }
        let url = test.url().expose().to_owned();
        Ok(Some((test, url)))
    }

    /// A `pg_roles` view earlier on the search path, showing no role at all,
    /// does not hide the superuser: the check reads `pg_catalog.pg_roles`.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn require_row_security_ignores_a_shadowing_pg_roles_view() -> TestResult {
        let Some((test, url)) = superuser_database().await? else {
            return Ok(());
        };
        exec(
            test.database(),
            "CREATE VIEW public.pg_roles AS SELECT * FROM pg_catalog.pg_roles WHERE false"
                .to_owned(),
        )
        .await?;
        assert_refused(
            "shadowing view",
            check(
                &with_setting(&url, "search_path=public,pg_catalog"),
                Environment::Production,
            )
            .await,
            "is a superuser",
            &credentials(&url)?,
        );
        test.cleanup().await?;
        Ok(())
    }

    /// A user-defined `=` operator for `name`, earlier on a `search_path`
    /// setting, does not make the superuser look like a harmless role: the
    /// check pins `search_path` to `pg_catalog`, so its operators are
    /// PostgreSQL's own.
    #[tokio::test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL"]
    async fn require_row_security_ignores_a_shadowing_operator() -> TestResult {
        let Some((test, url)) = superuser_database().await? else {
            return Ok(());
        };
        // The harmless role every comparison would match. Roles are
        // cluster-wide.
        let harmless = format!("forge_test_harmless_{}", Uuid::now_v7().simple());
        let _dropped = Roles(vec![harmless.clone()]);
        let database = test.database();
        {
            exec(database, format!("CREATE ROLE \"{harmless}\" NOLOGIN")).await?;
            exec(
                database,
                format!(
                    "CREATE FUNCTION public.fake_eq(name, name) RETURNS boolean LANGUAGE sql \
                     IMMUTABLE AS $$ SELECT $1::text = '{harmless}' $$"
                ),
            )
            .await?;
            exec(
                database,
                "CREATE OPERATOR public.= (LEFTARG = name, RIGHTARG = name, FUNCTION = public.fake_eq)"
                    .to_owned(),
            )
            .await?;
            for url in [
                SecretString::new(url.clone()),
                with_setting(&url, "search_path=public,pg_catalog"),
            ] {
                assert_refused(
                    "shadowing operator",
                    check(&url, Environment::Production).await,
                    "is a superuser",
                    &credentials(url.expose())?,
                );
            }
        }
        test.cleanup().await?;
        Ok(())
    }

    /// Cluster-wide roles a test created, dropped when the test ends, panic
    /// or not (from a helper thread with its own runtime, like the
    /// databases of `TestDatabase`).
    struct Roles(Vec<String>);

    impl Drop for Roles {
        fn drop(&mut self) {
            let roles = std::mem::take(&mut self.0);
            let dropped = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async {
                    use sqlx::Connection as _;
                    let url = admin_url().map_err(|error| error.to_string())?;
                    let mut connection = sqlx::PgConnection::connect(&url)
                        .await
                        .map_err(|error| error.to_string())?;
                    for role in roles {
                        sqlx::query(AssertSqlSafe(format!("DROP ROLE IF EXISTS \"{role}\"")))
                            .execute(&mut connection)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    Ok::<(), String>(())
                })
            })
            .join();
            if !matches!(dropped, Ok(Ok(()))) {
                eprintln!("failed to drop the test roles: {dropped:?}");
            }
        }
    }
}
