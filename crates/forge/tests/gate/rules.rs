//! The rules of the supply-chain gate, each a function from a file's text to
//! its violations. Shared by `crates/forge/tests/supply_chain.rs` (the
//! framework's workflows and templates) and by forge-cli's generator tests
//! (the workflows `forge new` renders, for every option), through `#[path]`.
//! Test code only: the YAML parser it uses is a dev-dependency.

#![allow(dead_code)]

pub(crate) fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// `owner/repo[/path]@<40 hex>`.
pub(crate) fn is_pinned_action(reference: &str) -> bool {
    let Some((name, commit)) = reference.split_once('@') else {
        return false;
    };
    name.contains('/')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
        && is_hex(commit, 40)
}

/// `# vX`, `# vX.Y` or `# vX.Y.Z`, the version the commit was verified against.
pub(crate) fn is_version_comment(comment: &str) -> bool {
    let Some(version) = comment.trim().strip_prefix("# v") else {
        return false;
    };
    is_numeric_version(version, 1..=3)
}

/// `X`, `X.Y` or `X.Y.Z` (as many parts as `parts` allows), digits only.
pub(crate) fn is_numeric_version(version: &str, parts: std::ops::RangeInclusive<usize>) -> bool {
    let split: Vec<&str> = version.split('.').collect();
    parts.contains(&split.len())
        && split
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Whether `reference` ends in `@sha256:<64 hex>`.
pub(crate) fn has_digest(reference: &str) -> bool {
    reference
        .rsplit_once("@sha256:")
        .is_some_and(|(_, digest)| is_hex(digest, 64))
}

/// The references of a container image in `text` that start with `prefix`:
/// up to the next whitespace, quote or backslash.
pub(crate) fn image_references<'a>(text: &'a str, prefix: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(prefix) {
        let tail = &rest[start..];
        let end = tail
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\\' | '\'')
            })
            .unwrap_or(tail.len());
        found.push(&tail[..end]);
        rest = &tail[end..];
    }
    found
}

/// A YAML line split into its code and its comment: a `#` that starts the
/// line or follows a space or a tab.
pub(crate) fn split_comment(line: &str) -> (&str, &str) {
    // A quote that starts a token (after the line start, a blank or `[`, `{`,
    // `,`, `:`) opens a quoted scalar, in which `#` is text.
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(b'"') if byte == b'\\' => index += 1,
            Some(b'\'') if byte == b'\'' && bytes.get(index + 1) == Some(&b'\'') => index += 1,
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => {
                let starts_token = index == 0
                    || matches!(bytes[index - 1], b' ' | b'\t' | b'[' | b'{' | b',' | b':');
                if matches!(byte, b'"' | b'\'') && starts_token {
                    quote = Some(byte);
                } else if byte == b'#' && (index == 0 || matches!(bytes[index - 1], b' ' | b'\t')) {
                    return line.split_at(index);
                }
            }
        }
        index += 1;
    }
    (line, "")
}

pub(crate) fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// The values of every `key` in the code of a line, unquoted: block style
/// (`key: x`, `- key: x`, `"key": x`, `key : x`) and flow style
/// (`- { key: x, other: y }`). A flow value ends at `,` or `}`.
pub(crate) fn key_values<'a>(code: &'a str, key: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    for (start, _) in code.match_indices(key) {
        // What precedes the key, past an opening quote and blanks: the start
        // of the line, a list dash, or a flow collection's `{`, `[` or `,`.
        let before = code[..start].trim_end_matches(['"', '\'']).trim_end();
        if !(before.is_empty() || before.ends_with(['-', '{', '[', ','])) {
            continue;
        }
        let after = code[start + key.len()..]
            .trim_start_matches(['"', '\''])
            .trim_start();
        let Some(value) = after.strip_prefix(':') else {
            continue;
        };
        let value = value.split([',', '}', ']']).next().unwrap_or_default();
        found.push(value.trim().trim_matches(['"', '\'']));
    }
    found
}

/// The `uses: ./path` references of a workflow or action, without `./`.
pub(crate) fn local_actions(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| key_values(split_comment(line).0, "uses"))
        .filter_map(|reference| reference.strip_prefix("./"))
        .map(|path| path.trim_end_matches('/').to_owned())
        .collect()
}

