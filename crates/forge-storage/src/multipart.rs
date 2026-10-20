//! A strict `multipart/form-data` parser over a complete in-memory body.
//!
//! `forge-http` buffers each request body up to `FORGE_MAX_BODY_BYTES` before a
//! handler runs, so the parser works on a slice and returns *ranges* into it:
//! file contents are never copied. It is deliberately stricter than the RFCs
//! allow, because every deviation from what browsers send is an attack surface:
//!
//! - the body starts with the first delimiter (no preamble) and ends with the
//!   closing delimiter, optionally followed by one CRLF (no epilogue);
//! - delimiter lines carry no transport padding;
//! - a part has at most [`MAX_HEADERS`] header lines of at most
//!   [`MAX_HEADER_LINE`] bytes, no folded lines, no bare CR or LF, no NUL;
//! - exactly one `Content-Disposition: form-data` with a `name`, at most one
//!   `Content-Type` and one `Content-Transfer-Encoding`, and no repeated
//!   disposition parameter;
//! - the declared transfer encoding must be an identity encoding.
//!
//! A part ends at the first CRLF `--boundary`, as the specification defines.

use std::ops::Range;

use crate::StorageError;

/// Longest boundary the RFC allows.
const MAX_BOUNDARY: usize = 70;
/// Most header lines per part.
pub(crate) const MAX_HEADERS: usize = 16;
/// Longest header line, in bytes.
pub(crate) const MAX_HEADER_LINE: usize = 8192;
/// Longest field name, in bytes.
pub(crate) const MAX_NAME: usize = 256;

fn invalid(reason: &'static str) -> StorageError {
    StorageError::InvalidUpload(reason)
}

/// One part of the body.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Part {
    /// The form field name.
    pub(crate) name: String,
    /// The client's file name; `None` for a text field (an empty `filename` is
    /// also `None`, like a browser's "no file chosen").
    pub(crate) filename: Option<String>,
    /// The part's declared `Content-Type`, if any.
    pub(crate) content_type: Option<String>,
    /// Where the content lies in the body.
    pub(crate) content: Range<usize>,
}

/// Extracts and validates the boundary from a request `Content-Type`.
pub(crate) fn boundary(content_type: &str) -> Result<String, StorageError> {
    let mut pieces = split_parameters(content_type)?;
    let media = pieces.remove(0);
    if !media.trim().eq_ignore_ascii_case("multipart/form-data") {
        return Err(StorageError::NotMultipart);
    }
    let mut found = None;
    for piece in pieces {
        let Some((name, value)) = piece.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("boundary") {
            if found.is_some() {
                return Err(StorageError::NotMultipart);
            }
            found = Some(unquote(value.trim())?);
        }
    }
    let boundary = found.ok_or(StorageError::NotMultipart)?;
    let valid = !boundary.is_empty()
        && boundary.len() <= MAX_BOUNDARY
        && !boundary.ends_with(' ')
        && boundary.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'\''
                        | b'('
                        | b')'
                        | b'+'
                        | b'_'
                        | b','
                        | b'-'
                        | b'.'
                        | b'/'
                        | b':'
                        | b'='
                        | b'?'
                        | b' '
                )
        });
    if valid {
        Ok(boundary)
    } else {
        Err(StorageError::NotMultipart)
    }
}

/// Splits `a; b="c;d"; e` on semicolons outside quotes.
fn split_parameters(value: &str) -> Result<Vec<&str>, StorageError> {
    let mut pieces = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (index, character) in value.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                pieces.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted {
        return Err(invalid("unterminated quoted string in a multipart header"));
    }
    pieces.push(&value[start..]);
    Ok(pieces)
}

/// A token or a quoted string with backslash escapes.
fn unquote(value: &str) -> Result<String, StorageError> {
    let Some(inner) = value.strip_prefix('"') else {
        return Ok(value.to_owned());
    };
    let inner = inner
        .strip_suffix('"')
        .ok_or_else(|| invalid("malformed quoted string in a multipart header"))?;
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for character in inner.chars() {
        match character {
            _ if escaped => {
                out.push(character);
                escaped = false;
            }
            '\\' => escaped = true,
            '"' => return Err(invalid("malformed quoted string in a multipart header")),
            _ => out.push(character),
        }
    }
    if escaped {
        return Err(invalid("malformed quoted string in a multipart header"));
    }
    Ok(out)
}

