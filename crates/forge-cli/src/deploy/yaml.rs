//! A strict reader for the YAML that `config/deploy.yml` is written in.
//!
//! Forge-Go decodes the file with `go.yaml.in/yaml/v3`; no YAML crate is in
//! this workspace's dependency graph, so this module reads the subset a
//! deploy configuration uses and refuses everything else with a line
//! number, never guessing:
//!
//! - block mappings and sequences (including `key:` followed by `- item`
//!   at the same indentation, and `- key: value` items);
//! - flow sequences and mappings (`[a, b]`, `{clear: {A: "1"}}`), which may
//!   span lines;
//! - plain, single-quoted and double-quoted scalars on one line;
//! - comments, one optional `---` document start and a `...` end.
//!
//! Anchors, aliases, tags, block scalars (`|`, `>`), complex keys (`?`),
//! directives, multi-line scalars and further documents are errors. Values
//! keep their text and whether they were quoted, so the decoder can apply
//! yaml.v3's typing rules (a plain `30` is an integer, `"30"` a string).

use std::fmt;

/// A parsed value and the line it starts on (1-based).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Node {
    pub(crate) value: Value,
    pub(crate) line: usize,
}

/// The shape of a node.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    /// An empty value, `~` or `null`.
    Null,
    /// A scalar's text; `plain` when it was not quoted.
    Scalar { text: String, plain: bool },
    /// A sequence.
    Seq(Vec<Node>),
    /// A mapping in document order; keys are unique.
    Map(Vec<(Node, Node)>),
}

/// A syntax error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyntaxError {
    pub(crate) line: usize,
    pub(crate) message: String,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "yaml: line {}: {}", self.line, self.message)
    }
}

fn error<T>(line: usize, message: impl Into<String>) -> Result<T, SyntaxError> {
    Err(SyntaxError {
        line,
        message: message.into(),
    })
}

/// A non-empty line without its comment.
#[derive(Clone, Debug)]
struct Line {
    number: usize,
    indent: usize,
    content: String,
}

/// The resolved YAML 1.2 core tag of a plain scalar (yaml.v3's rules).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Tag {
    Null,
    Bool,
    Int,
    Float,
    Str,
}

/// Classifies a plain scalar the way yaml.v3 resolves it.
pub(crate) fn resolve(text: &str) -> Tag {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Tag::Null,
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" => return Tag::Bool,
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => return Tag::Float,
        _ => {}
    }
    let first = text.as_bytes()[0];
    if !(first.is_ascii_digit() || matches!(first, b'+' | b'-' | b'.')) {
        return Tag::Str;
    }
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    if parse_go_int(&cleaned).is_some() {
        return Tag::Int;
    }
    if is_yaml_float(&cleaned) {
        return Tag::Float;
    }
    Tag::Str
}

