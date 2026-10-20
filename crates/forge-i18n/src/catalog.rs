//! Translation files: JSON under `config/locales`, Rails layout (ADR 0037).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::path::Path;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::LoadError;
use crate::locale::{canonical_locale, truncate};
use crate::plural::Category;

/// The largest translation file read, in bytes.
pub const MAX_FILE_BYTES: usize = 1 << 20;
/// The most translation files one catalog may have.
pub const MAX_FILES: usize = 512;
/// The deepest nesting of objects (and lists) below the file's root.
pub const MAX_DEPTH: usize = 16;
/// The most leaf keys one locale may have.
pub const MAX_KEYS_PER_LOCALE: usize = 50_000;
/// The longest dotted key, in bytes.
pub const MAX_KEY_LENGTH: usize = 256;

/// One value of a locale's tree.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    Text(String),
    Number(f64),
    Bool(bool),
    /// Strings and `null`, such as `date.month_names` (which starts with
    /// `null`, as in Rails).
    List(Vec<Option<String>>),
    Map(Tree),
}

pub(crate) type Tree = BTreeMap<String, Node>;

/// `[A-Za-z0-9_-]+`: one segment of a dotted key.
pub(crate) fn is_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// `[a-z0-9_-]+(\.[a-z0-9_-]+)*`: the part of a file name before the locale.
fn is_scope(scope: &str) -> bool {
    scope.split('.').all(|part| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            })
    })
}

/// The locale of a translation file name: `LOCALE.json` or
/// `SCOPE.LOCALE.json` (such as `posts.en.json`), where LOCALE is spelled
/// canonically (see [`canonical_locale`]) and SCOPE uses lowercase letters,
/// digits, `_`, `-` and `.`.
///
/// ```
/// assert_eq!(forge_i18n::file_locale("posts.pt-BR.json").as_deref(), Ok("pt-BR"));
/// assert!(forge_i18n::file_locale("pt-br.json").is_err());
/// ```
pub fn file_locale(name: &str) -> Result<String, LoadError> {
    if !name.ends_with(".json") || name.contains(['/', '\\']) {
        return Err(LoadError::new(format!(
            "{} is not a translation file name (LOCALE.json)",
            truncate(name)
        )));
    }
    locale_of_file(name)
}

fn locale_of_file(name: &str) -> Result<String, LoadError> {
    let base = name.strip_suffix(".json").unwrap_or(name);
    let (scope, locale) = match base.rfind('.') {
        Some(cut) => (&base[..cut], &base[cut + 1..]),
        None => ("", base),
    };
    let canonical = canonical_locale(locale).map_err(|error| {
        LoadError::new(format!(
            "{name}: name translation files LOCALE.json or SCOPE.LOCALE.json, such as en.json or \
             posts.pt-BR.json: {}",
            error.message()
        ))
    })?;
    if canonical != locale {
        let prefix = name.strip_suffix(&format!("{locale}.json")).unwrap_or("");
        return Err(LoadError::new(format!(
            "{name}: spell the locale {canonical} (rename the file to {prefix}{canonical}.json)"
        )));
    }
    if !scope.is_empty() && !is_scope(scope) {
        return Err(LoadError::new(format!(
            "{name}: the part before the locale must use lowercase letters, digits, '_', '-' and '.'"
        )));
    }
    Ok(canonical)
}

/// The merged trees of every locale.
pub(crate) type Catalog = HashMap<String, Tree>;

/// Reads the `*.json` files of `dir` (not its subdirectories; dot files and
/// other files are ignored) into named contents.
pub(crate) fn read_dir(dir: &Path) -> Result<Vec<(String, String)>, LoadError> {
    let entries = fs::read_dir(dir).map_err(|error| {
        LoadError::new(format!("read translations in {}: {error}", dir.display()))
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            LoadError::new(format!("read translations in {}: {error}", dir.display()))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| LoadError::new(format!("{name}: {error}")))?;
        if file_type.is_dir() {
            continue;
        }
        if !file_type.is_file() {
            // A link could name a file outside the directory: translation
            // files are code and are read only from where they are reviewed.
            return Err(LoadError::new(format!(
                "{name} is not a regular file (symbolic links are not read)"
            )));
        }
        if files.len() >= MAX_FILES {
            return Err(LoadError::new(format!(
                "more than {MAX_FILES} translation files"
            )));
        }
        let file = fs::File::open(entry.path())
            .map_err(|error| LoadError::new(format!("open {name}: {error}")))?;
        let mut data = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|error| LoadError::new(format!("read {name}: {error}")))?;
        if data.len() > MAX_FILE_BYTES {
            return Err(LoadError::new(format!(
                "{name} is larger than {MAX_FILE_BYTES} bytes"
            )));
        }
        let text =
            String::from_utf8(data).map_err(|_| LoadError::new(format!("{name} is not UTF-8")))?;
        files.push((name, text));
    }
    Ok(files)
}

