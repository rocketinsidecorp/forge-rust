//! Parsing and validation of `forge generate scaffold` arguments.
//!
//! Everything here is pure: it reads no files. Names are checked so every
//! generated Rust identifier, SQL identifier, route and file name is valid,
//! unambiguous and within database identifier limits.

use std::fmt;

/// Longest resource, plural or field name.
pub(crate) const MAX_NAME_LENGTH: usize = 40;
/// Most fields one scaffold may declare.
pub(crate) const MAX_FIELDS: usize = 32;
/// PostgreSQL truncates identifiers longer than 63 bytes; MySQL allows 64.
const MAX_SQL_IDENTIFIER: usize = 63;

/// The column types a scaffold field may have.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldType {
    /// Single-line text of at most 255 characters.
    String,
    /// Multi-line text of at most 65,535 characters.
    Text,
    /// A signed 64-bit integer (store money as integer minor units).
    Integer,
    /// A checkbox.
    Boolean,
    /// A UTC instant with microsecond precision (`forge::core::Timestamp`).
    Timestamp,
    /// A UUID of any version (`forge::core::Uuid`).
    Uuid,
}

impl FieldType {
    pub(crate) const NAMES: &'static str = "string, text, integer, boolean, timestamp, uuid \
        (also int, bool, time and datetime)";

    /// The canonical names (D22), plus Forge-Go's `int`, `bool` and `time`
    /// and Rails' `datetime`.
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "string" => Self::String,
            "text" => Self::Text,
            "integer" | "int" => Self::Integer,
            "boolean" | "bool" => Self::Boolean,
            "timestamp" | "time" | "datetime" => Self::Timestamp,
            "uuid" => Self::Uuid,
            _ => return None,
        })
    }

    /// Whether the domain checks the value for blankness and length.
    pub(crate) fn is_textual(self) -> bool {
        matches!(self, Self::String | Self::Text)
    }
}

/// One declared field, such as `title:string` or `published_at:timestamp?`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) kind: FieldType,
    pub(crate) optional: bool,
}

/// Whether rows belong to a tenant (`--tenant-scoped`) or to everyone
/// (`--global`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Scope {
    Tenant,
    Global,
}

/// A validated scaffold request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Resource {
    /// Singular snake_case name (`line_item`).
    pub(crate) singular: String,
    /// Plural snake_case name: table, route segment and module (`line_items`).
    pub(crate) plural: String,
    pub(crate) fields: Vec<Field>,
    pub(crate) scope: Scope,
}

/// An invalid scaffold argument, with the reason shown to the user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InvalidScaffold(pub(crate) String);

impl fmt::Display for InvalidScaffold {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn invalid<T>(reason: impl Into<String>) -> Result<T, InvalidScaffold> {
    Err(InvalidScaffold(reason.into()))
}

/// Columns every generated table has, and names generated code uses next to
/// field names (entity methods, view model fields, the CSRF form field).
const RESERVED_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "version",
    "created_at",
    "updated_at",
    "csrf_token",
    "attributes",
    "restore",
    "with_version",
    "validate",
    "show_path",
    "edit_path",
    "new",
    // Trait methods the entity derives or that generated code calls on it;
    // a getter with the same name would shadow them.
    "as_ref",
    "borrow",
    "clone",
    "default",
    "eq",
    "fmt",
    "from",
    "hash",
    "into",
    "ne",
    "to_owned",
    "to_string",
];

/// Rust strict and reserved keywords plus `self`/`Self`-like names.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "abstract", "become", "box", "do", "final", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield", "union",
];

/// PascalCase names that generated modules import or that the prelude
/// defines; an entity with one of these names would not compile.
const RESERVED_TYPES: &[&str] = &[
    "AccessDenied",
    "Arc",
    "Box",
    "ComposeError",
    "Database",
    "DateTime",
    "DbError",
    "Default",
    "Error",
    "ErrorCategory",
    "FieldErrors",
    "Flash",
    "ForgeError",
    "Future",
    "GlobalAccess",
    "Id",
    "Layout",
    "Method",
    "Mutex",
    "Option",
    "Permission",
    "Request",
    "Response",
    "Result",
    "Router",
    "Row",
    "Site",
    "StatusCode",
    "String",
    "Template",
    "TenantAccess",
    "TenantContext",
    "TenantId",
    "Timestamp",
    "Utc",
    "Uuid",
    "Vec",
    "Version",
];

