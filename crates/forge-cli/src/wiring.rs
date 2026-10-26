//! What the generators register in files that already exist (D21): no
//! wiring is left for the developer to paste.
//!
//! - Every module a generator writes is declared (`pub mod NAME;`) in the
//!   `mod.rs` of its directory, in sorted order; `forge destroy` takes the
//!   declaration back out.
//! - The routes of a scaffold, resource or controller are mounted in
//!   [`ROUTES_FILE`], whose `generated_routes` function `compose` in
//!   `src/bootstrap/mod.rs` calls once (`forge new` writes both). The
//!   generators own that file and rewrite it whole: a list of blocks, each
//!   opened by a `// forge:KIND NAME` comment, formatted with the
//!   application's rustfmt. A `// forge:checksum` line records what they
//!   wrote, so a hand edit is refused rather than overwritten. Forge-Go's
//!   `app/bootstrap/routes_gen.go` works the same way.
//!
//! Every edit is idempotent: a module already declared, or a block already
//! wired with the same code, is left alone, so running a generator's wiring
//! again changes nothing. An application whose `compose` does not call the
//! seam (created before it existed) keeps the printed wiring.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::CliError;
use crate::generators::refuse_symlinks;
use crate::scaffold::rustfmt;

/// The application-relative path of the routes seam.
pub(crate) const ROUTES_FILE: &str = "src/bootstrap/routes_gen.rs";

/// The function [`ROUTES_FILE`] declares and `compose` calls.
pub(crate) const ROUTES_FUNCTION: &str = "generated_routes";

/// The composition root that calls the seam.
const BOOTSTRAP_FILE: &str = "src/bootstrap/mod.rs";

const CHECKSUM_PREFIX: &str = "// forge:checksum ";
const CHECKSUM_PLACEHOLDER: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
const MARKER_PREFIX: &str = "    // forge:";

const ROUTES_HEADER: &str = "\
//! Routes the generators mount. `forge generate scaffold`, `forge generate
//! resource` and `forge generate controller` write this file, and `forge
//! destroy` takes their blocks back out; each block starts with a `forge:`
//! comment. Do not edit it: the generators refuse to change it once its
//! checksum no longer matches. Mount your own routes in `compose`
//! (src/bootstrap/mod.rs), which calls this function once.
";

/// One block of [`ROUTES_FILE`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Block {
    /// `scaffold`, `resource` or `controller` for a generator's own block;
    /// `guard` (an access policy) or `streams` (the live-update transport)
    /// for a shared one.
    kind: String,
    /// The plural (resource, scaffold) or name (controller) of a generator's
    /// block, the variable a shared block declares.
    name: String,
    /// The statements, indented by four spaces, each line ending in `\n`.
    body: String,
}

impl Block {
    /// A generator's block: `statements` run in their own scope, so the
    /// variables of two blocks never clash.
    pub(crate) fn scoped(kind: &str, name: &str, statements: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            name: name.to_owned(),
            body: format!("    {{\n{statements}    }}\n"),
        }
    }

    /// A shared block, declared once at the top of the function for every
    /// block that uses the variable `name`.
    pub(crate) fn shared(kind: &str, name: &str, statements: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            name: name.to_owned(),
            body: statements.to_owned(),
        }
    }

    fn id(&self) -> String {
        format!("{} {}", self.kind, self.name)
    }

    fn is_shared(&self) -> bool {
        SHARED_KINDS.contains(&self.kind.as_str())
    }
}

/// The kinds of the shared blocks, in the order the seam declares them:
/// the guards, then the stream the pages publish to.
const SHARED_KINDS: &[&str] = &["guard", "streams"];

/// What one generator registers.
#[derive(Clone, Debug, Default)]
pub(crate) struct Registration {
    /// `(mod.rs file, module)` pairs, in the order to report them.
    pub(crate) modules: Vec<(String, String)>,
    /// The generator's routes, when it mounts any.
    pub(crate) routes: Option<Block>,
    /// The shared blocks the routes may need.
    pub(crate) shared: Vec<Block>,
}

