use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::CliError;

/// Toolchain pinned by generated applications and their builder image.
/// Kept equal to the framework's `rust-toolchain.toml` by a unit test.
pub(crate) const RUST_VERSION: &str = "1.98.1";

const FORGE_REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const FORGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where a generated application obtains the `forge` crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForgeSource {
    /// The release tag matching this CLI's version.
    GitTag,
    /// A local checkout, for framework development (like `rails new --dev`).
    Path(String),
}

/// Database backend selected with `forge new -d` (ADR 0021).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Database {
    /// PostgreSQL 18 with row-level security and least-privilege roles (default).
    #[value(name = "postgresql", alias = "postgres")]
    Postgresql,
    /// MySQL 8.4.
    Mysql,
    /// MariaDB 11.8 LTS.
    Mariadb,
    /// SQLite stored in a file.
    #[value(name = "sqlite3", alias = "sqlite")]
    Sqlite3,
    /// No database: no SQLx, migrations or database Compose services.
    #[value(name = "none")]
    Disabled,
}

impl Database {
    fn is_enabled(self) -> bool {
        self != Self::Disabled
    }

    /// MySQL and MariaDB share the MySQL-protocol driver and migrations.
    fn is_mysql_family(self) -> bool {
        matches!(self, Self::Mysql | Self::Mariadb)
    }

    /// SQLx driver feature of the application's `sqlx` dependency.
    fn driver(self) -> &'static str {
        match self {
            Self::Postgresql | Self::Disabled => "postgres",
            Self::Mysql | Self::Mariadb => "mysql",
            Self::Sqlite3 => "sqlite",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Postgresql => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Mariadb => "MariaDB",
            Self::Sqlite3 => "SQLite",
            Self::Disabled => "no database",
        }
    }

    /// Cargo options of the `forge` dependency that select this backend and,
    /// for full-stack applications, the `views` feature (plus `testing` for
    /// the dev-dependency that provides `TestDatabase`).
    fn forge_features(self, views: bool, testing: bool) -> String {
        let (default_features, mut features) = match self {
            Self::Postgresql => (true, Vec::new()),
            Self::Mysql | Self::Mariadb => (false, vec!["mysql"]),
            Self::Sqlite3 => (true, vec!["sqlite"]),
            Self::Disabled => (false, Vec::new()),
        };
        if views {
            features.push("views");
        }
        if testing {
            features.push("testing");
        }
        let mut options = String::new();
        if !default_features {
            options.push_str(", default-features = false");
        }
        if !features.is_empty() {
            let quoted: Vec<String> = features
                .iter()
                .map(|feature| format!("\"{feature}\""))
                .collect();
            options.push_str(&format!(", features = [{}]", quoted.join(", ")));
        }
        options
    }

    /// Lists what `forge new` does not generate for this backend, if anything.
    ///
    /// ADR 0021 section 7: until a backend has full parity the generator
    /// reports the missing modules instead of emitting PostgreSQL SQL.
    pub(crate) fn unavailable_notice(self) -> Option<String> {
        if matches!(self, Self::Postgresql | Self::Disabled) {
            return None;
        }
        let backend = self.label();
        Some(format!(
            "note: the following PostgreSQL-only modules are not available for {backend} yet \
             (ADR 0021) and were not generated:\n\
             \x20 - row-level security policies and runtime/migrator role setup\n\
             \x20 - audit event persistence (PgAuditSink)\n\
             \x20 - identity, session and password-credential persistence (PostgresSessionStore, \
             PostgresPasswordCredentialStore)\n\
             \x20 - tenant membership persistence and tenant transaction settings (PgMembershipStore, \
             begin_tenant_transaction)\n\
             Tenant isolation on {backend} rests on the application layer: always bind the tenant \
             predicate from `TenantContext`. Use PostgreSQL when the database must enforce isolation."
        ))
    }
}

/// Options accepted by `forge new`.
#[derive(Debug, Clone)]
pub(crate) struct NewApplication {
    pub(crate) name: String,
    pub(crate) docker: bool,
    pub(crate) ci: bool,
    pub(crate) database: Database,
    /// Full-stack (server-rendered views, ADR 0022); `false` with `--api`.
    pub(crate) views: bool,
    pub(crate) forge: ForgeSource,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Always,
    Docker,
    Ci,
    Database,
    Postgres,
    PostgresDocker,
    Sqlite,
    /// Full-stack applications only.
    Views,
    /// `--api` applications only.
    Api,
    /// Applications with a database or views.
    BuildScript,
    /// Full-stack PostgreSQL applications: sign-in pages and `setup`
    /// (ADR 0026). Also the files of `forge generate authentication`.
    Authentication,
}

struct Template {
    path: &'static str,
    content: &'static str,
    group: Group,
}

/// A binary file (an image) that `forge new` copies byte for byte: it is not
/// rendered, so it takes no variables or conditionals.
struct BinaryFile {
    path: &'static str,
    content: &'static [u8],
    group: Group,
}

const BINARY_FILES: &[BinaryFile] = &[BinaryFile {
    path: "assets/forge-rust-logo.png",
    content: include_bytes!("../templates/app/assets/forge-rust-logo.png"),
    group: Group::Views,
}];

macro_rules! template {
    ($group:expr, $path:literal => $source:literal) => {
        Template {
            path: $path,
            content: include_str!(concat!("../templates/app/", $source)),
            group: $group,
        }
    };
}

