//! The model, controller, mailer and job generators and `forge destroy`
//! against freshly generated applications (ADR 0038). Nothing here builds
//! the application: `tests/scaffold.rs` does, with
//! `postgresql_application_generates_and_destroys_every_generator`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::controller::{self, ControllerOptions};
use crate::destroy::{self, DestroyFlags, DestroyPlan};
use crate::generate::{Database, ForgeSource, Locale, NewApplication, create_application};
use crate::job::{self, JobOptions};
use crate::mailer::{self, MailerOptions};
use crate::scaffold::{self, Layers, ScaffoldOptions, Scope};

const VERSION: &str = "1760832000000000000";

fn application(database: Database, views: bool, locale: Locale) -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("temporary directory: {error}"));
    let options = NewApplication {
        name: "shop".to_owned(),
        docker: false,
        ci: false,
        database,
        views,
        forge: ForgeSource::Path("../forge/crates/forge".to_owned()),
        locale,
    };
    let root = create_application(parent.path(), &options)
        .unwrap_or_else(|error| panic!("application: {error}"));
    (parent, root)
}

fn tree(root: &Path) -> BTreeMap<String, String> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            // `forge new` writes random development credentials.
            if path.ends_with("config/credentials") {
                continue;
            }
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

fn resource(name: &str, fields: &[&str], scope: Scope, layers: Layers) -> ScaffoldOptions {
    ScaffoldOptions {
        name: name.to_owned(),
        fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        scope,
        plural: None,
        broadcast: false,
        dry_run: false,
        layers,
    }
}

fn controller(name: &str, actions: &[&str]) -> ControllerOptions {
    ControllerOptions {
        name: name.to_owned(),
        actions: actions.iter().map(|action| (*action).to_owned()).collect(),
        api: false,
        public: false,
        dry_run: false,
    }
}

fn destroyed(root: &Path, plan: DestroyPlan, flags: DestroyFlags) -> Result<String, String> {
    let mut output = Vec::new();
    destroy::apply(root, &plan, flags, &mut output)
        .map(|()| String::from_utf8_lossy(&output).into_owned())
        .map_err(|error| error.to_string())
}

fn destroy_resource(
    root: &Path,
    options: &ScaffoldOptions,
    flags: DestroyFlags,
) -> Result<String, String> {
    let (files, unwire, owned_elsewhere) =
        scaffold::plan_destroy(root, options).map_err(|error| error.to_string())?;
    destroyed(
        root,
        DestroyPlan {
            files,
            unwire,
            owned_elsewhere,
        },
        flags,
    )
}

fn destroy_controller(
    root: &Path,
    options: &ControllerOptions,
    flags: DestroyFlags,
) -> Result<String, String> {
    let (files, unwire) =
        controller::plan_destroy(root, options).map_err(|error| error.to_string())?;
    destroyed(
        root,
        DestroyPlan {
            files,
            unwire,
            owned_elsewhere: None,
        },
        flags,
    )
}

const ALL: DestroyFlags = DestroyFlags {
    dry_run: false,
    force: false,
    migrations: true,
};

