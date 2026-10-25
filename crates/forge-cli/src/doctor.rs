//! `forge doctor [--json]`: check the toolchain, the `FORGE_*` configuration
//! and that the configured database server answers.
//!
//! The database URL is read only to describe it and to open a TCP connection
//! to its host: the report names the engine, database and host, never a user,
//! a password or a query parameter, and the CLI does not log in (the
//! application does, with its own credentials). The host and port are the
//! ones the driver would use: for PostgreSQL, a URL that leaves them out takes
//! `PGHOSTADDR`, `PGHOST` and `PGPORT` from the environment, and with none of
//! them the default socket if a server listens there, else `localhost`. Like Forge-Go's `forge
//! doctor` it exits with status 1 when anything is wrong.

use std::ffi::OsString;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs as _};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use forge_config::database_url::{self, DEFAULT_POSTGRES_SOCKET_DIRECTORIES, Server};
use forge_config::{AppConfig, DatabaseScheme};
use serde_json::{Value, json};

use crate::CliError;
use crate::lifecycle::ALLOW_DESTRUCTIVE;

/// How long the database server gets to accept a TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// What the checks need from the machine, injectable for tests.
pub(crate) trait Probes {
    /// The first line of `<tool> --version`, or why it cannot be run.
    fn tool_version(&self, tool: &str) -> Result<String, String>;
    /// Whether a TCP connection to the endpoint succeeds, or why not.
    fn connect(&self, host: &str, port: u16) -> Result<(), String>;
    /// Whether a file exists.
    fn file_exists(&self, path: &Path) -> bool;
}

/// The real machine.
pub(crate) struct System;

impl Probes for System {
    fn tool_version(&self, tool: &str) -> Result<String, String> {
        let output = Command::new(tool)
            .arg("--version")
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!("`{tool} --version` failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned())
    }

    fn connect(&self, host: &str, port: u16) -> Result<(), String> {
        let addresses = (host, port)
            .to_socket_addrs()
            .map_err(|_| "the host name does not resolve".to_owned())?;
        let mut last = "no address".to_owned();
        for address in addresses {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(_) => return Ok(()),
                Err(error) => last = error.kind().to_string(),
            }
        }
        Err(last)
    }

    fn file_exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// One finding, as Forge-Go's `forge doctor --json` reports it.
struct Diagnostic {
    code: &'static str,
    severity: &'static str,
    summary: String,
    remediation: String,
}

impl Diagnostic {
    fn error(code: &'static str, summary: &str, remediation: &str) -> Self {
        Self {
            code,
            severity: "error",
            summary: summary.to_owned(),
            remediation: remediation.to_owned(),
        }
    }

