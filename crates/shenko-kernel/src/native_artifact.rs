//! Identity of the executable that actually contains a built-in native plugin.
//! Contracts and schemas have their own identities; neither descriptive text
//! nor a repository source file is an executable artifact.
use std::{fs::File, io::{Read, Seek, SeekFrom}};

use anyhow::{Context as _, Result};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use shenko_types::object::{Digest, ObjectRef, SchemaRef};

pub struct NativeExecutableArtifact {
    file: Mutex<File>,
    digest: Digest,
    bytes: u64,
}

impl NativeExecutableArtifact {
    fn open() -> Result<Self> {
        // Linux keeps this inode alive across an atomic executable replacement.
        // Other platforms retain the initially opened executable handle too.
        #[cfg(target_os = "linux")]
        let path = std::path::PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let path = std::env::current_exe()?;
        let mut file = File::open(path).context("open executing native artifact")?;
        let mut hash = blake3::Hasher::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 { break; }
            hash.update(&buffer[..read]);
            bytes += read as u64;
        }
        let digest = Digest(format!("blake3:{}", hash.finalize().to_hex()));
        Ok(Self { file: Mutex::new(file), digest, bytes })
    }

    pub fn digest(&self) -> Digest { self.digest.clone() }

    /// A typed recovery root; a runtime must retain these exact bytes in its
    /// content store before installing the corresponding executable handler.
    pub fn object_ref(&self) -> ObjectRef {
        ObjectRef {
            digest: self.digest(), bytes: self.bytes,
            media_type: "application/octet-stream".into(),
            store_namespace: "native-implementation".into(),
            locator: format!("native:executable/{}", self.digest),
            codec: SchemaRef { name: "native-executable".into(), major: 1,
                digest: Digest::blake3(b"native-executable") },
        }
    }

    /// Preserve the same open inode, not a mutable path re-opened after an
    /// update. Reading is serialized because cloned handles share an offset.
    pub fn read<T>(&self, reader: impl FnOnce(&mut File) -> Result<T>) -> Result<T> {
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(0))?;
        reader(&mut file)
    }
}

pub fn running_native_artifact() -> Result<&'static NativeExecutableArtifact> {
    static ARTIFACT: OnceCell<NativeExecutableArtifact> = OnceCell::new();
    ARTIFACT.get_or_try_init(NativeExecutableArtifact::open)
}

pub fn native_artifact_digest() -> Result<Digest> {
    Ok(running_native_artifact()?.digest())
}

/// Stable versioned schema identity. It identifies the published schema
/// contract; it is not a digest of executable bytes or an operation's prose.
pub fn native_schema_digest(name: &str, major: u32) -> Digest {
    // This array is the same JSON as the original string/string/u32 tuple.
    // Construct its primitive values directly so no fallible user serializer
    // or panic is required for the published schema identity.
    let encoded = serde_json::Value::Array(vec![
        serde_json::Value::String("shenko.native-schema".into()),
        serde_json::Value::String(name.into()),
        serde_json::Value::Number(major.into()),
    ]).to_string();
    Digest::blake3(encoded.as_bytes())
}