#[test]
fn a_model_is_the_scaffold_without_its_http_layer_byte_for_byte() {
    let fields = ["title:string", "body:text?", "published:boolean"];
    let (_model_parent, model_root) = application(Database::Postgresql, true, Locale::En);
    let (_scaffold_parent, scaffold_root) = application(Database::Postgresql, true, Locale::En);
    let before = tree(&model_root);
    let model = scaffold::generate(
        &model_root,
        &resource("post", &fields, Scope::Tenant, Layers::Model),
        VERSION,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    scaffold::generate(
        &scaffold_root,
        &resource("post", &fields, Scope::Tenant, Layers::Scaffold),
        VERSION,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        model.created,
        [
            format!("migrations/{VERSION}_create_posts.up.sql"),
            format!("migrations/{VERSION}_create_posts.down.sql"),
            "src/domain/entities/post.rs".to_owned(),
            "src/application/ports/post_repository.rs".to_owned(),
            "src/application/use_cases/posts.rs".to_owned(),
            "src/infrastructure/database/posts.rs".to_owned(),
            "tests/posts_repository.rs".to_owned(),
        ]
    );
    let scaffolded = tree(&scaffold_root);
    for path in &model.created {
        assert_eq!(
            fs::read_to_string(model_root.join(path)).ok().as_ref(),
            scaffolded.get(path),
            "{path}"
        );
    }
    assert!(!model_root.join(scaffold::ACCESS_PATH).exists());
    assert!(
        model.wiring.contains("pub mod post_repository;"),
        "{}",
        model.wiring
    );
    assert!(
        model.wiring.contains("PgPostRepository::new"),
        "{}",
        model.wiring
    );
    assert!(!model.wiring.contains("adapters::http"), "{}", model.wiring);

    // A scaffold of the same plural collides; destroying the model while a
    // scaffold's pages exist is refused.
    let collision = scaffold::generate(
        &model_root,
        &resource("post", &fields, Scope::Tenant, Layers::Scaffold),
        "1760832000000000001",
    )
    .err()
    .map(|error| error.to_string())
    .unwrap_or_default();
    assert!(collision.contains("_create_posts.up.sql"), "{collision}");
    let owned = destroy_resource(
        &scaffold_root,
        &resource("post", &fields, Scope::Tenant, Layers::Model),
        ALL,
    )
    .err()
    .unwrap_or_default();
    assert!(owned.contains("src/adapters/http/posts.rs"), "{owned}");

    // Destroying the model restores the application.
    destroy_resource(
        &model_root,
        &resource("post", &fields, Scope::Tenant, Layers::Model),
        ALL,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(tree(&model_root), before);
}

#[test]
fn models_need_a_database_and_its_test_support_but_no_views() {
    // A full-stack SQLite application: a model needs no views of its own.
    let (_parent, sqlite) = application(Database::Sqlite3, true, Locale::En);
    let views =
        fs::read_to_string(sqlite.join("src/adapters/http/views/mod.rs")).unwrap_or_default();
    scaffold::generate(
        &sqlite,
        &resource("post", &["title:string"], Scope::Global, Layers::Model),
        VERSION,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        fs::read_to_string(sqlite.join("src/adapters/http/views/mod.rs")).unwrap_or_default(),
        views
    );
    // An `--api` application has no views, and a model needs none: it gets the
    // layers below HTTP, plus the repository tests (`forge new --api` has the
    // `forge::db::testing` dev-dependency for them).
    let (_parent, api) = application(Database::Sqlite3, false, Locale::En);
    let outcome = scaffold::generate(
        &api,
        &resource("post", &["title:string"], Scope::Global, Layers::Model),
        VERSION,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        outcome
            .created
            .iter()
            .any(|path| path == "tests/posts_repository.rs"),
        "{:?}",
        outcome.created
    );
    assert!(
        outcome
            .created
            .iter()
            .all(|path| !path.starts_with("templates/")
                && !path.contains("views")
                && !path.starts_with("config/locales/")
                && path != "src/adapters/http/posts.rs"),
        "no view or page files in an API application: {:?}",
        outcome.created
    );
    // A scaffold is still refused: it writes pages.
    let refusal = scaffold::generate(
        &api,
        &resource("note", &["title:string"], Scope::Global, Layers::Scaffold),
        VERSION,
    )
    .err()
    .map(|error| error.to_string())
    .unwrap_or_default();
    assert!(refusal.contains("--api"), "{refusal}");
    // An application generated before the dev-dependency had `testing` in
    // `--api` mode is told what to add.
    let (_parent, old_api) = application(Database::Sqlite3, false, Locale::En);
    let manifest = fs::read_to_string(old_api.join("Cargo.toml")).unwrap_or_default();
    fs::write(
        old_api.join("Cargo.toml"),
        manifest.replace("\"testing\"", "\"jobs\""),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let refusal = scaffold::generate(
        &old_api,
        &resource("post", &["title:string"], Scope::Global, Layers::Model),
        VERSION,
    )
    .err()
    .map(|error| error.to_string())
    .unwrap_or_default();
    assert!(refusal.contains("\"testing\""), "{refusal}");
    let (_parent, none) = application(Database::Disabled, true, Locale::En);
    let refusal = scaffold::generate(
        &none,
        &resource("post", &["title:string"], Scope::Global, Layers::Model),
        VERSION,
    )
    .err()
    .map(|error| error.to_string())
    .unwrap_or_default();
    assert!(
        refusal.starts_with("cannot generate the model:"),
        "{refusal}"
    );
    assert!(refusal.contains("no database"), "{refusal}");
}

#[test]
fn destroying_a_scaffold_keeps_shared_files_migrations_and_translations() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::Es);
    let before = tree(&root);
    let mut options = resource("post", &["title:string"], Scope::Global, Layers::Scaffold);
    options.broadcast = true;
    scaffold::generate(&root, &options, VERSION).unwrap_or_else(|error| panic!("{error}"));
    let translated = root.join("config/locales/posts.es.json");
    assert!(translated.exists());
    fs::write(
        &translated,
        "{\"posts\": {\"index\": {\"title\": \"Artículos\"}}}\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));

    // Other arguments render other files: nothing is removed.
    let mut other = options.clone();
    other.broadcast = false;
    let refusal = destroy_resource(&root, &other, DestroyFlags::default())
        .err()
        .unwrap_or_default();
    assert!(refusal.contains("src/adapters/http/posts.rs"), "{refusal}");
    assert!(root.join("src/adapters/http/posts.rs").exists());

    let printed = destroy_resource(&root, &options, DestroyFlags::default())
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        printed.contains("keep         config/locales/posts.es.json  (translated"),
        "{printed}"
    );
    assert!(
        printed.contains("remove       config/locales/posts.en.json"),
        "{printed}"
    );
    assert!(printed.contains("keep         migrations/"), "{printed}");
    assert!(translated.exists());
    assert!(!root.join("templates/posts").exists());
    assert!(root.join(scaffold::ACCESS_PATH).exists());
    assert!(root.join("src/adapters/http/streams.rs").exists());
    fs::remove_file(&translated).unwrap_or_else(|error| panic!("{error}"));
    destroy_resource(&root, &options, ALL).unwrap_or_else(|error| panic!("{error}"));

    let mut after = tree(&root);
    after.remove(scaffold::ACCESS_PATH);
    after.remove("src/adapters/http/streams.rs");
    assert_eq!(after, before);
}

#[test]
fn a_controller_writes_guarded_pages_and_destroy_takes_them_back() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let before = tree(&root);
    let options = controller("pages", &["index", "about_us"]);
    let outcome = controller::generate(&root, &options).unwrap_or_else(|error| panic!("{error}"));
    let paths: Vec<&str> = outcome
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "src/adapters/http/access.rs",
            "src/adapters/http/pages_controller.rs",
            "src/adapters/http/views/pages_controller.rs",
            "templates/pages/index.html",
            "templates/pages/about_us.html",
            "tests/pages_controller_http.rs",
        ]
    );
    let handlers =
        fs::read_to_string(root.join("src/adapters/http/pages_controller.rs")).unwrap_or_default();
    assert!(handlers.contains("pub const READ_PERMISSION: &str = \"pages:read\";"));
    assert!(handlers.contains("route(router, &state, \"/pages\", index)?;"));
    assert!(handlers.contains("route(router, &state, \"/pages/about_us\", about_us)?;"));
    assert!(!handlers.contains("Method::POST"));
    assert!(
        outcome.wiring.contains("pub mod access;"),
        "{}",
        outcome.wiring
    );
    assert!(
        outcome.wiring.contains("access::DenyAll"),
        "{}",
        outcome.wiring
    );
    assert!(outcome.wiring.contains("GET /pages, GET /pages/about_us"));

    // Never overwrites; a second controller reuses the access module.
    let again = controller::generate(&root, &options)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(again.contains("refusing to overwrite"), "{again}");
    let mut api = controller("reports", &["monthly"]);
    api.api = true;
    let reports = controller::generate(&root, &api).unwrap_or_else(|error| panic!("{error}"));
    assert!(reports.reused_access);
    assert!(!reports.wiring.contains("pub mod access;"));
    let json = fs::read_to_string(root.join("src/adapters/http/reports_controller.rs"))
        .unwrap_or_default();
    assert!(json.contains("\"/v1/reports/monthly\""), "{json}");
    assert!(!root.join("templates/reports").exists());

    // A model whose permission a controller declares is refused.
    let refusal = scaffold::generate(
        &root,
        &resource("report", &["title:string"], Scope::Global, Layers::Model),
        VERSION,
    )
    .err()
    .map(|error| error.to_string())
    .unwrap_or_default();
    assert!(
        refusal.contains("src/adapters/http/reports_controller.rs"),
        "{refusal}"
    );
    assert!(refusal.contains("reports:read"), "{refusal}");

    // An edited page is kept unless --force; then everything goes but the
    // shared access module.
    fs::write(root.join("templates/pages/index.html"), "mine")
        .unwrap_or_else(|error| panic!("{error}"));
    let kept = destroy_controller(&root, &options, DestroyFlags::default())
        .err()
        .unwrap_or_default();
    assert!(kept.contains("templates/pages/index.html"), "{kept}");
    let forced = DestroyFlags {
        force: true,
        ..DestroyFlags::default()
    };
    destroy_controller(&root, &options, forced).unwrap_or_else(|error| panic!("{error}"));
    destroy_controller(&root, &api, DestroyFlags::default())
        .unwrap_or_else(|error| panic!("{error}"));
    let mut after = tree(&root);
    assert!(after.remove(scaffold::ACCESS_PATH).is_some());
    assert_eq!(after, before);
}

