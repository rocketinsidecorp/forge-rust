//! `forge generate scaffold`: a server-rendered HTML CRUD resource through
//! every hexagonal ring, for the application's database backend
//! (ADR 0022, decision 7).
//!
//! The generator writes only new files, all or nothing, and never edits an
//! existing file: the module declarations and the composition-root lines
//! that wire the resource are printed for the developer to add, like the
//! generated code they are reviewable.

mod render;
mod spec;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) use render::Backend;
pub(crate) use render::File;
use render::{Broadcast, STREAMS_PATH};
pub(crate) use spec::Scope;

use crate::CliError;
use crate::generate::write_new_file;
use render::ACCESS_PATH;
use spec::Resource;

/// Arguments of `forge generate scaffold`.
#[derive(Debug, Clone)]
pub(crate) struct ScaffoldOptions {
    pub(crate) name: String,
    pub(crate) fields: Vec<String>,
    pub(crate) scope: Scope,
    pub(crate) plural: Option<String>,
    pub(crate) broadcast: bool,
    pub(crate) dry_run: bool,
}

/// What the generator did (or, with `--dry-run`, would do).
#[derive(Debug)]
pub(crate) struct Outcome {
    /// Files created, relative to the application root, in write order.
    pub(crate) created: Vec<String>,
    /// Whether the shared access module already existed and was reused.
    pub(crate) reused_access: bool,
    /// Whether the shared streams module already existed and was reused.
    pub(crate) reused_streams: bool,
    /// The wiring instructions to print.
    pub(crate) wiring: String,
}

/// What the generator learned about the application in `root`.
#[derive(Debug)]
struct Application {
    backend: Backend,
    access_exists: bool,
    streams_exists: bool,
}

/// Generates the scaffold in the application at `root`. `migration` is the
/// numeric version prefix of the migration pair.
pub(crate) fn generate(
    root: &Path,
    options: &ScaffoldOptions,
    migration: &str,
) -> Result<Outcome, CliError> {
    let resource = spec::parse(
        &options.name,
        &options.fields,
        options.scope,
        options.plural.as_deref(),
    )
    .map_err(|invalid| CliError::Scaffold(invalid.to_string()))?;
    let application = inspect(root, &resource, options.broadcast)?;
    let broadcast = Broadcast {
        enabled: options.broadcast,
        streams_module: options.broadcast && !application.streams_exists,
    };
    let mut files = render::files(
        &resource,
        application.backend,
        migration,
        !application.access_exists,
        broadcast,
    );
    refuse_collisions(root, &resource, &files)?;
    for file in files.iter_mut().filter(|file| file.path.ends_with(".rs")) {
        file.content = rustfmt(root, &file.path, &file.content)?;
    }
    let wiring = wiring(
        root,
        &resource,
        application.backend,
        !application.access_exists,
        broadcast,
    )?;
    if !options.dry_run {
        write_all(root, &files)?;
    }
    Ok(Outcome {
        created: files.iter().map(|file| file.path.clone()).collect(),
        reused_access: application.access_exists,
        reused_streams: options.broadcast && application.streams_exists,
        wiring,
    })
}

