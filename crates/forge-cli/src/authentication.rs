//! `forge generate authentication`: adds the sign-in, sign-out and
//! password-reset pages and the `setup` command to an existing full-stack
//! PostgreSQL application (ADR 0026), like `rails generate authentication`.
//!
//! The files are byte-identical to the ones `forge new` writes, including
//! the pages' translations (`config/locales/authentication.en.json`, plus
//! the Spanish and Brazilian Portuguese files when the application has those
//! locales). The generator writes only new files, all or nothing, never
//! edits an existing file and prints the wiring to add by hand.
//!
//! With `--api-auth` it also adds the JSON authentication API of `forge new
//! --api-auth` (bearer tokens for API clients, Forge-Go's flag of the same
//! name); in an application that already has every page file it writes
//! only the API's files.

use std::fs;
use std::path::Path;

use crate::CliError;
use crate::generate::{
    LOCALES, LOCALES_DIRECTORY, Locale, api_authentication_compose_statements,
    api_authentication_files, authentication_compose_statements, authentication_files, locale_file,
};
use crate::scaffold::{File, write_all};

/// What the generator did (or, with `--dry-run`, would do).
#[derive(Debug)]
pub(crate) struct Outcome {
    /// Files created, relative to the application root, in write order.
    pub(crate) created: Vec<String>,
    /// The wiring instructions to print.
    pub(crate) wiring: String,
}

/// Mechanisms of `forge new`'s PostgreSQL infrastructure the pages call.
const REQUIRED: &[(&str, &[&str])] = &[
    (
        "src/infrastructure/security/mod.rs",
        &[
            "pub async fn authenticate_password",
            "pub async fn verify_password",
            "pub async fn verify_principal_password",
            "pub async fn issue_persisted_session",
            "pub fn issue_session_secrets",
            "pub fn session_set_cookie",
            "pub fn session_remove_cookie",
            "pub async fn authenticate_request",
            "pub async fn enforce_csrf",
            "pub struct Argon2idPasswordHasher",
        ],
    ),
    (
        "src/infrastructure/auth/mod.rs",
        &[
            "pub struct PostgresPasswordCredentialStore",
            "async fn find_by_principal",
            "pub struct PostgresSessionStore",
            "pub fn with_idle_timeout",
        ],
    ),
    (
        "src/infrastructure/tenancy/mod.rs",
        &["pub struct PgMembershipStore"],
    ),
    (
        "src/infrastructure/audit/mod.rs",
        &["pub struct PgAuditSink"],
    ),
    (
        "migrations/00000000000000000002_forge_identity.up.sql",
        &["CREATE TABLE forge_principals"],
    ),
    (
        "migrations/00000000000000000006_forge_session_idle_timeout.up.sql",
        &["ADD COLUMN last_seen_at"],
    ),
];

/// What the JSON authentication API uses besides the pages' infrastructure.
const API_REQUIRED: &[(&str, &[&str])] = &[
    (
        "src/infrastructure/accounts/mod.rs",
        &["pub struct PgTenantDirectory", "pub struct PgMfaStore"],
    ),
    (
        "migrations/00000000000000000004_forge_mfa.up.sql",
        &["CREATE TABLE forge_totp_factors"],
    ),
];

/// Generates the authentication files in the application at `root`: the
/// pages, and with `api_auth` the JSON API (only the API when every page
/// file is already there).
pub(crate) fn generate(root: &Path, api_auth: bool, dry_run: bool) -> Result<Outcome, CliError> {
    inspect(root)?;
    let mut pages: Vec<File> = authentication_files()
        .into_iter()
        .map(|(path, content)| File {
            path: path.to_owned(),
            content,
        })
        .collect();
    pages.extend(locale_files(root).into_iter().map(|(path, content)| File {
        path,
        content: content.to_owned(),
    }));
    let with_pages = !(api_auth && pages.iter().all(|file| root.join(&file.path).is_file()));
    let mut files = if with_pages { pages } else { Vec::new() };
    if api_auth {
        if !with_pages {
            for (file, items) in API_REQUIRED {
                let source = read(root, file).unwrap_or_default();
                if let Some(item) = items.iter().find(|item| !source.contains(**item)) {
                    return refuse(format!(
                        "{file} has no `{item}`; the JSON authentication API uses the \
                         PostgreSQL identity infrastructure `forge new` generates"
                    ));
                }
            }
        }
        files.extend(
            api_authentication_files()
                .into_iter()
                .map(|(path, content)| File {
                    path: path.to_owned(),
                    content,
                }),
        );
    }
    refuse_collisions(root, &files)?;
    let wiring = wiring(with_pages, api_auth)?;
    if !dry_run {
        write_all(root, &files)?;
    }
    Ok(Outcome {
        created: files.into_iter().map(|file| file.path).collect(),
        wiring,
    })
}