const TEMPLATES: &[Template] = &[
    template!(Group::Always, "Cargo.toml" => "Cargo.toml.tmpl"),
    template!(Group::Always, "rust-toolchain.toml" => "rust-toolchain.toml.tmpl"),
    template!(Group::Always, ".gitignore" => "gitignore.tmpl"),
    template!(Group::Always, "README.md" => "README.md.tmpl"),
    template!(Group::Always, "src/main.rs" => "src/main.rs.tmpl"),
    template!(Group::Always, "src/lib.rs" => "src/lib.rs.tmpl"),
    template!(Group::Always, "src/domain/mod.rs" => "src/domain/mod.rs.tmpl"),
    template!(Group::Always, "src/application/mod.rs" => "src/application/mod.rs.tmpl"),
    template!(Group::Always, "src/adapters/mod.rs" => "src/adapters/mod.rs.tmpl"),
    template!(Group::Api, "src/adapters/http/mod.rs" => "src/adapters/http/mod.rs.tmpl"),
    template!(Group::Views, "src/adapters/http/mod.rs" => "src/adapters/http/mod_views.rs.tmpl"),
    template!(
        Group::Views,
        "src/adapters/http/views/mod.rs" => "src/adapters/http/views/mod.rs.tmpl"
    ),
    template!(
        Group::Views,
        "src/adapters/http/views/welcome.rs" => "src/adapters/http/views/welcome.rs.tmpl"
    ),
    template!(
        Group::Views,
        "templates/layouts/application.html" => "templates/layouts/application.html.tmpl"
    ),
    template!(
        Group::Views,
        "templates/layouts/_flash.html" => "templates/layouts/_flash.html.tmpl"
    ),
    template!(
        Group::Views,
        "templates/welcome/index.html" => "templates/welcome/index.html.tmpl"
    ),
    template!(Group::Views, "assets/application.css" => "assets/application.css.tmpl"),
    template!(Group::Views, "assets/application.js" => "assets/application.js.tmpl"),
    template!(Group::Views, "tests/http.rs" => "tests/http.rs.tmpl"),
    template!(Group::Always, "src/infrastructure/mod.rs" => "src/infrastructure/mod.rs.tmpl"),
    template!(Group::Always, "src/bootstrap/mod.rs" => "src/bootstrap/mod.rs.tmpl"),
    template!(Group::Always, "tests/architecture.rs" => "tests/architecture.rs.tmpl"),
    template!(Group::BuildScript, "build.rs" => "build.rs.tmpl"),
    template!(Group::Database, ".gitattributes" => "gitattributes.tmpl"),
    template!(Group::Database, "migrations/.gitkeep" => "migrations/gitkeep.tmpl"),
    template!(
        Group::Postgres,
        "migrations/00000000000000000001_forge_audit.up.sql" => "migrations/00000000000000000001_forge_audit.up.sql.tmpl"
    ),
    template!(
        Group::Postgres,
        "migrations/00000000000000000001_forge_audit.down.sql" => "migrations/00000000000000000001_forge_audit.down.sql.tmpl"
    ),
    template!(
        Group::Postgres,
        "migrations/00000000000000000002_forge_identity.up.sql" => "migrations/00000000000000000002_forge_identity.up.sql.tmpl"
    ),
    template!(
        Group::Postgres,
        "migrations/00000000000000000002_forge_identity.down.sql" => "migrations/00000000000000000002_forge_identity.down.sql.tmpl"
    ),
    template!(
        Group::Authentication,
        "migrations/00000000000000000003_forge_authentication.up.sql" => "migrations/00000000000000000003_forge_authentication.up.sql.tmpl"
    ),
    template!(
        Group::Authentication,
        "migrations/00000000000000000003_forge_authentication.down.sql" => "migrations/00000000000000000003_forge_authentication.down.sql.tmpl"
    ),
    template!(
        Group::Authentication,
        "src/infrastructure/accounts/mod.rs" => "src/infrastructure/accounts/mod.rs.tmpl"
    ),
    template!(
        Group::Authentication,
        "src/adapters/http/authentication.rs" => "src/adapters/http/authentication.rs.tmpl"
    ),
    template!(
        Group::Authentication,
        "src/adapters/http/views/authentication.rs" => "src/adapters/http/views/authentication.rs.tmpl"
    ),
    template!(
        Group::Authentication,
        "templates/sessions/new.html" => "templates/sessions/new.html.tmpl"
    ),
    template!(
        Group::Authentication,
        "templates/sessions/_sign_out.html" => "templates/sessions/_sign_out.html.tmpl"
    ),
    template!(
        Group::Authentication,
        "templates/passwords/new.html" => "templates/passwords/new.html.tmpl"
    ),
    template!(
        Group::Authentication,
        "templates/passwords/edit.html" => "templates/passwords/edit.html.tmpl"
    ),
    template!(Group::Authentication, "src/bootstrap/setup.rs" => "src/bootstrap/setup.rs.tmpl"),
    template!(
        Group::Authentication,
        "tests/authentication_http.rs" => "tests/authentication_http.rs.tmpl"
    ),
    template!(
        Group::Authentication,
        "tests/authentication_postgres.rs" => "tests/authentication_postgres.rs.tmpl"
    ),
    template!(Group::Database, "src/bootstrap/seeds.rs" => "src/bootstrap/seeds.rs.tmpl"),
    template!(
        Group::Postgres,
        "src/infrastructure/audit/mod.rs" => "src/infrastructure/audit/mod.rs.tmpl"
    ),
    template!(
        Group::Postgres,
        "src/infrastructure/auth/mod.rs" => "src/infrastructure/auth/mod.rs.tmpl"
    ),
    template!(
        Group::Postgres,
        "src/infrastructure/tenancy/mod.rs" => "src/infrastructure/tenancy/mod.rs.tmpl"
    ),
    template!(
        Group::Database,
        "src/infrastructure/security/mod.rs" => "src/infrastructure/security/mod.rs.tmpl"
    ),
    template!(
        Group::Postgres,
        "src/infrastructure/database/mod.rs" => "src/infrastructure/database/mod.rs.tmpl"
    ),
    template!(
        Group::PostgresDocker,
        "docker/postgres/init.sql" => "docker/postgres/init.sql.tmpl"
    ),
    template!(Group::Sqlite, "storage/.gitkeep" => "storage/gitkeep.tmpl"),
    template!(Group::Docker, "Dockerfile" => "Dockerfile.tmpl"),
    template!(Group::Docker, ".dockerignore" => "dockerignore.tmpl"),
    template!(Group::Docker, "compose.yaml" => "compose.yaml.tmpl"),
    template!(Group::Docker, "tests/container.rs" => "tests/container.rs.tmpl"),
    template!(Group::Ci, ".github/workflows/ci.yml" => "github/workflows/ci.yml.tmpl"),
    template!(Group::Ci, ".github/dependabot.yml" => "github/dependabot.yml.tmpl"),
];

/// The files of the sign-in pages and `setup` (ADR 0026), exactly as
/// `forge new` writes them for a full-stack PostgreSQL application.
pub(crate) fn authentication_files() -> Vec<(&'static str, String)> {
    TEMPLATES
        .iter()
        .filter(|template| template.group == Group::Authentication)
        .map(|template| {
            (
                template.path,
                render(template.content, &[], AUTHENTICATION_CONDITIONS),
            )
        })
        .collect()
}

/// The statements `forge new` puts at the end of `compose` to wire the
/// sign-in pages, from the comment that introduces them to the call of
/// `authentication::routes`.
pub(crate) fn authentication_compose_statements() -> Option<String> {
    let bootstrap = TEMPLATES
        .iter()
        .find(|template| template.path == "src/bootstrap/mod.rs")?;
    let rendered = render(bootstrap.content, &[], AUTHENTICATION_CONDITIONS);
    let start = rendered.find("    // Sign-in, sign-out")?;
    let call = "crate::adapters::http::authentication::routes(composition, &site, ports)?;\n";
    let end = start + rendered[start..].find(call)? + call.len();
    Some(rendered[start..end].to_owned())
}

const AUTHENTICATION_CONDITIONS: Conditions = Conditions {
    docker: false,
    database: Database::Postgresql,
    views: true,
};

/// Modules that exist to make each ring's responsibilities discoverable.
/// Each is a documented, empty module until a feature needs it.
const RESERVED_MODULES: &[(&str, &str)] = &[
    (
        "src/domain/entities",
        "Entities: domain objects with identity and invariants.",
    ),
    (
        "src/domain/value_objects",
        "Value objects: immutable, self-validating domain values.",
    ),
    (
        "src/domain/services",
        "Domain services: business rules spanning several entities.",
    ),
    (
        "src/domain/events",
        "Domain events: facts produced by domain behavior.",
    ),
    (
        "src/domain/errors",
        "Domain errors: typed business rule violations.",
    ),
    (
        "src/application/commands",
        "Commands: validated intents that change state.",
    ),
    (
        "src/application/queries",
        "Queries: read models that never change state.",
    ),
    (
        "src/application/use_cases",
        "Use cases: orchestration and transaction boundaries.",
    ),
    (
        "src/application/ports",
        "Ports: traits implemented by adapters and infrastructure.",
    ),
    (
        "src/adapters/persistence",
        "Persistence adapters implementing repository ports.",
    ),
    (
        "src/adapters/messaging",
        "Messaging adapters for integration events.",
    ),
    ("src/adapters/storage", "Object storage adapters."),
    ("src/adapters/email", "Email delivery adapters."),
    (
        "src/adapters/ai",
        "AI provider adapters implementing the application's AI ports.",
    ),
    (
        "src/infrastructure/database",
        "Database pools, migrations and transactions.",
    ),
    ("src/infrastructure/cache", "Cache clients."),
    ("src/infrastructure/queue", "Job queue infrastructure."),
    (
        "src/infrastructure/telemetry",
        "Telemetry exporters and instrumentation.",
    ),
    (
        "src/infrastructure/security",
        "Secrets, keys and security mechanisms.",
    ),
];

