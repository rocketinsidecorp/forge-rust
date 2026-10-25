//! The database's own command-line client: `forge db console` opens `psql`,
//! `mysql` or `sqlite3`, and `forge db reset` runs `psql` to restore the
//! privileges of `docker/postgres/init.sql`.
//!
//! This is the one place the CLI reads a database URL (every other lifecycle
//! command runs the application, which reads it itself). The URL is read from
//! the process environment only: `FORGE_DATABASE_URL` names the database,
//! and `FORGE_MIGRATION_DATABASE_URL`, when set, supplies the credentials
//! (the migration role, as the application's `db` commands use), like
//! Forge-Go's `forge db console`.
//!
//! The password never reaches an argument list, a log or the terminal:
//! PostgreSQL gets it in `PGPASSWORD`, MySQL/MariaDB in an option file
//! (mode 0600, in a directory of mode 0700) that is removed when the client
//! exits, SQLite has none. Every inherited `PG*` variable is dropped first, so
//! a service file, an options string or another password cannot send the
//! client anywhere the URL does not say; MySQL's login-path file and
//! `MYSQL_PWD` are kept out the same way.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use forge_config::database_url::{self, ClientTarget, Server};
use forge_config::{DatabaseScheme, SecretString};

use crate::CliError;
use crate::process::{OwnedChild, Shutdown, Signals};

/// The script `forge new` writes for the Compose database, relative to the
/// application root.
pub(crate) const INIT_SCRIPT: &str = "docker/postgres/init.sql";

/// How long a client gets to exit after it was asked to.
const CLIENT_GRACE: Duration = Duration::from_secs(5);

/// The application's own settings (`FORGE_*`: its database URLs, keys and
/// passwords) do not reach a client: its environment is the user's to inspect
/// (`\!` in psql, `system` in mysql).
const APPLICATION_PREFIX: &str = "FORGE_";

/// The database name of the URL, and the credentials of the role that
/// administers it.
enum Connection {
    Server {
        scheme: DatabaseScheme,
        database: String,
        admin: Box<ClientTarget>,
    },
    File(PathBuf),
}

/// What building an [`Invocation`] reads from the machine, injectable for
/// tests.
pub(crate) struct Sources<'a> {
    /// The process environment.
    pub(crate) environment: &'a dyn Fn(&str) -> Option<String>,
    /// Finds a tool on `PATH`.
    pub(crate) locate: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// Whether the `mysql` client at that path is MariaDB's (their TLS options
    /// differ).
    pub(crate) mariadb: &'a dyn Fn(&Path) -> bool,
    /// The application directory: relative SQLite paths and the client's
    /// working directory.
    pub(crate) root: &'a Path,
}

/// A client ready to run: its program, arguments, environment and the
/// credential file that must outlive it. Dropping it removes the file.
pub(crate) struct Invocation {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    directory: PathBuf,
    /// Environment variable names starting with these are not inherited.
    dropped_prefixes: &'static [&'static str],
    /// Environment variables that are not inherited.
    dropped: Vec<String>,
    /// Variables set for the client; values can be secrets.
    set: Vec<(String, String)>,
    /// Holds the option file until the client has exited.
    _files: Option<OptionFiles>,
}

impl std::fmt::Debug for Invocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The environment and the option file can hold the password.
        formatter
            .debug_struct("Invocation")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl Invocation {
    /// The environment the client runs with: the inherited variables `base`
    /// minus the ones that could redirect it, plus the connection.
    pub(crate) fn environment(
        &self,
        base: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        let mut environment: Vec<(OsString, OsString)> = base
            .into_iter()
            .filter(|(name, _)| {
                let name = name.to_string_lossy();
                !self
                    .dropped_prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                    && !self.dropped.iter().any(|dropped| *dropped == name)
            })
            .collect();
        environment.extend(
            self.set
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        environment
    }

    /// The working directory of the client.
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// The option file's path, for tests.
    #[cfg(test)]
    pub(crate) fn option_file(&self) -> Option<PathBuf> {
        self._files.as_ref().map(|files| files.file.clone())
    }
}

/// A private directory holding the MySQL option file; removed on drop.
struct OptionFiles {
    directory: PathBuf,
    file: PathBuf,
}

impl OptionFiles {
    fn create(content: &str) -> Result<Self, CliError> {
        let mut random = [0u8; 12];
        getrandom::fill(&mut random).map_err(|error| {
            CliError::Usage(format!("could not create the option file: {error}"))
        })?;
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("forge-mysql-{name}"));
        let file = directory.join("client.cnf");
        let failed = |action: &str, error: std::io::Error| {
            CliError::Usage(format!("could not {action} for the MySQL client: {error}"))
        };
        create_private_directory(&directory)
            .map_err(|error| failed("create a directory", error))?;
        // From here on the directory exists and `files` removes it.
        let files = Self { directory, file };
        write_private_file(&files.file, content.as_bytes())
            .map_err(|error| failed("write the option file", error))?;
        Ok(files)
    }
}

impl Drop for OptionFiles {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        builder.mode(0o700);
    }
    builder.create(path)
}

fn write_private_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(content)?;
    file.flush()
}

/// The executable named `tool` in the directories of `path` (a `PATH` value).
pub(crate) fn locate(tool: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|directory| directory.join(tool))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// Whether the `mysql` client at `path` is MariaDB's, by its version banner.
pub(crate) fn is_mariadb(path: &Path) -> bool {
    Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("mariadb")
        })
}

/// A failure that mentions neither the URL nor its parts.
fn usage(message: impl Into<String>) -> CliError {
    CliError::Usage(message.into())
}

