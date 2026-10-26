//! The seam: module declarations and `src/bootstrap/routes_gen.rs` are
//! edited idempotently, removed by destroy, and a conflicting hand edit is
//! refused without writing anything.

use std::fs;
use std::path::Path;

use super::*;

fn fail(reason: String) -> CliError {
    CliError::Scaffold(reason)
}

fn application(views: bool) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    for (path, content) in [
        (
            "src/adapters/http/mod.rs",
            "//! HTTP adapter.\n\npub mod authentication;\npub mod status;\n\nuse std::sync::Arc;\n",
        ),
        ("src/domain/entities/mod.rs", "//! Entities.\n"),
        (
            "src/bootstrap/mod.rs",
            "mod routes_gen;\n\nfn compose() {\n    routes_gen::generated_routes(composition)?;\n}\n",
        ),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(path, content).expect("write");
    }
    if views {
        let path = root.join("src/adapters/http/views/mod.rs");
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(path, "//! Views.\n").expect("write");
    }
    let seam = initial_routes(views);
    fs::write(root.join(ROUTES_FILE), seam).expect("write seam");
    directory
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).expect("read")
}

fn products() -> Registration {
    Registration {
        modules: vec![
            (
                "src/domain/entities/mod.rs".to_owned(),
                "product".to_owned(),
            ),
            ("src/adapters/http/mod.rs".to_owned(), "products".to_owned()),
            (
                "src/adapters/http/mod.rs".to_owned(),
                "products_api".to_owned(),
            ),
        ],
        routes: Some(Block::scoped(
            "scaffold",
            "products",
            "        crate::adapters::http::products::routes(composition.router(), site, &streams)?;\n",
        )),
        shared: vec![Block::shared(
            "streams",
            "streams",
            "    let streams = crate::adapters::http::streams::Streams::register(composition, forge::Fanout::InProcess)?;\n",
        )],
    }
}

fn wire(root: &Path, registration: &Registration, remove: bool) -> Vec<Edit> {
    let edits = plan(root, registration, remove, &fail).expect("plan");
    apply(root, &edits).expect("apply");
    edits
}

#[test]
fn the_initial_seam_is_formatted_signed_and_has_no_blocks() {
    for views in [true, false] {
        let directory = application(views);
        let root = directory.path();
        let seam = read(root, ROUTES_FILE);
        assert_eq!(parse_routes(&seam), Ok(Vec::new()));
        assert_eq!(rustfmt(root, ROUTES_FILE, &seam).expect("rustfmt"), seam);
        assert_eq!(render_routes(root, views, &[]).expect("render"), seam);
        assert!(
            seam.contains("_composition: &mut forge::Composition<'_>"),
            "{seam}"
        );
        assert_eq!(seam.contains("_site:"), views, "{seam}");
    }
}

#[test]
fn declarations_are_added_in_sorted_order_and_the_seam_gets_the_block() {
    let directory = application(true);
    let root = directory.path();
    let edits = wire(root, &products(), false);
    let verbs: Vec<(&str, &str)> = edits
        .iter()
        .map(|edit| (edit.verb, edit.path.as_str()))
        .collect();
    assert_eq!(
        verbs,
        [
            ("declare", "src/domain/entities/mod.rs"),
            ("declare", "src/adapters/http/mod.rs"),
            ("wire", ROUTES_FILE),
        ]
    );
    assert_eq!(
        read(root, "src/adapters/http/mod.rs"),
        "//! HTTP adapter.\n\npub mod authentication;\npub mod products;\npub mod products_api;\n\
         pub mod status;\n\nuse std::sync::Arc;\n"
    );
    assert_eq!(
        read(root, "src/domain/entities/mod.rs"),
        "//! Entities.\n\npub mod product;\n"
    );
    let seam = read(root, ROUTES_FILE);
    assert_eq!(rustfmt(root, ROUTES_FILE, &seam).expect("rustfmt"), seam);
    let streams = seam.find("// forge:streams streams").expect("shared block");
    let scaffold = seam
        .find("// forge:scaffold products")
        .expect("entry block");
    assert!(streams < scaffold, "{seam}");
    assert!(
        seam.contains("    composition: &mut forge::Composition<'_>,"),
        "{seam}"
    );
    assert!(seam.contains("    site: &std::sync::Arc<"), "{seam}");
}