/// Variables and fields of generated code that are named after the
/// resource (`let posts = ...`, `PostShow::post`) and so must differ.
const RESERVED_LOCALS: &[&str] = &[
    "access",
    "after",
    "attributes",
    "back_path",
    "collection",
    "composition",
    "context",
    "csrf_field",
    "current",
    "entity",
    "errors",
    "expected",
    "fetch",
    "form",
    "layout",
    "limit",
    "member",
    "next",
    "policy",
    "pool",
    "request",
    "result",
    "row",
    "rows",
    "site",
    "state",
    "stored",
    "target",
    "tenant",
    "transaction",
    "valid",
    "values",
    "version",
];

/// Crates generated code names by path (`forge::...`, `std::...`); `crate`,
/// `self` and `super` are Rust keywords, refused already. A module of one of
/// these names would shadow the crate in the code around it.
pub(crate) const CRATE_NAMES: &[&str] = &["alloc", "core", "forge", "std"];

/// Plural names that would collide with framework routes or generated
/// modules (`/health`, `/assets`, `/streams`, `/tenants/{tenant_id}/...`,
/// `adapters::http::views`, `adapters::http::access` and
/// `adapters::http::streams`).
const RESERVED_PLURALS: &[&str] = &["access", "assets", "health", "streams", "tenants", "views"];

/// The formatting scopes of the locale files, which `l` and `number` read
/// and `forge i18n check` never reports: a scaffold's text cannot live there.
pub(crate) const FORMATTING_SCOPES: &[&str] = &["date", "time", "number"];

/// Parses `NAME field:type[?]...` plus the scope flags into a [`Resource`].
pub(crate) fn parse(
    name: &str,
    fields: &[String],
    scope: Scope,
    plural: Option<&str>,
) -> Result<Resource, InvalidScaffold> {
    let singular = resource_name(name)?;
    let plural = match plural {
        Some(plural) => {
            check_name("--plural", plural)?;
            plural.to_owned()
        }
        None => {
            if let Some(suggestion) = looks_plural(&singular) {
                return invalid(format!(
                    "resource name `{singular}` looks plural; use the singular \
                     (`{suggestion}`) or give the plural with --plural"
                ));
            }
            pluralize(&singular)
        }
    };
    for name in [&singular, &plural] {
        if CRATE_NAMES.contains(&name.as_str()) {
            return invalid(format!(
                "`{name}` is the name of a crate the generated code uses by path ({}); choose \
                 another resource name or --plural",
                CRATE_NAMES.join(", ")
            ));
        }
        if crate::wiring::SEAM_NAMES.contains(&name.as_str()) {
            return invalid(format!(
                "`{name}` names a guard, shared block or part of the generated routes seam \
                 (src/bootstrap/routes_gen.rs: {}); choose another resource name or --plural",
                crate::wiring::SEAM_NAMES.join(", ")
            ));
        }
        if RESERVED_LOCALS.contains(&name.as_str()) {
            return invalid(format!(
                "`{name}` names a variable of the generated code; choose another resource name \
                 or --plural"
            ));
        }
    }
    if FORMATTING_SCOPES.contains(&plural.as_str()) {
        return invalid(format!(
            "plural `{plural}` is a formatting scope of the locale files ({}), which `l` and \
             `number` read and `forge i18n check` skips; pass another --plural",
            FORMATTING_SCOPES.join(", ")
        ));
    }
    if RESERVED_PLURALS.contains(&plural.as_str()) {
        return invalid(format!(
            "plural `{plural}` collides with a framework route or generated module; \
             choose another name or --plural"
        ));
    }
    if plural.starts_with("forge_") || plural.starts_with("sqlx") {
        return invalid(format!(
            "table name `{plural}` is reserved for framework tables (forge_*, _sqlx_*)"
        ));
    }

    if fields.is_empty() {
        return invalid(format!(
            "declare at least one field, for example `title:string` (types: {})",
            FieldType::NAMES
        ));
    }
    if fields.len() > MAX_FIELDS {
        return invalid(format!("at most {MAX_FIELDS} fields are supported"));
    }
    let mut parsed: Vec<Field> = Vec::with_capacity(fields.len());
    for declaration in fields {
        let field = parse_field(declaration)?;
        if parsed.iter().any(|existing| existing.name == field.name) {
            return invalid(format!("field `{}` is declared more than once", field.name));
        }
        parsed.push(field);
    }

    let resource = Resource {
        singular,
        plural,
        fields: parsed,
        scope,
    };
    check_identifier_lengths(&resource)?;
    Ok(resource)
}

