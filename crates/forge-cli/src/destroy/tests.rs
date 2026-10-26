use super::*;

fn plan(files: Vec<Planned>) -> DestroyPlan {
    DestroyPlan {
        files,
        unwire: Unwire::Printed(
            "\nForge did not edit any existing file. Wire it:\n\n  pub mod pages;\n".to_owned(),
        ),
        owned_elsewhere: None,
    }
}

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|error| panic!("{error}"));
    }
    fs::write(path, content).unwrap_or_else(|error| panic!("{error}"));
}

fn run(root: &Path, plan: &DestroyPlan, flags: DestroyFlags) -> Result<String, String> {
    let mut output = Vec::new();
    apply(root, plan, flags, &mut output)
        .map(|()| String::from_utf8_lossy(&output).into_owned())
        .map_err(|error| error.to_string())
}

fn files() -> Vec<Planned> {
    vec![
        Planned::new("migrations/1_create_pages.up.sql", "up", Role::Migration),
        Planned::new("src/adapters/http/pages.rs", "code", Role::Owned),
        Planned::new("templates/pages/home.html", "page", Role::Owned),
        Planned::new("config/locales/pages.es.json", "{}", Role::Translation),
        Planned::new("src/adapters/http/access.rs", "shared", Role::Shared),
    ]
}

fn setup(root: &Path) {
    for file in files() {
        write(root, &file.path, &file.content);
    }
    write(root, "templates/other/keep.html", "x");
}

#[test]
fn unchanged_files_are_removed_and_migrations_and_shared_files_kept() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    setup(root);
    let printed = run(root, &plan(files()), DestroyFlags::default())
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        printed.contains("remove       src/adapters/http/pages.rs"),
        "{printed}"
    );
    assert!(printed.contains("keep         migrations/1_create_pages.up.sql"));
    assert!(printed.contains("pub mod pages;"), "{printed}");
    assert!(!printed.contains("Wire it"), "{printed}");
    assert!(!root.join("src/adapters/http/pages.rs").exists());
    assert!(
        !root.join("templates/pages").exists(),
        "empty directory removed"
    );
    assert!(root.join("templates/other/keep.html").exists());
    assert!(
        root.join("src/adapters/http").is_dir(),
        "layout directory kept"
    );
    assert!(root.join("src/adapters/http/access.rs").exists());
    assert!(root.join("migrations/1_create_pages.up.sql").exists());
    assert!(!root.join("config/locales/pages.es.json").exists());

    // Only the migration is left: says so, then removes it on request.
    let again = run(root, &plan(files()), DestroyFlags::default())
        .err()
        .unwrap_or_default();
    assert!(again.contains("--migrations"), "{again}");
    let flags = DestroyFlags {
        migrations: true,
        ..DestroyFlags::default()
    };
    run(root, &plan(files()), flags).unwrap_or_else(|error| panic!("{error}"));
    assert!(!root.join("migrations/1_create_pages.up.sql").exists());
    assert!(root.join("migrations").is_dir());
    let nothing = run(root, &plan(files()), flags).err().unwrap_or_default();
    assert!(nothing.contains("nothing to destroy"), "{nothing}");
}

#[test]
fn a_modified_file_refuses_everything_unless_forced() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    setup(root);
    write(root, "templates/pages/home.html", "edited");
    let refusal = run(root, &plan(files()), DestroyFlags::default())
        .err()
        .unwrap_or_default();
    assert!(refusal.contains("templates/pages/home.html"), "{refusal}");
    assert!(root.join("src/adapters/http/pages.rs").exists());

    let forced = DestroyFlags {
        force: true,
        ..DestroyFlags::default()
    };
    let printed = run(root, &plan(files()), forced).unwrap_or_else(|error| panic!("{error}"));
    assert!(printed.contains("--force"), "{printed}");
    assert!(!root.join("templates/pages/home.html").exists());
}

#[test]
fn a_translated_locale_file_is_kept_not_refused() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    setup(root);
    write(root, "config/locales/pages.es.json", "{\"pages\":{}}");
    let printed = run(root, &plan(files()), DestroyFlags::default())
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        printed.contains("keep         config/locales/pages.es.json  (translated"),
        "{printed}"
    );
    assert!(root.join("config/locales/pages.es.json").exists());
    assert!(!root.join("src/adapters/http/pages.rs").exists());
}

#[test]
fn dry_run_removes_nothing_and_ownership_needs_force() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    setup(root);
    let dry = DestroyFlags {
        dry_run: true,
        ..DestroyFlags::default()
    };
    let printed = run(root, &plan(files()), dry).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        printed.contains("would remove src/adapters/http/pages.rs"),
        "{printed}"
    );
    assert!(root.join("src/adapters/http/pages.rs").exists());

    let mut owned = plan(files());
    owned.owned_elsewhere = Some("the scaffold needs it".to_owned());
    let refusal = run(root, &owned, DestroyFlags::default())
        .err()
        .unwrap_or_default();
    assert!(refusal.contains("the scaffold needs it"), "{refusal}");
    assert!(root.join("src/adapters/http/pages.rs").exists());
}

#[test]
fn a_file_edited_after_the_check_is_not_removed() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    write(root, "src/page.rs", "generated");
    // Unchanged: removed.
    assert_eq!(remove_checked(root, "src/page.rs", b"generated"), Ok(()));
    assert!(!root.join("src/page.rs").exists());
    // Already gone: nothing to do.
    assert_eq!(remove_checked(root, "src/page.rs", b"generated"), Ok(()));

    // Edited between the check and the removal: kept, with or without --force
    // (the check is of the bytes that were verified, not of the flag).
    write(root, "src/page.rs", "generated");
    write(root, "src/page.rs", "my own work");
    let error = remove_checked(root, "src/page.rs", b"generated")
        .err()
        .unwrap_or_default();
    assert!(
        error.contains("changed while the command was running"),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/page.rs")).unwrap_or_default(),
        "my own work"
    );
}

#[cfg(unix)]
#[test]
fn a_file_swapped_for_a_symlink_after_the_check_is_not_removed() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    let outside = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    write(outside.path(), "secret", "generated");
    write(root, "src/page.rs", "generated");
    fs::remove_file(root.join("src/page.rs")).unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path().join("secret"), root.join("src/page.rs"))
        .unwrap_or_else(|error| panic!("{error}"));
    let error = remove_checked(root, "src/page.rs", b"generated")
        .err()
        .unwrap_or_default();
    assert!(error.contains("symbolic link"), "{error}");
    assert!(outside.path().join("secret").exists());

    // The same for a directory swapped for a link.
    fs::remove_file(root.join("src/page.rs")).unwrap_or_else(|error| panic!("{error}"));
    write(outside.path(), "page.rs", "generated");
    fs::remove_dir(root.join("src")).unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path(), root.join("src"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(remove_checked(root, "src/page.rs", b"generated").is_err());
    assert!(outside.path().join("page.rs").exists());
}

#[cfg(unix)]
#[test]
fn nothing_is_removed_through_a_symlink() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    let outside = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    write(outside.path(), "home.html", "page");
    fs::create_dir_all(root.join("templates")).unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path(), root.join("templates/pages"))
        .unwrap_or_else(|error| panic!("{error}"));
    let only = plan(vec![Planned::new(
        "templates/pages/home.html",
        "page",
        Role::Owned,
    )]);
    let refusal = run(root, &only, DestroyFlags::default())
        .err()
        .unwrap_or_default();
    assert!(refusal.contains("symbolic link"), "{refusal}");
    assert!(outside.path().join("home.html").exists());
}
