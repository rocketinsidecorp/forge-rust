//! The development environment files, as Forge-Go reads them.
//!
//! `forge dev`, `forge test`, `forge doctor` and the lifecycle commands
//! (`migrate`, `rollback`, `seed`, `setup`, `db`, `routes`, `openapi`) read
//! `.env.development` and then `.env.local` from the application directory:
//! a later file overrides an earlier one, and a variable already set in the
//! process environment always wins. Only the `forge` CLI reads them; the
//! application binary never does, so a production process (which `forge`
//! does not start) is configured by its environment alone.
//!
//! The format is deliberately small: `KEY=VALUE` lines, blank lines and lines
//! starting with `#` ignored, an optional `export ` prefix, and one pair of
//! matching single or double quotes removed from the value. There is no
//! interpolation and no inline comment, so URLs are taken literally. A parse
//! error names the file and the line number, never the line, which may hold a
//! secret.
//!
//! `FORGE_ALLOW_DESTRUCTIVE` is never taken from a file: it confirms a
//! destructive command only from the process environment.

use std::fs;
use std::io;
use std::path::Path;

use crate::CliError;
use crate::lifecycle::ALLOW_DESTRUCTIVE;

/// The files read, in order.
pub(crate) const ENV_FILES: [&str; 2] = [".env.development", ".env.local"];

/// The variables the files add to a process environment that lacks them, in
/// first-seen order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DevelopmentEnvironment {
    added: Vec<(String, String)>,
}

impl DevelopmentEnvironment {
    /// Reads the files in `root`. `in_process` says whether the process
    /// environment sets a variable. With `default_environment`, `FORGE_ENV`
    /// is `development` when neither the process nor a file sets it, as
    /// Forge-Go's `forge dev`, `test`, `db`, `migrate` and `rollback` do.
    pub(crate) fn load(
        root: &Path,
        in_process: &dyn Fn(&str) -> bool,
        default_environment: bool,
    ) -> Result<Self, CliError> {
        let mut merged: Vec<(String, String)> = Vec::new();
        for name in ENV_FILES {
            let Some(content) = read(root, name)? else {
                continue;
            };
            let pairs =
                parse(&content).map_err(|reason| CliError::Usage(format!("{name}: {reason}")))?;
            for (key, value) in pairs {
                match merged.iter_mut().find(|(seen, _)| *seen == key) {
                    Some(entry) => entry.1 = value,
                    None => merged.push((key, value)),
                }
            }
        }
        merged.retain(|(key, _)| key != ALLOW_DESTRUCTIVE && !in_process(key));
        if default_environment
            && !in_process("FORGE_ENV")
            && !merged.iter().any(|(key, _)| key == "FORGE_ENV")
        {
            merged.push(("FORGE_ENV".to_owned(), "development".to_owned()));
        }
        Ok(Self { added: merged })
    }

    /// The variables to add to a child's environment.
    pub(crate) fn added(&self) -> &[(String, String)] {
        &self.added
    }

    /// The value the files give `key` (the process environment wins, so a
    /// key it sets is never here).
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.added
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

/// The text of `root/name`, `None` when it does not exist. The file must
/// stay inside `root`: a symbolic link that leads out of it is refused, as
/// Forge-Go's `os.Root` refuses it.
fn read(root: &Path, name: &str) -> Result<Option<String>, CliError> {
    let path = root.join(name);
    let unreadable =
        |error: io::Error| CliError::Usage(format!("could not read {name}: {}", error.kind()));
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    }
    let target = match fs::canonicalize(&path) {
        Ok(target) => target,
        // A dangling link names no file.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    let inside = fs::canonicalize(root).is_ok_and(|root| target.starts_with(root));
    if !inside {
        return Err(CliError::Usage(format!(
            "{name} is a symbolic link that leads outside the application; replace it with a \
             file"
        )));
    }
    let bytes = fs::read(&target).map_err(unreadable)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| CliError::Usage(format!("{name} is not UTF-8 text")))
}