/// A planned change to an existing file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Edit {
    pub(crate) path: String,
    /// `declare`, `undeclare`, `wire`, `unwire` or `identical`.
    pub(crate) verb: &'static str,
    /// What changes, for the report (`(pub mod products;)`).
    pub(crate) detail: String,
    /// The new content; `None` when nothing changes.
    pub(crate) content: Option<String>,
}

/// Whether the application in `root` mounts [`ROUTES_FILE`]: its `compose`
/// calls the seam. Applications created before the seam print their wiring.
pub(crate) fn has_seam(root: &Path) -> bool {
    fs::read_to_string(root.join(BOOTSTRAP_FILE)).is_ok_and(|source| {
        source.contains(&format!("routes_gen::{ROUTES_FUNCTION}("))
            && source.contains("mod routes_gen;")
    })
}

/// Plans `registration` (or, with `remove`, its removal) in the application
/// at `root`. Refusals go through `fail`, so each generator labels them; a
/// refusal writes nothing.
pub(crate) fn plan(
    root: &Path,
    registration: &Registration,
    remove: bool,
    fail: &dyn Fn(String) -> CliError,
) -> Result<Vec<Edit>, CliError> {
    let mut edits = plan_modules(root, &registration.modules, remove, fail)?;
    if let Some(block) = &registration.routes {
        edits.extend(plan_routes(root, block, &registration.shared, remove, fail)?);
    }
    Ok(edits)
}

/// The seam `forge new` writes: no blocks. It is written as rustfmt
/// formats it, so `forge new` needs no rustfmt (a test holds them equal).
pub(crate) fn initial_routes(views: bool) -> String {
    sign(&unsigned_routes(views, &[]))
}

/// Writes every planned change, all or nothing: on a failure the files
/// already changed get their previous content back.
pub(crate) fn apply(root: &Path, edits: &[Edit]) -> Result<(), CliError> {
    let mut done: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    for edit in edits {
        let Some(content) = &edit.content else {
            continue;
        };
        let path = root.join(&edit.path);
        let before = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                restore(&done);
                return Err(CliError::WriteFile { path, source });
            }
        };
        if let Err(source) = write_atomically(&path, content.as_bytes()) {
            restore(&done);
            return Err(CliError::WriteFile { path, source });
        }
        done.push((path, before));
    }
    Ok(())
}

fn restore(done: &[(PathBuf, Option<Vec<u8>>)]) {
    for (path, before) in done.iter().rev() {
        let _ = match before {
            Some(bytes) => write_atomically(path, bytes),
            None => fs::remove_file(path),
        };
    }
}

/// Replaces `path` with `content` through a temporary file in the same
/// directory, synced before the rename: a reader (or a crash) sees the old
/// file or the new one, never half of one. The file keeps its permissions.
fn write_atomically(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).map_err(|error| io::Error::other(error.to_string()))?;
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = directory.join(format!(".{name}.{suffix}.tmp"));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        if let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        // The rename itself is durable once the directory is synced.
        if let Ok(directory) = fs::File::open(directory) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// The lock that serializes generators and `forge destroy` in one
/// application: `.forge/generate.lock`, created exclusively and removed
/// when dropped.
#[derive(Debug)]
pub(crate) struct GenerateLock {
    path: PathBuf,
}

/// The application-relative path of [`GenerateLock`].
pub(crate) const LOCK_FILE: &str = ".forge/generate.lock";

impl GenerateLock {
    /// Takes the lock of the application at `root`, or says who holds it.
    pub(crate) fn acquire(root: &Path) -> Result<Self, CliError> {
        let path = root.join(LOCK_FILE);
        let refused = |reason: String| CliError::Refused(reason);
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory).map_err(|source| CliError::CreateDirectory {
                path: directory.to_path_buf(),
                source,
            })?;
        }
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(refused(format!(
                "another `forge generate` or `forge destroy` is running in this application \
                 ({LOCK_FILE} exists), so nothing was written. Run the command again when it \
                 ends; if none is running, remove {LOCK_FILE}"
            ))),
            Err(source) => Err(CliError::WriteFile { path, source }),
        }
    }
}

