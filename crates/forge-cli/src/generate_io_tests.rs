//! `forge generate migration` and the template renderer on inputs that go
//! wrong. They live next to `generate.rs` instead of in it.

use std::fs;

use crate::CliError;
use crate::generate::{create_migration, render_template};

#[test]
fn a_migrations_path_that_cannot_be_listed_is_reported_and_nothing_is_created() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let migrations = directory.path().join("migrations");
    fs::write(&migrations, "not a directory").expect("file");
    match create_migration(directory.path(), "add_posts") {
        Err(CliError::ReadDirectory { path, .. }) => assert_eq!(path, migrations),
        other => panic!("expected a directory error, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(&migrations).expect("untouched"),
        "not a directory"
    );
}

#[cfg(unix)]
#[test]
fn a_dangling_migrations_link_is_not_followed_or_replaced() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let migrations = directory.path().join("migrations");
    std::os::unix::fs::symlink(directory.path().join("elsewhere"), &migrations).expect("symlink");
    match create_migration(directory.path(), "add_posts") {
        Err(CliError::CreateDirectory { path, .. }) => assert_eq!(path, migrations),
        other => panic!("expected a directory error, got {other:?}"),
    }
    assert!(!directory.path().join("elsewhere").exists());
}

#[test]
fn template_blocks_nest_and_stray_directives_are_ignored() {
    let render =
        |template: &str| render_template(template, &[("@NAME@", "shop")], |name| name == "on");
    assert_eq!(
        render("a\n%%IF_DOCKER%%\nb\n%%END_IF_DOCKER%%\nc\n"),
        "a\nc\n",
        "docker is not a condition of this renderer"
    );
    assert_eq!(
        render("%%if on%%\n%%if !on%%\nhidden\n%%end%%\nshown @NAME@\n%%end%%\n"),
        "shown shop\n"
    );
    // An unknown condition is false, and an unbalanced end changes nothing.
    assert_eq!(
        render("%%if unknown%%\nx\n%%end%%\ny\n%%end%%\nz\n"),
        "y\nz\n"
    );
    assert_eq!(render("%%END_IF_DOCKER%%\nkept\n"), "kept\n");
}