/// Parses `KEY=VALUE` lines; see the module documentation. A later line for
/// the same key replaces the value and keeps the key's first position.
pub(crate) fn parse(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected KEY=VALUE", index + 1));
        };
        let key = key.trim();
        if !is_key(key) {
            return Err(format!("line {}: expected KEY=VALUE", index + 1));
        }
        let value = unquote(value.trim());
        match pairs.iter_mut().find(|(seen, _)| seen == key) {
            Some(entry) => entry.1 = value.to_owned(),
            None => pairs.push((key.to_owned(), value.to_owned())),
        }
    }
    Ok(pairs)
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// `value` without one pair of matching surrounding quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2
            && let Some(inner) = value
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(content: &str) -> Vec<(String, String)> {
        parse(content).expect("valid")
    }

    #[test]
    fn the_format_is_forge_gos() {
        assert_eq!(
            pairs(
                "# comment\n\nA=1\nexport B = two \nC=\"quoted value\"\nD='single'\n\
                 E=postgres://u:p@h/db?sslmode=disable#frag\nF=\"unbalanced\nG=\nA=again\n  \
                 # indented comment\nH=x=y\nI=\"\"\nJ=\"\n"
            ),
            [
                ("A", "again"),
                ("B", "two"),
                ("C", "quoted value"),
                ("D", "single"),
                ("E", "postgres://u:p@h/db?sslmode=disable#frag"),
                ("F", "\"unbalanced"),
                ("G", ""),
                ("H", "x=y"),
                ("I", ""),
                ("J", "\""),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
        assert_eq!(pairs("A=1\r\nB=2\r\n").len(), 2);
    }

    #[test]
    fn errors_name_the_line_never_its_content() {
        for (content, line) in [
            ("A=1\nsecret-without-equals\n", 2),
            ("1A=x\n", 1),
            ("A-B=x\n", 1),
            ("=value\n", 1),
            ("export\n", 1),
            ("A B=hunter2\n", 1),
        ] {
            let error = parse(content).expect_err("refused");
            assert_eq!(
                error,
                format!("line {line}: expected KEY=VALUE"),
                "{content:?}"
            );
            assert!(!error.contains("hunter2") && !error.contains("secret"));
        }
    }

    fn write(root: &Path, name: &str, content: &str) {
        fs::write(root.join(name), content).expect("write");
    }

    #[test]
    fn later_files_override_and_the_process_environment_wins() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        write(
            root,
            ".env.development",
            "FORGE_DATABASE_URL=postgres://dev\nFORGE_HTTP_ADDR=127.0.0.1:3000\n\
             FORGE_ALLOW_DESTRUCTIVE=true\nSHARED=development\n",
        );
        write(root, ".env.local", "SHARED=local\nLOCAL_ONLY=1\n");
        let process = |key: &str| key == "FORGE_HTTP_ADDR";
        let loaded = DevelopmentEnvironment::load(root, &process, true).expect("loads");
        assert_eq!(
            loaded.added(),
            [
                ("FORGE_DATABASE_URL", "postgres://dev"),
                ("SHARED", "local"),
                ("LOCAL_ONLY", "1"),
                ("FORGE_ENV", "development"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
        assert_eq!(loaded.get("SHARED"), Some("local"));
        assert_eq!(loaded.get("FORGE_HTTP_ADDR"), None, "the process wins");
        assert_eq!(
            loaded.get(ALLOW_DESTRUCTIVE),
            None,
            "a file never allows destruction"
        );

        // FORGE_ENV from a file or the process is kept; doctor adds none.
        write(root, ".env.local", "FORGE_ENV=test\n");
        let loaded = DevelopmentEnvironment::load(root, &|_| false, true).expect("loads");
        assert_eq!(loaded.get("FORGE_ENV"), Some("test"));
        let loaded =
            DevelopmentEnvironment::load(root, &|key| key == "FORGE_ENV", true).expect("loads");
        assert_eq!(loaded.get("FORGE_ENV"), None);
        fs::remove_file(root.join(".env.local")).expect("remove");
        let loaded = DevelopmentEnvironment::load(root, &|_| false, false).expect("loads");
        assert_eq!(loaded.get("FORGE_ENV"), None);
    }

    #[test]
    fn missing_files_add_nothing_and_bad_files_are_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let empty = DevelopmentEnvironment::load(root, &|_| false, false).expect("no files");
        assert!(empty.added().is_empty());

        write(root, ".env.local", "PASSWORD hunter2\n");
        let error = DevelopmentEnvironment::load(root, &|_| false, false)
            .expect_err("malformed")
            .to_string();
        assert_eq!(error, ".env.local: line 1: expected KEY=VALUE");

        fs::write(root.join(".env.local"), [b'A', b'=', 0xff]).expect("write");
        let error = DevelopmentEnvironment::load(root, &|_| false, false)
            .expect_err("not UTF-8")
            .to_string();
        assert_eq!(error, ".env.local is not UTF-8 text");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_application_is_refused() {
        let outside = tempfile::tempdir().expect("tempdir");
        write(outside.path(), "secrets", "A=1\n");
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        std::os::unix::fs::symlink(outside.path().join("secrets"), root.join(".env.local"))
            .expect("symlink");
        let error = DevelopmentEnvironment::load(root, &|_| false, false)
            .expect_err("outside")
            .to_string();
        assert!(error.contains("leads outside the application"), "{error}");

        // A link inside the application is followed; a dangling one is no file.
        fs::remove_file(root.join(".env.local")).expect("remove");
        write(root, "shared.env", "B=2\n");
        std::os::unix::fs::symlink(root.join("shared.env"), root.join(".env.local"))
            .expect("symlink");
        std::os::unix::fs::symlink(root.join("missing"), root.join(".env.development"))
            .expect("symlink");
        let loaded = DevelopmentEnvironment::load(root, &|_| false, false).expect("inside");
        assert_eq!(loaded.get("B"), Some("2"));
    }
}
