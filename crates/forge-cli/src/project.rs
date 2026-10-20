//! Finding the generated application the lifecycle commands act on.

use std::fs;
use std::path::{Path, PathBuf};

use crate::CliError;

/// A Forge application: a Cargo package that depends on `forge`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Project {
    /// The directory holding `Cargo.toml`.
    pub(crate) root: PathBuf,
    /// The package name, which `forge new` also makes the binary name.
    pub(crate) name: String,
}

impl Project {
    /// Reads `Cargo.toml` in `directory`. Refuses a directory that is not a
    /// Cargo package or does not depend on `forge`, so a lifecycle command
    /// never builds and runs some other project's binary.
    pub(crate) fn discover(directory: &Path) -> Result<Self, CliError> {
        let manifest = directory.join("Cargo.toml");
        let text = fs::read_to_string(&manifest)
            .map_err(|_| CliError::NotAnApplication("no Cargo.toml here".to_owned()))?;
        let name = package_name(&text).ok_or_else(|| {
            CliError::NotAnApplication("Cargo.toml has no [package] name".to_owned())
        })?;
        if !depends_on_forge(&text) {
            return Err(CliError::NotAnApplication(
                "this package does not depend on `forge`".to_owned(),
            ));
        }
        Ok(Self {
            root: directory.to_path_buf(),
            name,
        })
    }
}

/// `(table, key, value)` of every `key = "value"` line, with the table the
/// line sits in. A deliberately small reader for the two facts needed here;
/// it understands plain and dotted table headers and quoted values.
fn entries(manifest: &str) -> Vec<(String, String, String)> {
    let mut table = String::new();
    let mut found = Vec::new();
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if let Some(header) = line.strip_prefix('[') {
            table = header
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_owned();
        } else if let Some((key, value)) = line.split_once('=') {
            found.push((
                table.clone(),
                key.trim().to_owned(),
                value.trim().to_owned(),
            ));
        }
    }
    found
}

fn package_name(manifest: &str) -> Option<String> {
    entries(manifest)
        .into_iter()
        .find(|(table, key, _)| table == "package" && key == "name")
        .and_then(|(_, _, value)| {
            let name = value.strip_prefix('"')?.strip_suffix('"')?;
            (!name.is_empty()).then(|| name.to_owned())
        })
}

fn depends_on_forge(manifest: &str) -> bool {
    let as_key = entries(manifest).iter().any(|(table, key, _)| {
        matches!(
            table.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) && (key == "forge" || key.starts_with("forge."))
    });
    let as_table = manifest
        .lines()
        .any(|line| line.trim() == "[dependencies.forge]");
    as_key || as_table
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = r#"
[package]
name = "shop"
version = "0.1.0"

[dependencies]
forge = { git = "https://github.com/rocketinsidecorp/forge-rust", tag = "v0.1.0", features = ["views"] }
serde = "1"
"#;

    #[test]
    fn a_forge_application_is_recognized() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("Cargo.toml"), APP).expect("write");
        let project = Project::discover(directory.path()).expect("project");
        assert_eq!(project.name, "shop");
        assert_eq!(project.root, directory.path());
    }

    #[test]
    fn other_directories_and_packages_are_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let error = Project::discover(directory.path()).expect_err("empty");
        assert_eq!(
            error.to_string(),
            "no Cargo.toml here: run this inside a Forge application"
        );
        fs::write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\"]\n",
        )
        .expect("write");
        let error = Project::discover(directory.path()).expect_err("workspace");
        assert!(error.to_string().contains("no [package] name"), "{error}");
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"other\"\n[dependencies]\nserde = \"1\"\n",
        )
        .expect("write");
        let error = Project::discover(directory.path()).expect_err("no forge");
        assert!(
            error.to_string().contains("does not depend on `forge`"),
            "{error}"
        );
    }

    #[test]
    fn dependency_spellings_are_understood() {
        for manifest in [
            "[package]\nname = \"a\"\n[dependencies]\nforge = \"1\"\n",
            "[package]\nname = \"a\"\n[dependencies]\nforge = { path = \"../forge\" } # local\n",
            "[package]\nname = \"a\"\n[dependencies.forge]\npath = \"../forge\"\n",
        ] {
            assert!(depends_on_forge(manifest), "{manifest}");
        }
        for manifest in [
            "[package]\nname = \"forge\"\n[dependencies]\nserde = \"1\"\n",
            "[package]\nname = \"a\"\n[dependencies]\nforge-http = \"1\"\n",
            "[package]\nname = \"a\"\n[features]\nforge = []\n",
        ] {
            assert!(!depends_on_forge(manifest), "{manifest}");
        }
    }
}
