//! Build-time asset embedding for an application's `build.rs`.
//!
//! ```no_run
//! use forge_views::build::{BuildError, embed_assets};
//!
//! // Called from `fn main` in the application's build.rs; exit non-zero on
//! // failure so the build stops with the message.
//! fn build_script() -> Result<(), BuildError> {
//!     embed_assets("assets")
//! }
//! # let _ = build_script;
//! ```
//!
//! The helper walks the directory, computes a SHA-256 content digest per file
//! and writes `$OUT_DIR/forge_assets.rs`, which defines
//! `pub static FORGE_ASSETS: &[forge_views::AssetEntry]`. An application that
//! reaches this crate through the `forge` facade calls
//! [`embed_assets_with_crate_path`]`("assets", "forge::views")` so the table
//! names `forge::views::AssetEntry` instead. The application includes it and
//! hands it to `AssetSet::new`:
//!
//! ```ignore
//! include!(concat!(env!("OUT_DIR"), "/forge_assets.rs"));
//! let assets = forge_views::AssetSet::new(FORGE_ASSETS)?;
//! ```
//!
//! This module needs only the `build` feature (`sha2` and the standard
//! library); an application's build script depends on `forge-views` with
//! `default-features = false, features = ["build"]`.
//!
//! Rules enforced here, so the runtime never has to trust the file system:
//!
//! - entries whose name starts with `.` (files and directories) are ignored;
//! - a symbolic link must resolve to a file inside the root; links that
//!   escape the root and links to directories are errors;
//! - names use only ASCII letters, digits, `.`, `_` and `-`;
//! - extensions must be on the served allowlist (css, js, mjs, map, svg,
//!   png, jpg, jpeg, gif, webp, ico, woff2, txt);
//! - the walk is bounded in depth, file count and file size.

use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read as _},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::common::{content_type, digested_name, extension_of, hex, valid_component};

/// Name of the generated file inside `OUT_DIR`.
pub const TABLE_FILE_NAME: &str = "forge_assets.rs";

/// Maximum number of embedded files.
pub const MAX_FILES: usize = 4096;
/// Maximum size of one embedded file, in bytes.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum directory nesting below the asset root.
pub const MAX_DEPTH: usize = 16;

/// One scanned file, ready to be written into the table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedAsset {
    /// Logical path relative to the root, `/`-separated (`css/app.css`).
    pub logical: String,
    /// Digested path relative to the prefix (`css/app-<16 hex>.css`).
    pub digested: String,
    /// Full SHA-256 content digest, lowercase hex.
    pub digest: String,
    /// Canonical absolute path of the file contents to embed.
    pub path: PathBuf,
}

/// Asset embedding failure. Every variant names the offending path.
#[derive(Debug, Error)]
pub enum BuildError {
    /// A file system operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being accessed.
        path: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A required Cargo environment variable is missing.
    #[error("environment variable {0} is not set (is this running from build.rs?)")]
    MissingEnv(&'static str),
    /// The asset root is not a directory.
    #[error("{0}: asset root is not a directory")]
    NotADirectory(String),
    /// A symbolic link resolves outside the asset root.
    #[error("{0}: symbolic link escapes the asset root")]
    SymlinkEscape(String),
    /// A symbolic link points at a directory, which is never followed.
    #[error("{0}: symbolic links to directories are not supported")]
    SymlinkDirectory(String),
    /// A name contains characters outside the allowed set or is not UTF-8.
    #[error("{0}: file names may only use ASCII letters, digits, '.', '_' and '-'")]
    InvalidName(String),
    /// The extension is missing or not on the served allowlist.
    #[error(
        "{0}: extension is not served (allowed: css, js, mjs, map, svg, png, jpg, jpeg, gif, webp, ico, woff2, txt)"
    )]
    UnsupportedExtension(String),
    /// More than [`MAX_FILES`] files.
    #[error("asset tree has more than {MAX_FILES} files")]
    TooManyFiles,
    /// A file is larger than [`MAX_FILE_BYTES`].
    #[error("{0}: file is larger than {MAX_FILE_BYTES} bytes")]
    FileTooLarge(String),
    /// Directories nest deeper than [`MAX_DEPTH`].
    #[error("{0}: directories nest deeper than {MAX_DEPTH} levels")]
    TooDeep(String),
    /// The crate path for the generated table is not `ident(::ident)*`.
    #[error("{0:?} is not a Rust crate path such as forge_views or forge::views")]
    InvalidCratePath(String),
}

