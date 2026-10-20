//! Exercises the `forge` executable as a subprocess.

use std::path::Path;
use std::process::{Command, Output};

fn forge(current_dir: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(arguments)
        .current_dir(current_dir)
        .output()
        .expect("forge executable should start")
}

#[test]
fn new_generates_a_dockerized_application() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let output = forge(
        directory.path(),
        &["new", "shop", "--skip-lockfile", "--forge-path", "../forge"],
    );

    assert!(output.status.success(), "{output:?}");
    let root = directory.path().join("shop");
    for path in [
        "Dockerfile",
        ".dockerignore",
        "compose.yaml",
        ".gitattributes",
        "build.rs",
        "migrations/.gitkeep",
        ".github/workflows/ci.yml",
    ] {
        assert!(root.join(path).is_file(), "{path} should be generated");
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("manifest");
    assert!(manifest.contains("sqlx"));
    let compose = std::fs::read_to_string(root.join("compose.yaml")).expect("compose");
    assert!(compose.contains("postgres:18.6-trixie@sha256:"));
    assert!(compose.contains("command: [\"migrate\"]"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Created Forge application `shop`"));
}

#[test]
fn new_refuses_existing_destination_and_invalid_names() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    std::fs::create_dir(directory.path().join("shop")).expect("fixture directory");

    let existing = forge(directory.path(), &["new", "shop", "--skip-lockfile"]);
    let traversal = forge(directory.path(), &["new", "../escape", "--skip-lockfile"]);

    assert!(!existing.status.success());
    assert!(String::from_utf8_lossy(&existing.stderr).contains("refusing to overwrite"));
    assert!(!traversal.status.success());
    assert!(!directory.path().join("../escape").exists());
}

#[test]
fn skip_docker_omits_container_assets() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let output = forge(
        directory.path(),
        &["new", "api", "--skip-docker", "--skip-lockfile"],
    );

    assert!(output.status.success(), "{output:?}");
    assert!(!directory.path().join("api/Dockerfile").exists());
    assert!(directory.path().join("api/Cargo.toml").is_file());
}

#[test]
fn database_none_omits_postgres_and_migration_assets() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let output = forge(
        directory.path(),
        &["new", "api", "-d", "none", "--api", "--skip-lockfile"],
    );

    assert!(output.status.success(), "{output:?}");
    let root = directory.path().join("api");
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("manifest");
    let compose = std::fs::read_to_string(root.join("compose.yaml")).expect("compose");
    assert!(!manifest.contains("sqlx"));
    assert!(!compose.contains("postgres:"));
    assert!(!root.join("migrations").exists());
    assert!(!root.join("build.rs").exists());
}

#[test]
fn new_is_full_stack_by_default_and_api_opts_out() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let full_stack = forge(
        directory.path(),
        &["new", "shop", "-d", "none", "--skip-lockfile"],
    );
    let api = forge(
        directory.path(),
        &["new", "service", "-d", "none", "--api", "--skip-lockfile"],
    );

    assert!(full_stack.status.success(), "{full_stack:?}");
    assert!(api.status.success(), "{api:?}");
    let shop = directory.path().join("shop");
    for path in [
        "build.rs",
        "templates/layouts/application.html",
        "templates/welcome/index.html",
        "assets/application.css",
        "assets/application.js",
        "assets/forge-rust-logo.png",
        "src/adapters/http/views/mod.rs",
        "tests/http.rs",
    ] {
        assert!(shop.join(path).is_file(), "{path} should be generated");
    }
    let manifest = std::fs::read_to_string(shop.join("Cargo.toml")).expect("manifest");
    assert!(manifest.contains("askama"));
    assert!(manifest.contains("features = [\"views\"]"));

    let service = directory.path().join("service");
    for path in [
        "build.rs",
        "templates",
        "assets",
        "src/adapters/http/views",
        "tests/http.rs",
    ] {
        assert!(
            !service.join(path).exists(),
            "{path} must not be generated with --api"
        );
    }
    let manifest = std::fs::read_to_string(service.join("Cargo.toml")).expect("manifest");
    assert!(!manifest.contains("askama"));
    assert!(!manifest.contains("views"));
}

#[test]
fn generate_migration_creates_reversible_pair() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let output = forge(
        directory.path(),
        &["generate", "migration", "Create Customers"],
    );

    assert!(output.status.success(), "{output:?}");
    let migrations = std::fs::read_dir(directory.path().join("migrations"))
        .expect("migrations directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();

    assert_eq!(migrations.len(), 2);
    assert!(
        migrations
            .iter()
            .any(|name| name.ends_with("_create_customers.up.sql"))
    );
    assert!(
        migrations
            .iter()
            .any(|name| name.ends_with("_create_customers.down.sql"))
    );
}

