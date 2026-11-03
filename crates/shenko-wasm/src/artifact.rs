//! Bounded, content-addressed tool artifacts — SPEC §8.2/§9/§18.8, R008/R101.
//!
//! A tool artifact is a real upstream WASM module. Two facts about it are
//! kept apart on purpose:
//!
//! * **Identity** is the blake3 digest of the artifact bytes. It is what the
//!   installation-unit record binds, what the executor re-verifies before a
//!   guest runs, and what a peer installation resolves.
//! * **Provenance** is the upstream digest recorded by the build receipt
//!   (sha256 for the toybox port). It says which upstream bytes were
//!   received. It is never an identity and is never compared against the
//!   blake3 identity.
//!
//! # Storage classes
//!
//! Artifacts are stored in the installation's content-addressed object store.
//! A small artifact (one the node's inline unit store admits) is also
//! carried inline by its installation unit, so node-to-node unit replication
//! keeps working exactly as before. A large artifact is never inlined: it is
//! staged as a sequence of fixed-size chunk objects plus a chunk manifest, and
//! the manifest is bound into the ledger's recovery closure so the artifact
//! travels with the installation's cognition archive.
//!
//! Every path here is bounded: the install path streams the file, the chunk
//! objects are fixed size, and the manifest is a small JSON document. No
//! artifact is ever base64-encoded into an in-memory JSON request, and no
//! artifact is ever accumulated in memory by the transport.
//!
//! [`ToolContentStore`] is the boundary between this crate and whichever
//! object store an installation uses. The shipped implementation is the
//! Shenko content store; a different runtime (a remote tool store, a
//! BoxLite-backed store) can supply its own without changing the tool
//! contracts.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// Fixed-size buffer used for every streaming artifact operation.
pub const ARTIFACT_STREAM_BUFFER: usize = 64 * 1024;

/// Size of one stored chunk object. A chunk is the unit that travels through
/// the cognition archive, so its size bounds the archive writer's memory.
pub const ARTIFACT_CHUNK_BYTES: usize = 64 * 1024;

/// Namespace the chunk objects and the chunk manifest are stored under.
pub const ARTIFACT_NAMESPACE: &str = "wasm-tool-artifact";

/// Media type of a stored tool artifact.
pub const ARTIFACT_MEDIA_TYPE: &str = "application/wasm";

/// Where a tool artifact's bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolArtifactKind {
    /// The installation's content-addressed object store, staged as chunk
    /// objects described by a chunk manifest.
    ContentStore,
    /// The node's inline installation-unit object store.
    NodeInline,
}

impl ToolArtifactKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ContentStore => "content_store",
            Self::NodeInline => "node_inline",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "content_store" => Self::ContentStore,
            "node_inline" => Self::NodeInline,
            _ => return None,
        })
    }
}

/// The artifact identity and provenance of one installed tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolArtifactRef {
    pub kind: ToolArtifactKind,
    /// blake3 identity of the artifact bytes. This is the tool's identity.
    pub digest: Digest,
    /// Exact byte length of the artifact. Zero means "not recorded"; the size
    /// check is then skipped and only the digest is verified.
    pub bytes: u64,
    /// Media type of the stored object.
    pub media_type: String,
    /// Namespace the object is stored under.
    pub store_namespace: String,
    /// Upstream provenance digest (for example the build receipt's sha256).
    /// Recorded for auditing only; never an identity.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The chunk manifest of a content-store artifact. Absent for inline
    /// artifacts, whose bytes are the unit's own artifact.
    #[serde(default)]
    pub chunk_manifest: Option<ObjectRef>,
}

impl ToolArtifactRef {
    /// An artifact held in the installation's content-addressed object store.
    pub fn content_store(
        digest: Digest,
        bytes: u64,
        sha256: Option<String>,
        chunk_manifest: ObjectRef,
    ) -> Self {
        Self {
            kind: ToolArtifactKind::ContentStore,
            digest,
            bytes,
            media_type: ARTIFACT_MEDIA_TYPE.into(),
            store_namespace: ARTIFACT_NAMESPACE.into(),
            sha256,
            chunk_manifest: Some(chunk_manifest),
        }
    }

    /// An artifact held in the node's inline installation-unit object store.
    pub fn node_inline(digest: Digest, bytes: u64, sha256: Option<String>) -> Self {
        Self {
            kind: ToolArtifactKind::NodeInline,
            digest,
            bytes,
            media_type: ARTIFACT_MEDIA_TYPE.into(),
            store_namespace: "unit-package".into(),
            sha256,
            chunk_manifest: None,
        }
    }

