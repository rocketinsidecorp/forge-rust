//! What may be said about a database URL without leaking its credentials:
//! the engine, the database name and the host.
//!
//! Tools that print or decide on a URL (`forge doctor`, the application's `db`
//! commands) use these functions instead of echoing the URL or its parts. The
//! URL is read with the parser SQLx uses (`url`), applying the same query
//! overrides the driver applies (`host`, `hostaddr`, `port` and `dbname` for
//! PostgreSQL, `socket` for MySQL/MariaDB), so what is described is what the
//! driver would connect to. Nothing here returns a user, a password or another
//! query parameter, with one deliberate exception: [`client_target`], for
//! tools that execute the vendor client (`psql`, `mysql`) and must hand it the
//! credentials.
//!
//! A URL is **well formed** only when `url` parses it and no `@` follows its
//! authority. An unescaped `/`, `?` or `#` in a password ends the authority
//! early, which would leave part of the password in the "host" or the path; the
//! `@` that follows gives it away, and such a URL is refused, never described.

use url::{Host, Url};

use crate::{DatabaseScheme, SecretString};

/// System databases that are never created or dropped.
const RESERVED: &[&str] = &[
    "postgres",
    "template0",
    "template1",
    "mysql",
    "information_schema",
    "performance_schema",
    "sys",
];

/// The message for a URL that is not well formed. It never contains the URL.
pub const MALFORMED: &str = "the database URL is malformed: percent-encode `/`, `?`, `#` and `@` \
     in the user name and password, and give a `host=` parameter a plain host name or an \
     absolute socket directory (no `@`, `/`, `?`, `#`, `\\` or spaces in a host name)";

/// Where a server URL points, after the driver's query overrides.
#[derive(Debug, Eq, PartialEq)]
struct Location {
    host: Option<String>,
    port: Option<u16>,
    socket: bool,
    /// The socket directory or file, when the URL names one.
    socket_path: Option<String>,
    database: Option<String>,
}

/// Whether `url` parses and nothing that looks like userinfo follows the
/// authority (see the module documentation). SQLite URLs have no authority and
/// are always well formed here.
#[must_use]
pub fn is_well_formed(url: &SecretString) -> bool {
    let raw = url.expose();
    if DatabaseScheme::of_url(raw) == Some(DatabaseScheme::Sqlite) {
        return true;
    }
    location(raw).is_some()
}

/// Names the database for messages: engine, database and host, never a user,
/// a password or a query. A URL that is not well formed is described by its
/// engine only.
#[must_use]
pub fn describe(url: &SecretString) -> String {
    let raw = url.expose();
    match DatabaseScheme::of_url(raw) {
        Some(DatabaseScheme::Sqlite) => format!("sqlite database {}", sqlite_path(raw)),
        Some(scheme) => {
            let engine = match scheme {
                DatabaseScheme::Postgres => "postgres",
                _ => "mysql",
            };
            let Some(found) = location(raw) else {
                return format!("{engine} database (the URL is malformed)");
            };
            describe_location(
                engine,
                found.database.as_deref(),
                found.host.as_deref(),
                found.port,
            )
        }
        None => "database".to_owned(),
    }
}

/// [`describe`] for an already resolved server, database and port.
#[must_use]
pub fn describe_location(
    engine: &str,
    database: Option<&str>,
    host: Option<&str>,
    port: Option<u16>,
) -> String {
    let name = database.map_or_else(|| "?".to_owned(), |name| format!("\"{name}\""));
    match (host, port) {
        (Some(host), Some(port)) => {
            format!("{engine} database {name} on {}:{port}", bracketed(host))
        }
        (Some(host), None) => format!("{engine} database {name} on {}", bracketed(host)),
        _ => format!("{engine} database {name}"),
    }
}

/// An IPv6 literal in brackets, exactly once: SQLx hands back the host of
/// `postgres://u@[::1]/db` already bracketed, `url`'s parser does not.
fn bracketed(host: &str) -> String {
    let bare = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    if bare.contains(':') {
        format!("[{bare}]")
    } else {
        bare.to_owned()
    }
}

/// Where the driver would connect for a server URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Server {
    /// A TCP endpoint; an IPv6 host has no brackets.
    Tcp {
        /// Host name or address.
        host: String,
        /// Port, from the URL, the environment or the engine's default.
        port: u16,
    },
    /// A Unix socket: the directory (PostgreSQL) or file (MySQL/MariaDB) when
    /// the URL or the environment names one.
    Socket {
        /// The directory or file, when known.
        path: Option<String>,
        /// PostgreSQL only: the port, which names the socket file
        /// (`.s.PGSQL.<port>`).
        port: Option<u16>,
    },
    /// PostgreSQL with no host in the URL or the environment: the driver
    /// uses a socket in one of
    /// [`DEFAULT_POSTGRES_SOCKET_DIRECTORIES`] when a server listens there,
    /// and `localhost` otherwise.
    Default {
        /// The port of the socket file name and of the `localhost` fallback.
        port: u16,
    },
}