/// RFC 5987 `filename*=UTF-8''percent-encoded`.
fn decode_extended(value: &str) -> Result<String, StorageError> {
    let encoded = value
        .strip_prefix("UTF-8''")
        .or_else(|| value.strip_prefix("utf-8''"))
        .ok_or_else(|| invalid("unsupported filename encoding in a multipart header"))?;
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .and_then(|pair| std::str::from_utf8(pair).ok())
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| invalid("malformed filename encoding in a multipart header"))?;
            out.push(hex);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| invalid("malformed filename encoding in a multipart header"))
}

struct Disposition {
    name: String,
    filename: Option<String>,
}

fn parse_disposition(value: &str) -> Result<Disposition, StorageError> {
    let mut pieces = split_parameters(value)?.into_iter();
    let kind = pieces.next().unwrap_or("").trim();
    if !kind.eq_ignore_ascii_case("form-data") {
        return Err(invalid("multipart part is not form-data"));
    }
    let (mut name, mut filename, mut filename_extended) = (None, None, None);
    for piece in pieces {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (parameter, raw) = piece
            .split_once('=')
            .ok_or_else(|| invalid("malformed parameter in a multipart header"))?;
        let parameter = parameter.trim().to_ascii_lowercase();
        let slot = match parameter.as_str() {
            "name" => &mut name,
            "filename" => &mut filename,
            "filename*" => &mut filename_extended,
            _ => continue,
        };
        if slot.is_some() {
            return Err(invalid("repeated parameter in a multipart header"));
        }
        let raw = raw.trim();
        *slot = Some(if parameter == "filename*" {
            decode_extended(raw)?
        } else {
            unquote(raw)?
        });
    }
    let name = name.filter(|name| !name.is_empty() && name.len() <= MAX_NAME);
    let name = name.ok_or_else(|| invalid("multipart part without a valid field name"))?;
    // The extended form wins when both are present, as in RFC 6266.
    let filename = filename_extended
        .or(filename)
        .filter(|name| !name.is_empty());
    Ok(Disposition { name, filename })
}

/// Parses `body` into its parts, or fails on the first deviation.
pub(crate) fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>, StorageError> {
    let delimiter = [b"--".as_slice(), boundary.as_bytes()].concat();
    let separator = [b"\r\n".as_slice(), &delimiter].concat();
    let mut parts = Vec::new();

    let mut position = expect(body, 0, &delimiter)?;
    loop {
        // After a delimiter: "--" closes the body, CRLF opens a part.
        if body[position..].starts_with(b"--") {
            let rest = &body[position + 2..];
            if rest.is_empty() || rest == b"\r\n" {
                return Ok(parts);
            }
            return Err(invalid("data after the closing multipart delimiter"));
        }
        position = expect(body, position, b"\r\n")?;

        let (headers_end, headers) = parse_headers(body, position)?;
        position = headers_end;
        let content_end = find(body, position, &separator)
            .ok_or_else(|| invalid("the multipart body is not terminated"))?;
        let content = position..content_end;
        position = content_end + separator.len();

        let mut disposition = None;
        let (mut content_type, mut encoding) = (None, None::<String>);
        for (name, value) in headers {
            match name.to_ascii_lowercase().as_str() {
                "content-disposition" => {
                    if disposition.replace(parse_disposition(&value)?).is_some() {
                        return Err(invalid("repeated Content-Disposition in a multipart part"));
                    }
                }
                "content-type" => {
                    if content_type.replace(value).is_some() {
                        return Err(invalid("repeated Content-Type in a multipart part"));
                    }
                }
                "content-transfer-encoding" => {
                    if encoding.replace(value).is_some() {
                        return Err(invalid(
                            "repeated Content-Transfer-Encoding in a multipart part",
                        ));
                    }
                }
                _ => {}
            }
        }
        if let Some(encoding) = encoding {
            let encoding = encoding.trim();
            if !["binary", "8bit", "7bit"]
                .iter()
                .any(|identity| encoding.eq_ignore_ascii_case(identity))
            {
                return Err(invalid("encoded multipart parts are not supported"));
            }
        }
        let disposition =
            disposition.ok_or_else(|| invalid("multipart part without Content-Disposition"))?;
        parts.push(Part {
            name: disposition.name,
            filename: disposition.filename,
            content_type,
            content,
        });
    }
}

