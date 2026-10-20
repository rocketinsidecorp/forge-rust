//! Byte-level encoders: RFC 2047 encoded words, quoted-printable, base64
//! lines, header folding, MIME parameters and the RFC 5322 date.
//!
//! Each follows the Go standard library function Forge-Go calls
//! (`mime.WordEncoder`, `mime/quotedprintable`, `mime.FormatMediaType`,
//! `net/mail.Address.String`, `time.RFC1123Z`) byte for byte, so both
//! frameworks emit identical messages for identical input; the golden tests
//! compare against Forge-Go's output.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD};

const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";
/// The longest encoded word (RFC 2047 section 2).
const MAX_ENCODED_WORD: usize = 75;
/// Content of one encoded word: 75 minus `=?UTF-8?q?` and `?=`.
const MAX_WORD_CONTENT: usize = MAX_ENCODED_WORD - 10 - 2;
/// Input bytes of one base64 encoded word: `DecodedLen(63)`.
const MAX_BASE64_WORD_INPUT: usize = MAX_WORD_CONTENT / 4 * 3;
/// Quoted-printable line limit, soft break included.
const QP_LINE: usize = 76;
/// Input bytes that encode to exactly 76 base64 characters.
pub(crate) const BASE64_LINE_INPUT: usize = 57;
/// Header lines are folded before this column where a space allows.
const FOLD_LIMIT: usize = 76;

/// Go's `needsEncoding`: anything but printable ASCII and tab.
fn needs_encoding(text: &str) -> bool {
    text.bytes()
        .any(|byte| !(b' '..=b'~').contains(&byte) && byte != b'\t')
}

/// `mime.QEncoding.Encode("utf-8", text)`.
pub(crate) fn q_encode(text: &str) -> String {
    if !needs_encoding(text) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(48);
    out.push_str("=?utf-8?q?");
    let mut current = 0;
    for character in text.chars() {
        let mut buffer = [0_u8; 4];
        let bytes = character.encode_utf8(&mut buffer).as_bytes();
        let plain = bytes.len() == 1
            && bytes[0] >= b' '
            && bytes[0] <= b'~'
            && !matches!(bytes[0], b'=' | b'?' | b'_');
        let length = if plain { 1 } else { 3 * bytes.len() };
        if current + length > MAX_WORD_CONTENT {
            out.push_str("?= =?utf-8?q?");
            current = 0;
        }
        for &byte in bytes {
            match byte {
                b' ' => out.push('_'),
                b'!'..=b'~' if !matches!(byte, b'=' | b'?' | b'_') => out.push(char::from(byte)),
                _ => push_hex(&mut out, byte),
            }
        }
        current += length;
    }
    out.push_str("?=");
    out
}

/// `mime.BEncoding.Encode("utf-8", text)`.
pub(crate) fn b_encode(text: &str) -> String {
    if !needs_encoding(text) {
        return text.to_owned();
    }
    let mut out = String::from("=?utf-8?b?");
    if STANDARD.encode(text).len() <= MAX_WORD_CONTENT {
        out.push_str(&STANDARD.encode(text));
        out.push_str("?=");
        return out;
    }
    let (mut current, mut last) = (0, 0);
    for (index, character) in text.char_indices() {
        let length = character.len_utf8();
        if current + length <= MAX_BASE64_WORD_INPUT {
            current += length;
        } else {
            out.push_str(&STANDARD.encode(&text[last..index]));
            out.push_str("?= =?utf-8?b?");
            last = index;
            current = length;
        }
    }
    out.push_str(&STANDARD.encode(&text[last..]));
    out.push_str("?=");
    out
}

/// Unstructured header text (a subject, a display name containing `=?`):
/// Q-encoded when not plain ASCII, and base64-encoded as a whole when it
/// contains `=?`, because a client would otherwise decode a hand-written
/// `=?utf-8?b?...?=` into attacker-chosen text.
pub(crate) fn encode_text(text: &str) -> String {
    if !text.contains("=?") {
        return q_encode(text);
    }
    const MAX_RAW: usize = 45;
    let mut words = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = MAX_RAW.min(rest.len());
        while end < rest.len() && !rest.is_char_boundary(end) {
            end -= 1;
        }
        words.push(format!("=?utf-8?b?{}?=", STANDARD.encode(&rest[..end])));
        rest = &rest[end..];
    }
    words.join(" ")
}

