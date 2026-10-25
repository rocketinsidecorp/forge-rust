use super::*;

#[test]
fn snake_case_names_are_bounded_and_not_keywords() {
    for good in ["pages", "about_us", "a1", "send_welcome"] {
        assert_eq!(snake_case("name", good, MAX_NAME_LENGTH), Ok(()), "{good}");
    }
    for bad in [
        "", "Pages", "1a", "a__b", "a_", "a-b", "a.b", "a/b", "fn", "type", "café",
    ] {
        assert!(snake_case("name", bad, MAX_NAME_LENGTH).is_err(), "{bad}");
    }
    assert!(snake_case("name", &"a".repeat(41), MAX_NAME_LENGTH).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_directories_and_files_are_refused() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    let outside = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    fs::create_dir_all(root.join("src")).unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path(), root.join("templates"))
        .unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path().join("x"), root.join("src/a.rs"))
        .unwrap_or_else(|error| panic!("{error}"));
    fs::create_dir_all(root.join("tests/dir.rs")).unwrap_or_else(|error| panic!("{error}"));

    assert!(refuse_symlinks(root, ["templates/pages/home.html"]).is_err());
    assert!(refuse_symlinks(root, ["src/a.rs"]).is_err());
    assert!(refuse_symlinks(root, ["tests/dir.rs"]).is_err());
    assert!(refuse_symlinks(root, ["src/b.rs", "new/dir/file.rs"]).is_ok());
    assert!(refuse_symlinks(root, ["../escape.rs"]).is_err());
}

#[test]
fn permission_owners_are_found_by_string_literal() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    fs::create_dir_all(root.join("src/a")).unwrap_or_else(|error| panic!("{error}"));
    fs::write(root.join("src/a/b.rs"), "const P: &str = \"posts:read\";")
        .unwrap_or_else(|error| panic!("{error}"));
    fs::write(root.join("src/c.rs"), "// posts:write without quotes")
        .unwrap_or_else(|error| panic!("{error}"));
    let permissions = ["posts:read".to_owned(), "posts:write".to_owned()];
    assert_eq!(
        permission_owner(root, &permissions, &[]),
        Ok(Some(("src/a/b.rs".to_owned(), "posts:read".to_owned())))
    );
    assert_eq!(
        permission_owner(root, &permissions, &["src/a/b.rs"]),
        Ok(None)
    );
    assert_eq!(
        permission_owner(root, &["notes:read".to_owned()], &[]),
        Ok(None)
    );
}

#[test]
fn permissions_assembled_by_macros_or_in_non_utf8_files_are_found() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    fs::create_dir_all(root.join("src")).unwrap_or_else(|error| panic!("{error}"));
    fs::write(
        root.join("src/assembled.rs"),
        "const P: &str = concat!(\"posts\", \":read\");",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    fs::write(
        root.join("src/formatted.rs"),
        "fn p() -> String { format!(\"posts:{}\", \"x\") } // \":write\"",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let permissions = ["posts:read".to_owned(), "posts:write".to_owned()];
    assert_eq!(
        permission_owner(root, &permissions, &["src/formatted.rs"]),
        Ok(Some((
            "src/assembled.rs".to_owned(),
            "posts:read".to_owned()
        )))
    );
    assert_eq!(
        permission_owner(root, &["posts:write".to_owned()], &["src/assembled.rs"]),
        Ok(Some((
            "src/formatted.rs".to_owned(),
            "posts:write".to_owned()
        )))
    );
    // A name that merely shares a word is not a mention.
    assert_eq!(
        permission_owner(root, &["tags:read".to_owned()], &[]),
        Ok(None)
    );

    let mut bytes = b"// \xff\xfe not utf-8\nconst P: &str = \"notes:read\";\n".to_vec();
    bytes.push(b'\n');
    fs::write(root.join("src/binary.rs"), bytes).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        permission_owner(root, &["notes:read".to_owned()], &[]),
        Ok(Some(("src/binary.rs".to_owned(), "notes:read".to_owned())))
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_to_code_under_src_stops_the_permission_scan() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    let outside = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    fs::write(
        outside.path().join("hidden.rs"),
        "const P: &str = \"posts:read\";",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    fs::create_dir_all(root.join("src")).unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path(), root.join("src/linked"))
        .unwrap_or_else(|error| panic!("{error}"));
    let error = permission_owner(root, &["posts:read".to_owned()], &[])
        .err()
        .unwrap_or_default();
    assert!(error.contains("src/linked is a symbolic link"), "{error}");

    // A linked Rust file is refused too; a dangling or non-code link holds nothing.
    fs::remove_file(root.join("src/linked")).unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path().join("hidden.rs"), root.join("src/hidden.rs"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(permission_owner(root, &["posts:read".to_owned()], &[]).is_err());
    fs::remove_file(root.join("src/hidden.rs")).unwrap_or_else(|error| panic!("{error}"));
    std::os::unix::fs::symlink(outside.path().join("missing"), root.join("src/dangling"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        permission_owner(root, &["posts:read".to_owned()], &[]),
        Ok(None)
    );
}
