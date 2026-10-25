//! What may be said about a database URL without leaking its credentials:
//! the engine, the database name and the host.
//!
//! Tools that print or decide on a URL (`forge doctor`, the application's `db`
//! commands) use these functions instead of echoing the URL or its parts. The
//! URL is read with the parser SQLx uses (`url`), applying the same query
//! overrides the driver applies (`host`, `hostaddr`, `port` and `dbname` for
//! PostgreSQL, `socket` for MySQL/MariaDB), so what is described is what the
//! driver would connect to. Nothing here returns a user, a password or another
//! query parameter.
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
     in the user name and password";

/// Where a server URL points, after the driver's query overrides.
#[derive(Debug, Eq, PartialEq)]
struct Location {
    host: Option<String>,
    port: Option<u16>,
    socket: bool,
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
            let host = if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            format!("{engine} database {name} on {host}:{port}")
        }
        (Some(host), None) => format!("{engine} database {name} on {host}"),
        _ => format!("{engine} database {name}"),
    }
}

/// The TCP endpoint a server URL names, with the engine's default port when
/// the URL has none; `None` for SQLite, a Unix socket or a URL that is not
/// well formed.
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
        database: None,
    };
    match url.host() {
        Some(Host::Domain(domain)) => {
            if scheme == DatabaseScheme::Postgres && domain.to_ascii_lowercase().starts_with("%2f")
            {
                found.socket = true;
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
            (DatabaseScheme::Postgres, "host") if value.starts_with('/') => found.socket = true,
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
            (DatabaseScheme::MySql, "socket") => found.socket = true,
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

/// The file of a `sqlite:` URL, without scheme or query.
fn sqlite_path(url: &str) -> &str {
    let rest = url.split_once(':').map_or(url, |(_, rest)| rest);
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    rest.split('?').next().unwrap_or(rest)
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
}