#[test]
fn running_the_wiring_again_changes_nothing() {
    let directory = application(true);
    let root = directory.path();
    wire(root, &products(), false);
    let before: Vec<String> = [
        "src/adapters/http/mod.rs",
        "src/domain/entities/mod.rs",
        ROUTES_FILE,
    ]
    .iter()
    .map(|path| read(root, path))
    .collect();

    let again = wire(root, &products(), false);

    assert_eq!(
        again,
        [Edit {
            path: ROUTES_FILE.to_owned(),
            verb: "identical",
            detail: "scaffold products".to_owned(),
            content: None,
        }]
    );
    let after: Vec<String> = [
        "src/adapters/http/mod.rs",
        "src/domain/entities/mod.rs",
        ROUTES_FILE,
    ]
    .iter()
    .map(|path| read(root, path))
    .collect();
    assert_eq!(before, after);
}

#[test]
fn destroy_takes_the_declarations_and_the_block_back_out() {
    let directory = application(true);
    let root = directory.path();
    let initial: Vec<String> = [
        "src/adapters/http/mod.rs",
        "src/domain/entities/mod.rs",
        ROUTES_FILE,
    ]
    .iter()
    .map(|path| read(root, path))
    .collect();
    wire(root, &products(), false);

    let edits = wire(root, &products(), true);

    let verbs: Vec<&str> = edits.iter().map(|edit| edit.verb).collect();
    assert_eq!(verbs, ["undeclare", "undeclare", "unwire"]);
    let after: Vec<String> = [
        "src/adapters/http/mod.rs",
        "src/domain/entities/mod.rs",
        ROUTES_FILE,
    ]
    .iter()
    .map(|path| read(root, path))
    .collect();
    assert_eq!(
        after, initial,
        "the shared streams block went with its last user"
    );
    assert!(
        plan(root, &products(), true, &fail)
            .expect("plan")
            .is_empty()
    );
}

#[test]
fn a_shared_block_stays_while_another_block_uses_it() {
    let directory = application(true);
    let root = directory.path();
    let mut posts = products();
    posts.modules.clear();
    posts.routes = Some(Block::scoped(
        "scaffold",
        "posts",
        "        crate::adapters::http::posts::routes(composition.router(), site, &streams)?;\n",
    ));
    wire(root, &products(), false);
    wire(root, &posts, false);
    let seam = read(root, ROUTES_FILE);
    assert_eq!(
        seam.matches("// forge:streams streams").count(),
        1,
        "{seam}"
    );

    wire(root, &products(), true);

    let seam = read(root, ROUTES_FILE);
    assert!(seam.contains("// forge:streams streams"), "{seam}");
    assert!(seam.contains("// forge:scaffold posts"), "{seam}");
    assert!(!seam.contains("// forge:scaffold products"), "{seam}");
}

#[test]
fn a_hand_edit_of_the_seam_is_refused_and_nothing_is_written() {
    let directory = application(true);
    let root = directory.path();
    wire(root, &products(), false);
    let edited = read(root, ROUTES_FILE).replace("site, &streams", "site, &streams /* mine */");
    fs::write(root.join(ROUTES_FILE), &edited).expect("edit");
    let http = read(root, "src/adapters/http/mod.rs");

    for remove in [false, true] {
        let error = plan(root, &products(), remove, &fail)
            .expect_err("refused")
            .to_string();
        assert!(error.contains("edited by hand"), "{error}");
    }
    assert_eq!(read(root, ROUTES_FILE), edited);
    assert_eq!(read(root, "src/adapters/http/mod.rs"), http);
}

