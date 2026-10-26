//! `migrate.env`: the schema owner's credential reaches the one-off migrate
//! container alone, through its own 0600 file that is removed when the step
//! ends. Mirrors Forge-Go's `migrate_env_test.go`: the file's content and
//! placement, upload/run/remove ordering, removal on every way out of the
//! step, the dry-run plan, and the configuration rules (never empty or
//! null, never in another env block, listed among the required secrets).

use super::commands::Commands;
use super::config::Config;
use super::errors::EXIT_MIGRATION_FAILED;
use super::init::{CONFIG_TEMPLATE, SECRETS_TEMPLATE};
use super::runner::{Cancel, RunError};
use super::secrets::Secrets;
use super::tests::{
    Fixture, PLAN_SECRETS, VALID_CONFIG, check_golden, parse, plan_fixture_for, remote,
};
use crate::process::Shutdown;

/// The schema owner's credential of the fixtures.
const MIGRATION_SECRET: &str = "postgres://shop_migrator:migration-secret-4@db/shop";
const MIGRATION_NAME: &str = "FORGE_MIGRATION_DATABASE_URL";

/// What deletes the migrate env file and an interrupted upload's temporary
/// copy.
const REMOVE_SCRIPT: &str =
    "rm -f .forge/shop/env/migrate.step.env .forge/shop/env/migrate.step.env.tmp";

/// `VALID_CONFIG` with the migrate step's own credential.
fn migrate_env_config() -> String {
    format!("{VALID_CONFIG}migrate:\n  env:\n    secret: [FORGE_MIGRATION_DATABASE_URL]\n")
}

fn fixture(dry_run: bool, config: &str) -> Fixture {
    plan_fixture_for(dry_run, config, &[(MIGRATION_NAME, MIGRATION_SECRET)])
}

fn error_of(config: &str) -> String {
    match Config::parse(config.as_bytes()) {
        Ok(_) => panic!("configuration should be refused:\n{config}"),
        Err(error) => error,
    }
}

fn removed(calls: &[String]) -> bool {
    calls
        .iter()
        .any(|call| *call == format!("192.0.2.10: {REMOVE_SCRIPT}"))
}

fn assert_clean(error: &str) {
    assert!(
        !error.contains(MIGRATION_SECRET) && !error.contains("migration-secret-4"),
        "the error leaks the credential: {error}"
    );
}

#[test]
fn migrate_env_reaches_only_the_migrate_container() {
    let fixture = fixture(false, &migrate_env_config());
    let (_, result) = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    result.expect("deploy");

    let mut migrate_uploads = Vec::new();
    for upload in fixture.runner.uploads() {
        if upload.script.contains("migrate.step.env") {
            migrate_uploads.push(upload);
        } else if upload.script.contains("/env/web.env") || upload.script.contains("/env/jobs.env")
        {
            assert!(
                !upload.stdin.contains(MIGRATION_SECRET) && !upload.stdin.contains(MIGRATION_NAME),
                "serving env file leaks the migration credential: {}",
                upload.script
            );
        }
    }
    assert_eq!(migrate_uploads.len(), 1, "{migrate_uploads:?}");
    let upload = &migrate_uploads[0];
    assert_eq!(
        upload.host, "192.0.2.10",
        "the migrate host only, not every host of the role"
    );
    assert_eq!(
        upload.script,
        "umask 077 && mkdir -p .forge/shop/env && cat > .forge/shop/env/migrate.step.env.tmp && mv .forge/shop/env/migrate.step.env.tmp .forge/shop/env/migrate.step.env"
    );
    // The role's own environment plus the credential, in one file.
    assert_eq!(
        upload.stdin,
        format!(
            "FORGE_ENV=production\nFORGE_DATABASE_URL=postgres://shop:db-secret-2@db/shop\n{MIGRATION_NAME}={MIGRATION_SECRET}\n"
        )
    );

    let calls = fixture.runner.calls();
    let (mut up, mut run, mut rm) = (None, None, None);
    for (index, call) in calls.iter().enumerate() {
        if call.contains("migrate.step.env") && call.contains("cat >") {
            up = Some(index);
        } else if call.contains("docker run --rm") && call.contains("migrate.step.env") {
            run = Some(index);
            assert!(
                call.starts_with("192.0.2.10: ")
                    && !call.contains("web.env")
                    && call.matches("--env-file").count() == 1,
                "migrate command = {call}"
            );
        } else if call.ends_with(&format!(": {REMOVE_SCRIPT}")) {
            rm = Some(index);
        } else {
            assert!(
                !call.contains("migrate.step.env"),
                "another command uses the migrate env file: {call}"
            );
        }
        assert!(
            !call.contains(MIGRATION_SECRET) && !call.contains("migration-secret-4"),
            "a command line carries the credential: {call}"
        );
        assert!(
            !(call.contains("docker run --detach") && call.contains("migrate")),
            "a serving container gets migrate settings: {call}"
        );
    }
    match (up, run, rm) {
        (Some(up), Some(run), Some(rm)) => {
            assert!(
                up < run && run < rm,
                "want upload ({up}) < migrate ({run}) < remove ({rm})"
            );
        }
        other => panic!("missing upload, migrate or remove: {other:?}"),
    }
}