/// A positive, time-sortable migration version: nanoseconds since the Unix
/// epoch.
pub(crate) fn migration_version() -> Result<String, CliError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(CliError::Clock)?;
    Ok(elapsed.as_nanos().to_string())
}

pub(crate) fn create_migration(root: &Path, name: &str) -> Result<(PathBuf, PathBuf), CliError> {
    let normalized = normalize_migration_name(name)?;
    let version = migration_version()?;
    let migrations = root.join("migrations");
    fs::create_dir_all(&migrations).map_err(|source| CliError::CreateDirectory {
        path: migrations.clone(),
        source,
    })?;

    let stem = format!("{version}_{normalized}");
    let up = migrations.join(format!("{stem}.up.sql"));
    let down = migrations.join(format!("{stem}.down.sql"));

    write_new_file(&up, &format!("-- {normalized}: apply migration\n\n"))?;
    if let Err(error) = write_new_file(&down, &format!("-- {normalized}: revert migration\n\n")) {
        let _ = fs::remove_file(&up);
        return Err(error);
    }

    Ok((up, down))
}

fn normalize_migration_name(name: &str) -> Result<String, CliError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CliError::InvalidMigrationName {
            name: name.to_owned(),
            reason: "the name cannot be empty",
        });
    }
    if name.len() > 80 || !name.is_ascii() {
        return Err(CliError::InvalidMigrationName {
            name: name.to_owned(),
            reason: "use at most 80 ASCII characters",
        });
    }

    let mut normalized = String::with_capacity(name.len());
    for character in name.chars() {
        match character {
            'a'..='z' | '0'..='9' | '_' => normalized.push(character),
            'A'..='Z' => normalized.push(character.to_ascii_lowercase()),
            '-' | ' ' => normalized.push('_'),
            _ => {
                return Err(CliError::InvalidMigrationName {
                    name: name.to_owned(),
                    reason: "use letters, numbers, spaces, '-' or '_' only",
                });
            }
        }
    }

    while normalized.contains("__") {
        normalized = normalized.replace("__", "_");
    }
    let normalized = normalized.trim_matches('_').to_owned();
    if normalized.is_empty()
        || !normalized
            .as_bytes()
            .first()
            .map(|byte| byte.is_ascii_lowercase())
            .unwrap_or(false)
    {
        return Err(CliError::InvalidMigrationName {
            name: name.to_owned(),
            reason: "the normalized name must start with a letter",
        });
    }
    Ok(normalized)
}

pub(crate) fn create_application(
    parent: &Path,
    options: &NewApplication,
) -> Result<PathBuf, CliError> {
    validate_application_name(&options.name)?;
    let forge_test_dependency =
        forge_dependency(&options.forge, options.database, options.views, true)?;
    let forge_dependency =
        forge_dependency(&options.forge, options.database, options.views, false)?;

    let destination = parent.join(&options.name);
    match fs::create_dir(&destination) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(CliError::DestinationExists(destination));
        }
        Err(source) => {
            return Err(CliError::CreateDirectory {
                path: destination,
                source,
            });
        }
    }

    let result = write_application(
        &destination,
        options,
        &forge_dependency,
        &forge_test_dependency,
    );

    if result.is_err() {
        let _cleanup_result = fs::remove_dir_all(&destination);
    }

    result.map(|()| destination)
}

fn validate_application_name(name: &str) -> Result<(), CliError> {
    if name.is_empty() {
        return invalid_name(name, "the name cannot be empty");
    }
    if name.len() > 64 {
        return invalid_name(name, "use at most 64 ASCII characters");
    }
    if !name.is_ascii() {
        return invalid_name(name, "use ASCII lowercase letters, numbers, `_`, or `-`");
    }
    if !name.as_bytes()[0].is_ascii_lowercase() {
        return invalid_name(
            name,
            "the first character must be an ASCII lowercase letter",
        );
    }
    if !name.bytes().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, b'_' | b'-')
    }) {
        return invalid_name(
            name,
            "use only ASCII lowercase letters, numbers, `_`, or `-`",
        );
    }
    if name.ends_with('-') || name.ends_with('_') {
        return invalid_name(name, "the last character must be a letter or number");
    }
    if is_reserved_name(name) {
        return invalid_name(name, "this package name is reserved by Rust or Cargo");
    }
    Ok(())
}

fn invalid_name(name: &str, reason: &'static str) -> Result<(), CliError> {
    Err(CliError::InvalidApplicationName {
        name: name.to_owned(),
        reason,
    })
}

fn is_reserved_name(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do",
        "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
        "std", "core", "alloc", "test", "app", "forge", "con", "prn", "aux", "nul", "com1", "com2",
        "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4",
        "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    RESERVED.contains(&name)
}

/// Renders the `forge` dependency, including the backend, `views` and (for
/// the dev-dependency) `testing` features, as a TOML inline table.
fn forge_dependency(
    source: &ForgeSource,
    database: Database,
    views: bool,
    testing: bool,
) -> Result<String, CliError> {
    let features = database.forge_features(views, testing);
    match source {
        ForgeSource::GitTag => Ok(format!(
            "{{ git = \"{FORGE_REPOSITORY}\", tag = \"v{FORGE_VERSION}\"{features} }}"
        )),
        ForgeSource::Path(path) => {
            validate_forge_path(path)?;
            Ok(format!("{{ path = \"{path}\"{features} }}"))
        }
    }
}

/// Renders the `forge-views` build dependency of full-stack applications:
/// only the asset embedding helper, from the same release tag or checkout as
/// `forge`. With `--forge-path`, `forge-views` is the sibling directory of
/// the `forge` crate (`crates/forge-views` next to `crates/forge`).
fn forge_views_build_dependency(source: &ForgeSource) -> Result<String, CliError> {
    const BUILD_ONLY: &str = "default-features = false, features = [\"build\"]";
    match source {
        ForgeSource::GitTag => Ok(format!(
            "{{ git = \"{FORGE_REPOSITORY}\", tag = \"v{FORGE_VERSION}\", {BUILD_ONLY} }}"
        )),
        ForgeSource::Path(path) => {
            validate_forge_path(path)?;
            let forge = path.trim_end_matches('/');
            let sibling = match forge.rsplit_once('/') {
                Some((parent, _)) => format!("{parent}/forge-views"),
                None => "forge-views".to_owned(),
            };
            Ok(format!("{{ path = \"{sibling}\", {BUILD_ONLY} }}"))
        }
    }
}

fn validate_forge_path(path: &str) -> Result<(), CliError> {
    let rejected = path.is_empty()
        || path
            .chars()
            .any(|character| character == '"' || character == '\\' || character.is_control());
    if rejected {
        return Err(CliError::InvalidForgePath {
            path: path.to_owned(),
            reason: "use a non-empty path with forward slashes and no quotes or control characters",
        });
    }
    Ok(())
}

