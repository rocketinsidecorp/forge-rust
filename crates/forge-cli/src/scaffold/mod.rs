//! `forge generate scaffold`: a server-rendered HTML CRUD resource through
//! every hexagonal ring, for the application's database backend
//! (ADR 0022, decision 7), plus its JSON API; `forge generate resource`:
//! the model and the JSON API under `/v1/<plural>` (ADR 0045); and
//! `forge generate model`: the resource without its HTTP layer (ADR 0038).
//!
//! The generator writes only new files, all or nothing, and never edits an
//! existing file: the module declarations and the composition-root lines
//! that wire the resource are printed for the developer to add, like the
//! generated code they are reviewable.

mod render;
mod spec;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) use render::Backend;
pub(crate) use render::File;
pub(crate) use render::column_migration;
pub(crate) use render::{ACCESS_PATH, API_ACCESS_PATH};
use render::{Api, Broadcast, STREAMS_PATH};
pub(crate) use spec::Scope;
pub(crate) use spec::{parse_columns, pascal_case, sentence_case};

use crate::CliError;
use crate::destroy::Unwire;
use crate::generate::write_new_file;
use crate::generators::{self, Planned, Role};
use crate::wiring::{self, Block, Registration};
use spec::Resource;

/// Which layers of a resource a generator writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Layers {
    /// `forge generate model`: migration, entity, repository port, use
    /// cases, repository adapter and its tests.
    Model,
    /// `forge generate resource`: the model plus the JSON API.
    Resource,
    /// `forge generate scaffold`: the resource plus the HTML pages.
    Scaffold,
}

impl Layers {
    fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Resource => "resource",
            Self::Scaffold => "scaffold",
        }
    }

    /// Whether the generation has the JSON API under `/v1/<plural>`.
    fn api(self) -> bool {
        self != Self::Model
    }
}

/// Arguments of `forge generate scaffold` and `forge generate model`.
#[derive(Debug, Clone)]
pub(crate) struct ScaffoldOptions {
    pub(crate) name: String,
    pub(crate) fields: Vec<String>,
    pub(crate) scope: Scope,
    pub(crate) plural: Option<String>,
    pub(crate) broadcast: bool,
    /// `--public`: a global resource anyone may use (applications without
    /// Forge identity only).
    pub(crate) public: bool,
    pub(crate) dry_run: bool,
    pub(crate) layers: Layers,
}

/// What the generator did (or, with `--dry-run`, would do).
#[derive(Debug)]
pub(crate) struct Outcome {
    /// Files created, relative to the application root, in write order.
    pub(crate) created: Vec<String>,
    /// The files existed already, identical: nothing was created.
    pub(crate) identical: bool,
    /// Whether the shared access module already existed and was reused.
    pub(crate) reused_access: bool,
    /// Whether the shared streams module already existed and was reused.
    pub(crate) reused_streams: bool,
    /// Whether the shared API access module already existed and was reused.
    pub(crate) reused_api_access: bool,
    /// The module declarations and routes the generator registered.
    pub(crate) edits: Vec<wiring::Edit>,
    /// What to print after the files: the notes, or the wiring to add by
    /// hand in an application without the seam.
    pub(crate) wiring: String,
}

/// What the generator learned about the application in `root`.
#[derive(Debug)]
struct Application {
    backend: Backend,
    access_exists: bool,
    streams_exists: bool,
    api_access_exists: bool,
    /// The PostgreSQL bearer session store of `--api-auth` exists.
    bearer_sessions: bool,
    /// Created with `forge new --api`: no views, no sign-in, nothing that
    /// issues bearer tokens.
    api_only: bool,
    /// The application's default locale (`DEFAULT_LOCALE` of
    /// [`LOCALES_MODULE`]).
    default_locale: String,
    /// Forge identity: the sign-in pages of `forge new` (full-stack
    /// PostgreSQL) or `forge generate authentication`.
    identity: bool,
    /// The access module's `SessionTenantAccess` also authorizes the JSON
    /// API (it is written now, or was written with that impl).
    session_access: bool,
    /// This generation writes the shared access module.
    writes_access: bool,
}

impl Application {
    /// Whether the tenant-scoped JSON API takes the pages' session cookie:
    /// PostgreSQL, sign-in pages, no bearer tokens.
    fn session_api(&self) -> bool {
        self.backend == Backend::Postgresql
            && self.identity
            && !self.bearer_sessions
            && !self.api_only
            && self.session_access
    }
}

/// The language of the text the scaffold writes.
const DEFAULT_TEXT_LOCALE: &str = "en";

/// The PostgreSQL bearer session store of `forge new --api-auth` (and
/// `forge generate authentication --api-auth`).
const BEARER_SESSIONS_MODULE: &str = "src/infrastructure/bearer_sessions/mod.rs";

/// The module `forge new` writes with the translator and the application's
/// `DEFAULT_LOCALE` (ADR 0037).
pub(crate) const LOCALES_MODULE: &str = "src/adapters/http/views/locales.rs";

/// The application's default locale: the `DEFAULT_LOCALE` constant of
/// [`LOCALES_MODULE`] (`forge new --locale`), else `en` when it is missing
/// or not a valid locale. `FORGE_I18N_DEFAULT_LOCALE` is a deployment
/// override and does not apply.
pub(crate) fn application_default_locale(root: &Path) -> String {
    read(root, LOCALES_MODULE)
        .and_then(|source| {
            source.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("pub const DEFAULT_LOCALE: &str = \"")
                    .and_then(|rest| rest.strip_suffix("\";"))
                    .and_then(|value| forge_i18n::canonical_locale(value).ok())
            })
        })
        .unwrap_or_else(|| DEFAULT_TEXT_LOCALE.to_owned())
}

/// The shared access-policy module (`src/adapters/http/access.rs`) the
/// first scaffold or guarded controller writes, before formatting.
pub(crate) fn access_module() -> String {
    render::ACCESS.to_owned()
}

/// Whether a plan is for writing the files or for `forge destroy`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Purpose {
    Generate,
    Destroy,
}

/// A rendered resource and what was learned on the way.
struct Plan {
    resource: Resource,
    application: Application,
    broadcast: Broadcast,
    files: Vec<Planned>,
    /// Every file already exists as the generator would write it.
    identical: bool,
}

/// Generates the scaffold (or model) in the application at `root`.
/// `migration` is the numeric version prefix of the migration pair.
pub(crate) fn generate(
    root: &Path,
    options: &ScaffoldOptions,
    migration: &str,
) -> Result<Outcome, CliError> {
    let layers = options.layers;
    if options.public && !wiring::has_seam(root) {
        return Err(fail(
            layers,
            format!(
                "--public needs the routes seam ({}, which `compose` calls in applications \
                 `forge new` creates now); without it the routes are wired by hand, so mount \
                 them with your own access policy instead",
                wiring::ROUTES_FILE
            ),
        ));
    }
    let plan = plan(root, options, Some(migration), Purpose::Generate)?;
    let wiring = plan_wiring(&plan, options, true);
    let (text, edits) = if wiring::has_seam(root) {
        let edits = wiring::plan(root, &wiring.registration(), false, &|reason| {
            fail(layers, reason)
        })?;
        (wiring.seam_notes(root)?, edits)
    } else {
        (wiring.printed(root)?, Vec::new())
    };
    if !options.dry_run && plan.identical {
        wiring::apply(root, &edits)?;
    } else if !options.dry_run {
        generators::write_and_wire(root, &plan.files, &edits)?;
    }
    let scaffold = options.layers == Layers::Scaffold;
    Ok(Outcome {
        created: plan.files.iter().map(|file| file.path.clone()).collect(),
        identical: plan.identical,
        reused_access: scaffold && plan.application.access_exists,
        reused_streams: scaffold && options.broadcast && plan.application.streams_exists,
        reused_api_access: options.layers.api() && plan.application.api_access_exists,
        edits,
        wiring: text,
    })
}

