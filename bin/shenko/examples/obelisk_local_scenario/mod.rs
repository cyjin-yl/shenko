//! Explicit local input and complete, digest-bound originals for the acceptance runner.
//!
//! Nothing discovers a home directory or reads history until `capture` is called
//! with an owner-selected input. `open` only reads retained snapshot artifacts.
//! Capture observes stable individual files, not an atomic multi-file frontier.

use super::{create_evidence_dir, write_bytes};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use shenko_exec_host::{
    external_history::{self, ArchivedSource, ExternalHistorySource},
    obelisk_runtime::{ObeliskConfig, ObeliskProvider, ObeliskSourceRoot},
};
use shenko_store::Store;
use shenko_types::{ids::EntityId, object::Digest};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, Metadata, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom},
    path::{Component, Path, PathBuf},
};

const FORMAT: &str = "shenko.obelisk.local-scenario/1";
const MAX_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_METADATA_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CaptureLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum IdentityBasis {
    NativeSession,
    ConfiguredRootRelativePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectedFile {
    pub source: ExternalHistorySource,
    pub identity_basis: IdentityBasis,
    /// Exact slash-separated path below the selected provider root. Codex keeps
    /// its `sessions/` or `archived_sessions/` prefix; OMP/Pi keep project paths.
    pub relative_path: String,
    pub bytes: u64,
    pub digest: Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalInput {
    schema_version: u32,
    scenario: String,
    question: String,
    rubric: String,
    source_instance: String,
    limits: CaptureLimits,
    roots: Vec<ObeliskSourceRoot>,
    files: Vec<SelectedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SnapshotRoot {
    pub provider: ObeliskProvider,
    pub root_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SnapshotManifest {
    pub format: String,
    pub scenario: String,
    pub question: String,
    pub question_digest: Digest,
    pub rubric: String,
    pub rubric_digest: Digest,
    pub source_instance: String,
    /// Exact input bytes, including the original node-local root locators.
    pub selection_digest: Digest,
    pub selection_bytes: u64,
    pub limits: CaptureLimits,
    pub roots: Vec<SnapshotRoot>,
    pub files: Vec<SelectedFile>,
    pub file_count: usize,
    pub total_bytes: u64,
    /// Portable source/version inventory; absolute roots and input formatting
    /// are excluded. Equal bytes at distinct source identities remain entries.
    pub corpus_digest: Digest,
    pub coverage: String,
}

/// Constructed only after structural and byte validation. Its manifest digest
/// must enter the run binding before either arm starts inference.
#[derive(Debug)]
pub(super) struct FrozenScenario {
    root: PathBuf,
    manifest: SnapshotManifest,
    manifest_digest: Digest,
}

impl FrozenScenario {
    /// The only API that opens the selected live originals. All declarations,
    /// identities, counts and byte budgets are admitted before source reads.
    /// A failure keeps partial evidence and never publishes `snapshot.json`.
    pub fn capture(input: &Path, destination: &Path) -> Result<Self> {
        let input_bytes = stable_read(input, MAX_INPUT_BYTES, None)?;
        let selection = decode_input(&input_bytes)?;
        let manifest = make_manifest(&selection, &input_bytes)?;
        let destination = new_destination(destination)?;
        let mut roots = BTreeMap::new();
        for selected in &selection.roots {
            let path = selected.path.canonicalize().context("selected provider root unavailable")?;
            ensure!(path.is_dir(), "selected provider root is not a directory");
            ensure!(!path.starts_with(&destination) && !destination.starts_with(&path),
                "snapshot destination overlaps a selected provider root");
            ensure!(roots.values().all(|other: &PathBuf|
                !path.starts_with(other) && !other.starts_with(&path)),
                "selected provider roots overlap after resolving their locators");
            roots.insert(selected.provider.as_str(), path);
        }
        create_new_directory(&destination)?;
        for (index, selected) in manifest.files.iter().enumerate() {
            let root = roots.get(selected.source.provider.as_str())
                .context("selected source has no provider root")?;
            let path = root.join(&selected.relative_path);
            // A file alias is not another owner-selected source. Require the
            // preserved relative locator to resolve without symlink traversal.
            ensure!(path.canonicalize().context("selected original unavailable")? == path,
                "selected original uses a symlink; select its real provider-relative locator");
            let bytes = read_selected(&path, selected)
                .with_context(|| format!("capture selected original {index}"))?;
            retain_original(&destination, selected, &bytes)?;
        }
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        ensure!(manifest_bytes.len() as u64 <= MAX_INPUT_BYTES,
            "snapshot metadata exceeds its byte bound");
        write_bytes(&destination.join("selection.json"), &input_bytes)?;
        // Publish the completion marker last. A prefix or partial file set is
        // never a usable snapshot, even if earlier copies were synchronized.
        write_bytes(&destination.join("snapshot.json"), &manifest_bytes)?;
        Ok(Self { root: destination, manifest,
            manifest_digest: Digest::blake3(&manifest_bytes) })
    }

    /// Offline admission requires the digest retained in the original run
    /// binding. It never opens any root/path named by the original selection.
    pub fn open(root: &Path, expected_manifest_digest: &Digest) -> Result<Self> {
        let root = root.canonicalize().context("retained snapshot root unavailable")?;
        let (manifest, _, _) = retained_metadata(&root, expected_manifest_digest)?;
        validate_inventory(&root, &manifest)?;
        validate_originals(&root, &manifest)?;
        Ok(Self { root, manifest,
            manifest_digest: expected_manifest_digest.clone() })
    }

    pub fn manifest(&self) -> &SnapshotManifest {
        &self.manifest
    }

    pub fn manifest_digest(&self) -> &Digest {
        &self.manifest_digest
    }

    /// Materialize independent regular-file copies into a fresh arm directory;
    /// no hard links or rewrites of native provider filenames or bodies.
    /// This is a snapshot copy, not an OS mount or a JavaScript sandbox.
    pub fn mount(&self, destination: &Path) -> Result<Self> {
        let (manifest, selection_bytes, manifest_bytes) =
            retained_metadata(&self.root, &self.manifest_digest)?;
        ensure!(manifest == self.manifest, "snapshot binding changed");
        validate_inventory(&self.root, &manifest)?;
        let destination = new_destination(destination)?;
        let source_root = self.root.canonicalize()?;
        ensure!(!source_root.starts_with(&destination) && !destination.starts_with(&source_root),
            "arm snapshot destination overlaps the retained snapshot");
        create_new_directory(&destination)?;
        for selected in &manifest.files {
            let bytes = read_selected(&original_path(&self.root, selected), selected)?;
            retain_original(&destination, selected, &bytes)?;
        }
        write_bytes(&destination.join("selection.json"), &selection_bytes)?;
        write_bytes(&destination.join("snapshot.json"), &manifest_bytes)?;
        Ok(Self { root: destination, manifest,
            manifest_digest: self.manifest_digest.clone() })
    }

    /// Replace every configured live provider root with the selected copy and
    /// use a fresh, caller-owned index outside the provider roots.
    /// Executable, audited checkout, arguments and execution limits are kept.
    pub fn configuration(&self, template: &ObeliskConfig, index_home: &Path) -> Result<ObeliskConfig> {
        let index_home = new_destination(index_home)?;
        let roots: Vec<_> = self.manifest.roots.iter().map(|selected| ObeliskSourceRoot {
            provider: selected.provider,
            root_label: selected.root_label.clone(),
            path: self.root.join("originals").join(selected.provider.as_str()),
        }).collect();
        ensure!(roots.iter().all(|source|
            !index_home.starts_with(&source.path) && !source.path.starts_with(&index_home)),
            "disposable index overlaps a snapshot source root");
        let mut configuration = template.clone();
        configuration.sources = roots;
        configuration.source_instance = self.manifest.source_instance.clone();
        configuration.index_home = index_home;
        configuration.validate()?;
        Ok(configuration)
    }

    /// Use the shipped original-byte archive and its projections. Stable reads
    /// have the same complete-file boundary as runtime `archive_file`; the
    /// declared digest is checked BEFORE archive_bytes can admit any source.
    /// This does not count as subject retrieval or grant imported instructions.
    pub fn archive_into(&self, store: &Store, agent_id: EntityId) -> Result<Vec<ArchivedSource>> {
        let (manifest, _, _) = retained_metadata(&self.root, &self.manifest_digest)?;
        ensure!(manifest == self.manifest, "snapshot binding changed");
        validate_inventory(&self.root, &manifest)?;
        let mut archives = Vec::with_capacity(manifest.files.len());
        for selected in &manifest.files {
            let bytes = read_selected(&original_path(&self.root, selected), selected)?;
            let archive = external_history::archive_bytes(store, agent_id, &selected.source, &bytes)?;
            ensure!(archive.source == selected.source && archive.bytes == selected.bytes
                && archive.raw_ref.digest == selected.digest,
                "native archive differs from the selected complete source version");
            archives.push(archive);
        }
        Ok(archives)
    }

    /// Revalidate after an arm, including failed arms. Upstream custom scripts
    /// have host authority; a successful query is not source-integrity proof.
    pub fn validate(&self) -> Result<()> {
        let (manifest, _, _) = retained_metadata(&self.root, &self.manifest_digest)?;
        ensure!(manifest == self.manifest, "snapshot binding changed");
        validate_inventory(&self.root, &manifest)?;
        validate_originals(&self.root, &manifest)
    }
}

fn decode_input(bytes: &[u8]) -> Result<LocalInput> {
    let input: LocalInput = serde_json::from_slice(bytes).context("decode explicit local scenario")?;
    ensure!(input.schema_version == 1, "unsupported local scenario schema");
    metadata_text(&input.scenario)?;
    metadata_text(&input.source_instance)?;
    ensure!(input.source_instance.len() <= 512,
        "source instance exceeds the native runtime identity bound");
    for text in [&input.question, &input.rubric] {
        ensure!(!text.trim().is_empty() && text.len() <= MAX_TEXT_BYTES,
            "question and rubric must be nonempty and within 64 KiB each");
    }
    ensure!((1..=MAX_FILES).contains(&input.limits.max_files)
        && (1..=MAX_FILE_BYTES).contains(&input.limits.max_file_bytes)
        && (1..=MAX_TOTAL_BYTES).contains(&input.limits.max_total_bytes),
        "invalid local scenario count or byte limits");
    ensure!(!input.files.is_empty() && input.files.len() <= input.limits.max_files,
        "explicit source selection is empty or exceeds its file count budget");
    ensure!(!input.roots.is_empty() && input.roots.len() <= 3,
        "select one root for each configured provider");
    let mut providers = BTreeMap::new();
    let mut labels = BTreeSet::new();
    for root in &input.roots {
        metadata_text(&root.root_label)?;
        ensure!(!root.root_label.contains(['/', '\\']),
            "root labels cannot contain path separators");
        absolute_path(&root.path)?;
        ensure!(providers.insert(root.provider.as_str(), root).is_none(),
            "a provider has more than one selected root");
        ensure!(labels.insert(&root.root_label), "selected root labels are duplicated");
    }
    let mut logical_sources = BTreeSet::new();
    let mut paths: BTreeMap<&str, BTreeSet<&Path>> = BTreeMap::new();
    let mut total_bytes = 0u64;
    for selected in &input.files {
        let source = &selected.source;
        for value in [&source.provider, &source.source_instance, &source.source_key] {
            metadata_text(value)?;
        }
        ensure!(source.source_instance == input.source_instance,
            "selected source belongs to a different source instance");
        let root = providers.get(source.provider.as_str())
            .context("selected source provider has no explicit root")?;
        let relative = relative_path(&selected.relative_path)?;
        match selected.identity_basis {
            IdentityBasis::NativeSession => {
                let session_id = source.session_id.as_deref()
                    .context("native session identity has no session id")?;
                metadata_text(session_id)?;
                ensure!(session_id.strip_prefix(source.provider.as_str())
                    .and_then(|suffix| suffix.strip_prefix(':'))
                    .is_some_and(|native| !native.trim().is_empty())
                    && source.source_key == format!("session:{session_id}"),
                    "native source key must match the provider-qualified runtime session identity");
            }
            IdentityBasis::ConfiguredRootRelativePath => {
                ensure!(source.session_id.is_none()
                    && source.source_key == format!("root:{}/{}", root.root_label, selected.relative_path),
                    "locator source key must match the runtime root-label/relative-path identity");
            }
        }
        ensure!(logical_sources.insert((&source.provider, &source.source_instance, &source.source_key)),
            "the same logical source was selected more than once");
        let provider_paths = paths.entry(source.provider.as_str()).or_default();
        ensure!(provider_paths.iter().all(|other|
            !relative.starts_with(other) && !other.starts_with(relative)),
            "selected provider-relative locators collide or overlap");
        provider_paths.insert(relative);
        ensure!((1..=input.limits.max_file_bytes).contains(&selected.bytes),
            "selected original is empty or exceeds its per-file byte budget");
        canonical_digest(&selected.digest)?;
        total_bytes = total_bytes.checked_add(selected.bytes).context("source byte total overflow")?;
        ensure!(total_bytes <= input.limits.max_total_bytes,
            "selected originals exceed the total byte budget");
    }
    ensure!(providers.keys().all(|provider| paths.contains_key(provider)),
        "a selected provider root has no explicitly selected original");
    Ok(input)
}

fn make_manifest(input: &LocalInput, input_bytes: &[u8]) -> Result<SnapshotManifest> {
    let mut roots: Vec<_> = input.roots.iter().map(|root| SnapshotRoot {
        provider: root.provider, root_label: root.root_label.clone(),
    }).collect();
    roots.sort_by_key(|root| root.provider);
    let mut files = input.files.clone();
    files.sort_by(|a, b| (&a.source.provider, &a.relative_path)
        .cmp(&(&b.source.provider, &b.relative_path)));
    let corpus_digest = Digest::blake3(&serde_json::to_vec(&(FORMAT, &roots, &files))?);
    Ok(SnapshotManifest {
        format: FORMAT.into(), scenario: input.scenario.clone(),
        question: input.question.clone(), question_digest: Digest::blake3(input.question.as_bytes()),
        rubric: input.rubric.clone(), rubric_digest: Digest::blake3(input.rubric.as_bytes()),
        source_instance: input.source_instance.clone(),
        selection_digest: Digest::blake3(input_bytes), selection_bytes: input_bytes.len() as u64,
        limits: input.limits.clone(), roots, file_count: files.len(),
        total_bytes: files.iter().map(|file| file.bytes).sum(), files, corpus_digest,
        coverage: "complete selected individual file versions; unselected files and cross-file frontier unknown".into(),
    })
}

fn retained_metadata(root: &Path, expected: &Digest) -> Result<(SnapshotManifest, Vec<u8>, Vec<u8>)> {
    canonical_digest(expected)?;
    let manifest_bytes = stable_read(&root.join("snapshot.json"), MAX_INPUT_BYTES, None)?;
    ensure!(Digest::blake3(&manifest_bytes) == *expected,
        "retained snapshot differs from the original run binding");
    let manifest: SnapshotManifest = serde_json::from_slice(&manifest_bytes)
        .context("decode retained snapshot")?;
    ensure!(manifest.format == FORMAT && manifest.selection_bytes <= MAX_INPUT_BYTES,
        "unsupported or oversized retained snapshot metadata");
    let input_bytes = stable_read(&root.join("selection.json"), MAX_INPUT_BYTES,
        Some((manifest.selection_bytes, &manifest.selection_digest)))?;
    let input = decode_input(&input_bytes)?;
    ensure!(make_manifest(&input, &input_bytes)? == manifest,
        "snapshot fields differ from the retained owner selection");
    Ok((manifest, input_bytes, manifest_bytes))
}

fn metadata_text(value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty() && value.len() <= MAX_METADATA_BYTES
        && !value.chars().any(char::is_control), "invalid source identity metadata");
    Ok(())
}

fn canonical_digest(digest: &Digest) -> Result<()> {
    ensure!(digest.0.strip_prefix("blake3:").is_some_and(|hex|
        hex.len() == 64 && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))),
        "source digest must be a canonical blake3 digest");
    Ok(())
}

fn relative_path(value: &str) -> Result<&Path> {
    metadata_text(value)?;
    let path = Path::new(value);
    ensure!(!value.contains('\\') && value.split('/').all(|part|
        !part.is_empty() && part != "." && part != "..")
        && path.components().all(|part| matches!(part, Component::Normal(_))),
        "original locator must be an exact portable provider-relative path");
    Ok(path)
}

fn absolute_path(path: &Path) -> Result<()> {
    let text = path.to_str().context("path is not UTF-8")?;
    metadata_text(text)?;
    ensure!(path.is_absolute()
        && !path.components().any(|part| matches!(part, Component::CurDir | Component::ParentDir)),
        "local root must be an absolute path without dot components");
    Ok(())
}

fn new_destination(path: &Path) -> Result<PathBuf> {
    absolute_path(path)?;
    let parent = path.parent().context("snapshot destination has no parent")?;
    let parent = parent.canonicalize().context("snapshot parent must already exist")?;
    let path = parent.join(path.file_name().context("snapshot destination has no filename")?);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Ok(_) => anyhow::bail!("snapshot or index destination already exists; originals are never replaced"),
        Err(error) => Err(error).context("inspect snapshot destination"),
    }
}