#[test]
fn a_controller_reuses_the_permission_its_model_declares() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let model = resource("widget", &["name:string"], Scope::Global, Layers::Model);
    scaffold::generate(&root, &model, VERSION).unwrap_or_else(|error| panic!("{error}"));
    let options = controller("widgets", &["dashboard"]);
    let outcome = controller::generate(&root, &options).unwrap_or_else(|error| panic!("{error}"));
    let handlers = fs::read_to_string(root.join("src/adapters/http/widgets_controller.rs"))
        .unwrap_or_default();
    assert!(
        handlers.contains("pub use crate::application::use_cases::widgets::READ_PERMISSION;"),
        "{handlers}"
    );
    assert!(!handlers.contains("\"widgets:read\""));
    assert!(
        outcome.wiring.contains("already declares"),
        "{}",
        outcome.wiring
    );

    // The model cannot go while the controller depends on it.
    let refusal = destroy_resource(&root, &model, ALL)
        .err()
        .unwrap_or_default();
    assert!(refusal.contains("widgets_controller.rs"), "{refusal}");
    destroy_controller(&root, &options, DestroyFlags::default())
        .unwrap_or_else(|error| panic!("{error}"));
    destroy_resource(&root, &model, ALL).unwrap_or_else(|error| panic!("{error}"));

    // A hand-written owner of the permission is refused, not reused.
    fs::write(
        root.join("src/adapters/http/gadgets.rs"),
        "pub const READ: &str = \"gadgets:read\";\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let refusal = controller::generate(&root, &controller("gadgets", &["index"]))
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(
        refusal.contains("src/adapters/http/gadgets.rs"),
        "{refusal}"
    );
}