    fn json(&self) -> Value {
        json!({
            "code": self.code,
            "severity": self.severity,
            "summary": self.summary,
            "remediation": self.remediation,
        })
    }
}

/// Runs the checks and writes the report to `output`. `variables` is the
/// process environment; `directory` the current directory.
pub(crate) fn run(
    json_output: bool,
    variables: Vec<(OsString, OsString)>,
    directory: &Path,
    probes: &dyn Probes,
    output: &mut dyn Write,
) -> Result<(), CliError> {
    let mut lines: Vec<String> = Vec::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut environment = None;
    let mut database = "none".to_owned();

    // The CLI's own switch is not an application setting, and the
    // application would refuse it as an unknown key.
    let variables = variables
        .into_iter()
        .filter(|(key, _)| key != ALLOW_DESTRUCTIVE)
        .collect::<Vec<_>>();
    // Only the host and port settings of the driver are read from it.
    let process_environment = |name: &str| {
        variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    };
    let process_environment: &dyn Fn(&str) -> Option<String> = &process_environment;
    match AppConfig::from_vars(variables.clone()) {
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                "configuration.invalid",
                "Forge configuration is invalid.",
                "Review the FORGE_* settings against `cargo run -- help`; secret values are intentionally omitted.",
            ));
            lines.push(format!("configuration: invalid ({error})"));
        }
        Ok(config) => {
            lines.push("configuration: ok".to_owned());
            let name = format!("{:?}", config.environment).to_lowercase();
            lines.push(format!("environment: {name}"));
            environment = Some(name);
            lines.push(format!("http address: {}", config.server.bind));
            match config.database.url.as_ref() {
                None => lines.push("database: not configured".to_owned()),
                Some(url) => {
                    database = match DatabaseScheme::of_url(url.expose()) {
                        Some(DatabaseScheme::Postgres) => "postgres",
                        Some(DatabaseScheme::MySql) => "mysql",
                        Some(DatabaseScheme::Sqlite) => "sqlite",
                        None => "none",
                    }
                    .to_owned();
                    lines.push(format!("database: {}", database_url::describe(url)));
                    if !database_url::is_well_formed(url) {
                        diagnostics.push(Diagnostic::error(
                            "database.url_malformed",
                            "FORGE_DATABASE_URL is malformed.",
                            "Percent-encode `/`, `?`, `#` and `@` in the user name and password (for example `/` as %2F), and give a `host=` parameter a plain host name or an absolute socket directory; the URL is not shown because it holds credentials.",
                        ));
                        lines.push(
                            "database reachability: not checked (the URL is malformed)".to_owned(),
                        );
                    } else {
                        lines.push(match reachability(url, directory, probes, process_environment) {
                        Ok(text) => format!("database reachability: {text}"),
                        Err(reason) => {
                            diagnostics.push(Diagnostic::error(
                                "database.unreachable",
                                "The configured database server does not answer.",
                                &format!("Start it (for example `docker compose up -d db`) and check the host and port of FORGE_DATABASE_URL: {reason}."),
                            ));
                            format!("database reachability: unreachable ({reason})")
                        }
                        });
                    }
                    if config.database.migration_url.is_none() {
                        lines.push(
                            "migration database: FORGE_MIGRATION_DATABASE_URL is not set (migrate, seed and setup need it)"
                                .to_owned(),
                        );
                    }
                }
            }
        }
    }
    lines.push(toolchain(probes, &mut diagnostics));

    let healthy = diagnostics.iter().all(|found| found.severity != "error");
    if json_output {
        let report = json!({
            "schema_version": 1,
            "ok": healthy,
            "environment": environment,
            "database": if environment.is_some() { Some(database) } else { None },
            "diagnostics": diagnostics.iter().map(Diagnostic::json).collect::<Vec<_>>(),
        });
        writeln!(output, "{report}").map_err(CliError::Output)?;
    } else {
        for line in &lines {
            writeln!(output, "{line}").map_err(CliError::Output)?;
        }
        for found in &diagnostics {
            writeln!(
                output,
                "{}: {} {}",
                found.severity, found.summary, found.remediation
            )
            .map_err(CliError::Output)?;
        }
    }
    if healthy {
        Ok(())
    } else {
        Err(CliError::Reported)
    }
}

/// `cargo` and `rustc` versions; a missing tool is an error finding.
fn toolchain(probes: &dyn Probes, diagnostics: &mut Vec<Diagnostic>) -> String {
    let mut parts = Vec::new();
    for tool in ["cargo", "rustc"] {
        match probes.tool_version(tool) {
            Ok(version) => parts.push(version),
            Err(_) => {
                diagnostics.push(Diagnostic::error(
                    if tool == "cargo" {
                        "toolchain.cargo_missing"
                    } else {
                        "toolchain.rustc_missing"
                    },
                    &format!("`{tool}` could not be run."),
                    "Install the Rust toolchain with rustup (https://rustup.rs); generated applications pin theirs in rust-toolchain.toml.",
                ));
                parts.push(format!("{tool} not found"));
            }
        }
    }
    format!("toolchain: {}", parts.join("; "))
}

