use std::path::Path;
use std::process::Command;

use crate::CliError;

pub(crate) enum CargoCommand {
    Check,
    /// `cargo test`; arguments replace the default `--all-targets
    /// --all-features`, like `forge test ARGS` replaces `./...` in Forge-Go.
    Test(Vec<String>),
    Format,
    Lint,
    /// The optimized, locked release build, of one binary when named.
    Build(Option<String>),
    GenerateLockfile,
}

impl CargoCommand {
    pub(crate) fn run(self, current_dir: &Path) -> Result<(), CliError> {
        self.run_with(current_dir, &[])
    }

    /// [`CargoCommand::run`] with `environment` added to Cargo's (the
    /// development environment files, for `forge test`).
    pub(crate) fn run_with(
        self,
        current_dir: &Path,
        environment: &[(String, String)],
    ) -> Result<(), CliError> {
        let (display_name, arguments) = self.arguments();
        let status = Command::new("cargo")
            .args(arguments)
            .current_dir(current_dir)
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .status()
            .map_err(|source| CliError::SpawnCargo {
                command: display_name,
                source,
            })?;

        if status.success() {
            Ok(())
        } else {
            Err(CliError::CargoFailed {
                command: display_name,
                code: status.code(),
            })
        }
    }

    fn arguments(self) -> (&'static str, Vec<String>) {
        let fixed = |name: &'static str, arguments: &[&str]| {
            (
                name,
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
            )
        };
        match self {
            Self::Check => fixed("check", &["check", "--all-targets", "--all-features"]),
            Self::Test(arguments) if arguments.is_empty() => {
                fixed("test", &["test", "--all-targets", "--all-features"])
            }
            Self::Test(arguments) => {
                let mut all = vec!["test".to_owned()];
                all.extend(arguments);
                ("test", all)
            }
            Self::Format => fixed("fmt", &["fmt", "--all"]),
            Self::Build(None) => fixed("build", &["build", "--release", "--locked"]),
            Self::Build(Some(name)) => {
                let (display, mut all) =
                    fixed("build", &["build", "--release", "--locked", "--bin"]);
                all.push(name);
                (display, all)
            }
            Self::GenerateLockfile => fixed("generate-lockfile", &["generate-lockfile"]),
            Self::Lint => fixed(
                "clippy",
                &[
                    "clippy",
                    "--all-targets",
                    "--all-features",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(command: CargoCommand) -> Vec<String> {
        command.arguments().1
    }

    #[test]
    fn test_runs_everything_unless_arguments_replace_the_defaults() {
        assert_eq!(
            arguments(CargoCommand::Test(Vec::new())),
            ["test", "--all-targets", "--all-features"]
        );
        assert_eq!(
            arguments(CargoCommand::Test(vec![
                "--lib".to_owned(),
                "orders".to_owned(),
                "--".to_owned(),
                "--nocapture".to_owned(),
            ])),
            ["test", "--lib", "orders", "--", "--nocapture"]
        );
    }

    #[test]
    fn build_is_the_locked_release_build_of_the_named_binary() {
        assert_eq!(
            arguments(CargoCommand::Build(None)),
            ["build", "--release", "--locked"]
        );
        assert_eq!(
            arguments(CargoCommand::Build(Some("shop".to_owned()))),
            ["build", "--release", "--locked", "--bin", "shop"]
        );
    }
}
