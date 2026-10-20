//! `forge generate controller NAME ACTION... [--api] [--public]` (ADR 0038):
//! read-only `GET` pages (or JSON endpoints with `--api`), their routes,
//! views and tests, without persistence.
//!
//! The routes are never public by accident: without `--public` they ask the
//! application's access policy (`src/adapters/http/access.rs`, shared with
//! the scaffolds) for `NAME:read`, and the printed wiring passes `DenyAll`
//! until the application decides who may read them. A model or scaffold
//! whose plural is `NAME` already declares `NAME:read`; the controller then
//! reuses that constant instead of declaring the permission a second time.

use std::fs;
use std::path::Path;

use crate::CliError;
use crate::generate::render_template;
use crate::generators::{self, MAX_NAME_LENGTH, Planned, Role};
use crate::scaffold::{self, ACCESS_PATH, pascal_case};

const GENERATOR: &str = "controller";

/// Most actions one controller may declare.
const MAX_ACTIONS: usize = 20;

/// Actions that change data: they belong to `forge generate scaffold`.
const WRITE_ACTIONS: &[&str] = &["create", "update", "destroy"];

/// Functions of the generated module an action may not shadow.
const RESERVED_ACTIONS: &[&str] = &["authorize", "respond", "route", "routes"];

/// Types the generated controller module imports or declares besides its
/// view types; a view type (`NAME` + `ACTION` in PascalCase) must differ.
const RESERVED_TYPES: &[&str] = &[
    "Arc",
    "ComposeError",
    "Future",
    "GlobalAccess",
    "Layout",
    "Method",
    "Permission",
    "Request",
    "Response",
    "Router",
    "Site",
    "Template",
];

/// Names whose routes, modules or templates the application already uses.
const RESERVED_NAMES: &[&str] = &[
    "access",
    "assets",
    "health",
    "layouts",
    "mailers",
    "passwords",
    "session",
    "sessions",
    "streams",
    "tenants",
    "views",
];

macro_rules! controller_template {
    ($source:literal) => {
        include_str!(concat!("../templates/controller/", $source))
    };
}

const HTML: &str = controller_template!("html.rs.tmpl");
const HTML_VIEWS: &str = controller_template!("html_views.rs.tmpl");
const PAGE: &str = controller_template!("page.html.tmpl");
const API: &str = controller_template!("api.rs.tmpl");
const TESTS: &str = controller_template!("tests.rs.tmpl");

/// Arguments of `forge generate controller`.
#[derive(Clone, Debug)]
pub(crate) struct ControllerOptions {
    pub(crate) name: String,
    pub(crate) actions: Vec<String>,
    pub(crate) api: bool,
    pub(crate) public: bool,
    pub(crate) dry_run: bool,
}

/// What the generator did (or, with `--dry-run`, would do).
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) files: Vec<Planned>,
    pub(crate) reused_access: bool,
    pub(crate) wiring: String,
}

/// Where the permission of a guarded controller comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Permission {
    /// `--public`: no guard, no permission.
    None,
    /// The controller declares `NAME:read` itself.
    Own,
    /// `src/application/use_cases/NAME.rs` (a model or scaffold) declares it.
    Reused,
}

struct Plan {
    files: Vec<Planned>,
    access_exists: bool,
    permission: Permission,
}

fn fail(reason: impl Into<String>) -> CliError {
    generators::generate_error(GENERATOR, reason)
}

/// Generates the controller in the application at `root`.
pub(crate) fn generate(root: &Path, options: &ControllerOptions) -> Result<Outcome, CliError> {
    let plan = plan(root, options, false)?;
    generators::refuse_overwrite(root, GENERATOR, &plan.files)?;
    let wiring = wiring(root, options, &plan)?;
    if !options.dry_run {
        generators::write(root, &plan.files)?;
    }
    Ok(Outcome {
        files: plan.files,
        reused_access: plan.access_exists && plan.permission != Permission::None,
        wiring,
    })
}

/// What `forge destroy controller` removes: the controller's own files (not
/// the shared access module) and the wiring to take out.
pub(crate) fn plan_destroy(
    root: &Path,
    options: &ControllerOptions,
) -> Result<(Vec<Planned>, String), CliError> {
    let plan = plan(root, options, true)?;
    let wiring = wiring(root, options, &plan)?;
    let files = plan
        .files
        .into_iter()
        .filter(|file| file.role != Role::Shared)
        .collect();
    Ok((files, wiring))
}

