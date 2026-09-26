//! Persistent iroh identity (`~/.blirp/identity.key`, 0600): 64 hex chars
//! of the Ed25519 secret key. The machine id is the public key in hex.

use iroh::SecretKey;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("cannot read identity key {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "identity key {0} is corrupt (expected 64 hex characters); move it away to create a new identity (this unpairs the machine)"
    )]
    Corrupt(PathBuf),
    #[error("cannot write identity key: {0}")]
    Write(#[from] blirp_core::paths::PathsError),
    #[error("cannot generate identity key: {0}")]
    Random(String),
}

/// Load the key at `path`, creating it on first use.
pub fn load_or_create(path: &Path) -> Result<SecretKey, IdentityError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let bytes = data_encoding::HEXLOWER_PERMISSIVE
                .decode(text.trim().as_bytes())
                .map_err(|_| IdentityError::Corrupt(path.to_path_buf()))?;
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| IdentityError::Corrupt(path.to_path_buf()))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|e| IdentityError::Random(e.to_string()))?;
            let key = SecretKey::from_bytes(&bytes);
            let text = format!("{}\n", data_encoding::HEXLOWER.encode(&bytes));
            let tmp = path.with_extension("key.tmp");
            blirp_core::paths::write_private(&tmp, text.as_bytes())?;
            std::fs::rename(&tmp, path).map_err(|source| IdentityError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(key)
        }
        Err(source) => Err(IdentityError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Machine id of a key: the endpoint id in lowercase hex.
pub fn machine_id(key: &SecretKey) -> String {
    key.public().to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn created_once_then_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let a = load_or_create(&path).unwrap();
        let b = load_or_create(&path).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(machine_id(&a).len(), 64);
        std::fs::write(&path, "nope").unwrap();
        assert!(matches!(
            load_or_create(&path),
            Err(IdentityError::Corrupt(_))
        ));
    }
}