impl Drop for GenerateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Prints one line per edit (`  declare      src/...  (pub mod x;)`).
pub(crate) fn report(
    output: &mut dyn io::Write,
    dry_run: bool,
    edits: &[Edit],
) -> Result<(), CliError> {
    for edit in edits {
        let verb = if dry_run && edit.content.is_some() {
            format!("would {}", edit.verb)
        } else {
            edit.verb.to_owned()
        };
        let detail = if edit.detail.is_empty() {
            String::new()
        } else {
            format!("  ({})", edit.detail)
        };
        writeln!(output, "  {verb:<12} {}{detail}", edit.path).map_err(CliError::Output)?;
    }
    Ok(())
}

fn read_existing(
    root: &Path,
    path: &str,
    fail: &dyn Fn(String) -> CliError,
) -> Result<Option<String>, CliError> {
    match fs::read_to_string(root.join(path)) {
        Ok(source) => Ok(Some(source)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(fail(format!("could not read {path}: {error}"))),
    }
}

fn plan_modules(
    root: &Path,
    modules: &[(String, String)],
    remove: bool,
    fail: &dyn Fn(String) -> CliError,
) -> Result<Vec<Edit>, CliError> {
    let mut files: Vec<(&str, Vec<&str>)> = Vec::new();
    for (file, module) in modules {
        match files.iter_mut().find(|(existing, _)| existing == file) {
            Some((_, names)) => {
                if !names.contains(&module.as_str()) {
                    names.push(module);
                }
            }
            None => files.push((file, vec![module])),
        }
    }
    let mut edits = Vec::new();
    for (file, names) in files {
        refuse_symlinks(root, [file]).map_err(fail)?;
        let Some(original) = read_existing(root, file, fail)? else {
            if remove {
                continue;
            }
            return Err(fail(format!(
                "{file} is missing; generators declare their modules in the `mod.rs` files \
                 `forge new` writes. Restore it (`pub mod` lines only are fine)"
            )));
        };
        let mut content = original.clone();
        let mut changed = Vec::new();
        for name in names {
            let next = if remove {
                undeclare(&content, name)
            } else {
                declare(&content, name)
            }
            .map_err(|line| {
                fail(format!(
                    "{file} declares the module `{name}` as `{line}`, not as the `pub mod \
                     {name};` the generators write and remove, so nothing was changed. Make it \
                     `pub mod {name};` (or remove it) and run the command again"
                ))
            })?;
            if let Some(next) = next {
                content = next;
                changed.push(format!("pub mod {name};"));
            }
        }
        if changed.is_empty() {
            continue;
        }
        // A file that was formatted stays formatted; a file someone left
        // unformatted only gets the line, never a reformat of their code.
        if rustfmt(root, file, &original).is_ok_and(|formatted| formatted == original) {
            content = rustfmt(root, file, &content)?;
        }
        edits.push(Edit {
            path: file.to_owned(),
            verb: if remove { "undeclare" } else { "declare" },
            detail: changed.join(" "),
            content: Some(content),
        });
    }
    Ok(edits)
}

/// How `line` declares a module, when it is a top-level `mod` item.
fn declared_module(line: &str) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "mod" {
            return tokens
                .next()
                .map(|name| name.trim_end_matches([';', '{']))
                .filter(|name| !name.is_empty());
        }
        let modifier = token == "pub" || token.starts_with("pub(") || token.starts_with("#[");
        if !modifier {
            return None;
        }
    }
    None
}

/// `source` with `pub mod name;` added in sorted order, `None` when it is
/// already there, or the line that declares `name` another way.
fn declare(source: &str, name: &str) -> Result<Option<String>, String> {
    let canonical = format!("pub mod {name};");
    let lines: Vec<&str> = source.lines().collect();
    for line in &lines {
        if declared_module(line) == Some(name) {
            return if line.trim_end() == canonical {
                Ok(None)
            } else {
                Err(line.trim_end().to_owned())
            };
        }
    }
    let declarations: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            line.strip_prefix("pub mod ")
                .and_then(|rest| rest.strip_suffix(';'))
                .map(|module| (index, module))
        })
        .collect();
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    if let Some((first, _)) = declarations.first() {
        let position = declarations
            .iter()
            .find(|(_, module)| *module > name)
            .map(|(index, _)| *index)
            .or_else(|| declarations.last().map(|(index, _)| index + 1))
            .unwrap_or(*first);
        out.insert(position, canonical);
    } else {
        // After the leading comments, set apart by blank lines.
        let position = lines
            .iter()
            .position(|line| !line.trim().is_empty() && !line.trim_start().starts_with("//"))
            .unwrap_or(lines.len());
        let mut insert = Vec::new();
        if position > 0 && !lines[position - 1].trim().is_empty() {
            insert.push(String::new());
        }
        insert.push(canonical);
        if position < lines.len() && !lines[position].trim().is_empty() {
            insert.push(String::new());
        }
        out.splice(position..position, insert);
    }
    Ok(Some(join_lines(&out)))
}