fn create_new_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path).context("create fresh private snapshot")?;
    File::open(path.parent().context("snapshot parent missing")?)?.sync_all()?;
    Ok(())
}

fn original_path(root: &Path, selected: &SelectedFile) -> PathBuf {
    root.join("originals").join(&selected.source.provider).join(&selected.relative_path)
}

fn retain_original(root: &Path, selected: &SelectedFile, bytes: &[u8]) -> Result<()> {
    let path = original_path(root, selected);
    create_evidence_dir(path.parent().context("original has no parent")?)?;
    write_bytes(&path, bytes)
}

fn read_selected(path: &Path, selected: &SelectedFile) -> Result<Vec<u8>> {
    stable_read(path, selected.bytes, Some((selected.bytes, &selected.digest)))
}

// Like archive_file, both complete reads use one descriptor and compare size,
// modification observation and content. The second pass compares bounded
// chunks directly, avoiding another full-file allocation or a new hasher API.
fn stable_read(path: &Path, max_bytes: u64, expected: Option<(u64, &Digest)>) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).context("selected or retained source unavailable")?;
    let before = file.metadata()?;
    ensure!(before.is_file() && before.len() <= max_bytes,
        "source is not a bounded complete regular file; no prefix accepted");
    if let Some((bytes, _)) = expected {
        ensure!(before.len() == bytes, "source byte length differs from the selected version");
    }
    let read_limit = before.len().checked_add(1).context("source read limit overflow")?;
    let mut bytes = Vec::with_capacity(usize::try_from(read_limit)?);
    (&mut file).take(read_limit).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == before.len(), "source changed during its first complete read");
    file.seek(SeekFrom::Start(0))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut offset = 0usize;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        let end = offset.checked_add(count).context("source verification length overflow")?;
        ensure!(end <= bytes.len() && bytes[offset..end] == buffer[..count],
            "source changed during its second complete read");
        offset = end;
    }
    let after = file.metadata()?;
    let locator = std::fs::symlink_metadata(path).context("source locator disappeared")?;
    ensure!(before.len() == after.len() && before.modified()? == after.modified()?
        && offset == bytes.len() && locator.is_file() && same_file(&after, &locator)?,
        "source changed or its locator was replaced while capturing");
    if let Some((_, digest)) = expected {
        ensure!(Digest::blake3(&bytes) == *digest,
            "complete source digest differs from the selected version");
    }
    Ok(bytes)
}

