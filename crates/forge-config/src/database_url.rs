//! What may be said about a database URL without leaking its credentials:
//! the engine, the database name, the host and whether it is this machine.
//!
//! Tools that print or decide on a URL (`forge doctor`, `db drop`'s guard,
//! the application's `db` commands) use these functions instead of echoing
//! the URL or its parts. Nothing here returns a user, a password or a query
//! parameter.

use crate::{DatabaseScheme, SecretString, is_loopback_host};

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

/// Names the database for messages: engine, database and host, never a user,
/// a password or a query.
#[must_use]
pub fn describe(url: &SecretString) -> String {
    let url = url.expose();
    match DatabaseScheme::of_url(url) {
        Some(DatabaseScheme::Sqlite) => format!("sqlite database {}", sqlite_path(url)),
        Some(scheme) => {
            let engine = match scheme {
                DatabaseScheme::Postgres => "postgres",
                _ => "mysql",
            };
            let name = server_name(url).unwrap_or("?");
            match host_port(url) {
                Some(host) if !host.is_empty() => {
                    format!("{engine} database \"{name}\" on {host}")
                }
                _ => format!("{engine} database \"{name}\""),
            }
        }
        None => "database".to_owned(),
    }
}

/// Whether every server the URL names is on this machine: a SQLite file, a
/// loopback or `localhost` address, or a Unix socket.
#[must_use]
pub fn is_local(url: &SecretString) -> bool {
    let url = url.expose();
    if DatabaseScheme::of_url(url) == Some(DatabaseScheme::Sqlite) {
        return true;
    }
    let Some(authority) = authority(url) else {
        return false;
    };
    let host = host_of(authority);
    host.is_empty() || host.starts_with('/') || is_loopback_host(host)
}

/// The TCP endpoint a server URL names, with the engine's default port when
/// the URL has none; `None` for SQLite, a Unix socket or an unusable URL.
#[must_use]
pub fn endpoint(url: &SecretString) -> Option<(String, u16)> {
    let url = url.expose();
    let scheme = DatabaseScheme::of_url(url)?;
    let default_port = match scheme {
        DatabaseScheme::Postgres => 5432,
        DatabaseScheme::MySql => 3306,
        DatabaseScheme::Sqlite => return None,
    };
    let authority = authority(url)?;
    let host = host_of(authority);
    if host.is_empty() || host.starts_with('/') {
        return None;
    }
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, rest)| rest);
    let after_host = host_port.strip_prefix('[').map_or_else(
        || host_port.split_once(':').map(|(_, port)| port),
        |rest| {
            rest.split_once(']')
                .and_then(|(_, tail)| tail.strip_prefix(':'))
        },
    );
    let port = match after_host {
        None | Some("") => default_port,
        Some(port) => port.parse().ok()?,
    };
    Some((host.to_owned(), port))
}

/// The database name of a server URL when it is acceptable to create or drop:
/// 1-63 characters of `[A-Za-z0-9_$-]`, not starting with `$` or `-`, and not
/// one of the engines' system databases.
///
/// # Errors
///
/// A message that never contains the URL.
pub fn manageable_name(url: &SecretString) -> Result<&str, String> {
    let name = server_name(url.expose()).ok_or_else(|| {
        "the database name in the URL must be 1-63 letters, digits, underscores, hyphens \
         or dollar signs, and start with a letter, digit or underscore"
            .to_owned()
    })?;
    if RESERVED
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(format!("refusing to touch the \"{name}\" system database"));
    }
    Ok(name)
}

/// The part of a server URL between `://` and the first `/`, `?` or `#`.
fn authority(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// `host[:port]` without the userinfo.
fn host_port(url: &str) -> Option<&str> {
    let authority = authority(url)?;
    Some(
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, rest)| rest),
    )
}

/// The host of `authority`, without userinfo, port or IPv6 brackets.
fn host_of(authority: &str) -> &str {
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, rest)| rest);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or_default();
    }
    host_port.split(':').next().unwrap_or_default()
}

