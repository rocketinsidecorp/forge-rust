//! `forge generate job NAME [field:type...] --queue` (ADR 0038): a typed job
//! of the persistent PostgreSQL queue (`forge::jobs`, ADR 0025) with its
//! handler, registration, enqueue function and tests that need no
//! database.

use std::fs;
use std::path::Path;

use crate::CliError;
use crate::generate::render_template;
use crate::generators::{self, MAX_NAME_LENGTH, Planned, Role};
use crate::scaffold::{self, pascal_case};

const GENERATOR: &str = "job";

/// Most payload fields one job may declare.
const MAX_FIELDS: usize = 20;

/// The module directory every generated job lives in, written by the first.
pub(crate) const JOBS_MODULE: &str = "src/adapters/jobs/mod.rs";
const JOBS_MODULE_CONTENT: &str = "//! Job handlers of the persistent queue (`forge::jobs`): adapters that turn\n\
//! a job into use-case calls, like HTTP handlers do for requests. `forge\n\
//! generate job --queue` adds one module per job.\n";

/// Types the generated job module and its tests import.
const RESERVED_TYPES: &[&str] = &[
    "DbTransaction",
    "Deserialize",
    "Duration",
    "EnqueueOptions",
    "Enqueued",
    "Error",
    "Job",
    "JobContext",
    "JobError",
    "JobHandler",
    "Jobs",
    "JobsError",
    "Serialize",
];

const JOB: &str = include_str!("../templates/job/job.rs.tmpl");
const TESTS: &str = include_str!("../templates/job/tests.rs.tmpl");

/// Arguments of `forge generate job`.
#[derive(Clone, Debug)]
pub(crate) struct JobOptions {
    pub(crate) name: String,
    pub(crate) fields: Vec<String>,
    pub(crate) queue: bool,
    pub(crate) dry_run: bool,
}

/// What the generator did (or, with `--dry-run`, would do).
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) files: Vec<Planned>,
    pub(crate) wiring: String,
}

fn fail(reason: impl Into<String>) -> CliError {
    generators::generate_error(GENERATOR, reason)
}

/// One payload field.
struct Field {
    name: String,
    rust: String,
    sample: String,
}

fn parse_field(declaration: &str) -> Result<Field, CliError> {
    let Some((name, kind)) = declaration.split_once(':') else {
        return Err(fail(format!(
            "field `{declaration}` must be written name:type (types: string, text, integer, \
             boolean, timestamp, uuid; append `?` for an optional field)"
        )));
    };
    generators::snake_case("field name", name, MAX_NAME_LENGTH).map_err(fail)?;
    let (kind, optional) = match kind.strip_suffix('?') {
        Some(kind) => (kind, true),
        None => (kind, false),
    };
    let (rust, sample) = match kind {
        "string" | "text" => (
            "String",
            format!("\"sample {}\".to_owned()", name.replace('_', " ")),
        ),
        "integer" => ("i64", "42".to_owned()),
        "boolean" => ("bool", "true".to_owned()),
        "timestamp" => (
            "forge::core::Timestamp",
            "forge::core::Timestamp::now()".to_owned(),
        ),
        "uuid" => (
            "forge::core::Uuid",
            "forge::core::Id::<()>::new().into_uuid()".to_owned(),
        ),
        _ => {
            return Err(fail(format!(
                "field `{name}` has unsupported type `{kind}` (types: string, text, integer, \
                 boolean, timestamp, uuid)"
            )));
        }
    };
    Ok(if optional {
        Field {
            name: name.to_owned(),
            rust: format!("Option<{rust}>"),
            sample: format!("Some({sample})"),
        }
    } else {
        Field {
            name: name.to_owned(),
            rust: rust.to_owned(),
            sample,
        }
    })
}

