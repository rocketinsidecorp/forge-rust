//! `forge credentials edit`: decrypt into a private temporary directory, run
//! the editor without a shell, validate, re-encrypt atomically, and wipe the
//! directory on every way out.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use forge_credentials::{
    MAX_PLAINTEXT_BYTES, file_name, indent, read_file, seal, validate_document, write_file,
};
use tokio::sync::oneshot;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::CliError;
use crate::credentials::{Context, Decrypted, fail};

/// Opens a file in the user's editor and returns when it exits. The error is
/// a message for the user.
pub(crate) type EditorRunner<'a> =
    dyn FnMut(&[String], &Path, &Interrupts) -> Result<(), String> + 'a;

const INTERRUPT: u8 = 1;
const TERMINATE: u8 = 2;
const HANGUP: u8 = 4;

/// Signals received while `edit` runs. A handler thread (one, stopped and
/// joined on drop) only records them; `edit` acts on them at checkpoints so
/// its cleanup always runs.
pub(crate) struct Interrupts {
    received: Arc<AtomicU8>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Interrupts {
    /// Records nothing until [`raise`](Self::raise) is called (tests).
    pub(crate) fn none() -> Self {
        Self {
            received: Arc::new(AtomicU8::new(0)),
            stop: None,
            thread: None,
        }
    }

    /// Catches SIGINT, SIGTERM and SIGHUP (Ctrl-C elsewhere) for the whole
    /// command, so their default action cannot end the process before the
    /// temporary directory is wiped.
    pub(crate) fn install() -> Result<Self, CliError> {
        let received = Arc::new(AtomicU8::new(0));
        let (stop, stopped) = oneshot::channel();
        let (ready, registered) = std::sync::mpsc::channel();
        let flags = Arc::clone(&received);
        let thread = std::thread::Builder::new()
            .name("forge-credentials-signals".to_owned())
            .spawn(move || listen(&flags, stopped, &ready))
            .map_err(CliError::Signals)?;
        match registered.recv() {
            Ok(Ok(())) => Ok(Self {
                received,
                stop: Some(stop),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(CliError::Signals(error))
            }
            Err(_) => {
                let _ = thread.join();
                Err(CliError::Signals(io::Error::other(
                    "the signal thread stopped before it was ready",
                )))
            }
        }
    }

    /// Records a signal as if it had arrived.
    #[cfg(test)]
    pub(crate) fn raise_terminate(&self) {
        self.received.fetch_or(TERMINATE, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn raise_interrupt(&self) {
        self.received.fetch_or(INTERRUPT, Ordering::SeqCst);
    }

    /// The name of a pending termination signal (SIGTERM or SIGHUP), without
    /// consuming it.
    pub(crate) fn termination(&self) -> Option<&'static str> {
        name(self.received.load(Ordering::SeqCst) & (TERMINATE | HANGUP))
    }

    /// Reports a pending signal as an error and clears it. With
    /// `skip_interrupt`, SIGINT is discarded: one typed in the editor
    /// reached this process too and belongs to the editor.
    pub(crate) fn check(&self, skip_interrupt: bool) -> Result<(), CliError> {
        let mut pending = self.received.swap(0, Ordering::SeqCst);
        if skip_interrupt {
            pending &= !INTERRUPT;
        }
        match name(pending) {
            Some(signal) => Err(fail(format!(
                "interrupted by {signal}; credentials are unchanged"
            ))),
            None => Ok(()),
        }
    }
}

fn name(pending: u8) -> Option<&'static str> {
    if pending & TERMINATE != 0 {
        Some("SIGTERM")
    } else if pending & HANGUP != 0 {
        Some("SIGHUP")
    } else if pending & INTERRUPT != 0 {
        Some("SIGINT")
    } else {
        None
    }
}

impl Drop for Interrupts {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn listen(
    received: &AtomicU8,
    mut stopped: oneshot::Receiver<()>,
    ready: &std::sync::mpsc::Sender<io::Result<()>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    runtime.block_on(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let streams = signal(SignalKind::interrupt()).and_then(|interrupt| {
                let terminate = signal(SignalKind::terminate())?;
                let hangup = signal(SignalKind::hangup())?;
                Ok((interrupt, terminate, hangup))
            });
            let (mut interrupt, mut terminate, mut hangup) = match streams {
                Ok(streams) => streams,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            loop {
                let flag = tokio::select! {
                    _ = interrupt.recv() => INTERRUPT,
                    _ = terminate.recv() => TERMINATE,
                    _ = hangup.recv() => HANGUP,
                    _ = &mut stopped => return,
                };
                received.fetch_or(flag, Ordering::SeqCst);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ready.send(Ok(()));
            loop {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            received.fetch_or(INTERRUPT, Ordering::SeqCst);
                        }
                    }
                    _ = &mut stopped => return,
                }
            }
        }
    });
}