/// The pages' translations: English, and every other shipped locale the
/// application has a `LOCALE.json` for.
fn locale_files(root: &Path) -> Vec<(String, &'static str)> {
    LOCALES
        .into_iter()
        .filter(|locale| {
            *locale == Locale::En
                || root
                    .join(LOCALES_DIRECTORY)
                    .join(format!("{}.json", locale.tag()))
                    .is_file()
        })
        .map(|locale| locale_file(Some("authentication"), locale))
        .collect()
}

fn refuse<T>(reason: impl Into<String>) -> Result<T, CliError> {
    Err(CliError::Authentication(reason.into()))
}

fn read(root: &Path, relative: &str) -> Option<String> {
    fs::read_to_string(root.join(relative)).ok()
}

/// Refuses applications the pages cannot serve, with the reason.
fn inspect(root: &Path) -> Result<(), CliError> {
    let Some(manifest) = read(root, "Cargo.toml") else {
        return refuse(format!(
            "no Cargo.toml in {}; run `forge generate authentication` in the root of an \
             application created by `forge new`",
            root.display()
        ));
    };
    let dependency = |name: &str| {
        manifest
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{name} =")))
            .map(str::to_owned)
    };
    if dependency("askama").is_none() || !root.join("src/adapters/http/views/mod.rs").is_file() {
        return refuse(
            "this application has no views (it was created with `forge new --api`). The \
             generator adds server-rendered sign-in pages; API clients authenticate with the \
             session mechanisms in src/infrastructure/security/mod.rs",
        );
    }
    let Some(sqlx) = dependency("sqlx") else {
        return refuse(
            "this application has no database (`forge new -d none`). Accounts, sessions and \
             tenant memberships are persisted in PostgreSQL; create the application with \
             `forge new <name>` (PostgreSQL is the default)",
        );
    };
    if !sqlx.contains("\"postgres\"") {
        return refuse(
            "Forge persists identities, sessions and tenant memberships only in PostgreSQL \
             today (ADR 0021); MySQL, MariaDB and SQLite applications have no session or \
             membership stores for the pages to use. Use a PostgreSQL application",
        );
    }
    if !manifest.contains("\"testing\"") {
        return refuse(
            "the generated PostgreSQL tests need forge::db::testing: add the `forge` \
             dependency to [dev-dependencies] again with `features = [..., \"testing\"]` \
             (as `forge new` generates it)",
        );
    }
    let bootstrap = read(root, "src/bootstrap/mod.rs").unwrap_or_default();
    if !bootstrap.contains("crate::adapters::http::site(") {
        return refuse(
            "src/bootstrap/mod.rs has no `compose` function that loads the site with \
             `crate::adapters::http::site(..)`; the printed wiring needs it (see the \
             `compose` function generated by `forge new`)",
        );
    }
    if !root.join("src/adapters/http/views/locales.rs").is_file() {
        return refuse(
            "src/adapters/http/views/locales.rs is missing: the pages are translated with \
             `layout.i18n` (ADR 0037). Add config/locales and the translations to the site as \
             \"Applications created before i18n\" in docs/i18n.md describes, then run the \
             generator again",
        );
    }
    for (file, items) in REQUIRED {
        let source = read(root, file).unwrap_or_default();
        for item in *items {
            if !source.contains(item) {
                let upgrade =
                    if *item == "pub fn with_idle_timeout" || *item == "ADD COLUMN last_seen_at" {
                        " (an application generated before the session idle timeout adds the \
                     migration and the session store as \"Session idle timeout\" in \
                     docs/getting-started.md describes)"
                    } else {
                        ""
                    };
                return refuse(format!(
                    "{file} has no `{item}`; the pages use the PostgreSQL identity \
                     infrastructure `forge new` generates{upgrade}"
                ));
            }
        }
    }
    Ok(())
}