fn validate(options: &JobOptions) -> Result<Vec<Field>, CliError> {
    if !options.queue {
        return Err(fail(
            "pass --queue: Forge-Rust jobs run on the persistent PostgreSQL queue \
             (`forge::jobs`), and a recurring job is a queue job with a `Schedule` \
             (docs/jobs.md). Usage: forge generate job NAME [field:type...] --queue",
        ));
    }
    let name = &options.name;
    generators::snake_case("job name", name, MAX_NAME_LENGTH).map_err(fail)?;
    if name == "mod" {
        return Err(fail("job name `mod` would replace the jobs module"));
    }
    let type_name = pascal_case(name);
    for generated in [type_name.clone(), format!("{type_name}Handler")] {
        if RESERVED_TYPES.contains(&generated.as_str()) {
            return Err(fail(format!(
                "job name `{name}` would define the type `{generated}`, which the generated \
                 code already imports; choose another name"
            )));
        }
    }
    if options.fields.len() > MAX_FIELDS {
        return Err(fail(format!(
            "a job payload has at most {MAX_FIELDS} fields"
        )));
    }
    let mut fields: Vec<Field> = Vec::new();
    for declaration in &options.fields {
        let field = parse_field(declaration)?;
        if fields.iter().any(|existing| existing.name == field.name) {
            return Err(fail(format!(
                "field `{}` is declared more than once",
                field.name
            )));
        }
        fields.push(field);
    }
    Ok(fields)
}

fn plan(root: &Path, options: &JobOptions, destroy: bool) -> Result<Vec<Planned>, CliError> {
    let fields = validate(options)?;
    let Ok(manifest) = fs::read_to_string(root.join("Cargo.toml")) else {
        return Err(fail(format!(
            "no Cargo.toml in {}; run `forge generate job` in the root of an application \
             created by `forge new`",
            root.display()
        )));
    };
    let sqlx = manifest
        .lines()
        .find(|line| line.trim_start().starts_with("sqlx ="))
        .unwrap_or_default();
    if !sqlx.contains("\"postgres\"") || !manifest.contains("async-trait") {
        return Err(fail(
            "jobs run on the persistent queue, which needs PostgreSQL (docs/jobs.md); this \
             application has another database or none",
        ));
    }
    let name = &options.name;
    let type_name = pascal_case(name);
    let field_lines: Vec<String> = fields
        .iter()
        .map(|field| format!("    pub {}: {},", field.name, field.rust))
        .collect();
    let sample_lines: Vec<String> = fields
        .iter()
        .map(|field| format!("        {}: {},", field.name, field.sample))
        .collect();
    let variables = [
        ("%%FIELDS%%", field_lines.join("\n")),
        ("%%SAMPLES%%", sample_lines.join("\n")),
        ("%%NAME%%", name.clone()),
        ("%%TYPE%%", type_name),
    ];
    let variables: Vec<(&str, &str)> = variables
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let render = |template: &str| render_template(template, &variables, |_| false);
    let mut files = Vec::new();
    if !destroy && fs::symlink_metadata(root.join(JOBS_MODULE)).is_err() {
        files.push(Planned::new(JOBS_MODULE, JOBS_MODULE_CONTENT, Role::Shared));
    }
    files.push(Planned::new(
        format!("src/adapters/jobs/{name}.rs"),
        render(JOB),
        Role::Owned,
    ));
    files.push(Planned::new(
        format!("tests/{name}_job.rs"),
        render(TESTS),
        Role::Owned,
    ));
    generators::refuse_symlinks(root, files.iter().map(|file| file.path.as_str())).map_err(fail)?;
    for file in files.iter_mut().filter(|file| file.role == Role::Owned) {
        file.content = scaffold::rustfmt(root, &file.path, &file.content)?;
    }
    Ok(files)
}

/// Generates the job in the application at `root`.
pub(crate) fn generate(root: &Path, options: &JobOptions) -> Result<Outcome, CliError> {
    let files = plan(root, options, false)?;
    generators::refuse_overwrite(root, GENERATOR, &files)?;
    let wiring = wiring(root, options)?;
    if !options.dry_run {
        generators::write(root, &files)?;
    }
    Ok(Outcome { files, wiring })
}

/// What `forge destroy job` removes and the wiring to take out.
pub(crate) fn plan_destroy(
    root: &Path,
    options: &JobOptions,
) -> Result<(Vec<Planned>, String), CliError> {
    let files = plan(root, options, true)?;
    Ok((files, wiring(root, options)?))
}

