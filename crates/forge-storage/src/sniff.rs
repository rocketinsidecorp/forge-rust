//! Content sniffing: what the bytes are, whatever the client says they are.
//!
//! This follows the signature tables of the WHATWG MIME Sniffing Standard for
//! the families that matter to a file store (images, documents, archives,
//! audio/video, fonts, HTML and XML) and decides between `text/plain` and
//! `application/octet-stream` by the presence of binary data bytes. It looks at
//! the first 512 bytes only.

use crate::OCTET_STREAM;

/// How many leading bytes [`sniff_content_type`] considers.
pub const SNIFF_LENGTH: usize = 512;

/// HTML tags that start a page when they follow optional whitespace, compared
/// ASCII case-insensitively and followed by a tag-terminating byte.
const HTML_TAGS: &[&str] = &[
    "<!DOCTYPE HTML",
    "<HTML",
    "<HEAD",
    "<SCRIPT",
    "<IFRAME",
    "<H1",
    "<DIV",
    "<FONT",
    "<TABLE",
    "<A",
    "<STYLE",
    "<TITLE",
    "<B",
    "<BODY",
    "<BR",
    "<P",
    "<!--",
];

/// The media type the leading bytes of a file identify: a specific type for
/// recognizable content, `text/plain` for text without a signature and
/// `application/octet-stream` for other binary data.
#[must_use]
pub fn sniff_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(SNIFF_LENGTH)];
    let leading = data
        .iter()
        .position(|byte| !matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' '))
        .unwrap_or(data.len());
    let trimmed = &data[leading..];

    if is_html(trimmed) {
        return "text/html";
    }
    if starts_with_ignore_case(trimmed, b"<?xml") {
        return "text/xml";
    }
    if data.starts_with(b"%PDF-") {
        return "application/pdf";
    }
    if data.starts_with(b"%!PS-Adobe-") {
        return "application/postscript";
    }
    if data.starts_with(&[0xfe, 0xff])
        || data.starts_with(&[0xff, 0xfe])
        || data.starts_with(&[0xef, 0xbb, 0xbf])
    {
        return "text/plain"; // UTF-16 and UTF-8 byte order marks
    }
    if let Some(kind) = binary_signature(data) {
        return kind;
    }
    if data.iter().any(|&byte| is_binary_byte(byte)) {
        OCTET_STREAM
    } else {
        "text/plain"
    }
}

fn is_html(trimmed: &[u8]) -> bool {
    HTML_TAGS.iter().any(|tag| {
        let tag = tag.as_bytes();
        trimmed.len() > tag.len()
            && starts_with_ignore_case(trimmed, tag)
            && matches!(trimmed[tag.len()], b' ' | b'>')
    })
}

