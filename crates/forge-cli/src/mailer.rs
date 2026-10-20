//! `forge generate mailer NAME METHOD[:field,...]...` (ADR 0038): a mailer
//! over `forge::mail` (ADR 0035) with one typed method per email, askama
//! subject, text and HTML templates, previews and tests.

use std::fs;
use std::path::Path;

use crate::CliError;
use crate::generate::render_template;
use crate::generators::{self, MAX_NAME_LENGTH, Planned, Role};
use crate::scaffold::{self, pascal_case, sentence_case};

const GENERATOR: &str = "mailer";

/// Most emails one mailer may declare, and fields one email may take.
const MAX_METHODS: usize = 20;
const MAX_FIELDS: usize = 20;

/// Functions of the generated mailer an email may not shadow.
const RESERVED_METHODS: &[&str] = &[
    "deliver",
    "from_config",
    "is_configured",
    "new",
    "previews",
    "unconfigured",
];

const MAILER: &str = include_str!("../templates/mailer/mailer.rs.tmpl");
const TESTS: &str = include_str!("../templates/mailer/tests.rs.tmpl");

/// Arguments of `forge generate mailer`.
#[derive(Clone, Debug)]
pub(crate) struct MailerOptions {
    pub(crate) name: String,
    pub(crate) methods: Vec<String>,
    pub(crate) dry_run: bool,
}

/// What the generator did (or, with `--dry-run`, would do).
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) files: Vec<Planned>,
    pub(crate) wiring: String,
}

/// One email: its name and the string fields of its data.
struct Method {
    name: String,
    fields: Vec<String>,
}

fn fail(reason: impl Into<String>) -> CliError {
    generators::generate_error(GENERATOR, reason)
}

fn parse(options: &MailerOptions) -> Result<Vec<Method>, CliError> {
    let name = &options.name;
    generators::snake_case("mailer name", name, MAX_NAME_LENGTH).map_err(fail)?;
    if let Some(stem) = name.strip_suffix("_mailer") {
        return Err(fail(format!(
            "mailer name `{name}` ends in _mailer, which Forge adds to the names; name it \
             `{stem}`"
        )));
    }
    if options.methods.is_empty() {
        return Err(fail(
            "declare at least one email (method), for example: welcome or welcome:name,url",
        ));
    }
    if options.methods.len() > MAX_METHODS {
        return Err(fail(format!("a mailer has at most {MAX_METHODS} emails")));
    }
    let type_name = pascal_case(name);
    // Every generated Rust name, with the email that generates it.
    let mut functions: Vec<(String, String)> = Vec::new();
    let mut types: Vec<(String, String)> = vec![(format!("{type_name}Mailer"), String::new())];
    let mut methods = Vec::new();
    for declaration in &options.methods {
        let (method, list) = match declaration.split_once(':') {
            Some((method, list)) => (method, Some(list)),
            None => (declaration.as_str(), None),
        };
        generators::snake_case("email", method, MAX_NAME_LENGTH).map_err(|reason| {
            fail(format!(
                "{reason}; optionally followed by :field,field (`welcome:name,url`)"
            ))
        })?;
        if RESERVED_METHODS.contains(&method) {
            return Err(fail(format!(
                "email `{method}` would shadow a function of the generated mailer ({}); \
                 choose another name",
                RESERVED_METHODS.join(", ")
            )));
        }
        let mut fields: Vec<String> = Vec::new();
        if let Some(list) = list {
            let declared: Vec<&str> = list.split(',').collect();
            if declared.len() > MAX_FIELDS {
                return Err(fail(format!(
                    "email `{method}` has more than {MAX_FIELDS} fields"
                )));
            }
            for field in declared {
                generators::snake_case(
                    &format!("field of email `{method}`"),
                    field,
                    MAX_NAME_LENGTH,
                )
                .map_err(|reason| {
                    fail(format!("{reason}; write fields as {method}:field,field"))
                })?;
                if fields.iter().any(|existing| existing == field) {
                    return Err(fail(format!(
                        "field `{field}` of email `{method}` is declared twice"
                    )));
                }
                fields.push(field.to_owned());
            }
        }
        let pascal = pascal_case(method);
        for function in [method.to_owned(), format!("build_{method}")] {
            if let Some((_, other)) = functions.iter().find(|(name, _)| *name == function) {
                return Err(fail(format!(
                    "email `{method}` generates the function `{function}`, which email \
                     `{other}` already generates; rename one"
                )));
            }
            functions.push((function, method.to_owned()));
        }
        for generated in [
            format!("{type_name}{pascal}"),
            format!("{pascal}Subject"),
            format!("{pascal}Text"),
            format!("{pascal}Html"),
        ] {
            if let Some((_, other)) = types.iter().find(|(name, _)| *name == generated) {
                let owner = if other.is_empty() {
                    "the mailer".to_owned()
                } else {
                    format!("email `{other}`")
                };
                return Err(fail(format!(
                    "email `{method}` generates the type `{generated}`, which {owner} already \
                     uses; rename it"
                )));
            }
            types.push((generated, method.to_owned()));
        }
        methods.push(Method {
            name: method.to_owned(),
            fields,
        });
    }
    Ok(methods)
}

