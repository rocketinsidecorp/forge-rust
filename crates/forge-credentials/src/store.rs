//! The decrypted document an application reads at startup.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use forge_config::SecretString;
use forge_encryption::KeyRing;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroize as _;

use crate::document::{parse_document, wipe_map};
use crate::{
    CredentialsError, DEFAULT_DIR, ENV_MASTER_KEY, MAX_FILE_BYTES, load_keys, parse_keys,
    read_file, unseal, validate_environment,
};

/// Reads a process environment variable; tests substitute their own.
pub type Lookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// Tunes [`open`]. The default reads `config/credentials` relative to the
/// working directory and the key from the process environment.
#[derive(Clone, Default)]
pub struct Options {
    dir: Option<PathBuf>,
    embedded: Option<&'static [u8]>,
    lookup: Option<Arc<Lookup>>,
    required: Vec<String>,
}

impl Options {
    /// The credentials directory; `config/credentials` by default.
    #[must_use]
    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// The encrypted file embedded in the binary (`include_bytes!`) instead of
    /// the disk. The key then must come from `FORGE_MASTER_KEY`; key files are
    /// never embedded.
    #[must_use]
    pub fn with_embedded(mut self, file: &'static [u8]) -> Self {
        self.embedded = Some(file);
        self
    }

    /// Reads `FORGE_MASTER_KEY` through `lookup` instead of the process
    /// environment.
    #[must_use]
    pub fn with_lookup(
        mut self,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.lookup = Some(Arc::new(lookup));
        self
    }

    /// Dotted paths (for example `stripe.api_key`) that must exist as
    /// non-empty values. They are enforced outside development and test, so
    /// a production process fails at startup instead of on first use.
    #[must_use]
    pub fn with_required<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.required = paths.into_iter().map(Into::into).collect();
        self
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Options")
            .field("dir", &self.dir)
            .field("embedded", &self.embedded.map(<[u8]>::len))
            .field("lookup", &self.lookup.as_ref().map(|_| "custom"))
            .field("required", &self.required)
            .finish()
    }
}

fn process_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Missing credentials are an error everywhere except development and test.
fn strict(environment: &str) -> bool {
    environment != "development" && environment != "test"
}

/// Reads and decrypts the credentials of `environment` with default options.
pub fn load(environment: &str) -> Result<Store, CredentialsError> {
    open(environment, Options::default())
}

/// Reads and decrypts the credentials of `environment`.
///
/// In development and test a missing file yields an empty [`Store`]
/// ([`Store::exists`] is false). In every other environment, such as
/// production, a missing file, a missing key or a missing required credential
/// is an error. In all environments an existing file that cannot be
/// decrypted is an error: credentials fail closed.
pub fn open(environment: &str, options: Options) -> Result<Store, CredentialsError> {
    validate_environment(environment)?;
    let dir = options
        .dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    let lookup: &Lookup = match &options.lookup {
        Some(lookup) => lookup.as_ref(),
        None => &process_lookup,
    };
    let file = match options.embedded {
        Some(file) if file.len() > MAX_FILE_BYTES => {
            return Err(CredentialsError::Format("file is too large".to_owned()));
        }
        Some(file) => Ok(file.to_vec()),
        None => read_file(&dir, environment),
    };
    let file = match file {
        Ok(file) => file,
        Err(CredentialsError::NotFound(path)) => {
            if strict(environment) {
                return Err(CredentialsError::NotFound(path));
            }
            return Ok(Store::new(environment, Map::new(), false));
        }
        Err(error) => return Err(error),
    };
    let keys = if options.embedded.is_some() {
        let value = lookup(ENV_MASTER_KEY)
            .filter(|value| !value.trim().is_empty())
            .map(zeroize::Zeroizing::new)
            .ok_or(CredentialsError::KeyMissing)?;
        parse_keys(&value).map_err(|error| match error {
            CredentialsError::InvalidKey(message) => {
                CredentialsError::InvalidKey(format!("{ENV_MASTER_KEY}: {message}"))
            }
            other => other,
        })?
    } else {
        load_keys(&dir, environment, lookup)?.0
    };
    let plaintext = unseal(environment, &file, &keys)?;
    let store = Store::new(environment, parse_document(&plaintext)?, true);
    if strict(environment) {
        let required: Vec<&str> = options.required.iter().map(String::as_str).collect();
        store.require(&required)?;
    }
    Ok(store)
}

/// Builds a [`Store`] from a decrypted document, for tools such as the forge
/// CLI that already hold the plaintext.
pub fn parse(environment: &str, plaintext: &[u8]) -> Result<Store, CredentialsError> {
    validate_environment(environment)?;
    Ok(Store::new(environment, parse_document(plaintext)?, true))
}

struct StoreData {
    environment: String,
    document: Map<String, Value>,
    exists: bool,
}