/// Fails, before anything is written, when a file to create exists or the
/// authentication migration was added before.
fn refuse_collisions(root: &Path, files: &[File]) -> Result<(), CliError> {
    let mut existing: Vec<String> = files
        .iter()
        .filter(|file| root.join(&file.path).exists())
        .map(|file| file.path.clone())
        .collect();
    if let Ok(entries) = fs::read_dir(root.join("migrations")) {
        let mut migrations: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                let pages = files
                    .iter()
                    .any(|file| file.path.ends_with("_forge_authentication.up.sql"));
                let api = files
                    .iter()
                    .any(|file| file.path.ends_with("_forge_bearer_sessions.up.sql"));
                (pages
                    && (name.ends_with("_forge_authentication.up.sql")
                        || name.ends_with("_forge_mfa.up.sql")
                        || name.ends_with("_forge_login_throttle.up.sql")))
                    || (api && name.ends_with("_forge_bearer_sessions.up.sql"))
            })
            .map(|name| format!("migrations/{name}"))
            .filter(|path| !existing.contains(path))
            .collect();
        migrations.sort();
        existing.extend(migrations);
    }
    if existing.is_empty() {
        return Ok(());
    }
    refuse(format!(
        "refusing to overwrite existing files; nothing was written:\n  {}",
        listed(&existing)
    ))
}

/// Files named in the refusal before it says how many more there are: the
/// pages are about twenty files, which `forge new` already wrote in most
/// applications, and listing them all buries the one-line reason.
const MAX_LISTED: usize = 5;

/// The first [`MAX_LISTED`] of `existing`, one per line, and the count of
/// the rest.
fn listed(existing: &[String]) -> String {
    let mut lines: Vec<String> = existing.iter().take(MAX_LISTED).cloned().collect();
    if existing.len() > MAX_LISTED {
        lines.push(format!(
            "... and {} more (the sign-in pages are already in this application: \
             `forge new` generates them)",
            existing.len() - MAX_LISTED
        ));
    }
    lines.join("\n  ")
}

