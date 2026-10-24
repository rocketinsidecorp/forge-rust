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