/// The database name of a server URL, when it is well formed.
fn server_name(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let path = rest.find('/').map(|slash| &rest[slash + 1..])?;
    let name = path.split(['?', '#']).next()?;
    let mut characters = name.chars();
    let first = characters.next()?;
    let acceptable = (first.is_ascii_alphanumeric() || first == '_')
        && name.len() <= 63
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '-'));
    acceptable.then_some(name)
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

    #[test]
    fn names_are_restricted_to_what_every_tool_accepts() {
        for good in ["app", "app_dev", "App-1", "_x", "a$b", &"a".repeat(63)] {
            let url = format!("postgres://u:p@h/{good}?sslmode=require");
            assert_eq!(server_name(&url), Some(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "$x",
            "a b",
            "a\"b",
            "a;b",
            "a%20b",
            &"a".repeat(64),
        ] {
            let url = format!("postgres://u:p@h/{bad}");
            assert_eq!(server_name(&url), None, "{bad:?}");
        }
        assert_eq!(server_name("postgres://u:p@h"), None);
    }

    #[test]
    fn system_databases_and_bad_names_are_refused_without_echoing_the_url() {
        for name in ["postgres", "TEMPLATE1", "template0", "mysql", "sys"] {
            let url = secret(&format!("postgres://admin:hunter2@db.internal/{name}"));
            let message = manageable_name(&url).expect_err("reserved");
            assert!(message.contains("system database"), "{message}");
            assert!(!message.contains("hunter2") && !message.contains("db.internal"));
        }
        let url = secret("postgres://admin:hunter2@db.internal/bad;name");
        let message = manageable_name(&url).expect_err("name");
        assert!(message.contains("1-63 letters"), "{message}");
        assert!(!message.contains("hunter2"));
        assert_eq!(
            manageable_name(&secret("postgres://u:p@h/shop_dev")),
            Ok("shop_dev")
        );
    }

    #[test]
    fn describe_never_includes_credentials_or_query() {
        let url = secret(
            "postgres://admin:hunter2@db.internal:5433/shop?password=hunter3&sslmode=require",
        );
        let text = describe(&url);
        assert_eq!(text, "postgres database \"shop\" on db.internal:5433");
        assert!(!text.contains("hunter"));
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

    #[test]
    fn only_this_machine_is_local() {
        for local in [
            "postgres://u:p@localhost/app",
            "postgres://u:p@LOCALHOST:5432/app",
            "postgres://u:p@127.0.0.1/app",
            "postgres://u:p@[::1]:5432/app",
            "postgres:///app?host=/var/run/postgresql",
            "mysql://u:p@127.0.0.1:3306/app",
            "sqlite:storage/dev.db",
        ] {
            assert!(is_local(&secret(local)), "{local}");
        }
        for remote in [
            "postgres://u:p@db.example.com/app",
            "postgres://u:p@10.0.0.5/app",
            "postgres://u:p@localhost.evil.test/app",
            "postgres://localhost:5432@evil.test/app",
            "mysql://u:p@[2001:db8::1]/app",
        ] {
            assert!(!is_local(&secret(remote)), "{remote}");
        }
    }

    #[test]
    fn endpoints_use_the_default_port_of_the_engine() {
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
            endpoint("mysql://u:p@127.0.0.1/app"),
            Some(("127.0.0.1".to_owned(), 3306))
        );
        assert_eq!(
            endpoint("mariadb://u:p@[::1]:3307/app"),
            Some(("::1".to_owned(), 3307))
        );
        assert_eq!(
            endpoint("postgres://u:p@[::1]/app"),
            Some(("::1".to_owned(), 5432))
        );
        assert_eq!(endpoint("postgres:///app?host=/var/run/postgresql"), None);
        assert_eq!(endpoint("sqlite:dev.db"), None);
        assert_eq!(endpoint("postgres://u:p@h:notaport/app"), None);
        assert_eq!(endpoint("redis://h/app"), None);
    }
}