    /// Verify a byte slice against this artifact's identity.
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<()> {
        if self.bytes != 0 {
            ensure!(
                bytes.len() as u64 == self.bytes,
                "TOOL_ARTIFACT_SIZE_MISMATCH: expected {} bytes, got {}",
                self.bytes,
                bytes.len()
            );
        }
        ensure!(
            Digest::blake3(bytes) == self.digest,
            "TOOL_ARTIFACT_DIGEST_MISMATCH: bytes do not match {}",
            self.digest
        );
        Ok(())
    }
}

/// The chunk manifest of a staged artifact: the ordered list of chunk object
/// references plus the identity of the whole artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactChunkManifest {
    pub schema: u32,
    /// blake3 identity of the complete artifact.
    ///
    /// The field is named `artifact_digest`, not `digest`, on purpose. The
    /// recovery archive's JSON reference collector stops descending at a map
    /// that carries a `digest` key: it would record the manifest as an
    /// opaque alias and never follow the chunk list inside it, so the
    /// archive would claim a closure it does not hold. Under this name the
    /// manifest's `objects` list is reachable and the chunks travel with
    /// the installation.
    pub artifact_digest: Digest,
    /// Total length of the complete artifact.
    pub bytes: u64,
    /// Number of chunks.
    pub chunks: usize,
    /// Ordered chunk object references.
    pub objects: Vec<ObjectRef>,
    /// Upstream provenance digest, when the build receipt recorded one.
    #[serde(default)]
    pub sha256: Option<String>,
}

/// Schema version of the chunk manifest.
pub const ARTIFACT_CHUNK_MANIFEST_SCHEMA: u32 = 1;

/// The unit's own artifact for a tool whose bytes live out of line.
///
/// An installation unit carries one artifact object, and that object is
/// bounded by the node's inline limit. A tool larger than that limit
/// therefore cannot be the unit's artifact — the unit's artifact is this
/// pointer, and the module itself lives in the installation's content store
/// as chunk objects described by a chunk manifest.
///
/// The pointer is a real, digest-verified document: the unit's artifact
/// digest is the blake3 of exactly these bytes, and the runtime re-reads and
/// re-verifies them before the module is compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPointer {
    pub schema: u32,
    /// Where the module bytes are and how they are identified.
    pub artifact: ToolArtifactRef,
}

/// Schema version of the artifact pointer.
pub const ARTIFACT_POINTER_SCHEMA: u32 = 1;

impl ArtifactPointer {
    /// The pointer document for an out-of-line artifact.
    pub fn new(artifact: ToolArtifactRef) -> Self {
        Self {
            schema: ARTIFACT_POINTER_SCHEMA,
            artifact,
        }
    }

    /// The pointer's bytes, which are the unit's artifact.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Parse a pointer and check that it describes the artifact the unit
    /// claims. A pointer that names a different artifact is a manifest
    /// error, never a silent substitution.
    pub fn parse(bytes: &[u8], expected: &ToolArtifactRef) -> Result<Self> {
        let pointer: ArtifactPointer =
            serde_json::from_slice(bytes).context("parse artifact pointer")?;
        ensure!(
            pointer.schema == ARTIFACT_POINTER_SCHEMA,
            "TOOL_ARTIFACT_POINTER_UNSUPPORTED: pointer schema {}",
            pointer.schema
        );
        ensure!(
            pointer.artifact == *expected,
            "TOOL_ARTIFACT_POINTER_MISMATCH: pointer describes {}, unit claims {}",
            pointer.artifact.digest,
            expected.digest
        );
        Ok(pointer)
    }
}

/// Read a stream to its end, computing the blake3 digest and the byte count
/// with a fixed-size buffer. The digest of the *stream* is the identity of
/// the artifact.
pub fn stream_digest(reader: &mut impl Read) -> Result<(Digest, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; ARTIFACT_STREAM_BUFFER];
    let mut total = 0u64;
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other.context("read artifact stream")?,
        };
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        total += count as u64;
    }
    Ok((Digest(format!("blake3:{}", hasher.finalize().to_hex())), total))
}