#[test]
fn controllers_are_validated_and_need_a_full_stack_application() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let before = tree(&root);
    for (name, actions) in [
        ("pages_controller", &["index"][..]),
        ("Pages", &["index"][..]),
        ("assets", &["index"][..]),
        ("session", &["new"][..]),
        ("pages", &[][..]),
        ("pages", &["create"][..]),
        ("pages", &["destroy"][..]),
        ("pages", &["routes"][..]),
        ("pages", &["fn"][..]),
        ("pages", &["about-us"][..]),
        ("pages", &["home", "home"][..]),
        ("compose", &["error"][..]),
        ("pages", &["controller"][..]),
    ] {
        let refusal = controller::generate(&root, &controller(name, actions))
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(
            refusal.starts_with("cannot generate the controller:"),
            "{name} {actions:?}"
        );
    }
    assert_eq!(tree(&root), before, "a refusal writes nothing");

    let (_parent, api) = application(Database::Postgresql, false, Locale::En);
    let refusal = controller::generate(&api, &controller("pages", &["index"]))
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(refusal.contains("--api"), "{refusal}");

    // Without a database there is no session store to guard with.
    let (_parent, plain) = application(Database::Disabled, true, Locale::En);
    let refusal = controller::generate(&plain, &controller("pages", &["index"]))
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(refusal.contains("--public"), "{refusal}");
    let mut public = controller("pages", &["index"]);
    public.public = true;
    let outcome = controller::generate(&plain, &public).unwrap_or_else(|error| panic!("{error}"));
    assert!(outcome.wiring.contains("WARNING"), "{}", outcome.wiring);
    assert!(!plain.join(scaffold::ACCESS_PATH).exists());
}