fn wiring(root: &Path, options: &JobOptions) -> Result<String, CliError> {
    let name = &options.name;
    let mut out = String::from(
        "\nForge did not edit any existing file. Wire the job by hand:\n\n  \
         1. Declare the new modules (each in the listed file, keeping `mod` lines sorted):\n\n",
    );
    let adapters = fs::read_to_string(root.join("src/adapters/mod.rs")).unwrap_or_default();
    if !adapters.contains("pub mod jobs;") {
        out.push_str(&format!(
            "       {:<38} pub mod jobs;\n",
            "src/adapters/mod.rs"
        ));
    }
    out.push_str(&format!("       {JOBS_MODULE:<38} pub mod {name};\n"));
    let bootstrap = fs::read_to_string(root.join("src/bootstrap/mod.rs")).unwrap_or_default();
    // `forge new` generates `jobs_configure` for PostgreSQL applications;
    // older ones register jobs in `compose`.
    let function = if bootstrap.contains("fn jobs_configure(") {
        "jobs_configure"
    } else {
        "compose"
    };
    out.push_str(&format!(
        "\n  2. In src/bootstrap/mod.rs, at the end of `fn {function}`, before `Ok(())`:\n\n"
    ));
    let code = format!(
        "    crate::adapters::jobs::{name}::register(composition.jobs()?)\n        \
         .map_err(forge::ComposeError::other)?;\n"
    );
    out.push_str(&scaffold::format_statements(root, &code)?);
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    let mut step = 3;
    if !bootstrap.contains(".jobs(") && !bootstrap.contains(".events()") {
        out.push_str(&format!(
            "\n  {step}. In src/bootstrap/mod.rs, in `fn run`, before `.run()`:\n\n        \
             .jobs(forge::WorkerMode::InServe)\n\n  \
             The workers then run inside `serve`; `forge::WorkerMode::Dedicated` runs them\n  \
             only in `cargo run -- jobs work`.\n"
        ));
        step += 1;
    }
    let jobs_feature = manifest
        .lines()
        .filter(|line| line.trim_start().starts_with("forge ="))
        .all(|line| line.contains("\"jobs\"") || line.contains("\"events\""));
    if !jobs_feature {
        out.push_str(&format!(
            "\n  {step}. In Cargo.toml, add \"jobs\" to the features of both `forge` dependencies\n     \
             ([dependencies] and [dev-dependencies]).\n"
        ));
    }
    out.push_str(&format!(
        "\n  Enqueue with crate::adapters::jobs::{name}::enqueue(&mut transaction, &job, options)\n  \
         in the transaction that writes the business state; `cargo run -- migrate` applies\n  \
         the queue's tables. Write `perform`: until then every {name} job fails permanently\n  \
         and shows in `cargo run -- jobs dead`. Then run `forge test`.\n"
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(fields: &[&str]) -> JobOptions {
        JobOptions {
            name: "send_welcome".to_owned(),
            fields: fields.iter().map(|field| (*field).to_owned()).collect(),
            queue: true,
            dry_run: false,
        }
    }

    #[test]
    fn arguments_are_validated_before_reading_the_application() {
        let mut without_queue = options(&[]);
        without_queue.queue = false;
        assert!(validate(&without_queue).is_err());
        for fields in [
            &["user_id"][..],
            &["user_id:float"][..],
            &["fn:string"][..],
            &["a:string", "a:text"][..],
            &["Name:string"][..],
        ] {
            assert!(validate(&options(fields)).is_err(), "{fields:?}");
        }
        for name in ["SendWelcome", "job", "jobs_error", "duration", "mod"] {
            let mut bad_name = options(&[]);
            bad_name.name = name.to_owned();
            assert!(validate(&bad_name).is_err(), "{name}");
        }
        let fields = validate(&options(&["id:uuid", "version:integer", "note:text?"]))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(fields[0].rust, "forge::core::Uuid");
        assert_eq!(fields[2].rust, "Option<String>");
    }
}