/// Every action is `owner/repo[/path]@<40-hex commit> # vX.Y.Z`, every
/// `docker://` action is pinned by digest, and no `uses:` escapes the check
/// through flow style, an anchor or an alias. Local `./` actions are checked
/// as files of their own (`workflows_and_local_actions`).
pub(crate) fn pin_violations(name: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let (code, comment) = split_comment(line);
        for reference in key_values(code, "uses") {
            if reference.starts_with("./") {
                continue;
            }
            let pinned = match reference.strip_prefix("docker://") {
                Some(image) => has_digest(image),
                None => is_pinned_action(reference) && is_version_comment(comment),
            };
            if !pinned {
                violations.push(format!(
                    "{name}:{}: `{}` must be owner/repo@<40-hex commit> # vX.Y.Z, or docker://<image>@sha256:<digest>",
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    violations
}

/// The action repositories a workflow may use, compared without case
/// (GitHub resolves owner and name case-insensitively). Adding one is a
/// supply-chain decision: review the action's code, pin it by SHA, and add it
/// here and to the table of docs/supply-chain.md in the same change. Local
/// `./` actions are checked as files of their own; `docker://` actions are not
/// allowed.
pub(crate) const ALLOWED_ACTIONS: [&str; 10] = [
    "actions/cache",
    "actions/checkout",
    "actions/upload-artifact",
    "docker/build-push-action",
    "docker/login-action",
    "docker/setup-buildx-action",
    "embarkstudios/cargo-deny-action",
    "ossf/scorecard-action",
    "sigstore/cosign-installer",
    "swatinem/rust-cache",
];

/// Every `uses:` names an allowlisted repository (or a local `./` action),
/// whatever its pin: an installer action such as taiki-e/install-action
/// would otherwise put any tool on PATH, pinned or not.
pub(crate) fn allowlist_violations(name: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (number, line) in text.lines().enumerate() {
        for reference in key_values(split_comment(line).0, "uses") {
            if reference.starts_with("./") {
                continue;
            }
            let action = reference
                .split_once('@')
                .map_or(reference, |(action, _)| action);
            // owner/repo[/sub/path]: no empty, `.` or `..` segment can walk
            // from an allowlisted repository into another one.
            let plain_path = action
                .split('/')
                .all(|segment| !matches!(segment, "" | "." | ".."));
            let allowed = plain_path
                && action_repository(reference)
                    .is_some_and(|repository| ALLOWED_ACTIONS.contains(&repository.as_str()));
            if !allowed {
                violations.push(format!(
                    "{name}:{}: `{reference}` is not an allowlisted action (ALLOWED_ACTIONS, docs/supply-chain.md)",
                    number + 1
                ));
            }
        }
    }
    violations
}

/// The `owner/repo` of a `uses:` reference, lower case (GitHub resolves both
/// without case); `None` when it names none (`docker://...` included).
pub(crate) fn action_repository(reference: &str) -> Option<String> {
    let action = reference
        .split_once('@')
        .map_or(reference, |(action, _)| action);
    let mut parts = action.splitn(3, '/');
    let (owner, repository) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repository.is_empty() && !owner.contains(':'))
        .then(|| format!("{owner}/{repository}").to_ascii_lowercase())
}

/// Which lines of a gated file are nodes (keys and values the rules read;
/// block-scalar text, comments, blank lines and document markers are not),
/// and what makes the file leave the simple YAML subset the gate reads
/// exactly (docs/supply-chain.md, "Gated files are written in a simple YAML
/// subset"):
/// - a `\r` not followed by `\n`, U+0085, U+2028 or U+2029 (hidden line
///   breaks), or U+FEFF anywhere but at the start;
/// - a directive (`%YAML`), content after a document marker (`---`, `...`),
///   and a document root that does not start at column 0;
/// - outside block scalars, an anchor (`&`), alias (`*`), tag (`!`),
///   explicit key (`?`) or merge key (`<<`) where a key or value starts
///   (after the indentation, `- `, `: `, or a flow collection's `[`, `{`,
///   `,`);
/// - a plain scalar continued on a deeper line (write a `>-` block scalar);
/// - a flow collection or quoted scalar that does not close on its line, a
///   flow indicator or a `:` without a blank inside a flow collection's
///   plain scalar, a quoted key whose `:` is not followed by a blank, and a
///   double-quoted key with an escape.
pub(crate) fn yaml_subset(text: &str) -> (Vec<bool>, Vec<(usize, &'static str)>) {
    let mut problems = Vec::new();
    let mut line = 0;
    for (offset, character) in text.char_indices() {
        if character == '\n' {
            line += 1;
        }
        let hidden_break = matches!(character, '\u{85}' | '\u{2028}' | '\u{2029}')
            || (character == '\r' && !text[offset + 1..].starts_with('\n'))
            || (character == '\u{feff}' && offset > 0);
        if hidden_break {
            problems.push((
                line,
                "a hidden line break (lone CR, NEL, LS, PS) or a byte-order mark",
            ));
        }
    }
    let mut nodes = Vec::new();
    // The column of the key whose block scalar is being skipped, and of the
    // node whose plain scalar a deeper line would continue.
    let mut block: Option<usize> = None;
    let mut plain: Option<usize> = None;
    let mut expect_root = true;
    for (number, raw) in text.split('\n').enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let line = if number == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        let content = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - content.len();
        if content.is_empty() || content.starts_with('#') {
            if content.starts_with('#') && block.is_some_and(|column| indent <= column) {
                block = None;
            }
            nodes.push(false);
            continue;
        }
        if let Some(column) = block {
            if indent > column {
                nodes.push(false);
                continue;
            }
            block = None;
        }
        if indent == 0
            && ["---", "..."]
                .iter()
                .any(|marker| content == *marker || content.starts_with(&format!("{marker} ")))
        {
            let rest = split_comment(&content[3..]).0.trim();
            if !rest.is_empty() {
                problems.push((number, "content after a document marker"));
            }
            expect_root = true;
            plain = None;
            nodes.push(false);
            continue;
        }
        nodes.push(true);
        if indent == 0 && content.starts_with('%') {
            problems.push((number, "a YAML directive"));
            continue;
        }
        if expect_root {
            if indent != 0 {
                problems.push((number, "a document root that does not start at column 0"));
            }
            expect_root = false;
        }
        if plain.is_some_and(|column| indent > column) {
            problems.push((
                number,
                "a plain scalar continued on a deeper line (write a `>-` block scalar)",
            ));
        }
        plain = None;
        let mut content = content;
        while let Some(rest) = content
            .strip_prefix('-')
            .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        {
            content = rest.trim_start_matches([' ', '\t']);
        }
        let column = line.len() - content.len();
        let characters: Vec<(usize, char)> = content.char_indices().collect();
        let mut index = 0;
        let mut depth = 0_usize;
        let mut at_node = true;
        let mut last_plain = false;
        let mut last_block = false;
        while index < characters.len() {
            let (at, character) = characters[index];
            if at_node {
                if matches!(character, ' ' | '\t') {
                    index += 1;
                    continue;
                }
                at_node = false;
                last_plain = false;
                last_block = false;
                if matches!(character, '&' | '*' | '!' | '?') || content[at..].starts_with("<<") {
                    problems.push((number, "an anchor, alias, tag, explicit key or merge key"));
                }
                match character {
                    '"' | '\'' => {
                        let mut end = None;
                        let mut scan = index + 1;
                        while scan < characters.len() {
                            let (_, inner) = characters[scan];
                            if character == '"' && inner == '\\' {
                                scan += 2;
                                continue;
                            }
                            if inner == character {
                                if character == '\''
                                    && characters
                                        .get(scan + 1)
                                        .is_some_and(|(_, next)| *next == '\'')
                                {
                                    scan += 2;
                                    continue;
                                }
                                end = Some(scan);
                                break;
                            }
                            scan += 1;
                        }
                        let Some(end) = end else {
                            problems
                                .push((number, "a quoted scalar that does not close on its line"));
                            break;
                        };
                        let after = &content[characters[end].0 + 1..];
                        let trimmed = after.trim_start_matches([' ', '\t']);
                        if let Some(value) = trimmed.strip_prefix(':') {
                            if character == '"' && content[at..characters[end].0].contains('\\') {
                                problems.push((number, "a double-quoted key with an escape"));
                            }
                            if !(value.is_empty() || value.starts_with([' ', '\t'])) {
                                problems.push((
                                    number,
                                    "a quoted key whose `:` is not followed by a blank",
                                ));
                            }
                        }
                        index = end + 1;
                        continue;
                    }
                    '[' | '{' => {
                        depth += 1;
                        at_node = true;
                        index += 1;
                        continue;
                    }
                    ']' | '}' if depth > 0 => {
                        depth -= 1;
                        index += 1;
                        continue;
                    }
                    '|' | '>' => last_block = depth == 0,
                    '#' => break,
                    _ => last_plain = depth == 0,
                }
                index += 1;
                continue;
            }
            let next = characters.get(index + 1).map(|(_, next)| *next);
            match character {
                '#' if characters
                    .get(index.wrapping_sub(1))
                    .is_some_and(|(_, previous)| matches!(previous, ' ' | '\t')) =>
                {
                    break;
                }
                ':' if next.is_none_or(|next| matches!(next, ' ' | '\t' | ',' | ']' | '}')) => {
                    // A key ends: what follows is its value.
                    at_node = true;
                    last_plain = false;
                    last_block = false;
                }
                ':' if depth > 0 => {
                    problems.push((number, "a `:` without a blank inside a flow collection"));
                }
                ',' if depth > 0 => at_node = true,
                ']' | '}' if depth > 0 => depth -= 1,
                '[' | '{' if depth > 0 => {
                    problems.push((number, "a flow indicator inside a plain scalar"));
                }
                _ => {}
            }
            index += 1;
        }
        if depth > 0 {
            problems.push((number, "a flow collection that spans lines"));
        }
        if depth == 0 && last_block {
            block = Some(column);
        } else if depth == 0 && last_plain {
            plain = Some(column);
        }
    }
    (nodes, problems)
}

/// The YAML subset rule as violations.
pub(crate) fn yaml_subset_violations(name: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    yaml_subset(text)
        .1
        .into_iter()
        .map(|(number, problem)| {
            format!(
                "{name}:{}: `{}`: {problem}: gated files are written in a simple YAML subset (docs/supply-chain.md)",
                number + 1,
                lines.get(number).map_or("", |line| line.trim())
            )
        })
        .collect()
}

/// A generated-app template with its `%%...%%` markers blanked line-aligned
/// (a line that is only markers becomes empty, an inline `%%NAME%%` the
/// word `x`), so the scanner and the parser read every branch at the line
/// it has in the template.
pub(crate) fn blank_template_markers(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let mut kept = String::new();
            let mut rest = line;
            while let Some(start) = rest.find("%%") {
                let Some(length) = rest[start + 2..].find("%%") else {
                    break;
                };
                kept.push_str(&rest[..start]);
                kept.push('\u{0}');
                rest = &rest[start + 2 + length + 2..];
            }
            kept.push_str(rest);
            if kept
                .chars()
                .all(|character| character == '\u{0}' || character.is_whitespace())
                && kept.contains('\u{0}')
            {
                String::new()
            } else {
                kept.replace('\u{0}', "x")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A YAML node as the parser reads it, with the 1-based line it starts on.
#[derive(Debug)]
pub(crate) enum YamlNode {
    Scalar(String, usize),
    Sequence(Vec<YamlNode>),
    Mapping(Vec<(YamlNode, YamlNode)>),
}

impl YamlNode {
    fn scalar(&self) -> Option<&str> {
        match self {
            Self::Scalar(value, _) => Some(value),
            _ => None,
        }
    }

    /// The value of `key` in a mapping (the first one, as written).
    fn get(&self, key: &str) -> Option<&YamlNode> {
        match self {
            Self::Mapping(entries) => entries
                .iter()
                .find(|(name, _)| name.scalar() == Some(key))
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Every node of the tree, this one first.
    fn visit<'a>(&'a self, found: &mut Vec<&'a YamlNode>) {
        found.push(self);
        match self {
            Self::Scalar(..) => {}
            Self::Sequence(items) => items.iter().for_each(|item| item.visit(found)),
            Self::Mapping(entries) => entries.iter().for_each(|(key, value)| {
                key.visit(found);
                value.visit(found);
            }),
        }
    }
}

#[derive(Default)]
pub(crate) struct YamlTree {
    stack: Vec<(bool, Vec<YamlNode>)>,
    documents: Vec<YamlNode>,
    problems: Vec<String>,
}

impl YamlTree {
    fn push(&mut self, node: YamlNode) {
        match self.stack.last_mut() {
            Some((_, items)) => items.push(node),
            None => self.documents.push(node),
        }
    }
}

impl yaml_rust2::parser::MarkedEventReceiver for YamlTree {
    fn on_event(&mut self, event: yaml_rust2::Event, mark: yaml_rust2::scanner::Marker) {
        use yaml_rust2::Event;
        let line = mark.line();
        let refuse = |anchor: usize, tag: bool| anchor != 0 || tag;
        match event {
            Event::Scalar(value, _, anchor, tag) => {
                if refuse(anchor, tag.is_some()) {
                    self.problems.push(format!("line {line}: an anchor or tag"));
                }
                self.push(YamlNode::Scalar(value, line));
            }
            Event::Alias(_) => {
                self.problems.push(format!("line {line}: an alias"));
                self.push(YamlNode::Scalar(String::new(), line));
            }
            Event::SequenceStart(anchor, ref tag) | Event::MappingStart(anchor, ref tag) => {
                if refuse(anchor, tag.is_some()) {
                    self.problems.push(format!("line {line}: an anchor or tag"));
                }
                let mapping = matches!(event, Event::MappingStart(..));
                self.stack.push((mapping, Vec::new()));
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let Some((mapping, items)) = self.stack.pop() else {
                    return;
                };
                if mapping {
                    let mut entries = Vec::new();
                    let mut items = items.into_iter();
                    let mut seen = std::collections::BTreeSet::new();
                    while let (Some(key), Some(value)) = (items.next(), items.next()) {
                        if key.scalar() == Some("<<") {
                            self.problems.push(format!("line {line}: a merge key"));
                        }
                        // A reader that takes the first value and one that
                        // takes the last would disagree: no key twice, in
                        // any case.
                        if let YamlNode::Scalar(name, key_line) = &key
                            && !seen.insert(name.to_ascii_lowercase())
                        {
                            self.problems
                                .push(format!("line {key_line}: the key `{name}` appears twice"));
                        }
                        entries.push((key, value));
                    }
                    self.push(YamlNode::Mapping(entries));
                } else {
                    self.push(YamlNode::Sequence(items));
                }
            }
            _ => {}
        }
    }
}

/// The documents of a gated file as a YAML parser (yaml-rust2) reads them;
/// anchors, aliases, tags and merge keys are refused here too.
pub(crate) fn parse_yaml(text: &str) -> Result<Vec<YamlNode>, String> {
    let mut tree = YamlTree::default();
    yaml_rust2::parser::Parser::new_from_str(text)
        .load(&mut tree, true)
        .map_err(|error| error.to_string())?;
    if tree.problems.is_empty() {
        Ok(tree.documents)
    } else {
        Err(tree.problems.join("; "))
    }
}

/// Every `uses` key the parser reads, as `line:value`.
pub(crate) fn parsed_uses(documents: &[YamlNode]) -> Vec<String> {
    let mut nodes = Vec::new();
    documents
        .iter()
        .for_each(|document| document.visit(&mut nodes));
    let mut found = Vec::new();
    for node in nodes {
        if let YamlNode::Mapping(entries) = node {
            for (key, value) in entries {
                if let YamlNode::Scalar(name, line) = key
                    && name == "uses"
                {
                    found.push(format!(
                        "{line}:{}",
                        value.scalar().unwrap_or("<not a scalar>")
                    ));
                }
            }
        }
    }
    found.sort();
    found
}

/// Every `uses` the line scanner reads on node lines (block-scalar text
/// aside, so a decoy there cannot stand in for a key it misses), as
/// `line:value`.
pub(crate) fn scanned_uses(text: &str) -> Vec<String> {
    let (nodes, _) = yaml_subset(text);
    let mut found = Vec::new();
    for (number, line) in text.split('\n').enumerate() {
        if !nodes.get(number).copied().unwrap_or(false) {
            continue;
        }
        for value in key_values(
            split_comment(line.strip_suffix('\r').unwrap_or(line)).0,
            "uses",
        ) {
            found.push(format!("{}:{value}", number + 1));
        }
    }
    found.sort();
    found
}

/// The parser and the line scanner read the same `uses` keys: the same
/// values on the same lines.
pub(crate) fn parser_violations(name: &str, text: &str) -> Vec<String> {
    match parse_yaml(text) {
        Err(error) => vec![format!("{name}: {error}")],
        Ok(documents) => {
            let (parsed, scanned) = (parsed_uses(&documents), scanned_uses(text));
            if parsed == scanned {
                Vec::new()
            } else {
                vec![format!(
                    "{name}: the YAML parser reads uses keys {parsed:?}, the gate {scanned:?}: write every uses: key plainly"
                )]
            }
        }
    }
}

/// Read from the parsed tree: every step that uses actions/checkout (any
/// case) sets `with: persist-credentials: false` itself, and no
/// `permissions` grants `write` beyond the release job's `id-token` and
/// `packages` (or `write-all`).
pub(crate) fn tree_violations(name: &str, text: &str, release: bool) -> Vec<String> {
    let documents = match parse_yaml(text) {
        Ok(documents) => documents,
        Err(error) => return vec![format!("{name}: {error}")],
    };
    let mut nodes = Vec::new();
    documents
        .iter()
        .for_each(|document| document.visit(&mut nodes));
    let mut violations = Vec::new();
    for node in nodes {
        let YamlNode::Mapping(entries) = node else {
            continue;
        };
        if let Some(YamlNode::Scalar(reference, line)) = node.get("uses") {
            let action = reference
                .split_once('@')
                .map_or(reference.as_str(), |(action, _)| action);
            let kept = node
                .get("with")
                .and_then(|with| with.get("persist-credentials"))
                .and_then(YamlNode::scalar)
                == Some("false");
            if action.eq_ignore_ascii_case("actions/checkout") && !kept {
                violations.push(format!(
                    "{name}:{line}: this checkout step must set with: persist-credentials: false"
                ));
            }
        }
        for (key, value) in entries {
            if key.scalar() != Some("permissions") {
                continue;
            }
            match value {
                YamlNode::Scalar(scope, line) if scope != "read-all" && !scope.is_empty() => {
                    violations.push(format!(
                        "{name}:{line}: `permissions: {scope}` is not allowed"
                    ));
                }
                YamlNode::Mapping(scopes) => {
                    for (scope, access) in scopes {
                        let scope = scope.scalar().unwrap_or_default();
                        let allowed = match access.scalar() {
                            Some("read" | "none") => true,
                            Some("write") => release && matches!(scope, "id-token" | "packages"),
                            _ => false,
                        };
                        if !allowed {
                            violations.push(format!(
                                "{name}: `{scope}: {:?}` is not allowed",
                                access.scalar()
                            ));
                        }
                    }
                }
                YamlNode::Scalar(..) => {}
                YamlNode::Sequence(_) => {
                    violations.push(format!("{name}: permissions must be a mapping"));
                }
            }
        }
    }
    violations
}

/// The lines of the step (list item) that line `index` belongs to: from its
/// `- ` to the next line indented no deeper than that dash.
pub(crate) fn step_lines<'a, 'b>(lines: &'b [&'a str], index: usize) -> &'b [&'a str] {
    let own = indentation(lines[index]);
    let start = (0..=index)
        .rev()
        .find(|&candidate| {
            split_comment(lines[candidate])
                .0
                .trim_start()
                .starts_with('-')
                && indentation(lines[candidate]) <= own
        })
        .unwrap_or(index);
    let dash = indentation(lines[start]);
    let end = (start + 1..lines.len())
        .find(|&candidate| {
            !split_comment(lines[candidate]).0.trim().is_empty()
                && indentation(lines[candidate]) <= dash
        })
        .unwrap_or(lines.len());
    &lines[start..end]
}

/// Every `actions/checkout` step, in any spelling of the owner and name
/// (GitHub resolves them case-insensitively), sets
/// `persist-credentials: false` itself: the token stays out of .git/config,
/// where later steps could read it.
pub(crate) fn checkout_violations(name: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut violations = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let checkout = key_values(split_comment(line).0, "uses")
            .iter()
            .any(|reference| {
                reference
                    .split_once('@')
                    .map_or(*reference, |(action, _)| action)
                    .eq_ignore_ascii_case("actions/checkout")
            });
        if !checkout {
            continue;
        }
        let persists = !step_lines(&lines, index).iter().any(|line| {
            key_values(split_comment(line).0, "persist-credentials").contains(&"false")
        });
        if persists {
            violations.push(format!(
                "{name}:{}: this checkout step must set persist-credentials: false",
                index + 1
            ));
        }
    }
    violations
}

/// A mapping entry `key: value` (the value may be empty), with the key
/// unquoted; `None` for anything else, such as a shell line of a `run:` block.
pub(crate) fn mapping_entry(content: &str) -> Option<(&str, &str)> {
    let (key, value) = content.split_once(':')?;
    let key = key.trim().trim_matches(['"', '\'']);
    let plain = !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    (plain && (value.is_empty() || value.starts_with([' ', '\t']))).then_some((key, value.trim()))
}

/// The containers a workflow runs, as (line number, image): a service's
/// `image:` (`services.<id>.image`), a job's `container.image`, the
/// `container: <image>` shorthand, and the same in flow style. An `image:`
/// anywhere else, such as an action input under a step's `with:` or a matrix
/// entry (release.yml), is not a container.
pub(crate) fn container_images(text: &str) -> Vec<(usize, String)> {
    // The keys that enclose the current line, with their indentation.
    let mut parents: Vec<(usize, String)> = Vec::new();
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let code = split_comment(line).0;
        if code.trim().is_empty() {
            continue;
        }
        let mut indent = indentation(code);
        let mut content = code.trim();
        // A list item's mapping starts after its dash, one level deeper.
        if let Some(rest) = content
            .strip_prefix('-')
            .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        {
            while parents.last().is_some_and(|(depth, _)| *depth >= indent) {
                parents.pop();
            }
            let item = rest.trim_start();
            indent += content.len() - item.len();
            content = item;
        }
        while parents.last().is_some_and(|(depth, _)| *depth >= indent) {
            parents.pop();
        }
        let Some((key, value)) = mapping_entry(content) else {
            continue;
        };
        let parent = parents.last().map(|(_, key)| key.as_str());
        let grandparent = parents
            .len()
            .checked_sub(2)
            .map(|index| parents[index].1.as_str());
        let flow = value.starts_with('{');
        let image =
            key == "image" && (parent == Some("container") || grandparent == Some("services"));
        let shorthand = key == "container" && !value.is_empty() && !flow;
        if image || shorthand {
            found.push((number + 1, value.trim_matches(['"', '\'']).to_owned()));
        }
        if flow && (matches!(key, "services" | "container") || parent == Some("services")) {
            for image in key_values(value, "image") {
                found.push((number + 1, image.to_owned()));
            }
        }
        parents.push((indent, key.to_owned()));
    }
    found
}

pub(crate) fn container_violations(name: &str, text: &str) -> Vec<String> {
    container_images(text)
        .into_iter()
        .filter(|(_, image)| !has_digest(image))
        .map(|(number, image)| {
            format!(
                "{name}:{number}: service image or container `{image}` must be pinned by digest"
            )
        })
        .collect()
}

/// The scopes a line of code grants `write` to, in block (`contents: write`)
/// or flow style (`permissions: { contents: write }`), quoted or not.
pub(crate) fn write_scopes(code: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    for (position, _) in code.match_indices("write") {
        let after = code[position + "write".len()..]
            .trim_start_matches(['"', '\''])
            .trim_start();
        if !(after.is_empty() || after.starts_with([',', '}'])) {
            continue;
        }
        let Some(before) = code[..position]
            .trim_end_matches(['"', '\''])
            .trim_end()
            .strip_suffix(':')
        else {
            continue;
        };
        let before = before.trim_end().trim_end_matches(['"', '\'']);
        let start = before
            .rfind(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            })
            .map_or(0, |index| index + 1);
        scopes.push(before[start..].to_owned());
    }
    scopes
}

/// The only scopes any workflow may write are the release job's `id-token`
/// and `packages`; checks, statuses, contents, actions... stay read.
pub(crate) fn permission_violations(name: &str, text: &str, release: bool) -> Vec<String> {
    let mut violations = Vec::new();
    if !text.lines().any(|line| line.starts_with("permissions:")) {
        violations.push(format!("{name}: no top-level permissions"));
    }
    if text.contains("pull_request_target") {
        violations.push(format!(
            "{name}: pull_request_target runs fork code with secrets"
        ));
    }
    if text.contains("write-all") {
        violations.push(format!("{name}: write access to everything"));
    }
    for (number, line) in text.lines().enumerate() {
        for scope in write_scopes(split_comment(line).0) {
            if !(release && matches!(scope.as_str(), "id-token" | "packages")) {
                violations.push(format!(
                    "{name}:{}: `{scope}: write` is not allowed",
                    number + 1
                ));
            }
        }
    }
    for permission in ["id-token: write", "packages: write"] {
        let count = text.matches(permission).count();
        if !((release && count == 1) || (!release && count == 0)) {
            violations.push(format!("{name}: `{permission}` appears {count} times"));
        }
    }
    violations
}

/// No workflow hands its secrets on wholesale or by computed name
/// (`secrets[`, `toJSON(secrets)`, `join(secrets, ...)`, `{ x: secrets }`,
/// `secrets: inherit`), in any case and spacing: expression contexts are
/// case-insensitive. The only secret any workflow reads is the release job's
/// token, spelled `secrets.GITHUB_TOKEN`.
pub(crate) fn secret_violations(name: &str, text: &str, release: bool) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut violations = Vec::new();
    for (position, _) in lower.match_indices("secrets") {
        if position > 0
            && (bytes[position - 1].is_ascii_alphanumeric() || bytes[position - 1] == b'_')
        {
            continue;
        }
        let rest = lower[position + "secrets".len()..].trim_start();
        let inherit = rest
            .strip_prefix(':')
            .is_some_and(|value| value.trim_start().starts_with("inherit"));
        if inherit || rest.starts_with(['[', ')', ',', '}']) {
            let shown: String = text[position..].chars().take(24).collect();
            violations.push(format!(
                "{name}: `{shown}` reaches secrets outside the reviewed list"
            ));
        } else if rest.starts_with('.') {
            // The reference as written: `secrets`, blanks, `.`, blanks, name.
            let dot = lower.len() - rest.len();
            let after_dot = &text[dot + 1..];
            let name_start = dot + 1 + (after_dot.len() - after_dot.trim_start().len());
            let name_end = text[name_start..]
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_'))
                })
                .map_or(text.len(), |offset| name_start + offset);
            let reference = &text[position..name_end];
            if !(release && reference == "secrets.GITHUB_TOKEN") {
                violations.push(format!("{name}: unexpected `{reference}`"));
            }
        }
    }
    violations
}