fn same_file(a: &Metadata, b: &Metadata) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(not(unix))]
    {
        Ok(a.len() == b.len() && a.modified()? == b.modified()?)
    }
}

fn validate_originals(root: &Path, manifest: &SnapshotManifest) -> Result<()> {
    for selected in &manifest.files {
        read_selected(&original_path(root, selected), selected)?;
    }
    Ok(())
}

// Walk only the new, bounded snapshot tree. Never enumerate an original root.
// Unknown files/directories and symlinks cannot become upstream index inputs.
fn validate_inventory(root: &Path, manifest: &SnapshotManifest) -> Result<()> {
    let originals = root.join("originals");
    let metadata = std::fs::symlink_metadata(&originals)?;
    ensure!(metadata.is_dir(), "snapshot originals root is not a real directory");
    let expected_files: BTreeSet<_> = manifest.files.iter().map(|file|
        PathBuf::from(&file.source.provider).join(&file.relative_path)).collect();
    let mut pending = vec![PathBuf::new()];
    let mut seen = BTreeSet::new();
    let mut entries = 0usize;
    // This upper bound counts shared directories more than once, but never
    // allocates every progressively longer prefix of a deep selected path.
    let max_entries = expected_files.iter().map(|file| file.components().count()).sum::<usize>();
    while let Some(relative) = pending.pop() {
        for entry in std::fs::read_dir(originals.join(&relative))? {
            let entry = entry?;
            entries += 1;
            ensure!(entries <= max_entries, "snapshot contains unselected entries");
            let path = relative.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                ensure!(expected_files.iter().any(|file| file != &path && file.starts_with(&path)),
                    "snapshot contains an unselected directory");
                pending.push(path);
            } else {
                ensure!(kind.is_file() && expected_files.contains(&path),
                    "snapshot contains a symlink or unselected original");
                seen.insert(path);
            }
        }
    }
    ensure!(seen == expected_files, "snapshot is missing a selected original");
    Ok(())
}

#[cfg(test)]
mod tests;
