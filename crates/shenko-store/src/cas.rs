//! Content-addressed object store.
//! Objects are immutable; locators may change.
use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use anyhow::Result;
use shenko_types::object::{Digest, ObjectRef, SchemaRef, SealedObjectRef};
use shenko_types::error::ShenkoError;

pub struct ContentStore {
    base: PathBuf,
}

/// Provisional child references for one internal graph construction. There is
/// no SealedObjectRef here: only seal_graph_root can finish the directory
/// barrier and then publish the root through the ordinary durable path.
pub(super) struct PendingObjectGraph<'a> {
    cas: &'a ContentStore,
    // Every object has two digest shards. The set is bounded by that fixed
    // directory namespace, not by the number of historical objects.
    directories: BTreeSet<(Reverse<usize>, PathBuf)>,
}

impl PendingObjectGraph<'_> {
    pub(super) fn write_object(
        &mut self, data: &[u8], media_type: &str, namespace: &str,
        codec_name: &str, codec_major: u32,
    ) -> Result<ObjectRef, ShenkoError> {
        let cas = self.cas;
        let digest = Digest::blake3(data);
        let path = cas.object_path(&digest)?;
        let directories = &mut self.directories;
        cas.write_object_bytes(data, &digest, &path, |path| {
            let parent = path.parent().ok_or_else(||
                io::Error::new(io::ErrorKind::InvalidInput, "CAS object has no parent"))?;
            for directory in parent.ancestors() {
                directories.insert((Reverse(directory.components().count()), directory.to_path_buf()));
                if directory == cas.base { break; }
            }
            Ok(())
        })?;
        Ok(ContentStore::object_reference(&path, digest, data.len() as u64,
            media_type, namespace, codec_name, codec_major))
    }

    fn finish(self) -> io::Result<()> {
        // write_object_bytes already attempted each owned staging unlink.
        // Sync all leaf links before their ancestors, including CAS base last.
        for (_, directory) in self.directories {
            sync_directory(&directory)?;
        }
        Ok(())
    }
}

impl ContentStore {
    /// Attach to an existing object directory without creating or syncing it.
    /// Used by inspection commands that must not initialize installation state.
    pub fn open_existing(base: PathBuf) -> Result<Self, ShenkoError> {
        if !base.is_dir() {
            return Err(ShenkoError::not_found("CAS directory does not exist"));
        }
        Ok(Self { base })
    }

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
        self.write_object_bytes(data, &digest, &path, |path| self.sync_object_directories(path))?;

