//! `forge generate view [DIR/]NAME` (Forge-Go's command): one server-rendered
//! page, `templates/DIR/NAME.html` (DIR defaults to `pages`), in the layout
//! `forge new` generates, with the Askama view model that checks it at
//! compile time and a test that renders it. Nothing routes to it: a handler
//! renders it, and the generator prints how.

use std::fs;
use std::path::Path;

use crate::CliError;
use crate::destroy::Unwire;
use crate::generate::render_template;
use crate::generators::{self, MAX_NAME_LENGTH, Planned, Role};
use crate::scaffold::{self, pascal_case};
use crate::wiring::{self, Registration};

const GENERATOR: &str = "view";

/// The directory of a view generated without one, as in Forge-Go:
/// `forge generate view welcome` writes `templates/pages/welcome.html`.
pub(crate) const DEFAULT_DIRECTORY: &str = "pages";

macro_rules! view_template {
    ($source:literal) => {
        include_str!(concat!("../templates/view/", $source))
    };
}

/// The page itself: the controller generator's page, so a view and an action
/// look alike.
const PAGE: &str = include_str!("../templates/controller/page.html.tmpl");
const VIEW: &str = view_template!("view.rs.tmpl");
const TESTS: &str = view_template!("tests.rs.tmpl");

/// Arguments of `forge generate view` (and `forge destroy view`).
#[derive(Clone, Debug)]
pub(crate) struct ViewOptions {
    /// `NAME` or `DIR/NAME`.
    pub(crate) name: String,
    pub(crate) dry_run: bool,
}

/// What the generator did (or, with `--dry-run`, would do).
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) files: Vec<Planned>,
    /// The module declaration registered.
    pub(crate) edits: Vec<wiring::Edit>,
    pub(crate) wiring: String,
}

/// A validated view name.
struct Names {
    directory: String,
    name: String,
    /// `pages_welcome_view`.
    module: String,
    /// `PagesWelcomeView`.
    type_name: String,
}

fn fail(reason: impl Into<String>) -> CliError {
    generators::generate_error(GENERATOR, reason)
}

/// Reads `NAME` or `DIR/NAME`; touches no files.
fn names(options: &ViewOptions) -> Result<Names, CliError> {
    let (directory, name) = options
        .name
        .split_once('/')
        .unwrap_or((DEFAULT_DIRECTORY, &options.name));
    let shape = |part: &str| {
        generators::snake_case("view name", part, MAX_NAME_LENGTH).map_err(|reason| {
            fail(format!(
                "{reason}; a view is NAME or DIR/NAME, for example `welcome` or \
                 `pages/about_us`"
            ))
        })
    };
    shape(directory)?;
    shape(name)?;
    if directory == "layouts" {
        return Err(fail(
            "views in layouts/ are layouts, not pages; choose another directory",
        ));
    }
    let module = format!("{directory}_{name}_view");
    let type_name = format!("{}{}View", pascal_case(directory), pascal_case(name));
    Ok(Names {
        directory: directory.to_owned(),
        name: name.to_owned(),
        module,
        type_name,
    })
}

fn plan(root: &Path, options: &ViewOptions) -> Result<(Names, Vec<Planned>), CliError> {
    let names = names(options)?;
    if !root.join("Cargo.toml").is_file() {
        return Err(fail(format!(
            "no Cargo.toml in {}; run `forge generate view` in the root of an application \
             created by `forge new`",
            root.display()
        )));
    }
    let views = fs::read_to_string(root.join("src/adapters/http/views/mod.rs")).unwrap_or_default();
    if !views.contains("pub struct Layout") {
        return Err(fail(
            "this application has no views (it was created with `forge new --api`): pages \
             need the layout, Askama and the asset pipeline of a full-stack application. Create \
             one with `forge new NAME` and move your code into it",
        ));
    }
    let variables = [
        ("%%DIR%%", names.directory.as_str()),
        ("%%NAME%%", names.name.as_str()),
        ("%%MODULE%%", names.module.as_str()),
        ("%%TYPE%%", names.type_name.as_str()),
    ];
    let render = |template: &str| render_template(template, &variables, |_| false);
    let mut files = vec![
        Planned::new(
            format!("templates/{}/{}.html", names.directory, names.name),
            PAGE,
            Role::Owned,
        ),
        Planned::new(
            format!("src/adapters/http/views/{}.rs", names.module),
            render(VIEW),
            Role::Owned,
        ),
        Planned::new(
            format!("tests/{}.rs", names.module),
            render(TESTS),
            Role::Owned,
        ),
    ];
    generators::refuse_symlinks(root, files.iter().map(|file| file.path.as_str())).map_err(fail)?;
    for file in files.iter_mut().filter(|file| file.path.ends_with(".rs")) {
        file.content = scaffold::rustfmt(root, &file.path, &file.content)?;
    }
    Ok((names, files))
}

/// Generates the view in the application at `root`.
pub(crate) fn generate(root: &Path, options: &ViewOptions) -> Result<Outcome, CliError> {
    let (names, files) = plan(root, options)?;
    generators::refuse_overwrite(root, GENERATOR, &files)?;
    let (text, edits) = if wiring::has_seam(root) {
        let edits = wiring::plan(root, &registration(&names), false, &|reason| fail(reason))?;
        (usage(&names), edits)
    } else {
        (wiring(&names), Vec::new())
    };
    if !options.dry_run {
        generators::write_and_wire(root, &files, &edits)?;
    }
    Ok(Outcome {
        wiring: text,
        edits,
        files,
    })
}

