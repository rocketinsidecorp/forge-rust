use std::collections::BTreeMap;

use super::*;
use crate::generate::{Database, ForgeSource, NewApplication, create_application};

const VERSION: &str = "1760832000000000000";

fn application(database: Database, views: bool) -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("temporary directory: {error}"));
    let options = NewApplication {
        name: "shop".to_owned(),
        docker: false,
        ci: false,
        database,
        views,
        forge: ForgeSource::Path("../forge/crates/forge".to_owned()),
        locale: crate::generate::Locale::En,
    };
    let root = create_application(parent.path(), &options)
        .unwrap_or_else(|error| panic!("application: {error}"));
    (parent, root)
}

fn options(name: &str, fields: &[&str], scope: Scope) -> ScaffoldOptions {
    ScaffoldOptions {
        name: name.to_owned(),
        fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        scope,
        plural: None,
        broadcast: false,
        dry_run: false,
        layers: Layers::Scaffold,
    }
}

fn post() -> ScaffoldOptions {
    options(
        "post",
        &["title:string", "body:text", "published:boolean"],
        Scope::Global,
    )
}

fn event() -> ScaffoldOptions {
    options(
        "event",
        &[
            "name:string",
            "notes:text?",
            "starts_at:timestamp",
            "ends_at:timestamp?",
            "capacity:integer",
            "attendees:integer?",
            "public:boolean",
            "featured:boolean?",
            "venue:uuid",
            "organizer:uuid?",
        ],
        Scope::Tenant,
    )
}

fn ok(root: &Path, options: &ScaffoldOptions) -> Outcome {
    generate(root, options, VERSION).unwrap_or_else(|error| panic!("scaffold: {error}"))
}

fn refusal(root: &Path, options: &ScaffoldOptions) -> String {
    match generate(root, options, VERSION) {
        Ok(outcome) => panic!("accepted: {:?}", outcome.created),
        Err(error) => error.to_string(),
    }
}

/// Every file below `root`, relative path to content.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else if let (Ok(relative), Ok(content)) =
                (path.strip_prefix(root), fs::read_to_string(&path))
            {
                files.insert(relative.to_string_lossy().replace('\\', "/"), content);
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

/// Golden output, committed under `tests/fixtures/scaffold/<case>`:
/// every generated file plus `WIRING.txt`. Regenerate with
/// `FORGE_BLESS=1 cargo test -p forge-cli` and review the diff.
fn assert_golden(case: &str, database: Database, scaffolds: &[ScaffoldOptions]) {
    let (_parent, root) = application(database, true);
    let before = tree(&root);
    let mut expected = BTreeMap::new();
    let mut wiring = String::new();
    for options in scaffolds {
        let outcome = ok(&root, options);
        wiring.push_str(&outcome.wiring);
    }
    for (path, content) in tree(&root) {
        if before.get(&path) != Some(&content) {
            expected.insert(path, content);
        }
    }
    expected.insert("WIRING.txt".to_owned(), wiring);

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/scaffold")
        .join(case);
    if std::env::var_os("FORGE_BLESS").is_some() {
        let _ = fs::remove_dir_all(&fixture);
        for (path, content) in &expected {
            let target = fixture.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).unwrap_or_else(|error| panic!("{error}"));
            }
            fs::write(&target, content).unwrap_or_else(|error| panic!("{error}"));
        }
        return;
    }
    let actual = tree(&fixture);
    let drift: Vec<&String> = expected
        .keys()
        .chain(actual.keys())
        .filter(|path| expected.get(*path) != actual.get(*path))
        .collect();
    assert!(
        drift.is_empty(),
        "scaffold output differs from tests/fixtures/scaffold/{case} in {drift:?}; run \
         `FORGE_BLESS=1 cargo test -p forge-cli` and review the diff"
    );
}

fn broadcasting(mut options: ScaffoldOptions) -> ScaffoldOptions {
    options.broadcast = true;
    options
}

#[test]
fn postgresql_broadcasting_scaffolds_match_the_golden_output() {
    assert_golden(
        "postgresql-broadcast",
        Database::Postgresql,
        &[broadcasting(event()), broadcasting(post())],
    );
}