/// The socket directories SQLx tries, in order, for a PostgreSQL URL without
/// a host: the first that holds `.s.PGSQL.<port>` is used.
pub const DEFAULT_POSTGRES_SOCKET_DIRECTORIES: [&str; 3] =
    ["/var/run/postgresql", "/private/tmp", "/tmp"];

/// What the driver would connect to for `url`, applying the URL's query
/// overrides and, for PostgreSQL, the `PGHOSTADDR`, `PGHOST` and `PGPORT`
/// defaults read through `env` (the process environment for a tool that
/// describes the real connection; `|_| None` for one that must ignore it).
/// An empty host is unset, as in libpq. `None` for SQLite, a URL that is not
/// well formed or another engine.
#[must_use]
pub fn server(url: &SecretString, env: &dyn Fn(&str) -> Option<String>) -> Option<Server> {
    let raw = url.expose();
    let scheme = DatabaseScheme::of_url(raw)?;
    if scheme == DatabaseScheme::Sqlite {
        return None;
    }
    let found = location(raw)?;
    let host = found.host.filter(|host| !host.is_empty());
    match scheme {
        DatabaseScheme::Postgres => {
            let port = found
                .port
                .or_else(|| {
                    env("PGPORT")
                        .and_then(|port| port.trim().parse::<u16>().ok())
                        .filter(|port| *port != 0)
                })
                .unwrap_or(5432);
            if found.socket {
                return Some(Server::Socket {
                    path: found.socket_path,
                    port: Some(port),
                });
            }
            let host = host.or_else(|| {
                ["PGHOSTADDR", "PGHOST"]
                    .into_iter()
                    .find_map(|key| env(key).filter(|value| !value.is_empty()))
            });
            Some(match host {
                Some(host) if host.starts_with('/') => Server::Socket {
                    path: Some(host),
                    port: Some(port),
                },
                Some(host) => Server::Tcp { host, port },
                None => Server::Default { port },
            })
        }
        DatabaseScheme::MySql if found.socket => Some(Server::Socket {
            path: found.socket_path,
            port: None,
        }),
        DatabaseScheme::MySql => Some(Server::Tcp {
            host: host.unwrap_or_else(|| "localhost".to_owned()),
            port: found.port.unwrap_or(3306),
        }),
        DatabaseScheme::Sqlite => None,
    }
}

/// TLS settings a client tool needs, as the URL spells them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientTls {
    /// `sslmode` (PostgreSQL) or `ssl-mode` (MySQL/MariaDB).
    pub mode: Option<String>,
    /// The CA bundle file.
    pub root_certificate: Option<String>,
    /// The client certificate file.
    pub certificate: Option<String>,
    /// The client key file.
    pub key: Option<String>,
}

/// Everything a vendor client tool (`psql`, `mysql`) needs to connect where
/// the driver would, including the credentials. The password is a
/// [`SecretString`]; hand it to the tool through the environment or an option
/// file, never an argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientTarget {
    /// The user name, decoded; `None` leaves the tool's default.
    pub user: Option<String>,
    /// The password, decoded.
    pub password: Option<SecretString>,
    /// The database name after the driver's overrides.
    pub database: Option<String>,
    /// Where to connect.
    pub server: Server,
    /// TLS settings.
    pub tls: ClientTls,
}

/// Reads the connection a client tool needs from a PostgreSQL or
/// MySQL/MariaDB URL, with the query overrides the driver applies (`user`,
/// `password`, `host`, `hostaddr`, `port`, `dbname`, `socket` and the TLS
/// options). `None` when the URL is not well formed, names SQLite or another
/// engine. `env` is as in [`server`].
///
/// This is the one function of this module that returns credentials: it
/// exists for tools that execute the vendor client, and its result must not
/// be printed.
#[must_use]
pub fn client_target(
    url: &SecretString,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<ClientTarget> {
    let raw = url.expose();
    let scheme = DatabaseScheme::of_url(raw)?;
    let found = location(raw)?;
    let server = server(url, env)?;
    let parsed = Url::parse(raw.trim()).ok()?;
    let mut user = if parsed.username().is_empty() {
        None
    } else {
        percent_decode(parsed.username())
    };
    let mut password = parsed.password().and_then(percent_decode);
    let mut tls = ClientTls::default();
    for (key, value) in parsed.query_pairs() {
        let value = value.into_owned();
        match (scheme, &*key) {
            (DatabaseScheme::Postgres, "user") => user = Some(value),
            (DatabaseScheme::Postgres, "password") => password = Some(value),
            (_, "sslmode" | "ssl-mode") => tls.mode = Some(value),
            (DatabaseScheme::Postgres, "sslrootcert" | "ssl-root-cert" | "ssl-ca")
            | (DatabaseScheme::MySql, "sslca" | "ssl-ca") => tls.root_certificate = Some(value),
            (_, "sslcert" | "ssl-cert") => tls.certificate = Some(value),
            (_, "sslkey" | "ssl-key") => tls.key = Some(value),
            _ => {}
        }
    }
    Some(ClientTarget {
        user: user.filter(|value| !value.is_empty()),
        password: password
            .filter(|value| !value.is_empty())
            .map(SecretString::new),
        database: found.database,
        server,
        tls,
    })
}

/// The TCP endpoint a server URL names by itself, with the engine's default
/// port when the URL has none; `None` for SQLite, a Unix socket, a host-less
/// URL or a URL that is not well formed. It ignores the environment; use
/// [`server`] to describe the connection the driver would make.
#[must_use]
pub fn endpoint(url: &SecretString) -> Option<(String, u16)> {
    let raw = url.expose();
    let default_port = match DatabaseScheme::of_url(raw)? {
        DatabaseScheme::Postgres => 5432,
        DatabaseScheme::MySql => 3306,
        DatabaseScheme::Sqlite => return None,
    };
    let found = location(raw)?;
    if found.socket {
        return None;
    }
    let host = found.host.filter(|host| !host.is_empty())?;
    Some((host, found.port.unwrap_or(default_port)))
}

/// Whether `name` may be created or dropped: 1-63 characters of
/// `[A-Za-z0-9_$-]`, not starting with `$` or `-`, and not one of the engines'
/// system databases.
///
/// # Errors
///
/// A message that never contains the URL.
pub fn validate_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    let acceptable = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric() || first == '_')
        && name.len() <= 63
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '-'));
    if !acceptable {
        return Err(
            "the database name in the URL must be 1-63 letters, digits, underscores, \
             hyphens or dollar signs, and start with a letter, digit or underscore"
                .to_owned(),
        );
    }
    if RESERVED
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(format!("refusing to touch the \"{name}\" system database"));
    }
    Ok(())
}