/// The wiring of `plan`: for a generation, with the shared modules it
/// writes; for a destroy, only what the generation owns.
fn plan_wiring(plan: &Plan, options: &ScaffoldOptions, generating: bool) -> Wiring {
    let application = &plan.application;
    match options.layers {
        Layers::Scaffold => wiring(
            &plan.resource,
            application,
            generating && !application.access_exists,
            generating && !application.api_access_exists,
            Broadcast {
                enabled: options.broadcast,
                streams_module: generating && plan.broadcast.streams_module,
            },
            options.public,
        ),
        Layers::Resource => resource_wiring(
            &plan.resource,
            application,
            generating && !application.api_access_exists,
            generating && application.writes_access,
            options.public,
        ),
        Layers::Model => model_wiring(&plan.resource, application.backend),
    }
}

/// What `forge destroy scaffold|model` removes: the files the generator
/// writes from `options`, found where they are (the migrations at the
/// version they have), without the shared access and streams modules; what
/// to take out of the module declarations and the routes seam (or, without
/// the seam, the printed wiring to take out by hand); and, when a bigger or
/// dependent generation still needs the files, why.
pub(crate) fn plan_destroy(
    root: &Path,
    options: &ScaffoldOptions,
) -> Result<(Vec<Planned>, Unwire, Option<String>), CliError> {
    let plan = plan(root, options, None, Purpose::Destroy)?;
    let plural = &plan.resource.plural;
    let mut owned_elsewhere = None;
    let pages = format!("src/adapters/http/{plural}.rs");
    let api = format!("src/adapters/http/{plural}_api.rs");
    let bigger = match options.layers {
        Layers::Model => [(&pages, "scaffold"), (&api, "resource")]
            .into_iter()
            .find(|(path, _)| fs::symlink_metadata(root.join(path)).is_ok()),
        Layers::Resource => Some((&pages, "scaffold"))
            .filter(|(path, _)| fs::symlink_metadata(root.join(path)).is_ok()),
        Layers::Scaffold => None,
    };
    if let Some((path, generator)) = bigger {
        let kind = options.layers.name();
        owned_elsewhere = Some(format!(
            "{path} exists, and `forge generate {generator} {}` writes it around this {kind}: \
             the {kind}'s files belong to that {generator}, and removing them alone would \
             leave code that no longer compiles. Run `forge destroy {generator}` with the \
             arguments it was generated with, or pass --force to remove only the {kind}'s \
             files",
            plan.resource.singular
        ));
    }
    let controller = format!("src/adapters/http/{plural}_controller.rs");
    if owned_elsewhere.is_none()
        && read(root, &controller)
            .is_some_and(|source| source.contains(&format!("use_cases::{plural}::READ_PERMISSION")))
    {
        owned_elsewhere = Some(format!(
            "{controller} reuses the permission `{plural}:read` of this {}, and would no \
             longer compile without it. Run `forge destroy controller {plural}` with the \
             actions it was generated with first, or pass --force",
            options.layers.name()
        ));
    }
    let wiring = plan_wiring(&plan, options, false);
    let layers = options.layers;
    let unwire = if wiring::has_seam(root) && owned_elsewhere.is_none() {
        Unwire::Edits(wiring::plan(
            root,
            &wiring.registration(),
            true,
            &|reason| {
                CliError::Destroy(format!(
                    "refusing to destroy, nothing was removed: {reason}"
                ))
            },
        )?)
    } else if wiring::has_seam(root) {
        // --force removes the files only; the bigger generation keeps its
        // registrations.
        let _ = layers;
        Unwire::Edits(Vec::new())
    } else {
        Unwire::Printed(wiring.printed(root)?)
    };
    Ok((plan.files, unwire, owned_elsewhere))
}

fn plan(
    root: &Path,
    options: &ScaffoldOptions,
    migration: Option<&str>,
    purpose: Purpose,
) -> Result<Plan, CliError> {
    let layers = options.layers;
    let resource = spec::parse(
        &options.name,
        &options.fields,
        options.scope,
        options.plural.as_deref(),
    )
    .map_err(|invalid| fail(layers, invalid.to_string()))?;
    let broadcast_requested = layers == Layers::Scaffold && options.broadcast;
    let application = inspect(
        root,
        &resource,
        broadcast_requested,
        options.public,
        layers,
        purpose,
    )?;
    let broadcast = Broadcast {
        enabled: broadcast_requested,
        streams_module: broadcast_requested
            && !application.streams_exists
            && purpose == Purpose::Generate,
    };
    // The text is English; an application whose default locale is another
    // one gets it under that locale too, so production never falls back to
    // humanized keys, and `forge i18n check` finds nothing missing.
    let mut locales = vec![DEFAULT_TEXT_LOCALE.to_owned()];
    if application.default_locale != DEFAULT_TEXT_LOCALE {
        locales.push(application.default_locale.clone());
    }
    let write_access = application.writes_access;
    let api = Api {
        enabled: layers.api(),
        access_module: layers.api()
            && !application.api_access_exists
            && purpose == Purpose::Generate,
    };
    let rendered = render::files(
        &resource,
        application.backend,
        migration.unwrap_or("0"),
        write_access,
        broadcast,
        api,
        &locales,
    );
    let plural = &resource.plural;
    let migration_name = format!("create_{plural}");
    let model_paths = [
        format!("src/domain/entities/{}.rs", resource.singular),
        format!("src/application/ports/{}_repository.rs", resource.singular),
        format!("src/application/use_cases/{plural}.rs"),
        format!("src/infrastructure/database/{plural}.rs"),
        format!("tests/{plural}_repository.rs"),
    ];
    let mut files = Vec::new();
    for file in rendered {
        if file.path.starts_with("migrations/") {
            if purpose == Purpose::Destroy {
                continue;
            }
            files.push(Planned::new(file.path, file.content, Role::Migration));
            continue;
        }
        let api_paths = [
            format!("src/adapters/http/{plural}_api.rs"),
            format!("tests/{plural}_api.rs"),
            API_ACCESS_PATH.to_owned(),
        ];
        let kept = match layers {
            Layers::Model => model_paths.contains(&file.path),
            Layers::Resource => {
                model_paths.contains(&file.path)
                    || api_paths.contains(&file.path)
                    || (write_access && file.path == ACCESS_PATH)
            }
            Layers::Scaffold => true,
        };
        if !kept {
            continue;
        }
        let role = if file.path == ACCESS_PATH
            || file.path == STREAMS_PATH
            || file.path == API_ACCESS_PATH
        {
            Role::Shared
        } else if file.path.starts_with("config/locales/")
            && !file.path.ends_with(&format!(".{DEFAULT_TEXT_LOCALE}.json"))
        {
            Role::Translation
        } else {
            Role::Owned
        };
        files.push(Planned::new(file.path, file.content, role));
    }
    if purpose == Purpose::Destroy {
        let (up, down) = render::migration(&resource, application.backend);
        let (ups, downs) = generators::existing_migrations(root, &migration_name);
        let mut migrations: Vec<Planned> = ups
            .into_iter()
            .map(|path| Planned::new(path, up.clone(), Role::Migration))
            .chain(
                downs
                    .into_iter()
                    .map(|path| Planned::new(path, down.clone(), Role::Migration)),
            )
            .collect();
        migrations.append(&mut files);
        files = migrations;
        generators::refuse_symlinks(root, files.iter().map(|file| file.path.as_str()))
            .map_err(|reason| fail(layers, reason))?;
    }
    for file in files.iter_mut().filter(|file| file.path.ends_with(".rs")) {
        file.content = rustfmt(root, &file.path, &file.content)?;
    }
    let identical = purpose == Purpose::Generate && rerun(root, &migration_name, &mut files);
    if purpose == Purpose::Generate && !identical {
        refuse_collisions(root, &resource, &files, layers)?;
        refuse_second_permission_owner(root, &resource, layers)?;
    }
    Ok(Plan {
        resource,
        application,
        broadcast,
        files,
        identical,
    })
}

