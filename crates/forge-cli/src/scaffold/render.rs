//! Renders the files of a scaffold from `templates/scaffold/` and per-field
//! fragments. Pure: the same resource, backend and migration version always
//! produce the same bytes.

use std::fmt::Write as _;

use super::spec::{Field, FieldType, Resource, Scope, pascal_case, sentence_case, words};
use crate::generate::render_template;

/// The database backend of the application (MySQL and MariaDB share SQL).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Backend {
    Postgresql,
    MySql,
    Sqlite,
}

impl Backend {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Postgresql => "PostgreSQL",
            Self::MySql => "MySQL/MariaDB",
            Self::Sqlite => "SQLite",
        }
    }

    fn quote(self, identifier: &str) -> String {
        match self {
            Self::MySql => format!("`{identifier}`"),
            Self::Postgresql | Self::Sqlite => format!("\"{identifier}\""),
        }
    }

    fn now(self) -> &'static str {
        match self {
            Self::MySql => "CURRENT_TIMESTAMP(6)",
            Self::Postgresql | Self::Sqlite => "CURRENT_TIMESTAMP",
        }
    }

    /// `Database<B>` alias, row type and sqlx database type.
    fn types(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Postgresql => ("PgDatabase", "PgRow", "postgres::PgRow", "sqlx::Postgres"),
            Self::MySql => (
                "MySqlDatabase",
                "MySqlRow",
                "mysql::MySqlRow",
                "sqlx::MySql",
            ),
            Self::Sqlite => (
                "SqliteDatabase",
                "SqliteRow",
                "sqlite::SqliteRow",
                "sqlx::Sqlite",
            ),
        }
    }

    fn repository_prefix(self) -> &'static str {
        match self {
            Self::Postgresql => "Pg",
            Self::MySql => "MySql",
            Self::Sqlite => "Sqlite",
        }
    }
}

macro_rules! scaffold_template {
    ($source:literal) => {
        include_str!(concat!("../../templates/scaffold/", $source))
    };
}

const ENTITY: &str = scaffold_template!("entity.rs.tmpl");
const PORT: &str = scaffold_template!("port.rs.tmpl");
const SERVICE: &str = scaffold_template!("service.rs.tmpl");
const REPOSITORY: &str = scaffold_template!("repository.rs.tmpl");
pub(crate) const ACCESS: &str = scaffold_template!("access.rs.tmpl");
const STREAMS: &str = scaffold_template!("streams.rs.tmpl");
const ROW_PARTIAL: &str = scaffold_template!("templates/_row.html.tmpl");
const HTTP: &str = scaffold_template!("http.rs.tmpl");
const VIEWS: &str = scaffold_template!("views.rs.tmpl");
const TESTS_HTTP: &str = scaffold_template!("tests_http.rs.tmpl");
const TESTS_REPOSITORY: &str = scaffold_template!("tests_repository.rs.tmpl");
const PAGES: &[(&str, &str)] = &[
    (
        "index.html",
        scaffold_template!("templates/index.html.tmpl"),
    ),
    ("show.html", scaffold_template!("templates/show.html.tmpl")),
    ("new.html", scaffold_template!("templates/new.html.tmpl")),
    ("edit.html", scaffold_template!("templates/edit.html.tmpl")),
    (
        "_form.html",
        scaffold_template!("templates/_form.html.tmpl"),
    ),
    (
        "not_found.html",
        scaffold_template!("templates/not_found.html.tmpl"),
    ),
];

/// The shared access-policy module, written by the first scaffold.
pub(crate) const ACCESS_PATH: &str = "src/adapters/http/access.rs";
/// The shared live-update module, written by the first `--broadcast`
/// scaffold.
pub(crate) const STREAMS_PATH: &str = "src/adapters/http/streams.rs";

/// What `--broadcast` adds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Broadcast {
    /// The pages publish their changes and the index subscribes.
    pub(crate) enabled: bool,
    /// Write the shared streams module (the first broadcasting scaffold).
    pub(crate) streams_module: bool,
}

/// One file to write, relative to the application root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct File {
    pub(crate) path: String,
    pub(crate) content: String,
}

/// Every file of `resource` for `backend`, in a stable order. `migration`
/// is the version prefix of the migration pair; `access` adds the shared
/// access-policy module; `locales` are the locales whose
/// `config/locales/PLURAL.LOCALE.json` is written with the English text.
pub(crate) fn files(
    resource: &Resource,
    backend: Backend,
    migration: &str,
    access: bool,
    broadcast: Broadcast,
    locales: &[String],
) -> Vec<File> {
    let renderer = Renderer::new(resource, backend, broadcast.enabled);
    let singular = &resource.singular;
    let plural = &resource.plural;
    let (up, down) = renderer.migration();
    let mut files = vec![
        File {
            path: format!("migrations/{migration}_create_{plural}.up.sql"),
            content: up,
        },
        File {
            path: format!("migrations/{migration}_create_{plural}.down.sql"),
            content: down,
        },
        renderer.file(format!("src/domain/entities/{singular}.rs"), ENTITY),
        renderer.file(
            format!("src/application/ports/{singular}_repository.rs"),
            PORT,
        ),
        renderer.file(format!("src/application/use_cases/{plural}.rs"), SERVICE),
        renderer.file(
            format!("src/infrastructure/database/{plural}.rs"),
            REPOSITORY,
        ),
    ];
    if access {
        files.push(renderer.file(ACCESS_PATH.to_owned(), ACCESS));
    }
    if broadcast.enabled && broadcast.streams_module {
        files.push(renderer.file(STREAMS_PATH.to_owned(), STREAMS));
    }
    files.push(renderer.file(format!("src/adapters/http/{plural}.rs"), HTTP));
    files.push(renderer.file(format!("src/adapters/http/views/{plural}.rs"), VIEWS));
    for (page, template) in PAGES {
        files.push(renderer.file(format!("templates/{plural}/{page}"), template));
    }
    if broadcast.enabled {
        files.push(renderer.file(format!("templates/{plural}/_row.html"), ROW_PARTIAL));
    }
    for locale in locales {
        files.push(File {
            path: format!("config/locales/{plural}.{locale}.json"),
            content: renderer.translations(locale),
        });
    }
    files.push(renderer.file(format!("tests/{plural}_http.rs"), TESTS_HTTP));
    files.push(renderer.file(format!("tests/{plural}_repository.rs"), TESTS_REPOSITORY));
    files
}

/// The up and down migration of `resource`: the same for every version.
pub(crate) fn migration(resource: &Resource, backend: Backend) -> (String, String) {
    Renderer::new(resource, backend, false).migration()
}