/// Parses and merges translation files given as (file name, contents).
/// Files that are not `*.json` or start with `.` are ignored, like the
/// directory reader does.
pub(crate) fn read_catalog<N, C>(files: &[(N, C)]) -> Result<Catalog, LoadError>
where
    N: AsRef<str>,
    C: AsRef<str>,
{
    let mut sorted: Vec<(&str, &str)> = files
        .iter()
        .map(|(name, content)| (name.as_ref(), content.as_ref()))
        .filter(|(name, _)| !name.starts_with('.') && name.ends_with(".json"))
        .collect();
    sorted.sort_by(|left, right| left.0.cmp(right.0));
    if sorted.len() > MAX_FILES {
        return Err(LoadError::new(format!(
            "more than {MAX_FILES} translation files"
        )));
    }
    let mut catalog = Catalog::new();
    // locale -> leaf key -> file
    let mut sources: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (name, content) in sorted {
        if name.contains(['/', '\\']) {
            return Err(LoadError::new(format!(
                "{} is not a translation file name (LOCALE.json)",
                truncate(name)
            )));
        }
        let locale = locale_of_file(name)?;
        if content.len() > MAX_FILE_BYTES {
            return Err(LoadError::new(format!(
                "{name} is larger than {MAX_FILE_BYTES} bytes"
            )));
        }
        let parsed = parse_file(name, &locale, content)?;
        let tree = catalog.entry(locale.clone()).or_default();
        let leaves = sources.entry(locale.clone()).or_default();
        merge(tree, parsed, "", name, leaves)?;
        if leaves.len() > MAX_KEYS_PER_LOCALE {
            return Err(LoadError::new(format!(
                "locale {locale} has more than {MAX_KEYS_PER_LOCALE} keys"
            )));
        }
    }
    if catalog.is_empty() {
        return Err(LoadError::new(
            "no translation files found; expected LOCALE.json files such as en.json",
        ));
    }
    Ok(catalog)
}

/// A JSON value as written, with object keys in file order.
#[derive(Debug)]
enum Raw {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    List(Vec<Raw>),
    Object(Vec<(String, Raw)>),
}

/// Deserializes one value, refusing duplicate object keys (serde_json
/// keeps the last one, which the catalog format forbids) and nesting
/// deeper than [`MAX_DEPTH`].
struct RawSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for RawSeed {
    type Value = Raw;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Raw, D::Error> {
        deserializer.deserialize_any(RawVisitor { depth: self.depth })
    }
}

struct RawVisitor {
    depth: usize,
}

impl RawVisitor {
    fn nested<E: de::Error>(&self) -> Result<usize, E> {
        if self.depth > MAX_DEPTH {
            return Err(E::custom(format!("nesting deeper than {MAX_DEPTH} levels")));
        }
        Ok(self.depth + 1)
    }
}