/// Parses the `field:type[?]` arguments of `forge generate migration`, for
/// the columns of `table`.
pub(crate) fn parse_columns(table: &str, fields: &[String]) -> Result<Vec<Field>, InvalidScaffold> {
    check_name("table name", table)?;
    if table.starts_with("forge_") || table.starts_with("sqlite_") || table.starts_with("_sqlx") {
        return invalid(format!(
            "table `{table}` uses a prefix reserved for Forge, SQLx and SQLite internals"
        ));
    }
    if fields.len() > MAX_FIELDS {
        return invalid(format!("at most {MAX_FIELDS} fields are supported"));
    }
    let mut parsed: Vec<Field> = Vec::with_capacity(fields.len());
    for declaration in fields {
        let field = parse_field(declaration)?;
        if parsed.iter().any(|existing| existing.name == field.name) {
            return invalid(format!("field `{}` is declared more than once", field.name));
        }
        parsed.push(field);
    }
    check_identifier_lengths(&Resource {
        singular: table.to_owned(),
        plural: table.to_owned(),
        fields: parsed.clone(),
        scope: Scope::Global,
    })?;
    Ok(parsed)
}

/// Accepts `snake_case` or `PascalCase` and returns `snake_case`.
fn resource_name(name: &str) -> Result<String, InvalidScaffold> {
    let snake = if name.starts_with(|c: char| c.is_ascii_uppercase()) {
        if !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return invalid(format!(
                "resource name `{name}` must be snake_case (`line_item`) or PascalCase (`LineItem`)"
            ));
        }
        let mut snake = String::with_capacity(name.len() + 4);
        for (index, character) in name.chars().enumerate() {
            if character.is_ascii_uppercase() && index > 0 {
                snake.push('_');
            }
            snake.push(character.to_ascii_lowercase());
        }
        snake
    } else {
        name.to_owned()
    };
    check_name("resource name", &snake)?;
    let pascal = pascal_case(&snake);
    if RESERVED_TYPES.contains(&pascal.as_str()) {
        return invalid(format!(
            "resource name `{snake}` would define the type `{pascal}`, which generated code \
             already uses; choose another name"
        ));
    }
    Ok(snake)
}

/// `[a-z][a-z0-9]*(_[a-z0-9]+)*`, bounded, and not a keyword.
fn check_name(what: &str, name: &str) -> Result<(), InvalidScaffold> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LENGTH
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__");
    if !valid {
        return invalid(format!(
            "{what} `{name}` must be snake_case: lowercase ASCII letters, digits and single \
             underscores, starting with a letter, at most {MAX_NAME_LENGTH} characters"
        ));
    }
    if RUST_KEYWORDS.contains(&name) {
        return invalid(format!("{what} `{name}` is a Rust keyword"));
    }
    // SQL keywords (`order`, `group`) are fine: generated SQL quotes every
    // identifier, as Forge-Go's does.
    Ok(())
}

fn parse_field(declaration: &str) -> Result<Field, InvalidScaffold> {
    let Some((name, kind)) = declaration.split_once(':') else {
        return invalid(format!(
            "field `{declaration}` must be written name:type (types: {}; append `?` for an \
             optional field)",
            FieldType::NAMES
        ));
    };
    check_name("field name", name)?;
    if RESERVED_FIELDS.contains(&name) {
        return invalid(format!(
            "field name `{name}` is reserved: every table has id, version, created_at and \
             updated_at (and tenant_id when tenant-scoped), and generated code uses {}",
            RESERVED_FIELDS[5..].join(", ")
        ));
    }
    let (kind, optional) = match kind.strip_suffix('?') {
        Some(kind) => (kind, true),
        None => (kind, false),
    };
    let Some(kind) = FieldType::parse(kind) else {
        return invalid(format!(
            "field `{name}` has unsupported type `{kind}` (types: {}; store money as integer \
             minor units, there is no float type)",
            FieldType::NAMES
        ));
    };
    Ok(Field {
        name: name.to_owned(),
        kind,
        optional,
    })
}