struct Renderer<'a> {
    resource: &'a Resource,
    backend: Backend,
    broadcast: bool,
    variables: Vec<(&'static str, String)>,
}

impl<'a> Renderer<'a> {
    fn new(resource: &'a Resource, backend: Backend, broadcast: bool) -> Self {
        let mut renderer = Self {
            resource,
            backend,
            broadcast,
            variables: Vec::new(),
        };
        renderer.variables = renderer.variables();
        renderer
    }

    fn tenant(&self) -> bool {
        self.resource.scope == Scope::Tenant
    }

    fn has(&self, kind: FieldType) -> bool {
        self.resource.fields.iter().any(|field| field.kind == kind)
    }

    fn file(&self, path: String, template: &str) -> File {
        let condition = |name: &str| match name {
            "tenant" => self.tenant(),
            "global" => !self.tenant(),
            "postgresql" => self.backend == Backend::Postgresql,
            "mysql_family" => self.backend == Backend::MySql,
            "sqlite" => self.backend == Backend::Sqlite,
            "rls" => self.tenant() && self.backend == Backend::Postgresql,
            "string" => self.has(FieldType::String),
            "text" => self.has(FieldType::Text),
            "rules" => self
                .resource
                .fields
                .iter()
                .any(|field| field.kind.is_textual()),
            "timestamp" => self.has(FieldType::Timestamp),
            // The HTTP tests build attributes for the tenant grant test and
            // the sub-second timestamp test.
            "http_attributes" => self.tenant() || self.has(FieldType::Timestamp),
            "broadcast" => self.broadcast,
            // The HTTP test harness keeps the tenant of its routes when a
            // test needs it.
            "harness_tenant" => self.tenant() && (self.has(FieldType::Timestamp) || self.broadcast),
            _ => false,
        };
        let variables: Vec<(&str, &str)> = self
            .variables
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        File {
            path,
            content: render_template(template, &variables, condition),
        }
    }

    fn variables(&self) -> Vec<(&'static str, String)> {
        let resource = self.resource;
        let singular = &resource.singular;
        let plural = &resource.plural;
        let type_name = pascal_case(singular);
        let (database, row, row_path, sqlx_database) = self.backend.types();
        let tenant = self.tenant();
        let collection = if tenant {
            format!("/tenants/{{tenant_id}}/{plural}")
        } else {
            format!("/{plural}")
        };
        let mut core = vec!["Id"];
        if self.has(FieldType::Timestamp) {
            core.push("Timestamp");
        }
        if self.has(FieldType::Uuid) {
            core.push("Uuid");
        }
        core.push("Version");
        let mut repository_core = Vec::new();
        if self.has(FieldType::Timestamp) {
            repository_core.push("Timestamp");
        }
        repository_core.push("Version");
        let mut http_core = vec!["ErrorCategory", "ForgeError"];
        if self.has(FieldType::Timestamp) {
            http_core.push("Timestamp");
        }
        if self.has(FieldType::Uuid) {
            http_core.push("Uuid");
        }
        http_core.push("Version");
        let mut test_core = Vec::new();
        if self.has(FieldType::Timestamp) {
            test_core.push("Timestamp");
        }
        if self.has(FieldType::Uuid) {
            test_core.push("Uuid");
        }
        test_core.push("Version");

        let (context_arg, context_ref, context_arg_ref) = if tenant {
            ("context, ", "&context", "&context, ")
        } else {
            ("", "", "")
        };
        let context_param = if tenant {
            "context: &TenantContext, "
        } else {
            ""
        };
        let authorize = |permission: &str| {
            if tenant {
                format!(
                    "    let context = match state.access.authorize(&request, &state.{permission}).await {{\n        \
                     Ok(context) => context,\n        \
                     Err(denied) => return denied.response(),\n    \
                     }};"
                )
            } else {
                format!(
                    "    if let Err(denied) = state.access.authorize(&request, &state.{permission}).await {{\n        \
                     return denied.response();\n    \
                     }}"
                )
            }
        };

        vec![
            ("%%RESOURCES%%", plural.clone()),
            ("%%RESOURCE%%", singular.clone()),
            ("%%TYPES%%", pascal_case(plural)),
            ("%%TYPE%%", type_name.clone()),
            ("%%NOUNS%%", words(plural)),
            ("%%NOUN%%", words(singular)),
            ("%%LABELS%%", sentence_case(plural)),
            ("%%LABEL%%", sentence_case(singular)),
            ("%%BACKEND_LABEL%%", self.backend.label().to_owned()),
            (
                "%%REPOSITORY%%",
                format!("{}{type_name}Repository", self.backend.repository_prefix()),
            ),
            ("%%DATABASE_TYPE%%", database.to_owned()),
            ("%%ROW_TYPE%%", row.to_owned()),
            ("%%ROW_PATH%%", row_path.to_owned()),
            ("%%SQLX_DATABASE%%", sqlx_database.to_owned()),
            ("%%COLLECTION_PATTERN%%", collection),
            (
                "%%ACCESS_TRAIT%%",
                if tenant {
                    "TenantAccess"
                } else {
                    "GlobalAccess"
                }
                .to_owned(),
            ),
            (
                "%%ALLOW_ACCESS%%",
                if tenant {
                    "MemberOfRouteTenant"
                } else {
                    "AllowAll"
                }
                .to_owned(),
            ),
            (
                "%%MEMORY_ROW%%",
                if tenant {
                    format!("(TenantId, {type_name})")
                } else {
                    type_name.clone()
                },
            ),
            ("%%CONTEXT_ARG_REF%%", context_arg_ref.to_owned()),
            ("%%CONTEXT_PARAM%%", context_param.to_owned()),
            (
                "%%CONTEXT_NAME%%",
                if tenant { "context" } else { "" }.to_owned(),
            ),
            (
                "%%BROADCAST_SCOPE_DOC%%",
                if tenant {
                    " of the authorized tenant"
                } else {
                    ""
                }
                .to_owned(),
            ),
            (
                "%%SCOPE%%",
                if tenant { "tenant" } else { "global" }.to_owned(),
            ),
            ("%%CONTEXT_ARG%%", context_arg.to_owned()),
            ("%%CONTEXT_REF%%", context_ref.to_owned()),
            (
                "%%TENANT_ARG%%",
                if tenant { "&tenant, " } else { "" }.to_owned(),
            ),
            ("%%AUTHORIZE_READ%%", authorize("read")),
            ("%%AUTHORIZE_WRITE%%", authorize("write")),
            (
                "%%EXECUTOR%%",
                if tenant && self.backend == Backend::Postgresql {
                    "&mut *transaction"
                } else {
                    "self.database.pool()"
                }
                .to_owned(),
            ),
            (
                "%%RLS_DDL_SUMMARY%%",
                if tenant && self.backend == Backend::Postgresql {
                    " or switch its row-level security off"
                } else {
                    ""
                }
                .to_owned(),
            ),
            (
                "%%ISOLATION_SUMMARY%%",
                if tenant {
                    ", plus tenant isolation".to_owned()
                } else {
                    String::new()
                },
            ),
            ("%%CORE_IMPORTS%%", core.join(", ")),
            ("%%REPOSITORY_CORE_IMPORTS%%", use_list(&repository_core)),
            ("%%HTTP_CORE_IMPORTS%%", http_core.join(", ")),
            (
                "%%HTTP_TEST_CORE_IMPORTS%%",
                if self.tenant() || self.has(FieldType::Timestamp) {
                    use_list(&test_core)
                } else {
                    "Version".to_owned()
                },
            ),
            ("%%TEST_CORE_IMPORTS%%", use_list(&test_core)),
            ("%%ATTRIBUTE_FIELDS%%", self.attribute_fields()),
            ("%%VALIDATIONS%%", self.validations()),
            ("%%GETTERS%%", self.getters()),
            ("%%FIELD_VARIANTS%%", self.field_variants()),
            ("%%FIELD_NAMES%%", self.field_names()),
            ("%%SAMPLE_ATTRIBUTES%%", self.sample_attributes(false)),
            ("%%TEST_ATTRIBUTES%%", self.sample_attributes(true)),
            ("%%BLANK_ASSIGNMENTS%%", self.blank_assignments()),
            ("%%BLANK_ASSERTIONS%%", self.blank_assertions()),
            ("%%SQL_CONSTANTS%%", self.sql_constants()),
            ("%%ATTRIBUTE_BINDS%%", self.attribute_binds()),
            ("%%RESTORE_FIELDS%%", self.restore_fields()),
            ("%%ROW_STRUCT_FIELDS%%", self.row_struct_fields()),
            (
                "%%FORM_VALUE_STRUCT_FIELDS%%",
                self.form_value_struct_fields(),
            ),
            ("%%FORM_STRUCT_FIELDS%%", self.form_struct_fields()),
            ("%%FORM_TO_VALUES%%", self.form_to_values()),
            ("%%FORM_TO_ATTRIBUTES%%", self.form_to_attributes()),
            ("%%ENTITY_TO_VALUES%%", self.entity_to_values()),
            ("%%ENTITY_TO_ROW%%", self.entity_to_row()),
            ("%%PARSE_HELPERS%%", self.parse_helpers()),
            ("%%INDEX_HEADERS%%", self.index_headers()),
            ("%%INDEX_CELLS%%", self.index_cells()),
            ("%%SHOW_FIELDS%%", self.show_fields()),
            ("%%FORM_INPUTS%%", self.form_inputs()),
            ("%%VALID_FORM%%", self.valid_form()),
            ("%%SUBSECOND_FORM%%", self.subsecond_form()),
            ("%%SHOWN_VALUE%%", shown_value(&resource.fields[0])),
            ("%%INVALID_FORM%%", self.invalid_form().0),
            ("%%INVALID_MESSAGE%%", self.invalid_form().1),
        ]
    }