impl<'de> Visitor<'de> for RawVisitor {
    type Value = Raw;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Raw, E> {
        Ok(Raw::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Raw, E> {
        Ok(Raw::Bool(value))
    }

    #[allow(clippy::cast_precision_loss)] // formatting data, such as a precision
    fn visit_i64<E>(self, value: i64) -> Result<Raw, E> {
        Ok(Raw::Number(value as f64))
    }

    #[allow(clippy::cast_precision_loss)]
    fn visit_u64<E>(self, value: u64) -> Result<Raw, E> {
        Ok(Raw::Number(value as f64))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Raw, E> {
        Ok(Raw::Number(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Raw, E> {
        Ok(Raw::Text(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Raw, E> {
        Ok(Raw::Text(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Raw, A::Error> {
        let depth = self.nested()?;
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element_seed(RawSeed { depth })? {
            items.push(item);
        }
        Ok(Raw::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
        let depth = self.nested()?;
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate key {:?}",
                    truncate(&key)
                )));
            }
            let value = map.next_value_seed(RawSeed { depth })?;
            entries.push((key, value));
        }
        Ok(Raw::Object(entries))
    }
}

/// Decodes one file. Its only top-level key is the locale, like a Rails
/// YAML file: `{"en": {"users": {"welcome": "Welcome, %{name}!"}}}`.
fn parse_file(name: &str, locale: &str, content: &str) -> Result<Tree, LoadError> {
    let mut deserializer = serde_json::Deserializer::from_str(content);
    let root = RawSeed { depth: 0 }
        .deserialize(&mut deserializer)
        .map_err(|error| {
            if error.classify() == serde_json::error::Category::Data {
                LoadError::new(format!("{name}: {error}"))
            } else {
                LoadError::new(format!("{name}: invalid JSON: {error}"))
            }
        })?;
    if deserializer.end().is_err() {
        return Err(LoadError::new(format!(
            "{name}: unexpected data after the top-level object"
        )));
    }
    let Raw::Object(mut entries) = root else {
        return Err(only_locale_key(name, locale));
    };
    if entries.len() != 1 {
        return Err(only_locale_key(name, locale));
    }
    let (key, value) = entries.remove(0);
    let (true, Raw::Object(translations)) = (key == locale, value) else {
        return Err(LoadError::new(format!(
            "{name}: the only top-level key must be {locale:?} (the locale in the file name) and \
             hold an object"
        )));
    };
    to_tree(translations, "").map_err(|message| LoadError::new(format!("{name}: {message}")))
}

fn only_locale_key(name: &str, locale: &str) -> LoadError {
    LoadError::new(format!(
        "{name}: the file must be one object whose only key is the locale, {{{locale:?}: {{...}}}}"
    ))
}

/// Validates keys and values below a locale and builds its tree.
fn to_tree(entries: Vec<(String, Raw)>, prefix: &str) -> Result<Tree, String> {
    let mut tree = Tree::new();
    let all_plural = !entries.is_empty()
        && entries
            .iter()
            .all(|(key, _)| Category::from_key(key).is_some());
    for (key, value) in entries {
        let path = join_key(prefix, &key);
        if !is_segment(&key) {
            return Err(format!(
                "key {:?}: use letters, digits, '_' and '-' (dots separate levels)",
                truncate(&path)
            ));
        }
        if path.len() > MAX_KEY_LENGTH {
            return Err(format!(
                "key {:?} is longer than {MAX_KEY_LENGTH} bytes",
                truncate(&path)
            ));
        }
        let node = match value {
            Raw::Null => return Err(format!("key {path:?} is null")),
            Raw::Bool(value) => Node::Bool(value),
            Raw::Number(value) => Node::Number(value),
            Raw::Text(value) => Node::Text(value),
            Raw::List(items) => {
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Raw::Text(text) => list.push(Some(text)),
                        Raw::Null => list.push(None),
                        _ => {
                            return Err(format!(
                                "key {path:?}: lists may hold only strings and null"
                            ));
                        }
                    }
                }
                Node::List(list)
            }
            Raw::Object(children) => Node::Map(to_tree(children, &path)?),
        };
        tree.insert(key, node);
    }
    if all_plural {
        if !tree.contains_key(Category::Other.as_str()) {
            return Err(format!(
                "key {prefix:?}: plural forms need an \"other\" form"
            ));
        }
        if let Some((key, _)) = tree
            .iter()
            .find(|(_, value)| !matches!(value, Node::Text(_)))
        {
            return Err(format!(
                "key {prefix:?}: plural form {key:?} must be a string"
            ));
        }
    }
    Ok(tree)
}

/// Whether an object holds plural forms: an `other` form and only
/// category keys.
pub(crate) fn is_plural(tree: &Tree) -> bool {
    tree.contains_key(Category::Other.as_str())
        && tree.keys().all(|key| Category::from_key(key).is_some())
}

/// Adds `source` to `target`. A leaf defined by two files is an error
/// naming both.
fn merge(
    target: &mut Tree,
    source: Tree,
    prefix: &str,
    file: &str,
    sources: &mut HashMap<String, String>,
) -> Result<(), LoadError> {
    for (key, value) in source {
        let path = join_key(prefix, &key);
        match target.get_mut(&key) {
            None => {
                record_leaves(&value, &path, file, sources);
                target.insert(key, value);
            }
            Some(Node::Map(existing)) if !is_plural(existing) => match value {
                Node::Map(incoming) if !is_plural(&incoming) => {
                    merge(existing, incoming, &path, file, sources)?;
                }
                _ => return Err(conflict(&path, file, sources)),
            },
            Some(_) => return Err(conflict(&path, file, sources)),
        }
    }
    Ok(())
}

fn conflict(path: &str, file: &str, sources: &HashMap<String, String>) -> LoadError {
    LoadError::new(format!(
        "key {path:?} is defined in both {} and {file}",
        first_source(sources, path)
    ))
}

fn record_leaves(node: &Node, path: &str, file: &str, sources: &mut HashMap<String, String>) {
    if let Node::Map(tree) = node
        && !is_plural(tree)
    {
        for (key, child) in tree {
            record_leaves(child, &join_key(path, key), file, sources);
        }
        return;
    }
    sources.insert(path.to_owned(), file.to_owned());
}

fn first_source<'a>(sources: &'a HashMap<String, String>, path: &str) -> &'a str {
    if let Some(file) = sources.get(path) {
        return file;
    }
    let mut candidates: Vec<(&String, &String)> = sources
        .iter()
        .filter(|(key, _)| {
            key.starts_with(&format!("{path}.")) || path.starts_with(&format!("{key}."))
        })
        .collect();
    candidates.sort();
    candidates
        .first()
        .map_or("another file", |(_, file)| file.as_str())
}

/// The dotted keys of a tree's leaves, sorted. Plural forms are one leaf.
pub(crate) fn leaf_keys(tree: &Tree) -> Vec<String> {
    let mut keys = Vec::new();
    collect_leaf_keys(tree, "", &mut keys);
    keys.sort();
    keys
}

fn collect_leaf_keys(tree: &Tree, prefix: &str, keys: &mut Vec<String>) {
    for (key, value) in tree {
        let path = join_key(prefix, key);
        match value {
            Node::Map(child) if !is_plural(child) => collect_leaf_keys(child, &path, keys),
            _ => keys.push(path),
        }
    }
}

/// Walks a dotted key.
pub(crate) fn find<'a>(tree: &'a Tree, key: &str) -> Option<&'a Node> {
    let mut segments = key.split('.');
    let mut current = tree.get(segments.next()?)?;
    for segment in segments {
        let Node::Map(map) = current else {
            return None;
        };
        current = map.get(segment)?;
    }
    Some(current)
}

pub(crate) fn join_key(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(files: &[(&str, &str)]) -> Result<Catalog, String> {
        read_catalog(files).map_err(|error| error.to_string())
    }

    #[test]
    fn file_names_carry_a_canonical_locale() {
        for (name, want) in [
            ("en.json", "en"),
            ("posts.pt-BR.json", "pt-BR"),
            ("admin.users.zh-Hant-TW.json", "zh-Hant-TW"),
        ] {
            assert_eq!(file_locale(name).ok().as_deref(), Some(want), "{name}");
        }
        for name in [
            "en.yml",
            "../en.json",
            "a/en.json",
            "a\\en.json",
            "pt-br.json",
            "Posts.en.json",
            ".json",
            "x..en.json",
        ] {
            assert!(file_locale(name).is_err(), "{name}");
        }
        let error = file_locale("posts.pt-br.json").err().map(|e| e.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("rename the file to posts.pt-BR.json")),
            "{error:?}"
        );
    }

    #[test]
    fn unsafe_or_ambiguous_files_are_refused() {
        let large = format!("{{\"en\":{{\"a\":\"{}\"}}}}", "x".repeat(MAX_FILE_BYTES));
        let deep = format!(
            "{{\"en\":{}\"x\"{}",
            "{\"a\":".repeat(MAX_DEPTH + 1),
            "}".repeat(MAX_DEPTH + 2)
        );
        let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
            (
                vec![("en.json", r#"{"en":{}}"#), ("%2e%2e%2fetc.json", "{}")],
                "invalid locale",
            ),
            (vec![("pt_BR.json", r#"{"pt_BR":{}}"#)], "invalid locale"),
            (
                vec![("pt-br.json", r#"{"pt-BR":{}}"#)],
                "rename the file to pt-BR.json",
            ),
            (
                vec![("en.json", r#"{"pt":{"a":"b"}}"#)],
                r#"only top-level key must be "en""#,
            ),
            (
                vec![("en.json", r#"{"en":{},"pt":{}}"#)],
                "only key is the locale",
            ),
            (
                vec![("en.json", r#"{"en":{"a":"1","a":"2"}}"#)],
                r#"duplicate key "a""#,
            ),
            (
                vec![("en.json", r#"{"en":{"a":"1"},"en":{"b":"2"}}"#)],
                r#"duplicate key "en""#,
            ),
            (
                vec![("en.json", r#"{"en":{"a.b":"1"}}"#)],
                "dots separate levels",
            ),
            (vec![("en.json", r#"{"en":{"a":null}}"#)], "is null"),
            (
                vec![("en.json", r#"{"en":{"a":{"one":"x"}}}"#)],
                r#"need an "other" form"#,
            ),
            (
                vec![("en.json", r#"{"en":{"a":{"one":1,"other":"x"}}}"#)],
                "must be a string",
            ),
            (
                vec![("en.json", r#"{"en":{"a":[1]}}"#)],
                "lists may hold only strings and null",
            ),
            (vec![("en.json", r#"{"en":{"a":}}"#)], "invalid JSON"),
            (
                vec![("en.json", r#"{"en":{}} {}"#)],
                "after the top-level object",
            ),
            (
                vec![
                    ("en.json", r#"{"en":{"a":{"b":"1"}}}"#),
                    ("x.en.json", r#"{"en":{"a":{"b":"2"}}}"#),
                ],
                r#""a.b" is defined in both en.json and x.en.json"#,
            ),
            (
                vec![
                    ("en.json", r#"{"en":{"a":{"b":"1"}}}"#),
                    ("x.en.json", r#"{"en":{"a":"2"}}"#),
                ],
                r#""a" is defined in both en.json and x.en.json"#,
            ),
            (vec![("en.json", large.as_str())], "larger than"),
            (vec![("en.json", deep.as_str())], "nesting deeper"),
            (vec![("README.md", "x")], "no translation files"),
            (
                vec![
                    ("en.json", r#"{"en":{}}"#),
                    ("Posts.en.json", r#"{"en":{}}"#),
                ],
                "part before the locale",
            ),
        ];
        for (files, want) in cases {
            let error = load(&files).err().unwrap_or_default();
            assert!(error.contains(want), "{files:?}: {error:?} lacks {want:?}");
        }
    }

    #[test]
    fn scoped_files_merge_and_plural_forms_are_one_key() {
        let catalog = load(&[
            (
                "en.json",
                r#"{"en":{"a":{"b":"1"},"inbox":{"one":"x","other":"y"}}}"#,
            ),
            (
                "posts.en.json",
                r#"{"en":{"a":{"c":"2"},"posts":{"title":"Posts"}}}"#,
            ),
            ("README.md", "ignored"),
        ])
        .unwrap_or_default();
        let keys = catalog.get("en").map(leaf_keys).unwrap_or_default();
        assert_eq!(keys, ["a.b", "a.c", "inbox", "posts.title"]);
    }

    #[test]
    fn directories_are_read_flat_and_links_are_refused() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("en.json"), r#"{"en":{"a":"b"}}"#)?;
        fs::write(dir.path().join("README.md"), "notes")?;
        fs::write(dir.path().join(".hidden.json"), "not json")?;
        fs::create_dir(dir.path().join("nested"))?;
        fs::write(dir.path().join("nested/x.json"), "not read")?;
        let files = read_dir(dir.path())?;
        assert_eq!(files.len(), 1);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hostname", dir.path().join("pt.json"))?;
            let error = read_dir(dir.path()).err().map(|e| e.to_string());
            assert!(
                error
                    .as_deref()
                    .is_some_and(|e| e.contains("not a regular file")),
                "{error:?}"
            );
        }
        Ok(())
    }
}