/// Read a stream to its end, computing its sha256 with a fixed-size buffer.
///
/// The sha256 is the *upstream provenance* digest recorded by a build
/// receipt: it says which upstream bytes were received. It is never an
/// artifact identity — the identity is the blake3 of the same bytes.
pub fn stream_sha256(reader: &mut impl Read) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; ARTIFACT_STREAM_BUFFER];
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other.context("read artifact stream")?,
        };
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The identity and the provenance digest of one stream, computed together
/// in a single bounded pass.
///
/// The blake3 is the artifact's identity; the sha256 is the upstream
/// receipt digest. They are different facts about the same bytes and are
/// never compared against each other.
///
/// Both digests are a pure function of the bytes: re-reading the same
/// artifact always yields the same identity and the same provenance digest,
/// no matter which manifest, receipt or install produced them. A manifest
/// regenerated from the same artifacts therefore differs only in its own
/// fields, never in the identity it pins for those artifacts.
pub fn stream_identity(reader: &mut impl Read) -> Result<(Digest, String, u64)> {
    let mut identity = blake3::Hasher::new();
    let mut provenance = Sha256::new();
    let mut buffer = vec![0u8; ARTIFACT_STREAM_BUFFER];
    let mut total = 0u64;
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other.context("read artifact stream")?,
        };
        if count == 0 {
            break;
        }
        identity.update(&buffer[..count]);
        provenance.update(&buffer[..count]);
        total += count as u64;
    }
    Ok((
        Digest(format!("blake3:{}", identity.finalize().to_hex())),
        format!("{:x}", provenance.finalize()),
        total,
    ))
}

/// The installation's content-addressed object store, as the tool layer needs
/// it. Installing a tool seals the host file into this store by streaming;
/// running a tool reads the object back and verifies its digest.
pub trait ToolContentStore: Send + Sync {
    /// Seal a host file into the store by streaming. The returned object's
    /// digest is computed from the bytes, so it is the artifact's identity.
    fn seal_file(&self, path: &Path, media_type: &str, namespace: &str) -> Result<ObjectRef>;

    /// Seal an in-memory buffer as one object. Callers pass bounded buffers
    /// (one artifact chunk, one manifest).
    fn seal_bytes(&self, bytes: &[u8], media_type: &str, namespace: &str) -> Result<ObjectRef>;

    /// Open a stored object for reading. The caller verifies the digest while
    /// streaming; the store never returns bytes for a digest it does not hold.
    fn open_object(&self, digest: &Digest) -> Result<Box<dyn Read + Send>>;

    /// Whether the store already holds the object.
    fn contains(&self, digest: &Digest) -> bool;
}

/// The Shenko content store (`<data>/cas`) as a tool artifact store.
pub struct CasToolContentStore<'a> {
    cas: &'a shenko_store::ContentStore,
}

impl<'a> CasToolContentStore<'a> {
    pub fn new(cas: &'a shenko_store::ContentStore) -> Self {
        Self { cas }
    }
}

impl ToolContentStore for CasToolContentStore<'_> {
    fn seal_file(&self, path: &Path, media_type: &str, namespace: &str) -> Result<ObjectRef> {
        let mut file =
            File::open(path).with_context(|| format!("open artifact {}", path.display()))?;
        let sealed = self
            .cas
            .seal_reader(&mut file, media_type, namespace, "wasm-tool-artifact", 1)
            .map_err(|error| anyhow::anyhow!("seal artifact {}: {error}", path.display()))?;
        Ok(sealed.object)
    }

    fn seal_bytes(&self, bytes: &[u8], media_type: &str, namespace: &str) -> Result<ObjectRef> {
        let sealed = self
            .cas
            .seal_object(bytes, media_type, namespace, "wasm-tool-artifact", 1)
            .map_err(|error| anyhow::anyhow!("seal artifact object: {error}"))?;
        Ok(sealed.object)
    }

    fn open_object(&self, digest: &Digest) -> Result<Box<dyn Read + Send>> {
        let object = self
            .cas
            .read_object(digest)
            .map_err(|error| anyhow::anyhow!("read artifact object {digest}: {error}"))?;
        Ok(Box::new(std::io::Cursor::new(object)))
    }

    fn contains(&self, digest: &Digest) -> bool {
        self.cas.object_exists(digest)
    }
}