    fn fields(&self) -> impl Iterator<Item = &Field> {
        self.resource.fields.iter()
    }

    // ------------------------------------------------------------- domain

    fn attribute_fields(&self) -> String {
        self.fields()
            .map(|field| format!("    pub {}: {},", field.name, rust_type(field)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn validations(&self) -> String {
        let type_name = pascal_case(&self.resource.singular);
        let mut out = Vec::new();
        for field in self.fields().filter(|field| field.kind.is_textual()) {
            let max = if field.kind == FieldType::String {
                "STRING_MAX_CHARS"
            } else {
                "TEXT_MAX_CHARS"
            };
            let variant = pascal_case(&field.name);
            let name = &field.name;
            if field.optional {
                out.push(format!(
                    "        if let Some(value) = &self.{name} {{\n            \
                     check_text(&mut errors, {type_name}Field::{variant}, value, {max});\n        \
                     }}"
                ));
            } else {
                out.push(format!(
                    "        check_text(&mut errors, {type_name}Field::{variant}, &self.{name}, {max});"
                ));
            }
        }
        out.join("\n")
    }

    fn getters(&self) -> String {
        let mut out = String::new();
        for field in self.fields() {
            let name = &field.name;
            let (return_type, body) = match (field.kind.is_textual(), field.optional) {
                (true, false) => ("&str".to_owned(), format!("&self.attributes.{name}")),
                (true, true) => (
                    "Option<&str>".to_owned(),
                    format!("self.attributes.{name}.as_deref()"),
                ),
                (false, _) => (rust_type(field), format!("self.attributes.{name}")),
            };
            let _ = write!(
                out,
                "\n    #[must_use]\n    pub fn {name}(&self) -> {return_type} {{\n        {body}\n    }}\n"
            );
        }
        out.trim_end_matches('\n').to_owned()
    }

    fn field_variants(&self) -> String {
        self.fields()
            .map(|field| format!("    {},", pascal_case(&field.name)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn field_names(&self) -> String {
        self.fields()
            .map(|field| {
                format!(
                    "            Self::{} => \"{}\",",
                    pascal_case(&field.name),
                    field.name
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Valid attribute values; `fallible` uses `Timestamp::parse(..)?` with
    /// microseconds (repository round trips), otherwise a constant.
    fn sample_attributes(&self, fallible: bool) -> String {
        let indent = if fallible { "        " } else { "            " };
        self.fields()
            .map(|field| {
                let value = match field.kind {
                    FieldType::String | FieldType::Text => {
                        format!("\"{}\".to_owned()", sample_text(field))
                    }
                    FieldType::Integer => "42".to_owned(),
                    FieldType::Boolean => "true".to_owned(),
                    FieldType::Timestamp if fallible => {
                        "Timestamp::parse(\"2026-01-02T03:04:05.123456Z\")?".to_owned()
                    }
                    FieldType::Timestamp => "Timestamp::UNIX_EPOCH".to_owned(),
                    FieldType::Uuid => format!("Uuid::from_u128({SAMPLE_UUID_HEX})"),
                };
                let value = if field.optional {
                    format!("Some({value})")
                } else {
                    value
                };
                format!("{indent}{}: {value},", field.name)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn blank_assignments(&self) -> String {
        let mut out = Vec::new();
        let mut long_done = false;
        for field in self.fields().filter(|field| field.kind.is_textual()) {
            let max = if field.kind == FieldType::String {
                "STRING_MAX_CHARS"
            } else {
                "TEXT_MAX_CHARS"
            };
            // The first textual field is too long, the others blank.
            let value = if long_done {
                "\" \".to_owned()".to_owned()
            } else {
                long_done = true;
                format!("\"x\".repeat({max} + 1)")
            };
            let value = if field.optional {
                format!("Some({value})")
            } else {
                value
            };
            out.push(format!("        attributes.{} = {value};", field.name));
        }
        out.join("\n")
    }

    fn blank_assertions(&self) -> String {
        let type_name = pascal_case(&self.resource.singular);
        let mut out = Vec::new();
        let mut first = true;
        for field in self.fields().filter(|field| field.kind.is_textual()) {
            let max = if field.kind == FieldType::String {
                "STRING_MAX_CHARS"
            } else {
                "TEXT_MAX_CHARS"
            };
            let expected = if first {
                format!("{type_name}Violation::TooLong {{ max: {max} }}")
            } else {
                format!("{type_name}Violation::Blank")
            };
            first = false;
            out.push(format!(
                "        assert_eq!(\n            errors.get({type_name}Field::{}),\n            Some({expected})\n        );",
                pascal_case(&field.name)
            ));
        }
        out.join("\n")
    }

    // ---------------------------------------------------------------- SQL

    fn migration(&self) -> (String, String) {
        let backend = self.backend;
        let q = |name: &str| backend.quote(name);
        let plural = &self.resource.plural;
        let table = q(plural);
        let tenant = self.tenant();
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        let id_type = match backend {
            Backend::Postgresql => "uuid",
            Backend::MySql => "BINARY(16)",
            Backend::Sqlite => "BLOB",
        };
        columns.push(format!("{} {id_type} NOT NULL PRIMARY KEY", q("id")));
        if tenant {
            columns.push(format!("{} {id_type} NOT NULL", q("tenant_id")));
        }
        for field in &self.resource.fields {
            columns.push(self.column(field));
        }
        let (bigint, timestamp) = match backend {
            Backend::Postgresql => ("bigint", "timestamptz"),
            Backend::MySql => ("BIGINT", "DATETIME(6)"),
            Backend::Sqlite => ("INTEGER", "TEXT"),
        };
        let version_check = format!(
            "CONSTRAINT {} CHECK ({} >= 1)",
            q(&format!("{plural}_version_positive")),
            q("version")
        );
        if backend == Backend::MySql {
            columns.push(format!("{} {bigint} NOT NULL DEFAULT 1", q("version")));
            constraints.push(version_check);
        } else {
            columns.push(format!(
                "{} {bigint} NOT NULL DEFAULT 1 {version_check}",
                q("version")
            ));
        }
        for column in ["created_at", "updated_at"] {
            columns.push(format!(
                "{} {timestamp} NOT NULL DEFAULT {}",
                q(column),
                backend.now()
            ));
        }
        for field in &self.resource.fields {
            if backend == Backend::MySql
                && let Some(check) = self.check(field)
            {
                constraints.push(check);
            }
        }
        let index = q(&format!("{plural}_tenant_id_idx"));
        if tenant && backend == Backend::MySql {
            constraints.push(format!("INDEX {index} ({}, {})", q("tenant_id"), q("id")));
        }

        let mut up = format!(
            "-- {plural}: created by `forge generate scaffold` for {}.\n\
             -- Identifiers are UUIDv7 values generated by the application; the\n\
             -- database never generates them. `version` is the optimistic lock.\n",
            backend.label()
        );
        if tenant {
            up.push_str(match backend {
                Backend::Postgresql => {
                    "-- Rows belong to a tenant: the application binds `tenant_id` from the\n\
                     -- authorized TenantContext, and row-level security (forced, so it also\n\
                     -- applies to the table owner) is defense in depth.\n"
                }
                Backend::MySql | Backend::Sqlite => {
                    "-- Rows belong to a tenant. This backend has no row-level security\n\
                     -- (ADR 0021): isolation rests on the application binding `tenant_id`\n\
                     -- from the authorized TenantContext in every statement.\n"
                }
            });
        }
        let _ = write!(up, "\nCREATE TABLE {table} (\n");
        let lines: Vec<String> = columns
            .into_iter()
            .chain(constraints)
            .map(|line| format!("    {line}"))
            .collect();
        up.push_str(&lines.join(",\n"));
        up.push_str("\n)");
        if backend == Backend::MySql {
            up.push_str(" ENGINE = InnoDB DEFAULT CHARSET = utf8mb4");
        }
        up.push_str(";\n");
        if tenant && backend != Backend::MySql {
            let _ = write!(
                up,
                "\nCREATE INDEX {index} ON {table} ({}, {});\n",
                q("tenant_id"),
                q("id")
            );
        }
        if backend == Backend::Postgresql {
            if tenant {
                let policy = q(&format!("{plural}_tenant_isolation"));
                let tenant_id = q("tenant_id");
                let _ = write!(
                    up,
                    "\nALTER TABLE {table} ENABLE ROW LEVEL SECURITY;\n\
                     ALTER TABLE {table} FORCE ROW LEVEL SECURITY;\n\
                     \n\
                     -- Without `forge.tenant_id` (set per transaction by\n\
                     -- begin_tenant_transaction) no row is visible or writable.\n\
                     CREATE POLICY {policy} ON {table}\n    \
                     FOR ALL\n    \
                     USING ({tenant_id} = NULLIF(current_setting('forge.tenant_id', true), '')::uuid)\n    \
                     WITH CHECK ({tenant_id} = NULLIF(current_setting('forge.tenant_id', true), '')::uuid);\n"
                );
            }
            let _ = write!(
                up,
                "\nREVOKE ALL ON TABLE {table} FROM PUBLIC;\n\
                 \n\
                 DO $$\n\
                 BEGIN\n    \
                 IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_runtime') THEN\n        \
                 GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE {table} TO app_runtime;\n    \
                 END IF;\n\
                 END\n\
                 $$;\n"
            );
        }
        let down = format!(
            "-- Reverts the {plural} table created by `forge generate scaffold`.\n\nDROP TABLE IF EXISTS {table};\n"
        );
        (up, down)
    }

    fn column(&self, field: &Field) -> String {
        let backend = self.backend;
        let sql_type = match (backend, field.kind) {
            (Backend::Postgresql, FieldType::String | FieldType::Text) => "text",
            (Backend::Postgresql, FieldType::Integer) => "bigint",
            (Backend::Postgresql, FieldType::Boolean) => "boolean",
            (Backend::Postgresql, FieldType::Timestamp) => "timestamptz",
            (Backend::Postgresql, FieldType::Uuid) => "uuid",
            (Backend::MySql, FieldType::String) => "VARCHAR(255)",
            (Backend::MySql, FieldType::Text) => "MEDIUMTEXT",
            (Backend::MySql, FieldType::Integer) => "BIGINT",
            (Backend::MySql, FieldType::Boolean) => "BOOLEAN",
            (Backend::MySql, FieldType::Timestamp) => "DATETIME(6)",
            (Backend::MySql, FieldType::Uuid) => "BINARY(16)",
            (Backend::Sqlite, FieldType::String | FieldType::Text | FieldType::Timestamp) => "TEXT",
            (Backend::Sqlite, FieldType::Integer | FieldType::Boolean) => "INTEGER",
            (Backend::Sqlite, FieldType::Uuid) => "BLOB",
        };
        let null = if field.optional { "" } else { " NOT NULL" };
        let mut column = format!("{} {sql_type}{null}", backend.quote(&field.name));
        if backend != Backend::MySql
            && let Some(check) = self.check(field)
        {
            column.push(' ');
            column.push_str(&check);
        }
        column
    }

    /// Database-level checks mirroring the domain rules (defense in depth).
    fn check(&self, field: &Field) -> Option<String> {
        let backend = self.backend;
        let q = |name: &str| backend.quote(name);
        let name = q(&field.name);
        let constraint =
            |suffix: &str| q(&format!("{}_{}_{suffix}", self.resource.plural, field.name));
        match (backend, field.kind) {
            (Backend::Postgresql, FieldType::String) => Some(format!(
                "CONSTRAINT {} CHECK (char_length({name}) <= 255)",
                constraint("length")
            )),
            (Backend::Sqlite, FieldType::String) => Some(format!(
                "CONSTRAINT {} CHECK (length({name}) <= 255)",
                constraint("length")
            )),
            (Backend::Sqlite, FieldType::Boolean) => Some(format!(
                "CONSTRAINT {} CHECK ({name} IN (0, 1))",
                constraint("boolean")
            )),
            _ => None,
        }
    }

    fn placeholder(&self, index: usize) -> String {
        match self.backend {
            Backend::Postgresql => format!("${index}"),
            Backend::MySql | Backend::Sqlite => "?".to_owned(),
        }
    }

    fn sql_constants(&self) -> String {
        let backend = self.backend;
        let q = |name: &str| backend.quote(name);
        let table = q(&self.resource.plural);
        let tenant = self.tenant();
        let field_columns: Vec<String> = self.fields().map(|field| q(&field.name)).collect();
        let mut select_columns = vec![q("id"), q("version")];
        select_columns.extend(field_columns.iter().cloned());
        let select = format!(
            "SELECT {}\n    FROM {table}",
            wrap(&select_columns, "\n        ")
        );

        let mut index = 0;
        let mut next = || {
            index += 1;
            self.placeholder(index)
        };
        let tenant_predicate = |next: &mut dyn FnMut() -> String| {
            if tenant {
                format!("{} = {} AND ", q("tenant_id"), next())
            } else {
                String::new()
            }
        };

        let find = format!(
            "{select}\n    WHERE {}{} = {}",
            tenant_predicate(&mut next),
            q("id"),
            next()
        );
        index = 0;
        let mut next = || {
            index += 1;
            self.placeholder(index)
        };
        let page_first = if tenant {
            format!(
                "{select}\n    WHERE {} = {}\n    ORDER BY {}\n    LIMIT {}",
                q("tenant_id"),
                next(),
                q("id"),
                next()
            )
        } else {
            format!("{select}\n    ORDER BY {}\n    LIMIT {}", q("id"), next())
        };
        index = 0;
        let mut next = || {
            index += 1;
            self.placeholder(index)
        };
        let page_after = format!(
            "{select}\n    WHERE {}{} > {}\n    ORDER BY {}\n    LIMIT {}",
            tenant_predicate(&mut next),
            q("id"),
            next(),
            q("id"),
            next()
        );

        index = 0;
        let mut next = || {
            index += 1;
            self.placeholder(index)
        };
        let mut insert_columns = vec![q("id")];
        if tenant {
            insert_columns.push(q("tenant_id"));
        }
        insert_columns.push(q("version"));
        insert_columns.extend(field_columns.iter().cloned());
        let values: Vec<String> = insert_columns.iter().map(|_| next()).collect();
        let insert = format!(
            "INSERT INTO {table} (\n        {}\n    )\n    VALUES ({})",
            wrap(&insert_columns, "\n        "),
            wrap(&values, "\n        ")
        );

        index = 0;
        let mut next = || {
            index += 1;
            self.placeholder(index)
        };
        let mut assignments: Vec<String> = field_columns
            .iter()
            .map(|column| format!("{column} = {}", next()))
            .collect();
        assignments.push(format!("{0} = {0} + 1", q("version")));
        assignments.push(format!("{} = {}", q("updated_at"), backend.now()));
        let update = format!(
            "UPDATE {table}\n    SET {}\n    WHERE {}{} = {} AND {} = {}",
            assignments.join(",\n        "),
            tenant_predicate(&mut next),
            q("id"),
            next(),
            q("version"),
            next()
        );

        index = 0;
        let mut next = || {
            index += 1;
            self.placeholder(index)
        };
        let delete = format!(
            "DELETE FROM {table}\n    WHERE {}{} = {} AND {} = {}",
            tenant_predicate(&mut next),
            q("id"),
            next(),
            q("version"),
            next()
        );

        let mut out = String::new();
        for (name, doc, sql) in [
            ("FIND", "One row by id.", find),
            ("PAGE_FIRST", "The first page, in id order.", page_first),
            (
                "PAGE_AFTER",
                "The page after a cursor, in id order.",
                page_after,
            ),
            ("INSERT", "A new row at the initial version.", insert),
            (
                "UPDATE",
                "Replaces the attributes if the version still matches.",
                update,
            ),
            (
                "DELETE",
                "Deletes the row if the version still matches.",
                delete,
            ),
        ] {
            let _ = write!(out, "/// {doc}\nconst {name}: &str = r#\"{sql}\"#;\n\n");
        }
        out.trim_end().to_owned()
    }

    fn attribute_binds(&self) -> String {
        self.fields()
            .map(|field| {
                let name = &field.name;
                let value = match (field.kind, field.optional) {
                    (FieldType::String | FieldType::Text, _) => format!("&attributes.{name}"),
                    (FieldType::Timestamp, false) => format!("to_datetime(attributes.{name})?"),
                    (FieldType::Timestamp, true) => {
                        format!("attributes.{name}.map(to_datetime).transpose()?")
                    }
                    _ => format!("attributes.{name}"),
                };
                format!("            .bind({value})")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn restore_fields(&self) -> String {
        self.fields()
            .map(|field| {
                let name = &field.name;
                let get = format!("row.try_get(\"{name}\").map_err(corrupt)?");
                let value = match (field.kind, field.optional) {
                    (FieldType::Timestamp, false) => format!("from_datetime({get})?"),
                    (FieldType::Timestamp, true) => format!(
                        "row.try_get::<Option<DateTime<Utc>>, _>(\"{name}\")\n            .map_err(corrupt)?\n            .map(from_datetime)\n            .transpose()?"
                    ),
                    _ => get,
                };
                format!("        {name}: {value},")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // --------------------------------------------------------------- HTTP

    fn row_struct_fields(&self) -> String {
        self.fields()
            .map(|field| format!("    pub {}: String,", field.name))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn form_value_struct_fields(&self) -> String {
        self.fields()
            .map(|field| {
                let kind = if field.kind == FieldType::Boolean && !field.optional {
                    "bool"
                } else {
                    "String"
                };
                format!("    pub {}: {kind},", field.name)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn form_struct_fields(&self) -> String {
        self.fields()
            .map(|field| {
                let kind = if field.kind == FieldType::Boolean && !field.optional {
                    "bool"
                } else {
                    "String"
                };
                format!("    #[serde(default)]\n    {}: {kind},", field.name)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn form_to_values(&self) -> String {
        self.fields()
            .map(|field| {
                let name = &field.name;
                if field.kind == FieldType::Boolean && !field.optional {
                    format!("            {name}: self.{name},")
                } else {
                    format!("            {name}: self.{name}.clone(),")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn form_to_attributes(&self) -> String {
        self.fields()
            .map(|field| {
                let name = &field.name;
                let value = match (field.kind, field.optional) {
                    (FieldType::String | FieldType::Text, false) => format!("self.{name}.clone()"),
                    (FieldType::String | FieldType::Text, true) => {
                        format!("optional_text(&self.{name})")
                    }
                    (FieldType::Boolean, false) => format!("self.{name}"),
                    (FieldType::Boolean, true) => {
                        format!("optional_boolean(&mut errors, \"{name}\", &self.{name})")
                    }
                    (FieldType::Integer, false) => {
                        format!("integer(&mut errors, \"{name}\", &self.{name}).unwrap_or_default()")
                    }
                    (FieldType::Integer, true) => {
                        format!("optional(&mut errors, \"{name}\", &self.{name}, integer)")
                    }
                    (FieldType::Timestamp, false) => format!(
                        "timestamp(&mut errors, \"{name}\", &self.{name})\n                .unwrap_or(Timestamp::UNIX_EPOCH)"
                    ),
                    (FieldType::Timestamp, true) => {
                        format!("optional(&mut errors, \"{name}\", &self.{name}, timestamp)")
                    }
                    (FieldType::Uuid, false) => {
                        format!("uuid(&mut errors, \"{name}\", &self.{name}).unwrap_or_default()")
                    }
                    (FieldType::Uuid, true) => {
                        format!("optional(&mut errors, \"{name}\", &self.{name}, uuid)")
                    }
                };
                format!("            {name}: {value},")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn entity_to_values(&self) -> String {
        let singular = &self.resource.singular;
        self.fields()
            .map(|field| {
                let name = &field.name;
                let get = format!("{singular}.{name}()");
                let value = match (field.kind, field.optional) {
                    (FieldType::String | FieldType::Text, false) => format!("{get}.to_owned()"),
                    (FieldType::String | FieldType::Text, true) => {
                        format!("{get}.unwrap_or_default().to_owned()")
                    }
                    (FieldType::Boolean, false) => get,
                    (FieldType::Timestamp, false) => format!("{get}.to_datetime_local()"),
                    (FieldType::Timestamp, true) => format!(
                        "{get}\n                .map(Timestamp::to_datetime_local)\n                .unwrap_or_default()"
                    ),
                    (_, false) => format!("{get}.to_string()"),
                    (_, true) => format!(
                        "{get}\n                .map(|value| value.to_string())\n                .unwrap_or_default()"
                    ),
                };
                format!("        {name}: {value},")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn entity_to_row(&self) -> String {
        let singular = &self.resource.singular;
        self.fields()
            .map(|field| {
                let name = &field.name;
                let get = format!("{singular}.{name}()");
                let value = match (field.kind, field.optional) {
                    (FieldType::String | FieldType::Text, false) => format!("{get}.to_owned()"),
                    (FieldType::String | FieldType::Text, true) => {
                        format!("{get}.unwrap_or_default().to_owned()")
                    }
                    (FieldType::Boolean, false) => format!("yes_no({get})"),
                    (FieldType::Boolean, true) => format!("{get}.map(yes_no).unwrap_or_default()"),
                    (_, false) => format!("{get}.to_string()"),
                    (_, true) => format!(
                        "{get}\n            .map(|value| value.to_string())\n            .unwrap_or_default()"
                    ),
                };
                format!("        {name}: {value},")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn parse_helpers(&self) -> String {
        let optional_of = |kind: FieldType| {
            self.fields()
                .any(|field| field.kind == kind && field.optional)
        };
        let mut helpers = Vec::new();
        if self.has(FieldType::Boolean) {
            helpers.push(
                "fn yes_no(value: bool) -> String {\n    \
                 if value { \"Yes\" } else { \"No\" }.to_owned()\n\
                 }"
                .to_owned(),
            );
        }
        if self
            .fields()
            .any(|field| field.kind.is_textual() && field.optional)
        {
            helpers.push(
                "/// A blank optional text is no value.\n\
                 fn optional_text(value: &str) -> Option<String> {\n    \
                 if value.trim().is_empty() {\n        \
                 None\n    \
                 } else {\n        \
                 Some(value.to_owned())\n    \
                 }\n\
                 }"
                .to_owned(),
            );
        }
        if optional_of(FieldType::Boolean) {
            helpers.push(
                "/// The select of an optional checkbox: blank, `true` or `false`.\n\
                 fn optional_boolean(errors: &mut FieldErrors, field: &str, value: &str) -> Option<bool> {\n    \
                 match value {\n        \
                 \"\" => None,\n        \
                 \"true\" => Some(true),\n        \
                 \"false\" => Some(false),\n        \
                 _ => {\n            \
                 errors.add(field, \"must be yes, no or blank\");\n            \
                 None\n        \
                 }\n    \
                 }\n\
                 }"
                .to_owned(),
            );
        }
        if optional_of(FieldType::Integer)
            || optional_of(FieldType::Timestamp)
            || optional_of(FieldType::Uuid)
        {
            helpers.push(
                "/// A blank optional value is no value; anything else must parse.\n\
                 fn optional<T>(\n    \
                 errors: &mut FieldErrors,\n    \
                 field: &str,\n    \
                 value: &str,\n    \
                 parse: fn(&mut FieldErrors, &str, &str) -> Option<T>,\n\
                 ) -> Option<T> {\n    \
                 if value.trim().is_empty() {\n        \
                 None\n    \
                 } else {\n        \
                 parse(errors, field, value)\n    \
                 }\n\
                 }"
                .to_owned(),
            );
        }
        for (kind, name, rust, parse, message) in [
            (
                FieldType::Integer,
                "integer",
                "i64",
                "value.trim().parse().ok()",
                "must be a whole number",
            ),
            (
                FieldType::Timestamp,
                "timestamp",
                "Timestamp",
                // Whole seconds only: the form renders and edits timestamps
                // with `step="1"`, so a round trip through it is exact.
                "Timestamp::parse(value.trim())\n        .ok()\n        .filter(|parsed| parsed.subsec_micros() == 0)",
                "must be a date and time in whole seconds (UTC)",
            ),
            (
                FieldType::Uuid,
                "uuid",
                "Uuid",
                "Uuid::parse_str(value.trim()).ok()",
                "must be a UUID",
            ),
        ] {
            if !self.has(kind) {
                continue;
            }
            helpers.push(format!(
                "fn {name}(errors: &mut FieldErrors, field: &str, value: &str) -> Option<{rust}> {{\n    \
                 if value.trim().is_empty() {{\n        \
                 errors.add(field, \"can't be blank\");\n        \
                 return None;\n    \
                 }}\n    \
                 let parsed = {parse};\n    \
                 if parsed.is_none() {{\n        \
                 errors.add(field, \"{message}\");\n    \
                 }}\n    \
                 parsed\n\
                 }}"
            ));
        }
        if helpers.is_empty() {
            String::new()
        } else {
            format!("\n{}", helpers.join("\n\n"))
        }
    }

    // ---------------------------------------------------------- templates

    /// `config/locales/PLURAL.LOCALE.json`: the English text of the pages
    /// and messages under `locale` (en, or the application's default locale,
    /// where it is the text to translate). `forge i18n check` lists what
    /// other locales lack.
    fn translations(&self, locale: &str) -> String {
        use serde_json::{Map, Value, json};

        let noun = words(&self.resource.singular);
        let nouns = words(&self.resource.plural);
        let label = sentence_case(&self.resource.singular);
        let labels = sentence_case(&self.resource.plural);
        let mut fields = Map::new();
        for field in self.fields() {
            fields.insert(field.name.clone(), Value::from(sentence_case(&field.name)));
        }
        let mut form = Map::new();
        form.insert("create".to_owned(), json!(format!("Create {noun}")));
        form.insert("update".to_owned(), json!(format!("Update {noun}")));
        form.insert(
            "no_version".to_owned(),
            json!("The form has no valid version; reload the page."),
        );
        if self.has(FieldType::Timestamp) {
            form.insert("utc".to_owned(), json!("(UTC)"));
        }
        if self
            .fields()
            .any(|field| field.kind == FieldType::Boolean && field.optional)
        {
            form.insert("not_set".to_owned(), json!("Not set"));
            form.insert("yes".to_owned(), json!("Yes"));
            form.insert("no".to_owned(), json!("No"));
        }
        let scope = json!({
            "back": format!("Back to {nouns}"),
            "fields": fields,
            "form": form,
            "index": {
                "title": labels,
                "new": format!("New {noun}"),
                "actions": "Actions",
                "empty": format!("No {nouns} yet."),
                "next_page": "Next page",
                "show": "Show",
                "edit": "Edit",
            },
            "show": {
                "title": label,
                "delete": format!("Delete {noun}"),
                "confirm_delete": format!("Delete this {noun}?"),
            },
            "new": {"title": format!("New {noun}")},
            "edit": {
                "title": format!("Editing {noun}"),
                "conflict": format!(
                    "This {noun} was changed by someone else after you opened the form. \
                     Review your changes and save again to overwrite."
                ),
            },
            "not_found": {
                "title": format!("{label} not found"),
                "message": format!("The {noun} does not exist or was deleted."),
            },
            "flash": {
                "created": format!("{label} was successfully created."),
                "updated": format!("{label} was successfully updated."),
                "deleted": format!("{label} was successfully deleted."),
                "stale": format!(
                    "This {noun} was changed by someone else. Review it and delete it again."
                ),
                "no_version": "The delete form has no valid version; reload the page.",
            },
        });
        let mut plural = Map::new();
        plural.insert(self.resource.plural.clone(), scope);
        let mut root = Map::new();
        root.insert(locale.to_owned(), Value::Object(plural));
        // Serializing a map of strings cannot fail.
        let text = serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default();
        format!("{text}\n")
    }

    /// `{{ layout.i18n.t("posts.fields.title") }}`.
    fn field_label(&self, field: &Field, i18n: &str) -> String {
        format!(
            "{{{{ {i18n}.t(\"{}.fields.{}\") }}}}",
            self.resource.plural, field.name
        )
    }

    fn text(&self, key: &str) -> String {
        format!(
            "{{{{ layout.i18n.t(\"{}.{key}\") }}}}",
            self.resource.plural
        )
    }

    fn index_headers(&self) -> String {
        self.fields()
            .map(|field| {
                format!(
                    "      <th scope=\"col\">{}</th>",
                    self.field_label(field, "layout.i18n")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn index_cells(&self) -> String {
        self.fields()
            .map(|field| format!("      <td>{{{{ row.{} }}}}</td>", field.name))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn show_fields(&self) -> String {
        let singular = &self.resource.singular;
        self.fields()
            .map(|field| {
                format!(
                    "  <dt>{}</dt>\n  <dd>{{{{ {singular}.{} }}}}</dd>",
                    self.field_label(field, "layout.i18n"),
                    field.name
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn form_inputs(&self) -> String {
        let singular = &self.resource.singular;
        self.fields()
            .map(|field| {
                let name = &field.name;
                let id = format!("{singular}_{name}");
                let mut label = self.field_label(field, "layout.i18n");
                if field.kind == FieldType::Timestamp {
                    label.push(' ');
                    label.push_str(&self.text("form.utc"));
                }
                let input = match (field.kind, field.optional) {
                    (FieldType::String, _) => format!(
                        "<input type=\"text\" id=\"{id}\" name=\"{name}\" value=\"{{{{ form.{name} }}}}\" maxlength=\"255\">"
                    ),
                    (FieldType::Text, _) => format!(
                        "<textarea id=\"{id}\" name=\"{name}\" rows=\"6\">{{{{ form.{name} }}}}</textarea>"
                    ),
                    (FieldType::Integer, _) => format!(
                        "<input type=\"number\" id=\"{id}\" name=\"{name}\" value=\"{{{{ form.{name} }}}}\" step=\"1\">"
                    ),
                    (FieldType::Boolean, false) => format!(
                        "<input type=\"checkbox\" id=\"{id}\" name=\"{name}\" value=\"true\"{{% if form.{name} %}} checked{{% endif %}}>"
                    ),
                    (FieldType::Boolean, true) => format!(
                        "<select id=\"{id}\" name=\"{name}\">\n      \
                         <option value=\"\">{}</option>\n      \
                         <option value=\"true\"{{% if form.{name} == \"true\" %}} selected{{% endif %}}>{}</option>\n      \
                         <option value=\"false\"{{% if form.{name} == \"false\" %}} selected{{% endif %}}>{}</option>\n    \
                         </select>",
                        self.text("form.not_set"),
                        self.text("form.yes"),
                        self.text("form.no"),
                    ),
                    (FieldType::Timestamp, _) => format!(
                        "<input type=\"datetime-local\" id=\"{id}\" name=\"{name}\" value=\"{{{{ form.{name} }}}}\" step=\"1\">"
                    ),
                    (FieldType::Uuid, _) => format!(
                        "<input type=\"text\" id=\"{id}\" name=\"{name}\" value=\"{{{{ form.{name} }}}}\" spellcheck=\"false\" autocomplete=\"off\">"
                    ),
                };
                format!(
                    "  <div>\n    <label for=\"{id}\">{label}</label>\n    {input}\n    \
                     {{% if let Some(message) = errors.get(\"{name}\") %}}<p class=\"flash flash-alert\" role=\"alert\">{} {{{{ message }}}}</p>{{% endif %}}\n  </div>",
                    self.field_label(field, "layout.i18n")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -------------------------------------------------------------- tests

    fn valid_form(&self) -> String {
        self.fields()
            .map(|field| format!("{}={}", field.name, form_sample(field)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// The valid form with every timestamp half a second past its sample.
    fn subsecond_form(&self) -> String {
        self.fields()
            .map(|field| {
                let value = if field.kind == FieldType::Timestamp {
                    format!("{}.5", form_sample(field))
                } else {
                    form_sample(field)
                };
                format!("{}={value}", field.name)
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// A form with one broken value, and the text of its error.
    fn invalid_form(&self) -> (String, String) {
        let fields = &self.resource.fields;
        let with = |target: &str, value: &str| {
            let pairs: Vec<String> = fields
                .iter()
                .map(|field| {
                    if field.name == target {
                        format!("{}={value}", field.name)
                    } else {
                        format!("{}={}", field.name, form_sample(field))
                    }
                })
                .collect();
            format!("\"{}\".to_owned()", pairs.join("&"))
        };
        if let Some(field) = fields
            .iter()
            .find(|field| field.kind.is_textual() && !field.optional)
        {
            return (with(&field.name, ""), "be blank".to_owned());
        }
        for (kind, value, message) in [
            (FieldType::Integer, "abc", "must be a whole number"),
            (
                FieldType::Timestamp,
                "not-a-time",
                "must be a date and time",
            ),
            (FieldType::Uuid, "nope", "must be a UUID"),
            (FieldType::Boolean, "maybe", "must be yes, no or blank"),
        ] {
            if let Some(field) = fields
                .iter()
                .find(|field| field.kind == kind && (kind != FieldType::Boolean || field.optional))
            {
                return (with(&field.name, value), message.to_owned());
            }
        }
        if let Some(field) = fields.iter().find(|field| field.kind.is_textual()) {
            let max = if field.kind == FieldType::String {
                255
            } else {
                65_535
            };
            let pairs: Vec<String> = fields
                .iter()
                .filter(|other| other.name != field.name)
                .map(|other| format!("{}={}", other.name, form_sample(other)))
                .collect();
            let mut prefix = pairs.join("&");
            if !prefix.is_empty() {
                prefix.push('&');
            }
            return (
                format!(
                    "format!(\"{prefix}{}={{}}\", \"x\".repeat({}))",
                    field.name,
                    max + 1
                ),
                "is too long".to_owned(),
            );
        }
        // Only required checkboxes: an unexpected field is the only way to
        // send something the form never sends.
        (
            "format!(\"{VALID}&unexpected=1\")".to_owned(),
            "unknown form field".to_owned(),
        )
    }
}

const SAMPLE_UUID: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
const SAMPLE_UUID_HEX: &str = "0x67e5_5044_10b1_426f_9247_bb68_0e5f_e0c8";

fn sample_text(field: &Field) -> String {
    format!("Example {}", words(&field.name))
}

/// The URL-encoded value the test forms submit for `field`.
fn form_sample(field: &Field) -> String {
    match field.kind {
        FieldType::String | FieldType::Text => sample_text(field).replace(' ', "+"),
        FieldType::Integer => "42".to_owned(),
        FieldType::Boolean => "true".to_owned(),
        FieldType::Timestamp => "2026-01-02T03%3A04%3A05".to_owned(),
        FieldType::Uuid => SAMPLE_UUID.to_owned(),
    }
}

/// What the show page displays for the form sample of `field`.
fn shown_value(field: &Field) -> String {
    match field.kind {
        FieldType::String | FieldType::Text => sample_text(field),
        FieldType::Integer => "42".to_owned(),
        FieldType::Boolean => "Yes".to_owned(),
        FieldType::Timestamp => "2026-01-02T03:04:05Z".to_owned(),
        FieldType::Uuid => SAMPLE_UUID.to_owned(),
    }
}

fn rust_type(field: &Field) -> String {
    let base = match field.kind {
        FieldType::String | FieldType::Text => "String",
        FieldType::Integer => "i64",
        FieldType::Boolean => "bool",
        FieldType::Timestamp => "Timestamp",
        FieldType::Uuid => "Uuid",
    };
    if field.optional {
        format!("Option<{base}>")
    } else {
        base.to_owned()
    }
}

/// Joins `items` with `, `, breaking lines (with `break_with`) before an
/// item that would pass 72 characters.
fn wrap(items: &[String], break_with: &str) -> String {
    let mut out = String::new();
    let mut line = 0;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            if line + item.len() + 2 > 72 {
                out.push(',');
                out.push_str(break_with);
                line = 0;
            } else {
                out.push_str(", ");
                line += 2;
            }
        }
        out.push_str(item);
        line += item.len();
    }
    out
}

/// `Version` or `{Timestamp, Version}`.
fn use_list(names: &[&str]) -> String {
    if names.len() == 1 {
        names[0].to_owned()
    } else {
        format!("{{{}}}", names.join(", "))
    }
}
