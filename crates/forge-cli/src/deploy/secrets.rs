//! `.forge/secrets`: dotenv-like `NAME=value` lines or `NAME=$VARIABLE`
//! passthrough from the local environment, never printed.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use super::config::{ENV_KEY, PASSTHROUGH};

/// The local secrets file; `forge deploy init` creates it 0600 and adds it
/// to `.gitignore` and `.dockerignore`.
pub(crate) const DEFAULT_SECRETS_PATH: &str = ".forge/secrets";

/// The shortest secret value replaced in output. Shorter values would
/// redact unrelated text; such values are not credentials.
const MIN_REDACT_LENGTH: usize = 4;

/// What replaces a secret value in output.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// The values of `.forge/secrets`. `Debug` shows only the names.
#[derive(Clone, Default)]
pub(crate) struct Secrets {
    values: BTreeMap<String, String>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Secrets")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Secrets {
    /// Reads the secrets file at `path`.
    ///
    /// - `NAME=value` sets a literal value; one pair of matching quotes is
    ///   removed and nothing inside is interpreted.
    /// - `NAME=$OTHER` or `NAME=${OTHER}` copies `OTHER` from the local
    ///   environment (`lookup`); an unset variable is an error.
    /// - Blank lines and lines starting with `#` are ignored; `export ` is
    ///   allowed before a name.
    ///
    /// There is deliberately no command substitution. The file must not be
    /// accessible by group or others. Errors name the line or variable,
    /// never a value. A missing file yields an empty set; [`Secrets::require`]
    /// then reports the names the configuration needs.
    pub(crate) fn load(
        path: &Path,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut secrets = Self::default();
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(secrets),
            Err(error) => {
                return Err(format!(
                    "secrets file: {}",
                    super::config::go_io_error(path, &error)
                ));
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(format!(
                    "secrets file {} is accessible by other users (mode {mode:04o}); run: chmod 600 {}",
                    path.display(),
                    path.display()
                ));
            }
        }
        let content = std::fs::read(path).map_err(|error| {
            format!("secrets file: {}", super::config::go_io_error(path, &error))
        })?;
        let content = String::from_utf8_lossy(&content);
        for (index, line) in content.split_inclusive('\n').enumerate() {
            let number = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!(
                    "{} line {number}: expected NAME=VALUE",
                    path.display()
                ));
            };
            let key = key.trim();
            if !ENV_KEY.matches(key) {
                return Err(format!(
                    "{} line {number}: expected NAME=VALUE",
                    path.display()
                ));
            }
            if secrets.values.contains_key(key) {
                return Err(format!(
                    "{} line {number}: {key} is defined twice",
                    path.display()
                ));
            }
            let mut value = value.trim().to_owned();
            let bytes = value.as_bytes();
            let quoted = bytes.len() >= 2
                && (bytes[0] == b'"' || bytes[0] == b'\'')
                && bytes[bytes.len() - 1] == bytes[0];
            if quoted {
                value = value[1..value.len() - 1].to_owned();
            } else if PASSTHROUGH.matches(&value) {
                let name = value
                    .trim_matches(|c| c == '$' || c == '{' || c == '}')
                    .to_owned();
                let Some(resolved) = lookup(&name) else {
                    return Err(format!(
                        "{} line {number}: {key} takes its value from ${name}, which is not set",
                        path.display()
                    ));
                };
                value = resolved;
            }
            if value.contains(['\r', '\n', '\0']) {
                return Err(format!(
                    "{} line {number}: the value of {key} must be a single line",
                    path.display()
                ));
            }
            secrets.values.insert(key.to_owned(), value);
        }
        Ok(secrets)
    }

    /// A set from known values (tests).
    #[cfg(test)]
    pub(crate) fn from_values<K: Into<String>, V: Into<String>>(
        values: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// A secret value.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Reports every name in `names` without a value.
    pub(crate) fn require(&self, names: &[String]) -> Result<(), String> {
        let missing: Vec<&str> = names
            .iter()
            .filter(|name| !self.values.contains_key(*name))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "secrets not defined in {DEFAULT_SECRETS_PATH}: {}",
                missing.join(", ")
            ))
        }
    }

    /// A redactor for every value of four or more characters.
    pub(crate) fn redactor(&self) -> Redactor {
        let mut values: Vec<String> = self
            .values
            .values()
            .filter(|value| value.len() >= MIN_REDACT_LENGTH)
            .cloned()
            .collect();
        // Longest first, so a secret containing another is replaced whole.
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Redactor { values }
    }
}

