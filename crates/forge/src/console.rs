//! Reading a secret from the controlling terminal without echoing it.
//!
//! On Unix the terminal's `ECHO` mode is switched off with `tcsetattr`
//! (through `rustix`, no `unsafe` here) while one line is read, and the
//! original mode is restored afterwards, including when reading fails. The
//! prompt goes to standard error so standard output stays machine-readable.
//! Other platforms refuse the prompt; commands then accept the secret from
//! piped standard input or an environment variable.

use std::io::{self, BufRead, Write};

/// Prints `prompt` on standard error and reads one line from standard input
/// with echo turned off. The line ending is removed.
#[cfg(unix)]
pub(crate) fn prompt_hidden(prompt: &str) -> io::Result<String> {
    use std::os::fd::AsFd;

    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};

    /// Restores the saved terminal mode when dropped.
    struct Restore {
        original: rustix::termios::Termios,
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
    hidden.local_modes.remove(LocalModes::ECHO);
    // Keep echoing the newline so the next prompt starts on its own line.
    hidden.local_modes.insert(LocalModes::ECHONL);
    let restore = Restore { original };
    tcsetattr(stdin.as_fd(), OptionalActions::Flush, &hidden)?;

    let mut stderr = io::stderr().lock();
    stderr.write_all(prompt.as_bytes())?;
    stderr.flush()?;
    drop(stderr);

    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    drop(restore);
    read?;
    Ok(strip_line_ending(line))
}

/// Without a Unix terminal API the prompt is refused.
#[cfg(not(unix))]
pub(crate) fn prompt_hidden(_prompt: &str) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hidden terminal input is not supported on this platform",
    ))
}

fn strip_line_ending(mut line: String) -> String {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_are_removed_and_nothing_else() {
        assert_eq!(strip_line_ending("secret\n".to_owned()), "secret");
        assert_eq!(strip_line_ending("secret\r\n".to_owned()), "secret");
        assert_eq!(strip_line_ending(" secret ".to_owned()), " secret ");
    }
}