#[test]
fn the_same_block_with_other_code_is_refused() {
    let directory = application(true);
    let root = directory.path();
    wire(root, &products(), false);
    let mut other = products();
    other.routes = Some(Block::scoped(
        "scaffold",
        "products",
        "        crate::adapters::http::products::routes(composition.router(), site)?;\n",
    ));

    let error = plan(root, &other, false, &fail)
        .expect_err("refused")
        .to_string();

    assert!(
        error.contains("already wires the scaffold products"),
        "{error}"
    );
}

#[test]
fn a_module_declared_another_way_is_refused() {
    let directory = application(true);
    let root = directory.path();
    let path = root.join("src/adapters/http/mod.rs");
    fs::write(&path, "pub mod authentication;\nmod products;\n").expect("write");

    for remove in [false, true] {
        let error = plan(root, &products(), remove, &fail)
            .expect_err("refused")
            .to_string();
        assert!(error.contains("as `mod products;`"), "{error}");
    }
    assert_eq!(
        read(root, "src/adapters/http/mod.rs"),
        "pub mod authentication;\nmod products;\n"
    );
    assert!(!read(root, "src/domain/entities/mod.rs").contains("pub mod product;"));
}

#[test]
fn an_unformatted_mod_file_gets_only_the_line() {
    let directory = application(true);
    let root = directory.path();
    let path = root.join("src/domain/entities/mod.rs");
    fs::write(&path, "pub mod zebra;\nfn   kept( ) {}\n").expect("write");
    let mut registration = products();
    registration.routes = None;

    wire(root, &registration, false);

    assert_eq!(
        read(root, "src/domain/entities/mod.rs"),
        "pub mod product;\npub mod zebra;\nfn   kept( ) {}\n"
    );
}

#[test]
fn declared_module_reads_top_level_mod_items_only() {
    assert_eq!(declared_module("pub mod products;"), Some("products"));
    assert_eq!(
        declared_module("pub(crate) mod products;"),
        Some("products")
    );
    assert_eq!(declared_module("#[cfg(test)] mod tests {"), Some("tests"));
    assert_eq!(declared_module("mod products {"), Some("products"));
    assert_eq!(declared_module("    pub mod nested;"), None);
    assert_eq!(declared_module("use crate::products;"), None);
    assert_eq!(declared_module("// pub mod products;"), None);
}

#[test]
fn uses_matches_whole_identifiers() {
    assert!(uses("routes(&streams)", "streams"));
    assert!(!uses("routes(&streams_x)", "streams"));
    assert!(!uses("crate::adapters::http::streams::Streams", "site"));
    assert!(uses("(router, site)", "site"));
}

fn guarded() -> Registration {
    let mut registration = products();
    registration.routes = Some(Block::scoped(
        "scaffold",
        "products",
        "        let access = protect(composition, &[\"products:read\"])?;\n        crate::adapters::http::products::routes(composition.router(), site, access)?;\n",
    ));
    registration.shared = vec![Block::shared(
        "guard",
        "protect",
        "    pub(super) fn protect(\n        _composition: &mut forge::Composition<'_>,\n        _permissions: &[&str],\n    ) -> Result<(), forge::ComposeError> {\n        Ok(())\n    }\n",
    )];
    registration
}

#[test]
fn guards_are_module_level_functions_behind_an_ambiguity_tripwire() {
    let directory = application(true);
    let root = directory.path();
    wire(root, &guarded(), false);
    let seam = read(root, ROUTES_FILE);
    assert!(
        seam.contains("#[allow(unused_imports)]\nuse super::*;\nuse guards::*;"),
        "{seam}"
    );
    assert!(
        seam.contains("\nmod guards {\n    // forge:guard protect\n    pub(super) fn protect("),
        "{seam}"
    );
    assert!(!seam.contains("forge:streams"), "{seam}");
    assert_eq!(parse_routes(&seam).map(|blocks| blocks.len()), Ok(2));
}