/// `source` without `pub mod name;`, `None` when it is not declared, or the
/// line that declares `name` another way.
fn undeclare(source: &str, name: &str) -> Result<Option<String>, String> {
    let canonical = format!("pub mod {name};");
    let lines: Vec<&str> = source.lines().collect();
    let Some(index) = lines
        .iter()
        .position(|line| declared_module(line) == Some(name))
    else {
        return Ok(None);
    };
    if lines[index].trim_end() != canonical {
        return Err(lines[index].trim_end().to_owned());
    }
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    out.remove(index);
    // Never leave two blank lines in a row, or one at the end.
    if index > 0
        && out.get(index - 1).is_some_and(|line| line.trim().is_empty())
        && out.get(index).is_none_or(|line| line.trim().is_empty())
    {
        out.remove(index - 1);
    }
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    Ok(Some(join_lines(&out)))
}

fn join_lines(lines: &[String]) -> String {
    let mut joined = lines.join("\n");
    joined.push('\n');
    joined
}

/// Whether `text` uses the identifier `name` (not as part of a longer one).
fn uses(text: &str, name: &str) -> bool {
    let identifier = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(name).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + name.len()..].chars().next();
        !before.is_some_and(identifier) && !after.is_some_and(identifier)
    })
}

fn edited() -> String {
    format!(
        "{ROUTES_FILE} was edited by hand since a generator last wrote it (its forge:checksum \
         no longer matches), so nothing was written. Generators own that file: move your \
         change into {BOOTSTRAP_FILE} (or, to replace a guard such as protect, declare a \
         function of that name there), then restore the file with `git checkout \
         {ROUTES_FILE}` and run the command again"
    )
}