fn sample(field: &str) -> String {
    format!("Sample {}", field.replace('_', " "))
}

/// `UserWelcome { name: "Sample name".to_owned(), ... }`.
fn sample_data(type_name: &str, method: &Method) -> String {
    let fields: Vec<String> = method
        .fields
        .iter()
        .map(|field| format!("{field}: \"{}\".to_owned()", sample(field)))
        .collect();
    format!(
        "{type_name}{} {{ {} }}",
        pascal_case(&method.name),
        fields.join(", ")
    )
}

fn plan(root: &Path, options: &MailerOptions) -> Result<Vec<Planned>, CliError> {
    let methods = parse(options)?;
    let Ok(manifest) = fs::read_to_string(root.join("Cargo.toml")) else {
        return Err(fail(format!(
            "no Cargo.toml in {}; run `forge generate mailer` in the root of an application \
             created by `forge new`",
            root.display()
        )));
    };
    let forge_dependency = manifest
        .lines()
        .find(|line| line.trim_start().starts_with("forge ="))
        .unwrap_or_default();
    if !manifest.lines().any(|line| line.starts_with("askama ="))
        || !(forge_dependency.contains("\"views\"") || forge_dependency.contains("\"mail\""))
    {
        return Err(fail(
            "mailers render askama templates with `forge::mail`: add `askama` and the `mail` \
             feature of `forge` to Cargo.toml (full-stack applications from `forge new` have \
             both, see docs/mail.md)",
        ));
    }
    if !manifest.contains("\"testing\"") {
        return Err(fail(
            "the generated tests need `forge::mail::testing`: add the `forge` dependency to \
             [dev-dependencies] with `features = [..., \"testing\"]` (as `forge new` generates \
             it for database applications)",
        ));
    }
    if !root.join("src/adapters/email/mod.rs").is_file() {
        return Err(fail(
            "src/adapters/email/mod.rs is missing; the mailer is added next to the modules \
             `forge new` generates",
        ));
    }
    let name = &options.name;
    let type_name = pascal_case(name);
    let mut data = String::new();
    let mut functions = String::new();
    let mut clones = String::new();
    let mut previews = String::new();
    let mut tests = String::new();
    let mut imports = vec![format!("{type_name}Mailer")];
    let mut files = Vec::new();
    for method in &methods {
        let method_name = &method.name;
        let pascal = pascal_case(method_name);
        let data_type = format!("{type_name}{pascal}");
        let has_fields = !method.fields.is_empty();
        let title = sentence_case(method_name);
        let directory = format!("templates/mailers/{name}");
        imports.push(data_type.clone());

        let fields: String = method
            .fields
            .iter()
            .map(|field| format!("    pub {field}: String,\n"))
            .collect();
        data.push_str(&format!(
            "\n/// The data of the `{method_name}` email.\n\
             #[derive(Clone, Debug, Default, Eq, PartialEq)]\n\
             pub struct {data_type} {{\n{fields}}}\n"
        ));
        for (part, extension) in [
            ("Subject", "subject.txt"),
            ("Text", "txt"),
            ("Html", "html"),
        ] {
            // The subject is fixed text: it reads no data.
            let declaration = if has_fields && part != "Subject" {
                format!("struct {pascal}{part}<'a> {{\n    data: &'a {data_type},\n}}")
            } else {
                format!("struct {pascal}{part};")
            };
            data.push_str(&format!(
                "\n#[derive(Template)]\n\
                 #[template(path = \"mailers/{name}/{method_name}.{extension}\")]\n\
                 {declaration}\n"
            ));
        }
        let parameter = if has_fields { "data" } else { "_data" };
        let part = |pascal: &str, part: &str| {
            if has_fields && part != "Subject" {
                format!("&{pascal}{part} {{ data }}")
            } else {
                format!("&{pascal}{part}")
            }
        };
        functions.push_str(&format!(
            "\n    /// The `{method_name}` email to `to`, rendered completely but not sent.\n    \
             pub fn build_{method_name}(&self, to: Address, {parameter}: &{data_type}) -> Result<Message, MailError> {{\n        \
             Templates::new()\n            \
             .subject({})\n            \
             .text({})\n            \
             .html({})\n            \
             .message(Message::new(self.from.clone()).to(to))\n    \
             }}\n\n    \
             /// Sends the `{method_name}` email to `to`.\n    \
             pub async fn {method_name}(&self, to: Address, data: &{data_type}) -> Result<(), MailError> {{\n        \
             let message = self.build_{method_name}(to, data)?;\n        \
             self.deliver(&message).await\n    \
             }}\n",
            part(&pascal, "Subject"),
            part(&pascal, "Text"),
            part(&pascal, "Html"),
        ));
        clones.push_str(&format!(
            "        let {method_name}_mailer = self.clone();\n"
        ));
        previews.push_str(&format!(
            "            Preview::new(\"{method_name}\", \"{type_name}Mailer: {}\", move || {{\n                \
             Ok({method_name}_mailer.build_{method_name}(preview_recipient()?, &{})?)\n            \
             }}),\n",
            title.to_lowercase(),
            sample_data(&type_name, method)
        ));

        let samples: Vec<String> = method.fields.iter().map(|field| sample(field)).collect();
        let checks: String = samples
            .iter()
            .map(|value| {
                format!(
                    "    assert!(email.message.text.contains(\"{value}\"));\n    \
                     assert!(email.message.html.contains(\"{value}\"));\n"
                )
            })
            .collect();
        tests.push_str(&format!(
            "\nfn {method_name}_data() -> {data_type} {{\n    {}\n}}\n\n\
             #[tokio::test]\n\
             async fn {method_name}_is_delivered_with_its_subject_and_both_parts() -> TestResult {{\n    \
             let outbox = Arc::new(Recorder::new());\n    \
             mailer(&outbox)?.{method_name}(recipient()?, &{method_name}_data()).await?;\n    \
             assert_eq!(outbox.len(), 1);\n    \
             let email = outbox.last().ok_or(\"one email\")?;\n    \
             assert_eq!(email.recipients, [\"ada@example.org\"]);\n    \
             assert_eq!(email.header(\"Subject\").as_deref(), Some(\"{title}\"));\n    \
             assert!(!email.message.text.is_empty());\n    \
             assert!(!email.message.html.is_empty());\n\
             {checks}    \
             Ok(())\n\
             }}\n",
            sample_data(&type_name, method)
        ));
        if let Some(first) = method.fields.first() {
            let rest = if method.fields.len() > 1 {
                format!(", ..{method_name}_data()")
            } else {
                String::new()
            };
            tests.push_str(&format!(
                "\n#[test]\n\
                 fn {method_name}_escapes_its_data_in_the_html_part() -> TestResult {{\n    \
                 let outbox = Arc::new(Recorder::new());\n    \
                 let hostile = {data_type} {{ {first}: \"<script>alert(1)</script>\".to_owned(){rest} }};\n    \
                 let message = mailer(&outbox)?.build_{method_name}(recipient()?, &hostile)?;\n    \
                 assert!(!message.html.contains(\"<script>\"), \"{{}}\", message.html);\n    \
                 assert!(message.text.contains(\"<script>\"));\n    \
                 Ok(())\n\
                 }}\n"
            ));
        }

        let text_fields: String = method
            .fields
            .iter()
            .map(|field| format!("{}: {{{{ data.{field} }}}}\n", sentence_case(field)))
            .collect();
        let html_fields: String = method
            .fields
            .iter()
            .map(|field| format!("<p>{}: {{{{ data.{field} }}}}</p>\n", sentence_case(field)))
            .collect();
        files.push(Planned::new(
            format!("{directory}/{method_name}.subject.txt"),
            title.clone(),
            Role::Owned,
        ));
        files.push(Planned::new(
            format!("{directory}/{method_name}.txt"),
            format!(
                "{title}\n\n{text_fields}{}Edit this email in {directory}/{method_name}.txt.\n",
                if text_fields.is_empty() { "" } else { "\n" }
            ),
            Role::Owned,
        ));
        files.push(Planned::new(
            format!("{directory}/{method_name}.html"),
            format!(
                "<h1>{title}</h1>\n{html_fields}<p>Edit this email in {directory}/{method_name}.html.</p>\n"
            ),
            Role::Owned,
        ));
    }
    let first = &methods[0];
    let variables = [
        ("%%DATA%%", data),
        ("%%METHODS%%", functions),
        ("%%PREVIEW_CLONES%%", clones.trim_end().to_owned()),
        ("%%PREVIEWS%%", previews.trim_end().to_owned()),
        ("%%TESTS%%", tests),
        ("%%IMPORTS%%", imports.join(", ")),
        ("%%FIRST_DATA%%", format!("{}_data()", first.name)),
        ("%%FIRST%%", first.name.clone()),
        ("%%COUNT%%", methods.len().to_string()),
        ("%%WORDS%%", name.replace('_', " ")),
        ("%%NAME%%", name.clone()),
        ("%%TYPE%%", type_name),
    ];
    let variables: Vec<(&str, &str)> = variables
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let render = |template: &str| render_template(template, &variables, |_| false);
    files.insert(
        0,
        Planned::new(
            format!("src/adapters/email/{name}_mailer.rs"),
            render(MAILER),
            Role::Owned,
        ),
    );
    files.push(Planned::new(
        format!("tests/{name}_mailer.rs"),
        render(TESTS),
        Role::Owned,
    ));
    generators::refuse_symlinks(root, files.iter().map(|file| file.path.as_str())).map_err(fail)?;
    for file in files.iter_mut().filter(|file| file.path.ends_with(".rs")) {
        file.content = scaffold::rustfmt(root, &file.path, &file.content)?;
    }
    Ok(files)
}