#[test]
fn a_guard_the_application_declares_replaces_the_generated_one() {
    let directory = application(true);
    let root = directory.path();
    let bootstrap = root.join("src/bootstrap/mod.rs");
    let mut source = read(root, "src/bootstrap/mod.rs");
    // Neither a comment nor a longer name counts.
    source.push_str("// fn protect(\nfn protect_cookie() {}\n");
    fs::write(&bootstrap, &source).expect("write");
    wire(root, &guarded(), false);
    assert!(read(root, ROUTES_FILE).contains("pub(super) fn protect("));

    // Declared after generation: the next run of the generator drops the
    // generated guard (until then the build fails: the name is ambiguous).
    source.push_str("fn protect(\n    _: &mut forge::Composition<'_>,\n    _: &[&str],\n) {}\n");
    fs::write(&bootstrap, &source).expect("write");
    let edits = wire(root, &guarded(), false);
    assert_eq!(edits.last().map(|edit| edit.verb), Some("wire"));
    let seam = read(root, ROUTES_FILE);
    assert!(!seam.contains("mod guards"), "{seam}");
    assert!(seam.contains("use super::*;"), "{seam}");
    assert!(seam.contains("let access = protect(composition"), "{seam}");

    // In another file of src/bootstrap too; the generated one comes back
    // when the application's is gone.
    fs::write(
        &bootstrap,
        read(root, "src/bootstrap/mod.rs").replace("fn protect(\n", "fn unused(\n"),
    )
    .expect("write");
    fs::write(
        root.join("src/bootstrap/guards.rs"),
        "pub(super) fn protect<'a>() {}\n",
    )
    .expect("write");
    assert_eq!(
        plan(root, &guarded(), false, &fail)
            .expect("plan")
            .last()
            .map(|edit| edit.verb),
        Some("identical")
    );
    fs::remove_file(root.join("src/bootstrap/guards.rs")).expect("remove");
    wire(root, &guarded(), false);
    assert!(read(root, ROUTES_FILE).contains("pub(super) fn protect("));
}

#[test]
fn a_seam_needing_a_guard_it_cannot_define_is_refused() {
    let directory = application(true);
    let root = directory.path();
    let mut unguarded = guarded();
    unguarded.shared.clear();
    let error = plan(root, &unguarded, false, &fail)
        .expect_err("refused")
        .to_string();
    assert!(
        error.contains("needs protect, which it does not define"),
        "{error}"
    );
}

#[test]
fn a_structure_the_generators_never_write_is_refused_behind_a_valid_checksum() {
    let directory = application(true);
    let root = directory.path();
    wire(root, &products(), false);
    let seam = read(root, ROUTES_FILE);
    let unsigned = |text: &str| {
        let first = text.lines().next().unwrap_or_default().to_owned();
        text.replacen(
            &first,
            &format!("{CHECKSUM_PREFIX}{CHECKSUM_PLACEHOLDER}"),
            1,
        )
    };
    for edited in [
        // An item after the function.
        format!("{seam}\nfn mine() {{}}\n"),
        // A statement before the first block.
        seam.replacen(
            "    // forge:streams",
            "    let mine = 1;\n    // forge:streams",
            1,
        ),
        // A marker without a name.
        seam.replacen("// forge:scaffold products", "// forge:scaffold", 1),
    ] {
        let signed = sign(&unsigned(&edited));
        fs::write(root.join(ROUTES_FILE), &signed).expect("write");
        let error = plan(root, &products(), false, &fail)
            .expect_err("refused")
            .to_string();
        assert!(error.contains("edited by hand"), "{error}\n{signed}");
        assert_eq!(read(root, ROUTES_FILE), signed);
    }
}

