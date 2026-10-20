//! `forge credentials init|edit|show|rotate-key`: encrypted per-environment
//! credentials (ADR 0041), with Forge-Go's commands, files and messages.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use forge_credentials::{
    CredentialsError, DEFAULT_DIR, KeySource, file_name, indent, init, key_file_name, load_keys,
    new_key, read_file, redact, seal, unseal, validate_environment, write_file, write_key_file,
};
use forge_encryption::{Key, encode_key, generate_key};
use zeroize::Zeroizing;

use crate::CliError;
use crate::credentials_edit::{EditorRunner, Interrupts};

/// Keeps credential keys out of version control; the encrypted `.json.enc`
/// files are meant to be committed.
pub(crate) const GITIGNORE_ENTRY: &str = "/config/credentials/*.key";
/// Keeps credential keys out of the image build context.
pub(crate) const DOCKERIGNORE_ENTRY: &str = "config/credentials/*.key";

/// What `forge credentials` was asked to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    Init,
    Edit,
    Show {
        reveal: bool,
    },
    RotateKey {
        keep_old: bool,
        print_key: bool,
        write_key_file: bool,
    },
}

/// The process state the subcommands use, so tests can substitute it.
pub(crate) struct Context<'a> {
    /// The application root (the working directory).
    pub(crate) root: PathBuf,
    /// Reads environment variables (`FORGE_ENV`, `FORGE_MASTER_KEY`,
    /// `VISUAL`, `EDITOR`).
    pub(crate) lookup: &'a dyn Fn(&str) -> Option<String>,
    pub(crate) stdout: &'a mut dyn Write,
    pub(crate) stderr: &'a mut dyn Write,
    pub(crate) stdout_is_terminal: bool,
    pub(crate) editor: Box<EditorRunner<'a>>,
    pub(crate) interrupts: &'a Interrupts,
    /// Test hook: runs after `rotate-key` has read the file.
    pub(crate) after_read: Option<Box<dyn FnMut() + 'a>>,
}

/// A decrypted credentials file and how it was opened.
pub(crate) struct Decrypted {
    pub(crate) plaintext: Zeroizing<Vec<u8>>,
    /// The keys, the active one first.
    pub(crate) keys: Vec<Key>,
    pub(crate) source: KeySource,
    /// The encrypted file as read, for lost-update detection.
    pub(crate) file: Vec<u8>,
}

pub(crate) fn fail(error: impl std::fmt::Display) -> CliError {
    CliError::Credentials(error.to_string())
}

fn output(error: io::Error) -> CliError {
    CliError::Output(error)
}