/// Reads the connection from the environment. With `migration_required` the
/// credentials must come from `FORGE_MIGRATION_DATABASE_URL`; otherwise it
/// falls back to the application's own URL.
fn connection(sources: &Sources<'_>, migration_required: bool) -> Result<Connection, CliError> {
    let read = |name: &str| (sources.environment)(name).filter(|value| !value.is_empty());
    let url = read("FORGE_DATABASE_URL")
        .map(SecretString::new)
        .ok_or_else(|| usage("database is not configured: set FORGE_DATABASE_URL"))?;
    let scheme = DatabaseScheme::of_url(url.expose()).ok_or_else(|| {
        usage(
            "FORGE_DATABASE_URL must start with postgres://, postgresql://, mysql://, \
             mariadb:// or sqlite:",
        )
    })?;
    if scheme == DatabaseScheme::Sqlite {
        let file = database_url::sqlite_file(&url).unwrap_or_default();
        if file.is_empty() || file.starts_with(":memory:") {
            return Err(usage(
                "FORGE_DATABASE_URL names no SQLite file: there is nothing for a client to open",
            ));
        }
        let path = Path::new(&file);
        return Ok(Connection::File(if path.is_absolute() {
            path.to_owned()
        } else {
            sources.root.join(path)
        }));
    }
    // Where the application's own SQLx connects: what the URL leaves out
    // (host, port, user, password, TLS mode) comes from the PG* variables.
    let process = |name: &str| (sources.environment)(name);
    let target = |url: &SecretString| {
        database_url::client_target(url, &process).map(|target| {
            if scheme == DatabaseScheme::Postgres {
                with_pg_defaults(target, &process)
            } else {
                target
            }
        })
    };
    let app = target(&url).ok_or_else(|| usage(database_url::MALFORMED))?;
    let database = app
        .database
        .clone()
        .ok_or_else(|| usage("the database URL does not name a database"))?;
    database_url::validate_name(&database).map_err(usage)?;
    let admin = match read("FORGE_MIGRATION_DATABASE_URL") {
        Some(raw) => {
            let privileged = SecretString::new(raw);
            if DatabaseScheme::of_url(privileged.expose()) != Some(scheme) {
                return Err(usage(
                    "FORGE_MIGRATION_DATABASE_URL selects another engine than FORGE_DATABASE_URL",
                ));
            }
            target(&privileged).ok_or_else(|| {
                usage(format!(
                    "FORGE_MIGRATION_DATABASE_URL: {}",
                    database_url::MALFORMED
                ))
            })?
        }
        None if migration_required => {
            return Err(usage(
                "FORGE_MIGRATION_DATABASE_URL is required: the privileges are restored with the \
                 migration role",
            ));
        }
        None => app.clone(),
    };
    let database = if migration_required {
        // The application's `db create` and `db drop` act on the database of
        // the migration URL: that is the one whose privileges are restored,
        // and it must be the one the application runs on.
        let created = admin
            .database
            .clone()
            .ok_or_else(|| usage("FORGE_MIGRATION_DATABASE_URL does not name a database"))?;
        database_url::validate_name(&created).map_err(usage)?;
        if created != database {
            return Err(usage(
                "FORGE_DATABASE_URL and FORGE_MIGRATION_DATABASE_URL name different databases; \
                 `forge db reset` would restore the privileges of the wrong one",
            ));
        }
        created
    } else {
        database
    };
    Ok(Connection::Server {
        scheme,
        database,
        admin: Box::new(admin),
    })
}

/// What a PostgreSQL URL leaves out comes from `PGUSER`, `PGPASSWORD` and
/// `PGSSLMODE`, as SQLx reads them (host and port are resolved by
/// [`database_url::server`] already).
fn with_pg_defaults(
    mut target: ClientTarget,
    environment: &dyn Fn(&str) -> Option<String>,
) -> ClientTarget {
    let read = |name: &str| environment(name).filter(|value| !value.is_empty());
    if target.user.is_none() {
        target.user = read("PGUSER");
    }
    if target.password.is_none() {
        target.password = read("PGPASSWORD").map(SecretString::new);
    }
    if target.tls.mode.is_none() {
        target.tls.mode = read("PGSSLMODE");
    }
    target
}

fn missing_tool(tool: &str, engine: &str) -> CliError {
    usage(format!(
        "{tool} was not found on PATH: install the {engine} client tools"
    ))
}

/// `forge db console [-- CLIENT-ARGUMENTS]`: the database's own client, with
/// `extra` after the connection options (for example `-c "select 1"`).
pub(crate) fn console(sources: &Sources<'_>, extra: &[String]) -> Result<Invocation, CliError> {
    match connection(sources, false)? {
        Connection::File(path) => {
            let program =
                (sources.locate)("sqlite3").ok_or_else(|| missing_tool("sqlite3", "sqlite"))?;
            // A file name that starts with a dash would be read as an option.
            let mut file = path.into_os_string();
            if file.to_string_lossy().starts_with('-') {
                file = Path::new(".").join(&file).into_os_string();
            }
            let mut args = vec![file];
            args.extend(extra.iter().map(OsString::from));
            Ok(Invocation {
                program,
                args,
                directory: sources.root.to_owned(),
                dropped_prefixes: &[APPLICATION_PREFIX],
                dropped: Vec::new(),
                set: Vec::new(),
                _files: None,
            })
        }
        Connection::Server {
            scheme: DatabaseScheme::Postgres,
            database,
            admin,
        } => {
            let program =
                (sources.locate)("psql").ok_or_else(|| missing_tool("psql", "postgres"))?;
            let mut args = vec![OsString::from("-X")];
            args.extend(extra.iter().map(OsString::from));
            postgres(sources, program, args, &database, &admin)
        }
        Connection::Server {
            scheme: _,
            database,
            admin,
        } => {
            let program =
                (sources.locate)("mysql").ok_or_else(|| missing_tool("mysql", "mysql"))?;
            let files = OptionFiles::create(&mysql_option_file(
                &admin,
                &database,
                (sources.mariadb)(&program),
            )?)?;
            // The option file must be the first argument.
            let mut args = vec![OsString::from(format!(
                "--defaults-file={}",
                files.file.display()
            ))];
            args.extend(extra.iter().map(OsString::from));
            let no_login_path = files.directory.join("no-login-path.cnf");
            Ok(Invocation {
                program,
                args,
                directory: sources.root.to_owned(),
                dropped_prefixes: &[APPLICATION_PREFIX],
                // MySQL's clients read ~/.mylogin.cnf even with
                // --defaults-file; a name that never exists keeps it out.
                dropped: [
                    "MYSQL_PWD",
                    "MYSQL_TEST_LOGIN_FILE",
                    "LIBMYSQL_ENABLE_CLEARTEXT_PLUGIN",
                ]
                .map(str::to_owned)
                .to_vec(),
                set: vec![(
                    "MYSQL_TEST_LOGIN_FILE".to_owned(),
                    no_login_path.to_string_lossy().into_owned(),
                )],
                _files: Some(files),
            })
        }
    }
}

