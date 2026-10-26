//! Streaming archive framing for cognition recovery bundles — SPEC §5.4.
//!
//! The archive is a self-describing byte stream: a manifest frame that
//! declares every payload, followed by the payload frames themselves. Each
//! payload is verified against its declared digest and length before the
//! receiver uses it, so a truncated or corrupted transfer can never be
//! mistaken for a complete recovery snapshot.
//!
//! Frame layout (LF terminated):
//!
//! ```text
//! shenko-cognition-archive/1
//! manifest <bytes>
//! <manifest json bytes>
//! object <index> <bytes>
//! <object bytes>
//! file <index> <bytes>
//! <file bytes>
//! git <bytes>
//! <git bundle bytes>
//! ```

use std::io::{Read, Write};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, SchemaRef};

/// Magic line that opens every archive.
pub const ARCHIVE_MAGIC: &str = "shenko-cognition-archive/1";
/// Wire schema of the archive and of its manifest.
pub const ARCHIVE_SCHEMA: u32 = 2;

/// One replicated table row.
///
/// The values keep the exact stored text, so a fact is never reshaped on its
/// way between stores. `None` is a SQL NULL and stays NULL on import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitionRow {
    pub table: String,
    /// Primary key of the row, joined with the unit separator.
    pub key: String,
    /// Column name and text value, in the table's declared column order.
    pub values: Vec<(String, Option<String>)>,
}

impl CognitionRow {
    pub fn value(&self, column: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(name, _)| name == column)
            .and_then(|(_, value)| value.as_deref())
    }
}

/// Where a replicated file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitionFileOrigin {
    /// The managed memory root. It is recovery data by default.
    Memory,
    /// An explicitly requested workspace root.
    Workspace,
}

/// A verified CAS object of the closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitionObjectDescriptor {
    pub digest: Digest,
    pub bytes: u64,
    pub media_type: String,
    pub store_namespace: String,
    pub codec: SchemaRef,
}

/// One memory or opt-in workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitionFileDescriptor {
    /// Path relative to the file's root.
    pub path: String,
    pub bytes: u64,
    pub digest: Digest,
    pub origin: CognitionFileOrigin,
}

/// The history of the memory repository, as a Git bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitionGitDescriptor {
    pub bytes: u64,
    pub digest: Digest,
    /// The source repository's symbolic HEAD, such as `refs/heads/master`.
    /// Ref names alone cannot tell the receiver which branch to continue.
    pub head: String,
    /// Ref name and object id as the source repository reports them.
    pub refs: Vec<(String, String)>,
}

/// One derivative frame of a context window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitionDerivative {
    pub window_id: EntityId,
    pub idx: i64,
    pub media_type: String,
    pub blake3: String,
    pub bytes: Vec<u8>,
}

/// A hole in the exported closure.
///
/// A blocking gap means the snapshot is not complete: the receiver must not
/// publish it and the sender must not claim it as recoverable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitionGap {
    pub kind: String,
    pub detail: String,
    pub blocking: bool,
}

/// The manifest of one archive: every fact and every payload the receiver
/// needs, plus the watermark the snapshot was taken at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CognitionManifest {
    pub schema: u32,
    /// Content fingerprint of the snapshot. Equal fingerprints mean equal
    /// recovery content, whatever the ledger sequence is.
    pub fingerprint: Digest,
    pub source_ledger_id: EntityId,
    pub source_incarnation: EntityId,
    /// Durable, source-local export order. Unlike wall-clock timestamps this
    /// orders same-ledger snapshots even when the commit watermark is equal.
    /// It is transfer bookkeeping, not snapshot content, so it is not hashed.
    pub source_snapshot_seq: u64,
    pub agent_id: Option<EntityId>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Local ledger sequence the snapshot was taken at.
    pub high_water: LedgerSeq,
    pub rows: Vec<CognitionRow>,
    pub derivatives: Vec<CognitionDerivative>,
    pub objects: Vec<CognitionObjectDescriptor>,
    /// Origins whose complete current file sets are represented by `files`.
    /// This distinguishes an authoritative source deletion from an origin
    /// that was simply not included in this export.
    pub file_origins: Vec<CognitionFileOrigin>,
    pub files: Vec<CognitionFileDescriptor>,
    pub git_history: Option<CognitionGitDescriptor>,
    pub gaps: Vec<CognitionGap>,
}

impl CognitionManifest {
    /// The gaps that forbid claiming this snapshot recoverable.
    pub fn blocking_gaps(&self) -> Vec<&CognitionGap> {
        self.gaps.iter().filter(|gap| gap.blocking).collect()
    }
}

/// Write the archive header: the magic line and the manifest frame.
pub fn write_manifest<W: Write>(writer: &mut W, manifest: &CognitionManifest) -> Result<()> {
    let manifest_bytes = serde_json::to_vec(manifest)?;
    writeln!(writer, "{ARCHIVE_MAGIC}")?;
    write_payload(
        writer,
        "manifest",
        &[manifest_bytes.len().to_string()],
        &manifest_bytes,
    )
}