/// Keeps generated constraint and index names within database limits.
fn check_identifier_lengths(resource: &Resource) -> Result<(), InvalidScaffold> {
    let mut longest = resource.plural.len() + "_version_positive".len();
    longest = longest.max(resource.plural.len() + "_tenant_isolation".len());
    for field in &resource.fields {
        longest = longest.max(resource.plural.len() + 1 + field.name.len() + "_boolean".len());
    }
    if longest > MAX_SQL_IDENTIFIER {
        return invalid(format!(
            "the plural and field names are too long for database constraint names \
             ({longest} > {MAX_SQL_IDENTIFIER} characters); shorten them"
        ));
    }
    Ok(())
}

/// English plural rules: consonant + y becomes ies; s, x, z, ch and sh take
/// es; everything else takes s. Irregular plurals need `--plural`.
pub(crate) fn pluralize(name: &str) -> String {
    let bytes = name.as_bytes();
    if name.ends_with('y')
        && bytes.len() > 1
        && !matches!(bytes[bytes.len() - 2], b'a' | b'e' | b'i' | b'o' | b'u')
    {
        return format!("{}ies", &name[..name.len() - 1]);
    }
    if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return format!("{name}es");
    }
    format!("{name}s")
}

/// The singular a plural-looking name probably meant, or `None`. Singular
/// words that end in s (status, address, bus, analysis, photos is plural)
/// pass.
fn looks_plural(name: &str) -> Option<String> {
    let last_word = name.rsplit('_').next().unwrap_or(name);
    if last_word.len() > 4 && last_word.ends_with("ies") {
        return Some(format!("{}y", &name[..name.len() - 3]));
    }
    if !last_word.ends_with('s')
        || last_word.len() < 3
        || ["ss", "us", "is", "os"]
            .iter()
            .any(|suffix| last_word.ends_with(suffix))
    {
        return None;
    }
    if ["ses", "xes", "zes", "ches", "shes"]
        .iter()
        .any(|suffix| last_word.ends_with(suffix))
    {
        return Some(name[..name.len() - 2].to_owned());
    }
    Some(name[..name.len() - 1].to_owned())
}

/// `line_item` becomes `LineItem`.
pub(crate) fn pascal_case(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut characters = word.chars();
            characters
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
                .unwrap_or_default()
        })
        .collect()
}

/// `line_item` becomes `line item`.
pub(crate) fn words(name: &str) -> String {
    name.replace('_', " ")
}