/// Checks the arguments; touches no files.
fn validate(options: &ControllerOptions) -> Result<(), CliError> {
    let name = &options.name;
    generators::snake_case("controller name", name, MAX_NAME_LENGTH).map_err(fail)?;
    if let Some(stem) = name.strip_suffix("_controller") {
        return Err(fail(format!(
            "controller name `{name}` ends in _controller, which Forge adds to the module \
             names; name it `{stem}`"
        )));
    }
    if RESERVED_NAMES.contains(&name.as_str()) {
        return Err(fail(format!(
            "controller name `{name}` is a path, module or template directory the application \
             already uses ({}); choose another name",
            RESERVED_NAMES.join(", ")
        )));
    }
    if options.actions.is_empty() {
        return Err(fail(
            "declare at least one action, for example: forge generate controller pages index",
        ));
    }
    if options.actions.len() > MAX_ACTIONS {
        return Err(fail(format!(
            "a controller has at most {MAX_ACTIONS} actions"
        )));
    }
    let mut seen: Vec<String> = Vec::new();
    for action in &options.actions {
        generators::snake_case("action", action, MAX_NAME_LENGTH).map_err(fail)?;
        if WRITE_ACTIONS.contains(&action.as_str()) {
            return Err(fail(format!(
                "action `{action}` changes data, and a controller has no model: use `forge \
                 generate scaffold` for create, update and destroy, or write the handler \
                 yourself"
            )));
        }
        if RESERVED_ACTIONS.contains(&action.as_str()) {
            return Err(fail(format!(
                "action `{action}` would shadow a function of the generated module ({}); \
                 choose another name",
                RESERVED_ACTIONS.join(", ")
            )));
        }
        // `a_b` and `ab` differ, but two actions must not share a view type.
        let type_name = pascal_case(action);
        let view = format!("{}{type_name}", pascal_case(name));
        if RESERVED_TYPES.contains(&view.as_str()) || type_name == "Controller" {
            return Err(fail(format!(
                "action `{action}` would name the type `{view}`, which the generated module \
                 already uses; choose another action name"
            )));
        }
        if seen.contains(&type_name) {
            return Err(fail(format!(
                "action `{action}` is declared twice (or names the same view type as another \
                 action)"
            )));
        }
        seen.push(type_name);
    }
    Ok(())
}

fn read(root: &Path, relative: &str) -> Option<String> {
    fs::read_to_string(root.join(relative)).ok()
}

/// The path of the route of `action`.
fn path(options: &ControllerOptions, action: &str) -> String {
    let base = if options.api {
        format!("/v1/{}", options.name)
    } else {
        format!("/{}", options.name)
    };
    if action == "index" {
        base
    } else {
        format!("{base}/{action}")
    }
}