#[test]
fn migrate_env_is_removed_when_the_migration_fails() {
    let fixture = fixture(false, &migrate_env_config());
    fixture.runner.fail_with(|host, command| {
        command.script.contains("docker run --rm").then(|| {
            remote(
                host,
                1,
                &format!("migration 0002 failed using {MIGRATION_SECRET}"),
            )
        })
    });
    let (report, result) = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    let error = result.expect_err("migration fails");
    assert_eq!(error.exit_code(), EXIT_MIGRATION_FAILED, "{error}");
    assert_clean(&error.to_string());
    assert!(report.expect("report").warnings.is_empty());
    let calls = fixture.runner.calls();
    assert!(
        removed(&calls),
        "the credential file was left behind: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|call| !call.contains("docker run --detach")),
        "a container was started after a failed migration: {calls:?}"
    );
}

#[test]
fn migrate_env_removal_failure_is_a_warning() {
    let fixture = fixture(false, &migrate_env_config());
    fixture.runner.fail_with(|_, command| {
        (command.script == REMOVE_SCRIPT)
            .then(|| RunError::Other("ssh: connection reset".to_owned()))
    });
    let (report, result) = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    result.expect("a failed removal does not fail the deploy");
    let warnings = report.expect("report").warnings;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(
        warnings[0],
        "192.0.2.10: could not remove .forge/shop/env/migrate.step.env, delete it by hand: ssh: connection reset"
    );
}

#[test]
fn migrate_env_upload_failure_aborts_before_migrating_and_still_removes_the_file() {
    let fixture = fixture(false, &migrate_env_config());
    fixture.runner.fail_with(|_, command| {
        // The remote command may already have run when the error arrives
        // (Ctrl-C, a dropped connection).
        command
            .script
            .contains("cat > .forge/shop/env/migrate.step.env.tmp")
            .then(|| RunError::Other("disk full".to_owned()))
    });
    let (_, result) = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    let error = result.expect_err("upload fails");
    assert_eq!(error.exit_code(), EXIT_MIGRATION_FAILED, "{error}");
    assert!(error.message.contains("disk full"), "{error}");
    assert!(error.message.starts_with("migration env file"), "{error}");
    let calls = fixture.runner.calls();
    assert!(
        calls.iter().all(|call| !call.contains("docker run")),
        "a container ran after the upload failed: {calls:?}"
    );
    assert!(
        removed(&calls),
        "the interrupted upload's files were not removed on the migrate host: {calls:?}"
    );
}

#[test]
fn migrate_env_is_removed_when_the_deploy_is_cancelled_during_the_migration() {
    let fixture = fixture(false, &migrate_env_config());
    // Like the SSH runner, refuse to run anything once cancelled: the
    // removal can only happen if it ignores the cancellation.
    fixture.runner.refuse_cancelled();
    let shutdown = Shutdown::new();
    let trigger = shutdown.clone();
    fixture.runner.fail_with(move |host, command| {
        if command.script.contains("docker run --rm") {
            trigger.request();
            return Some(RunError::Other(format!("{host}: cancelled")));
        }
        None
    });
    let (_, result) = fixture
        .deployer
        .deploy(&Cancel::signal(shutdown), "v3", true);
    let error = result.expect_err("cancelled");
    assert_eq!(error.exit_code(), EXIT_MIGRATION_FAILED, "{error}");
    let calls = fixture.runner.calls();
    assert!(
        removed(&calls),
        "the file was not removed after cancellation: {calls:?}"
    );
}