/// The one place outside the cargo-tool action that may install a cargo
/// tool: release.yml builds from a clean cache, so it installs
/// cargo-auditable itself, in this named step.
pub(crate) const RELEASE_INSTALL: (&str, &str) =
    (".github/workflows/release.yml", "Install cargo-auditable");
pub(crate) const TOOL_ACTION: &str = ".github/actions/cargo-tool/action.yml";

/// The logical lines of a file, each with the index of its first physical
/// line: `\`-continued lines joined with `glue`, and YAML comments removed
/// when `strip_comments`. The install rule reads every combination: a `#`
/// inside shell quotes is no comment, and a continuation inside a word
/// (`c\` + `argo`) joins without a space.
pub(crate) fn logical_lines(text: &str, strip_comments: bool, glue: &str) -> Vec<(usize, String)> {
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut continued = false;
    for (index, line) in text.lines().enumerate() {
        let code = if strip_comments {
            split_comment(line).0
        } else {
            line
        }
        .trim_end();
        let (code, continues) = match code.strip_suffix('\\') {
            Some(code) => (code, true),
            None => (code, false),
        };
        match lines.last_mut() {
            Some((_, joined)) if continued => {
                joined.push_str(glue);
                joined.push_str(if glue.is_empty() {
                    code.trim_start()
                } else {
                    code
                });
            }
            _ => lines.push((index, code.to_owned())),
        }
        continued = continues;
    }
    lines
}