/// A display name as `net/mail.Address.String` writes it: a quoted string
/// when printable ASCII, otherwise an encoded word (base64 when the name has
/// characters an encoded word in a phrase must not contain).
pub(crate) fn display_name(name: &str) -> String {
    let printable = name
        .bytes()
        .all(|byte| (b'!'..=b'~').contains(&byte) || byte == b' ' || byte == b'\t');
    if printable {
        let mut out = String::with_capacity(name.len() + 2);
        out.push('"');
        for character in name.chars() {
            if character == '"' || character == '\\' {
                out.push('\\');
            }
            out.push(character);
        }
        out.push('"');
        return out;
    }
    if name.contains(|character| "\"#$%&'(),.:;<>@[]^`{|}~".contains(character)) {
        b_encode(name)
    } else {
        q_encode(name)
    }
}

fn push_hex(out: &mut String, byte: u8) {
    out.push('=');
    out.push(char::from(UPPER_HEX[usize::from(byte >> 4)]));
    out.push(char::from(UPPER_HEX[usize::from(byte & 0x0f)]));
}

/// `mime/quotedprintable.Writer` (text mode) over `text`, CRLF line ends.
pub(crate) fn quoted_printable(text: &[u8]) -> Vec<u8> {
    let mut writer = QpWriter {
        out: Vec::with_capacity(text.len() + text.len() / 8 + 8),
        line: Vec::with_capacity(QP_LINE + 2),
        cr: false,
    };
    for &byte in text {
        match byte {
            b'!'..=b'~' if byte != b'=' => writer.write(byte),
            b' ' | b'\t' | b'\n' | b'\r' => writer.write(byte),
            _ => writer.encode(byte),
        }
    }
    writer.check_last_byte();
    writer.flush();
    writer.out
}

struct QpWriter {
    out: Vec<u8>,
    line: Vec<u8>,
    cr: bool,
}

impl QpWriter {
    fn write(&mut self, byte: u8) {
        if byte == b'\n' || byte == b'\r' {
            if self.cr && byte == b'\n' {
                self.cr = false;
                return;
            }
            if byte == b'\r' {
                self.cr = true;
            }
            self.check_last_byte();
            self.insert_crlf();
            return;
        }
        if self.line.len() == QP_LINE - 1 {
            self.insert_soft_line_break();
        }
        self.line.push(byte);
        self.cr = false;
    }

    fn encode(&mut self, byte: u8) {
        if QP_LINE - 1 - self.line.len() < 3 {
            self.insert_soft_line_break();
        }
        self.line.push(b'=');
        self.line.push(UPPER_HEX[usize::from(byte >> 4)]);
        self.line.push(UPPER_HEX[usize::from(byte & 0x0f)]);
    }

    fn check_last_byte(&mut self) {
        if let Some(&last) = self.line.last()
            && (last == b' ' || last == b'\t')
        {
            self.line.pop();
            self.encode(last);
        }
    }

    fn insert_soft_line_break(&mut self) {
        self.line.push(b'=');
        self.insert_crlf();
    }

    fn insert_crlf(&mut self) {
        self.line.extend_from_slice(b"\r\n");
        self.flush();
    }

    fn flush(&mut self) {
        self.out.extend_from_slice(&self.line);
        self.line.clear();
    }
}

