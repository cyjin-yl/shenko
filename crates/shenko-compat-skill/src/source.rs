//! Bounded source index over one ordinary out-of-line CAS artifact.
use std::{collections::BTreeMap, io::{self, Read}};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use shenko_store::cas::ContentStore;
use shenko_types::object::{Digest, ObjectRef};
use crate::{metadata, validate_member_path};

pub const CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_READ_BYTES: u32 = 128 * 1024;
pub const MAX_PROVENANCE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub executable: bool,
    pub offset: u64,
    pub bytes: u64,
    pub digest: Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIndex {
    pub schema_version: u32,
    pub provenance_refs: Vec<String>,
    pub files: BTreeMap<String, SourceFile>,
    pub archive_digest: Digest,
    pub archive_bytes: u64,
    pub chunks: Vec<Digest>,
}

/// A selected, already opened regular file. `bytes` is the admitted length;
/// shrinking or growing during capture fails instead of truncating the source.
pub struct SourceInput<R> {
    pub path: String,
    pub executable: bool,
    pub bytes: u64,
    pub reader: R,
}

pub struct SkillSource {
    pub index: SourceIndex,
    pub entry: Vec<u8>,
    pub archive: ObjectRef,
}

impl SourceIndex {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 2, "unsupported Skill source index schema");
        ensure!(self.archive_bytes <= shenko_node::MAX_EXTERNAL_ARTIFACT_BYTES, "Skill archive exceeds the external artifact bound");
        ensure!(self.archive_digest.canonical_blake3_hex().is_some(), "invalid Skill archive digest");
        ensure!(serde_json::to_vec(&self.provenance_refs)?.len() <= MAX_PROVENANCE_BYTES, "Skill provenance exceeds its bound");
        ensure!(self.chunks.len() as u64 == self.archive_bytes.div_ceil(CHUNK_BYTES as u64), "Skill chunk coverage mismatch");
        ensure!(self.chunks.iter().all(|digest| digest.canonical_blake3_hex().is_some()), "invalid Skill chunk digest");
        let mut offset = 0u64;
        for (path, file) in &self.files {
            validate_member_path(path)?;
            ensure!(file.offset == offset, "Skill member offsets are not contiguous");
            offset = offset.checked_add(file.bytes).context("Skill member length overflow")?;
            ensure!(offset <= self.archive_bytes, "Skill member exceeds its archive");
            ensure!(file.digest.canonical_blake3_hex().is_some(), "invalid Skill member digest");
            let mut prefix = path.as_str();
            while let Some((parent, _)) = prefix.rsplit_once('/') {
                ensure!(!self.files.contains_key(parent), "source member is also a directory: {parent}");
                prefix = parent;
            }
        }
        ensure!(offset == self.archive_bytes, "Skill index does not cover its archive");
        let entry = self.files.get("SKILL.md").context("source has no SKILL.md")?;
        ensure!(entry.bytes <= shenko_node::MAX_ARTIFACT_BYTES as u64, "Skill entry exceeds its inline bound");
        Ok(())
    }

    /// Authenticate only the chunks touched by this range. The returned bytes
    /// are independent of an exported locator and never include unread data.
    pub fn read(&self, cas: &ContentStore, path: &str, offset: u64, limit: u32) -> Result<Vec<u8>> {
        ensure!(limit > 0 && limit <= CHUNK_BYTES as u32, "Skill indexed read exceeds one chunk");
        validate_member_path(path)?;
        let file = self.files.get(path).context("Skill source member is absent")?;
        ensure!(offset <= file.bytes, "Skill read offset exceeds member length");
        let length = (file.bytes - offset).min(u64::from(limit)) as usize;
        let mut result = Vec::with_capacity(length);
        let start = file.offset + offset;
        let end = start + length as u64;
        let mut position = start;
        while position < end {
            let chunk = position / CHUNK_BYTES as u64;
            let chunk_start = chunk * CHUNK_BYTES as u64;
            let chunk_len = (self.archive_bytes - chunk_start).min(CHUNK_BYTES as u64) as usize;
            let bytes = cas.read_indexed_range(&self.archive_digest, self.archive_bytes, chunk_start, chunk_len,
                self.chunks.get(chunk as usize).context("Skill chunk is absent")?)?;
            let first = (position - chunk_start) as usize;
            let count = (bytes.len() - first).min((end - position) as usize);
            result.extend_from_slice(&bytes[first..first + count]);
            position += count as u64;
        }
        Ok(result)
    }
}