/// The words of a logical line, with shell separators (`&&`, `||`, `;`,
/// `|`, `$(`, `(`, `)`, backquotes) as words of their own.
pub(crate) fn shell_words(code: &str) -> Vec<String> {
    const SEPARATORS: [&str; 8] = ["&&", "||", "$(", ";", "|", "(", ")", "`"];
    let mut spaced = String::with_capacity(code.len() * 2);
    let mut rest = code;
    while let Some(character) = rest.chars().next() {
        match SEPARATORS
            .iter()
            .find(|separator| rest.starts_with(**separator))
        {
            Some(separator) => {
                spaced.push_str(&format!(" {separator} "));
                rest = &rest[separator.len()..];
            }
            None => {
                spaced.push(character);
                rest = &rest[character.len_utf8()..];
            }
        }
    }
    // Quotes and backslashes change nothing a shell runs here: `ca"rgo"`,
    // `'install'` and `--g\it` are cargo, install and --git.
    spaced
        .split_whitespace()
        .map(|word| word.replace(['\'', '"', '\\'], ""))
        .filter(|word| !word.is_empty())
        .collect()
}

pub(crate) fn is_separator(word: &str) -> bool {
    matches!(word, "&&" | "||" | "$(" | ";" | "|" | "(" | ")" | "`")
}

