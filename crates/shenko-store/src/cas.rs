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
        let result = (|| -> std::io::Result<(Digest, u64)> {
            let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
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
            drop(output);
            let digest = Digest(format!("blake3:{}", hash.finalize().to_hex()));
            let destination = self.object_path(&digest);
            let parent = destination.parent().expect("CAS object has a parent");
            fs::create_dir_all(parent)?;
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Existing objects are immutable, but never attest a
                    // replaced/corrupted object merely because its name fits.
                    let mut existing = fs::File::open(&destination)?;
                    let mut existing_hash = blake3::Hasher::new();
                    let mut existing_bytes = 0u64;
                    loop {
                        let count = existing.read(&mut buffer)?;
                        if count == 0 { break; }
                        existing_hash.update(&buffer[..count]);
                        existing_bytes += count as u64;
                    }
                    if existing_bytes != bytes || existing_hash.finalize() != hash.finalize() {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "CAS object digest/size mismatch"));
                    }
                }
                Err(error) => return Err(error),
            }
            fs::File::open(parent)?.sync_all()?;
            Ok((digest, bytes))
        })();
        // Only this invocation's unpredictable staging name is removed.
        let _ = fs::remove_file(&temporary);
        let (digest, bytes) = result.map_err(|error| ShenkoError::storage(format!("CAS stream sealing: {error}")))?;
        Ok(SealedObjectRef {
            object: ObjectRef {
                locator: format!("file://{}", self.object_path(&digest).display()),
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
        let path = self.object_path(digest);
        fs::read(&path).map_err(|e| ShenkoError::not_found(format!("cas read {digest}: {e}")))
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