/// Whether every file of the generation already exists with the content it
/// would get (the migration pair at the version it has): running the
/// generator again with the same arguments, which changes nothing. Points
/// the planned migrations at the existing ones when so.
fn rerun(root: &Path, migration_name: &str, files: &mut [Planned]) -> bool {
    let (ups, downs) = generators::existing_migrations(root, migration_name);
    let ([up], [down]) = (ups.as_slice(), downs.as_slice()) else {
        return false;
    };
    let mut candidate = files.to_vec();
    for file in &mut candidate {
        if file.role == Role::Migration {
            file.path = if file.path.ends_with(".up.sql") {
                up.clone()
            } else {
                down.clone()
            };
        }
    }
    let identical =
        generators::refuse_symlinks(root, candidate.iter().map(|file| file.path.as_str())).is_ok()
            && candidate.iter().all(|file| {
                fs::read(root.join(&file.path))
                    .is_ok_and(|current| current == file.content.as_bytes())
            });
    if identical {
        files.clone_from_slice(&candidate);
    }
    identical
}

/// Refuses a resource whose permissions another module already declares:
/// every permission has one owner (ADR 0038).
fn refuse_second_permission_owner(
    root: &Path,
    resource: &Resource,
    layers: Layers,
) -> Result<(), CliError> {
    let plural = &resource.plural;
    let permissions = [format!("{plural}:read"), format!("{plural}:write")];
    let Some((owner, permission)) = generators::permission_owner(root, &permissions, &[])
        .map_err(|reason| fail(layers, reason))?
    else {
        return Ok(());
    };
    let kind = layers.name();
    Err(fail(
        layers,
        format!(
            "{owner} already declares the permission `{permission}`, which the {kind}'s use \
             cases would declare too. A permission belongs to the one module that declares \
             it, so it never gets a second one. Choose another name or --plural; or, to let \
             the {kind} own `{permission}`, destroy that generation first (a controller \
             generated afterwards reuses the {kind}'s permission)"
        ),
    ))
}

