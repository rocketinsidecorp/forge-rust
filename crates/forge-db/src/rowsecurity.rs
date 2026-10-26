//! Refusing a PostgreSQL serving role that could escape row-level security
//! (ADR 0011, ADR 0044; Forge-Go's `postgres.RequireRowSecurity`, its ADR
//! 0046).

use forge_config::Environment;
use sqlx::{Postgres, Row as _};

use crate::{Database, DbError};

/// Describes the login role (`session_user`) and the role in effect
/// (`current_user`): their own `SUPERUSER`, `BYPASSRLS`, `CREATEROLE` and
/// `REPLICATION` attributes; membership in another role with `SUPERUSER`,
/// `BYPASSRLS` or `REPLICATION` (attributes are not inherited, but a member
/// can `SET ROLE`); membership in a predefined role that reaches the server's
/// files or programs (`pg_execute_server_program`, `pg_write_server_files`,
/// `pg_read_server_files`, a path to superuser or to every tenant's data);
/// and whether it owns, or is a member of the owner of, a table of this
/// database with row-level security enabled (an owner can switch `FORCE` off
/// with `ALTER TABLE`; `pg_has_role(u, u)` is true, so direct ownership
/// counts too). Every row also carries both names, so a role in effect that
/// has no row still counts.
///
/// It runs with `search_path` pinned to `pg_catalog` (see
/// [`refuse_row_security_bypass`]), so the operators (`=`, `IN`, `<>`)
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
                 AND pg_catalog.pg_has_role(u.oid, c.relowner, 'MEMBER'))
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
    ///   or
    /// - owns, or is a member of the owner of, a table of this database with
    ///   row-level security enabled (an owner can `ALTER TABLE ... NO FORCE
    ///   ROW LEVEL SECURITY`),
    ///
    /// or when the two differ: a role setting (`options=-c role=...`,
    /// `ALTER ROLE ... SET role`) makes the session act as another role, and
    /// `SET ROLE NONE` returns it to the login role, so only a login as the
    /// serving role itself passes. Any failure to run the check (an
    /// unreachable server, a catalog that does not describe the login role)
    /// is an error too: it fails closed. In development and test it does
    /// nothing, so local tools may connect as an administrator.
    ///
    /// `serve`, `jobs work` and the `jobs` operator commands of an
    /// application declared with `forge::App::postgres` call it before using
    /// the pool. Serve as a `NOSUPERUSER NOBYPASSRLS NOCREATEROLE` runtime
    /// role that owns no table and migrate as a separate role
    /// (`FORGE_MIGRATION_DATABASE_URL`, ADR 0011).
    ///
    /// The role query runs in a `READ ONLY` transaction with `SET LOCAL
    /// search_path = pg_catalog`; the connection's own `search_path` is back
    /// after the rollback.
    pub async fn require_row_security(&self, environment: Environment) -> Result<(), DbError> {
        if matches!(environment, Environment::Development | Environment::Test) {
            return Ok(());
        }
        refuse_row_security_bypass(self).await
    }
}

/// Runs [`ROLE_ESCAPE_QUERY`] in a read-only transaction whose `search_path`
/// is `pg_catalog` alone and judges the rows with [`judge`]. Any failure,
/// including starting the transaction or pinning the path, is returned.
async fn refuse_row_security_bypass(database: &Database<Postgres>) -> Result<(), DbError> {
    let mut transaction = database.pool().begin_with("BEGIN READ ONLY").await?;
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

    use crate::{Database, DbError, testing::RlsTestDatabase};

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

    /// Opens a pool on `url` and runs the check for `environment`.
    async fn check(url: &SecretString, environment: Environment) -> Result<(), DbError> {
        let database = Database::<Postgres>::connect(url).await?;
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