impl Drop for StoreData {
    fn drop(&mut self) {
        wipe_map(&mut self.document);
    }
}

/// A decrypted credentials document. Immutable, cheap to clone and safe to
/// share between threads. Debug, Display and serialization print
/// `credentials.Store(<environment>, [REDACTED])`, never its content, and its
/// strings are overwritten when the last clone is dropped.
#[derive(Clone)]
pub struct Store {
    data: Arc<StoreData>,
}

impl Store {
    fn new(environment: &str, document: Map<String, Value>, exists: bool) -> Self {
        Self {
            data: Arc::new(StoreData {
                environment: environment.to_owned(),
                document,
                exists,
            }),
        }
    }

    /// The environment the store belongs to.
    #[must_use]
    pub fn environment(&self) -> &str {
        &self.data.environment
    }

    /// Whether a credentials file was read. False only for a development or
    /// test environment without a file.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.data.exists
    }

    fn lookup(&self, path: &[&str]) -> Result<&Value, CredentialsError> {
        let missing = || CredentialsError::Missing(path.join("."));
        let Some((first, rest)) = path.split_first() else {
            return Err(CredentialsError::Missing(String::new()));
        };
        let mut current = self.data.document.get(*first).ok_or_else(missing)?;
        for key in rest {
            current = current
                .as_object()
                .and_then(|object| object.get(*key))
                .ok_or_else(missing)?;
        }
        Ok(current)
    }

    /// Whether `path` exists.
    #[must_use]
    pub fn has(&self, path: &[&str]) -> bool {
        self.lookup(path).is_ok()
    }

    /// The string at `path`. A missing path is [`CredentialsError::Missing`],
    /// another JSON type [`CredentialsError::Type`]; errors name the path,
    /// never the value. Prefer [`secret`](Self::secret) for secrets.
    pub fn string(&self, path: &[&str]) -> Result<String, CredentialsError> {
        match self.lookup(path)? {
            Value::String(text) => Ok(text.clone()),
            other => Err(CredentialsError::Type {
                path: path.join("."),
                found: json_type(other),
                wanted: "a string",
            }),
        }
    }

    /// The string at `path` as a [`SecretString`], which prints as
    /// `[REDACTED]`; call `expose` at the point of use.
    pub fn secret(&self, path: &[&str]) -> Result<SecretString, CredentialsError> {
        self.string(path).map(SecretString::new)
    }

    /// Deserializes the value at `path` (the whole document when `path` is
    /// empty) into `T`, typically a struct deriving `Deserialize`. Unknown
    /// fields are ignored unless `T` denies them.
    pub fn decode<T: DeserializeOwned>(&self, path: &[&str]) -> Result<T, CredentialsError> {
        let decoded = if path.is_empty() {
            T::deserialize(&self.data.document)
        } else {
            T::deserialize(self.lookup(path)?)
        };
        // serde_json's messages can quote values; report the path only.
        decoded.map_err(|_| CredentialsError::Decode(path.join(".")))
    }

    /// The data-encryption key ring stored at `path`, in the form
    ///
    /// ```json
    /// {"active": "2026-10", "keys": {"2026-10": "<base64url>", "2025-03": "<base64url>"}}
    /// ```
    ///
    /// ready for [`forge_encryption::Cipher::new`]. Keeping data-encryption
    /// keys in the credentials means one master key protects them, and
    /// rotation is an edit: add a key, make it active, re-encrypt, remove the
    /// old key.
    pub fn key_ring(&self, path: &[&str]) -> Result<KeyRing, CredentialsError> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Ring {
            active: String,
            keys: BTreeMap<String, String>,
        }
        let mut ring: Ring = self.decode(path)?;
        let parsed = KeyRing::parse(&ring.active, &ring.keys);
        ring.keys.values_mut().for_each(|text| text.zeroize());
        Ok(parsed?)
    }

    /// Checks that every dotted path (for example `stripe.api_key`) exists
    /// and is a non-empty string, a number, a boolean or a non-empty object or
    /// array, and reports all that are not at once.
    pub fn require(&self, paths: &[&str]) -> Result<(), CredentialsError> {
        let mut missing: Vec<String> = paths
            .iter()
            .filter(|dotted| {
                let path: Vec<&str> = dotted.split('.').collect();
                self.lookup(&path).map_or(true, is_empty)
            })
            .map(|dotted| (*dotted).to_owned())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        missing.sort();
        Err(CredentialsError::MissingRequired {
            environment: self.data.environment.clone(),
            paths: missing,
        })
    }

    fn redacted(&self) -> String {
        format!("credentials.Store({}, [REDACTED])", self.data.environment)
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::String(_) => "a string",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a boolean",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted())
    }
}

impl fmt::Display for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted())
    }
}

impl Serialize for Store {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.redacted())
    }
}