/// Replaces known secret values in text. `Debug` shows only the count.
#[derive(Clone, Default)]
pub(crate) struct Redactor {
    values: Vec<String>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Redactor({} values)", self.values.len())
    }
}

impl Redactor {
    /// `text` with every secret value replaced by [`REDACTED`].
    pub(crate) fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for value in &self.values {
            if text.contains(value.as_str()) {
                text = text.replace(value.as_str(), REDACTED);
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(content: &str, mode: u32) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets");
        std::fs::write(&path, content).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        (dir, path)
    }

    fn env(values: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            values
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn parses_literals_and_passthrough() {
        let (_dir, path) = write(
            "# comment\n\nPLAIN=s3cr3t-value\nexport QUOTED=\"has $HOME and spaces\"\nSINGLE='$NOT_EXPANDED'\nFROM_ENV=$LOCAL_TOKEN\nBRACED=${LOCAL_TOKEN}\nEMBEDDED=prefix$LOCAL_TOKEN\nEMPTY=\n",
            0o600,
        );
        let secrets = Secrets::load(&path, &env(&[("LOCAL_TOKEN", "tok-123456")])).expect("valid");
        for (key, value) in [
            ("PLAIN", "s3cr3t-value"),
            ("QUOTED", "has $HOME and spaces"),
            ("SINGLE", "$NOT_EXPANDED"),
            ("FROM_ENV", "tok-123456"),
            ("BRACED", "tok-123456"),
            ("EMBEDDED", "prefix$LOCAL_TOKEN"),
            ("EMPTY", ""),
        ] {
            assert_eq!(secrets.get(key), Some(value), "{key}");
        }
        let error = secrets
            .require(&["PLAIN".into(), "MISSING_ONE".into(), "MISSING_TWO".into()])
            .expect_err("missing");
        assert!(error.contains("MISSING_ONE, MISSING_TWO"), "{error}");
        assert!(!format!("{secrets:?}").contains("s3cr3t"));
    }

    #[test]
    fn fails_closed_without_echoing_values() {
        for (content, mode, values, want) in [
            ("A=topsecretvalue\n", 0o640, &[][..], "chmod 600"),
            ("A=topsecretvalue\n", 0o604, &[][..], "chmod 600"),
            ("A=$NOT_SET\n", 0o600, &[][..], "$NOT_SET, which is not set"),
            (
                "A=topsecretvalue\nA=other\n",
                0o600,
                &[][..],
                "defined twice",
            ),
            (
                "topsecretvalue\n",
                0o600,
                &[][..],
                "line 1: expected NAME=VALUE",
            ),
            (
                "A-B=topsecretvalue\n",
                0o600,
                &[][..],
                "expected NAME=VALUE",
            ),
            (
                "A=$MULTI\n",
                0o600,
                &[("MULTI", "topsecret\nvalue")][..],
                "single line",
            ),
        ] {
            let (_dir, path) = write(content, mode);
            let lookup = |name: &str| {
                values
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            };
            let error = Secrets::load(&path, &lookup).expect_err(content);
            assert!(error.contains(want), "{error}");
            assert!(
                !error.contains("topsecret"),
                "error echoes a secret: {error}"
            );
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let secrets = Secrets::load(&dir.path().join("absent"), &|_| None).expect("missing file");
        secrets.require(&[]).expect("nothing required");
    }

    #[test]
    fn redacts_longest_values_first() {
        let secrets = Secrets::from_values([
            ("URL", "postgres://u:pa55word@db/x"),
            ("PASS", "pa55word"),
            ("SHORT", "abc"),
        ]);
        assert_eq!(
            secrets
                .redactor()
                .redact("connect postgres://u:pa55word@db/x then pa55word; abc stays"),
            "connect [REDACTED] then [REDACTED]; abc stays"
        );
    }
}