/// Go's `strconv.ParseInt(s, 0, 64)` and yaml.v3's `0b`/`0o` forms.
pub(crate) fn parse_go_int(text: &str) -> Option<i64> {
    let (negative, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let (radix, body) = if let Some(rest) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (16, rest)
    } else if let Some(rest) = digits
        .strip_prefix("0o")
        .or_else(|| digits.strip_prefix("0O"))
    {
        (8, rest)
    } else if let Some(rest) = digits
        .strip_prefix("0b")
        .or_else(|| digits.strip_prefix("0B"))
    {
        (2, rest)
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    if body.is_empty() || !body.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let magnitude = i128::from_str_radix(body, radix).ok()?;
    i64::try_from(if negative { -magnitude } else { magnitude }).ok()
}

/// yaml.v3's float syntax: `^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$`.
fn is_yaml_float(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        index += 1;
    }
    let digits_from = |mut i: usize| {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        (i, i - start)
    };
    if bytes.get(index) == Some(&b'.') {
        let (next, count) = digits_from(index + 1);
        if count == 0 {
            return false;
        }
        index = next;
    } else {
        let (next, count) = digits_from(index);
        if count == 0 {
            return false;
        }
        index = next;
        if bytes.get(index) == Some(&b'.') {
            index = digits_from(index + 1).0;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let (next, count) = digits_from(index);
        if count == 0 {
            return false;
        }
        index = next;
    }
    index == bytes.len()
}

/// Parses one document. `Ok(None)` when it holds nothing but comments.
pub(crate) fn parse(text: &str) -> Result<Option<Node>, SyntaxError> {
    let lines = prepare(text)?;
    if lines.is_empty() {
        return Ok(None);
    }
    let mut parser = Parser { lines, index: 0 };
    let indent = parser.lines[0].indent;
    let node = parser.node(indent)?;
    if let Some(line) = parser.peek() {
        return error(
            line.number,
            if line.indent > 0 {
                "did not find expected key (bad indentation)"
            } else {
                "did not find expected key"
            },
        );
    }
    Ok(Some(node))
}

/// Splits `text` into content lines: comments and blank lines dropped,
/// document markers checked, tabs in indentation refused.
fn prepare(text: &str) -> Result<Vec<Line>, SyntaxError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = Vec::new();
    let mut started = false;
    let mut ended = false;
    for (index, raw) in text.split('\n').enumerate() {
        let number = index + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let rest = &raw[indent..];
        if rest.starts_with('\t') && !strip_comment(rest).trim().is_empty() {
            return error(number, "found a tab character that violates indentation");
        }
        let content = strip_comment(rest).trim_end().to_owned();
        if content.is_empty() {
            continue;
        }
        if indent == 0 && is_marker(&content, "---") {
            if started || ended {
                return error(
                    number,
                    "deploy config must contain exactly one YAML document",
                );
            }
            started = true;
            let after = content[3..].trim();
            if !after.is_empty() {
                return error(number, "content after --- is not supported");
            }
            continue;
        }
        if indent == 0 && is_marker(&content, "...") {
            ended = true;
            continue;
        }
        if indent == 0 && content.starts_with('%') {
            return error(number, "directives are not supported");
        }
        if ended {
            return error(
                number,
                "deploy config must contain exactly one YAML document",
            );
        }
        started = true;
        lines.push(Line {
            number,
            indent,
            content,
        });
    }
    Ok(lines)
}

fn is_marker(content: &str, marker: &str) -> bool {
    content == marker
        || (content.starts_with(marker) && content[marker.len()..].starts_with([' ', '\t']))
}

/// Removes a `#` comment: one at the start or after whitespace, outside
/// quotes.
fn strip_comment(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 1;
                    } else {
                        quote = None;
                    }
                }
            }
            Some(_) => {
                if byte == b'\\' {
                    index += 1;
                } else if byte == b'"' {
                    quote = None;
                }
            }
            None => {
                if byte == b'#'
                    && (index == 0 || bytes[index - 1] == b' ' || bytes[index - 1] == b'\t')
                {
                    return &text[..index];
                }
                // A quote opens a scalar only where a scalar can start.
                if (byte == b'"' || byte == b'\'')
                    && (index == 0
                        || matches!(
                            bytes[index - 1],
                            b' ' | b'\t' | b'[' | b'{' | b',' | b':' | b'-'
                        ))
                {
                    quote = Some(byte);
                }
            }
        }
        index += 1;
    }
    text
}

struct Parser {
    lines: Vec<Line>,
    index: usize,
}

/// Where a mapping key ends: the `:` followed by a space or the end.
fn key_split(content: &str) -> Option<(String, bool, &str)> {
    let bytes = content.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if bytes[0] == b'"' || bytes[0] == b'\'' {
        let (key, end) = quoted_prefix(content).ok()?;
        let rest = &content[end..];
        let rest = rest.trim_start_matches(' ');
        let after = rest.strip_prefix(':')?;
        if after.is_empty() || after.starts_with(' ') {
            return Some((key, false, after.trim_start()));
        }
        return None;
    }
    if matches!(
        bytes[0],
        b'[' | b'{' | b'-' | b'?' | b'&' | b'*' | b'!' | b'|' | b'>' | b'%' | b'@' | b'`'
    ) && !(bytes[0] == b'-' && bytes.len() > 1 && bytes[1] != b' ')
    {
        return None;
    }
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b':' && (index + 1 == bytes.len() || bytes[index + 1] == b' ') {
            let key = content[..index].trim_end().to_owned();
            return Some((key, true, content[index + 1..].trim_start()));
        }
        index += 1;
    }
    None
}

