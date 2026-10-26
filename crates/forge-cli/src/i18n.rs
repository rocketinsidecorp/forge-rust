//! `forge i18n check`: compares every locale of `config/locales` with the
//! keys of the others and with the literal keys the application uses, like
//! Forge-Go's command of the same name (ADR 0037). Read-only: it reads files
//! and never runs application code.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use forge_i18n::{DEFAULT_LOCALE, Options, Translator, file_locale};
use serde::Serialize;

use crate::CliError;
use crate::generate::LOCALES_DIRECTORY;

/// Bounds of the reference scan, so a stray large tree cannot stall it.
const MAX_SCANNED_FILES: usize = 20_000;
const MAX_SCANNED_FILE_BYTES: u64 = 2 << 20;

/// Directories never scanned for references (dot directories neither). The
/// `tests` directory holds test code, like Forge-Go's `_test.go` files.
const SKIPPED_DIRECTORIES: &[&str] = &[
    "vendor",
    "node_modules",
    "tmp",
    "bin",
    "storage",
    "target",
    "tests",
];

/// Formatting data read by `l` and `number`, never text.
const DATA_SCOPES: &[&str] = &["date.", "time.", "number."];

/// The report, in Forge-Go's JSON shape (`schema_version` 1).
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    locales: Vec<String>,
    keys: usize,
    referenced: usize,
    missing: BTreeMap<String, Vec<String>>,
    plural_forms_missing: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    unused: Vec<String>,
}

impl Report {
    /// Whether a locale lacks a key or a plural form.
    fn incomplete(&self) -> bool {
        !self.missing.is_empty() || !self.plural_forms_missing.is_empty()
    }
}

/// Runs the check in the application at `root` and prints the report;
/// [`CliError::TranslationsMissing`] when a locale is incomplete.
pub(crate) fn check(root: &Path, json: bool, output: &mut dyn Write) -> Result<(), CliError> {
    let report = report(root)?;
    let text = if json {
        let mut text = serde_json::to_string(&report)
            .map_err(|error| CliError::Output(std::io::Error::other(error)))?;
        text.push('\n');
        text
    } else {
        render(&report)
    };
    output
        .write_all(text.as_bytes())
        .map_err(CliError::Output)?;
    if report.incomplete() {
        return Err(CliError::TranslationsMissing);
    }
    Ok(())
}

pub(crate) fn report(root: &Path) -> Result<Report, CliError> {
    if !root.join("Cargo.toml").is_file() {
        return Err(CliError::NotAnApplication(
            "Cargo.toml not found".to_owned(),
        ));
    }
    let directory = root.join(LOCALES_DIRECTORY);
    let entries = fs::read_dir(&directory).map_err(|source| {
        CliError::I18n(format!(
            "read {LOCALES_DIRECTORY}: {source} (forge new generates it)"
        ))
    })?;
    let mut available = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|source| CliError::I18n(format!("read {LOCALES_DIRECTORY}: {source}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !name.ends_with(".json") || entry.path().is_dir() {
            continue;
        }
        available.push(file_locale(&name).map_err(|error| CliError::I18n(error.to_string()))?);
    }
    if available.is_empty() {
        return Err(CliError::I18n(format!(
            "{LOCALES_DIRECTORY} has no translation files (LOCALE.json)"
        )));
    }
    available.sort();
    let default_locale = if available.iter().any(|locale| locale == DEFAULT_LOCALE) {
        DEFAULT_LOCALE.to_owned()
    } else {
        available[0].clone()
    };
    let translator = Translator::load_dir(
        &directory,
        Options::new()
            .default_locale(default_locale)
            .show_missing(true),
    )
    .map_err(|error| CliError::I18n(error.to_string()))?;
    let referenced = scan_references(root)?;

    let mut all = BTreeSet::new();
    let mut locale_keys = BTreeMap::new();
    for locale in translator.locales() {
        let keys: BTreeSet<String> = translator.keys(locale).into_iter().collect();
        all.extend(keys.iter().cloned());
        locale_keys.insert(locale.clone(), keys);
    }
    let defined = all.len();
    all.extend(referenced.iter().cloned());

    let mut report = Report {
        schema_version: 1,
        locales: translator.locales().to_vec(),
        keys: defined,
        referenced: referenced.len(),
        missing: BTreeMap::new(),
        plural_forms_missing: BTreeMap::new(),
        unused: Vec::new(),
    };
    for locale in translator.locales() {
        let required = translator.plural_categories(locale);
        let own = locale_keys.get(locale);
        for key in &all {
            let present = own.is_some_and(|keys| keys.contains(key));
            if !present || (!translator.has(locale, key) && !is_data_key(key)) {
                report
                    .missing
                    .entry(locale.clone())
                    .or_default()
                    .push(key.clone());
                continue;
            }
            let Some(forms) = translator.plural_forms(locale, key) else {
                continue;
            };
            let absent: Vec<String> = required
                .iter()
                .filter(|category| !forms.contains(category))
                .map(|category| category.as_str().to_owned())
                .collect();
            if !absent.is_empty() {
                report
                    .plural_forms_missing
                    .entry(locale.clone())
                    .or_default()
                    .insert(key.clone(), absent);
            }
        }
    }
    report.unused = all
        .iter()
        .filter(|key| !referenced.contains(*key) && !is_data_key(key))
        .cloned()
        .collect();
    Ok(report)
}