#[test]
fn a_missing_migrate_secret_fails_closed_before_any_remote_command() {
    let mut fixture = fixture(false, &migrate_env_config());
    fixture.deployer.secrets = Secrets::from_values(PLAN_SECRETS);
    let (report, result) = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    assert!(report.is_none(), "the operation never started");
    let error = result.expect_err("missing secret");
    assert!(
        error.message.contains(&format!(
            "secrets not defined in .forge/secrets: {MIGRATION_NAME}"
        )),
        "{error}"
    );
    assert!(
        fixture.runner.calls().is_empty(),
        "commands ran before the secret check: {:?}",
        fixture.runner.calls()
    );
}

#[test]
fn dry_run_with_migrate_env_shows_no_value() {
    let fixture = fixture(true, &migrate_env_config());
    let (_, result) = fixture.deployer.deploy(&Cancel::never(), "v3", false);
    result.expect("dry-run deploy");
    drop(fixture.deployer);
    let plan = fixture.log.text();
    check_golden("deploy-migrate-env", &plan);
    assert!(
        !plan.contains(MIGRATION_SECRET) && !plan.contains("migration-secret-4"),
        "the plan leaks the credential"
    );
    for upload in fixture.runner.uploads() {
        assert!(
            !upload.script.contains("migrate.step.env"),
            "dry run uploaded the migrate env file: {upload:?}"
        );
    }
}

#[test]
fn a_deploy_without_migrate_env_keeps_the_role_env_file() {
    let fixture = fixture(false, VALID_CONFIG);
    let (_, result) = fixture.deployer.deploy(&Cancel::never(), "v3", true);
    result.expect("deploy");
    let mut migrate = Vec::new();
    for call in fixture.runner.calls() {
        assert!(
            !call.contains("migrate.step.env") && !call.contains(": rm -f .forge/shop/env/"),
            "a deploy without migrate.env touches {call}"
        );
        if call.contains("docker run --rm") {
            migrate.push(call);
        }
    }
    assert_eq!(migrate.len(), 1, "{migrate:?}");
    assert!(
        migrate[0].contains("--env-file .forge/shop/env/web.env "),
        "{migrate:?}"
    );
}

#[test]
fn migrate_env_with_a_disabled_migration_is_rejected() {
    let error = error_of(&format!(
        "{VALID_CONFIG}migrate:\n  enabled: false\n  env:\n    secret: [X]\n"
    ));
    assert!(
        error.contains("migrate.env is set but migrate.enabled is false, so nothing would use it"),
        "{error}"
    );
}

#[test]
fn migrate_env_adds_to_the_migrate_role_environment() {
    let config = parse(&format!(
        "{VALID_CONFIG}migrate:\n  role: jobs\n  env:\n    clear:\n      MIGRATION_LOCK_TIMEOUT: 5m\n    secret: [FORGE_MIGRATION_DATABASE_URL]\n"
    ));
    let commands = Commands::new(config);
    let (clear, secret) = commands.migrate_env();
    // The global env and the migrate role's env, plus the overlay.
    assert_eq!(clear.len(), 3, "{clear:?}");
    assert_eq!(clear["WORKERS"], "4");
    assert_eq!(clear["FORGE_ENV"], "production");
    assert_eq!(clear["MIGRATION_LOCK_TIMEOUT"], "5m");
    assert_eq!(secret, ["FORGE_DATABASE_URL", MIGRATION_NAME]);
    // The serving roles are untouched.
    let (clear, secret) = commands.role_env("web");
    assert_eq!(clear.len(), 1, "{clear:?}");
    assert_eq!(clear["FORGE_ENV"], "production");
    assert_eq!(secret, ["FORGE_DATABASE_URL"]);
}