/// A directory only the current user can enter, overwritten with zeros and
/// removed when dropped: on success, on every error and while unwinding.
struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    /// Prefers memory-backed `/dev/shm` on Linux, so the plaintext never
    /// reaches a disk; otherwise the system temporary directory.
    fn create() -> io::Result<Self> {
        let mut parents = Vec::new();
        if cfg!(target_os = "linux") {
            parents.push(PathBuf::from("/dev/shm"));
        }
        parents.push(std::env::temp_dir());
        let mut last = io::Error::other("no temporary directory");
        for parent in parents {
            let path = parent.join(format!("forge-credentials-{}", Uuid::now_v7().simple()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;

                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        if let Err(error) = shred_dir(&self.path) {
            eprintln!("warning: could not remove {}: {error}", self.path.display());
        }
    }
}

/// Overwrites every regular file under `dir` with zeros, syncs it, and
/// removes the tree. Overwriting is best effort: copy-on-write and journaling
/// filesystems may keep old blocks, which is why the directory prefers tmpfs.
fn shred_dir(dir: &Path) -> io::Result<()> {
    let mut first_error = None;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && let Err(error) = shred_file(&entry.path())
            {
                first_error.get_or_insert(error);
            }
        }
    }
    let removed = match fs::remove_dir_all(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    };
    match first_error {
        Some(error) => Err(error),
        None => removed,
    }
}

fn shred_file(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    let mut remaining = file.metadata()?.len();
    let zeros = [0_u8; 8192];
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(8192)).unwrap_or(8192);
        file.write_all(&zeros[..chunk])?;
        remaining -= chunk as u64;
    }
    file.sync_all()
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

fn read_bounded(path: &Path, limit: usize) -> Result<Zeroizing<Vec<u8>>, CliError> {
    let mut data = Zeroizing::new(Vec::new());
    let file = File::open(path).map_err(|error| fail(format!("read the edited file: {error}")))?;
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    file.take(cap)
        .read_to_end(&mut data)
        .map_err(|error| fail(format!("read the edited file: {error}")))?;
    if data.len() > limit {
        return Err(fail(format!("edited document exceeds {limit} bytes")));
    }
    Ok(data)
}

fn trim(data: &[u8]) -> &[u8] {
    let start = data
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(data.len());
    let end = data
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &data[start..end.max(start)]
}