/// The view's module declaration.
fn registration(names: &Names) -> Registration {
    Registration {
        modules: vec![(VIEWS_MODULE.to_owned(), names.module.clone())],
        routes: None,
        shared: Vec::new(),
    }
}

/// What `forge destroy view` removes, the wiring to take out, and why the
/// page belongs to a controller instead (Forge-Go's rule: a controller's
/// page is the same stub, so only the controller can tell it is its own).
pub(crate) fn plan_destroy(
    root: &Path,
    options: &ViewOptions,
) -> Result<(Vec<Planned>, Unwire, Option<String>), CliError> {
    let (names, files) = plan(root, options)?;
    let page = format!("templates/{}/{}.html", names.directory, names.name);
    let controller_views = format!("src/adapters/http/views/{}_controller.rs", names.directory);
    let renders = fs::read_to_string(root.join(&controller_views)).is_ok_and(|source| {
        source.contains(&format!(
            "#[template(path = \"{}/{}.html\")]",
            names.directory, names.name
        ))
    });
    let owned_elsewhere = renders.then(|| {
        format!(
            "{page} is a page of the {directory} controller ({controller_views} renders it), and \
             removing it alone would make that page fail. Run `forge destroy controller \
             {directory}` with the actions it was generated with, or pass --force to remove \
             only the view",
            directory = names.directory
        )
    });
    let unwire = if wiring::has_seam(root) && owned_elsewhere.is_none() {
        Unwire::Edits(wiring::plan(
            root,
            &registration(&names),
            true,
            &|reason| {
                CliError::Destroy(format!(
                    "refusing to destroy, nothing was removed: {reason}"
                ))
            },
        )?)
    } else if wiring::has_seam(root) {
        Unwire::Edits(Vec::new())
    } else {
        Unwire::Printed(wiring(&names))
    };
    Ok((files, unwire, owned_elsewhere))
}

/// The `mod.rs` that declares the view modules.
const VIEWS_MODULE: &str = "src/adapters/http/views/mod.rs";

/// How to render the view, printed once its module is declared.
fn usage(names: &Names) -> String {
    let Names {
        directory,
        name,
        module,
        type_name,
    } = names;
    let title = pascal_case(name);
    format!(
        "\n  Render it from a handler that has the application's `Site` (as the welcome\n  \
         page does in src/adapters/http/mod.rs):\n\n    \
         forge::web::html(&crate::adapters::http::views::{module}::{type_name} {{\n        \
         layout: site.layout(&request),\n        \
         title: \"{title}\",\n        \
         content: \"Find me in templates/{directory}/{name}.html\",\n    \
         }})\n\n  \
         Nothing routes to the page until a handler renders it. Then run `forge test`.\n"
    )
}

/// The module declaration and the rendering call, printed after generation
/// (and by `forge destroy` to take out) in an application without the seam.
fn wiring(names: &Names) -> String {
    let Names {
        directory,
        name,
        module,
        type_name,
    } = names;
    let file = "src/adapters/http/views/mod.rs";
    let title = pascal_case(name);
    format!(
        "\nForge did not edit any existing file. Use the view by hand:\n\n  \
         1. Declare the new module (keeping `mod` lines sorted):\n\n       \
         {file:<38} pub mod {module};\n\n  \
         2. Render it from a handler that has the application's `Site` (as the\n     \
         welcome page does in src/adapters/http/mod.rs):\n\n    \
         forge::web::html(&crate::adapters::http::views::{module}::{type_name} {{\n        \
         layout: site.layout(&request),\n        \
         title: \"{title}\",\n        \
         content: \"Find me in templates/{directory}/{name}.html\",\n    \
         }})\n\n  \
         Nothing routes to the page until a handler renders it. Then run `forge test`.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(name: &str) -> ViewOptions {
        ViewOptions {
            name: name.to_owned(),
            dry_run: false,
        }
    }

    #[test]
    fn names_are_dir_slash_name_with_pages_by_default() {
        let welcome = names(&options("welcome")).expect("valid");
        assert_eq!(
            (
                welcome.directory.as_str(),
                welcome.name.as_str(),
                welcome.module.as_str(),
                welcome.type_name.as_str()
            ),
            ("pages", "welcome", "pages_welcome_view", "PagesWelcomeView")
        );
        let nested = names(&options("admin/audit_log")).expect("valid");
        assert_eq!(nested.module, "admin_audit_log_view");
        assert_eq!(nested.type_name, "AdminAuditLogView");
        for (name, reason) in [
            ("Welcome", "must be snake_case"),
            ("a/b/c", "must be snake_case"),
            ("../etc", "must be snake_case"),
            ("pages/", "must be snake_case"),
            ("/welcome", "must be snake_case"),
            ("pages/we.lcome", "must be snake_case"),
            ("pages/{{x}}", "must be snake_case"),
            ("fn", "is a Rust keyword"),
            ("layouts/main", "views in layouts/ are layouts"),
        ] {
            let error = names(&options(name))
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(error.contains(reason), "{name}: {error}");
        }
    }
}