/// `cargo` in any spelling a shell runs: a path to it, `$CARGO` or
/// `${CARGO...}`, quoted or not.
pub(crate) fn is_cargo_program(word: &str) -> bool {
    let word = word.trim_matches(['"', '\'']);
    // A variable can hold `cargo` (`c=cargo; $c install x`): a command whose
    // program is any variable counts as cargo, so it may not install either.
    let variable = word.strip_prefix('$').is_some_and(|name| {
        name.starts_with(|character: char| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '{')
        })
    });
    word == "cargo" || word.ends_with("/cargo") || variable
}

/// Whether a `cargo install --version` value is one exact `X.Y.Z` (an `=`
/// requirement allowed): `^`, `>=`, `~`, `*` and partial versions resolve to
/// whatever is newest.
pub(crate) fn is_exact_version(value: &str) -> bool {
    let value = value.trim_matches(['"', '\'']);
    is_numeric_version(value.strip_prefix('=').unwrap_or(value), 3..=3)
}

/// One cargo command that installs, as found in a logical line.
pub(crate) struct CargoInstall {
    /// The logical line's first physical line.
    line: usize,
    /// The whole command: `cargo`, its options, `install`, its arguments.
    words: Vec<String>,
    /// Whether the logical line holds nothing but `run:` and this command.
    alone: bool,
    /// The logical line as written, quotes included.
    raw: String,
}