impl Context<'_> {
    /// Decrypts the credentials into a private temporary directory, opens the
    /// editor, validates the result and re-encrypts it atomically. The
    /// directory is overwritten and removed on every path out of this
    /// function: success, editor failure, invalid JSON, a failed write,
    /// SIGTERM and SIGHUP. Only SIGKILL or a crash can leave it behind.
    pub(crate) fn edit(&mut self, environment: &str) -> Result<(), CliError> {
        let Some(editor) = editor_command(self.lookup) else {
            return Err(fail("no editor: set $VISUAL or $EDITOR"));
        };
        let interrupts = self.interrupts;
        let Decrypted {
            plaintext,
            keys,
            file,
            ..
        } = self.read_plaintext(environment)?;
        let original = indent(&plaintext);
        drop(plaintext);

        let workspace = PrivateDir::create()
            .map_err(|error| fail(format!("create private temporary directory: {error}")))?;
        let path = workspace.path().join(format!("{environment}.json"));
        write_private(&path, &original)
            .map_err(|error| fail(format!("write the temporary file: {error}")))?;
        interrupts.check(false)?;

        (self.editor)(&editor, &path, interrupts)
            .map_err(|error| fail(format!("editor failed; credentials are unchanged: {error}")))?;
        // An interrupt typed in the editor reached us too; it belongs to the
        // editor, so only termination signals count from here on.
        interrupts.check(true)?;
        let mut edited = read_bounded(&path, MAX_PLAINTEXT_BYTES)?;
        validate_document(&edited).map_err(|error| {
            fail(format!(
                "{error}; credentials are unchanged (your edits were discarded)"
            ))
        })?;
        if !edited.ends_with(b"\n") {
            edited.push(b'\n');
        }
        if trim(&edited) == trim(&original) {
            return writeln!(self.stdout, "no changes").map_err(CliError::Output);
        }
        interrupts.check(false)?;
        let Some(active) = keys.first() else {
            return Err(fail(forge_credentials::CredentialsError::KeyMissing));
        };
        let sealed = seal(environment, &edited, active).map_err(fail)?;
        // Refuse to overwrite a file somebody else changed while the editor
        // was open (a merge, a concurrent edit): their update would be lost.
        let dir = self.dir();
        if read_file(&dir, environment).ok().as_deref() != Some(file.as_slice()) {
            return Err(fail(
                "the credentials file changed while you were editing; credentials are unchanged, edit again",
            ));
        }
        write_file(&dir, environment, &sealed, true).map_err(fail)?;
        drop(workspace);
        writeln!(
            self.stdout,
            "saved {} (key {})",
            Context::shown(&file_name(environment)),
            active.id()
        )
        .map_err(CliError::Output)
    }
}

/// `$VISUAL` or `$EDITOR` split into words. No shell is involved, so quoting
/// is not interpreted: wrapper scripts work, shell pipelines do not.
/// Vim-family editors receive flags that turn off swap, backup, undo and
/// viminfo files, so plaintext does not outlive the session.
pub(crate) fn editor_command(lookup: &dyn Fn(&str) -> Option<String>) -> Option<Vec<String>> {
    ["VISUAL", "EDITOR"].into_iter().find_map(|name| {
        let words: Vec<String> = lookup(name)?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        (!words.is_empty()).then(|| harden_editor(words))
    })
}

fn harden_editor(mut words: Vec<String>) -> Vec<String> {
    if words.first().is_some_and(|command| is_vim_family(command)) {
        words.extend(
            [
                "-n",
                "-i",
                "NONE",
                "-c",
                "set noundofile nobackup nowritebackup viminfo=",
            ]
            .map(str::to_owned),
        );
    }
    words
}