/// Formats generated Rust with the application's pinned `rustfmt` (edition
/// 2024, the application's `rustfmt.toml` if any), exactly as
/// `cargo fmt` will. A formatter failure aborts before anything is written.
pub(crate) fn rustfmt(root: &Path, path: &str, source: &str) -> Result<String, CliError> {
    let failed = |detail: String| {
        CliError::Scaffold(format!(
            "rustfmt could not format the generated {path}: {detail} (rustfmt is a component \
             of the toolchain pinned in rust-toolchain.toml: `rustup component add rustfmt`)"
        ))
    };
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failed(error.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(source.as_bytes())
            .map_err(|error| failed(error.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| failed(error.to_string()))
}

/// Whether the `GlobalAccess` trait of an existing access module has the
/// current shape, returning the admitted principal (an `Option`), rather
/// than the `Result<(), AccessDenied>` of applications generated before.
pub(crate) fn global_access_returns_principal(access: &str) -> bool {
    access
        .split_once("pub trait GlobalAccess")
        .and_then(|(_, rest)| rest.split_once('}'))
        .is_some_and(|(declaration, _)| declaration.contains("Option<"))
}

fn unsupported<T>(reason: impl Into<String>) -> Result<T, CliError> {
    Err(CliError::Scaffold(reason.into()))
}

/// The refusal of `layers`' generator.
fn fail(layers: Layers, reason: String) -> CliError {
    match layers {
        Layers::Scaffold => CliError::Scaffold(reason),
        Layers::Model | Layers::Resource => generators::generate_error(layers.name(), reason),
    }
}

/// Labels the refusals of [`inspect_application`] with the generator.
fn inspect(
    root: &Path,
    resource: &Resource,
    broadcast: bool,
    public: bool,
    layers: Layers,
    purpose: Purpose,
) -> Result<Application, CliError> {
    inspect_application(root, resource, broadcast, public, layers, purpose).map_err(|error| {
        match error {
            CliError::Scaffold(reason) => fail(layers, reason),
            other => other,
        }
    })
}

fn read(root: &Path, relative: &str) -> Option<String> {
    fs::read_to_string(root.join(relative)).ok()
}

/// Detects the backend and shape from the files `forge new` generated and
/// refuses applications the scaffold (or model) cannot serve. A destroy
/// plan skips the checks of the shared modules it never removes.
fn inspect_application(
    root: &Path,
    resource: &Resource,
    broadcast: bool,
    public: bool,
    layers: Layers,
    purpose: Purpose,
) -> Result<Application, CliError> {
    let scaffold = layers == Layers::Scaffold;
    let api = layers.api();
    let generating = purpose == Purpose::Generate;
    let Some(manifest) = read(root, "Cargo.toml") else {
        return unsupported(format!(
            "no Cargo.toml in {}; run `forge generate {}` in the root of an application \
             created by `forge new`",
            root.display(),
            layers.name()
        ));
    };
    let dependency = |name: &str| {
        manifest
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{name} =")))
            .map(str::to_owned)
    };
    if scaffold
        && (dependency("askama").is_none()
            || !root.join("src/adapters/http/views/mod.rs").is_file())
    {
        return unsupported(
            "this application has no views (it was created with `forge new --api`). The \
             scaffold generates server-rendered HTML pages next to its JSON API; \
             `forge generate resource` writes the JSON API alone",
        );
    }
    let Some(sqlx) = dependency("sqlx") else {
        return unsupported(
            "this application has no database (`forge new -d none`); models and scaffolds \
             need PostgreSQL, MySQL, MariaDB or SQLite. Create a database application with \
             `forge new <name> -d <database>`",
        );
    };
    let backend = if let Some(backend) = backend_of(&sqlx) {
        backend
    } else {
        return unsupported(
            "could not tell the database backend from the sqlx features in Cargo.toml \
             (expected \"postgres\", \"mysql\" or \"sqlite\")",
        );
    };
    if !manifest.contains("\"testing\"") {
        return unsupported(
            "the generated repository tests need forge::db::testing: add the `forge` \
             dependency to [dev-dependencies] again with `features = [..., \"testing\"]` \
             (as `forge new` generates it)",
        );
    }
    let compose = read(root, "src/bootstrap/mod.rs").unwrap_or_default();
    if layers == Layers::Resource && generating && !compose.contains("\nfn compose(") {
        return unsupported(
            "src/bootstrap/mod.rs has no `fn compose`, where the printed wiring builds the \
             repository and mounts the JSON routes. `forge new` generates it (with `--api` \
             too): add it as `forge new` writes it, or wire the use cases and `routes` by hand",
        );
    }
    if scaffold && generating && !compose.contains("crate::adapters::http::site(") {
        return unsupported(
            "src/bootstrap/mod.rs has no `compose` function that loads the site with \
             `crate::adapters::http::site(..)`; the printed wiring needs it (see the \
             `compose` function generated by `forge new`)",
        );
    }
    if scaffold && generating && !root.join(LOCALES_MODULE).is_file() {
        return unsupported(format!(
            "{LOCALES_MODULE} is missing: the pages are translated with `layout.i18n` and \
             config/locales (ADR 0037). Add them as \"Applications created before i18n\" in \
             docs/i18n.md describes, then run the scaffold again"
        ));
    }
    for module in [
        "src/domain/entities/mod.rs",
        "src/application/ports/mod.rs",
        "src/application/use_cases/mod.rs",
        "src/infrastructure/database/mod.rs",
        "src/infrastructure/security/mod.rs",
        "src/adapters/http/mod.rs",
    ] {
        let model_module = !module.starts_with("src/infrastructure/security")
            && !module.starts_with("src/adapters/http");
        let needed = model_module || scaffold || (api && module == "src/adapters/http/mod.rs");
        if needed && !root.join(module).is_file() {
            return unsupported(format!(
                "{module} is missing; the scaffold adds modules next to the ones `forge new` \
                 generates"
            ));
        }
    }
    if backend == Backend::Postgresql && resource.scope == Scope::Tenant {
        let database = read(root, "src/infrastructure/database/mod.rs").unwrap_or_default();
        if !database.contains("pub async fn begin_tenant_transaction") {
            return unsupported(
                "src/infrastructure/database/mod.rs has no begin_tenant_transaction; \
                 tenant-scoped PostgreSQL repositories run in it for row-level security",
            );
        }
    }
    let identity = root.join("src/adapters/http/authentication.rs").is_file();
    let public = options_public(public, layers, purpose, resource, identity)?;
    let access_source = read(root, ACCESS_PATH);
    let bearer_sessions = root.join(BEARER_SESSIONS_MODULE).is_file();
    let api_only = !root.join("src/adapters/http/views/mod.rs").is_file();
    // A tenant-scoped JSON resource of an application with the sign-in pages
    // and no bearer tokens takes the pages' session (D29), so it needs the
    // access module's SessionTenantAccess as the scaffold does.
    let resource_session = layers == Layers::Resource
        && resource.scope == Scope::Tenant
        && backend == Backend::Postgresql
        && identity
        && !bearer_sessions
        && !api_only;
    let writes_access = generating && access_source.is_none() && (scaffold || resource_session);
    let session_access = access_source
        .as_deref()
        .map_or(scaffold || resource_session, |access| {
            access.contains("impl ApiTenantAccess for SessionTenantAccess")
        });
    if writes_access {
        let security = read(root, "src/infrastructure/security/mod.rs").unwrap_or_default();
        if !security.contains("pub fn tenant_from_request") {
            return unsupported(TENANT_COOKIE_MISSING);
        }
    }
    let access_exists = match access_source {
        None => false,
        Some(_) if !scaffold || !generating => true,
        Some(access) => {
            if resource.scope == Scope::Tenant && !access.contains("tenant_from_request(") {
                return unsupported(format!(
                    "{ACCESS_PATH} predates tenant-scoped pages at /<plural>: its \
                     SessionTenantAccess reads the tenant from `/tenants/{{tenant_id}}/...` only. \
                     Move the file aside so the scaffold writes the current one (and see \
                     \"Tenant-scoped routes\" in docs/generators.md for the tenant cookie), \
                     then run the scaffold again"
                ));
            }
            if public && !access.contains("pub struct Public") {
                return unsupported(format!(
                    "{ACCESS_PATH} has no `Public` access policy, which --public wires. Add \
                     `pub struct Public;` implementing GlobalAccess with `Ok(None)`, or move \
                     the file aside so the scaffold writes the current one"
                ));
            }
            for item in [
                "pub trait GlobalAccess",
                "pub trait TenantAccess",
                "pub struct DenyAll",
                "pub struct SessionTenantAccess",
            ] {
                if !access.contains(item) {
                    return unsupported(format!(
                        "{ACCESS_PATH} exists but has no `{item}`; generated routes depend on \
                         it. Restore it or move your file aside"
                    ));
                }
            }
            if (resource.scope == Scope::Global || broadcast)
                && !global_access_returns_principal(&access)
            {
                return unsupported(format!(
                    "`GlobalAccess::authorize` in {ACCESS_PATH} returns `Result<(), AccessDenied>`; \
                     generated code now expects `Result<Option<PrincipalId>, AccessDenied>` (the \
                     signed-in principal it admitted, `None` for an anonymous caller), so that \
                     live-update streams limit each person rather than each address. Change the \
                     trait, `DenyAll` and your own implementations (`Ok(())` becomes \
                     `Ok(Some(principal_id))`, or `Ok(None)` for public access) and run the \
                     scaffold again"
                ));
            }
            true
        }
    };
    let streams_exists = match read(root, STREAMS_PATH) {
        Some(streams) if broadcast && generating => {
            for item in [
                "pub struct Streams",
                "pub fn allow_tenant",
                "pub fn allow_global",
            ] {
                if !streams.contains(item) {
                    return unsupported(format!(
                        "{STREAMS_PATH} exists but has no `{item}`; generated routes depend on \
                         it. Restore it or move your file aside"
                    ));
                }
            }
            true
        }
        Some(_) => true,
        None => false,
    };
    if broadcast && generating {
        let forge = dependency("forge").unwrap_or_default();
        if !forge.contains("\"broadcast\"") {
            return unsupported(
                "--broadcast needs the `broadcast` feature of the forge dependency, which \
                 `forge new` now enables: add \"broadcast\" to the features of `forge` in \
                 Cargo.toml (see \"Live updates\" in docs/views.md)",
            );
        }
    }
    let api_access_exists = match read(root, API_ACCESS_PATH) {
        None => false,
        Some(_) if !api || !generating => true,
        Some(access) => {
            if public && !access.contains("pub struct Public") {
                return unsupported(format!(
                    "{API_ACCESS_PATH} has no `Public` access policy, which --public wires. Add \
                     `pub struct Public;` implementing ApiGlobalAccess with `Ok(())`, or move \
                     the file aside so the generator writes the current one"
                ));
            }
            for item in [
                "pub trait ApiTenantAccess",
                "pub trait ApiGlobalAccess",
                "pub struct DenyAll",
                "pub struct BearerTenantAccess",
            ] {
                if !access.contains(item) {
                    return unsupported(format!(
                        "{API_ACCESS_PATH} exists but has no `{item}`; generated JSON routes \
                         depend on it. Restore it or move your file aside"
                    ));
                }
            }
            true
        }
    };
    Ok(Application {
        backend,
        access_exists,
        streams_exists,
        api_access_exists,
        bearer_sessions,
        api_only,
        writes_access,
        default_locale: application_default_locale(root),
        identity,
        session_access,
    })
}

/// The backend the `sqlx` dependency line of Cargo.toml selects.
fn backend_of(sqlx: &str) -> Option<Backend> {
    if sqlx.contains("\"postgres\"") {
        Some(Backend::Postgresql)
    } else if sqlx.contains("\"mysql\"") {
        Some(Backend::MySql)
    } else if sqlx.contains("\"sqlite\"") {
        Some(Backend::Sqlite)
    } else {
        None
    }
}

/// The database backend of the application at `root`, from the features
/// of its `sqlx` dependency.
pub(crate) fn application_backend(root: &Path) -> Option<Backend> {
    read(root, "Cargo.toml")?
        .lines()
        .find(|line| line.trim_start().starts_with("sqlx ="))
        .and_then(backend_of)
}

/// Why an application's security module cannot serve a new access module.
pub(crate) const TENANT_COOKIE_MISSING: &str = "src/infrastructure/security/mod.rs has no \
    `tenant_from_request`, which the access module's SessionTenantAccess uses to find the \
    tenant of tenant-scoped pages at /<plural>: add `TENANT_COOKIE_NAME`, \
    `tenant_set_cookie`, `tenant_remove_cookie` and `tenant_from_request` as `forge new` \
    writes them (set the cookie at sign-in), then run the generator again";

/// Checks `--public`: only for global resources of applications without
/// Forge identity.
fn options_public(
    public: bool,
    layers: Layers,
    purpose: Purpose,
    resource: &Resource,
    identity: bool,
) -> Result<bool, CliError> {
    if !public || purpose == Purpose::Destroy {
        return Ok(public);
    }
    if layers == Layers::Model {
        return unsupported("--public applies to the routes of a scaffold or resource");
    }
    if resource.scope == Scope::Tenant {
        return unsupported(
            "--public applies to --global resources: tenant-scoped rows always need a \
             signed-in member of the tenant",
        );
    }
    if identity {
        return unsupported(
            "--public is refused in an application with Forge identity (the sign-in pages): \
             generate the resource --tenant-scoped, and the owner `forge setup` creates may use \
             it after signing in",
        );
    }
    Ok(true)
}

/// Fails, before anything is written, when a file to create exists or the
/// resource was scaffolded before.
fn refuse_collisions(
    root: &Path,
    resource: &Resource,
    files: &[Planned],
    layers: Layers,
) -> Result<(), CliError> {
    generators::refuse_symlinks(root, files.iter().map(|file| file.path.as_str()))
        .map_err(|reason| fail(layers, reason))?;
    let mut existing = generators::existing(root, files);
    let suffix = format!("_create_{}.up.sql", resource.plural);
    if let Ok(entries) = fs::read_dir(root.join("migrations")) {
        let mut migrations: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(&suffix))
            .map(|name| format!("migrations/{name}"))
            .collect();
        migrations.sort();
        existing.extend(migrations);
    }
    if existing.is_empty() {
        return Ok(());
    }
    Err(fail(
        layers,
        format!(
            "refusing to overwrite existing files; nothing was written:\n  {}",
            existing.join("\n  ")
        ),
    ))
}

/// Writes every file with create-new semantics. On failure, removes the
/// files and directories this call created, so nothing is half generated.
pub(crate) fn write_all(root: &Path, files: &[File]) -> Result<(), CliError> {
    let mut written: Vec<PathBuf> = Vec::new();
    let mut directories: Vec<PathBuf> = Vec::new();
    let result = files.iter().try_for_each(|file| {
        let path = root.join(&file.path);
        if let Some(parent) = path.parent() {
            let mut missing = Vec::new();
            let mut cursor = parent;
            while !cursor.exists() {
                missing.push(cursor.to_path_buf());
                match cursor.parent() {
                    Some(next) => cursor = next,
                    None => break,
                }
            }
            fs::create_dir_all(parent).map_err(|source| CliError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
            directories.extend(missing.into_iter().rev());
        }
        write_new_file(&path, &file.content)?;
        written.push(path);
        Ok(())
    });
    if result.is_err() {
        for path in written.iter().rev() {
            let _ = fs::remove_file(path);
        }
        for directory in directories.iter().rev() {
            let _ = fs::remove_dir(directory);
        }
    }
    result
}

/// The `protect` guard of tenant-scoped routes in an application with the
/// PostgreSQL identity stores: the session cookie, the CSRF token on
/// changes, an active membership in the session's tenant, and an RBAC grant
/// of the permission to the `owner` role, which `forge setup` gives the
/// first member. Declaring `fn protect` with this signature in
/// src/bootstrap/mod.rs replaces it (to grant other roles, for example).
const PROTECT_GUARD_DOC: &str =
    "    /// The guard of tenant-scoped routes: sign-in, the CSRF token on changes, a
    /// membership in the session's tenant, and the `owner` role (`forge setup`)
    /// granted `permissions`. Declare `fn protect` with this signature in
    /// src/bootstrap/mod.rs to grant other roles; the generators then use yours.
";

/// The rest of the guard, from `fn`.
const PROTECT_GUARD_ITEM: &str = "fn protect(
        composition: &mut forge::Composition<'_>,
        permissions: &[&str],
    ) -> Result<
        std::sync::Arc<
            impl crate::adapters::http::access::TenantAccess
            + crate::adapters::http::api_access::ApiTenantAccess
            + 'static,
        >,
        forge::ComposeError,
    > {
        let mut policy = forge::security::RbacPolicy::new();
        let owner = forge::security::Role::new(\"owner\").map_err(forge::ComposeError::other)?;
        for permission in permissions {
            let permission =
                forge::security::Permission::new(*permission).map_err(forge::ComposeError::other)?;
            policy.allow(owner.clone(), permission);
        }
        let pool = composition.postgres()?.pool().clone();
        // Sessions end after FORGE_AUTH_SESSION_IDLE_TIMEOUT without a request.
        let sessions = crate::infrastructure::auth::PostgresSessionStore::new(pool.clone())
            .with_idle_timeout(
                forge::auth::SessionIdleTimeout::new(composition.config().auth.session_idle_timeout)
                    .map_err(forge::ComposeError::other)?,
            );
        Ok(std::sync::Arc::new(
            crate::adapters::http::access::SessionTenantAccess::new(
                std::sync::Arc::new(sessions),
                std::sync::Arc::new(crate::infrastructure::tenancy::PgMembershipStore::new(pool)),
                policy,
            ),
        ))
    }
";

/// The `public` guard of `--public` routes: everyone is let in. Declaring
/// `fn public` with this signature in src/bootstrap/mod.rs puts the
/// application's own authentication in front instead.
const PUBLIC_GUARD_DOC: &str =
    "    /// The guard of --public routes: lets everyone in, signed in or not. Declare
    /// `fn public` with this signature in src/bootstrap/mod.rs to put your own
    /// authentication in front; the generators then use yours.
";

/// The rest of the guard, from `fn`.
const PUBLIC_GUARD_ITEM: &str = "fn public(
        _permissions: &[&str],
    ) -> std::sync::Arc<
        impl crate::adapters::http::access::GlobalAccess
        + crate::adapters::http::api_access::ApiGlobalAccess
        + 'static,
    > {
        std::sync::Arc::new(crate::adapters::http::access::Public)
    }
";

/// A guard as a module-level function of the seam's `mod guards`
/// (`pub(super) `), or as a function inside `compose` for the printed
/// wiring (`""`).
fn guard_code(doc: &str, item: &str, visibility: &str) -> String {
    format!("{doc}    {visibility}{item}")
}

/// The `site` argument of the pages' `routes`: the seam passes its
/// `&Arc<Site>` parameter as it is, `compose` borrows its local.
#[derive(Clone, Copy)]
enum Site {
    Seam,
    Compose,
}

impl Site {
    fn argument(self) -> &'static str {
        match self {
            Self::Seam => "site",
            Self::Compose => "&site",
        }
    }
}