/// A quoted scalar at the start of `text`: its value and the byte offset
/// after the closing quote.
fn quoted_prefix(text: &str) -> Result<(String, usize), String> {
    let mut chars = text.char_indices();
    let Some((_, open)) = chars.next() else {
        return Err("expected a quoted scalar".to_owned());
    };
    let mut value = String::new();
    if open == '\'' {
        let bytes = text.as_bytes();
        let mut index = 1;
        let mut start = 1;
        while index < bytes.len() {
            if bytes[index] == b'\'' {
                value.push_str(&text[start..index]);
                if bytes.get(index + 1) == Some(&b'\'') {
                    value.push('\'');
                    index += 2;
                    start = index;
                    continue;
                }
                return Ok((value, index + 1));
            }
            index += 1;
        }
        return Err("found unexpected end of stream while scanning a quoted scalar (multi-line quoted scalars are not supported)".to_owned());
    }
    while let Some((index, character)) = chars.next() {
        match character {
            '"' => return Ok((value, index + 1)),
            '\\' => {
                let Some((_, escape)) = chars.next() else {
                    break;
                };
                let mut hex = |count: usize| -> Result<char, String> {
                    let mut code = 0_u32;
                    for _ in 0..count {
                        let digit = chars
                            .next()
                            .and_then(|(_, c)| c.to_digit(16))
                            .ok_or_else(|| "did not find expected hexdecimal number".to_owned())?;
                        code = code * 16 + digit;
                    }
                    char::from_u32(code)
                        .ok_or_else(|| "found invalid Unicode character escape code".to_owned())
                };
                let decoded = match escape {
                    '0' => '\0',
                    'a' => '\u{7}',
                    'b' => '\u{8}',
                    't' | '\t' => '\t',
                    'n' => '\n',
                    'v' => '\u{b}',
                    'f' => '\u{c}',
                    'r' => '\r',
                    'e' => '\u{1b}',
                    ' ' => ' ',
                    '"' => '"',
                    '/' => '/',
                    '\\' => '\\',
                    'N' => '\u{85}',
                    '_' => '\u{a0}',
                    'L' => '\u{2028}',
                    'P' => '\u{2029}',
                    'x' => hex(2)?,
                    'u' => hex(4)?,
                    'U' => hex(8)?,
                    _ => {
                        return Err(
                            "found unknown escape character while parsing a quoted scalar"
                                .to_owned(),
                        );
                    }
                };
                value.push(decoded);
            }
            c => value.push(c),
        }
    }
    Err("found unexpected end of stream while scanning a quoted scalar (multi-line quoted scalars are not supported)".to_owned())
}

/// Characters that cannot start a plain scalar here.
fn check_plain_start(text: &str, line: usize) -> Result<(), SyntaxError> {
    match text.as_bytes().first() {
        Some(b'&') => error(line, "anchors are not supported"),
        Some(b'*') => error(line, "aliases are not supported"),
        Some(b'!') => error(line, "tags are not supported"),
        Some(b'|' | b'>') => error(line, "block scalars (| and >) are not supported"),
        Some(b'?') if text.len() == 1 || text.as_bytes()[1] == b' ' => {
            error(line, "complex keys (?) are not supported")
        }
        Some(b'@' | b'`' | b'%') => error(line, "found character that cannot start any token"),
        _ => Ok(()),
    }
}

impl Parser {
    fn peek(&self) -> Option<&Line> {
        self.lines.get(self.index)
    }

    fn node(&mut self, indent: usize) -> Result<Node, SyntaxError> {
        let Some(line) = self.peek().cloned() else {
            return Ok(Node {
                value: Value::Null,
                line: self.lines.last().map_or(1, |l| l.number),
            });
        };
        if line.content == "-" || line.content.starts_with("- ") {
            return self.sequence(indent);
        }
        if key_split(&line.content).is_some() {
            return self.mapping(indent);
        }
        self.index += 1;
        let node = self.inline(&line.content, line.number)?;
        if let Some(next) = self.peek()
            && next.indent > indent
        {
            return error(next.number, "multi-line plain scalars are not supported");
        }
        Ok(node)
    }