fn io_error(path: &Path, source: io::Error) -> BuildError {
    BuildError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Scans `root` and returns its assets sorted by logical path, so the
/// generated table (and therefore the build) is deterministic.
pub fn scan(root: &Path) -> Result<Vec<ScannedAsset>, BuildError> {
    let canonical_root = fs::canonicalize(root).map_err(|source| io_error(root, source))?;
    let metadata = fs::metadata(&canonical_root).map_err(|source| io_error(root, source))?;
    if !metadata.is_dir() {
        return Err(BuildError::NotADirectory(root.display().to_string()));
    }
    let mut assets = Vec::new();
    walk(&canonical_root, &canonical_root, "", 0, &mut assets)?;
    assets.sort_by(|left, right| left.logical.cmp(&right.logical));
    Ok(assets)
}

fn walk(
    root: &Path,
    directory: &Path,
    logical_prefix: &str,
    depth: usize,
    assets: &mut Vec<ScannedAsset>,
) -> Result<(), BuildError> {
    if depth > MAX_DEPTH {
        return Err(BuildError::TooDeep(directory.display().to_string()));
    }
    let entries = fs::read_dir(directory).map_err(|source| io_error(directory, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(directory, source))?;
        let path = entry.path();
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            return Err(BuildError::InvalidName(path.display().to_string()));
        };
        if name.starts_with('.') {
            continue;
        }
        if !valid_component(name) {
            return Err(BuildError::InvalidName(path.display().to_string()));
        }
        let metadata = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
        let file_type = metadata.file_type();
        let logical = if logical_prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{logical_prefix}/{name}")
        };
        if file_type.is_dir() {
            walk(root, &path, &logical, depth + 1, assets)?;
        } else if file_type.is_symlink() {
            let target = fs::canonicalize(&path).map_err(|source| io_error(&path, source))?;
            if !target.starts_with(root) {
                return Err(BuildError::SymlinkEscape(path.display().to_string()));
            }
            let target_metadata =
                fs::metadata(&target).map_err(|source| io_error(&target, source))?;
            if target_metadata.is_dir() {
                return Err(BuildError::SymlinkDirectory(path.display().to_string()));
            }
            if target_metadata.is_file() {
                push_asset(assets, logical, &path, target)?;
            }
        } else if file_type.is_file() {
            push_asset(assets, logical, &path, path.clone())?;
        }
        // Sockets, FIFOs and devices are not assets and are skipped.
    }
    Ok(())
}

fn push_asset(
    assets: &mut Vec<ScannedAsset>,
    logical: String,
    display_path: &Path,
    contents_path: PathBuf,
) -> Result<(), BuildError> {
    let allowed = extension_of(&logical)
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| content_type(&extension).is_some());
    if !allowed {
        return Err(BuildError::UnsupportedExtension(
            display_path.display().to_string(),
        ));
    }
    if assets.len() >= MAX_FILES {
        return Err(BuildError::TooManyFiles);
    }
    let digest = digest_file(&contents_path, display_path)?;
    let digested = digested_name(&logical, &digest)
        .ok_or_else(|| BuildError::InvalidName(display_path.display().to_string()))?;
    assets.push(ScannedAsset {
        logical,
        digested,
        digest,
        path: contents_path,
    });
    Ok(())
}