fn plan(root: &Path, options: &ControllerOptions, destroy: bool) -> Result<Plan, CliError> {
    validate(options)?;
    let name = &options.name;
    let Some(manifest) = read(root, "Cargo.toml") else {
        return Err(fail(format!(
            "no Cargo.toml in {}; run `forge generate controller` in the root of an \
             application created by `forge new`",
            root.display()
        )));
    };
    let bootstrap = read(root, "src/bootstrap/mod.rs").unwrap_or_default();
    if !root.join("src/adapters/http/views/mod.rs").is_file()
        || !bootstrap.contains("crate::adapters::http::site(")
    {
        return Err(fail(
            "this application has no views and no `compose` hook (it was created with `forge \
             new --api`). Controllers are wired in the `compose` function of a full-stack \
             application; in an API application add the handlers to src/adapters/http/mod.rs \
             by hand (docs/architecture.md)",
        ));
    }
    let guarded = !options.public;
    let access_exists = fs::symlink_metadata(root.join(ACCESS_PATH)).is_ok();
    if guarded && !destroy {
        match read(root, ACCESS_PATH) {
            Some(access) => {
                if !access.contains("pub trait GlobalAccess")
                    || !access.contains("pub struct DenyAll")
                {
                    return Err(fail(format!(
                        "{ACCESS_PATH} exists but has no `GlobalAccess` or `DenyAll`; the \
                         routes depend on them. Restore it or move your file aside"
                    )));
                }
                if !scaffold::global_access_returns_principal(&access) {
                    return Err(fail(format!(
                        "`GlobalAccess::authorize` in {ACCESS_PATH} returns `Result<(), \
                         AccessDenied>`; generated code expects `Result<Option<PrincipalId>, \
                         AccessDenied>`. Update the trait as `forge generate scaffold` \
                         explains, then run the controller again"
                    )));
                }
            }
            None if access_exists => {
                return Err(fail(format!("{ACCESS_PATH} is not a readable file")));
            }
            None => {
                let security = read(root, "src/infrastructure/security/mod.rs").unwrap_or_default();
                if !security.contains("pub async fn authenticate_request")
                    || !manifest.contains("async-trait")
                {
                    return Err(fail(
                        "guarded routes ask the access policy of src/adapters/http/access.rs, \
                         which needs the session stores of an application with a database. \
                         Pass --public for pages anyone may read, or create the application \
                         with a database",
                    ));
                }
            }
        }
    }
    let controller_path = format!("src/adapters/http/{name}_controller.rs");
    let permission = if options.public {
        Permission::None
    } else {
        let owner_module = format!("src/application/use_cases/{name}.rs");
        match generators::permission_owner(root, &[format!("{name}:read")], &[&controller_path]) {
            None => Permission::Own,
            Some((owner, _)) if owner == owner_module => Permission::Reused,
            // Destroy renders what the controller declared: its own constant.
            Some(_) if destroy => Permission::Own,
            Some((owner, permission)) => {
                return Err(fail(format!(
                    "{owner} already declares the permission `{permission}`. A permission has \
                     one owner; a controller reuses only the one a model or scaffold declares \
                     in {owner_module}. Choose another name, or --public"
                )));
            }
        }
    };

    let type_name = pascal_case(name);
    let mut table = String::new();
    let mut routes = String::new();
    let mut handlers = String::new();
    let mut view_types = Vec::new();
    let mut view_structs = String::new();
    let mut test_routes = String::new();
    let permission_cell = if guarded {
        format!("`{name}:read`")
    } else {
        "public".to_owned()
    };
    for action in &options.actions {
        let route = path(options, action);
        let view = format!("{type_name}{}", pascal_case(action));
        let title = format!("{type_name}#{action}");
        table.push_str(&format!(
            "//! | `GET` | `{route}` | {action} | {permission_cell} |\n"
        ));
        let guard = if guarded {
            "    if let Some(denied) = authorize(&state, &request).await {\n        \
             return denied;\n    \
             }\n"
        } else {
            ""
        };
        if options.api {
            let message = format!("Find me in {controller_path}");
            if guarded {
                routes.push_str(&format!(
                    "    route(router, &state, \"{route}\", \"{action}\", {action})?;\n"
                ));
                handlers.push_str(&format!(
                    "\n/// `GET {route}`.\n\
                     async fn {action}(state: Arc<{type_name}Controller>, request: Request) -> Response {{\n\
                     {guard}    \
                     respond(&{type_name}Body {{\n        \
                     controller: \"{name}\",\n        \
                     action: \"{action}\",\n        \
                     message: \"{message}\",\n    \
                     }})\n\
                     }}\n"
                ));
            } else {
                routes.push_str(&format!(
                    "    router.route(\n        Method::GET,\n        \"{route}\",\n        \
                     describe({action}, operation(\"{action}\")),\n    )?;\n"
                ));
                handlers.push_str(&format!(
                    "\n/// `GET {route}`.\n\
                     async fn {action}(_request: Request) -> Response {{\n    \
                     respond(&{type_name}Body {{\n        \
                     controller: \"{name}\",\n        \
                     action: \"{action}\",\n        \
                     message: \"{message}\",\n    \
                     }})\n\
                     }}\n"
                ));
            }
            test_routes.push_str(&format!(
                "    (\"{route}\", \"\\\"action\\\":\\\"{action}\\\"\"),\n"
            ));
        } else {
            routes.push_str(&format!(
                "    route(router, &state, \"{route}\", {action})?;\n"
            ));
            handlers.push_str(&format!(
                "\n/// `GET {route}`: `templates/{name}/{action}.html`.\n\
                 async fn {action}(state: Arc<{type_name}Controller>, request: Request) -> Response {{\n\
                 {guard}    \
                 forge::web::html(&{view} {{\n        \
                 layout: state.site.layout(&request),\n        \
                 title: \"{title}\",\n        \
                 content: \"Find me in templates/{name}/{action}.html\",\n    \
                 }})\n\
                 }}\n"
            ));
            view_structs.push_str(&format!(
                "\n/// `templates/{name}/{action}.html`.\n\
                 #[derive(Template)]\n\
                 #[template(path = \"{name}/{action}.html\")]\n\
                 pub struct {view}<'a> {{\n    \
                 pub layout: Layout<'a>,\n    \
                 pub title: &'a str,\n    \
                 pub content: &'a str,\n\
                 }}\n"
            ));
            view_types.push(view);
            test_routes.push_str(&format!("    (\"{route}\", \"{title}\"),\n"));
        }
    }
    let routes_arguments = match (options.api, guarded) {
        (false, true) => ", &site, access",
        (false, false) => ", &site",
        (true, true) => ", access",
        (true, false) => "",
    };
    let guard_summary = if guarded {
        format!(" and every route asks the access policy for `{name}:read`")
    } else {
        String::new()
    };
    let variables = [
        ("%%ROUTE_TABLE%%", table.trim_end().to_owned()),
        ("%%ROUTES%%", routes.trim_end().to_owned()),
        ("%%HANDLERS%%", handlers),
        ("%%VIEW_TYPES%%", view_types.join(", ")),
        ("%%VIEW_STRUCTS%%", view_structs),
        ("%%TEST_ROUTES%%", test_routes.trim_end().to_owned()),
        ("%%ROUTES_ARGUMENTS%%", routes_arguments.to_owned()),
        ("%%GUARD_SUMMARY%%", guard_summary),
        ("%%NAME%%", name.clone()),
        ("%%TYPE%%", type_name.clone()),
    ];
    let variables: Vec<(&str, &str)> = variables
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let condition = |flag: &str| match flag {
        "guarded" => guarded,
        "public" => !guarded,
        "own" => permission == Permission::Own,
        "reuse" => permission == Permission::Reused,
        "html" => !options.api,
        "api" => options.api,
        _ => false,
    };
    let render = |template: &str| render_template(template, &variables, condition);

    let mut files = Vec::new();
    if guarded && !access_exists && !destroy {
        files.push(Planned::new(
            ACCESS_PATH,
            scaffold::access_module(),
            Role::Shared,
        ));
    }
    if options.api {
        files.push(Planned::new(&controller_path, render(API), Role::Owned));
    } else {
        files.push(Planned::new(&controller_path, render(HTML), Role::Owned));
        files.push(Planned::new(
            format!("src/adapters/http/views/{name}_controller.rs"),
            render(HTML_VIEWS),
            Role::Owned,
        ));
        for action in &options.actions {
            files.push(Planned::new(
                format!("templates/{name}/{action}.html"),
                PAGE,
                Role::Owned,
            ));
        }
    }
    files.push(Planned::new(
        format!("tests/{name}_controller_http.rs"),
        render(TESTS),
        Role::Owned,
    ));
    generators::refuse_symlinks(root, files.iter().map(|file| file.path.as_str())).map_err(fail)?;
    for file in files.iter_mut().filter(|file| file.path.ends_with(".rs")) {
        file.content = scaffold::rustfmt(root, &file.path, &file.content)?;
    }
    Ok(Plan {
        files,
        access_exists,
        permission,
    })
}