#[test]
fn id_prints_a_uuid_version_seven() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let output = forge(directory.path(), &["id"]);

    assert!(output.status.success());
    let id = String::from_utf8_lossy(&output.stdout);
    let id = id.trim();
    assert_eq!(id.len(), 36);
    assert_eq!(id.as_bytes()[14], b'7', "version nibble of {id}");
}

#[test]
fn database_flag_accepts_aliases_and_prints_the_notice_for_non_postgres_backends() {
    for (value, expected_feature, notice) in [
        ("postgres", None, false),
        ("postgresql", None, false),
        ("mysql", Some("mysql"), true),
        ("mariadb", Some("mysql"), true),
        ("sqlite", Some("sqlite"), true),
        ("sqlite3", Some("sqlite"), true),
    ] {
        let directory = tempfile::tempdir().expect("temporary directory should be available");

        let output = forge(
            directory.path(),
            &[
                "new",
                "api",
                "--database",
                value,
                "--api",
                "--skip-lockfile",
            ],
        );

        assert!(output.status.success(), "{value}: {output:?}");
        let manifest =
            std::fs::read_to_string(directory.path().join("api/Cargo.toml")).expect("manifest");
        match expected_feature {
            Some(feature) => assert!(manifest.contains(&format!("features = [\"{feature}\"]"))),
            None => assert!(manifest.contains("\"postgres\"")),
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.contains("PostgreSQL-only modules are not available"),
            notice,
            "{value}"
        );
    }
}

#[test]
fn database_flag_short_form_and_invalid_values() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let short = forge(
        directory.path(),
        &["new", "api", "-d", "sqlite3", "--skip-lockfile"],
    );
    let invalid = forge(
        directory.path(),
        &["new", "other", "-d", "oracle", "--skip-lockfile"],
    );

    assert!(short.status.success(), "{short:?}");
    assert!(directory.path().join("api/storage/.gitkeep").is_file());
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid value 'oracle'"));
    assert!(!directory.path().join("other").exists());
}

#[test]
fn skip_database_flag_is_replaced_by_database_none() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");

    let output = forge(
        directory.path(),
        &["new", "api", "--skip-database", "--skip-lockfile"],
    );

    assert!(!output.status.success());
    assert!(!directory.path().join("api").exists());
}

#[test]
fn generate_scaffold_dry_run_lists_files_and_requires_a_scope() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let created = forge(
        directory.path(),
        &[
            "new",
            "shop",
            "--skip-lockfile",
            "--skip-docker",
            "--forge-path",
            "../forge",
        ],
    );
    assert!(created.status.success(), "{created:?}");
    let root = directory.path().join("shop");

    let without_scope = forge(&root, &["generate", "scaffold", "post", "title:string"]);
    assert!(!without_scope.status.success());
    assert!(String::from_utf8_lossy(&without_scope.stderr).contains("--tenant-scoped"));

    let both = forge(
        &root,
        &[
            "generate",
            "scaffold",
            "post",
            "title:string",
            "--global",
            "--tenant-scoped",
        ],
    );
    assert!(!both.status.success());

    let dry_run = forge(
        &root,
        &[
            "generate",
            "scaffold",
            "post",
            "title:string",
            "--global",
            "--dry-run",
        ],
    );
    assert!(dry_run.status.success(), "{dry_run:?}");
    let stdout = String::from_utf8_lossy(&dry_run.stdout);
    assert!(
        stdout.contains("would create src/domain/entities/post.rs"),
        "{stdout}"
    );
    assert!(stdout.contains("pub mod post_repository;"), "{stdout}");
    assert!(!root.join("src/domain/entities/post.rs").exists());

    let invalid = forge(
        &root,
        &["generate", "scaffold", "post", "price:float", "--global"],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("no float type"));
}

#[test]
fn generate_scaffold_refuses_api_and_database_free_applications() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    for (name, flags) in [("api", &["--api"][..]), ("plain", &["-d", "none"][..])] {
        let mut arguments = vec!["new", name, "--skip-lockfile", "--skip-docker"];
        arguments.extend_from_slice(flags);
        assert!(forge(directory.path(), &arguments).status.success());

        let refused = forge(
            &directory.path().join(name),
            &["generate", "scaffold", "post", "title:string", "--global"],
        );

        assert!(!refused.status.success(), "{name}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains("cannot generate the scaffold"), "{stderr}");
    }
}