/// Appends `data` as base64 in 76-character CRLF-terminated lines.
pub(crate) fn base64_lines(data: &[u8], out: &mut Vec<u8>) {
    for chunk in data.chunks(BASE64_LINE_INPUT) {
        out.extend_from_slice(STANDARD.encode(chunk).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
}

/// Renders `Name: value` folded at spaces so no line exceeds 76 columns
/// where a space allows. Folding inserts CRLF before an existing run of
/// spaces, which unfolding removes again. No line ends in whitespace and no
/// line is whitespace-only (a gateway that trims trailing whitespace would
/// otherwise end the header block early). Leading and trailing spaces of
/// the value are dropped. Values never contain CR or LF (validated).
pub(crate) fn fold_header(name: &str, value: &str) -> String {
    let value = value.trim_matches(' ');
    let mut line = format!("{name}:");
    if value.is_empty() {
        return line;
    }
    let mut out = String::new();
    let mut first = true;
    let mut rest = value;
    while !rest.is_empty() {
        let trimmed = rest.trim_start_matches(' ');
        let mut gap = &rest[..rest.len() - trimmed.len()];
        rest = trimmed;
        let word = rest.find(' ').map_or(rest, |index| &rest[..index]);
        rest = &rest[word.len()..];
        if first {
            gap = " ";
        }
        let width = line.len() + gap.len() + word.len();
        if (first && width > FOLD_LIMIT && word.len() > FOLD_LIMIT / 2)
            || (!first && width > FOLD_LIMIT)
        {
            out.push_str(&line);
            out.push_str("\r\n");
            line = format!("{gap}{word}");
        } else {
            line.push_str(gap);
            line.push_str(word);
        }
        first = false;
    }
    out.push_str(&line);
    out
}

fn is_tspecial(byte: u8) -> bool {
    b"()<>@,;:\\\"/[]?=".contains(&byte)
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte > 0x20 && byte < 0x7f && !is_tspecial(byte))
}

/// `mime.FormatMediaType`: `type/subtype; key=value` with parameters sorted
/// by key, quoted when not tokens and RFC 2231-encoded when not ASCII.
/// Returns `None` when the type or a key is not a token.
pub(crate) fn format_media_type(media_type: &str, params: &[(&str, &str)]) -> Option<String> {
    let mut out = String::new();
    match media_type.split_once('/') {
        None => {
            if !is_token(media_type) {
                return None;
            }
            out.push_str(&media_type.to_ascii_lowercase());
        }
        Some((major, sub)) => {
            if !is_token(major) || !is_token(sub) {
                return None;
            }
            out.push_str(&major.to_ascii_lowercase());
            out.push('/');
            out.push_str(&sub.to_ascii_lowercase());
        }
    }
    let mut sorted = params.to_vec();
    sorted.sort_by(|left, right| left.0.cmp(right.0));
    for (key, value) in sorted {
        out.push_str("; ");
        if !is_token(key) {
            return None;
        }
        out.push_str(&key.to_ascii_lowercase());
        if needs_encoding(value) {
            out.push_str("*=utf-8''");
            for byte in value.bytes() {
                if byte <= b' '
                    || byte >= 0x7f
                    || matches!(byte, b'*' | b'\'' | b'%')
                    || is_tspecial(byte)
                {
                    out.push('%');
                    out.push(char::from(UPPER_HEX[usize::from(byte >> 4)]));
                    out.push(char::from(UPPER_HEX[usize::from(byte & 0x0f)]));
                } else {
                    out.push(char::from(byte));
                }
            }
            continue;
        }
        out.push('=');
        if is_token(value) {
            out.push_str(value);
            continue;
        }
        out.push('"');
        for character in value.chars() {
            if character == '"' || character == '\\' {
                out.push('\\');
            }
            out.push(character);
        }
        out.push('"');
    }
    Some(out)
}

/// Splits a media type such as `image/png` into a canonical lowercase form,
/// refusing parameters, like `mime.ParseMediaType` followed by Forge-Go's
/// "no parameters" rule.
pub(crate) fn parse_media_type(value: &str) -> Option<String> {
    let value = value.trim();
    let valid = match value.split_once('/') {
        Some((major, sub)) => is_token(major) && is_token(sub),
        None => is_token(value),
    };
    valid.then(|| value.to_ascii_lowercase())
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Seconds since the Unix epoch, saturating for times before it.
pub(crate) fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Civil date of a day count since 1970-01-01 (Howard Hinnant's
/// algorithm).
fn civil_from_days(days: i64) -> (i64, usize, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, usize::try_from(month - 1).unwrap_or(0), day)
}

/// `time.RFC1123Z` in UTC: `Wed, 14 Oct 2026 18:04:05 +0000`.
pub(crate) fn rfc5322_date(time: SystemTime) -> String {
    let seconds = i64::try_from(unix_seconds(time)).unwrap_or(i64::MAX);
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[usize::try_from(days.rem_euclid(7)).unwrap_or(0)];
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} +0000",
        MONTHS[month],
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    )
}