/// `psql` reading a script from standard input and stopping at the first
/// error, in one transaction: how the privileges of `docker/postgres/init.sql`
/// are restored. Needs PostgreSQL and `FORGE_MIGRATION_DATABASE_URL`. Returns
/// the invocation and the name of the application's database.
pub(crate) fn psql_script(sources: &Sources<'_>) -> Result<(Invocation, String), CliError> {
    let Connection::Server {
        scheme: DatabaseScheme::Postgres,
        database,
        admin,
    } = connection(sources, true)?
    else {
        return Err(usage("the privilege bootstrap is for PostgreSQL databases"));
    };
    let program = (sources.locate)("psql").ok_or_else(|| missing_tool("psql", "postgres"))?;
    // `-w`: never prompt for a password, which would hang a reset.
    let args = ["-X", "-q", "-w", "-v", "ON_ERROR_STOP=1", "-1", "-f", "-"]
        .map(OsString::from)
        .to_vec();
    let invocation = postgres(sources, program, args, &database, &admin)?;
    Ok((invocation, database))
}

fn postgres(
    sources: &Sources<'_>,
    program: PathBuf,
    args: Vec<OsString>,
    database: &str,
    admin: &ClientTarget,
) -> Result<Invocation, CliError> {
    let mut set = vec![
        ("PGAPPNAME".to_owned(), "forge-cli".to_owned()),
        ("PGDATABASE".to_owned(), database.to_owned()),
    ];
    match &admin.server {
        Server::Tcp { host, port } => {
            set.push(("PGHOST".to_owned(), host.clone()));
            set.push(("PGPORT".to_owned(), port.to_string()));
        }
        Server::Socket { path, port } => {
            if let Some(path) = path {
                set.push(("PGHOST".to_owned(), path.clone()));
            }
            if let Some(port) = port {
                set.push(("PGPORT".to_owned(), port.to_string()));
            }
        }
        Server::Default { port } => set.push(("PGPORT".to_owned(), port.to_string())),
    }
    if let Some(user) = &admin.user {
        set.push(("PGUSER".to_owned(), user.clone()));
    }
    if let Some(password) = &admin.password {
        set.push(("PGPASSWORD".to_owned(), password.expose().to_owned()));
    }
    for (name, value) in [
        ("PGSSLMODE", &admin.tls.mode),
        ("PGSSLROOTCERT", &admin.tls.root_certificate),
        ("PGSSLCERT", &admin.tls.certificate),
        ("PGSSLKEY", &admin.tls.key),
    ] {
        if let Some(value) = value {
            set.push((name.to_owned(), value.clone()));
        }
    }
    // A NUL cannot be in an environment variable; refuse it by name.
    if let Some((name, _)) = set.iter().find(|(_, value)| value.contains('\0')) {
        return Err(usage(format!(
            "the database URL holds a NUL character that {name} cannot carry"
        )));
    }
    Ok(Invocation {
        program,
        args,
        directory: sources.root.to_owned(),
        dropped_prefixes: &["PG", APPLICATION_PREFIX],
        dropped: Vec::new(),
        set,
        _files: None,
    })
}

/// The `[client]` option file of the MySQL tools, so the password is never an
/// argument. `mariadb` selects MariaDB's spelling of the TLS options.
fn mysql_option_file(
    admin: &ClientTarget,
    database: &str,
    mariadb: bool,
) -> Result<String, CliError> {
    let mut out = String::from("[client]\n");
    let mut write = |key: &str, value: &str| -> Result<(), CliError> {
        if value.contains(['\n', '\r', '\0']) {
            return Err(usage(
                "the database URL holds a control character the MySQL option file cannot carry",
            ));
        }
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!("{key}=\"{escaped}\"\n"));
        Ok(())
    };
    if let Some(user) = &admin.user {
        write("user", user)?;
    }
    if let Some(password) = &admin.password {
        write("password", password.expose())?;
    }
    match &admin.server {
        Server::Tcp { host, port } => {
            write("host", host)?;
            write("port", &port.to_string())?;
        }
        Server::Socket {
            path: Some(path), ..
        } => write("socket", path)?,
        Server::Socket { path: None, .. } | Server::Default { .. } => {}
    }
    write("database", database)?;
    if matches!(admin.server, Server::Tcp { .. }) {
        // Never a socket, which would ignore host and port.
        out.push_str("protocol=tcp\n");
    }
    let mode = admin
        .tls
        .mode
        .as_deref()
        .map_or_else(|| "preferred".to_owned(), str::to_ascii_lowercase)
        .replace('-', "_");
    let tls = match (mariadb, mode.as_str()) {
        (false, "disabled") => "ssl-mode=DISABLED\n",
        (false, "preferred") => "ssl-mode=PREFERRED\n",
        (false, "required") => "ssl-mode=REQUIRED\n",
        (false, "verify_ca") => "ssl-mode=VERIFY_CA\n",
        (false, "verify_identity") => "ssl-mode=VERIFY_IDENTITY\n",
        (true, "disabled") => "ssl=0\n",
        (true, "preferred") => "",
        (true, "required") => "ssl=1\n",
        (true, "verify_ca" | "verify_identity") => "ssl=1\nssl-verify-server-cert=1\n",
        _ => {
            return Err(usage(
                "ssl-mode in the database URL must be disabled, preferred, required, \
                 verify_ca or verify_identity",
            ));
        }
    };
    out.push_str(tls);
    let mut write = |key: &str, value: &Option<String>| -> Result<(), CliError> {
        if let Some(value) = value {
            if value.contains(['\n', '\r', '\0']) {
                return Err(usage(
                    "the database URL holds a control character the MySQL option file cannot carry",
                ));
            }
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!("{key}=\"{escaped}\"\n"));
        }
        Ok(())
    };
    write("ssl-ca", &admin.tls.root_certificate)?;
    write("ssl-cert", &admin.tls.certificate)?;
    write("ssl-key", &admin.tls.key)?;
    Ok(out)
}

/// What a client reads from its standard input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Input<'a> {
    /// The terminal: an interactive session.
    Terminal,
    /// A script, then end of input.
    Script(&'a str),
}