/// The lowercase hex SHA-256 of `text`.
fn sha256_hex(text: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The blocks of a seam, after checking its checksum: the shared and
/// generated blocks of `generated_routes`, and the guards of `mod guards`.
fn parse_routes(source: &str) -> Result<Vec<Block>, String> {
    let recorded = source
        .lines()
        .next()
        .and_then(|line| line.strip_prefix(CHECKSUM_PREFIX))
        .filter(|sum| sum.len() == CHECKSUM_PLACEHOLDER.len())
        .ok_or_else(edited)?;
    let unsigned = source.replacen(
        &format!("{CHECKSUM_PREFIX}{recorded}"),
        &format!("{CHECKSUM_PREFIX}{CHECKSUM_PLACEHOLDER}"),
        1,
    );
    if sha256_hex(&unsigned) != recorded {
        return Err(edited());
    }
    if !source.contains(&format!("pub(super) fn {ROUTES_FUNCTION}(")) {
        return Err(edited());
    }
    // Every line of `generated_routes`' body and of `mod guards` belongs to
    // a block; the rest of the file is the generators' own text, which
    // `plan_routes` checks by rendering the blocks again.
    let mut blocks: Vec<Block> = Vec::new();
    let mut region = false;
    let mut open = false;
    for line in source.lines() {
        let signature = line == ") -> Result<(), forge::ComposeError> {"
            || (line.starts_with(&format!("pub(super) fn {ROUTES_FUNCTION}(")) && line.ends_with('{'));
        if signature || line == "mod guards {" {
            region = true;
            open = false;
        } else if region && (line == "    Ok(())" || line == "}") {
            region = false;
            open = false;
        } else if let Some(marker) = line.strip_prefix(MARKER_PREFIX) {
            let (kind, name) = marker.split_once(' ').ok_or_else(edited)?;
            if !region || kind.is_empty() || name.is_empty() || name.contains(' ') {
                return Err(edited());
            }
            blocks.push(Block {
                kind: kind.to_owned(),
                name: name.to_owned(),
                body: String::new(),
            });
            open = true;
        } else if region {
            if !open {
                return Err(edited());
            }
            let block = blocks.last_mut().ok_or_else(edited)?;
            block.body.push_str(line);
            block.body.push('\n');
        }
    }
    Ok(blocks)
}

/// The text after `use super::*;` in a seam with guards.
const GUARDS_NOTE: &str = "\
// A guard the application declares itself (a `fn protect` in
// src/bootstrap/mod.rs) replaces the generated one below, which the
// generators then leave out. Declaring one while the generated one is still
// here makes the name ambiguous, a compile error rather than a guard silently
// ignored: run the generator again.
";

/// The seam with `blocks`, before formatting, with the checksum placeholder.
fn unsigned_routes(views: bool, blocks: &[Block]) -> String {
    let mut body = String::new();
    let mut guards = String::new();
    for block in blocks {
        let text = format!("{MARKER_PREFIX}{}\n{}", block.id(), block.body);
        if block.kind == "guard" {
            guards.push_str(&text);
        } else {
            body.push_str(&text);
        }
    }
    let parameter = |name: &str| {
        if uses(&body, name) {
            name.to_owned()
        } else {
            format!("_{name}")
        }
    };
    let site = if views {
        format!(
            "    {}: &std::sync::Arc<crate::adapters::http::views::Site>,\n",
            parameter("site")
        )
    } else {
        String::new()
    };
    let calls_guard = GUARDS.iter().any(|guard| calls(&body, guard));
    let mut imports = String::new();
    if calls_guard {
        imports.push_str(GUARDS_NOTE);
        imports.push_str("#[allow(unused_imports)]\nuse super::*;\n");
        if !guards.is_empty() {
            imports.push_str("use guards::*;\n");
        }
        imports.push('\n');
    }
    let guards_module = if guards.is_empty() {
        String::new()
    } else {
        format!("\n/// The guards of the generated routes.\nmod guards {{\n{guards}}}\n")
    };
    format!(
        "{CHECKSUM_PREFIX}{CHECKSUM_PLACEHOLDER}\n{ROUTES_HEADER}\n{imports}\
         /// Mounts the routes of the generated resources and controllers.\n\
         pub(super) fn {ROUTES_FUNCTION}(\n    \
         {}: &mut forge::Composition<'_>,\n\
         {site}) -> Result<(), forge::ComposeError> {{\n\
         {body}    Ok(())\n}}\n{guards_module}",
        parameter("composition"),
    )
}

/// Replaces the checksum placeholder of `formatted` with its checksum.
fn sign(formatted: &str) -> String {
    let sum = sha256_hex(formatted);
    formatted.replacen(
        &format!("{CHECKSUM_PREFIX}{CHECKSUM_PLACEHOLDER}"),
        &format!("{CHECKSUM_PREFIX}{sum}"),
        1,
    )
}

/// The seam with `blocks`, formatted and signed.
fn render_routes(root: &Path, views: bool, blocks: &[Block]) -> Result<String, CliError> {
    let formatted = rustfmt(root, ROUTES_FILE, &unsigned_routes(views, blocks))?;
    Ok(sign(&formatted))
}

/// The guards a seam may define, in the order it defines them.
const GUARDS: &[&str] = &["protect", "public"];

/// Whether `text` calls the function `name`.
fn calls(text: &str, name: &str) -> bool {
    uses(text, name) && text.contains(&format!("{name}("))
}

/// Whether an entry needs the shared block `block`.
fn needs(used: &str, block: &Block) -> bool {
    match block.kind.as_str() {
        "guard" => calls(used, &block.name),
        _ => uses(used, &block.name),
    }
}

/// The seam's blocks for `entries`: the shared blocks some entry needs, in
/// [`SHARED_KINDS`] order (existing ones as they are, missing ones from
/// `offered`), except the guards the application declares itself, then
/// the entries in the order they were generated.
fn with_shared(
    existing: &[Block],
    offered: &[Block],
    entries: &[Block],
    overridden: &dyn Fn(&str) -> bool,
) -> Result<Vec<Block>, String> {
    let used: String = entries.iter().map(|entry| entry.body.as_str()).collect();
    let mut shared: Vec<Block> = Vec::new();
    for candidate in existing.iter().chain(offered) {
        let skip = !needs(&used, candidate)
            || (candidate.kind == "guard" && overridden(&candidate.name))
            || shared.iter().any(|block| block.id() == candidate.id());
        if !skip {
            shared.push(candidate.clone());
        }
    }
    for guard in GUARDS {
        let defined = shared.iter().any(|block| block.kind == "guard" && block.name == *guard);
        if calls(&used, guard) && !defined && !overridden(guard) {
            return Err(format!(
                "{ROUTES_FILE} needs {guard}, which it does not define: restore the file from \
                 version control, or run `forge destroy` and the generator again"
            ));
        }
    }
    shared.sort_by_key(|block| {
        (
            SHARED_KINDS.iter().position(|kind| *kind == block.kind),
            GUARDS.iter().position(|guard| *guard == block.name),
        )
    });
    shared.extend(entries.iter().cloned());
    Ok(shared)
}

/// The Rust sources of `src/bootstrap` other than the seam: where the
/// application declares the guards that replace the generated ones.
fn bootstrap_declarations(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root.join("src/bootstrap")) else {
        return Vec::new();
    };
    let mut sources: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".rs")
                && name != "routes_gen.rs"
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .collect();
    sources.sort();
    sources
}