/// Every cargo command of a file that installs: a word `install` or
/// `binstall` anywhere in a command whose program is cargo (options before
/// the subcommand included). A cargo command that ends its line is read on
/// into the next line, as a YAML-folded scalar would be.
pub(crate) fn cargo_installs(text: &str) -> Vec<CargoInstall> {
    let mut found: Vec<CargoInstall> = Vec::new();
    for (strip_comments, glue) in [(true, " "), (false, " "), (true, ""), (false, "")] {
        for install in cargo_installs_in(&logical_lines(text, strip_comments, glue)) {
            let known = found
                .iter()
                .any(|seen| seen.line == install.line && seen.words == install.words);
            if !known {
                found.push(install);
            }
        }
    }
    found
}

pub(crate) fn cargo_installs_in(lines: &[(usize, String)]) -> Vec<CargoInstall> {
    let mut found = Vec::new();
    for (at, (line, code)) in lines.iter().enumerate() {
        let words = shell_words(code);
        for (position, word) in words.iter().enumerate() {
            if !is_cargo_program(word) {
                continue;
            }
            let mut command: Vec<String> = words[position..]
                .iter()
                .take_while(|word| !is_separator(word))
                .cloned()
                .collect();
            let reaches_end = position + command.len() == words.len();
            let only_options = command[1..]
                .iter()
                .all(|word| word.starts_with('-') || word.starts_with('+'));
            if reaches_end && only_options {
                // A YAML-folded scalar continues the command on the next
                // line; a literal block starts a new one there. Read on only
                // when the next line goes on with the subcommand.
                if let Some((_, next)) = lines.get(at + 1) {
                    let next: Vec<String> = shell_words(next)
                        .into_iter()
                        .take_while(|word| !is_separator(word))
                        .collect();
                    let subcommand = next.iter().find(|word| {
                        !(word.starts_with('-') || word.starts_with('+') || word.contains('='))
                    });
                    if subcommand.is_some_and(|word| {
                        matches!(word.trim_matches(['"', '\'']), "install" | "binstall")
                    }) {
                        command.extend(next);
                    }
                }
            }
            let installs = command[1..]
                .iter()
                .any(|word| matches!(word.trim_matches(['"', '\'']), "install" | "binstall"));
            if installs {
                let prefix = &words[..position];
                let alone = (prefix.is_empty() || prefix == ["run:"] || prefix == ["-", "run:"])
                    && position + command.len() == words.len();
                found.push(CargoInstall {
                    line: *line,
                    words: command,
                    alone,
                    raw: code.clone(),
                });
            }
        }
    }
    found
}

