//! The hidden prompt where no terminal is attached, and bytes that are not
//! text.

use std::io::IsTerminal as _;

use super::*;

#[test]
fn a_line_that_is_not_utf8_is_invalid_data() {
    let error = read_hidden_line(&mut io::Cursor::new(vec![0xff, 0xfe, b'\n'])).expect_err("bytes");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn an_empty_line_is_an_empty_secret_and_input_ends_the_prompt() {
    assert_eq!(
        read_hidden_line(&mut io::Cursor::new(b"\n".to_vec()))
            .ok()
            .as_deref(),
        Some("")
    );
    assert_eq!(
        read_hidden_line(&mut io::empty())
            .expect_err("nothing typed")
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
}

#[cfg(unix)]
#[test]
fn the_prompt_is_refused_without_a_terminal_and_never_waits() {
    // Run from a terminal, the prompt would wait for a person: skip.
    if io::stdin().is_terminal() {
        return;
    }
    let error = prompt_hidden("Password: ").expect_err("stdin is not a terminal");
    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::Other | io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
        ) || error.raw_os_error().is_some(),
        "{error:?}"
    );
}