#[test]
fn the_streams_module_is_written_once_and_reused() {
    let (_parent, root) = application(Database::Sqlite3, true);

    let plain = ok(&root, &post());
    assert!(!plain.created.iter().any(|path| path == STREAMS_PATH));
    assert!(!root.join("templates/posts/_row.html").exists());
    assert!(!plain.wiring.contains("Streams::register"));

    let first = ok(&root, &broadcasting(event()));
    assert!(first.created.iter().any(|path| path == STREAMS_PATH));
    assert!(
        first
            .created
            .iter()
            .any(|path| path == "templates/events/_row.html")
    );
    assert!(!first.reused_streams);
    assert!(first.wiring.contains("pub mod streams;"));
    // SQLite has no LISTEN/NOTIFY: the wiring says so and stays in process.
    assert!(
        first.wiring.contains("Fanout::InProcess"),
        "{}",
        first.wiring
    );
    assert!(!first.wiring.contains("Fanout::Postgres"));
    assert!(first.wiring.contains("&streams,"));

    let mut note = options("note", &["body:text"], Scope::Global);
    note.broadcast = true;
    let second = ok(&root, &note);
    assert!(!second.created.iter().any(|path| path == STREAMS_PATH));
    assert!(second.reused_streams);
    assert!(!second.wiring.contains("Streams::register"));
    assert!(!second.wiring.contains("pub mod streams;"));
    assert!(second.wiring.contains("&streams,"));
}