fn write_application(
    root: &Path,
    options: &NewApplication,
    forge_dependency: &str,
    forge_test_dependency: &str,
) -> Result<(), CliError> {
    let database_name = options.name.replace('-', "_");
    let urls = DatabaseUrls::new(options.database, &database_name);
    let forge_views_dependency = forge_views_build_dependency(&options.forge)?;
    let variables = [
        ("%%APP_NAME%%", options.name.as_str()),
        ("%%RUST_VERSION%%", RUST_VERSION),
        ("%%FORGE_DEPENDENCY%%", forge_dependency),
        ("%%FORGE_TEST_DEPENDENCY%%", forge_test_dependency),
        (
            "%%FORGE_VIEWS_BUILD_DEPENDENCY%%",
            forge_views_dependency.as_str(),
        ),
        ("%%DATABASE_NAME%%", database_name.as_str()),
        ("%%DATABASE_DRIVER%%", options.database.driver()),
        ("%%DATABASE_LABEL%%", options.database.label()),
        (
            "%%DATABASE_RUNTIME_URL_LOCAL%%",
            urls.runtime_local.as_str(),
        ),
        (
            "%%DATABASE_MIGRATION_URL_LOCAL%%",
            urls.migration_local.as_str(),
        ),
        (
            "%%DATABASE_RUNTIME_URL_CONTAINER%%",
            urls.runtime_container.as_str(),
        ),
        (
            "%%DATABASE_MIGRATION_URL_CONTAINER%%",
            urls.migration_container.as_str(),
        ),
    ];

    let enabled = |group: Group| match group {
        Group::Always => true,
        Group::Docker => options.docker,
        Group::Ci => options.ci,
        Group::Database => options.database.is_enabled(),
        Group::Postgres => options.database == Database::Postgresql,
        Group::PostgresDocker => options.database == Database::Postgresql && options.docker,
        Group::Sqlite => options.database == Database::Sqlite3,
        Group::Views => options.views,
        Group::Api => !options.views,
        Group::BuildScript => options.views || options.database.is_enabled(),
        Group::Authentication => options.database == Database::Postgresql && options.views,
    };
    for template in TEMPLATES {
        if enabled(template.group) {
            let content = render(
                template.content,
                &variables,
                Conditions {
                    docker: options.docker,
                    database: options.database,
                    views: options.views,
                },
            );
            write_file(&root.join(template.path), &content)?;
        }
    }
    for file in BINARY_FILES {
        if enabled(file.group) {
            write_file(&root.join(file.path), file.content)?;
        }
    }

    for (directory, description) in RESERVED_MODULES {
        let replaced_by_template = match *directory {
            "src/infrastructure/database" => options.database == Database::Postgresql,
            "src/infrastructure/security" => options.database.is_enabled(),
            _ => false,
        };
        if replaced_by_template {
            continue;
        }
        write_file(
            &root.join(directory).join("mod.rs"),
            format!("//! {description}\n"),
        )?;
    }

    Ok(())
}

/// Local and container connection URLs of the selected backend.
///
/// Credentials are development-only and appear in the generated Compose file
/// and README. PostgreSQL uses separate migration and runtime roles; MySQL,
/// MariaDB and SQLite use one development account or file.
struct DatabaseUrls {
    runtime_local: String,
    migration_local: String,
    runtime_container: String,
    migration_container: String,
}

impl DatabaseUrls {
    fn new(database: Database, name: &str) -> Self {
        match database {
            Database::Postgresql | Database::Disabled => Self {
                runtime_local: format!("postgres://app_runtime:app_runtime@127.0.0.1:5432/{name}"),
                migration_local: format!(
                    "postgres://app_migrator:app_migrator@127.0.0.1:5432/{name}"
                ),
                runtime_container: format!("postgres://app_runtime:app_runtime@db:5432/{name}"),
                migration_container: format!("postgres://app_migrator:app_migrator@db:5432/{name}"),
            },
            Database::Mysql | Database::Mariadb => {
                let scheme = if database == Database::Mysql {
                    "mysql"
                } else {
                    "mariadb"
                };
                let local = format!("{scheme}://app:app@127.0.0.1:3306/{name}");
                let container = format!("{scheme}://app:app@db:3306/{name}");
                Self {
                    migration_local: local.clone(),
                    runtime_local: local,
                    migration_container: container.clone(),
                    runtime_container: container,
                }
            }
            Database::Sqlite3 => {
                let local = "sqlite://storage/development.sqlite3?mode=rwc".to_owned();
                let container = "sqlite:///data/production.sqlite3?mode=rwc".to_owned();
                Self {
                    migration_local: local.clone(),
                    runtime_local: local,
                    migration_container: container.clone(),
                    runtime_container: container,
                }
            }
        }
    }
}

/// The capabilities template conditionals can test.
#[derive(Clone, Copy)]
struct Conditions {
    docker: bool,
    database: Database,
    views: bool,
}

/// Substitutes template variables and evaluates nested capability conditionals.
///
/// `%%if name%%` / `%%if !name%%` ... `%%end%%` blocks nest. Known names are
/// `docker`, `views` (full-stack, not `--api`), `authentication` (full-stack
/// PostgreSQL: sign-in pages and `setup`), `database` (any backend),
/// `postgresql`, `mysql`, `mariadb`, `mysql_family` (MySQL or MariaDB) and
/// `sqlite`. Unknown names are false.
fn render(template: &str, variables: &[(&str, &str)], conditions: Conditions) -> String {
    let Conditions {
        docker,
        database,
        views,
    } = conditions;
    let condition = |name: &str| match name {
        "docker" => docker,
        "views" => views,
        "authentication" => views && database == Database::Postgresql,
        "database" => database.is_enabled(),
        "postgresql" => database == Database::Postgresql,
        "mysql" => database == Database::Mysql,
        "mariadb" => database == Database::Mariadb,
        "mysql_family" => database.is_mysql_family(),
        "sqlite" => database == Database::Sqlite3,
        _ => false,
    };
    render_template(template, variables, condition)
}