/// The module declarations and composition-root lines to add by hand: the
/// pages' when `pages`, the JSON authentication API's when `api`.
fn wiring(pages: bool, api: bool) -> Result<String, CliError> {
    let mut statements = String::new();
    if pages {
        let Some(pages) = authentication_compose_statements() else {
            return refuse("the composition template has no authentication wiring");
        };
        statements.push_str(&pages);
    }
    if api {
        let Some(api) = api_authentication_compose_statements() else {
            return refuse("the composition template has no JSON authentication API wiring");
        };
        statements.push_str(&api);
    }
    let what = if pages {
        "pages"
    } else {
        "JSON authentication API"
    };
    let mut out = format!(
        "\nForge did not edit any existing file. Wire the {what} by hand:\n\n  \
         1. Declare the new modules (each in the listed file, keeping `mod` lines sorted):\n\n"
    );
    let mut declarations = Vec::new();
    if pages {
        declarations.extend([
            ("src/infrastructure/mod.rs", "accounts"),
            ("src/adapters/http/mod.rs", "authentication"),
            ("src/adapters/http/views/mod.rs", "authentication"),
            ("src/bootstrap/mod.rs", "setup"),
        ]);
    }
    if api {
        declarations.extend([
            ("src/infrastructure/mod.rs", "bearer_sessions"),
            ("src/adapters/http/mod.rs", "api_authentication"),
        ]);
    }
    for (file, module) in declarations {
        out.push_str(&format!("       {file:<38} pub mod {module};\n"));
    }
    out.push_str(
        "\n  2. In src/bootstrap/mod.rs, at the end of `fn compose`, before `Ok(())`:\n\n",
    );
    out.push_str(&statements);
    if pages {
        out.push_str(
            "\n  3. In src/bootstrap/mod.rs, in `fn run`, before `.run()`:\n\n        \
             .command(\"setup\", setup::SUMMARY, setup::run)\n",
        );
        out.push_str(
            "\n  Tenant-scoped scaffolds grant the `owner` role that `setup` gives the first\n  \
             member; signed-out browsers asking for their pages are sent to /session/new.\n  \
             Then apply the migration and create the first owner:\n\n    \
             FORGE_MIGRATION_DATABASE_URL=... cargo run -- migrate\n    \
             FORGE_MIGRATION_DATABASE_URL=... cargo run -- setup --tenant acme --email owner@example.com\n\n  \
             Reset links are emailed once FORGE_MAIL_URL, FORGE_MAIL_FROM and\n  \
             FORGE_AUTH_PASSWORD_RESET_URL are set (the `mail` feature of `forge`, on with\n  \
             `views`); until then nothing is delivered and the page answers the same.\n  \
             They go only to the login the credential store returns: if\n  \
             src/infrastructure/auth/mod.rs predates this generator, select `login` in\n  \
             `find_by_login` and add `.with_login(login)` to the credential, as `forge new`\n  \
             does; without it no link is delivered.\n\n  \
             Two-factor authentication (/mfa, /session/mfa) needs FORGE_AUTH_MFA_KEY,\n  \
             canonical unpadded base64url of 32 random bytes:\n\n    \
             export FORGE_AUTH_MFA_KEY=$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')\n",
        );
    }
    if api {
        out.push_str(
            "\n  The JSON authentication API: apply its migration (forge_bearer_sessions),\n  \
             then sign in for a bearer token and use it:\n\n    \
             forge migrate\n    \
             curl -s -X POST http://127.0.0.1:3000/auth/login -H 'Content-Type: application/json' \\\n      \
             -d '{\"email\":\"owner@example.com\",\"password\":\"...\",\"tenant\":\"acme\"}'\n    \
             curl -s http://127.0.0.1:3000/v1/auth/session -H \"Authorization: Bearer $TOKEN\"\n    \
             curl -s -X POST http://127.0.0.1:3000/auth/logout -H \"Authorization: Bearer $TOKEN\"\n",
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::{Database, ForgeSource, NewApplication, create_application};

    fn application(database: Database, views: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        application_with(database, views, false)
    }

    fn application_with(
        database: Database,
        views: bool,
        api_auth: bool,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let parent = tempfile::tempdir().expect("temporary directory");
        let root = create_application(
            parent.path(),
            &NewApplication {
                name: "shop".to_owned(),
                docker: false,
                ci: false,
                database,
                views,
                forge: ForgeSource::GitTag,
                locale: crate::generate::Locale::En,
                api_auth,
            },
        )
        .expect("application");
        (parent, root)
    }

    /// Removes the authentication files `forge new` wrote, keeping their
    /// contents, as in an application created before the generator existed.
    fn strip(root: &Path) -> Vec<(String, String)> {
        authentication_files()
            .into_iter()
            .map(|(path, _)| path.to_owned())
            .chain(locale_files(root).into_iter().map(|(path, _)| path))
            .map(|path| {
                let content = fs::read_to_string(root.join(&path)).expect("generated by forge new");
                fs::remove_file(root.join(&path)).expect("remove");
                (path, content)
            })
            .collect()
    }

    #[test]
    fn the_files_are_those_of_forge_new_and_the_wiring_is_printed() {
        let (_parent, root) = application(Database::Postgresql, true);
        let original = strip(&root);

        let dry = generate(&root, false, true).expect("dry run");
        assert!(original.iter().all(|(path, _)| !root.join(path).exists()));

        let outcome = generate(&root, false, false).expect("generation");
        assert_eq!(outcome.created, dry.created);
        assert_eq!(outcome.wiring, dry.wiring);
        for (path, content) in &original {
            assert_eq!(
                &fs::read_to_string(root.join(path)).expect("written"),
                content,
                "{path}"
            );
        }
        let bootstrap = fs::read_to_string(root.join("src/bootstrap/mod.rs")).expect("bootstrap");
        let statements = authentication_compose_statements().expect("statements");
        assert!(
            bootstrap.contains(&statements),
            "the printed statements are forge new's"
        );
        assert!(outcome.wiring.contains(&statements));
        for declaration in [
            "src/infrastructure/mod.rs              pub mod accounts;",
            "src/bootstrap/mod.rs                   pub mod setup;",
        ] {
            assert!(outcome.wiring.contains(declaration), "{}", outcome.wiring);
        }
        assert!(
            outcome
                .wiring
                .contains(".command(\"setup\", setup::SUMMARY, setup::run)")
        );
    }

    #[test]
    fn existing_files_are_never_overwritten() {
        let (_parent, root) = application(Database::Postgresql, true);
        let error = generate(&root, false, false).expect_err("forge new already wrote them");
        let message = error.to_string();
        assert!(message.contains("refusing to overwrite"), "{message}");
        assert!(message.contains("... and "), "{message}");
        assert!(message.contains("\n  migrations/"), "{message}");

        // A renamed authentication migration also counts.
        strip(&root);
        fs::write(
            root.join("migrations/20260101000000_forge_authentication.up.sql"),
            "-- applied before\n",
        )
        .expect("write");
        let error = generate(&root, false, false).expect_err("migration exists");
        assert!(
            error
                .to_string()
                .contains("20260101000000_forge_authentication")
        );
        assert!(!root.join("src/adapters/http/authentication.rs").exists());
    }

    /// An application whose session store predates the idle timeout is
    /// refused before anything is written, with the way to upgrade: the
    /// printed wiring passes the timeout to the store.
    #[test]
    fn a_session_store_without_the_idle_timeout_is_refused_with_the_upgrade() {
        let (_parent, root) = application(Database::Postgresql, true);
        strip(&root);
        let store = root.join("src/infrastructure/auth/mod.rs");
        let current = fs::read_to_string(&store).expect("generated");
        fs::write(
            &store,
            current.replace("pub fn with_idle_timeout", "pub fn without_it"),
        )
        .expect("write");
        let message = generate(&root, false, true)
            .expect_err("refused")
            .to_string();
        assert!(message.contains("`pub fn with_idle_timeout`"), "{message}");
        assert!(message.contains("\"Session idle timeout\""), "{message}");

        fs::write(&store, current).expect("write");
        fs::remove_file(
            root.join("migrations/00000000000000000006_forge_session_idle_timeout.up.sql"),
        )
        .expect("remove");
        let message = generate(&root, false, true)
            .expect_err("refused")
            .to_string();
        assert!(message.contains("ADD COLUMN last_seen_at"), "{message}");
        assert!(!root.join("src/adapters/http/authentication.rs").exists());
    }

    #[test]
    fn the_refusal_names_a_few_files_and_counts_the_rest() {
        let few: Vec<String> = (0..MAX_LISTED).map(|n| format!("f{n}")).collect();
        assert_eq!(listed(&few), "f0\n  f1\n  f2\n  f3\n  f4");
        let many: Vec<String> = (0..20).map(|n| format!("f{n}")).collect();
        let text = listed(&many);
        assert!(text.contains("f4"), "{text}");
        assert!(!text.contains("f5"), "{text}");
        assert!(text.contains("... and 15 more"), "{text}");
        let (_parent, root) = application(Database::Postgresql, true);
        let message = generate(&root, false, false)
            .expect_err("forge new already wrote them")
            .to_string();
        assert!(message.lines().count() <= 10, "{message}");
    }

    #[test]
    fn other_backends_api_and_database_free_applications_are_refused() {
        for (database, views, reason) in [
            (Database::Mysql, true, "only in PostgreSQL"),
            (Database::Mariadb, true, "only in PostgreSQL"),
            (Database::Sqlite3, true, "only in PostgreSQL"),
            (Database::Disabled, true, "no database"),
            (Database::Postgresql, false, "--api"),
        ] {
            let (_parent, root) = application(database, views);
            let message = generate(&root, false, true)
                .expect_err("refused")
                .to_string();
            assert!(message.contains(reason), "{database:?} {views}: {message}");
        }
        let empty = tempfile::tempdir().expect("temporary directory");
        assert!(generate(empty.path(), false, true).is_err());
    }

    #[test]
    fn missing_identity_infrastructure_is_refused() {
        let (_parent, root) = application(Database::Postgresql, true);
        strip(&root);
        fs::write(root.join("src/infrastructure/audit/mod.rs"), "//! audit\n").expect("write");
        let message = generate(&root, false, true)
            .expect_err("refused")
            .to_string();
        assert!(message.contains("PgAuditSink"), "{message}");
    }

    /// `forge new --api-auth` writes the JSON authentication API and wires
    /// it in `compose`; without the flag nothing of it exists.
    #[test]
    fn forge_new_api_auth_writes_and_wires_the_json_api() {
        let (_parent, root) = application_with(Database::Postgresql, true, true);
        for (path, content) in api_authentication_files() {
            assert_eq!(
                fs::read_to_string(root.join(path)).expect("written"),
                content,
                "{path}"
            );
        }
        let bootstrap = fs::read_to_string(root.join("src/bootstrap/mod.rs")).expect("bootstrap");
        let statements = api_authentication_compose_statements().expect("statements");
        assert!(bootstrap.contains(&statements), "{bootstrap}");
        assert!(statements.contains("api_authentication::routes("));
        assert!(statements.trim_end().ends_with(")?;"), "{statements}");
        for (file, declaration) in [
            ("src/infrastructure/mod.rs", "pub mod bearer_sessions;"),
            ("src/adapters/http/mod.rs", "pub mod api_authentication;"),
        ] {
            assert!(
                fs::read_to_string(root.join(file))
                    .expect("module")
                    .contains(declaration),
                "{file}"
            );
        }

        let (_parent, plain) = application(Database::Postgresql, true);
        for (path, _) in api_authentication_files() {
            assert!(!plain.join(path).exists(), "{path}");
        }
        let bootstrap = fs::read_to_string(plain.join("src/bootstrap/mod.rs")).expect("bootstrap");
        assert!(!bootstrap.contains("api_authentication"));
    }

    /// With the pages in place, `--api-auth` writes only the API's files,
    /// byte-identical to `forge new --api-auth`, and prints only its wiring;
    /// a second run refuses.
    #[test]
    fn api_auth_adds_only_the_json_api_to_an_application_with_the_pages() {
        let (_parent, root) = application(Database::Postgresql, true);
        let outcome = generate(&root, true, false).expect("generation");
        let expected: Vec<String> = api_authentication_files()
            .into_iter()
            .map(|(path, _)| path.to_owned())
            .collect();
        assert_eq!(outcome.created, expected);
        assert!(
            outcome
                .wiring
                .contains("Wire the JSON authentication API by hand")
        );
        assert!(
            outcome
                .wiring
                .contains("src/infrastructure/mod.rs              pub mod bearer_sessions;")
        );
        assert!(!outcome.wiring.contains("pub mod setup;"));
        assert!(
            outcome
                .wiring
                .contains(&api_authentication_compose_statements().expect("statements"))
        );
        assert!(outcome.wiring.contains("Authorization: Bearer $TOKEN"));

        let again = generate(&root, true, true)
            .expect_err("the API exists")
            .to_string();
        assert!(again.contains("refusing to overwrite"), "{again}");
        fs::remove_file(root.join("src/adapters/http/api_authentication.rs")).expect("remove");
        let migration = generate(&root, true, true)
            .expect_err("migration exists")
            .to_string();
        assert!(migration.contains("forge_bearer_sessions"), "{migration}");
    }

    /// Without the pages, `--api-auth` writes both and prints both wirings.
    #[test]
    fn api_auth_without_the_pages_writes_both() {
        let (_parent, root) = application(Database::Postgresql, true);
        strip(&root);
        let outcome = generate(&root, true, true).expect("dry run");
        assert!(
            outcome
                .created
                .contains(&"src/adapters/http/authentication.rs".to_owned())
        );
        assert!(
            outcome
                .created
                .contains(&"src/adapters/http/api_authentication.rs".to_owned())
        );
        assert!(outcome.wiring.contains("Wire the pages by hand"));
        assert!(outcome.wiring.contains("pub mod api_authentication;"));
        assert!(outcome.wiring.contains(".command(\"setup\""));
        let pages = authentication_compose_statements().expect("pages");
        let api = api_authentication_compose_statements().expect("api");
        assert!(outcome.wiring.contains(&format!("{pages}{api}")));
    }
}