/// Generates the mailer in the application at `root`.
pub(crate) fn generate(root: &Path, options: &MailerOptions) -> Result<Outcome, CliError> {
    let files = plan(root, options)?;
    generators::refuse_overwrite(root, GENERATOR, &files)?;
    let wiring = wiring(root, options)?;
    if !options.dry_run {
        generators::write(root, &files)?;
    }
    Ok(Outcome { files, wiring })
}

/// What `forge destroy mailer` removes and the wiring to take out.
pub(crate) fn plan_destroy(
    root: &Path,
    options: &MailerOptions,
) -> Result<(Vec<Planned>, String), CliError> {
    let files = plan(root, options)?;
    Ok((files, wiring(root, options)?))
}

fn wiring(root: &Path, options: &MailerOptions) -> Result<String, CliError> {
    let name = &options.name;
    let type_name = pascal_case(name);
    let mut out = String::from(
        "\nForge did not edit any existing file. Wire the mailer by hand:\n\n  \
         1. Declare the new module (keeping `mod` lines sorted):\n\n",
    );
    out.push_str(&format!(
        "       {:<38} pub mod {name}_mailer;\n",
        "src/adapters/email/mod.rs"
    ));
    out.push_str(
        "\n  2. In src/bootstrap/mod.rs, at the end of `fn compose`, before `Ok(())`:\n\n",
    );
    let code = format!(
        "    // {type_name}Mailer: FORGE_MAIL_URL and FORGE_MAIL_FROM configure delivery (docs/mail.md);\n    \
         // without FORGE_MAIL_URL every send fails with MailError::NotConfigured.\n    \
         let {name}_mailer = crate::adapters::email::{name}_mailer::{type_name}Mailer::from_config(\n        \
         composition.config(),\n        \
         composition.telemetry(),\n    \
         )\n    \
         .map_err(forge::ComposeError::other)?;\n    \
         // FORGE_MAIL_PREVIEW=true (development and test only) shows the emails at\n    \
         // /_mail/{name} without sending them.\n    \
         if composition.config().mail.preview {{\n        \
         let previews = forge::mail::PreviewHandler::new(\n            \
         composition.config(),\n            \
         \"/_mail/{name}\",\n            \
         {name}_mailer.previews(),\n        \
         )\n        \
         .map_err(forge::ComposeError::other)?;\n        \
         composition.router().layer(previews);\n    \
         }}\n"
    );
    out.push_str(&scaffold::format_statements(root, &code)?);
    out.push_str(&format!(
        "\n  Pass `{name}_mailer` (cheap to clone) to the code that sends, for example\n  \
         `{name}_mailer.{}(to, &data).await`. Then run `forge test`.\n",
        options
            .methods
            .first()
            .and_then(|method| method.split(':').next())
            .unwrap_or("welcome")
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(name: &str, methods: &[&str]) -> MailerOptions {
        MailerOptions {
            name: name.to_owned(),
            methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            dry_run: false,
        }
    }

    #[test]
    fn methods_and_fields_are_parsed_and_validated() {
        let methods = parse(&options("user", &["welcome:name,url", "password_reset"]))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(methods[0].fields, ["name", "url"]);
        assert!(methods[1].fields.is_empty());
        for (name, declarations) in [
            ("user_mailer", &["welcome"][..]),
            ("User", &["welcome"][..]),
            ("user", &[][..]),
            ("user", &["Welcome"][..]),
            ("user", &["welcome:"][..]),
            ("user", &["welcome:name,name"][..]),
            ("user", &["welcome:fn"][..]),
            ("user", &["welcome", "welcome"][..]),
            ("user", &["previews"][..]),
            ("user", &["x", "build_x"][..]),
            ("user", &["mailer"][..]),
            ("user", &["welcome:na-me"][..]),
        ] {
            assert!(
                parse(&options(name, declarations)).is_err(),
                "{name} {declarations:?}"
            );
        }
    }
}
