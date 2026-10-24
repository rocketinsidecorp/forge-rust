//! Inputs the continuous fuzzer found in Forge-Go's encoder
//! (`FuzzAddressAndHeaderValidation`, forge-go#30), which shares its output
//! byte for byte with this one. An accepted message must read back as
//! exactly what was written: the display name, the subject and the address.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use forge_mail::{Address, Message, encode};

/// Decodes RFC 2047 `b` and `q` words and joins what lies between them the
/// way a client does (whitespace between two encoded words disappears).
fn decode(text: &str) -> String {
    let mut out = String::new();
    let mut pending_gap = String::new();
    let mut previous_was_word = false;
    for token in text.split(' ') {
        let word = token
            .strip_prefix("=?utf-8?")
            .and_then(|rest| rest.strip_suffix("?="))
            .and_then(|rest| rest.split_once('?'));
        match word {
            Some((encoding, body)) => {
                if !previous_was_word {
                    out.push_str(&pending_gap);
                }
                pending_gap.clear();
                let bytes = if encoding == "b" {
                    STANDARD.decode(body).expect("base64")
                } else {
                    let mut bytes = Vec::new();
                    let mut iter = body.bytes();
                    while let Some(byte) = iter.next() {
                        match byte {
                            b'_' => bytes.push(b' '),
                            b'=' => {
                                let hex: Vec<u8> = iter.by_ref().take(2).collect();
                                let hex = std::str::from_utf8(&hex).expect("hex");
                                bytes.push(u8::from_str_radix(hex, 16).expect("hex"));
                            }
                            other => bytes.push(other),
                        }
                    }
                    bytes
                };
                out.push_str(&String::from_utf8(bytes).expect("utf-8"));
                previous_was_word = true;
            }
            None => {
                out.push_str(&pending_gap);
                out.push_str(token);
                pending_gap = " ".to_owned();
                previous_was_word = false;
                continue;
            }
        }
        pending_gap = " ".to_owned();
    }
    out
}

/// The value of header `name` with folding removed.
fn header(wire: &str, name: &str) -> String {
    let head = wire.split("\r\n\r\n").next().expect("head");
    let unfolded = head.replace("\r\n ", " ").replace("\r\n\t", "\t");
    unfolded
        .split("\r\n")
        .find_map(|line| line.strip_prefix(&format!("{name}:")))
        .map(|value| value.trim_matches(' ').to_owned())
        .unwrap_or_default()
}

fn check(name: &str, email: &str, subject: &str) {
    let from = Address::new(name, "sender@example.com").expect("valid sender");
    let to = Address::new(name, email).expect("valid recipient");
    let message = Message::new(from).to(to).subject(subject).text("body");
    let encoded = encode(&message).expect("accepted by Forge-Go, so accepted here");
    let wire = String::from_utf8(encoded.data).expect("7-bit clean");
    assert!(wire.is_ascii());
    assert!(wire.split("\r\n").all(|line| line.len() <= 998));
    assert!(!wire.contains("Bcc:"));
    assert_eq!(encoded.recipients, [email]);

    // The subject reads back as written, minus insignificant outer spaces.
    assert_eq!(
        decode(&header(&wire, "Subject")),
        subject.trim_matches(' '),
        "{subject:?}"
    );
    // One To address: the phrase (the name) and the angle address.
    let to = header(&wire, "To");
    let phrase = to
        .strip_suffix(&format!(" <{email}>"))
        .unwrap_or_else(|| panic!("To is {to:?}"));
    if name.is_ascii() && !name.contains("=?") {
        assert_eq!(phrase, format!("\"{name}\""));
    } else {
        // An encoded phrase has no raw specials a parser would trip over.
        assert!(
            phrase.split(' ').all(|word| word.starts_with("=?utf-8?b?")),
            "{phrase:?}"
        );
        assert!(!phrase.contains(['\\', '"']), "{phrase:?}");
        assert_eq!(decode(phrase), name.trim_matches(' '), "{name:?}");
    }
}

#[test]
fn spaces_runs_and_encoded_word_lookalikes_survive() {
    for subject in [
        " 0  0",
        "  ",
        " ",
        " =?",
        &format!("0  {}", "0".repeat(65)),
        "a  b",
        "Olá  mundo",
        "  =?utf-8?q?x?=  ",
    ] {
        check("0", "0@0", subject);
    }
}

#[test]
fn a_non_ascii_name_with_a_backslash_is_base64() {
    check("\\\u{073C}", "0@0", "0");
    check("Jo\\ão \"Z\"", "joao@example.com", "Olá");
    check("Ünï (Inc.)", "u@example.org", "Héllo");
}