/// Whether the server answers: a TCP connection for server databases, the
/// file for SQLite. Never logs in.
fn reachability(
    url: &forge_config::SecretString,
    directory: &Path,
    probes: &dyn Probes,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    if DatabaseScheme::of_url(url.expose()) == Some(DatabaseScheme::Sqlite) {
        // The file is created on first use; only say whether it exists yet.
        let described = database_url::describe(url);
        let file = described
            .strip_prefix("sqlite database ")
            .unwrap_or_default();
        let exists = probes.file_exists(&directory.join(file));
        return Ok(if exists {
            "the file exists".to_owned()
        } else {
            "the file does not exist yet (`forge db create` or the first migrate creates it)"
                .to_owned()
        });
    }
    let accepts = |host: &str, port: u16| {
        probes
            .connect(host, port)
            .map(|()| "the server accepts connections".to_owned())
    };
    match database_url::server(url, environment) {
        Some(Server::Tcp { host, port }) => accepts(&host, port),
        // The driver prefers a socket in a well-known directory when a
        // server listens there, and falls back to `localhost`.
        Some(Server::Default { port }) => {
            let socket = format!(".s.PGSQL.{port}");
            if DEFAULT_POSTGRES_SOCKET_DIRECTORIES
                .iter()
                .any(|directory| probes.file_exists(&Path::new(directory).join(&socket)))
            {
                Ok("not checked (a Unix socket)".to_owned())
            } else {
                accepts("localhost", port)
            }
        }
        Some(Server::Socket { .. }) | None => Ok("not checked (a Unix socket)".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct Fake {
        cargo: Result<&'static str, &'static str>,
        rustc: Result<&'static str, &'static str>,
        connect: Result<(), &'static str>,
        file: bool,
        connected: RefCell<Vec<(String, u16)>>,
    }

    impl Fake {
        fn healthy() -> Self {
            Self {
                cargo: Ok("cargo 1.98.1 (test)"),
                rustc: Ok("rustc 1.98.1 (test)"),
                connect: Ok(()),
                file: false,
                connected: RefCell::default(),
            }
        }
    }

    impl Probes for Fake {
        fn tool_version(&self, tool: &str) -> Result<String, String> {
            let answer = if tool == "cargo" {
                self.cargo
            } else {
                self.rustc
            };
            answer.map(str::to_owned).map_err(str::to_owned)
        }
        fn connect(&self, host: &str, port: u16) -> Result<(), String> {
            self.connected.borrow_mut().push((host.to_owned(), port));
            self.connect.map_err(str::to_owned)
        }
        fn file_exists(&self, _: &Path) -> bool {
            self.file
        }
    }

    fn variables(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    fn report(
        json_output: bool,
        pairs: &[(&str, &str)],
        probes: &Fake,
    ) -> (Result<(), CliError>, String) {
        let mut output = Vec::new();
        let result = run(
            json_output,
            variables(pairs),
            Path::new("."),
            probes,
            &mut output,
        );
        (result, String::from_utf8(output).expect("utf-8"))
    }

    #[test]
    fn a_healthy_setup_reports_each_check() {
        let probes = Fake::healthy();
        let (result, text) = report(
            false,
            &[
                (
                    "FORGE_DATABASE_URL",
                    "postgres://app_runtime:hunter2@127.0.0.1:5432/shop?sslmode=disable&password=hunter3",
                ),
                (
                    "FORGE_MIGRATION_DATABASE_URL",
                    "postgres://app_migrator:hunter4@127.0.0.1:5432/shop",
                ),
            ],
            &probes,
        );
        result.expect("healthy");
        assert!(text.contains("configuration: ok\n"), "{text}");
        assert!(text.contains("environment: development\n"), "{text}");
        assert!(text.contains("http address: 127.0.0.1:3000\n"), "{text}");
        assert!(
            text.contains("database: postgres database \"shop\" on 127.0.0.1:5432\n"),
            "{text}"
        );
        assert!(
            text.contains("database reachability: the server accepts connections\n"),
            "{text}"
        );
        assert!(
            text.contains("toolchain: cargo 1.98.1 (test); rustc 1.98.1 (test)\n"),
            "{text}"
        );
        for secret in ["hunter", "app_runtime", "app_migrator"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert_eq!(*probes.connected.borrow(), [("127.0.0.1".to_owned(), 5432)]);
    }

    #[test]
    fn an_unreachable_database_is_an_error_without_leaking_the_url() {
        let mut probes = Fake::healthy();
        probes.connect = Err("connection refused");
        let (result, text) = report(
            false,
            &[(
                "FORGE_DATABASE_URL",
                "mysql://root:hunter2@db.internal/shop",
            )],
            &probes,
        );
        assert!(matches!(result, Err(CliError::Reported)));
        assert!(text.contains("database reachability: unreachable (connection refused)"));
        assert!(text.contains("error: The configured database server does not answer."));
        assert!(
            !text.contains("hunter2") && !text.contains("root"),
            "{text}"
        );
        assert_eq!(
            *probes.connected.borrow(),
            [("db.internal".to_owned(), 3306)]
        );
    }

    #[test]
    fn invalid_configuration_is_reported_and_fails() {
        let probes = Fake::healthy();
        for pairs in [
            &[("FORGE_ENV", "staging")][..],
            &[("FORGE_BIDN", "127.0.0.1:1")][..],
            &[("FORGE_REQUEST_TIMEOUT_SECS", "0")][..],
        ] {
            let (result, text) = report(false, pairs, &probes);
            assert!(matches!(result, Err(CliError::Reported)), "{pairs:?}");
            assert!(text.contains("configuration: invalid"), "{text}");
            assert!(
                text.contains("error: Forge configuration is invalid."),
                "{text}"
            );
        }
    }

    #[test]
    fn the_destructive_switch_is_the_clis_not_a_setting() {
        let (result, text) = report(
            false,
            &[("FORGE_ALLOW_DESTRUCTIVE", "true")],
            &Fake::healthy(),
        );
        result.expect("not an unknown key");
        assert!(text.contains("configuration: ok"), "{text}");
    }

    #[test]
    fn a_missing_toolchain_fails() {
        let mut probes = Fake::healthy();
        probes.cargo = Err("not found");
        let (result, text) = report(false, &[], &probes);
        assert!(matches!(result, Err(CliError::Reported)));
        assert!(
            text.contains("toolchain: cargo not found; rustc 1.98.1 (test)"),
            "{text}"
        );
        assert!(text.contains("error: `cargo` could not be run."), "{text}");
    }

    #[test]
    fn a_malformed_database_url_is_an_error_that_shows_nothing_of_it() {
        let probes = Fake::healthy();
        for url in [
            "postgres://admin:s3cr/et@db.internal/shop",
            "postgres://admin:123/et@db.internal/shop",
            "mysql://root:123?et@db.internal/shop",
            "postgres://admin:123#et@db.internal/shop",
        ] {
            let (result, text) = report(false, &[("FORGE_DATABASE_URL", url)], &probes);
            assert!(matches!(result, Err(CliError::Reported)), "{url}");
            assert!(
                text.contains("error: FORGE_DATABASE_URL is malformed."),
                "{text}"
            );
            for word in ["admin", "root", "s3cr", "123", "et@", "db.internal"] {
                assert!(!text.contains(word), "{word} leaked:\n{text}");
            }
            assert!(
                probes.connected.borrow().is_empty(),
                "no connection to a guessed host"
            );
        }
    }

    #[test]
    fn sqlite_and_sockets_need_no_connection() {
        let probes = Fake::healthy();
        let (result, text) = report(
            false,
            &[("FORGE_DATABASE_URL", "sqlite:storage/development.sqlite3")],
            &probes,
        );
        result.expect("healthy");
        assert!(text.contains("database: sqlite database storage/development.sqlite3"));
        assert!(text.contains("the file does not exist yet"), "{text}");
        let (result, text) = report(
            false,
            &[(
                "FORGE_DATABASE_URL",
                "postgres:///shop?host=/var/run/postgresql",
            )],
            &probes,
        );
        result.expect("healthy");
        assert!(text.contains("not checked (a Unix socket)"), "{text}");
        assert!(probes.connected.borrow().is_empty());
    }

    #[test]
    fn the_probe_honors_the_pg_environment_like_the_real_connection() {
        let url = "postgres:///shop";
        // PGHOST and PGPORT fill what the URL leaves out.
        let probes = Fake::healthy();
        let (result, text) = report(
            false,
            &[
                ("FORGE_DATABASE_URL", url),
                ("PGHOST", "db.internal"),
                ("PGPORT", "6543"),
            ],
            &probes,
        );
        result.expect("healthy");
        assert!(text.contains("the server accepts connections"), "{text}");
        assert_eq!(
            *probes.connected.borrow(),
            [("db.internal".to_owned(), 6543)]
        );
        // PGHOSTADDR wins over PGHOST, as in the driver; the URL wins over both.
        let probes = Fake::healthy();
        report(
            false,
            &[
                ("FORGE_DATABASE_URL", url),
                ("PGHOST", "db.internal"),
                ("PGHOSTADDR", "10.0.0.5"),
            ],
            &probes,
        )
        .0
        .expect("healthy");
        assert_eq!(*probes.connected.borrow(), [("10.0.0.5".to_owned(), 5432)]);
        let probes = Fake::healthy();
        report(
            false,
            &[
                ("FORGE_DATABASE_URL", "postgres://u:p@own.host:7000/shop"),
                ("PGHOST", "db.internal"),
                ("PGPORT", "6543"),
            ],
            &probes,
        )
        .0
        .expect("healthy");
        assert_eq!(*probes.connected.borrow(), [("own.host".to_owned(), 7000)]);
        // A socket directory in PGHOST is not a TCP endpoint.
        let probes = Fake::healthy();
        let (_, text) = report(
            false,
            &[
                ("FORGE_DATABASE_URL", url),
                ("PGHOST", "/var/run/postgresql"),
            ],
            &probes,
        );
        assert!(text.contains("not checked (a Unix socket)"), "{text}");
        assert!(probes.connected.borrow().is_empty());
        // The probe never prints what it read.
        let (_, text) = report(
            false,
            &[
                ("FORGE_DATABASE_URL", url),
                ("PGHOST", "db.internal"),
                ("PGPASSWORD", "hunter2"),
            ],
            &Fake::healthy(),
        );
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn without_a_host_anywhere_the_default_socket_or_localhost_is_probed() {
        let url = ("FORGE_DATABASE_URL", "postgres:///shop");
        let probes = Fake::healthy();
        report(false, &[url], &probes).0.expect("healthy");
        assert_eq!(*probes.connected.borrow(), [("localhost".to_owned(), 5432)]);
        let mut probes = Fake::healthy();
        probes.file = true; // the default socket file exists
        let (_, text) = report(false, &[url], &probes);
        assert!(text.contains("not checked (a Unix socket)"), "{text}");
        assert!(probes.connected.borrow().is_empty());
    }

    #[test]
    fn json_output_is_versioned_and_secret_free() {
        let probes = Fake::healthy();
        let (result, text) = report(
            true,
            &[("FORGE_DATABASE_URL", "postgres://u:hunter2@127.0.0.1/shop")],
            &probes,
        );
        result.expect("healthy");
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            value,
            json!({
                "schema_version": 1,
                "ok": true,
                "environment": "development",
                "database": "postgres",
                "diagnostics": [],
            })
        );

        let (result, text) = report(true, &[("FORGE_ENV", "staging")], &probes);
        assert!(matches!(result, Err(CliError::Reported)));
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["ok"], false);
        assert_eq!(value["environment"], Value::Null);
        assert_eq!(value["diagnostics"][0]["code"], "configuration.invalid");
        assert!(!text.contains("staging"), "values stay out of JSON: {text}");
    }
}
