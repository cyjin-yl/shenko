//! The subject's encounter signing key.

use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

const PREFIX: &str = "ed25519:";

/// A subject's public identity: the Ed25519 public key, `ed25519:<hex>`.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct PublicIdentity([u8; 32]);

impl PublicIdentity {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&ED25519, &self.0)
            .verify(message, signature)
            .is_ok()
    }
}

impl fmt::Display for PublicIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}", hex::encode(self.0))
    }
}

impl TryFrom<String> for PublicIdentity {
    type Error = KeyError;
    fn try_from(text: String) -> Result<Self, KeyError> {
        let hex_part = text
            .strip_prefix(PREFIX)
            .ok_or(KeyError::MalformedIdentity)?;
        let bytes = hex::decode(hex_part).map_err(|_| KeyError::MalformedIdentity)?;
        Ok(Self(
            bytes.try_into().map_err(|_| KeyError::MalformedIdentity)?,
        ))
    }
}

impl From<PublicIdentity> for String {
    fn from(identity: PublicIdentity) -> String {
        identity.to_string()
    }
}

#[derive(Debug)]
pub enum KeyError {
    Io(std::io::Error),
    /// The key file exists but is not a regular owner-only file holding a
    /// valid PKCS#8 Ed25519 key. It is never replaced silently: replacing it
    /// would change the subject's identity.
    Unusable(String),
    MalformedIdentity,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "encounter key I/O failed: {error}"),
            Self::Unusable(reason) => write!(f, "encounter key is unusable: {reason}"),
            Self::MalformedIdentity => f.write_str("malformed public identity"),
        }
    }
}

impl std::error::Error for KeyError {}

/// The subject's Ed25519 signing key, held in one owner-only PKCS#8 file.
pub struct SubjectKey {
    pair: Ed25519KeyPair,
    identity: PublicIdentity,
}

impl SubjectKey {
    /// Load the key at `path`, or create it if no file exists.
    ///
    /// Creation is exclusive (`create_new`) with mode 0600, so two processes
    /// cannot each mint a different identity. Opening never follows a
    /// symlink, and a file readable by group or others is refused.
    pub fn load_or_create(path: &Path) -> Result<Self, KeyError> {
        match Self::load(path) {
            Err(KeyError::Io(error)) if error.kind() == ErrorKind::NotFound => {}
            other => return other,
        }
        let rng = SystemRandom::new();
        let document = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| KeyError::Unusable("key generation failed".into()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(KeyError::Io)?;
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(document.as_ref()).map_err(KeyError::Io)?;
                file.sync_all().map_err(KeyError::Io)?;
                Self::from_pkcs8(document.as_ref())
            }
            // Another process created it first: use that identity.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Self::load(path),
            Err(error) => Err(KeyError::Io(error)),
        }
    }

    /// Load an existing key; never creates one.
    pub fn load(path: &Path) -> Result<Self, KeyError> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| match error.raw_os_error() {
                Some(libc::ELOOP) => KeyError::Unusable("key path is a symlink".into()),
                _ => KeyError::Io(error),
            })?;
        let metadata = file.metadata().map_err(KeyError::Io)?;
        if !metadata.is_file() {
            return Err(KeyError::Unusable("key path is not a regular file".into()));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(KeyError::Unusable(
                "key file is readable by group or others".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(KeyError::Io)?;
        Self::from_pkcs8(&bytes)
    }

    fn from_pkcs8(bytes: &[u8]) -> Result<Self, KeyError> {
        let pair = Ed25519KeyPair::from_pkcs8(bytes)
            .map_err(|_| KeyError::Unusable("not a PKCS#8 Ed25519 key".into()))?;
        let identity = PublicIdentity(
            <[u8; 32]>::try_from(pair.public_key().as_ref())
                .map_err(|_| KeyError::Unusable("public key is not 32 bytes".into()))?,
        );
        Ok(Self { pair, identity })
    }

    pub fn identity(&self) -> &PublicIdentity {
        &self.identity
    }

    /// Sign `message`; the result is a 64-byte Ed25519 signature.
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shenko-encounter-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_persists_owner_only_and_keeps_its_identity() {
        let dir = scratch("persist");
        let path = dir.join("keys/subject-ed25519.pk8");
        let first = SubjectKey::load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let again = SubjectKey::load_or_create(&path).unwrap();
        assert_eq!(first.identity(), again.identity());
        let signature = again.sign(b"hello");
        assert!(first.identity().verify(b"hello", &signature));
        assert!(!first.identity().verify(b"hellO", &signature));
        let text = first.identity().to_string();
        assert_eq!(PublicIdentity::try_from(text).unwrap(), *first.identity());
    }

    #[test]
    fn unsafe_key_files_are_refused_not_replaced() {
        let dir = scratch("refuse");
        let path = dir.join("k.pk8");
        SubjectKey::load_or_create(&path).unwrap();
        let original = std::fs::read(&path).unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            SubjectKey::load_or_create(&path),
            Err(KeyError::Unusable(_))
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let link = dir.join("link.pk8");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(matches!(
            SubjectKey::load_or_create(&link),
            Err(KeyError::Unusable(_))
        ));

        let garbage = dir.join("garbage.pk8");
        std::fs::write(&garbage, b"not a key").unwrap();
        std::fs::set_permissions(&garbage, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            SubjectKey::load_or_create(&garbage),
            Err(KeyError::Unusable(_))
        ));
        assert_eq!(std::fs::read(&garbage).unwrap(), b"not a key");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
}