/// Streams the file through SHA-256, refusing files larger than
/// [`MAX_FILE_BYTES`] even if they grow while being read.
fn digest_file(path: &Path, display_path: &Path) -> Result<String, BuildError> {
    let file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut limited = file.take(MAX_FILE_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    loop {
        let read = limited
            .read(&mut buffer)
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }
    if total > MAX_FILE_BYTES {
        return Err(BuildError::FileTooLarge(display_path.display().to_string()));
    }
    Ok(hex(&hasher.finalize()))
}

/// Renders the generated Rust source for `assets`.
///
/// Paths are written as Rust string literals, so any legal path (including
/// quotes or backslashes in parent directories) is embedded verbatim.
#[must_use]
pub fn render_table(assets: &[ScannedAsset]) -> String {
    render(assets, "forge_views")
}

/// Like [`render_table`], naming `AssetEntry` through `crate_path` (for
/// example `forge::views` when the application uses the `forge` facade).
pub fn render_table_with_crate_path(
    assets: &[ScannedAsset],
    crate_path: &str,
) -> Result<String, BuildError> {
    validate_crate_path(crate_path)?;
    Ok(render(assets, crate_path))
}

/// `crate_path` is spliced into generated Rust source, so it must be a plain
/// path of identifiers.
fn validate_crate_path(crate_path: &str) -> Result<(), BuildError> {
    let valid = crate_path.split("::").all(|segment| {
        let mut characters = segment.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
    });
    if valid {
        Ok(())
    } else {
        Err(BuildError::InvalidCratePath(crate_path.to_owned()))
    }
}

fn render(assets: &[ScannedAsset], crate_path: &str) -> String {
    let mut source = format!(
        "// @generated by forge_views::build::embed_assets. Do not edit.\n\
         pub static FORGE_ASSETS: &[{crate_path}::AssetEntry] = &[\n",
    );
    for asset in assets {
        // Writing to a `String` cannot fail.
        let _ = writeln!(
            source,
            "    {crate_path}::AssetEntry::new({:?}, {:?}, {:?}, include_bytes!({:?})),",
            asset.logical,
            asset.digested,
            asset.digest,
            asset.path.to_string_lossy(),
        );
    }
    source.push_str("];\n");
    source
}

/// Scans `root` and writes `forge_assets.rs` into `out_dir`, returning the
/// scanned assets. Used by [`embed_assets`]; callable directly in tests.
pub fn embed_assets_into(root: &Path, out_dir: &Path) -> Result<Vec<ScannedAsset>, BuildError> {
    write_table(root, out_dir, "forge_views")
}

fn write_table(
    root: &Path,
    out_dir: &Path,
    crate_path: &str,
) -> Result<Vec<ScannedAsset>, BuildError> {
    let assets = scan(root)?;
    let source = render_table_with_crate_path(&assets, crate_path)?;
    let target = out_dir.join(TABLE_FILE_NAME);
    fs::write(&target, source).map_err(|source| io_error(&target, source))?;
    Ok(assets)
}

/// Embeds the assets under `dir` (relative to `CARGO_MANIFEST_DIR`) for the
/// crate being built: writes `$OUT_DIR/forge_assets.rs` and tells Cargo to
/// rerun the build script when the tree changes.
pub fn embed_assets(dir: impl AsRef<Path>) -> Result<(), BuildError> {
    embed_assets_with_crate_path(dir, "forge_views")
}

/// Like [`embed_assets`], for applications that use this crate through a
/// re-export: the table names `AssetEntry` as `{crate_path}::AssetEntry`.
/// Generated Forge applications call it with `"forge::views"`.
pub fn embed_assets_with_crate_path(
    dir: impl AsRef<Path>,
    crate_path: &str,
) -> Result<(), BuildError> {
    validate_crate_path(crate_path)?;
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or(BuildError::MissingEnv("CARGO_MANIFEST_DIR"))?;
    let out_dir = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or(BuildError::MissingEnv("OUT_DIR"))?;
    let root = manifest_dir.join(dir.as_ref());
    println!("cargo:rerun-if-changed={}", root.display());
    let assets = write_table(&root, &out_dir, crate_path)?;
    for asset in &assets {
        println!("cargo:rerun-if-changed={}", asset.path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    fn tree() -> TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "app.css", "body{color:black}");
        write(dir.path(), "js/app.js", "console.log(1)");
        dir
    }

    #[test]
    fn scan_digests_and_sorts_files() {
        let dir = tree();

        let assets = scan(dir.path()).expect("scan");

        let logical: Vec<_> = assets.iter().map(|asset| asset.logical.as_str()).collect();
        assert_eq!(logical, ["app.css", "js/app.js"]);
        let css = &assets[0];
        assert_eq!(css.digest.len(), 64);
        assert!(css.digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(css.digested, format!("app-{}.css", &css.digest[..16]));
        assert_eq!(
            assets[1].digested,
            format!("js/app-{}.js", &assets[1].digest[..16])
        );
    }

    #[test]
    fn digest_is_the_known_sha256_of_the_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "a.txt", "abc");

        let assets = scan(dir.path()).expect("scan");

        assert_eq!(
            assets[0].digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn names(assets: &[ScannedAsset]) -> Vec<(String, String, String)> {
        assets
            .iter()
            .map(|asset| {
                (
                    asset.logical.clone(),
                    asset.digested.clone(),
                    asset.digest.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn digests_are_stable_and_change_with_content() {
        let first = tree();
        let second = tree();
        let before = scan(first.path()).expect("scan");

        // Same contents in a different directory: identical names and digests.
        assert_eq!(names(&before), names(&scan(second.path()).expect("scan")));

        write(first.path(), "app.css", "body{color:white}");
        let after = scan(first.path()).expect("scan");

        assert_ne!(before[0].digest, after[0].digest);
        assert_ne!(before[0].digested, after[0].digested);
        assert_eq!(before[1].digested, after[1].digested);
    }

    #[test]
    fn dotfiles_and_dot_directories_are_ignored() {
        let dir = tree();
        write(dir.path(), ".env", "SECRET=1");
        write(dir.path(), ".git/config", "[core]");
        write(dir.path(), "js/.hidden.js", "x");

        let assets = scan(dir.path()).expect("scan");

        let logical: Vec<_> = assets.iter().map(|asset| asset.logical.as_str()).collect();
        assert_eq!(logical, ["app.css", "js/app.js"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_escaping_the_root_are_rejected() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().expect("outside dir");
        write(outside.path(), "secret.txt", "secret");
        let dir = tree();
        symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .expect("create symlink");

        let failure = scan(dir.path()).expect_err("escaping link");

        assert!(matches!(failure, BuildError::SymlinkEscape(_)), "{failure}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_directories_and_dangling_links_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tree();
        symlink(dir.path().join("js"), dir.path().join("alias")).expect("dir link");
        assert!(matches!(
            scan(dir.path()).expect_err("dir link"),
            BuildError::SymlinkDirectory(_)
        ));

        let dangling = tree();
        symlink(
            dangling.path().join("missing.css"),
            dangling.path().join("x.css"),
        )
        .expect("dangling link");
        assert!(matches!(
            scan(dangling.path()).expect_err("dangling"),
            BuildError::Io { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_files_inside_the_root_are_embedded() {
        use std::os::unix::fs::symlink;
        let dir = tree();
        symlink(dir.path().join("app.css"), dir.path().join("alias.css")).expect("file link");

        let assets = scan(dir.path()).expect("scan");

        let alias = assets
            .iter()
            .find(|asset| asset.logical == "alias.css")
            .expect("alias");
        let original = assets
            .iter()
            .find(|asset| asset.logical == "app.css")
            .expect("original");
        assert_eq!(alias.digest, original.digest);
    }

    #[test]
    fn unsupported_extensions_and_names_are_rejected() {
        for (name, expected) in [
            ("page.html", "UnsupportedExtension"),
            ("noext", "UnsupportedExtension"),
            ("my file.css", "InvalidName"),
            ("a{b}.css", "InvalidName"),
        ] {
            let dir = tempfile::tempdir().expect("temp dir");
            write(dir.path(), name, "x");

            let failure = scan(dir.path()).expect_err(name);

            assert!(
                format!("{failure:?}").starts_with(expected),
                "{name}: {failure:?}"
            );
        }
    }

    #[test]
    fn extension_matching_is_case_insensitive_but_names_keep_case() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "Logo.PNG", "x");

        let assets = scan(dir.path()).expect("scan");

        assert!(assets[0].digested.starts_with("Logo-"));
        assert!(assets[0].digested.ends_with(".PNG"));
    }

    #[test]
    fn missing_or_non_directory_roots_are_errors() {
        let dir = tree();
        assert!(matches!(
            scan(&dir.path().join("nope")).expect_err("missing"),
            BuildError::Io { .. }
        ));
        assert!(matches!(
            scan(&dir.path().join("app.css")).expect_err("file"),
            BuildError::NotADirectory(_)
        ));
    }

    #[test]
    fn deep_trees_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nested = vec!["d"; MAX_DEPTH + 2].join("/");
        write(dir.path(), &format!("{nested}/a.css"), "x");

        assert!(matches!(
            scan(dir.path()).expect_err("too deep"),
            BuildError::TooDeep(_)
        ));
    }

    #[test]
    fn table_lists_every_asset_with_include_bytes() {
        let dir = tree();
        let out = tempfile::tempdir().expect("out dir");

        let assets = embed_assets_into(dir.path(), out.path()).expect("embed");
        let source = fs::read_to_string(out.path().join(TABLE_FILE_NAME)).expect("table");

        assert!(source.contains("pub static FORGE_ASSETS: &[forge_views::AssetEntry]"));
        assert_eq!(source.matches("include_bytes!").count(), assets.len());
        for asset in &assets {
            assert!(source.contains(&format!("{:?}", asset.digested)));
            assert!(source.contains(&asset.digest));
        }
    }

    #[test]
    fn table_can_name_a_re_exported_crate_path() {
        let dir = tree();
        let assets = scan(dir.path()).expect("scan");

        let source = render_table_with_crate_path(&assets, "forge::views").expect("valid path");

        assert!(source.contains("pub static FORGE_ASSETS: &[forge::views::AssetEntry]"));
        assert_eq!(
            source.matches("forge::views::AssetEntry::new(").count(),
            assets.len()
        );
        assert!(!source.contains("forge_views::AssetEntry::new("));
        for invalid in [
            "",
            "forge::",
            "::forge",
            "forge views",
            "forge;evil",
            "1forge",
            "a::b-c",
        ] {
            assert!(
                matches!(
                    render_table_with_crate_path(&assets, invalid),
                    Err(BuildError::InvalidCratePath(_))
                ),
                "{invalid:?}"
            );
        }
    }
}