fn expect(body: &[u8], position: usize, literal: &[u8]) -> Result<usize, StorageError> {
    if body[position..].starts_with(literal) {
        Ok(position + literal.len())
    } else {
        Err(invalid("the multipart body is malformed"))
    }
}

fn find(body: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let first = *needle.first()?;
    let mut start = from;
    while let Some(offset) = body.get(start..)?.iter().position(|&byte| byte == first) {
        let at = start + offset;
        if body[at..].starts_with(needle) {
            return Some(at);
        }
        start = at + 1;
    }
    None
}

type Header = (String, String);

/// Header lines up to the blank line; returns the position after it.
fn parse_headers(body: &[u8], start: usize) -> Result<(usize, Vec<Header>), StorageError> {
    let mut headers = Vec::new();
    let mut position = start;
    loop {
        let line_end = find(body, position, b"\r\n")
            .ok_or_else(|| invalid("the multipart part headers are not terminated"))?;
        let line = &body[position..line_end];
        position = line_end + 2;
        if line.is_empty() {
            return Ok((position, headers));
        }
        if headers.len() >= MAX_HEADERS || line.len() > MAX_HEADER_LINE {
            return Err(invalid("multipart part headers are too large"));
        }
        if line
            .iter()
            .any(|&byte| byte == b'\r' || byte == b'\n' || byte == 0)
        {
            return Err(invalid("malformed multipart header line"));
        }
        if matches!(line[0], b' ' | b'\t') {
            return Err(invalid("folded multipart header lines are not supported"));
        }
        let text = std::str::from_utf8(line)
            .map_err(|_| invalid("multipart header is not valid UTF-8"))?;
        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| invalid("malformed multipart header line"))?;
        let token = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !token {
            return Err(invalid("malformed multipart header name"));
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const BOUNDARY: &str = "----forge-test-boundary";

    /// A part for [`encode`].
    #[derive(Clone, Default)]
    pub(crate) struct Spec {
        pub(crate) field: String,
        pub(crate) filename: Option<String>,
        pub(crate) content_type: Option<String>,
        pub(crate) content: Vec<u8>,
        pub(crate) headers: Vec<(String, String)>,
    }

    impl Spec {
        pub(crate) fn text(field: &str, value: &str) -> Self {
            Self {
                field: field.into(),
                content: value.as_bytes().to_vec(),
                ..Self::default()
            }
        }

        pub(crate) fn file(
            field: &str,
            filename: &str,
            content_type: &str,
            content: &[u8],
        ) -> Self {
            Self {
                field: field.into(),
                filename: Some(filename.into()),
                content_type: (!content_type.is_empty()).then(|| content_type.into()),
                content: content.to_vec(),
                headers: Vec::new(),
            }
        }
    }

    /// Encodes specs the way a browser does.
    pub(crate) fn encode(specs: &[Spec]) -> Vec<u8> {
        let mut body = Vec::new();
        for spec in specs {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            let mut disposition =
                format!("Content-Disposition: form-data; name=\"{}\"", spec.field);
            if let Some(filename) = &spec.filename {
                disposition.push_str(&format!("; filename=\"{filename}\""));
            }
            body.extend_from_slice(format!("{disposition}\r\n").as_bytes());
            if let Some(content_type) = &spec.content_type {
                body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
            }
            for (name, value) in &spec.headers {
                body.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(&spec.content);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    #[test]
    fn parses_what_a_browser_sends_into_ranges_of_the_body() {
        let png = b"\x89PNG\r\n\x1a\n\r\n--not-the-boundary\r\n";
        let body = encode(&[
            Spec::text("name", "Ana"),
            Spec::file("avatar", "me.png", "image/png", png),
            Spec::file("empty", "", "application/octet-stream", b""),
        ]);
        let parts = parse(&body, BOUNDARY).expect("parse");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].name, "name");
        assert_eq!(parts[0].filename, None);
        assert_eq!(&body[parts[0].content.clone()], b"Ana");
        assert_eq!(parts[1].filename.as_deref(), Some("me.png"));
        assert_eq!(parts[1].content_type.as_deref(), Some("image/png"));
        assert_eq!(
            &body[parts[1].content.clone()],
            png,
            "binary content with CR LF and dashes is intact"
        );
        assert_eq!(
            parts[2].filename, None,
            "an empty filename is a text field, like a browser's no file chosen"
        );
    }

    #[test]
    fn the_closing_delimiter_may_omit_the_trailing_crlf() {
        let mut body = encode(&[Spec::text("a", "1")]);
        body.truncate(body.len() - 2);
        assert_eq!(parse(&body, BOUNDARY).expect("parse").len(), 1);
    }

    #[test]
    fn quoted_names_escapes_and_extended_file_names() {
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"a\\\"b\"; filename=\"x;y.png\"\r\n\r\nz\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=f; filename*=UTF-8''%C3%A9.png\r\n\r\nz\r\n--{BOUNDARY}--"
        );
        let parts = parse(body.as_bytes(), BOUNDARY).expect("parse");
        assert_eq!(parts[0].name, "a\"b");
        assert_eq!(parts[0].filename.as_deref(), Some("x;y.png"));
        assert_eq!(parts[1].filename.as_deref(), Some("é.png"));
    }

    #[test]
    fn every_deviation_is_refused() {
        let good = encode(&[Spec::text("a", "1")]);
        let b = BOUNDARY;
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty body", Vec::new()),
            ("preamble", [b"junk\r\n".as_slice(), &good].concat()),
            ("epilogue", [good.as_slice(), b"junk"].concat()),
            ("no closing delimiter", good[..good.len() - format!("--{b}--\r\n").len()].to_vec()),
            ("transport padding", format!("--{b} \r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("bare LF line ends", format!("--{b}\nContent-Disposition: form-data; name=\"a\"\n\n1\n--{b}--").into_bytes()),
            ("missing disposition", format!("--{b}\r\nContent-Type: text/plain\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("disposition not form-data", format!("--{b}\r\nContent-Disposition: attachment; name=\"a\"\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("no name", format!("--{b}\r\nContent-Disposition: form-data\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("empty name", format!("--{b}\r\nContent-Disposition: form-data; name=\"\"\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("repeated name parameter", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\"; name=\"b\"\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("two dispositions", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Disposition: form-data; name=\"b\"\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("two content types", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Type: a/b\r\nContent-Type: c/d\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("folded header", format!("--{b}\r\nContent-Disposition: form-data;\r\n name=\"a\"\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("NUL in header", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\0\"\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("header without colon", format!("--{b}\r\nContent-Disposition form-data\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("base64 transfer encoding", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Transfer-Encoding: base64\r\n\r\nMQ==\r\n--{b}--").into_bytes()),
            ("quoted-printable", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("unterminated quote", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("unsupported filename charset", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\"; filename*=iso-8859-1''x\r\n\r\n1\r\n--{b}--").into_bytes()),
            ("headers never end", format!("--{b}\r\nContent-Disposition: form-data; name=\"a\"").into_bytes()),
            ("garbage after delimiter", format!("--{b}xyz\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--{b}--").into_bytes()),
        ];
        for (name, body) in cases {
            assert!(
                matches!(parse(&body, BOUNDARY), Err(StorageError::InvalidUpload(_))),
                "{name}: {:?}",
                parse(&body, BOUNDARY).map(|p| p.len())
            );
        }
        assert!(parse(&good, BOUNDARY).is_ok());
    }

    #[test]
    fn header_budgets_are_enforced() {
        let b = BOUNDARY;
        let many: String = (0..=MAX_HEADERS).map(|i| format!("X-{i}: v\r\n")).collect();
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"a\"\r\n{many}\r\n1\r\n--{b}--"
        );
        assert!(
            parse(body.as_bytes(), BOUNDARY).is_err(),
            "too many headers"
        );
        let long = "v".repeat(MAX_HEADER_LINE);
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"a\"\r\nX-Long: {long}\r\n\r\n1\r\n--{b}--"
        );
        assert!(
            parse(body.as_bytes(), BOUNDARY).is_err(),
            "header line too long"
        );
        let name = "n".repeat(MAX_NAME + 1);
        let body =
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n1\r\n--{b}--");
        assert!(
            parse(body.as_bytes(), BOUNDARY).is_err(),
            "field name too long"
        );
    }

    #[test]
    fn boundaries_are_validated() {
        for (header, expected) in [
            ("multipart/form-data; boundary=abc", Some("abc")),
            ("Multipart/Form-Data; Boundary=\"a b\"", Some("a b")),
            (
                "multipart/form-data; charset=utf-8; boundary=\"x:y\"",
                Some("x:y"),
            ),
            ("multipart/form-data; boundary=\"x;y\"", None),
            ("multipart/form-data", None),
            ("multipart/form-data; boundary=", None),
            ("multipart/mixed; boundary=abc", None),
            ("text/plain; boundary=abc", None),
            ("multipart/form-data; boundary=a; boundary=b", None),
            ("multipart/form-data; boundary=\"unterminated", None),
        ] {
            assert_eq!(boundary(header).ok().as_deref(), expected, "{header}");
        }
        let long = format!("multipart/form-data; boundary={}", "b".repeat(71));
        assert!(boundary(&long).is_err());
        assert!(boundary("multipart/form-data; boundary=a\u{e9}b").is_err());
        assert!(boundary("multipart/form-data; boundary=\"trailing \"").is_err());
    }

    /// Differential test: random bodies split by an independent encoder always
    /// parse back to the generated parts, and corrupting one byte of the
    /// framing is either rejected or changes the parse; the parser never panics.
    #[test]
    fn random_bodies_round_trip_and_mutations_never_panic() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..400 {
            let count = 1 + (next() % 5) as usize;
            let mut specs = Vec::new();
            for index in 0..count {
                let length = (next() % 300) as usize;
                // Content avoids the boundary but is otherwise arbitrary, with CR, LF and dashes.
                let content: Vec<u8> = (0..length)
                    .map(|_| match next() % 6 {
                        0 => b'\r',
                        1 => b'\n',
                        2 => b'-',
                        _ => (next() % 256) as u8,
                    })
                    .collect();
                let content = if windows_contain(&content, BOUNDARY.as_bytes()) {
                    Vec::new()
                } else {
                    content
                };
                specs.push(if next() % 2 == 0 {
                    Spec {
                        field: format!("f{index}"),
                        content,
                        ..Spec::default()
                    }
                } else {
                    Spec::file(
                        &format!("f{index}"),
                        &format!("n{index}.bin"),
                        "application/octet-stream",
                        &content,
                    )
                });
            }
            let body = encode(&specs);
            let parts = parse(&body, BOUNDARY).unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert_eq!(parts.len(), specs.len(), "round {round}");
            for (part, spec) in parts.iter().zip(&specs) {
                assert_eq!(part.name, spec.field);
                assert_eq!(
                    &body[part.content.clone()],
                    spec.content.as_slice(),
                    "round {round}"
                );
            }
            // Mutations: flip, delete or insert one byte anywhere; truncate anywhere.
            for _ in 0..8 {
                let mut mutated = body.clone();
                let at = (next() as usize) % mutated.len();
                match next() % 4 {
                    0 => mutated[at] ^= 1 << (next() % 8),
                    1 => {
                        mutated.remove(at);
                    }
                    2 => mutated.insert(at, (next() % 256) as u8),
                    _ => mutated.truncate(at),
                }
                if let Ok(parsed) = parse(&mutated, BOUNDARY) {
                    for part in parsed {
                        assert!(
                            part.content.end <= mutated.len()
                                && part.content.start <= part.content.end
                        );
                    }
                }
            }
        }
    }

    fn windows_contain(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