#[cfg(unix)]
#[test]
fn symlinked_seams_and_mod_files_are_refused() {
    let directory = application(true);
    let root = directory.path();
    let outside = tempfile::tempdir().expect("temporary directory");
    for path in [ROUTES_FILE, "src/domain/entities/mod.rs"] {
        let target = outside.path().join("target.rs");
        fs::write(&target, read(root, path)).expect("write");
        let original = root.join(path);
        let kept = read(root, path);
        fs::remove_file(&original).expect("remove");
        std::os::unix::fs::symlink(&target, &original).expect("symlink");
        let error = plan(root, &products(), false, &fail)
            .expect_err("refused")
            .to_string();
        assert!(
            error.contains("symlink") || error.contains("link"),
            "{path}: {error}"
        );
        fs::remove_file(&original).expect("remove link");
        fs::write(&original, kept).expect("restore");
    }
}

#[test]
fn odd_files_in_bootstrap_are_ignored_by_the_guard_scan() {
    let directory = application(true);
    let root = directory.path();
    fs::create_dir_all(root.join("src/bootstrap/dir.rs")).expect("directory");
    fs::write(root.join("src/bootstrap/binary.rs"), [0xff_u8, 0xfe, 0x00]).expect("write");
    fs::write(root.join("src/bootstrap/notes.txt"), "fn protect(").expect("write");
    wire(root, &guarded(), false);
    assert!(read(root, ROUTES_FILE).contains("pub(super) fn protect("));
}

#[test]
fn a_missing_seam_is_written_fresh() {
    let directory = application(false);
    let root = directory.path();
    fs::remove_file(root.join(ROUTES_FILE)).expect("remove");
    let mut registration = products();
    registration.routes = Some(Block::scoped(
        "resource",
        "products",
        "        let _ = composition;\n",
    ));
    registration.shared.clear();
    let edits = wire(root, &registration, false);
    assert_eq!(edits.last().map(|edit| edit.verb), Some("wire"));
    let seam = read(root, ROUTES_FILE);
    assert!(!seam.contains("site"), "{seam}");
    assert!(plan(root, &registration, true, &fail).is_ok());
}

#[test]
fn report_lists_each_edit_and_dry_runs_say_would() {
    let edits = [
        Edit {
            path: ROUTES_FILE.to_owned(),
            verb: "wire",
            detail: "scaffold products".to_owned(),
            content: Some(String::new()),
        },
        Edit {
            path: ROUTES_FILE.to_owned(),
            verb: "identical",
            detail: String::new(),
            content: None,
        },
    ];
    let mut output = Vec::new();
    report(&mut output, true, &edits).expect("report");
    assert_eq!(
        String::from_utf8(output).expect("utf-8"),
        format!(
            "  would wire   {ROUTES_FILE}  (scaffold products)\n  identical    {ROUTES_FILE}\n"
        )
    );
}

#[test]
fn one_generator_at_a_time_holds_the_lock() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let lock = GenerateLock::acquire(root).expect("first");
    let error = GenerateLock::acquire(root).expect_err("second").to_string();
    assert!(error.contains(".forge/generate.lock"), "{error}");
    drop(lock);
    assert!(!root.join(LOCK_FILE).exists());
    drop(GenerateLock::acquire(root).expect("again"));
}

#[test]
fn edits_are_written_whole_and_leave_no_temporary_file() {
    let directory = application(true);
    let root = directory.path();
    wire(root, &products(), false);
    let leftovers: Vec<_> = fs::read_dir(root.join("src/bootstrap"))
        .expect("directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // A failed edit gives the files already changed their content back.
    let blocked = Edit {
        path: "src/missing/dir/mod.rs".to_owned(),
        verb: "declare",
        detail: String::new(),
        content: Some(String::new()),
    };
    let first = Edit {
        path: "src/domain/entities/mod.rs".to_owned(),
        verb: "declare",
        detail: String::new(),
        content: Some("changed\n".to_owned()),
    };
    let before = read(root, "src/domain/entities/mod.rs");
    assert!(apply(root, &[first, blocked]).is_err());
    assert_eq!(read(root, "src/domain/entities/mod.rs"), before);
}
