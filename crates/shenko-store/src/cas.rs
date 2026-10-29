//! Content-addressed object store. SPEC §41.2.2
//! Objects are immutable; locators may change.
use std::fs;
use std::io::Write;
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

        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| ShenkoError::storage(format!("cas mkdir: {e}")))?;
            }
            // Equal content may be sealed concurrently by unrelated actions.
            // Each writer owns a distinct temporary inode; both renames of
            // the same verified bytes are safe, unlike a shared digest.tmp.
            let tmp = path.with_extension(format!("{}.tmp", shenko_types::ids::EntityId::new()));
            let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)
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
        let path = self.object_path(digest)?;
        fs::read(&path).map_err(|e| ShenkoError::not_found(format!("cas read {digest}: {e}")))
    }

    /// Resolve immutable content in THIS store, never a historical locator.
    pub fn verified_object_path(&self, object: &ObjectRef) -> Result<PathBuf, ShenkoError> {
        let bytes = self.read_object(&object.digest)?;
        if bytes.len() as u64 != object.bytes || Digest::blake3(&bytes) != object.digest {
            return Err(ShenkoError::storage("CAS object length/digest mismatch"));
        }
        self.object_path(&object.digest)
    }

    pub fn object_exists(&self, digest: &Digest) -> bool {
        self.object_path(digest).is_ok_and(|path| path.exists())
    }

    pub fn object_size(&self, digest: &Digest) -> Option<u64> {
        fs::metadata(self.object_path(digest).ok()?).ok().map(|m| m.len())
    }
}
