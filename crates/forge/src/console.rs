//! Reading a secret from the controlling terminal without echoing it.
//!
//! On Unix the terminal is switched (through `rustix`, no `unsafe` here) to
//! a mode without echo, line editing or signal keys while one line is read
//! byte by byte, and the original mode is restored by a guard on every path.
//! Because the terminal does not turn Ctrl-C into `SIGINT` in that mode, the
//! process is never killed with echo turned off: Ctrl-C is read as a byte
//! and ends the prompt with [`io::ErrorKind::Interrupted`] after the mode is
//! restored. Ctrl-D ends it as end of input. The prompt goes to standard
//! error so standard output stays machine-readable. Other platforms refuse
//! the prompt; commands then accept the secret from piped standard input or
//! an environment variable.

use std::io::{self, Read, Write};

const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;
const BACKSPACE: u8 = 0x08;
const DELETE: u8 = 0x7f;

/// Prints `prompt` on standard error and reads one line from the terminal
/// with echo turned off.
#[cfg(unix)]
pub(crate) fn prompt_hidden(prompt: &str) -> io::Result<String> {
    use std::os::fd::AsFd;

    use rustix::termios::{
        LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcsetattr,
    };

    /// Restores the saved terminal mode when dropped.
    struct Restore {
        original: Termios,
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            let stdin = io::stdin();
            // Best effort: there is no caller left to report a failure to.
            let _ = tcsetattr(stdin.as_fd(), OptionalActions::Now, &self.original);
        }
    }

    let stdin = io::stdin();
    let original = tcgetattr(stdin.as_fd())?;
    let mut hidden = original.clone();
    hidden
        .local_modes
        .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG);
    hidden.special_codes[SpecialCodeIndex::VMIN] = 1;
    hidden.special_codes[SpecialCodeIndex::VTIME] = 0;
    let restore = Restore { original };
    tcsetattr(stdin.as_fd(), OptionalActions::Flush, &hidden)?;

    let mut stderr = io::stderr().lock();
    stderr.write_all(prompt.as_bytes())?;
    stderr.flush()?;
    let line = read_hidden_line(&mut stdin.lock());
    drop(restore);
    // The terminal did not echo the line ending.
    stderr.write_all(b"\n")?;
    stderr.flush()?;
    line
}

/// Without a Unix terminal API the prompt is refused.
#[cfg(not(unix))]
pub(crate) fn prompt_hidden(_prompt: &str) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hidden terminal input is not supported on this platform",
    ))
}

/// Reads one line typed in non-canonical mode: Enter ends it, Backspace
/// removes the last character, Ctrl-C interrupts, Ctrl-D or end of input
/// fails.
fn read_hidden_line(input: &mut impl Read) -> io::Result<String> {
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if input.read(&mut byte)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        match byte[0] {
            b'\n' | b'\r' => break,
            CTRL_C => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "interrupted at the password prompt",
                ));
            }
            CTRL_D => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            BACKSPACE | DELETE => {
                // Remove one whole UTF-8 character.
                while let Some(last) = line.pop() {
                    if last & 0b1100_0000 != 0b1000_0000 {
                        break;
                    }
                }
            }
            other => line.push(other),
        }
    }
    String::from_utf8(line).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}

#[cfg(test)]
mod console_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> io::Result<String> {
        read_hidden_line(&mut io::Cursor::new(bytes.to_vec()))
    }

    #[test]
    fn a_line_ends_at_enter_and_backspace_edits_it() {
        assert_eq!(read(b"secret\r").ok().as_deref(), Some("secret"));
        assert_eq!(
            read(b"secrex\x7ft\nignored").ok().as_deref(),
            Some("secret")
        );
        assert_eq!(
            read("pass\u{e7}\x08c\n".as_bytes()).ok().as_deref(),
            Some("passc")
        );
        assert_eq!(read(b"\x7f\x7fok\n").ok().as_deref(), Some("ok"));
    }

    #[test]
    fn ctrl_c_interrupts_and_end_of_input_fails() {
        let interrupted = read(b"half\x03rest\n").expect_err("Ctrl-C");
        assert_eq!(interrupted.kind(), io::ErrorKind::Interrupted);
        for ended in [&b"half"[..], &b"half\x04"[..]] {
            assert_eq!(
                read(ended).expect_err("no Enter").kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }
}
