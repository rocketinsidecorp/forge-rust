//! The table `routes` prints: every route of the composed application, like
//! `rails routes` and Forge-Go's `forge routes` (same columns, same sorting).
//!
//! Forge-Go reads the source for literal `Handle` calls. Rust routes are
//! built by code, so this lists the routes the composed [`Router`] holds,
//! HTML and JSON alike, and the source line that registered each one.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::path::Path;

use forge_http::{Method, OPENAPI_PATH, Router};

/// What the listing covers, printed under the table.
const SCOPE: &str = "every route registered while composing the application, HTML and JSON alike, \
    and Forge's built-in routes; routes added only while serving are not included";

/// The source of Forge's own routes (health probes, assets).
const BUILT_IN: &str = "forge (built in)";

/// The source of `GET /openapi.json`, which `serve` mounts only when allowed.
const OPENAPI_SOURCE: &str =
    "forge (built in; development and test, or FORGE_OPENAPI_ENABLED=true)";

/// One line of the table.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Row {
    method: String,
    path: String,
    source: String,
}

/// The table and its footer for `router`.
pub(crate) fn render(router: &Router) -> String {
    let mut rows: Vec<Row> = router
        .routes()
        .map(|route| Row {
            method: route.method.as_str().to_owned(),
            path: route.path.to_owned(),
            source: source(route.source.file(), route.source.line()),
        })
        .collect();
    if !router.has_route(&Method::GET, OPENAPI_PATH) {
        rows.push(Row {
            method: Method::GET.as_str().to_owned(),
            path: OPENAPI_PATH.to_owned(),
            source: OPENAPI_SOURCE.to_owned(),
        });
    }
    rows.sort_by(compare);
    rows.dedup();

    let mut cells = vec![["METHOD", "PATH", "ACTION", "SOURCE"].map(str::to_owned)];
    // Rust has no conventional `Resources` router, so no route has an action.
    cells.extend(
        rows.into_iter()
            .map(|row| [row.method, row.path, "-".to_owned(), row.source]),
    );
    let mut text = table(&cells);
    let _ = writeln!(text, "\nScope: {SCOPE}.");
    text
}

/// Path, then method, then source: the order of Forge-Go's table.
fn compare(a: &Row, b: &Row) -> Ordering {
    a.path
        .cmp(&b.path)
        .then_with(|| a.method.cmp(&b.method))
        .then_with(|| a.source.cmp(&b.source))
}

/// Columns padded to their widest cell plus two spaces, the last one left
/// unpadded (Go's `text/tabwriter` with padding 2).
fn table(rows: &[[String; 4]]) -> String {
    let mut widths = [0_usize; 4];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index + 1 == row.len() {
                text.push_str(cell);
            } else {
                let _ = write!(text, "{cell:<width$}", width = widths[index] + 2);
            }
        }
        text.push('\n');
    }
    text
}

/// `file:line` for the application's own files, [`BUILT_IN`] for Forge's.
fn source(file: &str, line: u32) -> String {
    if is_forge_file(file) {
        BUILT_IN.to_owned()
    } else {
        format!("{}:{line}", file.replace('\\', "/"))
    }
}

/// Whether `file` belongs to a Forge crate. All of them sit next to this
/// one (`crates/forge`, `crates/forge-http`, ... in the workspace, or
/// side by side in a registry or git checkout), so the directory that holds
/// this crate holds them all.
fn is_forge_file(file: &str) -> bool {
    Path::new(file!())
        .ancestors()
        .nth(3)
        .is_some_and(|crates| !crates.as_os_str().is_empty() && Path::new(file).starts_with(crates))
}

#[cfg(test)]
mod tests {
    use forge_http::{Response, StatusCode};

    use super::*;

    fn router() -> Router {
        let mut router = Router::new();
        router.standard_health_routes().expect("health");
        router
            .route(Method::POST, "/posts", |_| async {
                Response::empty(StatusCode::CREATED)
            })
            .expect("route");
        router
            .route(Method::GET, "/posts", |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("route");
        router
    }

    #[test]
    fn the_table_is_sorted_by_path_and_method_with_forges_own_routes_marked() {
        let text = render(&router());
        let lines: Vec<&str> = text.lines().collect();

        assert!(lines[0].starts_with("METHOD  PATH "), "{text}");
        assert!(lines[0].contains("  ACTION  SOURCE"), "{text}");
        let fields: Vec<Vec<&str>> = lines[1..6]
            .iter()
            .map(|line| line.split_whitespace().take(3).collect())
            .collect();
        assert_eq!(
            fields,
            [
                ["GET", "/health", "-"],
                ["GET", "/health/live", "-"],
                ["GET", "/health/ready", "-"],
                ["GET", "/openapi.json", "-"],
                ["GET", "/posts", "-"],
            ]
        );
        assert!(lines[1].ends_with("  forge (built in)"), "{text}");
        assert!(lines[4].ends_with(OPENAPI_SOURCE), "{text}");
        assert!(lines[5].starts_with("GET     /posts"), "{text}");
        assert!(lines[6].starts_with("POST    /posts"), "{text}");
        assert!(text.ends_with(&format!("\nScope: {SCOPE}.\n")), "{text}");
    }

    #[test]
    fn an_application_route_at_the_openapi_path_is_listed_once_as_its_own() {
        let mut router = router();
        router
            .route(Method::GET, OPENAPI_PATH, |_| async {
                Response::empty(StatusCode::OK)
            })
            .expect("route");
        let text = render(&router);

        assert_eq!(text.matches("/openapi.json").count(), 1, "{text}");
        assert!(!text.contains(OPENAPI_SOURCE), "{text}");
    }

    #[test]
    fn columns_align_to_the_widest_cell_plus_two_spaces() {
        let rows = [
            ["A", "BBB", "-", "s1"].map(str::to_owned),
            ["CC", "D", "-", "s2"].map(str::to_owned),
        ];

        assert_eq!(table(&rows), "A   BBB  -  s1\nCC  D    -  s2\n");
    }

    #[test]
    fn an_application_route_names_the_line_that_registered_it() {
        assert_eq!(
            source("src/bootstrap/mod.rs", 42),
            "src/bootstrap/mod.rs:42"
        );
        assert_eq!(source("src\\http\\mod.rs", 7), "src/http/mod.rs:7");
        assert_eq!(source(file!(), 1), BUILT_IN);
    }

    #[test]
    fn only_forge_crates_are_forge_files() {
        assert!(is_forge_file(file!()));
        assert!(!is_forge_file("src/bootstrap/mod.rs"));
        assert!(!is_forge_file("/work/app/src/main.rs"));
    }
}