    fn mapping(&mut self, indent: usize) -> Result<Node, SyntaxError> {
        let first_line = self.peek().map_or(1, |line| line.number);
        let mut entries: Vec<(Node, Node)> = Vec::new();
        while let Some(line) = self.peek().cloned() {
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return error(
                    line.number,
                    "mapping values are not allowed in this context (bad indentation)",
                );
            }
            let Some((key, plain, rest)) = key_split(&line.content) else {
                if line.content == "-" || line.content.starts_with("- ") {
                    return error(line.number, "did not find expected key");
                }
                return error(line.number, "could not find expected ':'");
            };
            let rest = rest.to_owned();
            if plain {
                check_plain_start(&key, line.number)?;
            }
            if let Some((previous, _)) = entries
                .iter()
                .find(|(existing, _)| scalar_text(existing) == Some(&key))
            {
                return error(
                    line.number,
                    format!(
                        "mapping key {key:?} already defined at line {}",
                        previous.line
                    ),
                );
            }
            let key_node = Node {
                value: Value::Scalar { text: key, plain },
                line: line.number,
            };
            self.index += 1;
            let value = if rest.is_empty() {
                match self.peek() {
                    Some(next)
                        if next.indent > indent
                            || (next.indent == indent
                                && (next.content == "-" || next.content.starts_with("- "))) =>
                    {
                        let child = next.indent;
                        self.node(child)?
                    }
                    _ => Node {
                        value: Value::Null,
                        line: line.number,
                    },
                }
            } else {
                let value = self.inline_with_continuation(&rest, line.number)?;
                if let Some(next) = self.peek()
                    && next.indent > indent
                {
                    return error(next.number, "multi-line plain scalars are not supported");
                }
                value
            };
            entries.push((key_node, value));
        }
        Ok(Node {
            value: Value::Map(entries),
            line: first_line,
        })
    }

    fn sequence(&mut self, indent: usize) -> Result<Node, SyntaxError> {
        let first_line = self.peek().map_or(1, |line| line.number);
        let mut items = Vec::new();
        while let Some(line) = self.peek().cloned() {
            if line.indent != indent || !(line.content == "-" || line.content.starts_with("- ")) {
                if line.indent > indent {
                    return error(line.number, "did not find expected '-' indicator");
                }
                break;
            }
            let rest = line.content[1..].trim_start();
            if rest.is_empty() {
                self.index += 1;
                match self.peek() {
                    Some(next) if next.indent > indent => {
                        let child = next.indent;
                        items.push(self.node(child)?);
                    }
                    _ => items.push(Node {
                        value: Value::Null,
                        line: line.number,
                    }),
                }
                continue;
            }
            let offset = line.content.len() - rest.len();
            if rest == "-" || rest.starts_with("- ") || key_split(rest).is_some() {
                // A compact nested collection: re-read the rest of the line
                // as if it started on its own line at its column.
                let child = line.indent + offset;
                self.lines[self.index] = Line {
                    number: line.number,
                    indent: child,
                    content: rest.to_owned(),
                };
                items.push(self.node(child)?);
                continue;
            }
            self.index += 1;
            let item = self.inline_with_continuation(rest, line.number)?;
            if let Some(next) = self.peek()
                && next.indent > indent
            {
                return error(next.number, "multi-line plain scalars are not supported");
            }
            items.push(item);
        }
        Ok(Node {
            value: Value::Seq(items),
            line: first_line,
        })
    }

    /// An inline value; a flow collection may continue on following lines.
    fn inline_with_continuation(&mut self, text: &str, line: usize) -> Result<Node, SyntaxError> {
        if !(text.starts_with('[') || text.starts_with('{')) {
            return self.inline(text, line);
        }
        let mut joined = text.to_owned();
        while !flow_closed(&joined) {
            let Some(next) = self.peek().cloned() else {
                return error(
                    line,
                    "did not find expected ',' or ']' (unclosed flow collection)",
                );
            };
            joined.push(' ');
            joined.push_str(&next.content);
            self.index += 1;
        }
        self.inline(&joined, line)
    }

    fn inline(&self, text: &str, line: usize) -> Result<Node, SyntaxError> {
        if text.starts_with('[') || text.starts_with('{') {
            let mut flow = Flow {
                text,
                position: 0,
                line,
            };
            let node = flow.value()?;
            flow.skip_spaces();
            if flow.position != text.len() {
                return error(
                    line,
                    "did not find expected <document end> after flow collection",
                );
            }
            return Ok(node);
        }
        if text.starts_with('"') || text.starts_with('\'') {
            let (value, end) =
                quoted_prefix(text).map_err(|message| SyntaxError { line, message })?;
            if !text[end..].trim().is_empty() {
                return error(line, "did not find expected key after a quoted scalar");
            }
            return Ok(Node {
                value: Value::Scalar {
                    text: value,
                    plain: false,
                },
                line,
            });
        }
        if text == "-" || text.starts_with("- ") {
            return error(
                line,
                "block sequence entries are not allowed in this context",
            );
        }
        check_plain_start(text, line)?;
        if key_split(text).is_some() {
            return error(line, "mapping values are not allowed in this context");
        }
        Ok(plain_node(text.trim(), line))
    }
}