impl Context<'_> {
    /// The credentials directory, absolute.
    pub(crate) fn dir(&self) -> PathBuf {
        self.root.join(DEFAULT_DIR)
    }

    /// `config/credentials/<name>` as shown to the user.
    pub(crate) fn shown(name: &str) -> String {
        format!("{DEFAULT_DIR}/{name}")
    }

    /// Runs `action` for `environment` (default: `$FORGE_ENV`, else
    /// `development`).
    pub(crate) fn run(
        &mut self,
        action: &Action,
        environment: Option<String>,
    ) -> Result<(), CliError> {
        let environment = environment
            .or_else(|| (self.lookup)("FORGE_ENV").filter(|value| !value.is_empty()))
            .unwrap_or_else(|| "development".to_owned());
        validate_environment(&environment).map_err(fail)?;
        match action {
            Action::Init => self.init(&environment),
            Action::Edit => self.edit(&environment),
            Action::Show { reveal } => self.show(&environment, *reveal),
            Action::RotateKey {
                keep_old,
                print_key,
                write_key_file,
            } => self.rotate(&environment, *keep_old, *print_key, *write_key_file),
        }
    }

    fn init(&mut self, environment: &str) -> Result<(), CliError> {
        let dir = self.dir();
        match read_file(&dir, environment) {
            Ok(_) => {
                return Err(fail(format!(
                    "{} already exists; use `forge credentials edit --environment {environment}`",
                    Self::shown(&file_name(environment))
                )));
            }
            Err(CredentialsError::NotFound(_)) => {}
            Err(error) => return Err(fail(error)),
        }
        let result = init(&dir, environment);
        let created = match &result {
            Ok(created) => created.as_slice(),
            Err(error) => error.created.as_slice(),
        };
        for path in created {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            writeln!(self.stdout, "  create  {}", Self::shown(&name)).map_err(output)?;
        }
        result.map_err(fail)?;
        ensure_line(&self.root.join(".gitignore"), GITIGNORE_ENTRY, true).map_err(|error| {
            fail(format!(
                "update .gitignore (add {GITIGNORE_ENTRY:?} yourself before committing): {error}"
            ))
        })?;
        ensure_line(&self.root.join(".dockerignore"), DOCKERIGNORE_ENTRY, true).map_err(
            |error| {
                fail(format!(
                    "update .dockerignore (add {DOCKERIGNORE_ENTRY:?} yourself): {error}"
                ))
            },
        )?;
        writeln!(
            self.stdout,
            "  ignore  {GITIGNORE_ENTRY} in .gitignore and .dockerignore"
        )
        .map_err(output)?;
        let key_path = Self::shown(&key_file_name(environment));
        write!(
            self.stdout,
            "\nThe key in {key_path} is the only way to read {}.\n\
             Back it up in a password manager or secret store; it is never committed.\n\
             Deploy with:  FORGE_MASTER_KEY=$(cat {key_path})   (see docs/credentials.md)\n\
             Next:         forge credentials edit --environment {environment}\n",
            Self::shown(&file_name(environment))
        )
        .map_err(output)
    }

    /// Decrypts the credentials of `environment` and returns them with the
    /// keys, where the keys came from and the encrypted file as read (for
    /// lost-update detection).
    pub(crate) fn read_plaintext(&self, environment: &str) -> Result<Decrypted, CliError> {
        let dir = self.dir();
        let (keys, source) = match load_keys(&dir, environment, self.lookup) {
            Ok(found) => found,
            Err(CredentialsError::KeyMissing) => {
                return Err(fail(format!(
                    "{}; create the credentials with `forge credentials init --environment {environment}` or provide the key",
                    CredentialsError::KeyMissing
                )));
            }
            Err(error) => return Err(fail(error)),
        };
        let file = match read_file(&dir, environment) {
            Ok(file) => file,
            Err(CredentialsError::NotFound(_)) => {
                return Err(fail(format!(
                    "{} not found; run `forge credentials init --environment {environment}`",
                    Self::shown(&file_name(environment))
                )));
            }
            Err(error) => return Err(fail(error)),
        };
        let plaintext = unseal(environment, &file, &keys).map_err(fail)?;
        Ok(Decrypted {
            plaintext,
            keys,
            source,
            file,
        })
    }

    fn show(&mut self, environment: &str, reveal: bool) -> Result<(), CliError> {
        if reveal && !self.stdout_is_terminal {
            return Err(fail(
                "--reveal prints secrets and needs an interactive terminal; output to a pipe or \
                 file is refused (use `forge credentials edit` to change values)",
            ));
        }
        let Decrypted { plaintext, .. } = self.read_plaintext(environment)?;
        let out = if reveal {
            indent(&plaintext)
        } else {
            redact(&plaintext).map_err(fail)?
        };
        self.stdout.write_all(&out).map_err(output)
    }

    fn rotate(
        &mut self,
        environment: &str,
        keep_old: bool,
        print_key: bool,
        write_key_file_anyway: bool,
    ) -> Result<(), CliError> {
        let Decrypted {
            plaintext,
            keys,
            source,
            file,
        } = self.read_plaintext(environment)?;
        let to_file = source != KeySource::Environment || write_key_file_anyway;
        if !to_file && !print_key {
            return Err(fail(
                "the current key comes from FORGE_MASTER_KEY, so there is no key file to update: \
                 pass --print-key (to store the new key in your secret manager) or --write-key-file",
            ));
        }
        let fresh = generate_key()
            .map_err(fail)
            .and_then(|key| new_key(key).map_err(fail))?;
        let mut with_old = vec![fresh.clone()];
        with_old.extend(keys.iter().cloned());
        let last = if keep_old {
            with_old.clone()
        } else {
            vec![fresh.clone()]
        };
        let sealed = seal(environment, &plaintext, &fresh).map_err(fail)?;
        if let Some(hook) = self.after_read.as_mut() {
            hook();
        }
        let dir = self.dir();
        // Everything that can reject the rotation happens before any output
        // or file change: a refused rotation must not leave a fresh key
        // behind that nobody was told about.
        if read_file(&dir, environment).ok().as_deref() != Some(file.as_slice()) {
            return Err(fail(
                "the credentials file changed during rotation; nothing was re-encrypted, run the command again",
            ));
        }
        if print_key {
            // Before any file changes: if this fails, nothing was modified.
            let list = Zeroizing::new(
                last.iter()
                    .map(|key| encode_key(key.material()).to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            writeln!(self.stdout, "{}", list.as_str()).map_err(output)?;
        }
        // Crash safety: the key file first gains the new key while still
        // holding the old ones, then the encrypted file changes, and only then
        // are the old keys dropped. At every instant the file on disk is
        // readable with the key file on disk.
        if to_file {
            write_key_file(&dir, environment, &with_old, true).map_err(fail)?;
        }
        if let Err(error) = write_file(&dir, environment, &sealed, true) {
            let mut message = error.to_string();
            if to_file && let Err(restore) = write_key_file(&dir, environment, &keys, true) {
                message.push_str(&format!(
                    "\ncould not restore the previous key file: {restore}"
                ));
            }
            return Err(fail(message));
        }
        if to_file && !keep_old {
            write_key_file(&dir, environment, &last, true).map_err(|error| {
                fail(format!(
                    "re-encrypted, but could not drop the old keys from the key file: {error}"
                ))
            })?;
        }
        let retained = if keep_old {
            format!(
                "the previous key list stays readable ({} key(s)); drop it once every deployment has the new file",
                keys.len()
            )
        } else {
            "the previous key is no longer accepted".to_owned()
        };
        writeln!(
            self.stderr,
            "re-encrypted {} with key {}; {retained}\n\
             update FORGE_MASTER_KEY wherever it is stored (docs/credentials.md, \"Rotating the key\")",
            Self::shown(&file_name(environment)),
            fresh.id()
        )
        .map_err(output)
    }
}

/// Appends `line` to the ignore file at `path` unless an equivalent entry
/// exists. With `create` the file is created when missing.
pub(crate) fn ensure_line(path: &Path, line: &str, create: bool) -> io::Result<()> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !create {
                return Ok(());
            }
            return fs::write(path, format!("{line}\n"));
        }
        Err(error) => return Err(error),
    };
    let unrooted = line.trim_start_matches('/');
    let covered = data.lines().map(str::trim).any(|existing| {
        existing == line
            || existing == unrooted
            || existing == "*.key"
            || existing == "/config/credentials/"
            || existing == "config/credentials/"
    });
    if covered {
        return Ok(());
    }
    let separator = if data.is_empty() || data.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    write!(
        file,
        "{separator}# Credential keys: never commit.\n{line}\n"
    )
}