/// A staged artifact: the chunk manifest plus the artifact's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedArtifact {
    /// The manifest object describing the chunks.
    pub manifest: ObjectRef,
    /// The manifest's own digest — the identity of the manifest object.
    pub manifest_digest: Digest,
    /// blake3 identity of the complete artifact.
    pub digest: Digest,
    /// Total length of the complete artifact.
    pub bytes: u64,
    /// Number of chunk objects.
    pub chunks: usize,
    /// Upstream provenance digest, when the caller had one.
    pub sha256: Option<String>,
}

/// Stage a host file as a chunked content-store artifact.
///
/// The file is read once, in fixed-size chunks; each chunk becomes its own
/// content-addressed object and the manifest lists them in order. The
/// artifact's identity is the blake3 digest of the complete byte stream, so
/// the staged artifact has exactly the same identity as the file it came
/// from.
pub fn stage_artifact_file(
    store: &dyn ToolContentStore,
    path: &Path,
    sha256: Option<String>,
) -> Result<StagedArtifact> {
    let mut file =
        File::open(path).with_context(|| format!("open artifact {}", path.display()))?;
    let mut reader = BufReader::with_capacity(ARTIFACT_STREAM_BUFFER, &mut file);
    let mut hasher = blake3::Hasher::new();
    let mut objects: Vec<ObjectRef> = Vec::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; ARTIFACT_CHUNK_BYTES];
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other.context("read artifact chunk")?,
        };
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        total += count as u64;
        objects.push(store.seal_bytes(
            &buffer[..count],
            ARTIFACT_MEDIA_TYPE,
            ARTIFACT_NAMESPACE,
        )?);
    }
    let digest = Digest(format!("blake3:{}", hasher.finalize().to_hex()));
    let manifest = ArtifactChunkManifest {
        schema: ARTIFACT_CHUNK_MANIFEST_SCHEMA,
        artifact_digest: digest.clone(),
        bytes: total,
        chunks: objects.len(),
        objects,
        sha256: sha256.clone(),
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_object = store.seal_bytes(
        &manifest_bytes,
        "application/json",
        ARTIFACT_NAMESPACE,
    )?;
    Ok(StagedArtifact {
        manifest_digest: manifest_object.digest.clone(),
        manifest: manifest_object,
        digest,
        bytes: total,
        chunks: manifest.chunks,
        sha256,
    })
}

/// Read a staged artifact back, verifying every chunk and the whole-artifact
/// identity. The chunks are read one at a time; only the reassembled
/// artifact is resident, because a WASM compile needs the module bytes.
pub fn read_staged_artifact(
    store: &dyn ToolContentStore,
    manifest: &ObjectRef,
    expected: &Digest,
    expected_bytes: u64,
) -> Result<Vec<u8>> {
    let mut reader = store.open_object(&manifest.digest)?;
    let mut manifest_bytes = Vec::new();
    reader
        .read_to_end(&mut manifest_bytes)
        .context("read artifact chunk manifest")?;
    ensure!(
        Digest::blake3(&manifest_bytes) == manifest.digest,
        "TOOL_ARTIFACT_DIGEST_MISMATCH: chunk manifest {} does not match its content",
        manifest.digest
    );
    let parsed: ArtifactChunkManifest =
        serde_json::from_slice(&manifest_bytes).context("parse artifact chunk manifest")?;
    ensure!(
        parsed.schema == ARTIFACT_CHUNK_MANIFEST_SCHEMA,
        "TOOL_ARTIFACT_MANIFEST_UNSUPPORTED: chunk manifest schema {}",
        parsed.schema
    );
    ensure!(
        parsed.artifact_digest == *expected,
        "TOOL_ARTIFACT_DIGEST_MISMATCH: manifest describes {}, expected {expected}",
        parsed.artifact_digest
    );
    ensure!(
        expected_bytes == 0 || parsed.bytes == expected_bytes,
        "TOOL_ARTIFACT_SIZE_MISMATCH: manifest describes {} bytes, expected {expected_bytes}",
        parsed.bytes
    );
    let mut bytes = Vec::with_capacity(parsed.bytes as usize);
    let mut hasher = blake3::Hasher::new();
    for object in &parsed.objects {
        let mut reader = store.open_object(&object.digest)?;
        let mut chunk = Vec::with_capacity(object.bytes as usize);
        reader.read_to_end(&mut chunk).context("read artifact chunk")?;
        ensure!(
            chunk.len() as u64 == object.bytes && Digest::blake3(&chunk) == object.digest,
            "TOOL_ARTIFACT_DIGEST_MISMATCH: chunk {} does not match its content",
            object.digest
        );
        hasher.update(&chunk);
        bytes.extend_from_slice(&chunk);
    }
    let digest = Digest(format!("blake3:{}", hasher.finalize().to_hex()));
    ensure!(
        digest == *expected,
        "TOOL_ARTIFACT_DIGEST_MISMATCH: reassembled artifact is {digest}, expected {expected}"
    );
    Ok(bytes)
}