/// Runs clients; the real one starts processes, tests record.
pub(crate) trait Client {
    /// Finds a tool on `PATH`.
    fn locate(&self, tool: &str) -> Option<PathBuf>;
    /// Whether the `mysql` client at `path` is MariaDB's.
    fn is_mariadb(&self, path: &Path) -> bool;
    /// Runs the client and returns its exit code (`None`: ended by a signal).
    fn run(&mut self, invocation: &Invocation, input: Input<'_>) -> Result<Option<i32>, CliError>;
}

/// The machine's `PATH` and processes.
pub(crate) struct System;

impl Client for System {
    fn locate(&self, tool: &str) -> Option<PathBuf> {
        locate(tool, std::env::var_os("PATH").as_deref())
    }

    fn is_mariadb(&self, path: &Path) -> bool {
        is_mariadb(path)
    }

    fn run(&mut self, invocation: &Invocation, input: Input<'_>) -> Result<Option<i32>, CliError> {
        let shutdown = Shutdown::new();
        // Registered before the client exists, so no signal can end the CLI
        // between writing the password and removing it.
        let _signals = match input {
            Input::Terminal => Signals::install_for_client(shutdown.clone())?,
            Input::Script(_) => Signals::install_for(shutdown.clone())?,
        };
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .current_dir(invocation.directory())
            .env_clear()
            .envs(invocation.environment(std::env::vars_os()))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        match input {
            Input::Terminal => command.stdin(Stdio::inherit()),
            Input::Script(_) => command.stdin(Stdio::piped()),
        };
        // Not in its own group: the terminal's Ctrl-C must reach the client.
        let mut child = OwnedChild::spawn(command, false)?;
        if let (Input::Script(script), Some(mut stdin)) = (input, child.take_stdin()) {
            // A client that stopped reading is reported by its exit code.
            let _ = stdin.write_all(script.as_bytes());
        }
        let program = invocation.program.display().to_string();
        let (status, interrupted) = child
            .wait(&shutdown, CLIENT_GRACE)
            .map_err(|source| CliError::SpawnProcess { program, source })?;
        if interrupted {
            return Err(CliError::Interrupted);
        }
        Ok(status.code())
    }
}

/// The SQL a project's `docker/postgres/init.sql` applies inside the
/// database: everything after its `\connect NAME` line. The roles and the
/// database itself are created before that line; they survive a drop, the
/// grants inside the database do not.
///
/// # Errors
///
/// A reason (never SQL or a URL) when the file names another database than
/// `database`, has no `\connect` line, or the part after it contains a
/// backslash or a colon that is not half of a `::` cast. `psql` runs a
/// backslash command anywhere on a line (`SELECT 1; \! cmd`, `\o FILE`,
/// `\gexec`, `\copy`) and interpolates `:name`, `:'name'` and `:"name"`, and
/// only plain SQL is meant to reach it. The shipped script has neither.
pub(crate) fn bootstrap_sql(init: &str, database: &str) -> Result<String, String> {
    let mut lines = init.lines();
    let connected = lines.by_ref().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("\\connect")
            .map(|rest| rest.trim().to_owned())
    });
    let Some(name) = connected else {
        return Err(
            "it has no \\connect line, so there is nothing to apply to a new database".to_owned(),
        );
    };
    let name = name.trim_matches('"');
    if name != database {
        return Err(format!(
            "it is for the database \"{name}\", but FORGE_DATABASE_URL names \"{database}\""
        ));
    }
    let mut sql = String::new();
    for line in lines {
        if line.contains('\\') {
            return Err(
                "it has a backslash after \\connect (a psql command); only plain SQL is applied"
                    .to_owned(),
            );
        }
        if has_variable_reference(line) {
            return Err(
                "it has a colon after \\connect that is not part of `::` (a psql variable); \
                 only plain SQL is applied"
                    .to_owned(),
            );
        }
        sql.push_str(line);
        sql.push('\n');
    }
    Ok(sql)
}

