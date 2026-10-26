//! `forge generate authentication`: adds the sign-in, sign-out and
//! password-reset pages and the `setup` command to an existing full-stack
//! PostgreSQL application (ADR 0026), like `rails generate authentication`.
//!
//! The files are byte-identical to the ones `forge new` writes, including
//! the pages' translations (`config/locales/authentication.en.json`, plus
//! the Spanish and Brazilian Portuguese files when the application has those
//! locales). The generator writes only new files, all or nothing, never
//! edits an existing file and prints the wiring to add by hand.

use std::fs;
use std::path::Path;

use crate::CliError;
use crate::generate::{
    LOCALES, LOCALES_DIRECTORY, Locale, authentication_compose_statements, authentication_files,
    locale_file,
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
];

/// Generates the authentication files in the application at `root`.
pub(crate) fn generate(root: &Path, dry_run: bool) -> Result<Outcome, CliError> {
    inspect(root)?;
    let mut files: Vec<File> = authentication_files()
        .into_iter()
        .map(|(path, content)| File {
            path: path.to_owned(),
            content,
        })
        .collect();
    files.extend(locale_files(root).into_iter().map(|(path, content)| File {
        path,
        content: content.to_owned(),
    }));
    refuse_collisions(root, &files)?;
    let wiring = wiring()?;
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
                return refuse(format!(
                    "{file} has no `{item}`; the pages use the PostgreSQL identity \
                     infrastructure `forge new` generates"
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
                name.ends_with("_forge_authentication.up.sql")
                    || name.ends_with("_forge_mfa.up.sql")
                    || name.ends_with("_forge_login_throttle.up.sql")
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

/// The module declarations and composition-root lines to add by hand.
fn wiring() -> Result<String, CliError> {
    let Some(statements) = authentication_compose_statements() else {
        return refuse("the composition template has no authentication wiring");
    };
    let mut out = String::from(
        "\nForge did not edit any existing file. Wire the pages by hand:\n\n  \
         1. Declare the new modules (each in the listed file, keeping `mod` lines sorted):\n\n",
    );
    for (file, module) in [
        ("src/infrastructure/mod.rs", "accounts"),
        ("src/adapters/http/mod.rs", "authentication"),
        ("src/adapters/http/views/mod.rs", "authentication"),
        ("src/bootstrap/mod.rs", "setup"),
    ] {
        out.push_str(&format!("       {file:<38} pub mod {module};\n"));
    }
    out.push_str(
        "\n  2. In src/bootstrap/mod.rs, at the end of `fn compose`, before `Ok(())`:\n\n",
    );
    out.push_str(&statements);
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::{Database, ForgeSource, NewApplication, create_application};

    fn application(database: Database, views: bool) -> (tempfile::TempDir, std::path::PathBuf) {
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

        let dry = generate(&root, true).expect("dry run");
        assert!(original.iter().all(|(path, _)| !root.join(path).exists()));

        let outcome = generate(&root, false).expect("generation");
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
        let error = generate(&root, false).expect_err("forge new already wrote them");
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
        let error = generate(&root, false).expect_err("migration exists");
        assert!(
            error
                .to_string()
                .contains("20260101000000_forge_authentication")
        );
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
        let message = generate(&root, false)
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
            let message = generate(&root, true).expect_err("refused").to_string();
            assert!(message.contains(reason), "{database:?} {views}: {message}");
        }
        let empty = tempfile::tempdir().expect("temporary directory");
        assert!(generate(empty.path(), true).is_err());
    }

    #[test]
    fn missing_identity_infrastructure_is_refused() {
        let (_parent, root) = application(Database::Postgresql, true);
        strip(&root);
        fs::write(root.join("src/infrastructure/audit/mod.rs"), "//! audit\n").expect("write");
        let message = generate(&root, true).expect_err("refused").to_string();
        assert!(message.contains("PgAuditSink"), "{message}");
    }
}