/// Problems of an install at one of the two allowed sites: it must be a
/// plain `cargo install <crate>` alone in its `run`, with one exact
/// `--version` (the action's `"$VERSION"`, checked at each caller) and
/// `--locked`, from crates.io.
pub(crate) fn allowed_install_problems(
    install: &CargoInstall,
    in_tool_action: bool,
) -> Option<&'static str> {
    let words: Vec<&str> = install.words.iter().map(String::as_str).collect();
    if !install.alone || words.get(1) != Some(&"install") {
        return Some(
            "must be a plain `cargo install`, alone in its run, with no option before install",
        );
    }
    let arguments = &words[2..];
    let version =
        arguments
            .iter()
            .enumerate()
            .find_map(|(at, argument)| match argument.split_once('=') {
                Some(("--version" | "--vers", value)) => Some(value),
                _ if matches!(*argument, "--version" | "--vers") => arguments.get(at + 1).copied(),
                _ => None,
            });
    // The action passes its caller's checked version exactly as
    // `--version "$VERSION"`; no other spelling of it.
    let raw_version = install
        .raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair == ["--version", "\"$VERSION\""]);
    let exact = version.is_some_and(|value| {
        is_exact_version(value) || (in_tool_action && value == "$VERSION" && raw_version)
    });
    let source = arguments.iter().any(|argument| {
        let flag = argument.split_once('=').map_or(*argument, |(flag, _)| flag);
        matches!(
            flag,
            "--git" | "--path" | "--registry" | "--index" | "--config" | "-Z"
        )
    });
    if !exact || !arguments.contains(&"--locked") || source {
        return Some("needs one exact --version X.Y.Z and --locked, from crates.io");
    }
    // The release step installs cargo-auditable and nothing else: once the
    // version and --locked are set aside, that is the whole command.
    if !in_tool_action {
        let mut rest = Vec::new();
        let mut words = arguments.iter();
        while let Some(word) = words.next() {
            match *word {
                "--locked" => {}
                "--version" | "--vers" => {
                    words.next();
                }
                word if word.starts_with("--version=") || word.starts_with("--vers=") => {}
                word => rest.push(word),
            }
        }
        if rest != ["cargo-auditable"] {
            return Some("the release step installs exactly `cargo-auditable`");
        }
    }
    None
}