/// Recognizes Vim by the command's own name and by what it resolves to, so
/// `vi`, `editor` or a wrapper symlinked to `vim.basic`, `nvim` or `gvim` are
/// covered.
pub(crate) fn is_vim_family(command: &str) -> bool {
    let mut names = vec![base_name(Path::new(command))];
    if let Some(resolved) = look_path(command) {
        names.push(base_name(&resolved));
        if let Ok(target) = fs::canonicalize(&resolved) {
            names.push(base_name(&target));
        }
    }
    names.iter().any(|name| {
        let name = name.to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        [
            "vim", "nvim", "gvim", "gview", "mvim", "view", "rvim", "rview",
        ]
        .iter()
        .any(|prefix| {
            name == *prefix
                || name.starts_with(&format!("{prefix}."))
                || name.starts_with(&format!("{prefix}-"))
        })
    })
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The executable `command` names: itself when it contains a separator,
/// otherwise the first match on `PATH`.
fn look_path(command: &str) -> Option<PathBuf> {
    if command.contains(std::path::MAIN_SEPARATOR) || command.contains('/') {
        return Some(PathBuf::from(command));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

/// The editor's environment: the caller's minus every `FORGE_` variable.
/// `FORGE_MASTER_KEY` and the other settings hold secrets, and an editor
/// (with its plugins, shell escapes and subprocesses) has no use for them.
pub(crate) fn editor_environment(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    environment
        .into_iter()
        .filter(|(name, _)| {
            !name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("FORGE_")
        })
        .collect()
}

/// Runs the editor attached to the terminal. SIGINT is left to the editor
/// (the terminal delivers it to both, as git does); SIGTERM and SIGHUP stop
/// the editor so the caller can clean up.
pub(crate) fn run_editor_process(
    editor: &[String],
    path: &Path,
    interrupts: &Interrupts,
) -> Result<(), String> {
    let Some((program, arguments)) = editor.split_first() else {
        return Err("no editor".to_owned());
    };
    let mut command = Command::new(program);
    command
        .args(arguments)
        .arg(path)
        .env_clear()
        .envs(editor_environment(std::env::vars_os()));
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {program}: {error}"))?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{program} exited with {status}")),
            Ok(None) => {}
            Err(error) => return Err(format!("wait for {program}: {error}")),
        }
        if let Some(signal) = interrupts.termination() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("interrupted by {signal}"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editors_are_split_without_a_shell_and_vim_is_hardened() {
        let lookup = |name: &str| match name {
            "VISUAL" => Some("  ".to_owned()),
            "EDITOR" => Some("code --wait".to_owned()),
            _ => None,
        };
        assert_eq!(
            editor_command(&lookup),
            Some(vec!["code".to_owned(), "--wait".to_owned()])
        );
        let vim = |name: &str| (name == "VISUAL").then(|| "/usr/bin/vim.basic -p".to_owned());
        let command = editor_command(&vim).expect("editor");
        assert_eq!(&command[..2], ["/usr/bin/vim.basic", "-p"]);
        assert!(command.contains(&"set noundofile nobackup nowritebackup viminfo=".to_owned()));
        assert_eq!(editor_command(&|_| None), None);
        for name in ["vim", "nvim", "gvim", "vim.nox", "view", "/opt/bin/rvim"] {
            assert!(is_vim_family(name), "{name}");
        }
        for name in ["nano", "emacs", "code", "vimdiffer"] {
            assert!(!is_vim_family(name), "{name}");
        }
        // Quoting is not interpreted: no shell.
        let quoted = |name: &str| (name == "EDITOR").then(|| "sh -c 'rm -rf x'".to_owned());
        assert_eq!(
            editor_command(&quoted).expect("editor"),
            ["sh", "-c", "'rm", "-rf", "x'"]
        );
    }

    #[test]
    fn the_editor_does_not_inherit_forge_variables() {
        let kept = editor_environment([
            (OsString::from("FORGE_MASTER_KEY"), OsString::from("secret")),
            (OsString::from("forge_env"), OsString::from("production")),
            (OsString::from("HOME"), OsString::from("/home/me")),
        ]);
        assert_eq!(kept, [(OsString::from("HOME"), OsString::from("/home/me"))]);
    }

    #[test]
    fn interrupts_are_reported_at_checkpoints() {
        let interrupts = Interrupts::none();
        assert!(interrupts.check(false).is_ok());
        interrupts.raise_interrupt();
        assert!(
            interrupts.check(true).is_ok(),
            "SIGINT belongs to the editor"
        );
        interrupts.raise_interrupt();
        assert!(interrupts.check(false).is_err());
        interrupts.raise_terminate();
        assert_eq!(interrupts.termination(), Some("SIGTERM"));
        let error = interrupts.check(true).expect_err("terminated");
        assert!(error.to_string().contains("SIGTERM"));
    }

    #[test]
    fn private_directories_are_owner_only_and_wiped() {
        let dir = PrivateDir::create().expect("directory");
        let path = dir.path().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(
                fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
                0o700
            );
        }
        write_private(&path.join("a.json"), b"secret").expect("write");
        fs::create_dir(path.join("swap")).expect("nested");
        fs::write(path.join("swap/.a.json.swp"), b"secret").expect("swap file");
        drop(dir);
        assert!(!path.exists());
        assert_eq!(trim(b"  {}\n\n"), b"{}");
        assert_eq!(trim(b" \n"), b"");
    }
}