/// Whether a line has a `:` that is not one half of a `::` pair, which psql
/// may read as a variable reference. Quotes are not parsed: a colon in a
/// string literal is refused too.
fn has_variable_reference(line: &str) -> bool {
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character == ':' {
            if characters.peek() == Some(&':') {
                characters.next();
            } else {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const PASSWORD: &str = "s3cr3t-p4ss";

    struct Machine {
        environment: HashMap<&'static str, String>,
        tools: Vec<&'static str>,
        mariadb: bool,
    }

    impl Machine {
        fn new(url: &str) -> Self {
            Self {
                environment: HashMap::from([("FORGE_DATABASE_URL", url.to_owned())]),
                tools: vec!["psql", "mysql", "sqlite3"],
                mariadb: false,
            }
        }

        fn with(mut self, name: &'static str, value: &str) -> Self {
            self.environment.insert(name, value.to_owned());
            self
        }

        fn without_tools(mut self) -> Self {
            self.tools.clear();
            self
        }

        fn console(&self, extra: &[&str]) -> Result<Invocation, CliError> {
            let extra: Vec<String> = extra.iter().map(|text| (*text).to_owned()).collect();
            self.with_sources(|sources| console(sources, &extra))
        }

        fn script(&self) -> Result<Invocation, CliError> {
            self.with_sources(psql_script)
                .map(|(invocation, _database)| invocation)
        }

        fn with_sources<T>(&self, run: impl FnOnce(&Sources<'_>) -> T) -> T {
            let environment = |name: &str| self.environment.get(name).cloned();
            let locate = |tool: &str| {
                self.tools
                    .contains(&tool)
                    .then(|| PathBuf::from(format!("/usr/bin/{tool}")))
            };
            let mariadb = |_: &Path| self.mariadb;
            run(&Sources {
                environment: &environment,
                locate: &locate,
                mariadb: &mariadb,
                root: Path::new("/work/shop"),
            })
        }
    }

    fn args(invocation: &Invocation) -> Vec<String> {
        invocation
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn base() -> Vec<(OsString, OsString)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/dev"),
            ("PGSERVICE", "other"),
            ("PGPASSWORD", "inherited"),
            ("PGHOST", "elsewhere.example"),
            ("PGOPTIONS", "-c search_path=evil"),
            ("MYSQL_PWD", "inherited"),
            ("FORGE_DATABASE_URL", "postgres://u:inherited@h/d"),
            ("FORGE_MIGRATION_DATABASE_URL", "postgres://u:inherited@h/d"),
            ("FORGE_ENV", "development"),
            ("FORGE_SECRET_KEY", "application-secret"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .to_vec()
    }

    fn env_of(invocation: &Invocation) -> HashMap<String, String> {
        invocation
            .environment(base())
            .into_iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn psql_gets_the_connection_from_the_environment_and_never_the_password_in_arguments() {
        let url = format!("postgres://app%40corp:{PASSWORD}@db.internal:5433/shop?sslmode=require");
        let invocation = Machine::new(&url)
            .console(&["-c", "select 1"])
            .expect("console");
        assert_eq!(invocation.program, PathBuf::from("/usr/bin/psql"));
        assert_eq!(args(&invocation), ["-X", "-c", "select 1"]);
        for argument in args(&invocation) {
            assert!(!argument.contains(PASSWORD), "{argument}");
        }
        assert!(
            !format!("{invocation:?}").contains(PASSWORD),
            "Debug is redacted"
        );
        let env = env_of(&invocation);
        assert_eq!(env["PGHOST"], "db.internal");
        assert_eq!(env["PGPORT"], "5433");
        assert_eq!(env["PGDATABASE"], "shop");
        assert_eq!(env["PGUSER"], "app@corp");
        assert_eq!(env["PGPASSWORD"], PASSWORD);
        assert_eq!(env["PGSSLMODE"], "require");
        assert_eq!(env["HOME"], "/home/dev", "ordinary variables are inherited");
    }

    #[test]
    fn inherited_pg_variables_and_the_applications_urls_never_reach_the_client() {
        let invocation = Machine::new("postgres://u:pw@127.0.0.1/shop")
            .console(&[])
            .expect("console");
        let env = env_of(&invocation);
        for name in ["PGSERVICE", "PGOPTIONS"] {
            assert!(!env.contains_key(name), "{name}");
        }
        assert_eq!(env["PGHOST"], "127.0.0.1", "the URL's host, not PGHOST");
        assert_eq!(
            env["PGPASSWORD"], "pw",
            "the URL's password, not PGPASSWORD"
        );
        for name in ["FORGE_DATABASE_URL", "FORGE_MIGRATION_DATABASE_URL"] {
            assert!(!env.contains_key(name), "{name}");
        }
        assert!(
            !env.keys().any(|name| name.starts_with("FORGE_")),
            "no application setting reaches the client: {env:?}"
        );
    }

    #[test]
    fn the_migration_role_supplies_the_credentials_and_the_application_url_the_database() {
        let machine = Machine::new("postgres://app_runtime:rt@127.0.0.1:5432/shop").with(
            "FORGE_MIGRATION_DATABASE_URL",
            "postgres://app_migrator:mg@127.0.0.1:5432/ignored",
        );
        let env = env_of(&machine.console(&[]).expect("console"));
        assert_eq!(env["PGUSER"], "app_migrator");
        assert_eq!(env["PGPASSWORD"], "mg");
        assert_eq!(env["PGDATABASE"], "shop");
    }

    #[test]
    fn sockets_and_the_default_server_reach_psql_like_the_driver_would() {
        let socket = Machine::new("postgres:///shop?host=/var/run/postgresql&port=5433")
            .console(&[])
            .expect("console");
        let env = env_of(&socket);
        assert_eq!(env["PGHOST"], "/var/run/postgresql");
        assert_eq!(env["PGPORT"], "5433");
        let default = Machine::new("postgres:///shop")
            .console(&[])
            .expect("console");
        let env = env_of(&default);
        assert!(!env.contains_key("PGHOST"), "libpq picks its own socket");
        assert_eq!(env["PGPORT"], "5432");
        assert!(!env.contains_key("PGPASSWORD"));
    }

    #[test]
    fn refusals_name_the_problem_and_never_the_url() {
        let secret = format!("postgres://admin:{PASSWORD}@db.internal/shop");
        let cases: Vec<(Machine, &str)> = vec![
            (
                Machine::new(&secret).without_tools(),
                "psql was not found on PATH",
            ),
            (
                Machine::new("mysql://u:p@h/shop").without_tools(),
                "mysql was not found on PATH",
            ),
            (
                Machine::new("sqlite:dev.db").without_tools(),
                "sqlite3 was not found on PATH",
            ),
            (Machine::new("redis://h/x"), "must start with postgres://"),
            (
                Machine::new(&format!("postgres://admin:{PASSWORD}/x@db.internal/shop")),
                "malformed",
            ),
            (Machine::new("postgres://u:p@h/postgres"), "system database"),
            (
                Machine::new("postgres://u:p@h/template1"),
                "system database",
            ),
            (Machine::new("postgres://u:p@h"), "does not name a database"),
            (
                Machine::new(&secret).with("FORGE_MIGRATION_DATABASE_URL", "mysql://u:p@h/shop"),
                "another engine",
            ),
            (Machine::new("sqlite::memory:"), "names no SQLite file"),
            (Machine::new(""), "not configured"),
        ];
        for (machine, expected) in cases {
            let error = machine.console(&[]).expect_err(expected).to_string();
            assert!(error.contains(expected), "{expected}: {error}");
            assert!(
                !error.contains(PASSWORD) && !error.contains("db.internal"),
                "{error}"
            );
        }
        let mut unset = Machine::new("x");
        unset.environment.clear();
        assert!(
            unset
                .console(&[])
                .expect_err("unset")
                .to_string()
                .contains("not configured")
        );
    }

    #[test]
    fn mysql_gets_an_option_file_with_the_password_and_never_an_argument() {
        let url = format!("mysql://root:{PASSWORD}@db.internal:3307/shop?ssl-mode=required");
        let invocation = Machine::new(&url)
            .console(&["-e", "select 1"])
            .expect("console");
        assert_eq!(invocation.program, PathBuf::from("/usr/bin/mysql"));
        let arguments = args(&invocation);
        let file = invocation.option_file().expect("option file");
        assert_eq!(
            arguments,
            [
                &format!("--defaults-file={}", file.display()),
                "-e",
                "select 1"
            ],
            "the option file is the first argument"
        );
        for argument in &arguments {
            assert!(!argument.contains(PASSWORD), "{argument}");
        }
        let content = fs::read_to_string(&file).expect("read");
        assert!(
            content.contains(&format!("password=\"{PASSWORD}\"")),
            "{content}"
        );
        for line in [
            "user=\"root\"",
            "host=\"db.internal\"",
            "port=\"3307\"",
            "database=\"shop\"",
            "protocol=tcp",
            "ssl-mode=REQUIRED",
        ] {
            assert!(
                content.lines().any(|found| found == line),
                "{line}: {content}"
            );
        }
        let env = env_of(&invocation);
        assert!(!env.contains_key("MYSQL_PWD"));
        assert!(env["MYSQL_TEST_LOGIN_FILE"].ends_with("no-login-path.cnf"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mode =
                |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode(&file), 0o600);
            assert_eq!(mode(file.parent().expect("directory")), 0o700);
        }
        let directory = file.parent().expect("directory").to_owned();
        drop(invocation);
        assert!(
            !directory.exists(),
            "the credential file is removed with the invocation"
        );
    }

    #[test]
    fn mysql_options_are_escaped_and_mariadb_spells_tls_its_own_way() {
        let admin = |url: &str| {
            database_url::client_target(&SecretString::new(url), &|_| None).expect("target")
        };
        let content = mysql_option_file(
            &admin("mysql://u:a%22b%5Cc@h/shop?ssl-mode=verify_identity&ssl-ca=/etc/ca.pem"),
            "shop",
            false,
        )
        .expect("renders");
        assert!(content.contains("password=\"a\\\"b\\\\c\""), "{content}");
        assert!(content.contains("ssl-mode=VERIFY_IDENTITY"), "{content}");
        assert!(content.contains("ssl-ca=\"/etc/ca.pem\""), "{content}");
        let maria = mysql_option_file(
            &admin("mariadb://u:p@h/shop?ssl-mode=required"),
            "shop",
            true,
        )
        .expect("renders");
        assert!(
            maria.contains("ssl=1\n") && !maria.contains("ssl-mode"),
            "{maria}"
        );
        let default =
            mysql_option_file(&admin("mysql://u:p@h/shop"), "shop", false).expect("renders");
        assert!(default.contains("ssl-mode=PREFERRED"), "{default}");
        // A newline would start a new option: refused, not escaped.
        let hostile = admin("mysql://u:a%0Aport=1@h/shop");
        assert!(mysql_option_file(&hostile, "shop", false).is_err());
        assert!(
            mysql_option_file(&admin("mysql://u:p@h/shop?ssl-mode=bogus"), "shop", false).is_err()
        );
    }

    #[test]
    fn sqlite_opens_the_file_next_to_the_application_without_options_in_the_name() {
        let relative = Machine::new("sqlite://storage/dev.sqlite3?mode=rwc")
            .console(&[".tables"])
            .expect("console");
        assert_eq!(relative.program, PathBuf::from("/usr/bin/sqlite3"));
        assert_eq!(
            args(&relative),
            ["/work/shop/storage/dev.sqlite3", ".tables"]
        );
        assert_eq!(relative.directory(), Path::new("/work/shop"));
        let absolute = Machine::new("sqlite:///var/lib/dev.db")
            .console(&[])
            .expect("console");
        assert_eq!(args(&absolute), ["/var/lib/dev.db"]);
    }

    #[test]
    fn the_script_client_stops_at_the_first_error_in_one_transaction() {
        let machine = Machine::new("postgres://app_runtime:rt@127.0.0.1/shop").with(
            "FORGE_MIGRATION_DATABASE_URL",
            &format!("postgres://app_migrator:{PASSWORD}@127.0.0.1/shop"),
        );
        let invocation = machine.script().expect("script");
        assert_eq!(
            args(&invocation),
            ["-X", "-q", "-w", "-v", "ON_ERROR_STOP=1", "-1", "-f", "-"]
        );
        let env = env_of(&invocation);
        assert_eq!(env["PGUSER"], "app_migrator");
        assert_eq!(env["PGPASSWORD"], PASSWORD);
        assert!(
            args(&invocation)
                .iter()
                .all(|argument| !argument.contains(PASSWORD))
        );

        // Without the migration URL the privileges cannot be restored, and
        // the application's own URL is not a substitute for it.
        let runtime_only = Machine::new("postgres://app_runtime:rt@127.0.0.1/shop");
        let error = runtime_only.script().expect_err("needs the migration URL");
        assert!(
            error
                .to_string()
                .contains("FORGE_MIGRATION_DATABASE_URL is required"),
            "{error}"
        );
        let error = Machine::new("mysql://u:p@h/shop")
            .with("FORGE_MIGRATION_DATABASE_URL", "mysql://u:p@h/shop")
            .script()
            .expect_err("postgres only");
        assert!(error.to_string().contains("PostgreSQL"), "{error}");
    }

    #[test]
    fn the_bootstrap_is_what_follows_connect_and_only_for_the_applications_database() {
        let init = "CREATE ROLE a;\nCREATE DATABASE shop OWNER a;\n\n\\connect shop\n\n\
                    GRANT CONNECT ON DATABASE shop TO a;\nREVOKE ALL ON SCHEMA public FROM PUBLIC;\n";
        assert_eq!(
            bootstrap_sql(init, "shop").expect("sql"),
            "\nGRANT CONNECT ON DATABASE shop TO a;\nREVOKE ALL ON SCHEMA public FROM PUBLIC;\n"
        );
        for (init, database, expected) in [
            (init, "other", "is for the database \"shop\""),
            ("CREATE ROLE a;\n", "shop", "no \\connect line"),
            ("\\connect shop\n\\! rm -rf x\n", "shop", "backslash"),
            ("\\connect shop\n  \\i other.sql\n", "shop", "backslash"),
        ] {
            let error = bootstrap_sql(init, database).expect_err(expected);
            assert!(error.contains(expected), "{expected}: {error}");
        }
        assert_eq!(
            bootstrap_sql("\\connect \"shop\"\nSELECT 1;\n", "shop").expect("quoted"),
            "SELECT 1;\n"
        );
    }

    #[test]
    fn plain_sql_passes_and_every_psql_escape_is_refused_wherever_it_stands() {
        let accepted =
            "\\connect shop\nSELECT 1::int, '{}'::jsonb;\nGRANT ALL ON SCHEMA public TO a;\n";
        assert!(bootstrap_sql(accepted, "shop").is_ok());
        for hostile in [
            // A backslash command after SQL on the same line, in each spelling.
            "SELECT 1; \\! touch /tmp/pwned",
            "SELECT $$x$$ \\o /tmp/out",
            "SELECT 1 \\gexec",
            "GRANT CONNECT ON DATABASE shop TO a; \\o /tmp/out",
            "SELECT 1; \\copy t to '/tmp/f'",
            "\\! touch /tmp/pwned",
            "-- a comment \\! touch /tmp/pwned",
            "SELECT 'a\\b';",
            // psql variable references.
            "SELECT :name;",
            "SELECT :'name';",
            "SELECT :\"name\";",
            "SELECT 1 :x",
            "SELECT 1:int;",
            "SELECT ':name';",
            "SELECT 1:::int;",
        ] {
            let init = format!("\\connect shop\n{hostile}\n");
            let error = bootstrap_sql(&init, "shop").expect_err(hostile);
            assert!(
                error.contains("backslash") || error.contains("colon"),
                "{hostile}: {error}"
            );
            assert!(
                !error.contains("pwned"),
                "the reason never quotes the SQL: {error}"
            );
        }
        // The text before \connect is never sent, so it is not inspected.
        assert!(bootstrap_sql("\\! echo hi\n\\connect shop\nSELECT 1;\n", "shop").is_ok());
    }

    #[test]
    fn the_restore_connects_where_the_application_does_and_to_one_database() {
        // The URL leaves the port out: SQLx takes PGPORT, and so does psql.
        let machine = Machine::new("postgres://app_runtime:rt@127.0.0.1/shop")
            .with(
                "FORGE_MIGRATION_DATABASE_URL",
                "postgres://app_migrator:mg@127.0.0.1/shop",
            )
            .with("PGPORT", "5433");
        let env = env_of(&machine.script().expect("script"));
        assert_eq!(
            (env["PGHOST"].as_str(), env["PGPORT"].as_str()),
            ("127.0.0.1", "5433")
        );
        // No host in the URL: PGHOSTADDR, then PGHOST (a directory is a socket).
        let machine = Machine::new("postgres:///shop")
            .with("FORGE_MIGRATION_DATABASE_URL", "postgres:///shop")
            .with("PGHOST", "/var/run/postgresql")
            .with("PGUSER", "app_migrator")
            .with("PGPASSWORD", "from-env");
        let invocation = machine.script().expect("script");
        let env = env_of(&invocation);
        assert_eq!(env["PGHOST"], "/var/run/postgresql");
        assert_eq!(env["PGUSER"], "app_migrator");
        assert_eq!(env["PGPASSWORD"], "from-env");
        assert!(
            args(&invocation)
                .iter()
                .all(|argument| !argument.contains("from-env"))
        );
        let machine = Machine::new("postgres:///shop")
            .with("FORGE_MIGRATION_DATABASE_URL", "postgres:///shop")
            .with("PGHOST", "db.internal")
            .with("PGHOSTADDR", "10.0.0.5");
        assert_eq!(
            env_of(&machine.script().expect("script"))["PGHOST"],
            "10.0.0.5"
        );
        // The URL's own values beat the environment.
        let machine = Machine::new("postgres://u:p@own.host:7000/shop")
            .with(
                "FORGE_MIGRATION_DATABASE_URL",
                "postgres://m:p@own.host:7000/shop",
            )
            .with("PGHOST", "elsewhere")
            .with("PGPORT", "1");
        let env = env_of(&machine.script().expect("script"));
        assert_eq!(
            (env["PGHOST"].as_str(), env["PGPORT"].as_str()),
            ("own.host", "7000")
        );
        assert_eq!(env["PGUSER"], "m");

        // The console resolves the server the same way.
        let machine = Machine::new("postgres://u:p@127.0.0.1/shop").with("PGPORT", "5433");
        assert_eq!(
            env_of(&machine.console(&[]).expect("console"))["PGPORT"],
            "5433"
        );

        // The application's `db create` and `db drop` use the migration URL's
        // database: a different one than FORGE_DATABASE_URL's is refused.
        let machine = Machine::new("postgres://u:p@127.0.0.1/shop").with(
            "FORGE_MIGRATION_DATABASE_URL",
            "postgres://m:p@127.0.0.1/shop_admin",
        );
        let error = machine
            .script()
            .expect_err("different databases")
            .to_string();
        assert!(error.contains("name different databases"), "{error}");
        assert!(!error.contains("shop_admin"), "{error}");
        let machine = Machine::new("postgres://u:p@127.0.0.1/shop").with(
            "FORGE_MIGRATION_DATABASE_URL",
            "postgres://m:p@127.0.0.1/postgres",
        );
        assert!(
            machine
                .script()
                .expect_err("system database")
                .to_string()
                .contains("system database")
        );
        let machine = Machine::new("postgres://u:p@127.0.0.1/shop")
            .with("FORGE_MIGRATION_DATABASE_URL", "postgres://m:p@127.0.0.1");
        assert!(
            machine
                .script()
                .expect_err("no database")
                .to_string()
                .contains("does not name a database")
        );
    }

    #[test]
    fn the_shipped_init_script_has_a_bootstrap_for_its_database() {
        let template = include_str!("../templates/app/docker/postgres/init.sql.tmpl");
        let rendered = template.replace("%%DATABASE_NAME%%", "shop");
        let sql = bootstrap_sql(&rendered, "shop").expect("the shipped script parses");
        for statement in [
            "REVOKE CONNECT, TEMPORARY ON DATABASE shop FROM PUBLIC;",
            "GRANT CONNECT ON DATABASE shop TO app_migrator, app_runtime;",
            "REVOKE ALL ON SCHEMA public FROM PUBLIC;",
            "GRANT USAGE ON SCHEMA public TO app_runtime;",
            "ALTER DEFAULT PRIVILEGES FOR ROLE app_migrator IN SCHEMA public",
        ] {
            assert!(sql.contains(statement), "{statement}");
        }
        assert!(!sql.contains("CREATE ROLE") && !sql.contains("CREATE DATABASE"));
    }

    #[test]
    fn executables_are_found_on_the_given_path_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let tool = directory.path().join("psql");
        fs::write(&tool, "#!/bin/sh\n").expect("write");
        let path =
            std::env::join_paths([Path::new("/nonexistent"), directory.path()]).expect("join");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(locate("psql", Some(&path)), None, "not executable yet");
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        assert_eq!(locate("psql", Some(&path)), Some(tool));
        assert_eq!(locate("mysql", Some(&path)), None);
        assert_eq!(locate("psql", None), None);
    }

    /// What the real server says about the privileges of a database: its
    /// owner and ACL, the `public` schema's, and the default privileges.
    const GRANTS: &str = "\
        SELECT 'database', coalesce(datacl::text, '-'), pg_get_userbyid(datdba) \
          FROM pg_database WHERE datname = current_database() \
        UNION ALL SELECT 'schema', coalesce(nspacl::text, '-'), pg_get_userbyid(nspowner) \
          FROM pg_namespace WHERE nspname = 'public' \
        UNION ALL SELECT 'default ' || defaclobjtype::text, defaclacl::text, pg_get_userbyid(defaclrole) \
          FROM pg_default_acl \
        ORDER BY 1, 2, 3;";

    /// Runs `sql` with the real `psql` as `target` in `database`; the output.
    fn psql(target: &ClientTarget, database: &str, sql: &str) -> String {
        use std::io::Write as _;

        let machine = Machine::new("postgres://unused@127.0.0.1/unused");
        let invocation = machine
            .with_sources(|sources| {
                postgres(
                    sources,
                    PathBuf::from("psql"),
                    ["-X", "-A", "-t", "-q", "-v", "ON_ERROR_STOP=1", "-f", "-"]
                        .map(OsString::from)
                        .to_vec(),
                    database,
                    target,
                )
            })
            .expect("invocation");
        let mut child = Command::new("psql")
            .args(&invocation.args)
            .env_clear()
            .envs(invocation.environment(std::env::vars_os()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("psql starts");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(sql.as_bytes())
            .expect("write");
        let output = child.wait_with_output().expect("psql runs");
        assert!(
            output.status.success(),
            "psql failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Drops what a test created, even when it panics.
    struct Cleanup {
        admin: ClientTarget,
        databases: Vec<String>,
        roles: Vec<String>,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let mut sql = String::new();
            for database in &self.databases {
                sql.push_str(&format!(
                    "DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE);\n"
                ));
            }
            for role in &self.roles {
                sql.push_str(&format!("DROP ROLE IF EXISTS \"{role}\";\n"));
            }
            // A failed clean-up must not turn a failing test into an abort.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                psql(&self.admin, "postgres", &sql);
            }));
        }
    }

    /// `forge db reset` must leave the privileges a fresh Compose start leaves
    /// (the shipped init script, run by the container's superuser): the database
    /// is created again by the migration role, as the application's
    /// `db create` does, and the script's part inside the database is applied
    /// by the code under test. The control, a database created and not
    /// restored, must differ, or the comparison proves nothing.
    #[test]
    #[ignore = "needs FORGE_TEST_DATABASE_URL (a superuser) and psql"]
    fn a_reset_database_has_the_privileges_of_a_fresh_bootstrap() {
        let url = std::env::var("FORGE_TEST_DATABASE_URL").expect("FORGE_TEST_DATABASE_URL");
        let admin = database_url::client_target(&SecretString::new(url), &|_| None).expect("admin");
        let Server::Tcp { host, port } = admin.server.clone() else {
            panic!("FORGE_TEST_DATABASE_URL must name a TCP server");
        };
        let mut random = [0u8; 5];
        getrandom::fill(&mut random).expect("random");
        let unique: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let (migrator, runtime) = (format!("fm_{unique}"), format!("fr_{unique}"));
        let (fresh, reset, control) = (
            format!("fa_{unique}"),
            format!("fb_{unique}"),
            format!("fc_{unique}"),
        );
        let mut cleanup = Cleanup {
            admin: admin.clone(),
            databases: Vec::new(),
            roles: vec![migrator.clone(), runtime.clone()],
        };
        let template = include_str!("../templates/app/docker/postgres/init.sql.tmpl")
            .replace("app_migrator", &migrator)
            .replace("app_runtime", &runtime);
        let render = |database: &str| template.replace("%%DATABASE_NAME%%", database);
        let as_migrator = ClientTarget {
            user: Some(migrator.clone()),
            password: Some(SecretString::new(migrator.clone())),
            ..admin.clone()
        };

        // A fresh Compose start: roles, database and grants, as the superuser.
        cleanup.databases.push(fresh.clone());
        psql(&admin, "postgres", &render(&fresh));
        let expected = psql(&admin, &fresh, GRANTS);

        // A reset: the database is created by the migration role (it is the
        // owner, as `<app> db create` makes it) and the roles already exist.
        cleanup.databases.push(reset.clone());
        cleanup.databases.push(control.clone());
        psql(
            &as_migrator,
            "postgres",
            &format!("CREATE DATABASE \"{reset}\";"),
        );
        psql(
            &as_migrator,
            "postgres",
            &format!("CREATE DATABASE \"{control}\";"),
        );
        let urls = |database: &str| {
            let at = format!("@{host}:{port}/{database}");
            HashMap::from([
                (
                    "FORGE_DATABASE_URL",
                    format!("postgres://{runtime}:{runtime}{at}"),
                ),
                (
                    "FORGE_MIGRATION_DATABASE_URL",
                    format!("postgres://{migrator}:{migrator}{at}"),
                ),
            ])
        };
        let environment = urls(&reset);
        let lookup = |name: &str| environment.get(name).cloned();
        let locate = |tool: &str| System.locate(tool);
        let mariadb = |_: &Path| false;
        let (invocation, database) = psql_script(&Sources {
            environment: &lookup,
            locate: &locate,
            mariadb: &mariadb,
            root: Path::new("."),
        })
        .expect("psql_script");
        assert_eq!(database, reset);
        let sql = bootstrap_sql(&render(&reset), &database).expect("bootstrap");
        let code = System
            .run(&invocation, Input::Script(&sql))
            .expect("psql runs");
        assert_eq!(code, Some(0));

        let restored = psql(&admin, &reset, GRANTS);
        assert_eq!(
            restored.replace(&reset, "DB"),
            expected.replace(&fresh, "DB"),
            "the reset database has the privileges of a fresh bootstrap"
        );
        let unrestored = psql(&admin, &control, GRANTS);
        assert_ne!(
            unrestored, expected,
            "a database that was only created lacks them: the comparison discriminates"
        );
        // The runtime role really reads what the migration role creates, and
        // cannot create anything itself.
        psql(
            &as_migrator,
            &reset,
            "CREATE TABLE t (id int); INSERT INTO t VALUES (1);",
        );
        let as_runtime = ClientTarget {
            user: Some(runtime.clone()),
            password: Some(SecretString::new(runtime)),
            ..admin
        };
        assert_eq!(psql(&as_runtime, &reset, "SELECT id FROM t;").trim(), "1");
    }
}
