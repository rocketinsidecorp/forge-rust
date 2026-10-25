//! `forge new` and `forge generate migration` on directories that cannot take
//! them. They live next to `generate.rs` instead of in it.

use std::fs;

use crate::CliError;
use crate::generate::{
    Database, ForgeSource, Locale, NewApplication, create_application, create_migration,
};

fn options(name: &str) -> NewApplication {
    NewApplication {
        name: name.to_owned(),
        docker: false,
        ci: false,
        database: Database::Disabled,
        views: false,
        forge: ForgeSource::GitTag,
        locale: Locale::En,
    }
}

#[test]
fn an_application_needs_a_destination_directory_that_exists() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("no/such/parent");
    match create_application(&missing, &options("shop")) {
        Err(CliError::CreateDirectory { path, .. }) => assert_eq!(path, missing.join("shop")),
        other => panic!("expected a directory error, got {other:?}"),
    }
    assert!(!missing.exists());
}

#[test]
fn application_names_are_at_most_64_characters() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let long = "a".repeat(65);
    match create_application(directory.path(), &options(&long)) {
        Err(CliError::InvalidApplicationName { reason, .. }) => {
            assert_eq!(reason, "use at most 64 ASCII characters");
        }
        other => panic!("expected a name error, got {other:?}"),
    }
    assert!(create_application(directory.path(), &options(&"a".repeat(64))).is_ok());
}

#[test]
fn migration_names_collapse_repeated_separators() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (up, down) = create_migration(directory.path(), "add__posts---table").expect("migration");
    let name = |path: &std::path::Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    assert!(
        name(&up).ends_with("_add_posts_table.up.sql"),
        "{}",
        name(&up)
    );
    assert!(
        name(&down).ends_with("_add_posts_table.down.sql"),
        "{}",
        name(&down)
    );
}

#[test]
fn a_migrations_path_that_is_not_a_directory_is_an_error_naming_it() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("migrations"), "not a directory").expect("file");
    match create_migration(directory.path(), "add_posts") {
        Err(CliError::ReadDirectory { path, .. }) => {
            assert_eq!(path, directory.path().join("migrations"));
        }
        other => panic!("expected a read error, got {other:?}"),
    }
}