fn starts_with_ignore_case(data: &[u8], prefix: &[u8]) -> bool {
    data.len() >= prefix.len() && data[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Signatures of binary formats, by their leading bytes.
fn binary_signature(data: &[u8]) -> Option<&'static str> {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"BM", "image/bmp"),
        (b"\x00\x00\x01\x00", "image/x-icon"),
        (b"\x00\x00\x02\x00", "image/x-icon"),
        (b"OggS\x00", "application/ogg"),
        (b"\x1a\x45\xdf\xa3", "video/webm"),
        (b"ID3", "audio/mpeg"),
        (b"fLaC", "audio/flac"),
        (b"wOFF", "font/woff"),
        (b"wOF2", "font/woff2"),
        (b"\x00\x01\x00\x00", "font/ttf"),
        (b"OTTO", "font/otf"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b\x08", "application/x-gzip"),
        (b"Rar!\x1a\x07\x00", "application/x-rar-compressed"),
        (b"\x00asm", "application/wasm"),
    ];
    if let Some((_, kind)) = SIGNATURES
        .iter()
        .find(|(signature, _)| data.starts_with(signature))
    {
        // "BM" is only a bitmap when it is not ordinary text such as "BMW".
        if *kind == "image/bmp" && data.len() < 14 {
            return None;
        }
        return Some(kind);
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" {
        return match &data[8..12] {
            b"WEBP" => Some("image/webp"),
            b"WAVE" => Some("audio/wave"),
            b"AVI " => Some("video/avi"),
            _ => None,
        };
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        let brand = &data[8..12];
        return Some(if brand == b"avif" || brand == b"avis" {
            "image/avif"
        } else {
            "video/mp4"
        });
    }
    if data.len() >= 3 && data[0] == 0xff && data[1] & 0xe0 == 0xe0 && data[1] & 0x06 != 0 {
        return Some("audio/mpeg"); // MPEG audio frame sync
    }
    None
}

/// Binary data bytes of the standard: control characters other than the
/// whitespace and escape controls text may contain.
fn is_binary_byte(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0b | 0x0e..=0x1a | 0x1c..=0x1f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    const PDF: &[u8] = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n";

    #[test]
    fn recognizes_the_families_a_store_must_tell_apart() {
        for (data, expected) in [
            (PNG, "image/png"),
            (b"\xff\xd8\xff\xe0\x00\x10JFIF".as_slice(), "image/jpeg"),
            (b"GIF89a\x01\x00\x01\x00".as_slice(), "image/gif"),
            (b"RIFF\x24\x00\x00\x00WEBPVP8 ".as_slice(), "image/webp"),
            (
                b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00".as_slice(),
                "image/avif",
            ),
            (
                b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00".as_slice(),
                "video/mp4",
            ),
            (
                b"BM\x36\x00\x0c\x00\x00\x00\x00\x00\x36\x00\x00\x00".as_slice(),
                "image/bmp",
            ),
            (PDF, "application/pdf"),
            (b"PK\x03\x04\x14\x00".as_slice(), "application/zip"),
            (b"\x1f\x8b\x08\x00".as_slice(), "application/x-gzip"),
            (b"hello world\n".as_slice(), "text/plain"),
            (b"a,b\n1,2\n".as_slice(), "text/plain"),
            (
                b"\xef\xbb\xbfutf-8 with a byte order mark".as_slice(),
                "text/plain",
            ),
            (
                b"MZ\x90\x00\x03\x00\x00\x00\x00\x00".as_slice(),
                OCTET_STREAM,
            ),
            (b"\x00\x01\x02\x03".as_slice(), OCTET_STREAM),
            (b"".as_slice(), "text/plain"),
        ] {
            assert_eq!(sniff_content_type(data), expected, "{data:?}");
        }
    }

    #[test]
    fn html_is_recognized_through_whitespace_case_and_every_listed_tag() {
        for html in [
            "<!DOCTYPE html><html>",
            "  \n\t<html lang=en>",
            "<HTML>",
            "<script>alert(1)</script>",
            "<SCRIPT SRC=x>",
            "<iframe src=x>",
            "<h1>x</h1>",
            "<div>",
            "<table>",
            "<a href=x>",
            "<style>",
            "<title>x</title>",
            "<b>",
            "<body onload=x>",
            "<br>",
            "<p>",
            "<!-- c --><script>",
            "\r\n\r\n<html>",
        ] {
            assert_eq!(sniff_content_type(html.as_bytes()), "text/html", "{html:?}");
        }
        // A tag name continuing is not the tag, and text that merely mentions HTML is text.
        for text in ["<abbr>", "<scripts>", "html is fun", "a < b > c"] {
            assert_eq!(
                sniff_content_type(text.as_bytes()),
                "text/plain",
                "{text:?}"
            );
        }
    }

    #[test]
    fn xml_and_svg_sources_are_xml() {
        let svg =
            br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"/>"#;
        assert_eq!(sniff_content_type(svg), "text/xml");
        assert_eq!(sniff_content_type(b"  <?XML version='1.0'?>"), "text/xml");
        // SVG without a prolog is just text to the sniffer; the upload rules refuse it.
        assert_eq!(sniff_content_type(b"<svg xmlns='x'/>"), "text/plain");
    }

    #[test]
    fn only_the_first_512_bytes_count() {
        let mut data = vec![b'a'; SNIFF_LENGTH];
        data.extend_from_slice(b"\x00\x00binary after the window");
        assert_eq!(sniff_content_type(&data), "text/plain");
        let mut html = vec![b' '; SNIFF_LENGTH];
        html.extend_from_slice(b"<html>");
        assert_eq!(
            sniff_content_type(&html),
            "text/plain",
            "whitespace padding past the window hides nothing the browser would not also miss"
        );
    }

    #[test]
    fn a_polyglot_is_what_its_first_bytes_say() {
        let mut gif_html = b"GIF89a".to_vec();
        gif_html.extend_from_slice(b"<script>alert(1)</script>");
        assert_eq!(sniff_content_type(&gif_html), "image/gif");
        let mut png_then_html = PNG.to_vec();
        png_then_html.extend_from_slice(b"<html><script>alert(1)</script>");
        assert_eq!(sniff_content_type(&png_then_html), "image/png");
    }
}