fn is_data_key(key: &str) -> bool {
    DATA_SCOPES.iter().any(|scope| key.starts_with(scope))
}

fn render(report: &Report) -> String {
    let mut out = format!(
        "{LOCALES_DIRECTORY}: {}; {} keys, {} referenced in templates and Rust code\n",
        report.locales.join(", "),
        report.keys,
        report.referenced
    );
    for locale in &report.locales {
        let missing = report.missing.get(locale);
        let forms = report.plural_forms_missing.get(locale);
        if missing.is_none() && forms.is_none() {
            out.push_str(&format!("\n{locale}: complete\n"));
            continue;
        }
        if let Some(missing) = missing {
            out.push_str(&format!("\n{locale}: {} missing\n", missing.len()));
            for key in missing {
                out.push_str(&format!("  {key}\n"));
            }
        }
        if let Some(forms) = forms {
            out.push_str(&format!("\n{locale}: plural forms missing\n"));
            for (key, absent) in forms {
                out.push_str(&format!("  {key}: {}\n", absent.join(", ")));
            }
        }
    }
    if !report.unused.is_empty() {
        out.push_str(&format!(
            "\nunused: {} (no literal t(\"KEY\") or translate(locale, \"KEY\") found; keys built \
             at runtime are not detected)\n",
            report.unused.len()
        ));
        for key in &report.unused {
            out.push_str(&format!("  {key}\n"));
        }
    }
    out
}

/// The literal keys of `.t("KEY")` (templates and Rust) and
/// `translate(locale, "KEY")` / `translate_html(locale, "KEY")` (Rust) in
/// the application's `*.html` and `*.rs` files, outside test code and
/// build output.
fn scan_references(root: &Path) -> Result<BTreeSet<String>, CliError> {
    let mut referenced = BTreeSet::new();
    let mut scanned = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|source| CliError::ReadDirectory {
            path: directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| CliError::ReadDirectory {
                path: directory.clone(),
                source,
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !name.starts_with('.') && !SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                    pending.push(entry.path());
                }
                continue;
            }
            // Links are not followed: the scan stays inside the project.
            if !file_type.is_file() || !(name.ends_with(".rs") || name.ends_with(".html")) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_SCANNED_FILES {
                return Err(CliError::I18n(format!(
                    "scan for translation keys: more than {MAX_SCANNED_FILES} source files to scan"
                )));
            }
            let too_large = entry
                .metadata()
                .map(|metadata| metadata.len() > MAX_SCANNED_FILE_BYTES)
                .unwrap_or(true);
            if too_large {
                continue;
            }
            let Ok(source) = fs::read_to_string(entry.path()) else {
                continue;
            };
            collect_references(&source, &mut referenced);
        }
    }
    Ok(referenced)
}

