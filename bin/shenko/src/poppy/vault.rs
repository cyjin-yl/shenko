//! Node-local credential storage. Neither the encryption key nor its contents
//! enter Node packages, shared CAS, task inputs, reports, or Debug output.
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use zeroize::Zeroizing;

const MAGIC: &[u8] = b"SHENKO-POPPY-VAULT-1\0";
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The OS lock serializes separate CLI processes and live plugin generations.
/// Holding it across a token rotation prevents two writers spending the same
/// refresh token. A busy vault is reported before dispatch, never retried here.
pub(super) struct Vault {
    root: PathBuf,
    _lock: VaultLock,
    key: aead::LessSafeKey,
}

struct VaultLock(File);

impl Drop for VaultLock {
    fn drop(&mut self) {
        // A forked child can still hold this open file description before exec.
        // End the owner's lock lifetime even if that descriptor survives briefly.
        let _ = self.0.unlock();
    }
}

impl Vault {
    pub fn open(data_dir: &Path, create: bool) -> Result<Self> {
        ensure!(
            cfg!(unix),
            "this Poppy vault requires Unix owner permissions; native ACL support is not implemented on this platform"
        );
        let root = data_dir.join("poppy-private");
        if create {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&root) {
                Ok(()) => File::open(data_dir)?.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let metadata = fs::symlink_metadata(&root)
            .context("Poppy identity is not provisioned on this node; run compat poppy init")?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "invalid credential directory"
        );
        private_owner(&metadata)?;
        let root = root.canonicalize()?;
        let lock = open_private(&root.join("lock"), true)?;
        lock.try_lock()
            .context("Poppy credentials are in use by another operation")?;
        let lock = VaultLock(lock);
        let key_path = root.join("key");
        let mut key_bytes = match read_private(&key_path) {
            Ok(bytes) => bytes,
            Err(error)
                if create
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                // A missing key beside an old record is damaged state, not a
                // request to silently replace the account's identity.
                ensure!(
                    fs::read_dir(&root)?
                        .all(|entry| entry.is_ok_and(|entry| entry.file_name() == "lock")),
                    "credential key is missing from a nonempty vault"
                );
                let mut bytes = Zeroizing::new(vec![0_u8; 32]);
                random(&mut bytes)?;
                write_new(&key_path, &bytes)?;
                File::open(&root)?.sync_all()?;
                bytes
            }
            Err(error) => return Err(error.context("cannot read Poppy credential key")),
        };
        ensure!(key_bytes.len() == 32, "invalid credential key length");
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key_bytes)
            .map_err(|_| anyhow::anyhow!("cannot initialize credential encryption"))?;
        key_bytes.fill(0);
        Ok(Self {
            root,
            _lock: lock,
            key: aead::LessSafeKey::new(key),
        })
    }

    pub fn read(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let path = self.path(name)?;
        let mut sealed = match read_private(&path) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        ensure!(
            sealed.starts_with(MAGIC)
                && sealed.len() >= MAGIC.len() + 12 + aead::AES_256_GCM.tag_len(),
            "unsupported or truncated credential record"
        );
        let mut nonce = [0_u8; 12];
        nonce.copy_from_slice(&sealed[MAGIC.len()..MAGIC.len() + 12]);
        let payload = &mut sealed[MAGIC.len() + 12..];
        let plaintext = self
            .key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(name.as_bytes()),
                payload,
            )
            .map_err(|_| anyhow::anyhow!("credential record authentication failed"))?;
        Ok(Some(Zeroizing::new(plaintext.to_vec())))
    }

    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        ensure!(
            !immutable_reference(name),
            "instrument and commerce request references are immutable; use a new reference"
        );
        self.write_record(name, bytes, false)
    }

    /// Publish a fully synced encrypted record without replacing an existing
    /// identity. The vault lock covers both CLI imports and live send owners.
    pub fn write_immutable(&self, name: &str, bytes: &[u8]) -> Result<()> {
        ensure!(immutable_reference(name), "not an immutable private reference");
        self.write_record(name, bytes, true)
    }

    fn write_record(&self, name: &str, bytes: &[u8], immutable: bool) -> Result<()> {
        ensure!(
            bytes.len() as u64 <= MAX_BYTES - 128,
            "credential record exceeds size bound"
        );
        let destination = self.path(name)?;
        let mut nonce = [0_u8; 12];
        random(&mut nonce)?;
        let mut sealed = Zeroizing::new(bytes.to_vec());
        self.key
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(name.as_bytes()),
                &mut *sealed,
            )
            .map_err(|_| anyhow::anyhow!("credential encryption failed"))?;
        let mut wire = Zeroizing::new(Vec::with_capacity(MAGIC.len() + 12 + sealed.len()));
        wire.extend_from_slice(MAGIC);
        wire.extend_from_slice(&nonce);
        wire.extend_from_slice(&sealed);
        let temporary = self.root.join(format!("staging-{}", uuid::Uuid::new_v4()));
        write_new(&temporary, &wire)?;
        let result = if immutable {
            fs::hard_link(&temporary, &destination)
                .and_then(|()| fs::remove_file(&temporary))
        } else {
            fs::rename(&temporary, &destination)
        }
        .and_then(|()| File::open(&self.root)?.sync_all());
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.context("persisting node-local credential state")
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        ensure!(
            !immutable_reference(name),
            "cannot remove an immutable instrument or commerce request reference"
        );
        match fs::remove_file(self.path(name)?) {
            Ok(()) => File::open(&self.root)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn path(&self, name: &str) -> Result<PathBuf> {
        ensure!(
            !name.is_empty()
                && name.len() <= 160
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "invalid node-local credential reference"
        );
        Ok(self.root.join(format!("{name}.sealed")))
    }
}

fn immutable_reference(name: &str) -> bool {
    name.starts_with("instrument-") || name.starts_with("commerce-request-")
}

fn random(bytes: &mut [u8]) -> Result<()> {
    SystemRandom::new()
        .fill(bytes)
        .map_err(|_| anyhow::anyhow!("secure random source unavailable"))
}

fn private_owner(metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        cfg!(unix),
        "owner-private credential files are unsupported on this platform"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "credential state is not private to this installation owner"
        );
        ensure!(
            !metadata.is_file() || metadata.nlink() == 1,
            "credential file has additional links"
        );
    }
    Ok(())
}

fn open_private(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_BYTES,
        "invalid credential file"
    );
    private_owner(&metadata)?;
    Ok(file)
}

pub(super) fn read_private(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let file = open_private(path, false)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "credential file exceeds size bound"
    );
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn dropped_vault_releases_lock_with_an_inherited_descriptor() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("shenko-poppy-lock-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        let result = (|| -> Result<()> {
            let owner = Vault::open(&directory, true)?;
            // Like a descriptor inherited across fork, this shares the lock's
            // open file description and survives the parent's close.
            let inherited = owner._lock.0.try_clone()?;
            let contender = open_private(&owner.root.join("lock"), true)?;
            assert!(matches!(
                contender.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            drop(owner);
            let next = Vault::open(&directory, false)?;
            drop(inherited);
            assert!(matches!(
                contender.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            drop(next);
            contender.try_lock()?;
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&directory);
        result?;
        cleanup?;
        Ok(())
    }
}