        Ok(SealedObjectRef {
            object: Self::object_reference(&path, digest, data.len() as u64,
                media_type, namespace, codec_name, codec_major),
            sealed_at: chrono::Utc::now(),
            sealed_commit: shenko_types::commit::CommitRef {
                ledger_id: shenko_types::ids::EntityId::nil(),
                seq: shenko_types::ids::LedgerSeq::ZERO,
            },
        })
    }

    /// The control predecessor builder keeps provisional references inside
    /// this closure. Its serialized root is not published until every child
    /// directory barrier succeeds. The caller must retain its existing CAS
    /// publication custody; no lease is acquired beneath an enclosing lock.
    pub(super) fn seal_graph_root(
        &self, media_type: &str, namespace: &str, codec_name: &str, codec_major: u32,
        build: impl FnOnce(&mut PendingObjectGraph<'_>) -> Result<Vec<u8>>,
    ) -> Result<SealedObjectRef> {
        let mut graph = PendingObjectGraph { cas: self, directories: BTreeSet::new() };
        let root = build(&mut graph)?;
        graph.finish().map_err(|error| ShenkoError::storage(format!("CAS graph directory sync: {error}")))?;
        Ok(self.seal_object(&root, media_type, namespace, codec_name, codec_major)?)
    }

    fn write_object_bytes(
        &self, data: &[u8], digest: &Digest, path: &Path,
        directories: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<(), ShenkoError> {
        let result = match fs::File::open(path) {
            Ok(mut existing) => verify_file(&mut existing, digest, data.len() as u64)
                .and_then(|()| directories(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let temporary = self.base.join(format!(".object-{}", shenko_types::ids::EntityId::new()));
                let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)
                    .map_err(|error| ShenkoError::storage(format!("CAS staging creation: {error}")))?;
                let result = (|| {
                    output.write_all(data)?;
                    output.sync_all()?;
                    self.link_temporary(&temporary, path, digest, data.len() as u64)?;
                    directories(path)
                })();
                drop(output);
                // Cleanup owns only a staging file successfully created here.
                let _ = fs::remove_file(&temporary);
                result
            }
            Err(error) => Err(error),
        };
        result.map_err(|error| ShenkoError::storage(format!("CAS object sealing: {error}")))
    }

    fn object_reference(
        path: &Path, digest: Digest, bytes: u64, media_type: &str,
        namespace: &str, codec_name: &str, codec_major: u32,
    ) -> ObjectRef {
        ObjectRef {
            digest,
            bytes,
            media_type: media_type.to_string(),
            store_namespace: namespace.to_string(),
            locator: format!("file://{}", path.display()),
            codec: SchemaRef {
                name: codec_name.to_string(),
                major: codec_major,
                digest: Digest::blake3(codec_name.as_bytes()),
            },
        }
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

    /// The stored length of an object, or `None` when this CAS holds none.
    pub fn object_len(&self, digest: &Digest) -> Result<Option<u64>, ShenkoError> {
        let path = self.object_path(digest)?;
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ShenkoError::storage(format!("cas stat {digest}: {error}"))),
        }
    }

    /// The first `limit` bytes of an object, without reading the rest.
    pub fn read_prefix(&self, digest: &Digest, limit: usize) -> Result<Vec<u8>, ShenkoError> {
        let path = self.object_path(digest)?;
        let file = fs::File::open(&path).map_err(|e| ShenkoError::not_found(format!("cas read {digest}: {e}")))?;
        let mut prefix = Vec::with_capacity(limit.min(1 << 20));
        file.take(limit as u64)
            .read_to_end(&mut prefix)
            .map_err(|e| ShenkoError::storage(format!("cas read {digest}: {e}")))?;
        Ok(prefix)
    }

    pub fn read_object(&self, digest: &Digest) -> Result<Vec<u8>, ShenkoError> {
        let path = self.object_path(digest)?;
        fs::read(&path).map_err(|e| ShenkoError::not_found(format!("cas read {digest}: {e}")))
    }

    /// Audit a digest in this CAS with bounded streaming reads. The caller
    /// verifies the complete length/hash; no historical locator is followed.
    pub(crate) fn open_retained_object(&self, digest: &Digest) -> io::Result<fs::File> {
        let path = self.object_path(digest).map_err(io::Error::other)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        options.open(path)
    }

    /// Read at most 1 MiB authenticated by a separately trusted index. This
    /// verifies the requested range, NOT the unread remainder of the object.
    /// Resolve only in this CAS; an exported locator is never a read authority.
    pub fn read_indexed_range(
        &self,
        object: &Digest,
        object_bytes: u64,
        offset: u64,
        bytes: usize,
        range_digest: &Digest,
    ) -> Result<Vec<u8>, ShenkoError> {
        if bytes > 1024 * 1024 || offset.checked_add(bytes as u64).is_none_or(|end| end > object_bytes) {
            return Err(ShenkoError::conflict("CAS indexed range exceeds its bounds"));
        }
        let path = self.object_path(object)?;
        let result = (|| -> io::Result<Vec<u8>> {
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let mut file = options.open(path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() != object_bytes {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "CAS indexed object size/type mismatch"));
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut result = vec![0; bytes];
            file.read_exact(&mut result)?;
            if Digest::blake3(&result) != *range_digest {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "CAS indexed range digest mismatch"));
            }
            Ok(result)
        })();
        result.map_err(|error| ShenkoError::storage(format!("CAS indexed read: {error}")))
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
        self.link_temporary(temporary, destination, digest, bytes)?;
        self.sync_object_directories(destination)
    }

    fn link_temporary(&self, temporary: &Path, destination: &Path, digest: &Digest, bytes: u64) -> io::Result<()> {
        fs::create_dir_all(destination.parent().expect("CAS object has a parent"))?;
        match fs::hard_link(temporary, destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_file(&mut fs::File::open(destination)?, digest, bytes)?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
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

    /// The admitted object directory. Callers that reason about the whole
    /// store (root-aware reclamation) need the root and the names of the
    /// in-progress staging entries, not only single-digest lookups.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Every published object digest, ascending. Staging files (`.object-*`,
    /// `.stream-*`) are in-progress writers and are NOT objects, so they are
    /// never returned: a caller that must refuse while a publication is in
    /// flight has to inspect them explicitly.
    pub fn object_digests(&self) -> Result<Vec<Digest>, ShenkoError> {
        let entries = fs::read_dir(&self.base)
            .map_err(|error| ShenkoError::storage(format!("CAS enumeration: {error}")))?;
        let mut digests = Vec::new();
        for shard in entries {
            let shard = shard.map_err(|error| ShenkoError::storage(format!("CAS enumeration: {error}")))?;
            let prefix = shard.file_name();
            let prefix = prefix.to_str().unwrap_or_default();
            if prefix.len() != 2 || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
                continue;
            }
            let object_entries = match fs::read_dir(shard.path()) {
                Ok(object_entries) => object_entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(ShenkoError::storage(format!("CAS enumeration: {error}"))),
            };
            for object_entry in object_entries {
                let object_entry = object_entry
                    .map_err(|error| ShenkoError::storage(format!("CAS enumeration: {error}")))?;
                let second = object_entry.file_name();
                let second = second.to_str().unwrap_or_default();
                if second.len() != 2 || !second.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
                    continue;
                }
                let objects = match fs::read_dir(object_entry.path()) {
                    Ok(objects) => objects,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(ShenkoError::storage(format!("CAS enumeration: {error}"))),
                };
                for object in objects {
                    let object = object
                        .map_err(|error| ShenkoError::storage(format!("CAS enumeration: {error}")))?;
                    let name = object.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if name.len() != 64
                        || !name.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                    {
                        continue;
                    }
                    if !name.starts_with(prefix) || &name[2..4] != second {
                        // A name that is not the shard path of its own digest is
                        // not a published object this store can address; report
                        // nothing for it instead of guessing an identity.
                        continue;
                    }
                    let metadata = object
                        .metadata()
                        .map_err(|error| ShenkoError::storage(format!("CAS enumeration: {error}")))?;
                    if !metadata.is_file() {
                        continue;
                    }
                    digests.push(Digest(format!("blake3:{name}")));
                }
            }
        }
        digests.sort();
        digests.dedup();
        Ok(digests)
    }

    /// Remove one published object and make the removal durable. Returns
    /// whether the object was present. Only a caller that has proven the
    /// object unreachable may call this; the store itself has no root graph.
    pub fn remove_object(&self, digest: &Digest) -> Result<bool, ShenkoError> {
        let path = self.object_path(digest)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(ShenkoError::storage(format!("CAS object removal: {error}")));
            }
        }
        let shard = path.parent().expect("CAS object has a parent");
        for directory in [shard, shard.parent().expect("CAS shard has a parent")] {
            sync_directory(directory).map_err(|error| {
                ShenkoError::storage(format!("CAS object removal sync: {error}"))
            })?;
        }
        Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_root_waits_for_children_and_reopens_without_overwrite() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let base = directory.path().join("cas");
        let cas = ContentStore::new(base.clone())?;
        let publication = crate::roots::CasWriteLease::acquire(cas.base())?;
        let original = b"original child bytes";
        let existing = cas.seal_object(original, "text/plain", "test", "raw", 1)?.object;
        let existing_path = cas.object_path(&existing.digest)?;

        // Corrupt only this test's owned object, keeping its length unchanged.
        // Dedup must hash it and refuse; it must never repair by overwriting.
        let damaged = vec![b'!'; original.len()];
        fs::write(&existing_path, &damaged)?;
        let refused_root = br#"{"case":"corrupt-child"}"#;
        let refused = cas.seal_graph_root("application/json", "test", "graph", 1, |graph| {
            graph.write_object(original, "text/plain", "test", "raw", 1)?;
            Ok(refused_root.to_vec())
        });
        assert!(format!("{:#}", refused.unwrap_err()).contains("digest/size mismatch"));
        assert_eq!(fs::read(&existing_path)?, damaged);
        assert!(!cas.object_exists(&Digest::blake3(refused_root)));
        let mut repaired = fs::OpenOptions::new().write(true).open(&existing_path)?;
        repaired.write_all(original)?;
        repaired.sync_all()?;
        drop(repaired);

        // Make a required child directory unavailable after its object has
        // been linked, but before finish. A real filesystem error at that
        // barrier must prevent even starting the ordinary root publication.
        let new_child = b"new child bytes";
        let moved = directory.path().join("withheld-shard");
        let mut withheld = None;
        let mut root_digest = None;
        let failed = cas.seal_graph_root("application/json", "test", "graph", 1, |graph| {
            let first = graph.write_object(original, "text/plain", "test", "raw", 1)?;
            let second = graph.write_object(new_child, "text/plain", "test", "raw", 1)?;
            let root = serde_json::to_vec(&serde_json::json!({"children":[first, second]}))?;
            root_digest = Some(Digest::blake3(&root));
            let shard = cas.object_path(&Digest::blake3(new_child))?.parent().unwrap().to_path_buf();
            fs::rename(&shard, &moved)?;
            withheld = Some(shard);
            Ok(root)
        });
        assert!(format!("{:#}", failed.unwrap_err()).contains("CAS graph directory sync"));
        let root_digest = root_digest.unwrap();
        assert!(!cas.object_exists(&root_digest));
        fs::rename(&moved, withheld.unwrap())?;
        assert_eq!(fs::read(&existing_path)?, original);
        for entry in fs::read_dir(&base)? {
            assert!(!entry?.file_name().to_string_lossy().starts_with(".object-"));
        }

        let sealed = cas.seal_graph_root("application/json", "test", "graph", 1, |graph| {
            let first = graph.write_object(original, "text/plain", "test", "raw", 1)?;
            let second = graph.write_object(new_child, "text/plain", "test", "raw", 1)?;
            Ok(serde_json::to_vec(&serde_json::json!({"children":[first, second]}))?)
        })?;
        assert_eq!(sealed.object.digest, root_digest);
        drop(publication);
        drop(cas);

        let reopened = ContentStore::open_existing(base)?;
        let _publication = crate::roots::CasWriteLease::acquire(reopened.base())?;
        let root: serde_json::Value = serde_json::from_slice(&reopened.read_object(&root_digest)?)?;
        let children: Vec<ObjectRef> = serde_json::from_value(root["children"].clone())?;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0], existing);
        assert_eq!(reopened.read_object(&children[0].digest)?, original);
        assert_eq!(reopened.read_object(&children[1].digest)?, new_child);
        Ok(())
    }
}