/// Parses a PostgreSQL or MySQL/MariaDB URL like the driver does. `None` when
/// the URL is not well formed or names another engine.
fn location(raw: &str) -> Option<Location> {
    let scheme = DatabaseScheme::of_url(raw)?;
    let url = Url::parse(raw.trim()).ok()?;
    // A `/`, `?` or `#` inside a password ends the authority early; the rest of
    // the password is then in the path or query, next to the `@` that closes it.
    if url.path().contains('@') || url.query().is_some_and(|query| query.contains('@')) {
        return None;
    }
    if url
        .fragment()
        .is_some_and(|fragment| fragment.contains('@'))
    {
        return None;
    }
    let mut found = Location {
        host: None,
        port: url.port(),
        socket: false,
        socket_path: None,
        database: None,
    };
    match url.host() {
        Some(Host::Domain(domain)) => {
            if scheme == DatabaseScheme::Postgres && domain.to_ascii_lowercase().starts_with("%2f")
            {
                found.socket = true;
                found.socket_path = percent_decode(domain);
            } else if !domain.is_empty() {
                found.host = Some(domain.to_owned());
            }
        }
        Some(Host::Ipv4(address)) => found.host = Some(address.to_string()),
        Some(Host::Ipv6(address)) => found.host = Some(address.to_string()),
        None => {}
    }
    let path = url.path().trim_start_matches('/');
    if !path.is_empty() {
        found.database = Some(percent_decode(path)?);
    }
    for (key, value) in url.query_pairs() {
        match (scheme, &*key) {
            (DatabaseScheme::Postgres, "host") if value.starts_with('/') => {
                found.socket = true;
                found.socket_path = Some(value.into_owned());
            }
            // A host that carries a URL delimiter, a backslash, whitespace
            // or a control character is no host name (found by the
            // config_database_url fuzz target; seed `host-query-delimiter`):
            // the URL is not well formed.
            (DatabaseScheme::Postgres, "host") => {
                let plain = !value.chars().any(|character| {
                    character.is_whitespace()
                        || character.is_control()
                        || "@/?#\\".contains(character)
                });
                if !plain {
                    return None;
                }
                found.host = Some(value.into_owned());
            }
            (DatabaseScheme::Postgres, "hostaddr") => {
                value.parse::<std::net::IpAddr>().ok()?;
                found.host = Some(value.into_owned());
            }
            (DatabaseScheme::Postgres, "port") => found.port = Some(value.parse().ok()?),
            (DatabaseScheme::Postgres, "dbname") => found.database = Some(value.into_owned()),
            (DatabaseScheme::MySql, "socket") => {
                found.socket = true;
                found.socket_path = Some(value.into_owned());
            }
            _ => {}
        }
    }
    // Port 0 names no server (Forge-Go refuses ports outside 1-65535): a URL
    // with it is not well formed and has no endpoint.
    if found.port == Some(0) {
        return None;
    }
    Some(found)
}

/// Decodes `%XX` escapes; `None` for a malformed escape or invalid UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = text.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(pair, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The file a SQLite URL names, without scheme or query and percent-decoded
/// like the driver does; `None` for another engine. `:memory:` is returned
/// as is.
#[must_use]
pub fn sqlite_file(url: &SecretString) -> Option<String> {
    let raw = url.expose();
    if DatabaseScheme::of_url(raw)? != DatabaseScheme::Sqlite {
        return None;
    }
    let path = sqlite_path(raw);
    Some(percent_decode(path).unwrap_or_else(|| path.to_owned()))
}

