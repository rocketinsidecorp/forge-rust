//! `forge deploy`, `forge logs` and `forge app`: container deployment to
//! hosts over SSH with zero-downtime switches through forge-proxy (ADR
//! 0042, the port of Forge-Go's ADR 0030).
//!
//! Every remote action is a [`commands::RemoteCommand`] executed by a
//! [`runner::Runner`]. The production runner is the system `ssh` client;
//! tests use a recording fake or a real `sshd` with a docker stand-in, and
//! `--dry-run` prints commands instead of running them. The configuration
//! schema, commands, flags, messages and exit codes are Forge-Go's, so a
//! project can switch frameworks without rewriting `config/deploy.yml`.

pub(crate) mod cli;
pub(crate) mod commands;
pub(crate) mod config;
pub(crate) mod deployer;
pub(crate) mod errors;
pub(crate) mod init;
#[cfg(all(test, unix))]
mod lock_tests;
pub(crate) mod operations;
pub(crate) mod remote;
pub(crate) mod runner;
pub(crate) mod secrets;
#[cfg(all(test, unix))]
mod ssh_tests;
#[cfg(test)]
mod tests;
mod yaml;

pub(crate) use errors::DeployError;