/// Adds the literal keys `source` references.
fn collect_references(source: &str, referenced: &mut BTreeSet<String>) {
    for (at, _) in source.match_indices(".t(") {
        if let Some(key) = literal_key(&source[at + 3..]) {
            referenced.insert(key.to_owned());
        }
    }
    for call in ["translate(", "translate_html("] {
        for (at, _) in source.match_indices(call) {
            let before = source[..at].chars().next_back();
            if before.is_some_and(|character| character.is_alphanumeric() || character == '_') {
                continue;
            }
            let rest = &source[at + call.len()..];
            // The key is the second argument: skip the first, which has no
            // comma of its own outside parentheses.
            let mut depth = 0_i32;
            let mut comma = None;
            for (index, character) in rest.char_indices() {
                match character {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' if depth == 0 => break,
                    ')' | ']' | '}' => depth -= 1,
                    ',' if depth == 0 => {
                        comma = Some(index);
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(comma) = comma
                && let Some(key) = literal_key(&rest[comma + 1..])
            {
                referenced.insert(key.to_owned());
            }
        }
    }
}

/// `"KEY"` after optional whitespace, with KEY of letters, digits, `_`, `.`
/// and `-`.
fn literal_key(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    let key = &rest[..end];
    let valid = !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    valid.then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temporary directory");
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n",
        )
        .expect("manifest");
        for (name, content) in files {
            let path = root.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("directory");
            }
            fs::write(path, content).expect("file");
        }
        root
    }

    fn run(root: &Path, json: bool) -> (String, Result<(), CliError>) {
        let mut output = Vec::new();
        let result = check(root, json, &mut output);
        (String::from_utf8_lossy(&output).into_owned(), result)
    }

    /// Forge-Go's `TestI18nCheckReportsMissingPluralAndUnusedKeys`, with
    /// Rust sources.
    #[test]
    fn reports_missing_plural_and_unused_keys() {
        let root = project(&[
            (
                "config/locales/en.json",
                r#"{"en": {"greeting": "Hello", "inbox": {"one": "%{count} message", "other": "%{count} messages"}, "old": "Unused", "date": {"formats": {"short": "%b %d"}}}}"#,
            ),
            (
                "config/locales/pt-BR.json",
                r#"{"pt-BR": {"greeting": "Olá", "inbox": {"one": "%{count} mensagem", "other": "%{count} mensagens"}}}"#,
            ),
            (
                "config/locales/ru.json",
                r#"{"ru": {"greeting": "Привет", "inbox": {"one": "%{count} сообщение", "other": "%{count} сообщений"}, "old": "x", "date": {"formats": {"short": "%d %b"}}}}"#,
            ),
            (
                "config/locales/posts.en.json",
                r#"{"en": {"posts": {"title": "Posts"}}}"#,
            ),
            (
                "templates/posts/index.html",
                r#"<h1>{{ layout.i18n.t("posts.title") }}</h1><p>{{ layout.i18n.t( "greeting") }}</p>{{ layout.i18n.t("inbox").count(2) }}<p>{{ layout.i18n.t("posts.subtitle") }}</p>"#,
            ),
            (
                "src/adapters/http/posts.rs",
                "fn x(i18n: &I18n, t: &Translator) -> String {\n    i18n.t(\"posts.flash.created\").text() + &t.translate(\"en\", \"greeting\", &Args::new())\n}\n",
            ),
            ("tests/posts_http.rs", "fn y() { i18n.t(\"test.only\"); }\n"),
            ("target/debug/x.html", r#"{{ i18n.t("built.key") }}"#),
            ("vendor/x/y.html", r#"{{ i18n.t("vendored.key") }}"#),
        ]);
        let (output, result) = run(root.path(), false);
        assert!(
            matches!(result, Err(CliError::TranslationsMissing)),
            "{output}"
        );
        for want in [
            "config/locales: en, pt-BR, ru; 5 keys, 5 referenced",
            "en: 2 missing\n  posts.flash.created\n  posts.subtitle\n",
            "pt-BR: 5 missing\n  date.formats.short\n  old\n  posts.flash.created\n  posts.subtitle\n  posts.title\n",
            "ru: plural forms missing\n  inbox: few, many\n",
            "unused: 1",
            "  old\n",
        ] {
            assert!(output.contains(want), "report lacks {want:?}:\n{output}");
        }
        for unwanted in [
            "test.only",
            "vendored.key",
            "built.key",
            "  date.formats.short\n\nunused",
        ] {
            assert!(
                !output.contains(unwanted),
                "report contains {unwanted:?}:\n{output}"
            );
        }

        let (json, _) = run(root.path(), true);
        let report: serde_json::Value = serde_json::from_str(&json).expect("JSON report");
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["locales"].as_array().map(Vec::len), Some(3));
        assert_eq!(report["missing"]["pt-BR"].as_array().map(Vec::len), Some(5));
        assert_eq!(
            report["plural_forms_missing"]["ru"]["inbox"],
            serde_json::json!(["few", "many"])
        );
        assert_eq!(report["unused"], serde_json::json!(["old"]));
        assert!(
            json.starts_with("{\"schema_version\":1,\"locales\":"),
            "{json}"
        );
    }

    #[test]
    fn complete_translations_pass() {
        let root = project(&[
            ("config/locales/en.json", r#"{"en": {"a": "A"}}"#),
            ("config/locales/es.json", r#"{"es": {"a": "A"}}"#),
            ("src/lib.rs", "fn f() { i18n.t(\"a\"); }"),
        ]);
        let (output, result) = run(root.path(), false);
        assert!(result.is_ok(), "{output}");
        assert_eq!(
            output,
            "config/locales: en, es; 1 keys, 1 referenced in templates and Rust code\n\nen: complete\n\nes: complete\n"
        );
    }

    /// Forge-Go's `TestI18nCheckRefusesInvalidProjects`.
    #[test]
    fn invalid_projects_are_refused() {
        for (files, want) in [
            (vec![], "config/locales"),
            (
                vec![("config/locales/README.md", "x")],
                "no translation files",
            ),
            (vec![("config/locales/en_US.json", "{}")], "invalid locale"),
            (
                vec![("config/locales/en.json", r#"{"en": "#)],
                "invalid JSON",
            ),
            (
                vec![("config/locales/en.json", r#"{"en": {"a": "1", "a": "2"}}"#)],
                "duplicate key",
            ),
            (
                vec![("config/locales/pt-br.json", r#"{"pt-BR": {}}"#)],
                "pt-BR.json",
            ),
        ] {
            let root = project(&files);
            let (_, result) = run(root.path(), false);
            let message = result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(message.contains(want), "{files:?}: {message:?}");
        }
        let empty = tempfile::tempdir().expect("temporary directory");
        let message = report(empty.path()).err().map(|error| error.to_string());
        assert!(
            message
                .as_deref()
                .is_some_and(|message| message.contains("Cargo.toml")),
            "{message:?}"
        );
    }

    /// Forge-Go's `TestGeneratedApplicationTranslationsAreComplete`: every
    /// key the generated pages and handlers use exists in every generated
    /// locale, and nothing generated is unused.
    #[test]
    fn generated_applications_are_complete_in_every_locale() {
        use crate::generate::{Database, ForgeSource, LOCALES, NewApplication, create_application};
        use crate::scaffold::{ScaffoldOptions, Scope};

        for locale in LOCALES {
            let parent = tempfile::tempdir().expect("temporary directory");
            let options = NewApplication {
                name: "shop".to_owned(),
                docker: false,
                ci: false,
                database: Database::Postgresql,
                views: true,
                forge: ForgeSource::GitTag,
                locale,
                api_auth: false,
            };
            let root = create_application(parent.path(), &options).expect("application");
            for (name, fields, broadcast) in [
                (
                    "post",
                    vec!["title:string", "published:boolean?", "at:timestamp"],
                    false,
                ),
                ("event", vec!["name:string"], true),
            ] {
                crate::scaffold::generate(
                    &root,
                    &ScaffoldOptions {
                        name: name.to_owned(),
                        fields: fields.into_iter().map(str::to_owned).collect(),
                        scope: Scope::Tenant,
                        plural: None,
                        broadcast,
                        dry_run: false,
                        layers: crate::scaffold::Layers::Scaffold,
                    },
                    "1760832000000000000",
                )
                .expect("scaffold");
            }
            let report = report(&root).expect("report");
            assert!(
                report.missing.is_empty() && report.plural_forms_missing.is_empty(),
                "{locale:?}: {report:?}"
            );
            assert!(report.unused.is_empty(), "{locale:?}: {:?}", report.unused);
        }
    }

    #[test]
    fn references_need_a_literal_key() {
        let mut referenced = BTreeSet::new();
        collect_references(
            "a.t(\"x.y\") b.t(key) c.t(\n    \"multi\",\n) translate(&locale, \"z\") \
             translate_html(locale(\"q\"), \"w\") mytranslate(l, \"no\") d.t(\"bad key\")",
            &mut referenced,
        );
        assert_eq!(
            referenced.into_iter().collect::<Vec<_>>(),
            ["multi", "w", "x.y", "z"]
        );
    }
}