/// Write one payload frame.
pub fn write_payload<W: Write>(
    writer: &mut W,
    kind: &str,
    args: &[String],
    bytes: &[u8],
) -> Result<()> {
    write!(writer, "{kind}")?;
    for arg in args {
        write!(writer, " {arg}")?;
    }
    writeln!(writer)?;
    writer.write_all(bytes)?;
    writer.write_all(b"\n")?;
    Ok(())
}

/// Read an archive.
///
/// Every payload is verified against the manifest before it is handed to
/// `on_object` (CAS objects) or buffered (files, git history). The caller
/// therefore never sees unverified bytes.
pub fn read_archive<R: Read, F>(
    reader: &mut R,
    mut on_object: F,
    files: &mut Vec<(CognitionFileDescriptor, Vec<u8>)>,
    git_history: &mut Option<(CognitionGitDescriptor, Vec<u8>)>,
) -> Result<CognitionManifest>
where
    F: FnMut(&CognitionObjectDescriptor, &[u8]) -> Result<()>,
{
    let magic = read_line(reader)?;
    ensure!(magic == ARCHIVE_MAGIC, "not a cognition archive: {magic}");
    let (kind, args) = read_frame_header(reader)?;
    ensure!(
        kind == "manifest",
        "cognition archive must start with its manifest"
    );
    let length = frame_length(&args)?;
    let mut manifest_bytes = vec![0u8; length];
    reader.read_exact(&mut manifest_bytes)?;
    expect_newline(reader)?;
    let manifest: CognitionManifest =
        serde_json::from_slice(&manifest_bytes).context("decode cognition archive manifest")?;
    ensure!(
        manifest.schema == ARCHIVE_SCHEMA,
        "unsupported cognition archive schema {}",
        manifest.schema
    );

    for (index, descriptor) in manifest.objects.iter().enumerate() {
        let (kind, args) = read_frame_header(reader)?;
        ensure!(
            kind == "object",
            "cognition archive expected object {index}, found {kind}"
        );
        let expected = index.to_string();
        ensure!(
            args.first() == Some(&expected),
            "cognition archive object {index} arrived out of order"
        );
        let length = frame_length(&args)?;
        ensure!(
            length as u64 == descriptor.bytes,
            "cognition archive object {index} has the wrong length"
        );
        let mut bytes = vec![0u8; length];
        reader.read_exact(&mut bytes)?;
        expect_newline(reader)?;
        ensure!(
            Digest::blake3(&bytes) == descriptor.digest,
            "cognition archive object {index} fails its digest"
        );
        on_object(descriptor, &bytes)?;
    }

    files.clear();
    for (index, descriptor) in manifest.files.iter().enumerate() {
        let (kind, args) = read_frame_header(reader)?;
        ensure!(
            kind == "file",
            "cognition archive expected file {index}, found {kind}"
        );
        let expected = index.to_string();
        ensure!(
            args.first() == Some(&expected),
            "cognition archive file {index} arrived out of order"
        );
        let length = frame_length(&args)?;
        ensure!(
            length as u64 == descriptor.bytes,
            "cognition archive file {} has the wrong length",
            descriptor.path
        );
        let mut bytes = vec![0u8; length];
        reader.read_exact(&mut bytes)?;
        expect_newline(reader)?;
        ensure!(
            Digest::blake3(&bytes) == descriptor.digest,
            "cognition archive file {} fails its digest",
            descriptor.path
        );
        files.push((descriptor.clone(), bytes));
    }

    *git_history = None;
    if let Some(descriptor) = manifest.git_history.as_ref() {
        let (kind, args) = read_frame_header(reader)?;
        ensure!(kind == "git", "cognition archive expected the git history");
        let length = frame_length(&args)?;
        ensure!(
            length as u64 == descriptor.bytes,
            "cognition archive git history has the wrong length"
        );
        let mut bytes = vec![0u8; length];
        reader.read_exact(&mut bytes)?;
        expect_newline(reader)?;
        ensure!(
            Digest::blake3(&bytes) == descriptor.digest,
            "cognition archive git history fails its digest"
        );
        *git_history = Some((descriptor.clone(), bytes));
    }
    Ok(manifest)
}

fn read_line<R: Read>(reader: &mut R) -> Result<String> {
    let mut out: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 {
            bail!("cognition archive ended in the middle of a line");
        }
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
        if out.len() > 4096 {
            bail!("cognition archive line is too long");
        }
    }
    String::from_utf8(out).context("cognition archive line is not utf-8")
}

fn read_frame_header<R: Read>(reader: &mut R) -> Result<(String, Vec<String>)> {
    let line = read_line(reader)?;
    let mut parts = line.split_whitespace();
    let kind = parts.next().unwrap_or_default().to_string();
    let args: Vec<String> = parts.map(|part| part.to_string()).collect();
    Ok((kind, args))
}

fn frame_length(args: &[String]) -> Result<usize> {
    let raw = args
        .last()
        .context("cognition archive frame has no length")?
        .clone();
    raw.parse::<usize>()
        .with_context(|| format!("cognition archive frame length {raw}"))
}

fn expect_newline<R: Read>(reader: &mut R) -> Result<()> {
    let mut byte = [0u8; 1];
    let read = reader.read(&mut byte)?;
    ensure!(
        read == 1 && byte[0] == b'\n',
        "cognition archive frame is not terminated"
    );
    Ok(())
}