#[test]
fn broadcast_needs_the_feature_and_an_intact_streams_module() {
    let (_parent, root) = application(Database::Postgresql, true);
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    fs::write(
        root.join("Cargo.toml"),
        manifest.replace(", \"broadcast\"", ""),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(refusal(&root, &broadcasting(post())).contains("`broadcast` feature"));
    assert!(
        ok(&root, &post()).wiring.contains("posts"),
        "plain scaffolds still work"
    );

    let (_parent, root) = application(Database::Postgresql, true);
    fs::create_dir_all(root.join("src/adapters/http")).unwrap_or_else(|error| panic!("{error}"));
    fs::write(root.join(STREAMS_PATH), "//! mine\n").unwrap_or_else(|error| panic!("{error}"));
    assert!(refusal(&root, &broadcasting(post())).contains("pub struct Streams"));
}

#[test]
fn an_access_module_whose_global_access_returns_no_principal_is_refused_with_instructions() {
    let (_parent, root) = application(Database::Postgresql, true);
    ok(&root, &event());
    let access = fs::read_to_string(root.join(ACCESS_PATH)).unwrap_or_default();
    let old = access.replace(
        "Result<Option<PrincipalId>, AccessDenied>",
        "Result<(), AccessDenied>",
    );
    assert_ne!(old, access);
    fs::write(root.join(ACCESS_PATH), old).unwrap_or_else(|error| panic!("{error}"));

    for refused in [
        post(),
        broadcasting(options("meeting", &["name:string"], Scope::Tenant)),
    ] {
        let reason = refusal(&root, &refused);
        assert!(
            reason.contains("Result<Option<PrincipalId>, AccessDenied>"),
            "{reason}"
        );
    }
    assert!(
        ok(&root, &options("note", &["body:text"], Scope::Tenant)).reused_access,
        "tenant scaffolds without live updates do not use GlobalAccess"
    );
}

#[test]
fn postgresql_global_post_matches_the_golden_output() {
    assert_golden("postgresql-post-global", Database::Postgresql, &[post()]);
}

#[test]
fn sqlite_tenant_scoped_event_matches_the_golden_output() {
    assert_golden("sqlite3-event-tenant", Database::Sqlite3, &[event()]);
}

#[test]
fn output_is_deterministic_and_covers_every_ring() {
    let (_first_parent, first) = application(Database::Postgresql, true);
    let (_second_parent, second) = application(Database::Postgresql, true);
    let outcome = ok(&first, &event());
    let again = ok(&second, &event());

    assert_eq!(outcome.created, again.created);
    assert_eq!(outcome.wiring, again.wiring);
    assert_eq!(tree(&first), tree(&second));
    for path in [
        "migrations/1760832000000000000_create_events.up.sql",
        "migrations/1760832000000000000_create_events.down.sql",
        "src/domain/entities/event.rs",
        "src/application/ports/event_repository.rs",
        "src/application/use_cases/events.rs",
        "src/infrastructure/database/events.rs",
        "src/adapters/http/access.rs",
        "src/adapters/http/events.rs",
        "src/adapters/http/views/events.rs",
        "templates/events/index.html",
        "templates/events/show.html",
        "templates/events/new.html",
        "templates/events/edit.html",
        "templates/events/_form.html",
        "templates/events/not_found.html",
        "tests/events_http.rs",
        "tests/events_repository.rs",
    ] {
        assert!(
            outcome.created.iter().any(|created| created == path),
            "{path}"
        );
        assert!(first.join(path).is_file(), "{path}");
    }
    for (path, content) in tree(&first) {
        assert!(
            !content.contains("%%"),
            "{path} has an unrendered placeholder"
        );
        assert!(!content.contains("gen_random_uuid"), "{path}");
        assert!(!content.contains("unwrap()"), "{path}");
        assert!(!content.contains(".expect("), "{path}");
        assert!(!content.contains("TODO"), "{path}");
    }
}

#[test]
fn postgresql_tenant_tables_force_row_level_security_and_grant_the_runtime_role() {
    let (_parent, root) = application(Database::Postgresql, true);
    ok(&root, &event());

    let up = fs::read_to_string(root.join(format!("migrations/{VERSION}_create_events.up.sql")))
        .unwrap_or_default();
    for needle in [
        "\"tenant_id\" uuid NOT NULL",
        "ALTER TABLE \"events\" ENABLE ROW LEVEL SECURITY;",
        "ALTER TABLE \"events\" FORCE ROW LEVEL SECURITY;",
        "NULLIF(current_setting('forge.tenant_id', true), '')::uuid",
        "WITH CHECK",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE \"events\" TO app_runtime;",
        "REVOKE ALL ON TABLE \"events\" FROM PUBLIC;",
        "CONSTRAINT \"events_version_positive\" CHECK (\"version\" >= 1)",
    ] {
        assert!(up.contains(needle), "{needle}\n{up}");
    }
    let repository =
        fs::read_to_string(root.join("src/infrastructure/database/events.rs")).unwrap_or_default();
    assert!(repository.contains("begin_tenant_transaction(self.database.pool(), context)"));
    assert!(repository.contains("WHERE \"tenant_id\" = $1 AND \"id\" = $2"));
}

#[test]
fn other_backends_scope_tenants_by_predicate_without_postgresql_features() {
    for database in [Database::Sqlite3, Database::Mysql, Database::Mariadb] {
        let (_parent, root) = application(database, true);
        ok(&root, &event());
        for (path, content) in tree(&root) {
            if !path.contains("event") {
                continue;
            }
            for needle in [
                "ROW LEVEL SECURITY",
                "set_config",
                "begin_tenant_transaction",
                "app_runtime",
                "PgPool",
            ] {
                assert!(!content.contains(needle), "{database:?} {path}: {needle}");
            }
        }
        let repository = fs::read_to_string(root.join("src/infrastructure/database/events.rs"))
            .unwrap_or_default();
        let quote = if database == Database::Sqlite3 {
            "\"tenant_id\" = ?"
        } else {
            "`tenant_id` = ?"
        };
        assert!(repository.contains(quote), "{database:?}");
    }
}

#[test]
fn wiring_is_printed_and_never_written() {
    let (_parent, root) = application(Database::Postgresql, true);
    let bootstrap = fs::read_to_string(root.join("src/bootstrap/mod.rs")).unwrap_or_default();
    let http = fs::read_to_string(root.join("src/adapters/http/mod.rs")).unwrap_or_default();

    let outcome = ok(&root, &post());

    assert_eq!(
        fs::read_to_string(root.join("src/bootstrap/mod.rs")).unwrap_or_default(),
        bootstrap
    );
    assert_eq!(
        fs::read_to_string(root.join("src/adapters/http/mod.rs")).unwrap_or_default(),
        http
    );
    for needle in [
        "src/domain/entities/mod.rs",
        "pub mod post;",
        "pub mod post_repository;",
        "pub mod access;",
        "crate::adapters::http::posts::routes(",
        "crate::adapters::http::access::DenyAll",
        "composition.postgres()?.clone()",
    ] {
        assert!(
            outcome.wiring.contains(needle),
            "{needle}\n{}",
            outcome.wiring
        );
    }
    assert!(!outcome.reused_access);

    // The second scaffold reuses the access module.
    let second = ok(&root, &event());
    assert!(second.reused_access);
    assert!(!second.created.iter().any(|path| path == ACCESS_PATH));
    assert!(!second.wiring.contains("pub mod access;"));
    assert!(second.wiring.contains("SessionTenantAccess::new("));
    assert!(second.wiring.contains("PgMembershipStore::new(pool)"));
    // RBAC matches exactly: the owner `setup` creates gets both permissions,
    // and nothing else is granted.
    assert!(
        second
            .wiring
            .contains("let mut policy = forge::security::RbacPolicy::new();")
    );
    assert!(
        second
            .wiring
            .contains("forge::security::Role::new(\"owner\")")
    );
    assert!(
        second
            .wiring
            .contains("policy.allow(owner.clone(), permission);")
    );
    assert!(second.wiring.contains("policy.allow(viewer, read);"));
    assert!(second.wiring.contains("use_cases::events::READ_PERMISSION"));
    assert!(
        second
            .wiring
            .contains("use_cases::events::WRITE_PERMISSION")
    );
}

#[test]
fn dry_run_lists_the_files_and_writes_nothing() {
    let (_parent, root) = application(Database::Sqlite3, true);
    let before = tree(&root);

    let outcome = ok(
        &root,
        &ScaffoldOptions {
            dry_run: true,
            ..post()
        },
    );

    assert_eq!(tree(&root), before);
    assert_eq!(outcome.created.len(), 18);
    assert!(
        outcome
            .created
            .contains(&"config/locales/posts.en.json".to_owned())
    );
    assert!(outcome.wiring.contains("composition.sqlite()?.clone()"));
}

#[test]
fn the_default_locale_gets_the_english_text_too() {
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("temporary directory: {error}"));
    let options = NewApplication {
        name: "shop".to_owned(),
        docker: false,
        ci: false,
        database: Database::Sqlite3,
        views: true,
        forge: ForgeSource::Path("../forge/crates/forge".to_owned()),
        locale: crate::generate::Locale::PtBr,
    };
    let root = create_application(parent.path(), &options)
        .unwrap_or_else(|error| panic!("application: {error}"));
    let outcome = ok(&root, &post());
    let english = fs::read_to_string(root.join("config/locales/posts.en.json"))
        .unwrap_or_else(|error| panic!("{error}"));
    let portuguese = fs::read_to_string(root.join("config/locales/posts.pt-BR.json"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        outcome
            .created
            .contains(&"config/locales/posts.pt-BR.json".to_owned())
    );
    assert_eq!(
        portuguese.replacen("\"pt-BR\"", "\"en\"", 1),
        english,
        "the same English text under the default locale"
    );
    assert!(english.contains("\"created\": \"Post was successfully created.\""));
    // Every page key the templates use is in the file.
    let translator = forge_i18n::Translator::load_dir(
        root.join("config/locales"),
        forge_i18n::Options::new().default_locale("pt-BR"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    for page in ["index", "show", "new", "edit", "_form", "not_found"] {
        let template = fs::read_to_string(root.join(format!("templates/posts/{page}.html")))
            .unwrap_or_else(|error| panic!("{error}"));
        for (_, rest) in template
            .match_indices(".t(\"")
            .map(|(at, _)| ((), &template[at + 4..]))
        {
            let key = rest.split('"').next().unwrap_or_default();
            assert!(translator.has("pt-BR", key), "{page}: {key}");
        }
    }
}

#[test]
fn formatting_scopes_are_refused_as_plurals() {
    let (_parent, root) = application(Database::Sqlite3, true);
    for plural in ["date", "time", "number"] {
        let message = refusal(
            &root,
            &ScaffoldOptions {
                plural: Some(plural.to_owned()),
                ..post()
            },
        );
        assert!(message.contains("formatting scope"), "{plural}: {message}");
    }
}

#[test]
fn applications_without_translations_are_refused_with_instructions() {
    let (_parent, root) = application(Database::Sqlite3, true);
    fs::remove_file(root.join(LOCALES_MODULE)).unwrap_or_else(|error| panic!("{error}"));
    let message = refusal(&root, &post());
    assert!(
        message.contains("Applications created before i18n"),
        "{message}"
    );
}

#[test]
fn refuses_to_overwrite_anything_and_writes_nothing() {
    let (_parent, root) = application(Database::Postgresql, true);
    fs::create_dir_all(root.join("templates/posts")).unwrap_or_else(|error| panic!("{error}"));
    fs::write(root.join("templates/posts/show.html"), "mine")
        .unwrap_or_else(|error| panic!("{error}"));
    let before = tree(&root);

    let reason = refusal(&root, &post());

    assert!(reason.contains("templates/posts/show.html"), "{reason}");
    assert!(reason.contains("nothing was written"), "{reason}");
    assert_eq!(tree(&root), before);

    // A resource scaffolded before is refused through its migration.
    fs::remove_file(root.join("templates/posts/show.html"))
        .unwrap_or_else(|error| panic!("{error}"));
    ok(&root, &post());
    let after = tree(&root);
    let reason = match generate(&root, &post(), "1760832000000000001") {
        Ok(_) => panic!("a second scaffold of posts must be refused"),
        Err(error) => error.to_string(),
    };
    assert!(reason.contains("_create_posts.up.sql"), "{reason}");
    assert_eq!(tree(&root), after);
}

#[test]
fn refuses_applications_it_cannot_serve() {
    let (_parent, api) = application(Database::Postgresql, false);
    assert!(refusal(&api, &post()).contains("--api"));

    let (_parent, none) = application(Database::Disabled, true);
    assert!(refusal(&none, &post()).contains("-d none"));

    let empty = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    assert!(refusal(empty.path(), &post()).contains("no Cargo.toml"));

    let (_parent, root) = application(Database::Postgresql, true);
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    fs::write(
        root.join("Cargo.toml"),
        manifest.replace(", \"testing\"", ""),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(refusal(&root, &post()).contains("testing"));

    let (_parent, root) = application(Database::Postgresql, true);
    fs::write(root.join(ACCESS_PATH), "//! mine\n").unwrap_or_else(|error| panic!("{error}"));
    assert!(refusal(&root, &post()).contains("GlobalAccess"));
}

#[test]
fn invalid_arguments_are_refused_before_reading_the_application() {
    let empty = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    for options in [
        options("posts", &["title:string"], Scope::Global),
        options("post", &["title:float"], Scope::Global),
        options("post", &["id:uuid"], Scope::Global),
        options("post", &["clone:string"], Scope::Global),
        options("type", &["title:string"], Scope::Global),
    ] {
        let reason = refusal(empty.path(), &options);
        assert!(!reason.contains("Cargo.toml"), "{reason}");
    }
}

#[test]
fn a_formatter_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let error = match rustfmt(directory.path(), "broken.rs", "fn broken( {") {
        Ok(_) => panic!("invalid Rust must not format"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("broken.rs"), "{error}");
}

#[test]
fn a_failed_write_removes_what_it_wrote() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = directory.path();
    fs::write(root.join("blocker"), "a file, not a directory")
        .unwrap_or_else(|error| panic!("{error}"));
    let files = [
        File {
            path: "fresh/one.rs".to_owned(),
            content: "// one\n".to_owned(),
        },
        File {
            path: "blocker/two.rs".to_owned(),
            content: "// two\n".to_owned(),
        },
    ];

    assert!(write_all(root, &files).is_err());

    assert!(!root.join("fresh/one.rs").exists());
    assert!(!root.join("fresh").exists());
    assert!(root.join("blocker").is_file());
}