/// `20060102T150405Z`, the file sender's name prefix.
pub(crate) fn compact_utc(time: SystemTime) -> String {
    let seconds = i64::try_from(unix_seconds(time)).unwrap_or(i64::MAX);
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{:02}{day:02}T{:02}{:02}{:02}Z",
        month + 1,
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q_encoding_matches_go() {
        assert_eq!(q_encode("Welcome"), "Welcome");
        assert_eq!(q_encode("Olá, João"), "=?utf-8?q?Ol=C3=A1,_Jo=C3=A3o?=");
        assert_eq!(q_encode("a=b?c_d é"), "=?utf-8?q?a=3Db=3Fc=5Fd_=C3=A9?=");
        // Words split before 63 encoded bytes without splitting a character.
        let long = "é".repeat(12);
        assert_eq!(
            q_encode(&long),
            format!(
                "=?utf-8?q?{}?= =?utf-8?q?{}?=",
                "=C3=A9".repeat(10),
                "=C3=A9".repeat(2)
            )
        );
    }

    #[test]
    fn b_encoding_matches_go() {
        assert_eq!(b_encode("Zoë, Inc."), "=?utf-8?b?Wm/DqywgSW5jLg==?=");
        let long = "ç".repeat(30);
        let encoded = b_encode(&long);
        assert_eq!(encoded.matches("=?utf-8?b?").count(), 2, "{encoded}");
    }

    #[test]
    fn encoded_word_lookalikes_are_encoded_whole() {
        assert_eq!(
            encode_text("=?utf-8?q?evil?="),
            "=?utf-8?b?PT91dGYtOD9xP2V2aWw/PQ==?="
        );
        assert_eq!(encode_text("Plain subject"), "Plain subject");
    }

    #[test]
    fn display_names_match_net_mail() {
        assert_eq!(display_name("Forge App"), "\"Forge App\"");
        assert_eq!(display_name("Doe, \"J\""), "\"Doe, \\\"J\\\"\"");
        assert_eq!(display_name("João"), "=?utf-8?q?Jo=C3=A3o?=");
        assert_eq!(display_name("João, Inc."), b_encode("João, Inc."));
    }

    #[test]
    fn quoted_printable_matches_go() {
        assert_eq!(quoted_printable(b"Hello\r\nWorld"), b"Hello\r\nWorld");
        assert_eq!(quoted_printable(b"a=b"), b"a=3Db");
        assert_eq!(quoted_printable(b"trail \r\nx"), b"trail=20\r\nx");
        assert_eq!(quoted_printable("é".as_bytes()), b"=C3=A9");
        let long = "x".repeat(80);
        assert_eq!(
            quoted_printable(long.as_bytes()),
            format!("{}=\r\n{}", "x".repeat(75), "x".repeat(5)).into_bytes()
        );
    }

    #[test]
    fn folding_keeps_lines_short_and_never_blank() {
        let value = "word ".repeat(30);
        let folded = fold_header("Subject", &value);
        for line in folded.split("\r\n") {
            assert!(line.len() <= 76, "{line:?}");
            assert!(!line.trim().is_empty(), "{folded:?}");
            assert!(!line.ends_with(' '), "{folded:?}");
        }
        assert_eq!(
            folded.replace("\r\n", ""),
            format!("Subject: {}", value.trim())
        );
        assert_eq!(fold_header("X", "   "), "X:");
    }

    #[test]
    fn media_types_match_go() {
        assert_eq!(
            format_media_type("text/plain", &[("charset", "utf-8")]).as_deref(),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(
            format_media_type("application/pdf", &[("name", "my file.pdf")]).as_deref(),
            Some("application/pdf; name=\"my file.pdf\"")
        );
        assert_eq!(
            format_media_type("attachment", &[("filename", "relatório.pdf")]).as_deref(),
            Some("attachment; filename*=utf-8''relat%C3%B3rio.pdf")
        );
        assert_eq!(format_media_type("bad type", &[]), None);
        assert_eq!(
            parse_media_type(" Image/PNG ").as_deref(),
            Some("image/png")
        );
        assert_eq!(parse_media_type("text/plain; charset=utf-8"), None);
        assert_eq!(parse_media_type("text/"), None);
    }

    #[test]
    fn dates_are_rfc1123z_in_utc() {
        let time = UNIX_EPOCH + Duration::from_secs(1_792_001_045);
        assert_eq!(rfc5322_date(time), "Wed, 14 Oct 2026 18:04:05 +0000");
        assert_eq!(compact_utc(time), "20261014T180405Z");
        assert_eq!(rfc5322_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 +0000");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc5322_date(leap), "Tue, 29 Feb 2000 00:00:00 +0000");
    }
}