fn plain_node(text: &str, line: usize) -> Node {
    let value = if resolve(text) == Tag::Null {
        Value::Null
    } else {
        Value::Scalar {
            text: text.to_owned(),
            plain: true,
        }
    };
    Node { value, line }
}

fn scalar_text(node: &Node) -> Option<&String> {
    match &node.value {
        Value::Scalar { text, .. } => Some(text),
        _ => None,
    }
}

/// Whether the brackets of a flow collection are balanced (quotes aware).
fn flow_closed(text: &str) -> bool {
    let mut depth = 0_i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in text.chars() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if q == '"' && character == '\\' {
                    escaped = true;
                } else if character == q {
                    quote = None;
                }
            }
            None => match character {
                '"' | '\'' => quote = Some(character),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ => {}
            },
        }
    }
    depth <= 0 && quote.is_none()
}

/// A flow collection on one (joined) line.
struct Flow<'a> {
    text: &'a str,
    position: usize,
    line: usize,
}

impl Flow<'_> {
    fn rest(&self) -> &str {
        &self.text[self.position..]
    }

    fn skip_spaces(&mut self) {
        let rest = self.rest();
        self.position += rest.len() - rest.trim_start().len();
    }

    fn fail<T>(&self, message: &str) -> Result<T, SyntaxError> {
        error(self.line, message)
    }

    fn value(&mut self) -> Result<Node, SyntaxError> {
        self.skip_spaces();
        let line = self.line;
        match self.rest().as_bytes().first() {
            Some(b'[') => {
                self.position += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_spaces();
                    if self.rest().starts_with(']') {
                        self.position += 1;
                        break;
                    }
                    let item = self.value()?;
                    self.skip_spaces();
                    // `[a: b]` is a single-pair mapping.
                    let item = if self.rest().starts_with(':') {
                        self.position += 1;
                        let value = self.optional_value(&[',', ']'])?;
                        Node {
                            value: Value::Map(vec![(item, value)]),
                            line,
                        }
                    } else {
                        item
                    };
                    items.push(item);
                    self.skip_spaces();
                    if self.rest().starts_with(',') {
                        self.position += 1;
                    } else if self.rest().starts_with(']') {
                        self.position += 1;
                        break;
                    } else {
                        return self.fail("did not find expected ',' or ']'");
                    }
                }
                Ok(Node {
                    value: Value::Seq(items),
                    line,
                })
            }
            Some(b'{') => {
                self.position += 1;
                let mut entries: Vec<(Node, Node)> = Vec::new();
                loop {
                    self.skip_spaces();
                    if self.rest().starts_with('}') {
                        self.position += 1;
                        break;
                    }
                    let key = self.value()?;
                    let Some(key_text) = scalar_text(&key)
                        .cloned()
                        .or_else(|| (key.value == Value::Null).then(String::new))
                    else {
                        return self.fail("flow mapping keys must be scalars");
                    };
                    if entries.iter().any(|(existing, _)| {
                        scalar_text(existing).cloned().unwrap_or_default() == key_text
                    }) {
                        return error(
                            line,
                            format!("mapping key {key_text:?} already defined at line {line}"),
                        );
                    }
                    self.skip_spaces();
                    let value = if self.rest().starts_with(':') {
                        self.position += 1;
                        self.optional_value(&[',', '}'])?
                    } else {
                        Node {
                            value: Value::Null,
                            line,
                        }
                    };
                    entries.push((key, value));
                    self.skip_spaces();
                    if self.rest().starts_with(',') {
                        self.position += 1;
                    } else if self.rest().starts_with('}') {
                        self.position += 1;
                        break;
                    } else {
                        return self.fail("did not find expected ',' or '}'");
                    }
                }
                Ok(Node {
                    value: Value::Map(entries),
                    line,
                })
            }
            Some(b'"' | b'\'') => {
                let (value, end) =
                    quoted_prefix(self.rest()).map_err(|message| SyntaxError { line, message })?;
                self.position += end;
                Ok(Node {
                    value: Value::Scalar {
                        text: value,
                        plain: false,
                    },
                    line,
                })
            }
            Some(_) => {
                let rest = self.rest();
                check_plain_start(rest, line)?;
                let bytes = rest.as_bytes();
                let mut end = 0;
                while end < bytes.len() {
                    let byte = bytes[end];
                    if matches!(byte, b',' | b'[' | b']' | b'{' | b'}') {
                        break;
                    }
                    if byte == b':'
                        && (end + 1 == bytes.len()
                            || matches!(bytes[end + 1], b' ' | b',' | b']' | b'}'))
                    {
                        break;
                    }
                    end += 1;
                }
                let node = plain_node(rest[..end].trim(), line);
                self.position += end;
                Ok(node)
            }
            None => self.fail("did not find expected node content"),
        }
    }

    /// The value after `:`, or null when the entry ends there.
    fn optional_value(&mut self, terminators: &[char]) -> Result<Node, SyntaxError> {
        self.skip_spaces();
        if self.rest().is_empty() || terminators.iter().any(|t| self.rest().starts_with(*t)) {
            return Ok(Node {
                value: Value::Null,
                line: self.line,
            });
        }
        self.value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(text: &str, plain: bool) -> Value {
        Value::Scalar {
            text: text.to_owned(),
            plain,
        }
    }

    fn map(node: &Node) -> Vec<(String, Value)> {
        match &node.value {
            Value::Map(entries) => entries
                .iter()
                .map(|(k, v)| (scalar_text(k).cloned().unwrap_or_default(), v.value.clone()))
                .collect(),
            other => panic!("not a map: {other:?}"),
        }
    }

    fn parse_one(text: &str) -> Node {
        parse(text).expect("valid").expect("document")
    }

    #[test]
    fn block_and_flow_collections() {
        let node = parse_one(
            "# comment\nservice: shop # trailing\nservers:\n  web:\n    hosts: [192.0.2.10, \"192.0.2.11\"]\n  jobs:\n    hosts:\n    - a\n    -  b\n    cmd: ['it''s', \"a\\tb\"]\nenv: {clear: {A: \"1\", B: 2}, secret: []}\nempty:\nlist:\n  - x: 1\n    y: 2\n  - - nested\n",
        );
        let top = map(&node);
        assert_eq!(top[0], ("service".into(), scalar("shop", true)));
        let Value::Map(servers) = &top[1].1 else {
            panic!("servers")
        };
        let web = map(&servers[0].1);
        let Value::Seq(hosts) = &web[0].1 else {
            panic!("hosts")
        };
        assert_eq!(hosts[1].value, scalar("192.0.2.11", false));
        let jobs = map(&servers[1].1);
        let Value::Seq(hosts) = &jobs[0].1 else {
            panic!("compact hosts")
        };
        assert_eq!(hosts.len(), 2);
        let Value::Seq(cmd) = &jobs[1].1 else {
            panic!("cmd")
        };
        assert_eq!(cmd[0].value, scalar("it's", false));
        assert_eq!(cmd[1].value, scalar("a\tb", false));
        let env = &top[2].1;
        let Value::Map(env) = env else { panic!("env") };
        assert_eq!(map(&env[0].1)[1], ("B".into(), scalar("2", true)));
        assert_eq!(top[3].1, Value::Null);
        let Value::Seq(list) = &top[4].1 else {
            panic!("list")
        };
        assert_eq!(map(&list[0]).len(), 2);
        assert!(matches!(&list[1].value, Value::Seq(inner) if inner.len() == 1));
    }

    #[test]
    fn plain_scalars_keep_their_text() {
        let top = map(&parse_one(
            "a: a#b\nb: :x\nc: -x\nd: 1.5\ne: ~\nf: http://x:80/y\ng: [a:b]\nh: {a:b}\n",
        ));
        assert_eq!(top[0].1, scalar("a#b", true));
        assert_eq!(top[1].1, scalar(":x", true));
        assert_eq!(top[2].1, scalar("-x", true));
        assert_eq!(top[3].1, scalar("1.5", true));
        assert_eq!(top[4].1, Value::Null);
        assert_eq!(top[5].1, scalar("http://x:80/y", true));
        assert!(matches!(&top[6].1, Value::Seq(items) if items[0].value == scalar("a:b", true)));
        assert!(matches!(&top[7].1, Value::Map(entries) if entries[0].1.value == Value::Null));
    }

    #[test]
    fn multi_line_flow_collections() {
        let top = map(&parse_one("l: [\n  a,\n  b # c\n]\nm: {x: 1,\n  y: 2}\n"));
        assert!(matches!(&top[0].1, Value::Seq(items) if items.len() == 2));
        assert!(matches!(&top[1].1, Value::Map(entries) if entries.len() == 2));
    }

    #[test]
    fn documents() {
        assert_eq!(parse("").expect("empty"), None);
        assert_eq!(parse("# only\n\n").expect("comments"), None);
        assert!(parse("---\na: 1\n...\n").expect("markers").is_some());
        for text in ["a: 1\n---\nb: 2\n", "---\na: 1\n---\n", "a: 1\n...\nb: 2\n"] {
            let error = parse(text).expect_err("two documents");
            assert!(
                error.message.contains("exactly one YAML document"),
                "{error}"
            );
        }
    }

    #[test]
    fn refuses_what_it_does_not_support() {
        for (text, message) in [
            ("a: &x 1\n", "anchors"),
            ("a: *x\n", "aliases"),
            ("a: !!str 5\n", "tags"),
            ("a: |\n  x\n", "block scalars"),
            ("a: >\n  x\n", "block scalars"),
            ("? a\n: b\n", "complex keys"),
            ("%YAML 1.2\n---\na: 1\n", "directives"),
            ("a: x\n  y\n", "multi-line plain"),
            ("a: \"x\n  y\"\n", "multi-line quoted"),
            ("a: x\n\tb: 1\n", "tab character"),
            ("a: b: c\n", "mapping values are not allowed"),
            ("a: - x\n", "block sequence entries"),
            ("a: @x\n", "cannot start any token"),
            (
                "a: 1\na: 2\n",
                "mapping key \"a\" already defined at line 1",
            ),
            ("m: {a: 1, a: 2}\n", "already defined"),
            ("a: [1, 2\n", "unclosed"),
            ("a: \"\\q\"\n", "unknown escape"),
            ("a:\n  b: 1\n c: 2\n", "bad indentation"),
        ] {
            let error = parse(text).expect_err(text);
            assert!(error.to_string().contains(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn resolves_tags_like_yaml_v3() {
        for (text, tag) in [
            ("", Tag::Null),
            ("~", Tag::Null),
            ("NULL", Tag::Null),
            ("true", Tag::Bool),
            ("False", Tag::Bool),
            ("yes", Tag::Str),
            ("5432", Tag::Int),
            ("0x10", Tag::Int),
            ("1_000", Tag::Int),
            ("017", Tag::Int),
            ("0o17", Tag::Int),
            ("0b101", Tag::Int),
            ("+5", Tag::Int),
            ("-1", Tag::Int),
            ("1.5", Tag::Float),
            ("1e3", Tag::Float),
            (".5", Tag::Float),
            (".inf", Tag::Float),
            ("30s", Tag::Str),
            ("1m0s", Tag::Str),
            ("5432:5432", Tag::Str),
            ("-x", Tag::Str),
            ("192.0.2.10", Tag::Str),
            ("08", Tag::Float),
        ] {
            assert_eq!(resolve(text), tag, "{text:?}");
        }
    }
}
