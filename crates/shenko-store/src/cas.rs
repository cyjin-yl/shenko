//! Content-addressed object store. SPEC §41.2.2
//! Objects are immutable; locators may change.
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use anyhow::Result;
use shenko_types::object::{Digest, ObjectRef, SchemaRef, SealedObjectRef};
use shenko_types::error::ShenkoError;

pub struct ContentStore {
    base: PathBuf,
}

impl ContentStore {
    pub fn new(base: PathBuf) -> Result<Self, ShenkoError> {
        fs::create_dir_all(&base)
            .map_err(|error| ShenkoError::storage(format!("CAS directory creation: {error}")))?;
        // A newly created CAS root may itself have newly created parents.
        // Sync the whole ancestry once, before admitting durable objects.
        for directory in base.ancestors() {
            sync_directory(directory)
                .map_err(|error| ShenkoError::storage(format!("CAS directory sync: {error}")))?;
        }
        Ok(Self { base })
    }

    fn object_path(&self, digest: &Digest) -> Result<PathBuf, ShenkoError> {
        // Validate BEFORE slicing or resolving paths. A Digest is deserialized
        // from untrusted nested objects too; its wrapper alone proves nothing.
        let hex = digest.0.strip_prefix("blake3:").filter(|hex| {
            hex.len() == 64 && hex.bytes().all(|byte|
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }).ok_or_else(|| ShenkoError::conflict("CAS digest must be canonical blake3 lowercase hex"))?;
        Ok(self.base.join(&hex[..2]).join(&hex[2..4]).join(hex))
    }

    pub fn seal_object(
        &self,
        data: &[u8],
        media_type: &str,
        namespace: &str,
        codec_name: &str,
        codec_major: u32,
    ) -> Result<SealedObjectRef, ShenkoError> {
        let digest = Digest::blake3(data);
        let path = self.object_path(&digest)?;

        let result = match fs::File::open(&path) {
            Ok(mut existing) => verify_file(&mut existing, &digest, data.len() as u64)
                .and_then(|()| self.sync_object_directories(&path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let temporary = self.base.join(format!(".object-{}", shenko_types::ids::EntityId::new()));
                let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)
                    .map_err(|error| ShenkoError::storage(format!("CAS staging creation: {error}")))?;
                let result = (|| {
                    output.write_all(data)?;
                    output.sync_all()?;
                    self.publish_temporary(&temporary, &path, &digest, data.len() as u64)
                })();
                drop(output);
                // Cleanup owns only a staging file successfully created here.
                let _ = fs::remove_file(&temporary);
                result
            }
            Err(error) => Err(error),
        };
        result.map_err(|error| ShenkoError::storage(format!("CAS object sealing: {error}")))?;

        let locator = format!("file://{}", path.display());
        let codec = SchemaRef {
            name: codec_name.to_string(),
            major: codec_major,
            digest: Digest::blake3(codec_name.as_bytes()),
        };

        let object_ref = ObjectRef {
            digest: digest.clone(),
            bytes: data.len() as u64,
            media_type: media_type.to_string(),
            store_namespace: namespace.to_string(),
            locator: locator.clone(),
            codec,
        };

        Ok(SealedObjectRef {
            object: object_ref,
            sealed_at: chrono::Utc::now(),
            sealed_commit: shenko_types::commit::CommitRef {
                ledger_id: shenko_types::ids::EntityId::nil(),
                seq: shenko_types::ids::LedgerSeq::ZERO,
            },
        })
    }

    /// Seal a full stream with fixed-size memory, for execution output that
    /// must not be loaded into an inline normalized result. A unique staging
    /// file is published without replacing any existing content-addressed
    /// object; interrupted attempts do not reserve a deterministic filename.
    pub fn seal_reader(
        &self,
        reader: &mut impl Read,
        media_type: &str,
        namespace: &str,
        codec_name: &str,
        codec_major: u32,
    ) -> Result<SealedObjectRef, ShenkoError> {
        let temporary = self.base.join(format!(".stream-{}", shenko_types::ids::EntityId::new()));
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)
            .map_err(|error| ShenkoError::storage(format!("CAS staging creation: {error}")))?;
        let result = (|| -> std::io::Result<(Digest, u64)> {
            let mut buffer = [0u8; 64 * 1024];
            let mut hash = blake3::Hasher::new();
            let mut bytes = 0u64;
            loop {
                let count = match reader.read(&mut buffer) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    other => other?,
                };
                if count == 0 { break; }
                output.write_all(&buffer[..count])?;
                hash.update(&buffer[..count]);
                bytes += count as u64;
            }
            output.sync_all()?;
            let digest = Digest(format!("blake3:{}", hash.finalize().to_hex()));
            let destination = self.object_path(&digest)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            self.publish_temporary(&temporary, &destination, &digest, bytes)?;
            Ok((digest, bytes))
        })();
        drop(output);
        // Only this invocation's unpredictable staging name is removed.
        let _ = fs::remove_file(&temporary);
        let (digest, bytes) = result.map_err(|error| ShenkoError::storage(format!("CAS stream sealing: {error}")))?;
        Ok(SealedObjectRef {
            object: ObjectRef {
                locator: format!("file://{}", self.object_path(&digest)?.display()),
                digest,
                bytes,
                media_type: media_type.into(),
                store_namespace: namespace.into(),
                codec: SchemaRef {
                    name: codec_name.into(),
                    major: codec_major,
                    digest: Digest::blake3(codec_name.as_bytes()),
                },
            },
            sealed_at: chrono::Utc::now(),
            sealed_commit: shenko_types::commit::CommitRef::new(
                shenko_types::ids::EntityId::nil(), shenko_types::ids::LedgerSeq::ZERO,
            ),
        })
    }

    pub fn read_object(&self, digest: &Digest) -> Result<Vec<u8>, ShenkoError> {
        let path = self.object_path(digest)?;
        fs::read(&path).map_err(|e| ShenkoError::not_found(format!("cas read {digest}: {e}")))
    }

    /// Resolve immutable content in THIS store, never a historical locator.
    pub fn verified_object_path(&self, object: &ObjectRef) -> Result<PathBuf, ShenkoError> {
        let path = self.object_path(&object.digest)?;
        let mut file = fs::File::open(&path)
            .map_err(|error| ShenkoError::storage(format!("CAS object open: {error}")))?;
        verify_file(&mut file, &object.digest, object.bytes)
            .map_err(|error| ShenkoError::storage(format!("CAS object verification: {error}")))?;
        Ok(path)
    }

    fn publish_temporary(&self, temporary: &Path, destination: &Path, digest: &Digest, bytes: u64) -> io::Result<()> {
        fs::create_dir_all(destination.parent().expect("CAS object has a parent"))?;
        match fs::hard_link(temporary, destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_file(&mut fs::File::open(destination)?, digest, bytes)?;
            }
            Err(error) => return Err(error),
        }
        self.sync_object_directories(destination)
    }

    fn sync_object_directories(&self, path: &Path) -> io::Result<()> {
        // Publication can race another writer creating either shard. Sync
        // each link up to the already admitted root, not only the leaf.
        for directory in path.parent().expect("CAS object has a parent").ancestors() {
            sync_directory(directory)?;
            if directory == self.base { break; }
        }
        Ok(())
    }

    pub fn object_exists(&self, digest: &Digest) -> bool {
        self.object_path(digest).is_ok_and(|path| path.exists())
    }

    pub fn object_size(&self, digest: &Digest) -> Option<u64> {
        fs::metadata(self.object_path(digest).ok()?).ok().map(|m| m.len())
    }
}

/// Make a directory entry change durable. Filesystem journaling alone is not
/// a successful publication barrier. Unsupported platforms must reject the
/// operation rather than report durable CAS/recovery publication without one.
#[cfg(unix)]
pub(crate) fn sync_directory(directory: &Path) -> io::Result<()> {
    let directory = if directory.as_os_str().is_empty() { Path::new(".") } else { directory };
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_directory: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported,
        "durable directory synchronization is not supported on this platform"))
}

fn verify_file(file: &mut fs::File, digest: &Digest, expected_bytes: u64) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "CAS object size/type mismatch"));
    }
    let mut buffer = [0u8; 64 * 1024];
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0u64;
    loop {
        let count = match file.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 { break; }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    if bytes != expected_bytes || digest.0.strip_prefix("blake3:") != Some(hash.finalize().to_hex().as_str()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "CAS object digest/size mismatch"));
    }
    Ok(())
}