#[test]
fn migrate_env_names_may_not_appear_in_any_other_env_block() {
    let base = migrate_env_config();
    let swap = |old: &str, new: &str| {
        assert!(base.contains(old), "fixture lacks {old:?}");
        base.replacen(old, new, 1)
    };
    let cases = [
        (
            "global secret",
            swap(
                "secret: [FORGE_DATABASE_URL]",
                "secret: [FORGE_DATABASE_URL, FORGE_MIGRATION_DATABASE_URL]",
            ),
            "FORGE_MIGRATION_DATABASE_URL is in migrate.env and in env.secret: remove it from env.secret, so only the migrate step receives it",
        ),
        (
            "global clear",
            swap(
                "FORGE_ENV: production",
                "FORGE_ENV: production\n    FORGE_MIGRATION_DATABASE_URL: x",
            ),
            "FORGE_MIGRATION_DATABASE_URL is in migrate.env and in env.clear: remove it from env.clear, so only the migrate step receives it",
        ),
        (
            "role secret",
            swap(
                "      clear:\n        WORKERS",
                "      secret: [FORGE_MIGRATION_DATABASE_URL]\n      clear:\n        WORKERS",
            ),
            "in migrate.env and in servers.jobs.env.secret",
        ),
        (
            "role clear",
            swap(
                "WORKERS: \"4\"",
                "WORKERS: \"4\"\n        FORGE_MIGRATION_DATABASE_URL: x",
            ),
            "in migrate.env and in servers.jobs.env.clear",
        ),
        (
            "clear in migrate and global",
            format!("{VALID_CONFIG}migrate:\n  env:\n    clear: {{FORGE_ENV: migration}}\n"),
            "FORGE_ENV is in migrate.env and in env.clear",
        ),
        (
            "web role, not the migrate role",
            swap(
                "    hosts: [192.0.2.10, 192.0.2.11]",
                "    hosts: [192.0.2.10, 192.0.2.11]\n    env: {secret: [FORGE_MIGRATION_DATABASE_URL]}",
            ),
            "in migrate.env and in servers.web.env.secret",
        ),
    ];
    for (name, config, want) in cases {
        let error = error_of(&config);
        assert!(error.contains(want), "{name}: {error}, want {want:?}");
    }
    // Accessories keep their own env and are not checked.
    let accessory = swap(
        "      secret: [POSTGRES_PASSWORD]",
        "      secret: [POSTGRES_PASSWORD, FORGE_MIGRATION_DATABASE_URL]",
    );
    parse(&accessory);
}

#[test]
fn an_explicitly_null_migrate_env_is_rejected() {
    for (name, block) in [
        ("empty value", "migrate:\n  env:\n"),
        ("tilde", "migrate:\n  env: ~\n"),
        ("null", "migrate:\n  env: null\n"),
        ("flow null", "migrate: {enabled: true, env: null}\n"),
        ("capital", "migrate:\n  env: Null\n"),
    ] {
        let error = error_of(&format!("{VALID_CONFIG}{block}"));
        assert!(
            error.contains("migrate.env must set at least one clear value or secret (remove the block to migrate with the role's env file)"),
            "{name}: {error}"
        );
    }
    // A migrate block without env, or with other null fields, is still valid.
    for block in ["migrate:\n  enabled: true\n", "migrate:\n  role:\n"] {
        let config = parse(&format!("{VALID_CONFIG}{block}"));
        assert!(config.migrate.env.is_none(), "{block:?}");
    }
}

#[test]
fn migrate_env_secrets_are_required_and_listed() {
    let config = parse(&migrate_env_config());
    assert!(config.secret_names().contains(&MIGRATION_NAME.to_owned()));
    let config = parse(VALID_CONFIG);
    assert!(config.migrate.env.is_none());
    assert!(!config.secret_names().contains(&MIGRATION_NAME.to_owned()));
}

#[test]
fn the_init_template_keeps_the_migration_credential_out_of_the_serving_containers() {
    let config = parse(&CONFIG_TEMPLATE.replace("SERVICE", "shop"));
    let overlay = config
        .migrate
        .env
        .as_ref()
        .expect("template sets migrate.env");
    assert_eq!(overlay.secret, [MIGRATION_NAME]);
    assert!(overlay.clear.is_empty());
    assert!(
        !config.env.secret.iter().any(|name| name == MIGRATION_NAME),
        "the global env lists the migration credential: {:?}",
        config.env.secret
    );
    for role in config.servers.values() {
        assert!(!role.env.secret.iter().any(|name| name == MIGRATION_NAME));
    }
    // The credential is still a required secret, so a missing one stops the
    // deploy before any remote command.
    assert!(SECRETS_TEMPLATE.contains(&format!("\n{MIGRATION_NAME}=${MIGRATION_NAME}\n")));
    assert!(config.secret_names().contains(&MIGRATION_NAME.to_owned()));
}

#[test]
fn exec_keeps_the_role_environment() {
    let commands = Commands::new(parse(&migrate_env_config()));
    let script = commands.exec_one_off("web", &["migrate".to_owned(), "status".to_owned()], false);
    assert!(
        script.contains("--env-file .forge/shop/env/web.env")
            && !script.contains("migrate.step.env"),
        "{script}"
    );
}
