//! Content-addressed object store. SPEC §41.2.2
//! Objects are immutable; locators may change.
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use anyhow::Result;
use shenko_types::object::{Digest, ObjectRef, SchemaRef, SealedObjectRef};
use shenko_types::error::ShenkoError;

pub struct ContentStore {
    base: PathBuf,
}

impl ContentStore {
    pub fn new(base: PathBuf) -> Self {
        fs::create_dir_all(&base).ok();
        Self { base }
    }

    fn object_path(&self, digest: &Digest) -> PathBuf {
        let hex = digest.hex();
        if hex.len() >= 4 {
            self.base.join(&hex[..2]).join(&hex[2..4]).join(hex)
        } else {
            self.base.join(hex)
        }
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
        let path = self.object_path(&digest);

        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| ShenkoError::storage(format!("cas mkdir: {e}")))?;
            }
            let tmp = path.with_extension(".tmp");
            let mut f = fs::File::create(&tmp)
                .map_err(|e| ShenkoError::storage(format!("cas write tmp: {e}")))?;
            f.write_all(data)
                .map_err(|e| ShenkoError::storage(format!("cas write: {e}")))?;
            f.sync_all()
                .map_err(|e| ShenkoError::storage(format!("cas sync: {e}")))?;
            drop(f);
            fs::rename(&tmp, &path)
                .map_err(|e| ShenkoError::storage(format!("cas rename: {e}")))?;
        }

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

    pub fn read_object(&self, digest: &Digest) -> Result<Vec<u8>, ShenkoError> {
        let mut file = self.open_object(digest)?;
        let metadata = file.metadata()
            .map_err(|error| ShenkoError::storage(format!("cas object metadata: {error}")))?;
        if !metadata.is_file() {
            return Err(ShenkoError::conflict("CAS object is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| ShenkoError::storage(format!("cas object read: {error}")))?;
        Ok(bytes)
    }

    fn open_object(&self, digest: &Digest) -> Result<cap_std::fs::File, ShenkoError> {
        let hex = digest.canonical_blake3_hex()
            .ok_or_else(|| ShenkoError::conflict("noncanonical or unsupported CAS digest"))?;
        let root = cap_std::fs::Dir::open_ambient_dir(&self.base, cap_std::ambient_authority())
            .map_err(|error| ShenkoError::storage(format!("cas open root: {error}")))?;
        let relative = PathBuf::from(&hex[..2]).join(&hex[2..4]).join(hex);
        root.open(relative)
            .map_err(|error| ShenkoError::not_found(format!("cas confined open: {error}")))
    }

    /// Verify untrusted object metadata without trusting its locator or a
    /// resolve-then-open path. Digest validation precedes every filesystem
    /// access; capability-relative resolution cannot follow an ancestor or
    /// final symlink outside this CAS. Hash the opened file with bounded memory.
    pub fn verify_object(&self, object: &ObjectRef) -> Result<(), ShenkoError> {
        let mut file = self.open_object(&object.digest)?;
        let metadata = file.metadata()
            .map_err(|error| ShenkoError::storage(format!("cas object metadata: {error}")))?;
        if !metadata.is_file() || metadata.len() != object.bytes {
            return Err(ShenkoError::conflict("CAS object type/length mismatch"));
        }
        let mut hasher = blake3::Hasher::new();
        let mut count = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = match file.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(ShenkoError::storage(format!("cas object read: {error}"))),
            };
            if read == 0 {
                break;
            }
            count = count.checked_add(read as u64)
                .ok_or_else(|| ShenkoError::conflict("CAS object length overflow"))?;
            if count > object.bytes {
                return Err(ShenkoError::conflict("CAS object grew during verification"));
            }
            hasher.update(&buffer[..read]);
        }
        if count != object.bytes
            || hasher.finalize().to_hex().as_str() != object.digest.hex()
        {
            return Err(ShenkoError::conflict("CAS object length/digest mismatch"));
        }
        Ok(())
    }

    /// Resolve immutable content in THIS store, never a historical locator.
    pub fn verified_object_path(&self, object: &ObjectRef) -> Result<PathBuf, ShenkoError> {
        let bytes = self.read_object(&object.digest)?;
        if bytes.len() as u64 != object.bytes || Digest::blake3(&bytes) != object.digest {
            return Err(ShenkoError::storage("CAS object length/digest mismatch"));
        }
        Ok(self.object_path(&object.digest))
    }

    pub fn object_exists(&self, digest: &Digest) -> bool {
        self.object_path(digest).exists()
    }

    pub fn object_size(&self, digest: &Digest) -> Option<u64> {
        fs::metadata(self.object_path(digest)).ok().map(|m| m.len())
    }
}