/// The module declarations and composition-root lines that mount the
/// routes, printed after generation (and by `forge destroy` to take out).
fn wiring(root: &Path, options: &ControllerOptions, plan: &Plan) -> Result<String, CliError> {
    let name = &options.name;
    let mut out = String::from(
        "\nForge did not edit any existing file. Wire the controller by hand:\n\n  \
         1. Declare the new modules (each in the listed file, keeping `mod` lines sorted):\n\n",
    );
    let mut declarations = Vec::new();
    if plan.permission != Permission::None && !plan.access_exists {
        declarations.push(("src/adapters/http/mod.rs", "access".to_owned()));
    }
    declarations.push(("src/adapters/http/mod.rs", format!("{name}_controller")));
    if !options.api {
        declarations.push((
            "src/adapters/http/views/mod.rs",
            format!("{name}_controller"),
        ));
    }
    for (file, module) in &declarations {
        out.push_str(&format!("       {file:<38} pub mod {module};\n"));
    }
    out.push_str(
        "\n  2. In src/bootstrap/mod.rs, at the end of `fn compose`, before `Ok(())`:\n\n",
    );
    let site = if options.api { "" } else { "&site, " };
    let code = if options.public {
        format!(
            "    // {name} controller: public (--public), anyone may call its routes.\n    \
             crate::adapters::http::{name}_controller::routes(composition.router(), {site})?;\n"
        )
    } else {
        format!(
            "    // {name} controller: DenyAll refuses every request until you decide who may\n    \
             // read `{name}:read`: implement GlobalAccess (src/adapters/http/access.rs).\n    \
             crate::adapters::http::{name}_controller::routes(\n        \
             composition.router(),\n        \
             {site}std::sync::Arc::new(crate::adapters::http::access::DenyAll),\n    \
             )?;\n"
        )
    };
    out.push_str(&scaffold::format_statements(root, &code)?);
    let routes: Vec<String> = options
        .actions
        .iter()
        .map(|action| format!("GET {}", path(options, action)))
        .collect();
    out.push_str(&format!("\n  Routes: {}.\n", routes.join(", ")));
    match plan.permission {
        Permission::None => out.push_str(&format!(
            "  WARNING: --public mounts the {name} routes without a guard: anyone who can \
             reach\n  the server can call them.\n"
        )),
        Permission::Own => out.push_str(&format!(
            "  Every route asks the access policy for `{name}:read`\n  \
             (crate::adapters::http::{name}_controller::READ_PERMISSION, declared here);\n  \
             src/adapters/http/access.rs explains the deny-by-default choice.\n"
        )),
        Permission::Reused => out.push_str(&format!(
            "  Every route asks the access policy for `{name}:read`, which\n  \
             src/application/use_cases/{name}.rs already declares: the controller reuses it\n  \
             (a permission has one owner). Destroy this controller before that model.\n"
        )),
    }
    out.push_str("\n  Then run `forge test`.\n");
    Ok(out)
}