/// What a generation wires: the module declarations and the statements that
/// build its service and mount its routes.
struct Wiring {
    layers: Layers,
    plural: String,
    declarations: Vec<(&'static str, String)>,
    /// The access policy, before the live-update transport.
    access: String,
    /// The live-update transport, once for every broadcasting resource.
    streams: Option<String>,
    /// The guard the access statements call, if any.
    guard: Option<&'static str>,
    /// The service and its routes (for a model, how to build the service),
    /// as the seam and as `compose` pass the site.
    service: String,
    printed_service: String,
    /// What to know once it is wired.
    notes: String,
}

impl Wiring {
    /// What the seam registers.
    fn registration(&self) -> Registration {
        Registration {
            modules: self
                .declarations
                .iter()
                .map(|(file, module)| ((*file).to_owned(), module.clone()))
                .collect(),
            routes: (self.layers != Layers::Model).then(|| {
                Block::scoped(
                    self.layers.name(),
                    &self.plural,
                    &format!("{}{}", self.access, self.service),
                )
            }),
            shared: [
                Block::shared(
                    "guard",
                    "protect",
                    &guard_code(PROTECT_GUARD_DOC, PROTECT_GUARD_ITEM, "pub(super) "),
                ),
                Block::shared(
                    "guard",
                    "public",
                    &guard_code(PUBLIC_GUARD_DOC, PUBLIC_GUARD_ITEM, "pub(super) "),
                ),
            ]
            .into_iter()
            .chain(
                self.streams
                    .iter()
                    .map(|code| Block::shared("streams", "streams", code)),
            )
            .collect(),
        }
    }