#[test]
fn mailers_and_jobs_round_trip_through_destroy() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let before = tree(&root);
    let mailer_options = MailerOptions {
        name: "user".to_owned(),
        methods: vec!["welcome:name,url".to_owned(), "password_reset".to_owned()],
        dry_run: false,
    };
    let mailer = mailer::generate(&root, &mailer_options).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        mailer
            .files
            .iter()
            .any(|file| file.path == "templates/mailers/user/welcome.html")
    );
    let source =
        fs::read_to_string(root.join("src/adapters/email/user_mailer.rs")).unwrap_or_default();
    assert!(source.contains("pub struct UserWelcome {"), "{source}");
    assert!(source.contains("pub async fn password_reset("), "{source}");
    assert!(
        mailer.wiring.contains("\"/_mail/user\""),
        "{}",
        mailer.wiring
    );
    assert_eq!(
        fs::read_to_string(root.join("templates/mailers/user/welcome.subject.txt"))
            .unwrap_or_default(),
        "Welcome"
    );

    let job_options = JobOptions {
        name: "send_welcome".to_owned(),
        fields: vec!["user_id:uuid".to_owned(), "note:string?".to_owned()],
        queue: true,
        dry_run: false,
    };
    // `forge new` already declared the jobs and enabled the feature: the
    // job is only registered in `jobs_configure`.
    let job = job::generate(&root, &job_options).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        job.wiring.contains("at the end of `fn jobs_configure`"),
        "{}",
        job.wiring
    );
    assert!(!job.wiring.contains("WorkerMode"), "{}", job.wiring);
    assert!(!job.wiring.contains("add \"jobs\""), "{}", job.wiring);
    let source =
        fs::read_to_string(root.join("src/adapters/jobs/send_welcome.rs")).unwrap_or_default();
    assert!(source.contains("pub note: Option<String>,"), "{source}");
    assert!(source.contains("JobError::permanent("), "{source}");

    let (files, unwire) =
        mailer::plan_destroy(&root, &mailer_options).unwrap_or_else(|error| panic!("{error}"));
    let plan = DestroyPlan {
        files,
        unwire,
        owned_elsewhere: None,
    };
    destroyed(&root, plan, DestroyFlags::default()).unwrap_or_else(|error| panic!("{error}"));
    let (files, unwire) =
        job::plan_destroy(&root, &job_options).unwrap_or_else(|error| panic!("{error}"));
    let plan = DestroyPlan {
        files,
        unwire,
        owned_elsewhere: None,
    };
    destroyed(&root, plan, DestroyFlags::default()).unwrap_or_else(|error| panic!("{error}"));
    let mut after = tree(&root);
    assert!(
        after.remove(job::JOBS_MODULE).is_some(),
        "the jobs module is shared"
    );
    assert_eq!(after, before);
    assert!(!root.join("templates/mailers").exists());

    // An application generated before `forge new` wired jobs: the wiring
    // also declares them and enables the feature.
    let (_parent, older) = application(Database::Postgresql, true, Locale::En);
    let bootstrap = fs::read_to_string(older.join("src/bootstrap/mod.rs")).unwrap_or_default();
    let start = bootstrap
        .find("\n/// Configures the persistent job queue")
        .unwrap_or_else(|| panic!("{bootstrap}"));
    let bootstrap = bootstrap[..start]
        .replace("        .jobs(forge::WorkerMode::InServe)\n", "")
        .replace("        .compose(jobs_configure)\n", "");
    fs::write(older.join("src/bootstrap/mod.rs"), bootstrap)
        .unwrap_or_else(|error| panic!("{error}"));
    let manifest = fs::read_to_string(older.join("Cargo.toml")).unwrap_or_default();
    fs::write(older.join("Cargo.toml"), manifest.replace("\"jobs\", ", ""))
        .unwrap_or_else(|error| panic!("{error}"));
    let job = job::generate(&older, &job_options).unwrap_or_else(|error| panic!("{error}"));
    for wiring in [
        "at the end of `fn compose`",
        ".jobs(forge::WorkerMode::InServe)",
        "add \"jobs\"",
    ] {
        assert!(job.wiring.contains(wiring), "{wiring}: {}", job.wiring);
    }

    // Jobs need PostgreSQL; mailers need the mail test support.
    let (_parent, sqlite) = application(Database::Sqlite3, true, Locale::En);
    assert!(job::generate(&sqlite, &job_options).is_err());
    let (_parent, plain) = application(Database::Disabled, true, Locale::En);
    let refusal = mailer::generate(&plain, &mailer_options)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(refusal.contains("testing"), "{refusal}");
}

// --- refusals and rarely taken branches ---------------------------------------

use crate::CliError;
use crate::authentication;
use crate::generators::{self, Planned, Role};

/// Rewrites a file of the application.
fn edit(root: &Path, path: &str, change: impl FnOnce(&str) -> String) {
    let full = root.join(path);
    let source = fs::read_to_string(&full).unwrap_or_else(|error| panic!("{path}: {error}"));
    fs::write(&full, change(&source)).unwrap_or_else(|error| panic!("{path}: {error}"));
}