/// Resolve an installed tool's executable bytes and verify them against the
/// recorded identity.
///
/// A node-inline artifact comes from the node's unit object store; a
/// content-store artifact is reassembled from its chunk objects. In both
/// cases the returned bytes match the artifact's recorded identity, so an
/// artifact that does not match its descriptor never reaches a guest.
pub fn resolve_artifact_bytes(
    node: &shenko_node::Node,
    unit_artifact_digest: &Digest,
    artifact: &ToolArtifactRef,
    store: &dyn ToolContentStore,
) -> Result<Vec<u8>> {
    match artifact.kind {
        ToolArtifactKind::NodeInline => {
            let bytes = node.unit_object_bytes(unit_artifact_digest)?;
            artifact.verify_bytes(&bytes)?;
            Ok(bytes)
        }
        ToolArtifactKind::ContentStore => {
            // The unit's artifact is the pointer document, not the module.
            // It is re-read and re-verified here: the unit's bytes and the
            // descriptor's claim must agree before a single chunk is read.
            let pointer_bytes = node.unit_object_bytes(unit_artifact_digest)?;
            let pointer = ArtifactPointer::parse(&pointer_bytes, artifact)?;
            let manifest = artifact
                .chunk_manifest
                .clone()
                .context("TOOL_ARTIFACT_MANIFEST_MISSING: content-store artifact has no chunk manifest")?;
            ensure!(
                pointer.artifact.chunk_manifest.as_ref() == Some(&manifest),
                "TOOL_ARTIFACT_POINTER_MISMATCH: pointer names a different chunk manifest"
            );
            read_staged_artifact(store, &manifest, &artifact.digest, artifact.bytes)
        }
    }
}

/// Reject an artifact whose recorded provenance is not a usable digest. The
/// provenance is informational, so a missing value is allowed; a malformed
/// one is a manifest error, not a warning.
pub fn validate_provenance(sha256: Option<&str>) -> Result<()> {
    let Some(value) = sha256 else {
        return Ok(());
    };
    let value = value.trim();
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "TOOL_PROVENANCE_INVALID: expected a 64 hex character sha256, got {value:?}"
    );
    ensure!(
        !value.bytes().any(|byte| byte.is_ascii_uppercase()),
        "TOOL_PROVENANCE_INVALID: sha256 must be lowercase hex"
    );
    Ok(())
}

/// Reject an artifact identity that is not a blake3 digest.
///
/// The identity is what the installation binds; a manifest that carries
/// anything else cannot be verified against the bytes it describes.
pub fn validate_identity(identity: &str) -> Result<()> {
    let Some(hex) = identity.strip_prefix("blake3:") else {
        bail!("TOOL_IDENTITY_INVALID: expected a blake3 digest, got {identity:?}");
    };
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "TOOL_IDENTITY_INVALID: expected 64 lowercase hex characters, got {identity:?}"
    );
    Ok(())
}

/// Fail when an artifact claims a provenance digest that is not the recorded
/// one. The sha256 belongs to the upstream build receipt; the blake3 digest
/// belongs to this installation's unit identity. The two are never
/// interchangeable.
pub fn ensure_provenance_matches(expected: Option<&str>, actual: Option<&str>) -> Result<()> {
    match (expected, actual) {
        (None, _) => Ok(()),
        (Some(expected), Some(actual)) if expected.eq_ignore_ascii_case(actual) => Ok(()),
        (Some(expected), Some(actual)) => bail!(
            "TOOL_PROVENANCE_MISMATCH: manifest records {expected}, artifact receipt records {actual}"
        ),
        (Some(expected), None) => bail!(
            "TOOL_PROVENANCE_MISSING: manifest records {expected}, artifact receipt has none"
        ),
    }
}

/// The schema reference of a stored artifact object.
pub fn artifact_schema() -> SchemaRef {
    SchemaRef {
        name: "wasm-tool-artifact".into(),
        major: 1,
        digest: Digest::blake3(b"wasm-tool-artifact/v1"),
    }
}