    /// The wiring printed for an application without the seam.
    fn printed(&self, root: &Path) -> Result<String, CliError> {
        let model = self.layers == Layers::Model;
        let what = if model { "model" } else { "resource" };
        let mut out = format!(
            "\nForge did not edit any existing file. Wire the {what} by hand:\n\n  \
             1. Declare the new modules (each in the listed file, keeping `mod` lines sorted):\n\n"
        );
        for (file, module) in &self.declarations {
            out.push_str(&format!("       {file:<38} pub mod {module};\n"));
        }
        if model {
            out.push_str(&format!(
                "\n  2. Build the use cases where you need them, for example in `fn compose` of\n  \
                 src/bootstrap/mod.rs, and pass `{}` to the code that uses it:\n\n",
                self.plural
            ));
        } else {
            out.push_str(
                "\n  2. In src/bootstrap/mod.rs, at the end of `fn compose`, before `Ok(())`:\n\n",
            );
        }
        // Pasted into `compose`, a guard is a function inside it.
        let guard = match self.guard {
            Some("protect") => guard_code(PROTECT_GUARD_DOC, PROTECT_GUARD_ITEM, ""),
            Some("public") => guard_code(PUBLIC_GUARD_DOC, PUBLIC_GUARD_ITEM, ""),
            _ => String::new(),
        };
        let code = format!(
            "{guard}{}{}{}",
            self.access,
            self.streams.as_deref().unwrap_or_default(),
            self.printed_service
        );
        out.push_str(&format_statements(root, &code)?);
        out.push_str(&self.notes);
        if model {
            out.push_str(
                "\n  Then apply the migration (`forge migrate`) and run `forge test` (repository\n  \
                 tests on a database server need `cargo test -- --include-ignored` and\n  \
                 FORGE_TEST_* URLs, see docs/database.md).\n",
            );
        } else {
            out.push_str(
                "\n  Then apply the migration (`cargo run -- migrate` with FORGE_MIGRATION_DATABASE_URL)\n  \
                 and run `cargo test` (repository tests on a database server need\n  \
                 `cargo test -- --include-ignored` and FORGE_TEST_* URLs, see docs/database.md).\n",
            );
        }
        Ok(out)
    }