/// Formats generated Rust with the application's pinned `rustfmt` (edition
/// 2024, the application's `rustfmt.toml` if any), exactly as
/// `cargo fmt` will. A formatter failure aborts before anything is written.
pub(crate) fn rustfmt(root: &Path, path: &str, source: &str) -> Result<String, CliError> {
    let failed = |detail: String| {
        CliError::Scaffold(format!(
            "rustfmt could not format the generated {path}: {detail} (rustfmt is a component \
             of the toolchain pinned in rust-toolchain.toml: `rustup component add rustfmt`)"
        ))
    };
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failed(error.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(source.as_bytes())
            .map_err(|error| failed(error.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| failed(error.to_string()))
}

fn unsupported<T>(reason: impl Into<String>) -> Result<T, CliError> {
    Err(CliError::Scaffold(reason.into()))
}

fn read(root: &Path, relative: &str) -> Option<String> {
    fs::read_to_string(root.join(relative)).ok()
}

/// Detects the backend and shape from the files `forge new` generated and
/// refuses applications the scaffold cannot serve.
fn inspect(root: &Path, resource: &Resource, broadcast: bool) -> Result<Application, CliError> {
    let Some(manifest) = read(root, "Cargo.toml") else {
        return unsupported(format!(
            "no Cargo.toml in {}; run `forge generate scaffold` in the root of an application \
             created by `forge new`",
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
        return unsupported(
            "this application has no views (it was created with `forge new --api`). The \
             scaffold generates server-rendered HTML pages; JSON scaffolds are not available \
             yet, so add API resources by hand following docs/architecture.md",
        );
    }
    let Some(sqlx) = dependency("sqlx") else {
        return unsupported(
            "this application has no database (`forge new -d none`); the scaffold needs \
             PostgreSQL, MySQL, MariaDB or SQLite. Create a database application with \
             `forge new <name> -d <database>`",
        );
    };
    let backend = if sqlx.contains("\"postgres\"") {
        Backend::Postgresql
    } else if sqlx.contains("\"mysql\"") {
        Backend::MySql
    } else if sqlx.contains("\"sqlite\"") {
        Backend::Sqlite
    } else {
        return unsupported(
            "could not tell the database backend from the sqlx features in Cargo.toml \
             (expected \"postgres\", \"mysql\" or \"sqlite\")",
        );
    };
    if !manifest.contains("\"testing\"") {
        return unsupported(
            "the generated repository tests need forge::db::testing: add the `forge` \
             dependency to [dev-dependencies] again with `features = [..., \"testing\"]` \
             (as `forge new` generates it)",
        );
    }
    let compose = read(root, "src/bootstrap/mod.rs").unwrap_or_default();
    if !compose.contains("crate::adapters::http::site(") {
        return unsupported(
            "src/bootstrap/mod.rs has no `compose` function that loads the site with \
             `crate::adapters::http::site(..)`; the printed wiring needs it (see the \
             `compose` function generated by `forge new`)",
        );
    }
    for module in [
        "src/domain/entities/mod.rs",
        "src/application/ports/mod.rs",
        "src/application/use_cases/mod.rs",
        "src/infrastructure/database/mod.rs",
        "src/infrastructure/security/mod.rs",
        "src/adapters/http/mod.rs",
    ] {
        if !root.join(module).is_file() {
            return unsupported(format!(
                "{module} is missing; the scaffold adds modules next to the ones `forge new` \
                 generates"
            ));
        }
    }
    if backend == Backend::Postgresql && resource.scope == Scope::Tenant {
        let database = read(root, "src/infrastructure/database/mod.rs").unwrap_or_default();
        if !database.contains("pub async fn begin_tenant_transaction") {
            return unsupported(
                "src/infrastructure/database/mod.rs has no begin_tenant_transaction; \
                 tenant-scoped PostgreSQL repositories run in it for row-level security",
            );
        }
    }
    let access_exists = match read(root, ACCESS_PATH) {
        None => false,
        Some(access) => {
            for item in [
                "pub trait GlobalAccess",
                "pub trait TenantAccess",
                "pub struct DenyAll",
                "pub struct SessionTenantAccess",
            ] {
                if !access.contains(item) {
                    return unsupported(format!(
                        "{ACCESS_PATH} exists but has no `{item}`; generated routes depend on \
                         it. Restore it or move your file aside"
                    ));
                }
            }
            true
        }
    };
    let streams_exists = match read(root, STREAMS_PATH) {
        Some(streams) if broadcast => {
            for item in [
                "pub struct Streams",
                "pub fn allow_tenant",
                "pub fn allow_global",
            ] {
                if !streams.contains(item) {
                    return unsupported(format!(
                        "{STREAMS_PATH} exists but has no `{item}`; generated routes depend on \
                         it. Restore it or move your file aside"
                    ));
                }
            }
            true
        }
        Some(_) => true,
        None => false,
    };
    if broadcast {
        let forge = dependency("forge").unwrap_or_default();
        if !forge.contains("\"broadcast\"") {
            return unsupported(
                "--broadcast needs the `broadcast` feature of the forge dependency, which \
                 `forge new` now enables: add \"broadcast\" to the features of `forge` in \
                 Cargo.toml (see \"Live updates\" in docs/views.md)",
            );
        }
    }
    Ok(Application {
        backend,
        access_exists,
        streams_exists,
    })
}

/// Fails, before anything is written, when a file to create exists or the
/// resource was scaffolded before.
fn refuse_collisions(root: &Path, resource: &Resource, files: &[File]) -> Result<(), CliError> {
    let mut existing: Vec<String> = files
        .iter()
        .filter(|file| root.join(&file.path).exists())
        .map(|file| file.path.clone())
        .collect();
    let suffix = format!("_create_{}.up.sql", resource.plural);
    if let Ok(entries) = fs::read_dir(root.join("migrations")) {
        let mut migrations: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(&suffix))
            .map(|name| format!("migrations/{name}"))
            .collect();
        migrations.sort();
        existing.extend(migrations);
    }
    if existing.is_empty() {
        return Ok(());
    }
    unsupported(format!(
        "refusing to overwrite existing files; nothing was written:\n  {}",
        existing.join("\n  ")
    ))
}

/// Writes every file with create-new semantics. On failure, removes the
/// files and directories this call created, so nothing is half generated.
pub(crate) fn write_all(root: &Path, files: &[File]) -> Result<(), CliError> {
    let mut written: Vec<PathBuf> = Vec::new();
    let mut directories: Vec<PathBuf> = Vec::new();
    let result = files.iter().try_for_each(|file| {
        let path = root.join(&file.path);
        if let Some(parent) = path.parent() {
            let mut missing = Vec::new();
            let mut cursor = parent;
            while !cursor.exists() {
                missing.push(cursor.to_path_buf());
                match cursor.parent() {
                    Some(next) => cursor = next,
                    None => break,
                }
            }
            fs::create_dir_all(parent).map_err(|source| CliError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
            directories.extend(missing.into_iter().rev());
        }
        write_new_file(&path, &file.content)?;
        written.push(path);
        Ok(())
    });
    if result.is_err() {
        for path in written.iter().rev() {
            let _ = fs::remove_file(path);
        }
        for directory in directories.iter().rev() {
            let _ = fs::remove_dir(directory);
        }
    }
    result
}

/// The module declarations and composition-root lines that wire the
/// resource, printed after generation.
fn wiring(
    root: &Path,
    resource: &Resource,
    backend: Backend,
    new_access: bool,
    broadcast: Broadcast,
) -> Result<String, CliError> {
    let singular = &resource.singular;
    let plural = &resource.plural;
    let type_name = spec::pascal_case(singular);
    let mut declarations = vec![
        ("src/domain/entities/mod.rs", singular.clone()),
        (
            "src/application/ports/mod.rs",
            format!("{singular}_repository"),
        ),
        ("src/application/use_cases/mod.rs", plural.clone()),
        ("src/infrastructure/database/mod.rs", plural.clone()),
    ];
    if new_access {
        declarations.push(("src/adapters/http/mod.rs", "access".to_owned()));
    }
    if broadcast.streams_module {
        declarations.push(("src/adapters/http/mod.rs", "streams".to_owned()));
    }
    declarations.push(("src/adapters/http/mod.rs", plural.clone()));
    declarations.push(("src/adapters/http/views/mod.rs", plural.clone()));

    let (database, prefix) = match backend {
        Backend::Postgresql => ("postgres", "Pg"),
        Backend::MySql => ("mysql", "MySql"),
        Backend::Sqlite => ("sqlite", "Sqlite"),
    };
    let tenant = resource.scope == Scope::Tenant;
    let mut code = String::new();
    let access = if !tenant {
        code.push_str(&format!(
            "    // {plural}: global. DenyAll refuses every request until you decide who may\n    \
             // use them: implement GlobalAccess (src/adapters/http/access.rs).\n"
        ));
        "std::sync::Arc::new(crate::adapters::http::access::DenyAll)"
    } else if backend == Backend::Postgresql {
        code.push_str(&format!(
            "    // {plural}: tenant-scoped, under /tenants/{{tenant_id}}/{plural}. RBAC denies\n    \
             // every role `policy` does not name: the `owner` role (`setup` gives it to\n    \
             // the first member) may read and write; grant other tenant roles here.\n    \
             let mut policy = forge::security::RbacPolicy::new();\n    \
             let owner = forge::security::Role::new(\"owner\").map_err(forge::ComposeError::other)?;\n    \
             for permission in [\n        \
             crate::application::use_cases::{plural}::READ_PERMISSION,\n        \
             crate::application::use_cases::{plural}::WRITE_PERMISSION,\n    \
             ] {{\n        \
             let permission =\n            \
             forge::security::Permission::new(permission).map_err(forge::ComposeError::other)?;\n        \
             policy.allow(owner.clone(), permission);\n    \
             }}\n    \
             let pool = composition.postgres()?.pool().clone();\n    \
             let access = crate::adapters::http::access::SessionTenantAccess::new(\n        \
             std::sync::Arc::new(crate::infrastructure::auth::PostgresSessionStore::new(\n            \
             pool.clone(),\n        \
             )),\n        \
             std::sync::Arc::new(crate::infrastructure::tenancy::PgMembershipStore::new(pool)),\n        \
             policy,\n    \
             );\n"
        ));
        "std::sync::Arc::new(access)"
    } else {
        code.push_str(&format!(
            "    // {plural}: tenant-scoped, under /tenants/{{tenant_id}}/{plural}. {} has no\n    \
             // Forge session or membership persistence yet (ADR 0021), so DenyAll\n    \
             // refuses every request until you provide SessionStore and\n    \
             // MembershipStore implementations for SessionTenantAccess.\n",
            backend.label()
        ));
        "std::sync::Arc::new(crate::adapters::http::access::DenyAll)"
    };
    if broadcast.streams_module {
        let fanout = if backend == Backend::Postgresql {
            "    // Live updates (--broadcast), once for every broadcasting resource. On\n    \
             // PostgreSQL every replica hears every change (LISTEN/NOTIFY); production\n    \
             // needs FORGE_STREAMS_KEY (`openssl rand -base64 32`, the same everywhere).\n    \
             let streams =\n        \
             crate::adapters::http::streams::Streams::register(composition, forge::Fanout::Postgres)?;\n"
                .to_owned()
        } else {
            format!(
                "    // Live updates (--broadcast), once for every broadcasting resource. {}\n    \
                 // has no LISTEN/NOTIFY: updates reach the pages served by this process only.\n    \
                 // Production needs FORGE_STREAMS_KEY (`openssl rand -base64 32`).\n    \
                 let streams =\n        \
                 crate::adapters::http::streams::Streams::register(composition, forge::Fanout::InProcess)?;\n",
                backend.label()
            )
        };
        code.push_str(&fanout);
    }
    let streams_argument = if broadcast.enabled {
        "        &streams,\n"
    } else {
        ""
    };
    code.push_str(&format!(
        "    let {plural} = crate::application::use_cases::{plural}::{type_name}Service::new(\n        \
         std::sync::Arc::new(\n            \
         crate::infrastructure::database::{plural}::{prefix}{type_name}Repository::new(\n                \
         composition.{database}()?.clone(),\n            \
         ),\n        \
         ),\n    \
         );\n    \
         crate::adapters::http::{plural}::routes(\n        \
         composition.router(),\n        \
         &site,\n        \
         {plural},\n        \
         {access},\n\
         {streams_argument}    \
         )?;\n"
    ));

    let mut out = String::from(
        "\nForge did not edit any existing file. Wire the resource by hand:\n\n  \
         1. Declare the new modules (each in the listed file, keeping `mod` lines sorted):\n\n",
    );
    for (file, module) in &declarations {
        out.push_str(&format!("       {file:<38} pub mod {module};\n"));
    }
    out.push_str(
        "\n  2. In src/bootstrap/mod.rs, at the end of `fn compose`, before `Ok(())`:\n\n",
    );
    out.push_str(&format_statements(root, &code)?);
    out.push_str(&format!(
        "\n  Every route asks the access policy for `{plural}:read` or `{plural}:write`\n  \
         first; src/adapters/http/access.rs explains the deny-by-default choice.\n"
    ));
    if broadcast.enabled {
        out.push_str(&format!(
            "  Live updates: create, update and delete publish to the `{plural}` channel{}, and\n  \
             the index page subscribes; a stream is served only to callers the same policy\n  \
             grants `{plural}:read` (src/adapters/http/streams.rs).\n",
            if resource.scope == Scope::Tenant {
                " of the tenant"
            } else {
                ""
            }
        ));
    }
    if backend == Backend::Postgresql && tenant {
        out.push_str(&format!(
            "  Permissions match exactly: index and show need `{plural}:read`, new, create,\n  \
             edit, update and delete need `{plural}:write`. Signed-out browsers are sent\n  \
             to /session/new (the sign-in pages of `forge new` or\n  \
             `forge generate authentication`). To allow a read-only tenant role, add after\n  \
             `let mut policy`, for example:\n\n"
        ));
        let example = format!(
            "    let viewer = forge::security::Role::new(\"viewer\").map_err(forge::ComposeError::other)?;\n    \
             let read = forge::security::Permission::new(\n        \
             crate::application::use_cases::{plural}::READ_PERMISSION,\n    \
             )\n    \
             .map_err(forge::ComposeError::other)?;\n    \
             policy.allow(viewer, read);\n"
        );
        out.push_str(&format_statements(root, &example)?);
    }
    out.push_str(
        "\n  Then apply the migration (`cargo run -- migrate` with FORGE_MIGRATION_DATABASE_URL)\n  \
         and run `cargo test` (repository tests on a database server need\n  \
         `cargo test -- --include-ignored` and FORGE_TEST_* URLs, see docs/database.md).\n",
    );
    Ok(out)
}

/// Formats statements as they will read inside `fn compose`, so pasting them
/// keeps `cargo fmt --check` clean.
fn format_statements(root: &Path, statements: &str) -> Result<String, CliError> {
    const HEAD: &str = "fn compose() {\n";
    let formatted = rustfmt(root, "wiring", &format!("{HEAD}{statements}}}\n"))?;
    Ok(formatted
        .strip_prefix(HEAD)
        .and_then(|body| body.strip_suffix("}\n"))
        .unwrap_or(&formatted)
        .to_owned())
}

#[cfg(test)]
mod tests;