/// Evaluates `%%if name%%` / `%%if !name%%` ... `%%end%%` blocks (which
/// nest) with `condition`, then substitutes `variables` in order. The
/// directive lines themselves are removed.
pub(crate) fn render_template(
    template: &str,
    variables: &[(&str, &str)],
    condition: impl Fn(&str) -> bool,
) -> String {
    let mut output = String::with_capacity(template.len());
    let mut stack = vec![true];

    for line in template.split_inclusive('\n') {
        let marker = line.trim();
        let directive = match marker {
            "%%IF_DOCKER%%" => Some(("docker", false)),
            "%%END_IF_DOCKER%%" => {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            _ if marker.starts_with("%%if ") && marker.ends_with("%%") => {
                let name = &marker[5..marker.len() - 2];
                Some((
                    name.strip_prefix('!').unwrap_or(name),
                    name.starts_with('!'),
                ))
            }
            "%%end%%" => {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            _ => None,
        };

        if let Some((name, negated)) = directive {
            let enabled = condition(name);
            let enabled = if negated { !enabled } else { enabled };
            let parent = *stack.last().unwrap_or(&true);
            stack.push(parent && enabled);
            continue;
        }

        if *stack.last().unwrap_or(&true) {
            output.push_str(line);
        }
    }

    for (name, value) in variables {
        output = output.replace(name, value);
    }
    output
}

pub(crate) fn write_new_file(path: &Path, content: &str) -> Result<(), CliError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                CliError::DestinationExists(path.to_path_buf())
            } else {
                CliError::WriteFile {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
    file.write_all(content.as_bytes())
        .map_err(|source| CliError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
}

fn write_file(path: &Path, content: impl AsRef<[u8]>) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| CliError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, content).map_err(|source| CliError::WriteFile {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn options(name: &str) -> NewApplication {
        NewApplication {
            name: name.to_owned(),
            docker: true,
            ci: true,
            database: Database::Postgresql,
            views: true,
            forge: ForgeSource::GitTag,
        }
    }

    fn read(root: &Path, relative: &str) -> String {
        fs::read_to_string(root.join(relative))
            .unwrap_or_else(|error| panic!("{relative} should be readable: {error}"))
    }

    /// Reads every file below `root` except build outputs and the lockfile.
    fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
            let entries = fs::read_dir(directory).expect("directory should be readable");
            for entry in entries {
                let path = entry.expect("entry should be readable").path();
                let relative = path
                    .strip_prefix(root)
                    .expect("path is below root")
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative == "target" || relative == "Cargo.lock" {
                    continue;
                }
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    let content = fs::read(&path).expect("file should be readable");
                    files.insert(relative, content);
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    /// The rendered text files below `root`: the snapshot without the binary
    /// files `forge new` copies verbatim. Every other file must be UTF-8.
    fn text_snapshot(root: &Path) -> BTreeMap<String, String> {
        snapshot(root)
            .into_iter()
            .filter(|(path, _)| !BINARY_FILES.iter().any(|file| file.path == path))
            .map(|(path, content)| {
                let text =
                    String::from_utf8(content).unwrap_or_else(|_| panic!("{path} should be UTF-8"));
                (path, text)
            })
            .collect()
    }

    #[test]
    fn creates_reversible_migration_files() {
        let root = tempfile::tempdir().expect("temporary directory should be available");
        let (up, down) =
            create_migration(root.path(), "Create Users").expect("migration should be created");

        assert!(up.is_file());
        assert!(down.is_file());
        assert!(
            up.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("_create_users.up.sql")
        );
        assert!(
            down.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("_create_users.down.sql")
        );
        assert!(
            read(
                root.path(),
                up.strip_prefix(root.path()).unwrap().to_str().unwrap()
            )
            .contains("apply migration")
        );
        assert!(
            read(
                root.path(),
                down.strip_prefix(root.path()).unwrap().to_str().unwrap()
            )
            .contains("revert migration")
        );
    }

    #[test]
    fn rejects_unsafe_migration_names() {
        for name in ["", "../users", "1users", "users/roles", "café"] {
            assert!(normalize_migration_name(name).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn accepts_valid_cargo_package_names() {
        for name in ["shop", "shop-api", "shop_api", "shop2"] {
            assert!(validate_application_name(name).is_ok(), "rejected {name}");
        }
    }

    #[test]
    fn rejects_unsafe_or_invalid_names() {
        for name in [
            "", "Shop", "2shop", "../shop", "shop/api", "shop api", "shop-", "fn", "con", "café",
            "app", "forge",
        ] {
            assert!(validate_application_name(name).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn rust_version_matches_the_framework_toolchain() {
        let toolchain = read(
            Path::new(env!("CARGO_MANIFEST_DIR")),
            "../../rust-toolchain.toml",
        );
        assert!(
            toolchain.contains(&format!("channel = \"{RUST_VERSION}\"")),
            "RUST_VERSION must equal the workspace rust-toolchain.toml channel"
        );
    }

    #[test]
    fn creates_a_dockerized_application_by_default() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");

        let root = create_application(parent.path(), &options("sample-app"))
            .expect("application generation should succeed");

        assert_eq!(root, parent.path().join("sample-app"));
        for directory in RESERVED_MODULES.iter().map(|(directory, _)| directory) {
            assert!(root.join(directory).join("mod.rs").is_file(), "{directory}");
        }

        let manifest = read(&root, "Cargo.toml");
        assert!(manifest.contains("name = \"sample-app\""));
        assert!(manifest.contains("sqlx"));
        assert!(manifest.contains("argon2"));
        assert!(manifest.contains("cookie"));
        assert!(manifest.contains("getrandom"));
        assert!(manifest.contains("sha2"));
        assert!(root.join("build.rs").is_file());
        assert!(root.join("migrations/.gitkeep").is_file());
        assert!(
            root.join("migrations/00000000000000000001_forge_audit.up.sql")
                .is_file()
        );
        assert!(
            root.join("migrations/00000000000000000001_forge_audit.down.sql")
                .is_file()
        );
        assert!(
            root.join("migrations/00000000000000000002_forge_identity.up.sql")
                .is_file()
        );
        assert!(
            root.join("migrations/00000000000000000002_forge_identity.down.sql")
                .is_file()
        );
        assert!(root.join("src/infrastructure/audit/mod.rs").is_file());
        assert!(root.join("src/infrastructure/auth/mod.rs").is_file());
        assert!(root.join("src/infrastructure/tenancy/mod.rs").is_file());
        assert!(root.join("src/infrastructure/security/mod.rs").is_file());
        assert!(root.join("docker/postgres/init.sql").is_file());

        let identity_migration = read(
            &root,
            "migrations/00000000000000000002_forge_identity.up.sql",
        );
        assert!(identity_migration.contains("CREATE TABLE forge_principals"));
        assert!(identity_migration.contains("credential_digest bytea"));
        assert!(identity_migration.contains("csrf_digest bytea"));
        assert!(identity_migration.contains("CREATE TABLE forge_tenant_memberships"));
        assert!(identity_migration.contains("GRANT UPDATE (password_hash, updated_at, version)"));

        let auth_module = read(&root, "src/infrastructure/auth/mod.rs");
        assert!(auth_module.contains("PasswordCredentialVersion"));
        assert!(auth_module.contains("replace_password_hash"));
        assert!(auth_module.contains("version = version + 1"));

        let security_module = read(&root, "src/infrastructure/security/mod.rs");
        assert!(security_module.contains("Argon2idPasswordHasher"));
        assert!(security_module.contains("needs_rehash"));
        assert!(security_module.contains("replace_password_hash"));
        assert!(security_module.contains("__Host-forge_session"));
        assert!(security_module.contains("x-csrf-token"));

        let database_module = read(&root, "src/infrastructure/database/mod.rs");
        assert!(database_module.contains("begin_tenant_transaction"));
        assert!(database_module.contains("forge.tenant_id"));
        assert!(database_module.contains("forge.principal_id"));

        let postgres_init = read(&root, "docker/postgres/init.sql");
        assert!(postgres_init.contains("CREATE ROLE app_migrator"));
        assert!(postgres_init.contains("CREATE ROLE app_runtime"));
        assert!(postgres_init.contains("NOBYPASSRLS"));
        assert!(manifest.contains(&format!(
            "forge = {{ git = \"https://github.com/rocketinsidecorp/forge-rust\", tag = \"v{FORGE_VERSION}\", features = [\"views\"] }}"
        )));
        assert!(manifest.contains(&format!(
            "forge-views = {{ git = \"https://github.com/rocketinsidecorp/forge-rust\", tag = \"v{FORGE_VERSION}\", default-features = false, features = [\"build\"] }}"
        )));

        let dockerfile = read(&root, "Dockerfile");
        assert!(dockerfile.contains(&format!("ARG RUST_VERSION={RUST_VERSION}")));
        assert!(dockerfile.contains("cargo build --release --locked --bin sample-app"));
        assert!(dockerfile.contains("FROM gcr.io/distroless/cc-debian13:nonroot AS runtime"));
        assert!(dockerfile.contains("USER 65532:65532"));
        assert!(dockerfile.contains("CMD [\"/usr/local/bin/sample-app\", \"healthcheck\"]"));
        assert!(dockerfile.contains("ENTRYPOINT [\"/usr/local/bin/sample-app\"]"));
        assert!(!dockerfile.contains("generate-lockfile"));

        let dockerignore = read(&root, ".dockerignore");
        assert!(dockerignore.contains("**/.env"));
        assert!(dockerignore.contains("**/target"));

        let compose = read(&root, "compose.yaml");
        assert!(compose.contains("no-new-privileges:true"));
        assert!(compose.contains("app_migrator"));
        assert!(compose.contains("app_runtime"));
        assert!(compose.contains("FORGE_MIGRATION_DATABASE_URL"));
        assert!(compose.contains("FORGE_DATABASE_URL"));
        assert!(!compose.contains("postgres://app:app@"));
        assert!(read(&root, ".github/workflows/ci.yml").contains("docker/build-push-action"));
        assert!(read(&root, ".github/dependabot.yml").contains("package-ecosystem: docker"));
        assert!(root.join("tests/container.rs").is_file());
        assert!(root.join("tests/architecture.rs").is_file());
    }

    #[test]
    fn skip_flags_omit_docker_and_ci_assets() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let options = NewApplication {
            docker: false,
            ci: false,
            database: Database::Disabled,
            ..options("plain")
        };

        let root = create_application(parent.path(), &options).expect("generation should succeed");

        for path in [
            "Dockerfile",
            ".dockerignore",
            "compose.yaml",
            "tests/container.rs",
            ".github",
        ] {
            assert!(!root.join(path).exists(), "{path} should not be generated");
        }
        assert!(!read(&root, "README.md").contains("docker"));
        assert!(!read(&root, "Cargo.toml").contains("sqlx"));
        assert!(!root.join("migrations").exists());
        assert!(!root.join("src/infrastructure/audit").exists());
        assert!(!root.join("src/infrastructure/auth").exists());
        assert!(!root.join("src/infrastructure/tenancy").exists());
        assert!(!read(&root, "Cargo.toml").contains("argon2"));
        assert!(!read(&root, "Cargo.toml").contains("cookie"));
        assert!(!root.join("docker/postgres/init.sql").exists());
    }

    #[test]
    fn skipping_docker_keeps_database_code_without_compose_bootstrap() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let options = NewApplication {
            docker: false,
            database: Database::Postgresql,
            ..options("plain-db")
        };

        let root = create_application(parent.path(), &options).expect("generation should succeed");

        assert!(read(&root, "Cargo.toml").contains("sqlx"));
        assert!(root.join("src/infrastructure/database/mod.rs").is_file());
        assert!(root.join("migrations/.gitkeep").is_file());
        assert!(!root.join("compose.yaml").exists());
        assert!(!root.join("docker/postgres/init.sql").exists());
    }

    #[test]
    fn skipping_docker_keeps_ci_without_the_image_job() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let options = NewApplication {
            docker: false,
            database: Database::Disabled,
            ..options("plain")
        };

        let root = create_application(parent.path(), &options).expect("generation should succeed");

        let workflow = read(&root, ".github/workflows/ci.yml");
        assert!(workflow.contains("cargo test"));
        assert!(!workflow.contains("docker"));
        assert!(!read(&root, ".github/dependabot.yml").contains("docker"));
    }

    fn generate(database: Database, docker: bool) -> (tempfile::TempDir, PathBuf) {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let options = NewApplication {
            database,
            docker,
            forge: ForgeSource::Path("../forge/crates/forge".to_owned()),
            ..options("sample-app")
        };
        let root = create_application(parent.path(), &options).expect("generation should succeed");
        (parent, root)
    }

    const POSTGRES_ONLY_FILES: &[&str] = &[
        "migrations/00000000000000000001_forge_audit.up.sql",
        "migrations/00000000000000000002_forge_identity.up.sql",
        "docker/postgres/init.sql",
        "src/infrastructure/audit/mod.rs",
        "src/infrastructure/auth/mod.rs",
        "src/infrastructure/tenancy/mod.rs",
    ];

    fn assert_no_postgres_artifacts(root: &Path) {
        for file in POSTGRES_ONLY_FILES {
            assert!(!root.join(file).exists(), "{file} must not be generated");
        }
        for (path, content) in text_snapshot(root) {
            for needle in [
                "gen_random_uuid",
                "ROW LEVEL SECURITY",
                "set_config(",
                "PgPool",
                "postgres:",
                "pg_isready",
                "app_runtime",
            ] {
                assert!(!content.contains(needle), "{path} contains `{needle}`");
            }
        }
        assert!(read(root, "migrations/.gitkeep").is_empty());
    }

    #[test]
    fn postgresql_is_the_default_backend_with_default_features() {
        let (_parent, root) = generate(Database::Postgresql, true);

        assert!(
            read(&root, "Cargo.toml")
                .contains("forge = { path = \"../forge/crates/forge\", features = [\"views\"] }")
        );
        assert!(read(&root, "Cargo.toml").contains("\"postgres\", \"runtime-tokio\""));
        assert!(read(&root, "src/bootstrap/mod.rs").contains(".postgres(sqlx::migrate!())"));
        assert!(read(&root, "compose.yaml").contains("postgres:18.6-trixie@sha256:"));
        assert!(
            read(&root, "README.md").contains("postgres://app_runtime:app_runtime@127.0.0.1:5432/")
        );
        assert!(root.join("docker/postgres/init.sql").is_file());
        assert!(Database::Postgresql.unavailable_notice().is_none());
    }

    #[test]
    fn mysql_selects_the_mysql_driver_container_and_urls() {
        let (_parent, root) = generate(Database::Mysql, true);

        let manifest = read(&root, "Cargo.toml");
        assert!(manifest.contains(
            "forge = { path = \"../forge/crates/forge\", default-features = false, features = [\"mysql\", \"views\"] }"
        ));
        assert!(manifest.contains("\"mysql\", \"runtime-tokio\""));
        assert!(!manifest.contains("\"postgres\""));
        let bootstrap = read(&root, "src/bootstrap/mod.rs");
        assert!(bootstrap.contains(".mysql(sqlx::migrate!())"));
        assert!(!bootstrap.contains(".postgres("));
        let compose = read(&root, "compose.yaml");
        assert!(compose.contains("image: mysql:8.4"));
        assert!(compose.contains("MYSQL_DATABASE: sample_app"));
        assert!(compose.contains("mysqladmin\", \"ping\", \"--host=127.0.0.1"));
        assert!(compose.contains("db-data:/var/lib/mysql"));
        assert!(compose.contains("FORGE_DATABASE_URL: mysql://app:app@db:3306/sample_app"));
        assert!(compose.contains("  migrate:"));
        assert!(compose.contains("service_completed_successfully"));
        assert!(read(&root, "README.md").contains("mysql://app:app@127.0.0.1:3306/sample_app"));
        assert_no_postgres_artifacts(&root);
    }

    #[test]
    fn mariadb_uses_the_mysql_driver_with_a_mariadb_container_and_url() {
        let (_parent, root) = generate(Database::Mariadb, true);

        let manifest = read(&root, "Cargo.toml");
        assert!(manifest.contains("default-features = false, features = [\"mysql\", \"views\"]"));
        assert!(manifest.contains("\"mysql\", \"runtime-tokio\""));
        assert!(read(&root, "src/bootstrap/mod.rs").contains(".mysql(sqlx::migrate!())"));
        let compose = read(&root, "compose.yaml");
        assert!(compose.contains("image: mariadb:11.8"));
        assert!(compose.contains("MARIADB_DATABASE: sample_app"));
        assert!(compose.contains("healthcheck.sh\", \"--connect\", \"--innodb_initialized"));
        assert!(compose.contains("db-data:/var/lib/mysql"));
        assert!(compose.contains("mariadb://app:app@db:3306/sample_app"));
        assert!(!compose.contains("mysql:8.4"));
        assert!(read(&root, "README.md").contains("mariadb://app:app@127.0.0.1:3306/sample_app"));
        assert_no_postgres_artifacts(&root);
    }

    #[test]
    fn sqlite3_uses_a_file_volume_owned_by_the_runtime_user() {
        let (_parent, root) = generate(Database::Sqlite3, true);

        let manifest = read(&root, "Cargo.toml");
        assert!(manifest.contains(
            "forge = { path = \"../forge/crates/forge\", features = [\"sqlite\", \"views\"] }"
        ));
        assert!(manifest.contains("\"sqlite\", \"runtime-tokio\""));
        assert!(read(&root, "src/bootstrap/mod.rs").contains(".sqlite(sqlx::migrate!())"));
        let compose = read(&root, "compose.yaml");
        assert!(!compose.contains("  db:"));
        assert!(compose.contains("sqlite-data:/data"));
        assert!(compose.contains("sqlite:///data/production.sqlite3?mode=rwc"));
        assert!(compose.contains("  migrate:"));
        assert!(!compose.contains("service_healthy"));
        let dockerfile = read(&root, "Dockerfile");
        assert!(dockerfile.contains("COPY --from=build --chown=65532:65532 /out/data /data"));
        assert!(root.join("storage/.gitkeep").is_file());
        assert!(read(&root, ".gitignore").contains("/storage/*"));
        assert!(read(&root, ".dockerignore").contains("/storage"));
        assert!(read(&root, "README.md").contains("sqlite://storage/development.sqlite3?mode=rwc"));
        assert_no_postgres_artifacts(&root);
    }

    #[test]
    fn non_database_backends_have_no_storage_directory_or_sqlite_ignore_rules() {
        for database in [Database::Postgresql, Database::Mysql, Database::Disabled] {
            let (_parent, root) = generate(database, true);
            assert!(!root.join("storage").exists());
            assert!(!read(&root, ".gitignore").contains("storage"));
            assert!(!read(&root, "Dockerfile").contains("/data"));
        }
    }

    const AUTHENTICATION_FILES: &[&str] = &[
        "migrations/00000000000000000003_forge_authentication.up.sql",
        "migrations/00000000000000000003_forge_authentication.down.sql",
        "src/infrastructure/accounts/mod.rs",
        "src/adapters/http/authentication.rs",
        "src/adapters/http/views/authentication.rs",
        "templates/sessions/new.html",
        "templates/sessions/_sign_out.html",
        "templates/passwords/new.html",
        "templates/passwords/edit.html",
        "src/bootstrap/setup.rs",
        "tests/authentication_http.rs",
        "tests/authentication_postgres.rs",
    ];

    #[test]
    fn full_stack_postgresql_applications_get_the_sign_in_pages_setup_and_seeds() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let root = create_application(parent.path(), &options("auth-app"))
            .expect("application generation should succeed");

        for file in AUTHENTICATION_FILES {
            assert!(root.join(file).is_file(), "{file}");
        }
        assert!(root.join("src/bootstrap/seeds.rs").is_file());
        let bootstrap = read(&root, "src/bootstrap/mod.rs");
        assert!(bootstrap.contains("pub mod seeds;\npub mod setup;\n"));
        assert!(bootstrap.contains(".command(\"setup\", setup::SUMMARY, setup::run)"));
        assert!(bootstrap.contains(".command(\"seed\", seeds::SUMMARY, seeds::run)"));
        assert!(bootstrap.contains("crate::adapters::http::authentication::routes("));
        assert!(bootstrap.contains("forge::auth::UndeliverablePasswordResets"));
        assert!(read(&root, "src/infrastructure/mod.rs").contains("pub mod accounts;"));
        assert!(read(&root, "src/adapters/http/mod.rs").contains("pub mod authentication;"));
        assert!(read(&root, "src/adapters/http/views/mod.rs").contains("pub mod authentication;"));
        let migration = read(
            &root,
            "migrations/00000000000000000003_forge_authentication.up.sql",
        );
        assert!(migration.contains("token_digest bytea NOT NULL UNIQUE"));
        assert!(migration.contains("GRANT UPDATE (used_at) ON TABLE forge_password_resets"));
        assert!(!migration.contains("gen_random_uuid"));
    }

    #[test]
    fn only_full_stack_postgresql_applications_get_the_sign_in_pages() {
        for (database, views) in [
            (Database::Postgresql, false),
            (Database::Mysql, true),
            (Database::Sqlite3, true),
            (Database::Disabled, true),
        ] {
            let parent = tempfile::tempdir().expect("temporary directory should be available");
            let mut options = options("plain-app");
            options.database = database;
            options.views = views;
            let root = create_application(parent.path(), &options)
                .expect("application generation should succeed");

            for file in AUTHENTICATION_FILES {
                assert!(!root.join(file).exists(), "{database:?} {views}: {file}");
            }
            let bootstrap = read(&root, "src/bootstrap/mod.rs");
            assert!(
                !bootstrap.contains("pub mod setup;"),
                "{database:?} {views}"
            );
            assert_eq!(
                root.join("src/bootstrap/seeds.rs").is_file(),
                database != Database::Disabled,
                "{database:?}"
            );
            assert_eq!(
                bootstrap.contains(".command(\"seed\""),
                database != Database::Disabled
            );
        }
    }

    #[test]
    fn none_has_no_database_assets_and_minimal_forge_features() {
        let (_parent, root) = generate(Database::Disabled, true);

        assert!(
            read(&root, "Cargo.toml")
                .contains("default-features = false, features = [\"views\"] }")
        );
        assert!(!read(&root, "Cargo.toml").contains("sqlx"));
        assert!(!read(&root, "src/bootstrap/mod.rs").contains("migrate"));
        assert!(!root.join("migrations").exists());
        assert!(!read(&root, "compose.yaml").contains("db:"));
    }

    #[test]
    fn every_backend_and_option_combination_renders_every_placeholder() {
        for database in [
            Database::Postgresql,
            Database::Mysql,
            Database::Mariadb,
            Database::Sqlite3,
            Database::Disabled,
        ] {
            for docker in [true, false] {
                for views in [true, false] {
                    let parent = tempfile::tempdir().expect("temporary directory");
                    let options = NewApplication {
                        database,
                        docker,
                        views,
                        forge: ForgeSource::Path("../forge/crates/forge".to_owned()),
                        ..options("sample-app")
                    };
                    let root = create_application(parent.path(), &options).expect("generation");
                    for (path, content) in text_snapshot(&root) {
                        assert!(!content.contains("%%"), "{database:?} {views} {path}");
                    }
                }
            }
        }
    }

    #[test]
    fn unavailable_notice_lists_the_missing_modules_for_non_postgres_backends() {
        for database in [Database::Mysql, Database::Mariadb, Database::Sqlite3] {
            let notice = database.unavailable_notice().expect("notice");
            assert!(notice.contains(database.label()));
            for module in [
                "row-level security",
                "PgAuditSink",
                "PostgresSessionStore",
                "PgMembershipStore",
            ] {
                assert!(notice.contains(module), "{module}");
            }
        }
        assert!(Database::Disabled.unavailable_notice().is_none());
    }

    #[test]
    fn every_placeholder_is_rendered() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let root = create_application(parent.path(), &options("sample-app"))
            .expect("generation should succeed");

        for (path, content) in text_snapshot(&root) {
            assert!(
                !content.contains("%%"),
                "{path} contains an unrendered placeholder"
            );
        }
    }

    #[test]
    fn forge_path_is_written_as_a_path_dependency() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let options = NewApplication {
            forge: ForgeSource::Path("../forge/crates/forge".to_owned()),
            ..options("sample-app")
        };

        let root = create_application(parent.path(), &options).expect("generation should succeed");

        let manifest = read(&root, "Cargo.toml");
        assert!(
            manifest
                .contains("forge = { path = \"../forge/crates/forge\", features = [\"views\"] }")
        );
        assert!(manifest.contains(
            "forge-views = { path = \"../forge/crates/forge-views\", default-features = false, features = [\"build\"] }"
        ));
    }

    #[test]
    fn forge_views_build_dependency_is_the_sibling_of_the_forge_crate() {
        for (forge, views) in [
            ("../forge/crates/forge", "../forge/crates/forge-views"),
            (
                "/src/forge-rust/crates/forge/",
                "/src/forge-rust/crates/forge-views",
            ),
            (".forge/crates/forge", ".forge/crates/forge-views"),
            ("forge", "forge-views"),
        ] {
            let dependency = forge_views_build_dependency(&ForgeSource::Path(forge.to_owned()))
                .expect("valid path");
            assert_eq!(
                dependency,
                format!(
                    "{{ path = \"{views}\", default-features = false, features = [\"build\"] }}"
                ),
                "{forge}"
            );
        }
        assert!(forge_views_build_dependency(&ForgeSource::Path("a\"b".to_owned())).is_err());
    }

    /// Files that only full-stack applications have.
    const VIEW_FILES: &[&str] = &[
        "src/adapters/http/views/mod.rs",
        "src/adapters/http/views/welcome.rs",
        "templates/layouts/application.html",
        "templates/layouts/_flash.html",
        "templates/welcome/index.html",
        "assets/application.css",
        "assets/application.js",
        "assets/forge-rust-logo.png",
        "tests/http.rs",
    ];

    #[test]
    fn full_stack_is_the_default_with_views_assets_and_html_layers() {
        for database in [Database::Postgresql, Database::Sqlite3, Database::Disabled] {
            let (_parent, root) = generate(database, true);
            for file in VIEW_FILES {
                assert!(root.join(file).is_file(), "{database:?}: {file}");
            }
            let manifest = read(&root, "Cargo.toml");
            assert!(
                manifest.contains("askama = { version = \"0.16.1\""),
                "{database:?}"
            );
            assert!(manifest.contains("[build-dependencies]\n"), "{database:?}");
            assert!(
                manifest
                    .contains("tokio = { version = \"1.53.1\", features = [\"macros\", \"rt\"] }")
            );
            assert!(manifest.contains("\"views\"]"), "{database:?}");

            let build = read(&root, "build.rs");
            assert!(build.contains("embed_assets_with_crate_path(\"assets\", \"forge::views\")"));
            assert_eq!(
                build.contains("rerun-if-changed=migrations"),
                database.is_enabled()
            );

            let bootstrap = read(&root, "src/bootstrap/mod.rs");
            assert!(bootstrap.contains(".compose(compose)"));
            assert!(bootstrap.contains(
                "let site = crate::adapters::http::site(composition.router())?;\n    \
                 crate::adapters::http::routes(composition.router(), &site)?;"
            ));
            assert!(!bootstrap.contains(".routes("));
            assert_eq!(
                manifest.contains("features = [\"views\", \"testing\"]")
                    || manifest.contains("\"views\", \"testing\"] }"),
                database.is_enabled(),
                "{database:?}: TestDatabase dev-dependency"
            );

            let views = read(&root, "src/adapters/http/views/mod.rs");
            for needle in [
                "vendor::HOTWIRE_ASSETS",
                "CsrfProtection::new()",
                "FlashLayer::new()",
                ".with_import_map(&import_map)",
                "include!(concat!(env!(\"OUT_DIR\"), \"/forge_assets.rs\"))",
            ] {
                assert!(views.contains(needle), "{needle}");
            }
            let layout = read(&root, "templates/layouts/application.html");
            assert!(layout.contains("{{ csrf_meta|safe }}"));
            assert!(layout.contains("{{ layout.import_map|safe }}"));
            assert!(layout.contains("<script type=\"module\" src=\"{{ layout.javascript }}\">"));
            assert!(
                !layout.contains("<script>"),
                "no inline script but the import map"
            );
            assert!(!layout.contains("style="), "no inline style attributes");
            assert!(!layout.contains("http"), "no CDN");
            assert!(
                read(&root, "templates/welcome/index.html")
                    .contains("{% extends \"layouts/application.html\" %}")
            );

            let architecture = read(&root, "tests/architecture.rs");
            assert!(architecture.contains("\"askama\""));
            assert!(architecture.contains("\"forge::views\""));
            assert!(read(&root, "README.md").contains("## Views"));
        }
    }

    #[test]
    fn api_generates_the_json_only_shape() {
        for database in [Database::Postgresql, Database::Disabled] {
            let parent = tempfile::tempdir().expect("temporary directory should be available");
            let options = NewApplication {
                database,
                views: false,
                forge: ForgeSource::Path("../forge/crates/forge".to_owned()),
                ..options("sample-api")
            };
            let root = create_application(parent.path(), &options).expect("generation succeeds");

            for file in VIEW_FILES {
                assert!(!root.join(file).exists(), "{database:?}: {file}");
            }
            assert!(!root.join("templates").exists());
            assert!(!root.join("assets").exists());
            let manifest = read(&root, "Cargo.toml");
            for needle in [
                "askama",
                "forge-views",
                "views",
                "[build-dependencies]",
                "[dev-dependencies]",
            ] {
                assert!(!manifest.contains(needle), "{database:?}: {needle}");
            }
            assert!(
                read(&root, "src/bootstrap/mod.rs")
                    .contains(".routes(crate::adapters::http::routes)")
            );
            assert!(read(&root, "src/adapters/http/mod.rs").contains("Response::json"));
            assert!(!read(&root, "README.md").contains("## Views"));
            assert_eq!(root.join("build.rs").exists(), database.is_enabled());
        }
    }

    #[test]
    fn session_csrf_check_reads_the_form_field_only_in_full_stack_applications() {
        let (_parent, full_stack) = generate(Database::Postgresql, false);
        let security = read(&full_stack, "src/infrastructure/security/mod.rs");
        assert!(security.contains("forge::views::presented_csrf_token(request)"));

        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let api = create_application(
            parent.path(),
            &NewApplication {
                views: false,
                ..options("sample-api")
            },
        )
        .expect("generation succeeds");
        let security = read(&api, "src/infrastructure/security/mod.rs");
        assert!(!security.contains("forge::views"));
        assert!(security.contains("get_all(CSRF_HEADER_NAME)"));
    }

    #[test]
    fn forge_path_cannot_inject_toml() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        for path in ["", "x\" }\nevil = { path = \"/", "C:\\forge", "a\u{0}b"] {
            let options = NewApplication {
                forge: ForgeSource::Path(path.to_owned()),
                ..options("sample-app")
            };

            let error = create_application(parent.path(), &options)
                .expect_err("unsafe path must be rejected");

            assert!(matches!(error, CliError::InvalidForgePath { .. }));
            assert!(!parent.path().join("sample-app").exists());
        }
    }

    #[test]
    fn refuses_to_overwrite_an_existing_destination() {
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let existing = parent.path().join("sample-app");
        fs::create_dir(&existing).expect("test directory should be created");
        fs::write(existing.join("keep.txt"), "user data").expect("fixture should be written");

        let error = create_application(parent.path(), &options("sample-app"))
            .expect_err("an existing destination must be rejected");

        assert!(matches!(error, CliError::DestinationExists(path) if path == existing));
        assert_eq!(read(&existing, "keep.txt"), "user data");
    }

    /// `examples/hello-forge` is the committed database-free, full-stack output
    /// of `forge new hello-forge --skip-ci -d none --forge-path ../../crates/forge`.
    /// Regenerate it with `FORGE_BLESS=1 cargo test -p forge-cli`.
    #[test]
    fn hello_forge_example_matches_generator_output() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-forge");
        let parent = tempfile::tempdir().expect("temporary directory should be available");
        let options = NewApplication {
            ci: false,
            database: Database::Disabled,
            forge: ForgeSource::Path("../../crates/forge".to_owned()),
            ..options("hello-forge")
        };
        let generated = create_application(parent.path(), &options).expect("generation succeeds");
        let expected = snapshot(&generated);

        if std::env::var_os("FORGE_BLESS").is_some() {
            if example.exists() {
                for relative in snapshot(&example).keys() {
                    fs::remove_file(example.join(relative)).expect("stale file is removable");
                }
            }
            for (relative, content) in &expected {
                write_file(&example.join(relative), content).expect("example is writable");
            }
            return;
        }

        let actual = snapshot(&example);
        let drift: Vec<&str> = expected
            .keys()
            .chain(actual.keys())
            .filter(|path| expected.get(*path) != actual.get(*path))
            .map(String::as_str)
            .collect();
        assert!(
            drift.is_empty(),
            "examples/hello-forge differs from generator output in {drift:?}; \
             run `FORGE_BLESS=1 cargo test -p forge-cli` and review the diff"
        );
    }
}