/// `line_item` becomes `Line item`.
pub(crate) fn sentence_case(name: &str) -> String {
    let words = words(name);
    let mut characters = words.chars();
    characters
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(declarations: &[&str]) -> Vec<String> {
        declarations
            .iter()
            .map(|value| (*value).to_owned())
            .collect()
    }

    fn ok(name: &str, declarations: &[&str], plural: Option<&str>) -> Resource {
        match parse(name, &fields(declarations), Scope::Global, plural) {
            Ok(resource) => resource,
            Err(error) => panic!("{name} {declarations:?}: {error}"),
        }
    }

    fn reason(name: &str, declarations: &[&str], plural: Option<&str>) -> String {
        match parse(name, &fields(declarations), Scope::Global, plural) {
            Ok(resource) => panic!("accepted {resource:?}"),
            Err(error) => error.0,
        }
    }

    #[test]
    fn parses_names_types_and_optional_fields() {
        let resource = ok(
            "LineItem",
            &[
                "title:string",
                "body:text?",
                "quantity:integer",
                "paid:boolean",
                "due_at:timestamp?",
                "order_ref:uuid",
            ],
            None,
        );
        assert_eq!(resource.singular, "line_item");
        assert_eq!(resource.plural, "line_items");
        assert_eq!(resource.fields.len(), 6);
        assert_eq!(resource.fields[1].kind, FieldType::Text);
        assert!(resource.fields[1].optional);
        assert!(!resource.fields[0].optional);
        assert_eq!(resource.fields[5].kind, FieldType::Uuid);
    }

    #[test]
    fn pluralizes_and_detects_plural_names() {
        for (singular, plural) in [
            ("post", "posts"),
            ("category", "categories"),
            ("day", "days"),
            ("box_item", "box_items"),
            ("status", "statuses"),
            ("match_result", "match_results"),
            ("church", "churches"),
            ("address", "addresses"),
            ("analysis", "analyses"),
        ] {
            if singular == "analysis" {
                assert_eq!(ok(singular, &["a:string"], Some(plural)).plural, plural);
                continue;
            }
            assert_eq!(
                ok(singular, &["a:string"], None).plural,
                plural,
                "{singular}"
            );
        }
        assert!(reason("posts", &["a:string"], None).contains("`post`"));
        assert!(reason("categories", &["a:string"], None).contains("`category`"));
        assert_eq!(ok("news", &["a:string"], Some("news")).plural, "news");
    }

    #[test]
    fn rejects_invalid_and_reserved_names() {
        for name in [
            "",
            "1post",
            "post-item",
            "post__item",
            "post_",
            "café",
            "type",
            "tenant",
            "site",
            "Request",
            "Post_Item",
            "a_very_long_resource_name_that_exceeds_the_limit",
        ] {
            let _ = reason(name, &["a:string"], None);
        }
        for plural in [
            "health",
            "assets",
            "views",
            "access",
            "streams",
            "tenants",
            "forge_posts",
        ] {
            let _ = reason("thing", &["a:string"], Some(plural));
        }
    }

    /// The seam's own names (its guards, shared blocks, function, module and
    /// parameters) are never a resource or plural: `--plural protect` would
    /// shadow the guard of the routes (Forge-Go's review found such a
    /// collision turning a guard off).
    #[test]
    fn rejects_the_names_of_the_routes_seam() {
        for name in crate::wiring::SEAM_NAMES {
            let as_name = parse(name, &["a:string".to_owned()], Scope::Global, None)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(!as_name.is_empty(), "{name} is refused as a resource name");
            let as_plural = parse("thing", &["a:string".to_owned()], Scope::Global, Some(name))
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(as_plural.contains("routes seam"), "{name}: {as_plural}");
        }
    }

    /// Names of crates the generated code uses by path are refused as a
    /// resource or a plural, as are `crate`, `self` and `super` (keywords).
    #[test]
    fn rejects_the_names_of_crates_used_by_path() {
        for name in ["alloc", "core", "forge", "std"] {
            let as_plural = parse("thing", &["a:string".to_owned()], Scope::Global, Some(name))
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(
                as_plural.contains("crate the generated code uses"),
                "{name}: {as_plural}"
            );
            let as_name = parse(name, &["a:string".to_owned()], Scope::Global, None)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(
                as_name.contains("crate the generated code uses"),
                "{name}: {as_name}"
            );
        }
        for keyword in ["crate", "self", "super"] {
            assert!(
                parse(keyword, &["a:string".to_owned()], Scope::Global, None).is_err(),
                "{keyword}"
            );
        }
    }

    /// Generated SQL quotes every identifier, so SQL keywords are names like
    /// any other, as in Forge-Go (`forge generate resource order` makes the
    /// table `orders`); field types take Forge-Go's and Rails' spellings.
    #[test]
    fn accepts_sql_keywords_and_type_aliases() {
        let order = parse(
            "order",
            &[
                "select:string".to_owned(),
                "group:int".to_owned(),
                "public:bool?".to_owned(),
                "at:time".to_owned(),
                "on:datetime?".to_owned(),
            ],
            Scope::Global,
            None,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(order.plural, "orders");
        let kinds: Vec<FieldType> = order.fields.iter().map(|field| field.kind).collect();
        assert_eq!(
            kinds,
            [
                FieldType::String,
                FieldType::Integer,
                FieldType::Boolean,
                FieldType::Timestamp,
                FieldType::Timestamp
            ]
        );
    }

    #[test]
    fn rejects_invalid_fields() {
        for declarations in [
            &[][..],
            &["title"][..],
            &["title:float"][..],
            &["title:"][..],
            &["id:uuid"][..],
            &["tenant_id:uuid"][..],
            &["version:integer"][..],
            &["created_at:timestamp"][..],
            &["csrf_token:string"][..],
            &["match:string"][..],
            &["Title:string"][..],
            &["title:string", "title:text"][..],
            &["title:string??"][..],
        ] {
            let _ = reason("post", declarations, None);
        }
        let many: Vec<String> = (0..=MAX_FIELDS)
            .map(|index| format!("f{index}:integer"))
            .collect();
        assert!(parse("post", &many, Scope::Global, None).is_err());
    }

    #[test]
    fn keeps_constraint_names_within_database_limits() {
        let long = "a".repeat(30);
        assert!(
            reason(&long, &[&format!("{}:boolean", "b".repeat(30))], None).contains("too long")
        );
    }

    #[test]
    fn converts_case() {
        assert_eq!(pascal_case("line_item"), "LineItem");
        assert_eq!(sentence_case("line_item"), "Line item");
        assert_eq!(words("due_at"), "due at");
    }
}