/// The file of a `sqlite:` URL, without scheme or query.
fn sqlite_path(url: &str) -> &str {
    let rest = url.split_once(':').map_or(url, |(_, rest)| rest);
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    rest.split('?').next().unwrap_or(rest)
}

/// What a PostgreSQL URL needs in production, as the configuration error
/// says it. It never contains the URL.
pub const POSTGRES_TLS_REQUIRED: &str = "a URL with sslmode=verify-full in production, so \
     the server certificate and host name are verified: append ?sslmode=verify-full (and \
     sslrootcert=/path/to/ca.pem for a private CA). A Unix socket (host=/run/postgresql) needs no \
     TLS; FORGE_DATABASE_ALLOW_PLAINTEXT=true accepts an unverified or plaintext connection, only \
     on a trusted private network";

/// What a MySQL/MariaDB URL needs in production, as the configuration error
/// says it. It never contains the URL.
pub const MYSQL_TLS_REQUIRED: &str = "a URL with ssl-mode=VERIFY_IDENTITY in production, \
     so the server certificate and host name are verified: append ?ssl-mode=VERIFY_IDENTITY (and \
     ssl-ca=/path/to/ca.pem for a private CA). A Unix socket (socket=/run/mysqld/mysqld.sock) \
     needs no TLS; FORGE_DATABASE_ALLOW_PLAINTEXT=true accepts an unverified or plaintext \
     connection, only on a trusted private network";

