//! Creating an environment's key and encrypted skeleton.

use std::path::{Path, PathBuf};

use forge_encryption::{encode_key, generate_key};
use serde_json::json;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    CredentialsError, file_name, key_file_name, load_keys, new_key, seal, validate_environment,
    write_file, write_key_file,
};

/// A failed [`init`], with the files it created before failing.
#[derive(Debug, Error)]
#[error("{source}")]
pub struct InitError {
    /// Files created before the failure (a new key file, for example).
    pub created: Vec<PathBuf>,
    /// The cause.
    #[source]
    pub source: CredentialsError,
}

/// Creates the credentials of `environment` under `dir`: a new random key in
/// `<environment>.key` (mode 0600) and an encrypted skeleton holding a
/// data-encryption key ring for attribute encryption (active key `k1`). It
/// refuses to replace an existing credentials file and never overwrites a
/// key; when the key file already exists it is used, so a restored key can
/// start a new file. Returns the paths it created.
///
/// Callers must keep the key file out of version control; `forge credentials
/// init` and `forge new` add the `.gitignore` entry.
pub fn init(dir: &Path, environment: &str) -> Result<Vec<PathBuf>, InitError> {
    let mut created = Vec::new();
    let fail = |created: Vec<PathBuf>, source: CredentialsError| InitError { created, source };
    if let Err(source) = validate_environment(environment) {
        return Err(fail(created, source));
    }
    // Only the key file counts here; FORGE_MASTER_KEY never starts a file.
    let keys = match load_keys(dir, environment, &|_| None) {
        Ok((keys, _)) => keys,
        Err(CredentialsError::KeyMissing) => {
            let key = generate_key()
                .map_err(CredentialsError::from)
                .and_then(new_key)
                .map_err(|source| fail(Vec::new(), source))?;
            let keys = vec![key];
            write_key_file(dir, environment, &keys, false)
                .map_err(|source| fail(Vec::new(), source))?;
            created.push(dir.join(key_file_name(environment)));
            keys
        }
        Err(source) => return Err(fail(created, source)),
    };
    let sealed = match skeleton().and_then(|document| seal(environment, &document, &keys[0])) {
        Ok(sealed) => sealed,
        Err(source) => return Err(fail(created, source)),
    };
    let path = dir.join(file_name(environment));
    if let Err(source) = write_file(dir, environment, &sealed, false) {
        return Err(fail(created, source));
    }
    created.push(path);
    Ok(created)
}

fn skeleton() -> Result<Zeroizing<Vec<u8>>, CredentialsError> {
    let key = generate_key()?;
    let mut document = json!({
        "encryption": {
            "active": "k1",
            "keys": { "k1": encode_key(&key).as_str() },
        }
    });
    let text = serde_json::to_vec_pretty(&document);
    crate::document::wipe(&mut document);
    let mut text = Zeroizing::new(
        text.map_err(|_| CredentialsError::Document("cannot encode the skeleton".to_owned()))?,
    );
    text.push(b'\n');
    Ok(text)
}