/// Cargo tools are installed only through the local cargo-tool action
/// (exact version, cached under it) and in one reviewed release.yml step;
/// any other install in a workflow or action is refused, however it is
/// spelled: a path to cargo, `$CARGO`, options before `install`,
/// `binstall`, `cargo-binstall`, a `CARGO_ALIAS_*` that renames `install`,
/// a `\`-continued or folded line. Probes that would skip an install
/// because some binary is already there (`command -v`, `which`, `hash`,
/// `type`, file tests on `~/.cargo/bin`) are refused too, and a cargo-tool
/// step needs an exact `version: X.Y.Z`. `relative` is the file's path in
/// the repository.
pub(crate) fn tool_install_violations(relative: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let in_tool_action = relative == TOOL_ACTION;
    let release_step: Vec<usize> = if relative == RELEASE_INSTALL.0 {
        lines
            .iter()
            .position(|line| key_values(split_comment(line).0, "name").contains(&RELEASE_INSTALL.1))
            .map(|index| {
                let step = step_lines(&lines, index);
                let start = lines
                    .iter()
                    .position(|line| std::ptr::eq(*line, step[0]))
                    .unwrap_or(index);
                (start..start + step.len()).collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut violations = Vec::new();
    for install in cargo_installs(text) {
        let shown = install.words.join(" ");
        let allowed = in_tool_action || release_step.contains(&install.line);
        let problem = if allowed {
            allowed_install_problems(&install, in_tool_action)
        } else {
            Some("cargo tools are installed only through .github/actions/cargo-tool")
        };
        if let Some(problem) = problem {
            violations.push(format!(
                "{relative}:{}: `{shown}`: {problem}",
                install.line + 1
            ));
        }
    }
    for (index, line) in lines.iter().enumerate() {
        let code = split_comment(line).0;
        let words: Vec<&str> = code.split_whitespace().collect();
        let mut refuse = |reason: &str| {
            violations.push(format!(
                "{relative}:{}: `{}`: {reason}",
                index + 1,
                line.trim()
            ));
        };
        if words.iter().any(|word| {
            let word = word.trim_matches(['"', '\'']);
            word == "cargo-binstall" || word.ends_with("/cargo-binstall")
        }) {
            refuse("cargo-binstall installs prebuilt binaries");
        }
        if code.contains("CARGO_ALIAS_") {
            refuse("a CARGO_ALIAS_* can rename install");
        }
        if line.contains("$'") {
            refuse("ANSI-C quoting ($'...') can spell any word; quote plainly");
        }
        let flat = words.join(" ");
        let probes = [
            "command -v cargo-",
            "which cargo-",
            "hash cargo-",
            "type cargo-",
            "type -p cargo-",
        ];
        let file_test = [" -x ", " -f ", " -e ", " -s "]
            .iter()
            .any(|test| flat.contains(test))
            && (flat.contains(".cargo/bin") || flat.contains("/cargo-"));
        if probes.iter().any(|probe| flat.contains(probe)) || file_test {
            refuse("a cached cargo tool is found by its version, not by a probe");
        }
        let tool = key_values(code, "uses")
            .iter()
            .any(|reference| reference.trim_end_matches('/') == "./.github/actions/cargo-tool");
        if tool {
            let exact = step_lines(&lines, index)
                .iter()
                .flat_map(|line| key_values(split_comment(line).0, "version"))
                .next()
                .is_some_and(|version| is_numeric_version(version, 3..=3));
            if !exact {
                refuse("the cargo-tool step needs an exact `version: X.Y.Z`");
            }
        }
    }
    violations
}

/// Environment variables that redirect where cargo, rustup and their tools
/// come from: a crate registry or source replacement (`CARGO_HOME` points
/// cargo at another `config.toml`), the install root, the network settings
/// and the toolchain download servers. A prefix ends in `_`;
/// `CARGO_SOURCE` and `CARGO_CONFIG` are prefixes too.
pub(crate) const REDIRECTING_VARIABLES: [&str; 9] = [
    "CARGO_HOME",
    "CARGO_REGISTRIES_",
    "CARGO_REGISTRY_",
    "CARGO_SOURCE",
    "CARGO_INSTALL_ROOT",
    "CARGO_NET_",
    "CARGO_CONFIG",
    "RUSTUP_DIST_SERVER",
    "RUSTUP_UPDATE_ROOT",
];

/// No gated file names a variable of `REDIRECTING_VARIABLES` anywhere: not
/// in an `env:` block (workflow, job or step), not in `run` text. Compared
/// without case. No gated file writes `$GITHUB_ENV` either: a write there
/// sets variables for every later step under a name the shell may build at
/// run time, where no text rule can read it.
pub(crate) fn environment_violations(name: &str, text: &str) -> Vec<String> {
    let upper = text.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut violations = Vec::new();
    for (position, _) in upper.match_indices("GITHUB_ENV") {
        let line = upper[..position].matches('\n').count() + 1;
        violations.push(format!(
            "{name}:{line}: gated files do not write `$GITHUB_ENV`; set variables in an env: block"
        ));
    }
    for variable in REDIRECTING_VARIABLES {
        for (position, _) in upper.match_indices(variable) {
            let starts_word = position == 0
                || !(bytes[position - 1].is_ascii_alphanumeric() || bytes[position - 1] == b'_');
            let end = position + variable.len();
            let ends_word = variable.ends_with('_')
                || matches!(variable, "CARGO_SOURCE" | "CARGO_CONFIG")
                || bytes
                    .get(end)
                    .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'));
            if starts_word && ends_word {
                let line = upper[..position].matches('\n').count() + 1;
                violations.push(format!(
                    "{name}:{line}: `{variable}` redirects where cargo or rustup fetch from"
                ));
            }
        }
    }
    violations
}