/// Checks that a connection to `url` authenticates the server with TLS, the
/// rule production applies to both database URLs (Forge-Go's
/// `config/database.go`).
///
/// PostgreSQL needs `sslmode=verify-full`: SQLx's default, `prefer`, falls
/// back to plaintext, and `require` encrypts without checking the
/// certificate, so both can be intercepted. MySQL/MariaDB needs
/// `ssl-mode=VERIFY_IDENTITY` (`PREFERRED` and `REQUIRED` do not verify).
/// `verify-ca` and `VERIFY_CA` are refused too: SQLx's rustls backend adds
/// `sslrootcert`/`ssl-ca` to the public web roots and then skips the host
/// name check, so any publicly trusted certificate (one for a domain the
/// attacker owns) would pass. Forge-Go's driver trusts only the given CA, so
/// it accepts `verify-ca`; the stricter rule here is deliberate.
///
/// The mode is read as the driver reads it: either spelling of
/// the key (`sslmode`, `ssl-mode`), the last occurrence winning, the value in
/// any case. A Unix socket the URL names has no network path to intercept
/// (and the servers offer no TLS on it), and SQLite is a local file: both
/// pass. A URL that is not well formed fails.
///
/// # Errors
///
/// The contract the URL misses ([`POSTGRES_TLS_REQUIRED`],
/// [`MYSQL_TLS_REQUIRED`] or [`MALFORMED`]); it never contains the URL.
///
/// ```
/// use forge_config::{SecretString, database_url::require_verified_tls};
///
/// let verified = SecretString::new("postgres://app:pw@db.internal/shop?sslmode=verify-full");
/// assert!(require_verified_tls(&verified).is_ok());
/// let downgradable = SecretString::new("postgres://app:pw@db.internal/shop");
/// assert!(require_verified_tls(&downgradable).is_err());
/// ```
pub fn require_verified_tls(url: &SecretString) -> Result<(), &'static str> {
    let raw = url.expose();
    let (verified, required): (&[&str], _) = match DatabaseScheme::of_url(raw) {
        Some(DatabaseScheme::Sqlite) => return Ok(()),
        Some(DatabaseScheme::Postgres) => (&["verify-full"], POSTGRES_TLS_REQUIRED),
        Some(DatabaseScheme::MySql) => (&["verify_identity"], MYSQL_TLS_REQUIRED),
        None => return Err(MALFORMED),
    };
    let found = location(raw).ok_or(MALFORMED)?;
    if found.socket {
        return Ok(());
    }
    let parsed = Url::parse(raw.trim()).map_err(|_| MALFORMED)?;
    let mode = parsed
        .query_pairs()
        .filter(|(key, _)| matches!(&**key, "sslmode" | "ssl-mode"))
        .last()
        .map(|(_, value)| value.to_ascii_lowercase());
    match mode {
        Some(mode) if verified.contains(&mode.as_str()) => Ok(()),
        _ => Err(required),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(url: &str) -> SecretString {
        SecretString::new(url)
    }

    fn located(url: &str) -> Option<Location> {
        location(url)
    }

    #[test]
    fn names_are_restricted_to_what_every_tool_accepts() {
        for good in ["app", "app_dev", "App-1", "_x", "a$b", &"a".repeat(63)] {
            assert_eq!(validate_name(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "-x",
            "$x",
            "a b",
            "a\"b",
            "a;b",
            "a%20b",
            "a/b",
            &"a".repeat(64),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        for name in ["postgres", "TEMPLATE1", "template0", "mysql", "sys"] {
            let message = validate_name(name).expect_err("reserved");
            assert!(message.contains("system database"), "{message}");
        }
    }

    #[test]
    fn the_effective_database_follows_the_drivers_query_overrides() {
        let found = located("postgres://u:p@h/shop?dbname=postgres").expect("parses");
        assert_eq!(found.database.as_deref(), Some("postgres"));
        let found = located("postgres://u:p@h/shop%5Fdev").expect("parses");
        assert_eq!(found.database.as_deref(), Some("shop_dev"));
        let found = located("postgres://u:p@h").expect("parses");
        assert_eq!(found.database, None);
    }

    #[test]
    fn the_effective_host_and_port_follow_the_drivers_query_overrides() {
        let found = located("postgres://m:pw@localhost/shop?host=prod.db.internal").expect("p");
        assert_eq!(found.host.as_deref(), Some("prod.db.internal"));
        let found = located("postgres:///shop?host=prod").expect("parses");
        assert_eq!(found.host.as_deref(), Some("prod"));
        let found = located("postgres://u:p@localhost/shop?hostaddr=10.0.0.5").expect("p");
        assert_eq!(found.host.as_deref(), Some("10.0.0.5"));
        let found = located("postgres://u:p@h:5432/shop?port=6543").expect("parses");
        assert_eq!(found.port, Some(6543));
        let found = located("postgres:///shop?host=/var/run/postgresql").expect("parses");
        assert!(found.socket);
        for delimited in [
            "postgres://u:p@tqtidata/ipzp.d5?host=rwcerify-ll:0/dE",
            "postgres://u:p@h/shop?host=a%40b",
            "postgres://u:p@h/shop?host=a%5Cb",
            "postgres://u:p@h/shop?host=a%20b",
            "postgres://u:p@h/shop?host=a%0Ab",
        ] {
            assert!(located(delimited).is_none(), "{delimited}");
            assert_eq!(endpoint(&secret(delimited)), None, "{delimited}");
        }
        let found = located("postgres://%2Fvar%2Frun%2Fpostgresql/shop").expect("parses");
        assert!(found.socket);
        let found = located("mysql://u:p@h/shop?socket=/tmp/mysql.sock").expect("parses");
        assert!(found.socket);
        // A bad override is not guessed at.
        assert_eq!(located("postgres://u:p@h/shop?hostaddr=not-an-ip"), None);
        assert_eq!(located("postgres://u:p@h/shop?port=many"), None);
    }

    #[test]
    fn describe_names_the_effective_target_and_never_credentials() {
        let url = secret(
            "postgres://admin:hunter2@db.internal:5433/shop?password=hunter3&sslmode=require",
        );
        let text = describe(&url);
        assert_eq!(text, "postgres database \"shop\" on db.internal:5433");
        assert!(!text.contains("hunter"));
        assert_eq!(
            describe(&secret(
                "postgres://m:pw@localhost/shop?host=prod.db.internal&port=6543"
            )),
            "postgres database \"shop\" on prod.db.internal:6543"
        );
        assert_eq!(
            describe(&secret("mariadb://u:p@[::1]:3306/shop")),
            "mysql database \"shop\" on [::1]:3306"
        );
        assert_eq!(
            describe(&secret("sqlite://storage/dev.sqlite3?mode=rwc")),
            "sqlite database storage/dev.sqlite3"
        );
        assert_eq!(describe(&secret("sqlite:dev.db")), "sqlite database dev.db");
        assert_eq!(describe(&secret("redis://cache")), "database");
    }

    /// Passwords with URL delimiters, unescaped: nothing of the user name or
    /// the password may reach a description.
    #[test]
    fn delimiters_in_a_password_never_leak_into_a_description() {
        for (url, leaked) in [
            (
                "postgres://admin:s3cr/et@db.internal/shop",
                ["admin", "s3cr", "et"],
            ),
            (
                "postgres://admin:s3cr?et@db.internal/shop",
                ["admin", "s3cr", "et"],
            ),
            (
                "postgres://admin:s3cr#et@db.internal/shop",
                ["admin", "s3cr", "et"],
            ),
            // The part before the delimiter parses as a valid `host:port`.
            (
                "postgres://admin:123/et@db.internal/shop",
                ["admin", "123", "et@"],
            ),
            (
                "postgres://admin:123?et@db.internal/shop",
                ["admin", "123", "et@"],
            ),
            (
                "postgres://admin:123#et@db.internal/shop",
                ["admin", "123", "et@"],
            ),
            (
                "mysql://admin:123/et@db.internal/shop",
                ["admin", "123", "et@"],
            ),
            (
                "mysql://root:9/pa@ss@db.internal/shop",
                ["root", "pa", "ss"],
            ),
        ] {
            let url = secret(url);
            assert!(!is_well_formed(&url), "{url:?}");
            let text = describe(&url);
            assert!(text.ends_with("database (the URL is malformed)"), "{text}");
            for word in leaked {
                assert!(!text.contains(word), "{word} leaked: {text}");
            }
            assert_eq!(endpoint(&url), None);
        }
    }

    #[test]
    fn escaped_and_userinfo_at_signs_are_fine() {
        // Escaped delimiters, and an unescaped `@` before the host (the driver
        // splits at the last one), are well formed and describe only the host.
        for url in [
            "postgres://admin:s3cr%2Fet%3F%23%40x@db.internal/shop",
            "postgres://admin:p@ss@db.internal/shop",
            "postgres://admin:p%40ss@db.internal/shop",
        ] {
            let url = secret(url);
            assert!(is_well_formed(&url), "{url:?}");
            let text = describe(&url);
            assert_eq!(text, "postgres database \"shop\" on db.internal", "{text}");
        }
        // `@` in a query value is refused as well, the safe side of a guess.
        assert!(!is_well_formed(&secret(
            "postgres://u:p@h/shop?application_name=a@b"
        )));
    }

    #[test]
    fn port_zero_is_not_a_server() {
        for raw in [
            "postgres://u:p@db.internal:0/app",
            "postgres://u:p@db.internal/app?port=0",
            "mysql://u:p@db.internal:0/app",
        ] {
            let url = SecretString::new(raw);
            assert!(!is_well_formed(&url), "{raw}");
            assert_eq!(endpoint(&url), None, "{raw}");
        }
    }

    #[test]
    fn endpoints_use_the_default_port_of_the_engine_and_the_overrides() {
        let endpoint = |url: &str| endpoint(&secret(url));
        assert_eq!(
            endpoint("postgres://u:p@db.internal/app"),
            Some(("db.internal".to_owned(), 5432))
        );
        assert_eq!(
            endpoint("postgresql://u:p@db.internal:6543/app?sslmode=require"),
            Some(("db.internal".to_owned(), 6543))
        );
        assert_eq!(
            endpoint("postgres://u:p@localhost/app?host=prod&port=7000"),
            Some(("prod".to_owned(), 7000))
        );
        assert_eq!(
            endpoint("mysql://u:p@127.0.0.1/app"),
            Some(("127.0.0.1".to_owned(), 3306))
        );
        assert_eq!(
            endpoint("mariadb://u:p@[::1]:3307/app"),
            Some(("::1".to_owned(), 3307))
        );
        assert_eq!(endpoint("postgres:///app?host=/var/run/postgresql"), None);
        assert_eq!(endpoint("mysql://u:p@h/app?socket=/tmp/m.sock"), None);
        assert_eq!(endpoint("sqlite:dev.db"), None);
        assert_eq!(endpoint("postgres://u:p@h:notaport/app"), None);
        assert_eq!(endpoint("redis://h/app"), None);
    }

    #[test]
    fn an_ipv6_host_is_bracketed_once_whether_or_not_the_driver_already_did() {
        for host in ["::1", "[::1]"] {
            assert_eq!(
                describe_location("postgres", Some("shop"), Some(host), Some(5432)),
                "postgres database \"shop\" on [::1]:5432",
                "{host}"
            );
            assert_eq!(
                describe_location("postgres", Some("shop"), Some(host), None),
                "postgres database \"shop\" on [::1]",
                "{host}"
            );
        }
        assert_eq!(
            describe_location("mysql", Some("shop"), Some("db.internal"), Some(3306)),
            "mysql database \"shop\" on db.internal:3306"
        );
        assert_eq!(
            describe_location("mysql", Some("shop"), Some("10.0.0.5"), Some(3306)),
            "mysql database \"shop\" on 10.0.0.5:3306"
        );
    }

    fn environment<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn tcp(host: &str, port: u16) -> Option<Server> {
        Some(Server::Tcp {
            host: host.to_owned(),
            port,
        })
    }

    #[test]
    fn the_server_follows_the_pg_environment_like_the_driver() {
        let none = environment(&[]);
        let host = environment(&[("PGHOST", "db.internal"), ("PGPORT", "6543")]);
        let both = environment(&[("PGHOSTADDR", "10.0.0.5"), ("PGHOST", "db.internal")]);
        let socket = environment(&[("PGHOST", "/var/run/postgresql")]);
        let bad_port = environment(&[("PGHOST", "h"), ("PGPORT", "many")]);
        let empty = environment(&[("PGHOST", ""), ("PGHOSTADDR", "")]);
        let url = |text: &str| secret(text);

        // The URL wins; the environment fills what it leaves out.
        assert_eq!(
            server(&url("postgres://u:p@h:7000/app"), &host),
            tcp("h", 7000)
        );
        assert_eq!(
            server(&url("postgres:///app"), &host),
            tcp("db.internal", 6543)
        );
        assert_eq!(
            server(&url("postgres://u:p@/app"), &host),
            None,
            "not a URL"
        );
        assert_eq!(server(&url("postgres://u:p@h/app"), &host), tcp("h", 6543));
        // PGHOSTADDR is read before PGHOST, as SQLx does.
        assert_eq!(
            server(&url("postgres:///app"), &both),
            tcp("10.0.0.5", 5432)
        );
        assert_eq!(
            server(&url("postgres:///app"), &socket),
            Some(Server::Socket {
                path: Some("/var/run/postgresql".to_owned()),
                port: Some(5432)
            })
        );
        assert_eq!(server(&url("postgres:///app"), &bad_port), tcp("h", 5432));
        // Nothing anywhere: the driver's default socket or localhost.
        assert_eq!(
            server(&url("postgres:///app"), &none),
            Some(Server::Default { port: 5432 })
        );
        assert_eq!(
            server(&url("postgres:///app?port=5433"), &empty),
            Some(Server::Default { port: 5433 })
        );
        // The query overrides beat the environment.
        assert_eq!(
            server(&url("postgres:///app?host=prod"), &socket),
            tcp("prod", 5432)
        );
        assert_eq!(
            server(&url("postgres:///app?host=/tmp"), &host),
            Some(Server::Socket {
                path: Some("/tmp".to_owned()),
                port: Some(6543)
            })
        );
        assert_eq!(
            server(&url("postgres://%2Fvar%2Frun%2Fpostgresql/app"), &host),
            Some(Server::Socket {
                path: Some("/var/run/postgresql".to_owned()),
                port: Some(6543)
            })
        );
        // IPv6 hosts are bare, so they can be dialled.
        assert_eq!(
            server(&url("postgres://u:p@[::1]:5433/app"), &none),
            tcp("::1", 5433)
        );
        // MySQL has no environment defaults; SQLite and malformed URLs no server.
        assert_eq!(server(&url("mysql://u:p@db/app"), &host), tcp("db", 3306));
        assert_eq!(server(&url("mysql:///app"), &host), tcp("localhost", 3306));
        assert_eq!(
            server(&url("mysql://u:p@h/app?socket=/tmp/m.sock"), &none),
            Some(Server::Socket {
                path: Some("/tmp/m.sock".to_owned()),
                port: None
            })
        );
        assert_eq!(server(&url("sqlite:dev.db"), &none), None);
        assert_eq!(server(&url("postgres://a:b/c@h/app"), &none), None);
    }

    #[test]
    fn the_client_target_decodes_credentials_and_applies_the_drivers_overrides() {
        let none = environment(&[]);
        let target = client_target(
            &secret(
                "postgres://app%40corp:p%2Fw%3F%23@db.internal:5433/shop?sslmode=verify-full&\
                 sslrootcert=/etc/ca.pem&application_name=x",
            ),
            &none,
        )
        .expect("target");
        assert_eq!(target.user.as_deref(), Some("app@corp"));
        assert_eq!(
            target.password.as_ref().map(SecretString::expose),
            Some("p/w?#")
        );
        assert_eq!(target.database.as_deref(), Some("shop"));
        assert_eq!(
            target.server,
            Server::Tcp {
                host: "db.internal".to_owned(),
                port: 5433
            }
        );
        assert_eq!(target.tls.mode.as_deref(), Some("verify-full"));
        assert_eq!(target.tls.root_certificate.as_deref(), Some("/etc/ca.pem"));
        assert!(
            !format!("{target:?}").contains("p/w"),
            "the password stays redacted"
        );

        // `user`, `password` and `dbname` in the query win, as in SQLx.
        let target = client_target(
            &secret("postgres://a:b@h/shop?user=c&password=d&dbname=other"),
            &none,
        )
        .expect("target");
        assert_eq!(target.user.as_deref(), Some("c"));
        assert_eq!(
            target.password.as_ref().map(SecretString::expose),
            Some("d")
        );
        assert_eq!(target.database.as_deref(), Some("other"));

        let target = client_target(
            &secret("mariadb://root@127.0.0.1/shop?ssl-mode=REQUIRED&ssl-ca=/ca"),
            &none,
        )
        .expect("target");
        assert_eq!(target.password, None);
        assert_eq!(target.tls.mode.as_deref(), Some("REQUIRED"));
        assert_eq!(target.tls.root_certificate.as_deref(), Some("/ca"));

        for refused in [
            "sqlite:dev.db",
            "redis://h/x",
            "postgres://admin:s3cr/et@db/shop",
        ] {
            assert_eq!(client_target(&secret(refused), &none), None, "{refused}");
        }
    }

    #[test]
    fn the_sqlite_file_is_the_path_without_scheme_or_query() {
        for (url, file) in [
            (
                "sqlite://storage/dev.sqlite3?mode=rwc",
                "storage/dev.sqlite3",
            ),
            ("sqlite:dev.db", "dev.db"),
            ("sqlite:///var/lib/my%20app.db", "/var/lib/my app.db"),
            ("sqlite::memory:", ":memory:"),
        ] {
            assert_eq!(sqlite_file(&secret(url)).as_deref(), Some(file), "{url}");
        }
        assert_eq!(sqlite_file(&secret("postgres://h/app")), None);
    }

    #[test]
    fn production_needs_tls_that_verifies_the_server() {
        for verified in [
            "postgres://app:pw@db.internal/shop?sslmode=verify-full",
            "postgresql://app:pw@db.internal/shop?sslmode=verify-full&sslrootcert=/ca.pem",
            "postgres://app:pw@db.internal/shop?ssl-mode=VERIFY-FULL",
            // The driver applies the last occurrence.
            "postgres://app:pw@db.internal/shop?sslmode=disable&sslmode=verify-full",
            "mysql://app:pw@db.internal/shop?ssl-mode=VERIFY_IDENTITY",
            "mariadb://app:pw@db.internal/shop?sslmode=verify_identity&ssl-ca=/ca.pem",
            // A Unix socket has no network path; SQLite is a local file.
            "postgres:///shop?host=/var/run/postgresql",
            "postgres://%2Fvar%2Frun%2Fpostgresql/shop",
            "mysql://app:pw@localhost/shop?socket=/run/mysqld/mysqld.sock",
            "sqlite:///var/lib/app/app.db",
        ] {
            assert_eq!(
                require_verified_tls(&secret(verified)),
                Ok(()),
                "{verified}"
            );
        }
    }

    #[test]
    fn production_refuses_downgradable_tls_and_says_how_to_fix_it() {
        for (refused, contract) in [
            // SQLx's default, prefer, falls back to plaintext.
            (
                "postgres://app:hunter2@db.internal/shop",
                POSTGRES_TLS_REQUIRED,
            ),
            (
                "postgres://app:hunter2@db.internal/shop?sslmode=prefer",
                POSTGRES_TLS_REQUIRED,
            ),
            (
                "postgres://app:hunter2@db.internal/shop?sslmode=require",
                POSTGRES_TLS_REQUIRED,
            ),
            (
                "postgres://app:hunter2@db.internal/shop?sslmode=disable",
                POSTGRES_TLS_REQUIRED,
            ),
            (
                "postgres://app:hunter2@db.internal/shop?sslmode=allow",
                POSTGRES_TLS_REQUIRED,
            ),
            // SQLx's rustls verify-ca trusts the public web roots besides
            // sslrootcert and skips the host name: any public certificate passes.
            (
                "postgres://app:hunter2@db.internal/shop?sslmode=verify-ca&sslrootcert=/ca.pem",
                POSTGRES_TLS_REQUIRED,
            ),
            (
                "mysql://app:hunter2@db.internal/shop?ssl-mode=VERIFY_CA&ssl-ca=/ca.pem",
                MYSQL_TLS_REQUIRED,
            ),
            (
                "mariadb://app:hunter2@db.internal/shop?sslmode=verify_ca",
                MYSQL_TLS_REQUIRED,
            ),
            // The last occurrence wins, as in the driver.
            (
                "postgres://app:hunter2@db.internal/shop?sslmode=verify-full&ssl-mode=disable",
                POSTGRES_TLS_REQUIRED,
            ),
            // Keys are case-sensitive in the driver: this one is ignored.
            (
                "postgres://app:hunter2@db.internal/shop?SSLMODE=verify-full",
                POSTGRES_TLS_REQUIRED,
            ),
            (
                "postgres://app:hunter2@db.internal/shop?sslmode=verify-full%20",
                POSTGRES_TLS_REQUIRED,
            ),
            // No host: SQLx tries a default socket, then localhost over TCP.
            ("postgres:///shop", POSTGRES_TLS_REQUIRED),
            (
                "postgres://app:hunter2@127.0.0.1/shop",
                POSTGRES_TLS_REQUIRED,
            ),
            ("mysql://app:hunter2@db.internal/shop", MYSQL_TLS_REQUIRED),
            (
                "mysql://app:hunter2@db.internal/shop?ssl-mode=PREFERRED",
                MYSQL_TLS_REQUIRED,
            ),
            (
                "mariadb://app:hunter2@db.internal/shop?ssl-mode=REQUIRED",
                MYSQL_TLS_REQUIRED,
            ),
            // Forge-Go's spelling is not SQLx's: it would be ignored.
            (
                "mysql://app:hunter2@db.internal/shop?tls=true",
                MYSQL_TLS_REQUIRED,
            ),
            (
                "postgres://app:hunter2/x@db/shop?sslmode=verify-full",
                MALFORMED,
            ),
            ("redis://cache", MALFORMED),
        ] {
            let error = require_verified_tls(&secret(refused)).expect_err(refused);
            assert_eq!(error, contract, "{refused}");
            assert!(!error.contains("hunter2"), "{error}");
        }
        assert!(POSTGRES_TLS_REQUIRED.contains("sslmode=verify-full"));
        assert!(POSTGRES_TLS_REQUIRED.contains("FORGE_DATABASE_ALLOW_PLAINTEXT=true"));
        assert!(MYSQL_TLS_REQUIRED.contains("ssl-mode=VERIFY_IDENTITY"));
        assert!(MYSQL_TLS_REQUIRED.contains("FORGE_DATABASE_ALLOW_PLAINTEXT=true"));
    }
}