impl SkillSource {
    /// The inventory must be frozen before this call: CAS publication may
    /// create files, but newly created files must not enter source enumeration.
    /// Inputs are lexically ordered, so identical source bytes have one layout.
    pub fn capture<R: Read>(
        cas: &ContentStore,
        provenance_refs: Vec<String>,
        inputs: impl IntoIterator<Item = Result<SourceInput<R>>>,
    ) -> Result<Self> {
        ensure!(serde_json::to_vec(&provenance_refs)?.len() <= MAX_PROVENANCE_BYTES, "Skill provenance exceeds its bound");
        let mut stream = SourceStream {
            inputs: inputs.into_iter(), current: None, files: BTreeMap::new(), entry: Vec::new(),
            position: 0, names_bytes: 0, chunk_hash: blake3::Hasher::new(), chunk_bytes: 0, chunks: Vec::new(), finished: false,
        };
        let archive = cas.seal_reader(&mut stream, "application/octet-stream", "ecosystem-compat", "skill-source-bytes", 2)?.object;
        let index = SourceIndex { schema_version: 2, provenance_refs, files: stream.files,
            archive_digest: archive.digest.clone(), archive_bytes: archive.bytes, chunks: stream.chunks };
        index.validate()?;
        metadata(&stream.entry)?;
        ensure!(serde_json::to_vec(&index)?.len() <= shenko_node::MAX_ARTIFACT_BYTES, "Skill source index exceeds the Node inline bound");
        Ok(Self { index, entry: stream.entry, archive })
    }
}

struct Active<R> {
    input: SourceInput<R>,
    remaining: u64,
    offset: u64,
    hash: blake3::Hasher,
}

struct SourceStream<I, R> {
    inputs: I,
    current: Option<Active<R>>,
    files: BTreeMap<String, SourceFile>,
    entry: Vec<u8>,
    position: u64,
    names_bytes: usize,
    chunk_hash: blake3::Hasher,
    chunk_bytes: usize,
    chunks: Vec<Digest>,
    finished: bool,
}

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl<I, R> Read for SourceStream<I, R>
where I: Iterator<Item = Result<SourceInput<R>>>, R: Read {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.finished { return Ok(0); }
        loop {
            if self.current.is_none() {
                let Some(input) = self.inputs.next() else {
                    if self.chunk_bytes > 0 {
                        self.chunks.push(Digest(format!("blake3:{}", self.chunk_hash.finalize().to_hex())));
                    }
                    self.finished = true;
                    return Ok(0);
                };
                let input = input.map_err(|error| invalid(error.to_string()))?;
                validate_member_path(&input.path).map_err(|error| invalid(error.to_string()))?;
                if self.files.last_key_value().is_some_and(|(previous, _)| previous >= &input.path) {
                    return Err(invalid("Skill source inputs are not uniquely ordered"));
                }
                // Six is JSON's worst-case escaping expansion. Include each
                // fixed member record too, bounding memory before serialization.
                self.names_bytes = self.names_bytes.checked_add(input.path.len() * 6 + 256)
                    .ok_or_else(|| invalid("Skill index size overflow"))?;
                if self.names_bytes > shenko_node::MAX_ARTIFACT_BYTES
                    || input.bytes > shenko_node::MAX_EXTERNAL_ARTIFACT_BYTES - self.position
                    || (input.path == "SKILL.md" && input.bytes > shenko_node::MAX_ARTIFACT_BYTES as u64) {
                    return Err(invalid("Skill source exceeds its index, entry or external artifact bound"));
                }
                self.current = Some(Active { remaining: input.bytes, offset: self.position, input, hash: blake3::Hasher::new() });
            }
            let active = self.current.as_mut().expect("current source file");
            let limit = buffer.len().min(active.remaining.saturating_add(1).min(usize::MAX as u64) as usize);
            let count = active.input.reader.read(&mut buffer[..limit])?;
            if count == 0 {
                if active.remaining != 0 { return Err(invalid("Skill source member shrank during capture")); }
                let active = self.current.take().expect("completed source file");
                self.files.insert(active.input.path, SourceFile { executable: active.input.executable,
                    offset: active.offset, bytes: active.input.bytes,
                    digest: Digest(format!("blake3:{}", active.hash.finalize().to_hex())) });
                continue;
            }
            if count as u64 > active.remaining { return Err(invalid("Skill source member grew during capture")); }
            active.remaining -= count as u64;
            active.hash.update(&buffer[..count]);
            if active.input.path == "SKILL.md" { self.entry.extend_from_slice(&buffer[..count]); }
            self.position += count as u64;
            let mut bytes = &buffer[..count];
            while !bytes.is_empty() {
                let size = bytes.len().min(CHUNK_BYTES - self.chunk_bytes);
                self.chunk_hash.update(&bytes[..size]);
                self.chunk_bytes += size;
                bytes = &bytes[size..];
                if self.chunk_bytes == CHUNK_BYTES {
                    self.chunks.push(Digest(format!("blake3:{}", self.chunk_hash.finalize().to_hex())));
                    self.chunk_hash.reset();
                    self.chunk_bytes = 0;
                }
            }
            return Ok(count);
        }
    }
}
