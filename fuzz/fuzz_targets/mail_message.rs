//! Mail addresses and message encoding (forge_mail::Address, Message,
//! encode, ADR 0035): the places where header injection would happen.
//!
//! Input: NUL-separated fields: sender, recipient, subject, an extra header
//! name and value, and the text body. Properties: an address that parses
//! prints and parses back to itself; an encoded message is 7-bit clean with
//! CRLF line ends, names only headers the encoder writes or the allow-list
//! admits (no injected `Bcc`), and its envelope holds exactly the validated
//! recipients.
#![no_main]

use forge_mail::{ALLOWED_HEADERS, Address, Message, encode};
use libfuzzer_sys::fuzz_target;

const FIXED_HEADERS: &[&str] = &[
    "Date",
    "From",
    "Reply-To",
    "To",
    "Cc",
    "Subject",
    "Message-ID",
    "MIME-Version",
    "Content-Type",
    "Content-Transfer-Encoding",
    "Content-Disposition",
    "Content-ID",
];

fn check_address(text: &str) {
    if let Ok(address) = Address::parse(text) {
        assert!(address.validate().is_ok());
        let printed = address.to_string();
        assert!(!printed.contains(['\r', '\n']), "line break in {printed:?}");
        // The printed form is the wire form: a non-ASCII display name (or one
        // that could be mistaken for an encoded word) is an RFC 2047 encoded
        // word, which `parse` (a configuration reader) does not decode.
        if address.name.is_ascii() && !printed.contains("=?") {
            assert_eq!(
                Address::parse(&printed).as_ref(),
                Ok(&address),
                "{printed:?} does not read back"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut fields = text.split('\0');
    let sender = fields.next().unwrap_or_default();
    let recipient = fields.next().unwrap_or_default();
    let subject = fields.next().unwrap_or_default();
    let header_name = fields.next().unwrap_or_default();
    let header_value = fields.next().unwrap_or_default();
    let body = fields.next().unwrap_or_default();

    check_address(sender);
    check_address(recipient);

    let (Ok(from), Ok(to)) = (Address::parse(sender), Address::parse(recipient)) else {
        return;
    };
    let message = Message::new(from)
        .to(to)
        .subject(subject)
        .text(body)
        .header(header_name, header_value);
    let Ok(encoded) = encode(&message) else {
        return;
    };

    let wire = &encoded.data;
    assert!(wire.is_ascii(), "the message is not 7-bit clean");
    for (index, byte) in wire.iter().enumerate() {
        match byte {
            b'\n' => assert!(index > 0 && wire[index - 1] == b'\r', "bare LF"),
            b'\r' => assert!(wire.get(index + 1) == Some(&b'\n'), "bare CR"),
            _ => {}
        }
    }
    let wire = String::from_utf8_lossy(wire);
    let head = wire.split("\r\n\r\n").next().unwrap_or_default();
    for line in head.split("\r\n") {
        if line.starts_with([' ', '\t']) {
            continue; // a folded continuation of the previous header
        }
        let name = line.split(':').next().unwrap_or_default();
        assert!(
            FIXED_HEADERS.contains(&name)
                || ALLOWED_HEADERS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(name)),
            "unexpected header {name:?} in the message"
        );
    }
    assert!(
        !head
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("bcc:")),
        "a Bcc header was written"
    );
    assert_eq!(encoded.recipients, message.recipients());
    assert_eq!(encoded.sender, message.from.email);
});