    /// What is printed once the modules are declared and the routes are in
    /// the seam.
    fn seam_notes(&self, root: &Path) -> Result<String, CliError> {
        let mut out = String::new();
        if self.layers == Layers::Model {
            out.push_str(&format!(
                "\n  Build the use cases where you need them, for example in `compose`\n  \
                 (src/bootstrap/mod.rs), and pass `{}` to the code that uses it:\n\n",
                self.plural
            ));
            out.push_str(&format_statements(root, &self.service)?);
        }
        out.push_str(&self.notes);
        out.push_str("\n  Then: forge migrate, and forge dev (or forge test).\n");
        Ok(out)
    }
}

/// The backend's `Composition` accessor and repository prefix.
fn backend_names(backend: Backend) -> (&'static str, &'static str) {
    match backend {
        Backend::Postgresql => ("postgres", "Pg"),
        Backend::MySql => ("mysql", "MySql"),
        Backend::Sqlite => ("sqlite", "Sqlite"),
    }
}

/// The model's module declarations.
fn model_declarations(resource: &Resource) -> Vec<(&'static str, String)> {
    let singular = &resource.singular;
    let plural = &resource.plural;
    vec![
        ("src/domain/entities/mod.rs", singular.clone()),
        (
            "src/application/ports/mod.rs",
            format!("{singular}_repository"),
        ),
        ("src/application/use_cases/mod.rs", plural.clone()),
        ("src/infrastructure/database/mod.rs", plural.clone()),
    ]
}

/// The statement that builds the service `<plural>` on the repository.
fn service_statement(resource: &Resource, backend: Backend) -> String {
    let plural = &resource.plural;
    let type_name = spec::pascal_case(&resource.singular);
    let (database, prefix) = backend_names(backend);
    format!(
        "    let {plural} = crate::application::use_cases::{plural}::{type_name}Service::new(\n        \
         std::sync::Arc::new(\n            \
         crate::infrastructure::database::{plural}::{prefix}{type_name}Repository::new(\n                \
         composition.{database}()?.clone(),\n            \
         ),\n        \
         ),\n    \
         );\n"
    )
}

/// `forge generate model`: the module declarations and how to build the
/// use cases.
fn model_wiring(resource: &Resource, backend: Backend) -> Wiring {
    let plural = &resource.plural;
    let type_name = spec::pascal_case(&resource.singular);
    let mut notes = format!(
        "\n  The service offers list, find, create, update and delete with optimistic\n  \
         locking; `{plural}:read` and `{plural}:write` are its permissions\n  \
         (crate::application::use_cases::{plural}::READ_PERMISSION, WRITE_PERMISSION).\n"
    );
    if resource.scope == Scope::Tenant {
        notes.push_str(&format!(
            "  {type_name} is tenant-scoped: every use case takes the authorized TenantContext.\n"
        ));
    }
    Wiring {
        layers: Layers::Model,
        plural: plural.clone(),
        declarations: model_declarations(resource),
        access: String::new(),
        streams: None,
        guard: None,
        service: service_statement(resource, backend),
        printed_service: service_statement(resource, backend),
        notes,
    }
}

/// The RBAC policy that grants the `owner` role, which `forge setup` gives
/// the first member, both permissions of the resource.
fn owner_policy(plural: &str) -> String {
    format!(
        "    let mut policy = forge::security::RbacPolicy::new();\n    \
         let owner = forge::security::Role::new(\"owner\").map_err(forge::ComposeError::other)?;\n    \
         for permission in [\n        \
         crate::application::use_cases::{plural}::READ_PERMISSION,\n        \
         crate::application::use_cases::{plural}::WRITE_PERMISSION,\n    \
         ] {{\n        \
         let permission =\n            \
         forge::security::Permission::new(permission).map_err(forge::ComposeError::other)?;\n        \
         policy.allow(owner.clone(), permission);\n    \
         }}\n"
    )
}

/// `forge generate scaffold`: the module declarations, the access policy,
/// the pages and the JSON API.
fn wiring(
    resource: &Resource,
    application: &Application,
    new_access: bool,
    new_api_access: bool,
    broadcast: Broadcast,
    public: bool,
) -> Wiring {
    let backend = application.backend;
    let plural = &resource.plural;
    let mut declarations = model_declarations(resource);
    if new_access {
        declarations.push(("src/adapters/http/mod.rs", "access".to_owned()));
    }
    if new_api_access {
        declarations.push(("src/adapters/http/mod.rs", "api_access".to_owned()));
    }
    if broadcast.streams_module {
        declarations.push(("src/adapters/http/mod.rs", "streams".to_owned()));
    }
    declarations.push(("src/adapters/http/mod.rs", plural.clone()));
    declarations.push(("src/adapters/http/mod.rs", format!("{plural}_api")));
    declarations.push(("src/adapters/http/views/mod.rs", plural.clone()));

    let tenant = resource.scope == Scope::Tenant;
    let session_api = tenant && application.session_api();
    let mut access = String::new();
    let pages_access = if !tenant && public {
        access.push_str(&format!(
            "    // {plural}: global and public (--public): anyone who can reach the server may\n    \
             // read and change them, without signing in (forms still need their CSRF token).\n    \
             let access = public(&[\n        \
             crate::application::use_cases::{plural}::READ_PERMISSION,\n        \
             crate::application::use_cases::{plural}::WRITE_PERMISSION,\n    \
             ]);\n"
        ));
        "access.clone()"
    } else if !tenant {
        access.push_str(&format!(
            "    // {plural}: global. DenyAll refuses every request until you decide who may\n    \
             // use them: implement GlobalAccess (src/adapters/http/access.rs).\n"
        ));
        "std::sync::Arc::new(crate::adapters::http::access::DenyAll)"
    } else if backend == Backend::Postgresql {
        access.push_str(&format!(
            "    // {plural}: tenant-scoped, at /{plural} of the tenant the session signed in to,\n    \
             // for roles granted these permissions (the guard grants the owner role).\n    \
             let access = protect(\n        \
             composition,\n        \
             &[\n            \
             crate::application::use_cases::{plural}::READ_PERMISSION,\n            \
             crate::application::use_cases::{plural}::WRITE_PERMISSION,\n        \
             ],\n    \
             )?;\n"
        ));
        if session_api {
            "access.clone()"
        } else {
            "access"
        }
    } else {
        access.push_str(&format!(
            "    // {plural}: tenant-scoped. {} has no Forge session or membership persistence\n    \
             // yet (ADR 0021), so DenyAll refuses every request until you provide\n    \
             // SessionStore and MembershipStore implementations for SessionTenantAccess.\n",
            backend.label()
        ));
        "std::sync::Arc::new(crate::adapters::http::access::DenyAll)"
    };
    let streams = broadcast.enabled.then(|| {
        if backend == Backend::Postgresql {
            "    // Live updates (--broadcast), once for every broadcasting resource. On\n    \
             // PostgreSQL every replica hears every change (LISTEN/NOTIFY); production\n    \
             // needs FORGE_STREAMS_KEY (`openssl rand -base64 32`, the same everywhere).\n    \
             let streams =\n        \
             crate::adapters::http::streams::Streams::register(composition, forge::Fanout::Postgres)?;\n"
                .to_owned()
        } else {
            format!(
                "    // Live updates (--broadcast), once for every broadcasting resource. {}\n    \
                 // has no LISTEN/NOTIFY: updates reach the pages served by this process only.\n    \
                 // Production needs FORGE_STREAMS_KEY (`openssl rand -base64 32`).\n    \
                 let streams =\n        \
                 crate::adapters::http::streams::Streams::register(composition, forge::Fanout::InProcess)?;\n",
                backend.label()
            )
        }
    });
    let streams_argument = if broadcast.enabled {
        "        &streams,\n"
    } else {
        ""
    };
    let service_for = |site: Site| {
        let mut service = service_statement(resource, backend);
        // The JSON API shares the service.
        service.push_str(&format!(
            "    crate::adapters::http::{plural}::routes(\n        \
             composition.router(),\n        \
             {},\n        \
             {plural}.clone(),\n        \
             {pages_access},\n\
             {streams_argument}    \
             )?;\n",
            site.argument()
        ));
        service.push_str(&api_statements(resource, application, true, public));
        service
    };
    let guard = if !tenant && public {
        Some("public")
    } else if tenant && backend == Backend::Postgresql {
        Some("protect")
    } else {
        None
    };

    let mut notes = format!(
        "\n  Every route asks the access policy for `{plural}:read` or `{plural}:write`\n  \
         first; src/adapters/http/access.rs explains the deny-by-default choice for the\n  \
         pages, src/adapters/http/api_access.rs for the JSON API under /v1/{plural}.\n"
    );
    notes.push_str(&api_note(resource, application));
    if broadcast.enabled {
        notes.push_str(&format!(
            "  Live updates: create, update and delete publish to the `{plural}` channel{}, and\n  \
             the index page subscribes; a stream is served only to callers the same policy\n  \
             grants `{plural}:read` (src/adapters/http/streams.rs).\n",
            if tenant { " of the tenant" } else { "" }
        ));
    }
    if backend == Backend::Postgresql && tenant {
        notes.push_str(&format!(
            "  Permissions match exactly: index and show need `{plural}:read`, new, create,\n  \
             edit, update and delete need `{plural}:write`, which the `owner` role has.\n  \
             Signed-out browsers are sent to /session/new (the sign-in pages of `forge new`\n  \
             or `forge generate authentication`); the pages use the tenant of the session.\n"
        ));
    }
    if !tenant && public {
        notes.push_str(&format!(
            "  WARNING: --public serves /{plural} and /v1/{plural} to anyone who can reach the\n  \
             server, without signing in.\n"
        ));
    }
    Wiring {
        layers: Layers::Scaffold,
        plural: plural.clone(),
        declarations,
        access,
        streams,
        guard,
        service: service_for(Site::Seam),
        printed_service: service_for(Site::Compose),
        notes,
    }
}

/// `forge generate resource`: the module declarations, the use cases and
/// the JSON routes.
fn resource_wiring(
    resource: &Resource,
    application: &Application,
    new_api_access: bool,
    new_access: bool,
    public: bool,
) -> Wiring {
    let plural = &resource.plural;
    let mut declarations = model_declarations(resource);
    if new_access {
        declarations.push(("src/adapters/http/mod.rs", "access".to_owned()));
    }
    if new_api_access {
        declarations.push(("src/adapters/http/mod.rs", "api_access".to_owned()));
    }
    // With the pages' session (D29): the `protect` guard, as the scaffold's.
    let session = resource.scope == Scope::Tenant && application.session_api();
    let access = if session {
        format!(
            "    // {plural}: tenant-scoped JSON API of the tenant the session signed in to,\n    \
             // for roles granted these permissions (the guard grants the owner role).\n    \
             let access = protect(\n        \
             composition,\n        \
             &[\n            \
             crate::application::use_cases::{plural}::READ_PERMISSION,\n            \
             crate::application::use_cases::{plural}::WRITE_PERMISSION,\n        \
             ],\n    \
             )?;\n"
        )
    } else {
        String::new()
    };
    declarations.push(("src/adapters/http/mod.rs", format!("{plural}_api")));
    let mut service = service_statement(resource, application.backend);
    service.push_str(&api_statements(resource, application, false, public));
    let mut notes = format!(
        "\n  Every route of /v1/{plural} asks the access policy for `{plural}:read` or\n  \
         `{plural}:write` first; src/adapters/http/api_access.rs explains the\n  \
         deny-by-default choice.\n"
    );
    notes.push_str(&api_note(resource, application));
    if resource.scope == Scope::Global && public {
        notes.push_str(&format!(
            "  WARNING: --public serves /v1/{plural} to anyone who can reach the server,\n  \
             without signing in.\n"
        ));
    }
    Wiring {
        layers: Layers::Resource,
        plural: plural.clone(),
        declarations,
        access,
        streams: None,
        guard: session.then_some("protect"),
        printed_service: service.clone(),
        service,
        notes,
    }
}

/// The statements that mount the JSON API of `resource` on the service
/// `<plural>` (moved into the routes). `pages`: the scaffold's statements
/// come first, with the tenant `policy` and the pages' `access`.
fn api_statements(
    resource: &Resource,
    application: &Application,
    pages: bool,
    public: bool,
) -> String {
    let plural = &resource.plural;
    let tenant = resource.scope == Scope::Tenant;
    let backend = application.backend;
    let routes = |access: &str| {
        format!(
            "    crate::adapters::http::{plural}_api::routes(\n        \
             composition.router(),\n        \
             {plural},\n        \
             {access},\n    \
             )?;\n"
        )
    };
    let deny = "std::sync::Arc::new(crate::adapters::http::api_access::DenyAll)";
    if !tenant && public {
        return format!(
            "    // {plural} JSON API (/v1/{plural}): global and public (--public).\n{}",
            routes(if pages {
                "access"
            } else {
                "std::sync::Arc::new(crate::adapters::http::api_access::Public)"
            })
        );
    }
    if !tenant {
        return format!(
            "    // {plural} JSON API (/v1/{plural}): global. DenyAll refuses every request until\n    \
             // you decide who may use it: implement ApiGlobalAccess\n    \
             // (src/adapters/http/api_access.rs).\n{}",
            routes(deny)
        );
    }
    if backend != Backend::Postgresql {
        return format!(
            "    // {plural} JSON API (/v1/{plural}): tenant-scoped. {} has no Forge\n    \
             // bearer sessions or memberships yet (ADR 0021), so DenyAll refuses every\n    \
             // request until you implement ApiTenantAccess (src/adapters/http/api_access.rs).\n{}",
            backend.label(),
            routes(deny)
        );
    }
    if application.api_only {
        return format!(
            "    // {plural} JSON API (/v1/{plural}): tenant-scoped. This application has no\n    \
             // sign-in (`forge new --api`), so DenyAll refuses every request until you\n    \
             // implement ApiTenantAccess (src/adapters/http/api_access.rs) with the\n    \
             // authentication of your API clients.\n{}",
            routes(deny)
        );
    }
    if !application.bearer_sessions {
        if application.session_api() {
            return format!(
                "    // {plural} JSON API (/v1/{plural}): the same session cookie and policy as the\n    \
                 // pages; changes send the session's CSRF token in X-CSRF-Token.\n{}",
                routes("access")
            );
        }
        return format!(
            "    // {plural} JSON API (/v1/{plural}): tenant-scoped, for bearer tokens. This\n    \
             // application issues none yet, so DenyAll refuses every request: run\n    \
             // `forge generate authentication --api-auth`, then pass BearerTenantAccess\n    \
             // (src/adapters/http/api_access.rs) here.\n{}",
            routes(deny)
        );
    }
    let mut code = format!(
        "    // {plural} JSON API (/v1/{plural}): tenant-scoped. The bearer token of\n    \
         // POST /auth/login names the tenant; RBAC denies every role `policy` does\n    \
         // not name: the `owner` role (`forge setup` gives it to the first member) may\n    \
         // read and write.\n"
    );
    code.push_str(&owner_policy(plural));
    code.push_str(
        "    let pool = composition.postgres()?.pool().clone();\n    \
         // Tokens end after FORGE_AUTH_SESSION_IDLE_TIMEOUT without a request.\n    \
         let bearer_sessions =\n        \
         crate::infrastructure::bearer_sessions::PostgresBearerSessionStore::new(pool.clone())\n            \
         .with_idle_timeout(\n                \
         forge::auth::SessionIdleTimeout::new(composition.config().auth.session_idle_timeout)\n                    \
         .map_err(forge::ComposeError::other)?,\n            \
         );\n",
    );
    code.push_str(&routes(
        "std::sync::Arc::new(crate::adapters::http::api_access::BearerTenantAccess::new(\n            \
         std::sync::Arc::new(bearer_sessions),\n            \
         std::sync::Arc::new(crate::infrastructure::tenancy::PgMembershipStore::new(pool)),\n            \
         policy,\n        \
         ))",
    ));
    code
}

/// What the JSON API wiring needs to work, after the statements.
fn api_note(resource: &Resource, application: &Application) -> String {
    let plural = &resource.plural;
    if resource.scope != Scope::Tenant || application.backend != Backend::Postgresql {
        return String::new();
    }
    if application.bearer_sessions {
        format!(
            "  The JSON API takes the bearer tokens of POST /auth/login:\n\n    \
             curl -s http://127.0.0.1:3000/v1/{plural} -H \"Authorization: Bearer $TOKEN\"\n\n"
        )
    } else if application.session_api() {
        format!(
            "  The JSON API at /v1/{plural} takes the session cookie of the sign-in pages;\n  \
             changes send its CSRF token (the __Host-forge_csrf cookie) in X-CSRF-Token.\n"
        )
    } else {
        String::new()
    }
}

/// Formats statements as they will read inside `fn compose`, so pasting them
/// keeps `cargo fmt --check` clean.
pub(crate) fn format_statements(root: &Path, statements: &str) -> Result<String, CliError> {
    const HEAD: &str = "fn compose() {\n";
    let formatted = rustfmt(root, "wiring", &format!("{HEAD}{statements}}}\n"))?;
    Ok(formatted
        .strip_prefix(HEAD)
        .and_then(|body| body.strip_suffix("}\n"))
        .unwrap_or(&formatted)
        .to_owned())
}

#[cfg(test)]
mod tests;