/// Whether `source` declares a function `name` (not in a comment).
fn declares_fn(source: &str, name: &str) -> bool {
    source.lines().any(|line| {
        let code = line.split("//").next().unwrap_or_default();
        code.match_indices(&format!("fn {name}")).any(|(start, found)| {
            let rest = code[start + found.len()..].trim_start();
            let before = code[..start].chars().next_back();
            (rest.starts_with('(') || rest.starts_with('<'))
                && !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    })
}

fn plan_routes(
    root: &Path,
    block: &Block,
    offered: &[Block],
    remove: bool,
    fail: &dyn Fn(String) -> CliError,
) -> Result<Option<Edit>, CliError> {
    refuse_symlinks(root, [ROUTES_FILE]).map_err(fail)?;
    let views = root.join("src/adapters/http/views/mod.rs").is_file();
    let declared = bootstrap_declarations(root);
    let overridden = |name: &str| declared.iter().any(|source| declares_fn(source, name));
    let current = read_existing(root, ROUTES_FILE, fail)?;
    let blocks = match &current {
        Some(source) => {
            let blocks = parse_routes(source).map_err(fail)?;
            // A file signed by hand with a structure the generators never
            // write (an item outside the blocks, say) is refused too.
            if render_routes(root, views, &blocks)? != *source {
                return Err(fail(edited()));
            }
            blocks
        }
        None => Vec::new(),
    };
    let (shared, mut entries): (Vec<Block>, Vec<Block>) =
        blocks.into_iter().partition(Block::is_shared);
    let render = |entries: &[Block]| -> Result<String, CliError> {
        let blocks = with_shared(&shared, offered, entries, &overridden).map_err(fail)?;
        render_routes(root, views, &blocks)
    };
    let found = entries.iter().position(|entry| entry.id() == block.id());
    let verb = match found {
        None if remove => return Ok(None),
        Some(index) if remove => {
            entries.remove(index);
            "unwire"
        }
        Some(index) => {
            let existing = render(&entries)?;
            entries[index] = block.clone();
            if render(&entries)? != existing {
                return Err(fail(format!(
                    "refusing to generate: {ROUTES_FILE} already wires the {} with other code \
                     (generated with other arguments, or by another Forge version). Run `forge \
                     destroy` with the arguments it was generated with first",
                    block.id()
                )));
            }
            "wire"
        }
        None => {
            entries.push(block.clone());
            "wire"
        }
    };
    let rendered = render(&entries)?;
    let unchanged = current.as_deref() == Some(rendered.as_str());
    Ok(Some(Edit {
        path: ROUTES_FILE.to_owned(),
        verb: if unchanged { "identical" } else { verb },
        detail: block.id(),
        content: (!unchanged).then_some(rendered),
    }))
}

#[cfg(test)]
mod tests;
