//! Byte-for-byte parity with Forge-Go's encoder.
//!
//! The fixtures under `tests/fixtures/golden` are the output of Forge-Go's
//! `mail.Encode` for the same messages, with its clock fixed at
//! 2026-10-14T15:04:05-03:00, a UUIDv7 generator fed with `0x42` bytes and
//! boundary randomness `0, 1, 2, ...` (16 bytes per boundary). Identical
//! input must produce identical bytes in both frameworks.

use std::time::{Duration, UNIX_EPOCH};

use forge_mail::{Address, Attachment, Encoder, Message};
use uuid::Uuid;

fn address(name: &str, email: &str) -> Address {
    Address {
        name: name.to_owned(),
        email: email.to_owned(),
    }
}

fn encoder() -> Encoder {
    Encoder::new()
        .date(UNIX_EPOCH + Duration::from_secs(1_792_001_045))
        .message_id(Uuid::parse_str("01a13b95-f208-7242-8242-424242424242").expect("uuid"))
        .boundary_entropy((0..4096).map(|index: u32| index.to_le_bytes()[0]).collect())
}

fn golden(name: &str, message: &Message, recipients: &[&str]) {
    let path = format!(
        "{}/tests/fixtures/golden/{name}.eml",
        env!("CARGO_MANIFEST_DIR")
    );
    let expected = std::fs::read(&path).expect("golden file");
    let encoded = encoder().encode(message).expect("valid message");
    assert_eq!(
        String::from_utf8_lossy(&encoded.data),
        String::from_utf8_lossy(&expected),
        "{name}"
    );
    assert_eq!(encoded.data, expected, "{name}");
    assert_eq!(encoded.recipients, recipients, "{name}");
    assert_eq!(
        encoded.message_id,
        "<01a13b95-f208-7242-8242-424242424242@example.com>"
    );
}

fn from() -> Address {
    address("Forge App", "app@example.com")
}

fn alice() -> Address {
    address("Alice", "alice@example.org")
}

fn png() -> Vec<u8> {
    vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
}

#[test]
fn text_only() {
    let message = Message::new(from())
        .to(alice())
        .subject("Welcome")
        .text("Hello Alice\nSecond line");
    golden("text_only", &message, &["alice@example.org"]);
}

#[test]
fn alternative_with_encoded_and_folded_headers() {
    let message = Message::new(address("Zoë, Inc.", "billing@Example.COM"))
        .reply_to(address("Support \"Team\"", "support@example.com"))
        .to(address("João da Silva", "joao@example.com.br"))
        .to(address("", "bob@example.org"))
        .cc(address("Café Ünïcödé Ñame That Is Rather Long Indeed", "cafe@example.net"))
        .bcc(address("", "audit@example.org"))
        .subject("Sua fatura de outubro está disponível — referência 2026/10, vencimento em 30 dias, obrigado pela preferência")
        .text(format!(
            "Olá João,\r\n\r\nSua fatura está pronta: total = R$ 10,00.\rLinha com espaço no fim \n.linha com ponto\n{}",
            "longa ".repeat(30)
        ))
        .html("<p>Olá <strong>João</strong></p>\n<p>Total = R$ 10,00</p>")
        .header("list-unsubscribe", "<https://example.com/unsubscribe/42?token=abc>")
        .header("Auto-Submitted", "auto-generated")
        .header(
            "References",
            "<a@example.com> <b@example.com> <c@example.com> <d@example.com> <e@example.com>",
        );
    golden(
        "alternative",
        &message,
        &[
            "joao@example.com.br",
            "bob@example.org",
            "cafe@example.net",
            "audit@example.org",
        ],
    );
}

#[test]
fn mixed_with_related_inline_image_and_attachments() {
    let message = Message::new(from())
        .to(alice())
        .subject("Your receipt")
        .text("See the attached receipt.")
        .html("<p><img src=\"cid:logo@forge\"> See the attached receipt.</p>")
        .attach(Attachment::new(
            "../../relatório final.pdf",
            "Application/PDF",
            "%PDF-1.7 forge golden attachment bytes ".repeat(5),
        ))
        .attach(Attachment::new("logo.png", "image/png", png()).inline("logo@forge"))
        .attach(Attachment::new("notes.txt", "", "plain notes"));
    golden("mixed_related", &message, &["alice@example.org"]);
}

#[test]
fn html_only_with_inline_images_and_a_lookalike_subject() {
    let message = Message::new(from())
        .to(alice())
        .subject("=?utf-8?q?not an encoded word?= but text")
        .html("<img src=\"cid:a\"><img src=\"cid:b\">")
        .attach(Attachment::new("a.png", "image/png", png()).inline("a"))
        .attach(Attachment::new("b.gif", "image/gif", [0, 1, 2, 3, 255].repeat(40)).inline("b"));
    golden("html_inline", &message, &["alice@example.org"]);
}

#[test]
fn lookalike_names_and_quoted_printable_edges() {
    let message = Message::new(address("=?utf-8?b?QWRtaW4=?= Team", "app@example.com"))
        .to(address("Ünï", "u@example.org"))
        .subject("Héllo")
        .text(format!("{}\n\ttabbed\t\nend", "=".repeat(100)));
    golden("name_lookalike", &message, &["u@example.org"]);
}