/// The text of a refusal; fails the test when the generator accepted.
fn refused<T>(result: Result<T, CliError>) -> String {
    match result {
        Ok(_) => panic!("the generator accepted the request"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_scaffold_refuses_an_application_it_cannot_wire() {
    let scaffold_post = |root: &Path, scope: Scope| {
        scaffold::generate(
            root,
            &resource("post", &["title:string"], scope, Layers::Scaffold),
            VERSION,
        )
    };

    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    edit(&root, "Cargo.toml", |manifest| {
        manifest
            .lines()
            .map(|line| {
                if line.starts_with("sqlx =") {
                    line.replace("\"postgres\"", "\"odbc\"")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    assert!(
        refused(scaffold_post(&root, Scope::Global))
            .contains("could not tell the database backend"),
        "an sqlx dependency without a known backend"
    );

    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    edit(&root, "src/bootstrap/mod.rs", |source| {
        source.replace(
            "crate::adapters::http::site(",
            "crate::adapters::http::elsewhere(",
        )
    });
    assert!(refused(scaffold_post(&root, Scope::Global)).contains("has no `compose` function"));

    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    fs::remove_file(root.join("src/application/use_cases/mod.rs")).expect("remove");
    assert!(
        refused(scaffold_post(&root, Scope::Global))
            .contains("src/application/use_cases/mod.rs is missing")
    );

    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    edit(&root, "src/infrastructure/database/mod.rs", |source| {
        source.replace(
            "pub async fn begin_tenant_transaction",
            "pub async fn begin_other",
        )
    });
    assert!(
        refused(scaffold_post(&root, Scope::Tenant)).contains("has no begin_tenant_transaction"),
        "a tenant-scoped PostgreSQL repository runs in the tenant transaction"
    );
    // The same application serves a global scaffold.
    scaffold_post(&root, Scope::Global).unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn a_model_for_mysql_prints_the_mysql_repository() {
    let (_parent, root) = application(Database::Mysql, true, Locale::En);
    let outcome = scaffold::generate(
        &root,
        &resource("post", &["title:string"], Scope::Global, Layers::Model),
        VERSION,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        outcome.wiring.contains("MySqlPostRepository"),
        "{}",
        outcome.wiring
    );
    assert!(!outcome.wiring.contains("PgPostRepository"));
}

/// Generates a scaffold with `fields` and returns everything it wrote.
fn scaffolded(fields: &[&str]) -> String {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let before = tree(&root);
    scaffold::generate(
        &root,
        &resource("item", fields, Scope::Global, Layers::Scaffold),
        VERSION,
    )
    .unwrap_or_else(|error| panic!("{fields:?}: {error}"));
    tree(&root)
        .into_iter()
        .filter(|(path, content)| before.get(path) != Some(content))
        .map(|(_, content)| content)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_generated_form_tests_break_the_first_field_that_can_be_broken() {
    // A required text field is broken by leaving it blank (covered by the
    // goldens); without one, the first typed field gets a value of the wrong
    // type.
    for (fields, broken, message) in [
        (
            &["count:integer"][..],
            "count=abc",
            "must be a whole number",
        ),
        (
            &["at:timestamp"],
            "at=not-a-time",
            "must be a date and time",
        ),
        (&["owner:uuid"], "owner=nope", "must be a UUID"),
        (&["done:boolean?"], "done=maybe", "must be yes, no or blank"),
    ] {
        let written = scaffolded(fields);
        assert!(
            written.contains(broken),
            "{fields:?}: {broken} not in the generated tests"
        );
        assert!(
            written.contains(message),
            "{fields:?}: {message} not in the generated tests"
        );
    }
    // Only optional text left: an over-long value, with the other fields
    // valid.
    let written = scaffolded(&["done:boolean", "note:string?"]);
    assert!(written.contains("is too long"), "{written}");
    assert!(
        written.contains("\"done=true&note={}\", \"x\".repeat(256)"),
        "{written}"
    );
    let written = scaffolded(&["note:text?"]);
    assert!(
        written.contains("\"note={}\", \"x\".repeat(65536)"),
        "{written}"
    );
    // Nothing breakable but a required checkbox: an unexpected field.
    let written = scaffolded(&["done:boolean"]);
    assert!(written.contains("unknown form field"), "{written}");
    // The show page names the first field's value in the form's own type.
    for (fields, shown) in [
        (&["count:integer"][..], "42"),
        (&["done:boolean"], "Yes"),
        (&["at:timestamp"], "2026-01-02T03:04:05Z"),
    ] {
        assert!(
            scaffolded(fields).contains(&format!("\"{shown}\"")),
            "{fields:?}"
        );
    }
}

#[test]
fn a_controller_can_be_planned_without_writing_and_serves_public_json() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let before = tree(&root);
    let mut dry = controller("pages", &["index", "about"]);
    dry.dry_run = true;
    let outcome = controller::generate(&root, &dry).unwrap_or_else(|error| panic!("{error}"));
    assert!(!outcome.files.is_empty());
    assert_eq!(tree(&root), before, "a dry run writes nothing");

    let mut api = controller("status", &["index", "details"]);
    api.api = true;
    api.public = true;
    let outcome = controller::generate(&root, &api).unwrap_or_else(|error| panic!("{error}"));
    let source =
        fs::read_to_string(root.join("src/adapters/http/status_controller.rs")).unwrap_or_default();
    assert!(
        source.contains("describe(index, operation(\"index\"))"),
        "{source}"
    );
    assert!(source.contains("\"/v1/status/details\""), "{source}");
    assert!(
        !source.contains("authorize("),
        "a public controller asks nobody"
    );
    assert!(!outcome.reused_access);
}

#[test]
fn controllers_refuse_what_they_cannot_guard_or_place() {
    let too_many: Vec<String> = (0..21).map(|index| format!("action{index}")).collect();
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let options = ControllerOptions {
        name: "pages".to_owned(),
        actions: too_many,
        api: false,
        public: false,
        dry_run: false,
    };
    assert!(refused(controller::generate(&root, &options)).contains("at most 20 actions"));

    let empty = tempfile::tempdir().expect("temporary directory");
    assert!(
        refused(controller::generate(
            empty.path(),
            &controller("pages", &["index"])
        ))
        .contains("no Cargo.toml in ")
    );

    let access = root.join(scaffold::ACCESS_PATH);
    fs::create_dir_all(access.parent().expect("parent")).expect("directory");
    fs::write(&access, "pub struct Unrelated;\n").expect("access module");
    assert!(
        refused(controller::generate(
            &root,
            &controller("pages", &["index"])
        ))
        .contains("has no `GlobalAccess` or `DenyAll`")
    );
    fs::write(
        &access,
        "pub trait GlobalAccess {\n    fn authorize(&self) -> Result<(), AccessDenied>;\n}\npub struct DenyAll;\n",
    )
    .expect("old access module");
    assert!(
        refused(controller::generate(
            &root,
            &controller("pages", &["index"])
        ))
        .contains("returns `Result<(), AccessDenied>`")
    );
    fs::remove_file(&access).expect("remove");
    fs::create_dir(&access).expect("a directory in its place");
    assert!(
        refused(controller::generate(
            &root,
            &controller("pages", &["index"])
        ))
        .contains("is not a readable file")
    );
}

#[cfg(unix)]
#[test]
fn a_source_tree_the_permission_scan_cannot_vouch_for_fails_closed_but_not_destroy() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    // A link to code under src/ could declare any permission.
    std::os::unix::fs::symlink(root.join("src/domain"), root.join("src/linked")).expect("symlink");
    let reason = refused(controller::generate(
        &root,
        &controller("pages", &["index"]),
    ));
    assert!(
        reason.contains("src/linked is a symbolic link under src/"),
        "{reason}"
    );
    // Destroy renders what the controller would have declared itself.
    controller::plan_destroy(&root, &controller("pages", &["index"]))
        .unwrap_or_else(|error| panic!("{error}"));

    // Another module declaring the permission: generation refuses, destroy
    // still plans the controller's own files.
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    fs::write(
        root.join("src/adapters/other.rs"),
        "const P: &str = \"pages:read\";\n",
    )
    .expect("other module");
    assert!(
        refused(controller::generate(
            &root,
            &controller("pages", &["index"])
        ))
        .contains("already declares the permission `pages:read`")
    );
    controller::plan_destroy(&root, &controller("pages", &["index"]))
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn mailers_refuse_requests_and_applications_they_cannot_serve() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let mailer = |name: &str, methods: &[String]| MailerOptions {
        name: name.to_owned(),
        methods: methods.to_vec(),
        dry_run: false,
    };
    let many: Vec<String> = (0..21).map(|index| format!("email{index}")).collect();
    assert!(refused(mailer::generate(&root, &mailer("user", &many))).contains("at most 20 emails"));
    let fields: Vec<String> = (0..21).map(|index| format!("f{index}")).collect();
    let declaration = format!("welcome:{}", fields.join(","));
    assert!(
        refused(mailer::generate(&root, &mailer("user", &[declaration])))
            .contains("email `welcome` has more than 20 fields")
    );
    // `subject` and `welcome` both generate the type `WelcomeSubject` in the
    // mailer `welcome`.
    let reason = refused(mailer::generate(
        &root,
        &mailer("welcome", &["subject".to_owned(), "welcome".to_owned()]),
    ));
    assert!(
        reason.contains("generates the type `WelcomeSubject`, which email `subject`"),
        "{reason}"
    );

    let mut dry = mailer("user", &["welcome:name".to_owned()]);
    dry.dry_run = true;
    let before = tree(&root);
    mailer::generate(&root, &dry).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(tree(&root), before, "a dry run writes nothing");

    let empty = tempfile::tempdir().expect("temporary directory");
    assert!(
        refused(mailer::generate(
            empty.path(),
            &mailer("user", &["welcome".to_owned()])
        ))
        .contains("no Cargo.toml in ")
    );
    let send = |root: &Path| mailer::generate(root, &mailer("user", &["welcome".to_owned()]));

    let (_parent, no_askama) = application(Database::Postgresql, true, Locale::En);
    edit(&no_askama, "Cargo.toml", |manifest| {
        manifest
            .lines()
            .filter(|line| !line.starts_with("askama ="))
            .collect::<Vec<_>>()
            .join("\n")
    });
    assert!(refused(send(&no_askama)).contains("mailers render askama templates"));

    let (_parent, no_testing) = application(Database::Postgresql, true, Locale::En);
    edit(&no_testing, "Cargo.toml", |manifest| {
        manifest.replace("\"testing\"", "\"other\"")
    });
    assert!(refused(send(&no_testing)).contains("forge::mail::testing"));

    let (_parent, no_module) = application(Database::Postgresql, true, Locale::En);
    fs::remove_file(no_module.join("src/adapters/email/mod.rs")).expect("remove");
    assert!(refused(send(&no_module)).contains("src/adapters/email/mod.rs is missing"));
}

#[test]
fn jobs_refuse_requests_and_applications_they_cannot_serve() {
    let (_parent, root) = application(Database::Postgresql, true, Locale::En);
    let job = |name: &str, fields: &[&str]| JobOptions {
        name: name.to_owned(),
        fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        queue: true,
        dry_run: false,
    };
    assert!(refused(job::generate(&root, &job("mod", &[]))).contains("`mod` is a Rust keyword"));
    let many: Vec<String> = (0..21).map(|index| format!("f{index}:integer")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(refused(job::generate(&root, &job("report", &many))).contains("at most 20 fields"));

    // A timestamp payload field has a sample the test can serialize.
    let outcome = job::generate(&root, &job("stamp", &["at:timestamp"]))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(!outcome.files.is_empty());
    let source = fs::read_to_string(root.join("src/adapters/jobs/stamp.rs")).unwrap_or_default();
    assert!(
        source.contains("pub at: forge::core::Timestamp,"),
        "{source}"
    );

    let before = tree(&root);
    let mut dry = job("other", &["note:string"]);
    dry.dry_run = true;
    job::generate(&root, &dry).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(tree(&root), before, "a dry run writes nothing");

    let empty = tempfile::tempdir().expect("temporary directory");
    assert!(
        refused(job::generate(empty.path(), &job("report", &[]))).contains("no Cargo.toml in ")
    );
}

#[test]
fn authentication_refuses_applications_without_what_its_pages_need() {
    let (_parent, no_testing) = application(Database::Postgresql, true, Locale::En);
    edit(&no_testing, "Cargo.toml", |manifest| {
        manifest.replace("\"testing\"", "\"other\"")
    });
    assert!(refused(authentication::generate(&no_testing, true)).contains("forge::db::testing"));

    let (_parent, no_compose) = application(Database::Postgresql, true, Locale::En);
    edit(&no_compose, "src/bootstrap/mod.rs", |source| {
        source.replace(
            "crate::adapters::http::site(",
            "crate::adapters::http::elsewhere(",
        )
    });
    assert!(
        refused(authentication::generate(&no_compose, true)).contains("has no `compose` function")
    );

    let (_parent, no_locales) = application(Database::Postgresql, true, Locale::En);
    fs::remove_file(no_locales.join("src/adapters/http/views/locales.rs")).expect("remove");
    assert!(refused(authentication::generate(&no_locales, true)).contains("locales.rs is missing"));
}

#[test]
fn the_shared_helpers_tell_apart_what_they_scan() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();

    // A path below a regular file is neither missing nor a plain path.
    fs::write(root.join("file"), "x").expect("file");
    let reason = generators::refuse_symlinks(root, ["file/inside"]).expect_err("not a directory");
    assert!(reason.starts_with("file/inside: "), "{reason}");
    assert!(generators::refuse_symlinks(root, ["missing/inside"]).is_ok());

    // Migrations: only `<digits>_<name>.up|down.sql` count.
    fs::create_dir(root.join("migrations")).expect("migrations");
    for name in [
        "1_post.up.sql",
        "2_post.down.sql",
        "abc_post.up.sql",
        "_post.up.sql",
        "nounderscore.sql",
        "3_other.up.sql",
    ] {
        fs::write(root.join("migrations").join(name), "").expect("migration");
    }
    let (up, down) = generators::existing_migrations(root, "post");
    assert_eq!(up, ["migrations/1_post.up.sql"]);
    assert_eq!(down, ["migrations/2_post.down.sql"]);

    // The permission scan: `src` that is a file cannot be listed; a name
    // without a namespace is only found as a whole literal.
    fs::write(root.join("src"), "x").expect("src as a file");
    let reason = generators::permission_owner(root, &["posts:read".to_owned()], &[])
        .expect_err("src is a file");
    assert!(
        reason.starts_with("could not read src to look for permissions: "),
        "{reason}"
    );
    let (_parent, app) = application(Database::Disabled, true, Locale::En);
    fs::write(
        app.join("src/extra.rs"),
        "const A: &str = concat!(left, right);\n",
    )
    .expect("source");
    assert_eq!(
        generators::permission_owner(&app, &["plain".to_owned()], &[]).expect("scan"),
        None
    );

    // The file list printed by every generator.
    let files = [
        Planned::new("src/a.rs", "", Role::Owned),
        Planned::new("src/b.rs", "", Role::Shared),
    ];
    let mut output = Vec::new();
    generators::report(&mut output, false, &files).expect("report");
    generators::report(&mut output, true, &files[..1]).expect("dry report");
    assert_eq!(
        String::from_utf8_lossy(&output),
        "  create       src/a.rs\n  create       src/b.rs\n  would create src/a.rs\n"
    );
}
