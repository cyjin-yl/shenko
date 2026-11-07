//! Managed copy-on-write workspace/branch store (SPEC §41.3).
//!
//! A COW mount keeps its `source` base readonly on disk forever. All guest
//! mutations land in a per-branch overlay under the Shenko home:
//!
//! ```text
//! <home>/workspaces/<workspace>/branches/<branch>/
//!     overlay/<mount_key>/...        copied-up files/dirs (real bytes)
//!     tombstones/<mount_key>.json    removed base paths (durable set)
//!     commits.jsonl                  append-only commit records
//!     head                           current commit id (atomic write)
//!     publish.json                   in-flight publication marker
//!     publish-objects.jsonl          objects a publisher has sealed so far
//! ```
//!
//! The effective guest view is `overlay ∪ base − tombstones` with the
//! overlay winning on conflicts. A commit is a content-addressed manifest
//! over that effective view *and* a byte closure: every recorded file's bytes
//! are sealed into this installation's CAS, and every recorded symbolic link
//! keeps its exact target bytes, so a commit id names recoverable content
//! instead of a set of digests that nothing can read back.
//!
//! Two admission surfaces exist on purpose: guest-facing operations go
//! through [`crate::mount::OwnedDir`] handles (A08 confinement — no pathname
//! is ever re-resolved for the guest), while host-side lifecycle operations
//! (`commit`, `diff`, `discard`, seed preparation) run on absolute paths in
//! trusted host code after the guest has finished.
//!
//! Ownership is a real, cross-process claim: [`BranchWriter`] takes an
//! exclusive advisory lock on the branch directory's own descriptor — the
//! technique the native runtime owner already uses, which leaves no lock file
//! in the target and cannot be bypassed by an unlucky PID check. Commit and
//! discard acquire it before they touch the branch, so a live exec writer and
//! a racing publisher are refused with a typed reason instead of racing.

use anyhow::{Context, Result, anyhow, bail, ensure};
use cap_std::fs::OpenOptions;
use serde::{Deserialize, Serialize};
use shenko_store::cas::ContentStore;
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::checkpoint::{closure_directory, closure_file, closure_root, closure_symlink, validate_rel};
use crate::fsx;
use crate::mount::OwnedDir;

pub const WORKSPACES_DIR: &str = "workspaces";
pub const BRANCHES_DIR: &str = "branches";
pub const OVERLAY_DIR: &str = "overlay";
pub const TOMBSTONES_DIR: &str = "tombstones";
pub const COMMITS_FILE: &str = "commits.jsonl";
pub const HEAD_FILE: &str = "head";
/// Versioned commit format. A commit written under another format is refused
/// as a byte closure instead of being reinterpreted.
pub const COMMIT_FORMAT: &str = "shenko-cow-commit/2";
/// The earlier format recorded content digests without sealed bytes. Records
/// in it are still readable as history but are never claimed to be a closure.
pub const LEGACY_COMMIT_FORMAT: &str = "shenko-cow-commit/1";
pub const COMMIT_MEDIA_TYPE: &str = "application/vnd.shenko.cow-commit+json";
pub const COMMIT_CODEC_NAME: &str = "shenko-cow-commit";
pub const COMMIT_CODEC_MAJOR: u32 = 2;
/// CAS namespace shared by COW commit objects and their file objects.
pub const COW_STORE_NAMESPACE: &str = "seed-home-cow";
pub const FILE_MEDIA_TYPE: &str = "application/octet-stream";
pub const FILE_CODEC_NAME: &str = "shenko-cow-file";
pub const FILE_CODEC_MAJOR: u32 = 1;
/// In-flight publication: the marker exists from before the first object is
/// sealed until `head` names the published commit.
pub const PUBLISH_MARKER_FILE: &str = "publish.json";
pub const PUBLISH_OBJECTS_FILE: &str = "publish-objects.jsonl";
pub const PUBLISH_FORMAT: &str = "shenko-cow-publish/1";
/// What an in-flight publication is doing. A commit and a rollback both seal
/// objects and append one node; a rollback also rewrites the working state, so
/// an interrupted publication must say which one it was.
pub const PUBLISH_KIND_COMMIT: &str = "commit";
pub const PUBLISH_KIND_ROLLBACK: &str = "rollback";
pub const PUBLISH_KIND_DISCARD: &str = "discard";

/// Depth/entry caps for the recursive effective-view walk. A guest can
/// create at most CAP entries per mount before commit refuses; this bounds
/// manifest work without limiting ordinary tool use.
const MAX_TREE_DEPTH: usize = 64;
const MAX_TREE_ENTRIES: usize = 200_000;

fn rel_slash(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Durable set of removed base paths for one mount. Stored as normalized
/// `/`-separated relative paths; a removed directory covers its whole
/// subtree.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tombstones {
    #[serde(default)]
    removed: BTreeSet<String>,
}

impl Tombstones {
    pub fn load(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_slice(bytes).map_err(|e| anyhow!("tombstones JSON: {e}"))
    }

    pub fn save(&self) -> Vec<u8> {
        serde_json::to_vec(&self).expect("tombstones serialize")
    }

    /// Whether `rel` (or an ancestor) was removed from the base view.
    pub fn is_removed(&self, rel: &Path) -> bool {
        let mut probe = Some(rel.to_path_buf());
        while let Some(path) = probe {
            if self.removed.contains(&rel_slash(&path)) {
                return true;
            }
            probe = path.parent().map(|p| p.to_path_buf());
        }
        false
    }

    /// Record a removed path.
    pub fn remove(&mut self, rel: &Path) {
        self.removed.insert(rel_slash(rel));
    }

    /// Drop any tombstone at or below `rel` (a recreated dir/file shadows
    /// the removal again).
    pub fn clear_below(&mut self, rel: &Path) {
        let prefix = rel_slash(rel);
        let doomed: Vec<String> = self
            .removed
            .range(prefix.clone()..)
            .take_while(|entry| entry.as_str() == prefix.as_str() || entry.starts_with(&format!("{prefix}/")))
            .cloned()
            .collect();
        for entry in doomed {
            self.removed.remove(&entry);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.removed.is_empty()
    }

    pub fn len(&self) -> usize {
        self.removed.len()
    }
}

/// One entry of a commit manifest.
///
/// `digest` is the content identity: the file's bytes for a file, the empty
/// digest for a directory, and the exact link target bytes for a symbolic
/// link. A digest alone is not recoverable content, so a file entry also
/// carries the `object` that was sealed in this installation's CAS and a
/// symbolic link carries its literal `target`. `mode` is what a restore needs
/// to reproduce the same permission bits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEntry {
    pub kind: CommitEntryKind,
    pub bytes: u64,
    pub digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<ObjectRef>,
}

/// Entry kinds retain the existing manifest strings without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitEntryKind {
    #[serde(rename = "dir")]
    Directory,
    #[serde(rename = "file")]
    File,
    #[serde(rename = "symlink")]
    Symlink,
}

/// The CAS reference for one sealed commit file object. The locator is left
/// empty on purpose: a commit is resolved in *this* installation's store and
/// never through a historical host path.
fn cow_object_ref(digest: &Digest, bytes: u64) -> ObjectRef {
    ObjectRef {
        digest: digest.clone(),
        bytes,
        media_type: FILE_MEDIA_TYPE.into(),
        store_namespace: COW_STORE_NAMESPACE.into(),
        locator: String::new(),
        codec: SchemaRef {
            name: FILE_CODEC_NAME.into(),
            major: FILE_CODEC_MAJOR,
            digest: Digest::blake3(FILE_CODEC_NAME.as_bytes()),
        },
    }
}

/// A content-addressed commit over the branch's effective view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRecord {
    pub format: String,
    pub workspace: String,
    pub branch: String,
    pub parent: Option<Digest>,
    pub timestamp_unix_ms: u64,
    /// mount_key -> relative path -> entry (removed paths carry `None`).
    pub mounts: BTreeMap<String, BTreeMap<String, Option<CommitEntry>>>,
}

impl CommitRecord {
    pub fn id(&self) -> Digest {
        // Canonical form: serde_json over BTreeMaps is deterministic.
        let bytes = serde_json::to_vec(self).expect("commit manifest serialize");
        Digest::blake3(&bytes)
    }

    pub fn entry_count(&self) -> usize {
        self.mounts.values().map(BTreeMap::len).sum()
    }

    /// Verify this record is a complete, recoverable byte closure in `cas`.
    ///
    /// Every recorded path is canonical and its ancestors are recorded
    /// directories of the same mount; every file names a sealed object whose
    /// length and digest match; every link keeps its exact target bytes. This
    /// verifies content, not the working tree: it never reads the base or the
    /// overlay, so a commit stays checkable after the guest deleted its work.
    pub fn verify_closure(&self, cas: &ContentStore) -> Result<VerifiedCommit> {
        ensure!(
            self.format == COMMIT_FORMAT,
            "HOME_COW_INCOMPLETE: commit is format {:?}, not {COMMIT_FORMAT:?}; a {:?} record lists content digests without sealed bytes and is not a recoverable closure",
            self.format,
            LEGACY_COMMIT_FORMAT
        );
        ensure!(
            !self.workspace.is_empty() && !self.branch.is_empty(),
            "HOME_COW_INCOMPLETE: commit names no workspace/branch identity"
        );
        let mut entries = 0usize;
        let mut objects = 0usize;
        let mut bytes = 0u64;
        for (mount_key, tree) in &self.mounts {
            ensure!(
                !mount_key.is_empty(),
                "HOME_COW_INCOMPLETE: commit has an empty mount key"
            );
            for (rel, entry) in tree {
                validate_rel(rel)
                    .with_context(|| format!("HOME_COW_INCOMPLETE: commit path {rel:?}"))?;
                let mut ancestors: Vec<&str> = rel.split('/').collect();
                ancestors.pop();
                while !ancestors.is_empty() {
                    let ancestor = ancestors.join("/");
                    ensure!(
                        tree.get(&ancestor).is_some_and(|entry| entry
                            .as_ref()
                            .is_some_and(|entry| entry.kind == CommitEntryKind::Directory)),
                        "HOME_COW_INCOMPLETE: commit path {rel:?} has no recorded directory {ancestor:?}"
                    );
                    ancestors.pop();
                }
                let Some(entry) = entry else {
                    // A removed path carries no content; it is the durable
                    // tombstone for this view.
                    continue;
                };
                entries += 1;
                match entry.kind {
                    CommitEntryKind::Directory => {
                        ensure!(
                            entry.bytes == 0
                                && entry.target.is_none()
                                && entry.object.is_none()
                                && entry.digest == Digest::blake3(b""),
                            "HOME_COW_INCOMPLETE: commit directory {rel:?} carries file content"
                        );
                    }
                    CommitEntryKind::File => {
                        ensure!(
                            entry.target.is_none(),
                            "HOME_COW_INCOMPLETE: commit file {rel:?} carries a link target"
                        );
                        let object = entry.object.as_ref().ok_or_else(|| {
                            anyhow!(
                                "HOME_COW_INCOMPLETE: commit file {rel:?} has no sealed object, so its bytes are not recoverable"
                            )
                        })?;
                        ensure!(
                            object.digest == entry.digest && object.bytes == entry.bytes,
                            "HOME_COW_INCOMPLETE: commit file {rel:?} object {} does not match recorded {} ({} bytes)",
                            object.digest,
                            entry.digest,
                            entry.bytes
                        );
                        let path = cas.verified_object_path(object).map_err(|error| {
                            anyhow!(
                                "HOME_COW_INCOMPLETE: commit file {rel:?} object {} is missing or corrupt: {error}",
                                entry.digest
                            )
                        })?;
                        ensure!(
                            path.is_file(),
                            "HOME_COW_INCOMPLETE: commit file {rel:?} object is not a file"
                        );
                        objects += 1;
                        bytes += entry.bytes;
                    }
                    CommitEntryKind::Symlink => {
                        let target = entry.target.as_deref().ok_or_else(|| {
                            anyhow!("HOME_COW_INCOMPLETE: commit link {rel:?} has no recorded target")
                        })?;
                        ensure!(
                            entry.bytes == 0 && entry.object.is_none(),
                            "HOME_COW_INCOMPLETE: commit link {rel:?} carries file content"
                        );
                        ensure!(
                            entry.digest == Digest::blake3(target.as_bytes()),
                            "HOME_COW_INCOMPLETE: commit link {rel:?} target bytes do not match their digest"
                        );
                    }
                }
            }
        }
        Ok(VerifiedCommit {
            id: self.id(),
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            parent: self.parent.clone(),
            timestamp_unix_ms: self.timestamp_unix_ms,
            entries,
            objects,
            bytes,
        })
    }
}

/// Summary of a verified commit closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCommit {
    pub id: Digest,
    pub workspace: String,
    pub branch: String,
    pub parent: Option<Digest>,
    pub timestamp_unix_ms: u64,
    pub entries: usize,
    pub objects: usize,
    pub bytes: u64,
}

/// Result of committing a branch.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub record: CommitRecord,
    pub id: Digest,
    /// False when the effective view already equalled the published head, so
    /// no journal line was appended and `head` did not move.
    pub published: bool,
    /// Sealed file objects the commit references.
    pub objects: usize,
    pub bytes: u64,
}

/// Summary of base→branch changes for one mount.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CowDiff {
    pub added: BTreeSet<String>,
    pub modified: BTreeSet<String>,
    pub removed: BTreeSet<String>,
}

impl CowDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

/// A branch is already owned by a live writer.
#[derive(Debug, thiserror::Error)]
pub enum BranchBusy {
    #[error(
        "HOME_BRANCH_BUSY: COW branch {workspace}/{branch} is owned by another live writer \
         (exclusive descriptor lock held); commit, discard and a second writer are refused \
         rather than racing it"
    )]
    Writer { workspace: String, branch: String },
}

/// Exclusive, cross-process claim on one branch.
///
/// The claim is an advisory lock on the branch directory's own open
/// descriptor: it creates no lock file inside the branch, is released when the
/// last clone is dropped, and cannot be defeated by a stale PID or by two
/// processes that both believe they are the owner. Cloning the claim shares
/// the same descriptor and therefore the same lock.
#[derive(Clone)]
pub struct BranchWriter {
    inner: Arc<BranchWriterInner>,
}

struct BranchWriterInner {
    /// Kept open for as long as any clone lives: dropping it releases the lock.
    _directory: fs::File,
    device: u64,
    inode: u64,
}

impl std::fmt::Debug for BranchWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchWriter")
            .field("device", &self.inner.device)
            .field("inode", &self.inner.inode)
            .finish_non_exhaustive()
    }
}

impl BranchWriter {
    /// Acquire the branch's writer claim without waiting.
    pub fn try_acquire(home: &Path, workspace: &str, branch: &str) -> Result<Self> {
        let home_dir = OwnedDir::admit_absolute(home).with_context(|| {
            format!(
                "HOME_BRANCH_UNAVAILABLE: admit home {} for branch ownership",
                home.display()
            )
        })?;
        let rel = BranchStore::rel_branch_dir(workspace, branch);
        let branch_dir = OwnedDir::admit_relative(&home_dir, &rel).with_context(|| {
            format!("HOME_BRANCH_UNAVAILABLE: enter COW branch {workspace}/{branch}")
        })?;
        let (device, inode) = branch_dir.identity();
        let directory = branch_dir
            .dir()
            .try_clone()
            .map_err(|error| anyhow!("HOME_BRANCH_UNAVAILABLE: clone branch descriptor: {error}"))?
            .into_std_file();
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let result = unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(libc::EWOULDBLOCK)
                    || error.raw_os_error() == Some(libc::EAGAIN)
                {
                    return Err(BranchBusy::Writer {
                        workspace: workspace.to_string(),
                        branch: branch.to_string(),
                    }
                    .into());
                }
                bail!(
                    "HOME_BRANCH_UNAVAILABLE: lock COW branch {workspace}/{branch}: {error}"
                );
            }
        }
        #[cfg(not(unix))]
        {
            let _ = directory;
            bail!(
                "HOME_BRANCH_UNAVAILABLE: cross-process COW branch ownership requires descriptor \
                 locks, which this target does not provide"
            );
        }
        Ok(Self {
            inner: Arc::new(BranchWriterInner {
                _directory: directory,
                device,
                inode,
            }),
        })
    }

    /// Filesystem identity of the branch directory this claim locks. Receipts
    /// bind it so a claim on a replaced directory is detectable.
    pub fn identity(&self) -> (u64, u64) {
        (self.inner.device, self.inner.inode)
    }
}

/// Marker written before a commit seals its first object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishMarker {
    pub format: String,
    pub workspace: String,
    pub branch: String,
    pub parent: Option<Digest>,
    /// Set once the record id is known, before the journal append.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Digest>,
    pub started_unix_ms: u64,
    /// `commit` or `rollback`; a rollback may already have changed the working
    /// state when it stopped.
    #[serde(default = "default_publish_kind")]
    pub kind: String,
}

fn default_publish_kind() -> String {
    PUBLISH_KIND_COMMIT.to_string()
}

/// One object a publisher has sealed (durable list for reconciliation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedObject {
    pub digest: Digest,
    pub bytes: u64,
}

/// What reconciling an interrupted publication found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationReconciliation {
    /// No publication was in flight.
    Nothing,
    /// `head` already names the record the marker was publishing. For a
    /// `commit` nothing else was pending; for a `rollback`/`discard` the
    /// working state was re-applied from the published node so the branch
    /// view and the log agree.
    Completed { record: Digest, kind: String },
    /// The publisher stopped before `head` moved. The listed objects are not
    /// referenced by any published commit and are candidates for reclamation;
    /// the marker (and any journal line appended just before the interruption)
    /// is retained as evidence, not overwritten. A `rollback` may already have
    /// changed the working state.
    Abandoned { staged: Vec<StagedObject>, kind: String },
}

/// A workspace branch: the durable identity a COW exec binds to. Cheap to
/// clone; all state is on disk under the home.
#[derive(Debug, Clone)]
pub struct BranchStore {
    pub workspace: String,
    pub branch: String,
    /// Branch directory relative to the home root.
    rel_branch_dir: PathBuf,
}

impl BranchStore {
    /// Relative branch directory for a workspace/branch binding.
    pub fn rel_branch_dir(workspace: &str, branch: &str) -> PathBuf {
        PathBuf::from(WORKSPACES_DIR)
            .join(workspace)
            .join(BRANCHES_DIR)
            .join(branch)
    }

    /// Open the branch through the admitted home handle, creating the branch
    /// skeleton (`overlay/`, `tombstones/`) when absent. Creation goes
    /// through the handle, so workspace state is confined to the home.
    pub fn open(home: &OwnedDir, workspace: &str, branch: &str) -> Result<Self> {
        let rel = Self::rel_branch_dir(workspace, branch);
        let dir = home.dir();
        dir.create_dir_all(rel.join(OVERLAY_DIR))
            .and_then(|()| dir.create_dir_all(rel.join(TOMBSTONES_DIR)))
            .map_err(|e| anyhow!("create branch skeleton {}: {e}", rel.display()))?;
        Ok(Self {
            workspace: workspace.to_string(),
            branch: branch.to_string(),
            rel_branch_dir: rel,
        })
    }

    /// Open an existing branch without creating anything (exec-time
    /// validation: a declared binding must already be prepared).
    pub fn open_existing(home: &OwnedDir, workspace: &str, branch: &str) -> Result<Self> {
        let rel = Self::rel_branch_dir(workspace, branch);
        let exists = home
            .dir()
            .symlink_metadata(&rel)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        ensure!(
            exists,
            "HOME_INVALID: COW branch {workspace}/{branch} is not prepared under the home"
        );
        Ok(Self {
            workspace: workspace.to_string(),
            branch: branch.to_string(),
            rel_branch_dir: rel,
        })
    }

    /// Bind an existing branch identity without creating or admitting
    /// anything. Discovery uses this: reading a branch's durable records must
    /// not prepare workspace state.
    pub fn bind_absolute(workspace: &str, branch: &str) -> Self {
        Self {
            workspace: workspace.to_string(),
            branch: branch.to_string(),
            rel_branch_dir: Self::rel_branch_dir(workspace, branch),
        }
    }

    pub fn rel_overlay(&self, mount_key: &str) -> PathBuf {
        self.rel_branch_dir.join(OVERLAY_DIR).join(mount_key)
    }

    pub fn rel_tombstones(&self, mount_key: &str) -> PathBuf {
        self.rel_branch_dir
            .join(TOMBSTONES_DIR)
            .join(format!("{mount_key}.json"))
    }

    pub fn rel_commits_file(&self) -> PathBuf {
        self.rel_branch_dir.join(COMMITS_FILE)
    }

    pub fn rel_head_file(&self) -> PathBuf {
        self.rel_branch_dir.join(HEAD_FILE)
    }

    pub fn rel_publish_marker(&self) -> PathBuf {
        self.rel_branch_dir.join(PUBLISH_MARKER_FILE)
    }

    pub fn rel_publish_objects(&self) -> PathBuf {
        self.rel_branch_dir.join(PUBLISH_OBJECTS_FILE)
    }

    /// Take the branch's exclusive writer claim for the lifetime of the
    /// returned guard (the guest run keeps it; commit/discard take it too).
    pub fn acquire_writer(&self, home: &Path) -> Result<BranchWriter> {
        BranchWriter::try_acquire(home, &self.workspace, &self.branch)
    }

    /// Whether another live writer currently owns this branch.
    pub fn writer_busy(&self, home: &Path) -> Result<bool> {
        match self.acquire_writer(home) {
            Ok(_) => Ok(false),
            Err(error) => match error.downcast_ref::<BranchBusy>() {
                Some(BranchBusy::Writer { .. }) => Ok(true),
                None => Err(error),
            },
        }
    }

    /// Per-mount COW state: admitted overlay handle + tombstone set.
    pub fn mount(&self, home: &OwnedDir, mount_key: &str) -> Result<CowMountStore> {
        let rel_overlay = self.rel_overlay(mount_key);
        let rel_tombstones = self.rel_tombstones(mount_key);
        home.dir()
            .create_dir_all(&rel_overlay)
            .map_err(|e| anyhow!("create overlay {}: {e}", rel_overlay.display()))?;
        let tombstones = match home.dir().open(&rel_tombstones) {
            Ok(mut file) => {
                use std::io::Read;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|e| anyhow!("read tombstones {}: {e}", rel_tombstones.display()))?;
                Tombstones::load(&bytes)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Tombstones::default(),
            Err(e) => {
                return Err(anyhow!("open tombstones {}: {e}", rel_tombstones.display()));
            }
        };
        let overlay = OwnedDir::admit_relative(home, &rel_overlay)
            .with_context(|| format!("admit overlay {}", rel_overlay.display()))?;
        Ok(CowMountStore {
            home: overlay_handle_clone(home)?,
            overlay,
            rel_overlay,
            rel_tombstones,
            tombstones,
        })
    }

    /// Current head commit id, if any.
    pub fn head_abs(&self, home: &Path) -> Result<Option<Digest>> {
        let path = home.join(self.rel_head_file());
        match fs::read_to_string(&path) {
            Ok(text) => {
                let text = text.trim();
                if text.is_empty() {
                    Ok(None)
                } else {
                    let digest = Digest(text.to_string());
                    if digest.canonical_blake3_hex().is_none() {
                        bail!("corrupt head {}: {text}", path.display());
                    }
                    Ok(Some(digest))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!("read head {}: {e}", path.display())),
        }
    }

    /// All commits, oldest first.
    pub fn commits_abs(&self, home: &Path) -> Result<Vec<CommitRecord>> {
        let path = home.join(self.rel_commits_file());
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow!("read commits {}: {e}", path.display())),
        };
        let mut out = Vec::new();
        for line in bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            out.push(
                serde_json::from_slice(line)
                    .map_err(|e| anyhow!("parse commit in {}: {e}", path.display()))?,
            );
        }
        Ok(out)
    }

    /// One commit record from the append-only journal.
    pub fn record_abs(&self, home: &Path, id: &Digest) -> Result<CommitRecord> {
        self.commits_abs(home)?
            .into_iter()
            .find(|record| record.id() == *id)
            .ok_or_else(|| {
                anyhow!(
                    "HOME_COW_INCOMPLETE: commit {id} is not in {}",
                    home.join(self.rel_commits_file()).display()
                )
            })
    }

    /// Compute the effective-view diff for one mount (host-side, absolute
    /// paths; called after the guest has finished).
    pub fn diff_abs(&self, home: &Path, mount_key: &str, base: &Path) -> Result<CowDiff> {
        let overlay = home.join(self.rel_overlay(mount_key));
        let tombstones = self.tombstones_abs(home, mount_key)?;
        diff_trees(base, &overlay, &tombstones)
    }

    fn tombstones_abs(&self, home: &Path, mount_key: &str) -> Result<Tombstones> {
        let path = home.join(self.rel_tombstones(mount_key));
        match fs::read(&path) {
            Ok(bytes) => Tombstones::load(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tombstones::default()),
            Err(e) => Err(anyhow!("read tombstones {}: {e}", path.display())),
        }
    }

    /// Commit the branch's effective view across all supplied mount bases.
    ///
    /// Publication order follows SPEC §10.5: the in-flight marker is written
    /// before the first object, every recorded file's bytes are sealed and
    /// verified, the commit object is sealed (its digest must equal the
    /// record id), then the journal line, then `head` under an
    /// expected-old-value check. A racing commit or discard is refused by the
    /// writer claim instead of interleaving with this sequence.
    pub fn commit_abs(
        &self,
        home: &Path,
        cas: &ContentStore,
        bases: &BTreeMap<String, PathBuf>,
    ) -> Result<CommitOutcome> {
        let writer = self.acquire_writer(home)?;
        // A marker means a previous publication did not finish. Its sealed
        // objects are not referenced by any published commit yet, so publishing
        // another commit on top would leave the two indistinguishable.
        ensure!(
            self.publish_marker_abs(home)?.is_none(),
            "HOME_BRANCH_PUBLISHING: COW branch {}/{} has an unfinished publication at {}; reconcile it before committing",
            self.workspace,
            self.branch,
            home.join(self.rel_publish_marker()).display()
        );
        let parent = self.head_abs(home)?;
        // Diffs come first: a missing base or overlay root must fail before a
        // publication marker claims that something is being published.
        let mut diffs: Vec<(String, CowDiff)> = Vec::new();
        for (key, base) in bases {
            diffs.push((key.clone(), self.diff_abs(home, key, base)?));
        }
        let home_dir = OwnedDir::admit_absolute(home)
            .with_context(|| format!("admit home {} for commit", home.display()))?;
        let marker = PublishMarker {
            format: PUBLISH_FORMAT.into(),
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            parent: parent.clone(),
            record: None,
            started_unix_ms: now_unix_ms(),
            kind: PUBLISH_KIND_COMMIT.into(),
        };
        self.write_publish_marker_abs(home, &marker)?;

        let mut stats = CommitStats::default();
        let staged_log = home.join(self.rel_publish_objects());
        let mut mounts: BTreeMap<String, BTreeMap<String, Option<CommitEntry>>> = BTreeMap::new();
        for (key, diff) in diffs {
            let rel_overlay = self.rel_overlay(&key);
            // The overlay is entered through the admitted home handle: sealing
            // never resolves a guest-influenced pathname from the root.
            let overlay = OwnedDir::admit_relative(&home_dir, &rel_overlay).with_context(|| {
                format!("HOME_COW_INCOMPLETE: enter overlay {}", rel_overlay.display())
            })?;
            let mut entries: BTreeMap<String, Option<CommitEntry>> = BTreeMap::new();
            for rel in diff.added.iter().chain(diff.modified.iter()) {
                let entry = seal_overlay_entry(cas, overlay.dir(), rel, &mut stats)?;
                if let Some(object) = &entry.object {
                    // The staged list is durable before the object can be
                    // considered published, so an interrupted publication can
                    // name exactly which objects it left behind.
                    let mut line = serde_json::to_vec(&StagedObject {
                        digest: object.digest.clone(),
                        bytes: object.bytes,
                    })
                    .expect("staged object serialize");
                    line.push(b'\n');
                    append_durable(&staged_log, &line)?;
                }
                entries.insert(rel.clone(), Some(entry));
            }
            for rel in &diff.removed {
                entries.insert(rel.clone(), None);
            }
            mounts.insert(key, entries);
        }
        let record = CommitRecord {
            format: COMMIT_FORMAT.into(),
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            parent: parent.clone(),
            timestamp_unix_ms: now_unix_ms(),
            mounts,
        };
        let id = record.id();
        if let Some(head) = &parent {
            let previous = self.record_abs(home, head)?;
            if previous.mounts == record.mounts {
                // Unchanged view: no new commit, no journal line, no head move.
                self.clear_publication_abs(home)?;
                drop(writer);
                return Ok(CommitOutcome {
                    record: previous,
                    id: head.clone(),
                    published: false,
                    objects: stats.objects,
                    bytes: stats.bytes,
                });
            }
        }

        // The commit object itself is sealed so the head digest names bytes in
        // this store; verification of a record can then start from the CAS.
        let mut bytes = serde_json::to_vec(&record).expect("commit serialize");
        let sealed = cas
            .seal_object(
                &bytes,
                COMMIT_MEDIA_TYPE,
                COW_STORE_NAMESPACE,
                COMMIT_CODEC_NAME,
                COMMIT_CODEC_MAJOR,
            )
            .map_err(|error| anyhow!("HOME_COW_INCOMPLETE: seal commit {id}: {error}"))?;
        ensure!(
            sealed.object.digest == id,
            "HOME_COW_INCOMPLETE: sealed commit id {} does not match record id {id}",
            sealed.object.digest
        );
        self.write_publish_marker_abs(
            home,
            &PublishMarker { record: Some(id.clone()), ..marker },
        )?;

        bytes.push(b'\n');
        append_durable(&home.join(self.rel_commits_file()), &bytes)
            .with_context(|| "append COW commit journal")?;
        self.publish_head_abs(home, &id, parent.as_ref())?;
        self.clear_publication_abs(home)?;
        drop(writer);
        Ok(CommitOutcome {
            record,
            id,
            published: true,
            objects: stats.objects,
            bytes: stats.bytes,
        })
    }

    fn publish_head_abs(&self, home: &Path, id: &Digest, expected_old: Option<&Digest>) -> Result<()> {
        let current = self.head_abs(home)?;
        ensure!(
            current.as_ref() == expected_old,
            "HOME_COW_CONFLICT: head changed from {:?} to {:?} while publishing {id}; refusing to overwrite a concurrent publication",
            expected_old,
            current
        );
        write_atomic(&home.join(self.rel_head_file()), format!("{id}\n").as_bytes())
            .with_context(|| "publish COW commit head")
    }

    /// In-flight publication marker, if a commit is publishing (or stopped
    /// mid-publication).
    pub fn publish_marker_abs(&self, home: &Path) -> Result<Option<PublishMarker>> {
        let path = home.join(self.rel_publish_marker());
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(anyhow!("read {}: {error}", path.display())),
        };
        let marker: PublishMarker = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("HOME_COW_INCOMPLETE: decode {}: {error}", path.display()))?;
        ensure!(
            marker.format == PUBLISH_FORMAT,
            "HOME_COW_INCOMPLETE: unsupported publication marker format {:?}",
            marker.format
        );
        ensure!(
            marker.workspace == self.workspace && marker.branch == self.branch,
            "HOME_COW_INCOMPLETE: publication marker {} names {}/{} instead of {}/{}",
            path.display(),
            marker.workspace,
            marker.branch,
            self.workspace,
            self.branch
        );
        Ok(Some(marker))
    }

    /// Objects the in-flight (or interrupted) publisher has sealed so far.
    pub fn staged_objects_abs(&self, home: &Path) -> Result<Vec<StagedObject>> {
        let path = home.join(self.rel_publish_objects());
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(anyhow!("read {}: {error}", path.display())),
        };
        let mut out = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let staged: StagedObject = serde_json::from_slice(line).map_err(|error| {
                anyhow!(
                    "HOME_COW_INCOMPLETE: {} line {}: {error}",
                    path.display(),
                    index + 1
                )
            })?;
            out.push(staged);
        }
        Ok(out)
    }

    fn write_publish_marker_abs(&self, home: &Path, marker: &PublishMarker) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(marker).expect("publish marker serialize");
        write_atomic(&home.join(self.rel_publish_marker()), &bytes)
    }

    fn clear_publication_abs(&self, home: &Path) -> Result<()> {
        let marker = home.join(self.rel_publish_marker());
        let objects = home.join(self.rel_publish_objects());
        for path in [&marker, &objects] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow!(
                        "HOME_COW_INCOMPLETE: clear publication record {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        if let Some(parent) = marker.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    }

    /// Resolve a publication that did not finish. This never guesses: either
    /// `head` already names the record the marker was publishing (completed),
    /// or the publisher stopped earlier and the listed objects are unreferenced
    /// (abandoned). The evidence of both outcomes is reported.
    pub fn reconcile_publication_abs(
        &self,
        home: &Path,
        cas: &ContentStore,
    ) -> Result<PublicationReconciliation> {
        let writer = self.acquire_writer(home)?;
        let Some(marker) = self.publish_marker_abs(home)? else {
            return Ok(PublicationReconciliation::Nothing);
        };
        let staged = self.staged_objects_abs(home)?;
        let head = self.head_abs(home)?;
        let kind = marker.kind.clone();
        let outcome = match (&marker.record, &head) {
            (Some(record), Some(head)) if record == head => {
                // The journal line was appended before the head, so a head that
                // names this record is a completed publication. For a rollback
                // or discard the working state is re-applied from the node: the
                // view and the log then agree whatever the interruption caught.
                if kind == PUBLISH_KIND_ROLLBACK || kind == PUBLISH_KIND_DISCARD {
                    let record = self.record_abs(home, record)?;
                    let home_dir = OwnedDir::admit_absolute(home)?;
                    self.apply_view_abs(home, &home_dir, cas, &record)?;
                }
                PublicationReconciliation::Completed {
                    record: record.clone(),
                    kind,
                }
            }
            _ => PublicationReconciliation::Abandoned { staged, kind },
        };
        self.clear_publication_abs(home)?;
        drop(writer);
        Ok(outcome)
    }

    /// Reset the branch working state: overlay files, tombstones and commit
    /// history all drop; the next view is exactly the base. The branch
    /// skeleton (and the workspace itself) stays prepared.
    ///
    /// A branch with a live writer or an unfinished publication is refused:
    /// discarding state another process is still writing is not a reset.
    /// Reset the branch *working state*: overlay files and tombstones are
    /// cleared, so the next view is exactly the base.
    ///
    /// History is append-only (SPEC §51.4): `commits.jsonl` and `head` are
    /// never deleted. The reset itself is published as a new node whose
    /// per-mount change set is empty — "the view equals the base for the
    /// mounts this branch knew about" — so the log stays complete and an
    /// earlier node can still be selected by [`BranchStore::rollback_abs`].
    pub fn discard_abs(
        &self,
        home: &Path,
        cas: &ContentStore,
    ) -> Result<CommitOutcome> {
        let writer = self.acquire_writer(home)?;
        ensure!(
            self.publish_marker_abs(home)?.is_none(),
            "HOME_BRANCH_PUBLISHING: COW branch {}/{} has an unfinished publication; reconcile it before discarding",
            self.workspace,
            self.branch
        );
        let marker = PublishMarker {
            format: PUBLISH_FORMAT.into(),
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            parent: self.head_abs(home)?,
            record: None,
            started_unix_ms: now_unix_ms(),
            kind: PUBLISH_KIND_DISCARD.into(),
        };
        self.write_publish_marker_abs(home, &marker)?;
        // Publish the reset as a new node first: the recorded mounts keep their
        // keys (so the log says which mounts were reset) with no changes. A
        // crash before the working state is cleared is reconciled by
        // re-applying this node's (empty) view.
        let mounts = self
            .commits_abs(home)?
            .iter()
            .flat_map(|record| record.mounts.keys().cloned())
            .collect::<BTreeSet<String>>();
        let outcome = self.append_node_abs(
            home,
            cas,
            mounts
                .into_iter()
                .map(|key| (key, BTreeMap::new()))
                .collect(),
        )?;
        let branch_dir = home.join(&self.rel_branch_dir);
        for rel in [PathBuf::from(OVERLAY_DIR), PathBuf::from(TOMBSTONES_DIR)] {
            let path = branch_dir.join(rel);
            remove_any(&path)
                .with_context(|| format!("reset branch working state {}", path.display()))?;
        }
        fs::create_dir_all(branch_dir.join(OVERLAY_DIR))
            .and_then(|()| fs::create_dir_all(branch_dir.join(TOMBSTONES_DIR)))
            .map_err(|e| anyhow!("recreate branch skeleton: {e}"))?;
        self.clear_publication_abs(home)?;
        drop(writer);
        Ok(outcome)
    }

    /// Roll a branch back to a selected historical node without rewriting the
    /// log (SPEC §51.4, §10.1).
    ///
    /// The working state is materialized from the target node's sealed
    /// objects, and a *new* node whose content equals the target node is
    /// appended at the end of the chain, with the same parent as the target
    /// and the current node as its predecessor. Content-addressed objects are
    /// reused, nothing is truncated, and an external effect that already
    /// happened is not undone: the log still shows it.
    pub fn rollback_abs(
        &self,
        home: &Path,
        cas: &ContentStore,
        target: &Digest,
    ) -> Result<CommitOutcome> {
        let writer = self.acquire_writer(home)?;
        ensure!(
            self.publish_marker_abs(home)?.is_none(),
            "HOME_BRANCH_PUBLISHING: COW branch {}/{} has an unfinished publication; reconcile it before rolling back",
            self.workspace,
            self.branch
        );
        let target_record = self.record_abs(home, target)?;
        target_record.verify_closure(cas)?;

        // Working state must become the target view before the node is
        // published, and the publication marker says a selection is in
        // progress so an interruption is visible rather than silent.
        let marker = PublishMarker {
            format: PUBLISH_FORMAT.into(),
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            parent: self.head_abs(home)?,
            record: None,
            started_unix_ms: now_unix_ms(),
            kind: PUBLISH_KIND_ROLLBACK.into(),
        };
        self.write_publish_marker_abs(home, &marker)?;
        let home_dir = OwnedDir::admit_absolute(home)
            .with_context(|| format!("admit home {} for rollback", home.display()))?;
        self.apply_view_abs(home, &home_dir, cas, &target_record)?;

        let outcome = self.append_node_abs(home, cas, target_record.mounts.clone())?;
        self.clear_publication_abs(home)?;
        drop(writer);
        Ok(outcome)
    }

    /// Rewrite overlay+tombstones so the branch's effective view equals
    /// `record`'s recorded layer. Base files that the node never copied up are
    /// untouched (the base is immutable and outside the branch).
    fn apply_view_abs(
        &self,
        home: &Path,
        home_dir: &OwnedDir,
        cas: &ContentStore,
        record: &CommitRecord,
    ) -> Result<()> {
        for (mount_key, tree) in &record.mounts {
            let rel_overlay = self.rel_overlay(mount_key);
            home_dir
                .dir()
                .create_dir_all(&rel_overlay)
                .with_context(|| format!("HOME_COW_INCOMPLETE: prepare overlay {}", rel_overlay.display()))?;
            let overlay = OwnedDir::admit_relative(home_dir, &rel_overlay)?;
            let mut tombstones = Tombstones::default();

            // Directories first: verification guarantees ancestors are
            // recorded, so one ordered pass creates every level.
            for (rel, entry) in tree {
                if entry
                    .as_ref()
                    .is_some_and(|entry| entry.kind == CommitEntryKind::Directory)
                {
                    closure_directory(overlay.dir(), rel)?;
                }
            }
            for (rel, entry) in tree {
                match entry {
                    Some(entry) => match entry.kind {
                        CommitEntryKind::Directory => {}
                        CommitEntryKind::File => {
                            let object = entry.object.as_ref().ok_or_else(|| {
                                anyhow!("HOME_COW_INCOMPLETE: node file {rel:?} has no sealed object")
                            })?;
                            let path = cas.verified_object_path(object).map_err(|error| {
                                anyhow!(
                                    "HOME_COW_INCOMPLETE: node file {rel:?} object {}: {error}",
                                    object.digest
                                )
                            })?;
                            closure_file(overlay.dir(), rel, &path, entry.bytes, entry.mode)?;
                        }
                        CommitEntryKind::Symlink => {
                            let target = entry.target.as_deref().ok_or_else(|| {
                                anyhow!("HOME_COW_INCOMPLETE: node link {rel:?} has no target")
                            })?;
                            closure_symlink(overlay.dir(), rel, target)?;
                        }
                    },
                    None => {
                        // The node recorded this base path as removed from the
                        // view: the durable tombstone is part of the state.
                        tombstones.remove(Path::new(rel));
                        match overlay.dir().remove_file(rel) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(anyhow!(
                                    "HOME_COW_INCOMPLETE: apply removal {rel:?}: {error}"
                                ));
                            }
                        }
                    }
                }
            }
            // Overlay entries the selected node never recorded must not survive
            // the rollback, or the view would still contain later work.
            let mut extra = Vec::new();
            walk_overlay_relative(overlay.dir(), "", &mut extra)?;
            for rel in extra {
                if !tree.contains_key(&rel) {
                    match overlay.dir().remove_file(&rel) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => {
                            let _ = overlay.dir().remove_dir_all(&rel);
                        }
                    }
                }
            }
            let bytes = tombstones.save();
            let rel_tombstones = self.rel_tombstones(mount_key);
            write_atomic(&home.join(&rel_tombstones), &bytes)
                .with_context(|| format!("HOME_COW_INCOMPLETE: persist {}", rel_tombstones.display()))?;
        }
        Ok(())
    }

    /// Seal and publish one node with `mounts` over the current head.
    ///
    /// The caller holds the writer claim and has already published the
    /// in-flight marker; this only performs the durable append + head move
    /// under the expected-old-value check.
    fn append_node_abs(
        &self,
        home: &Path,
        cas: &ContentStore,
        mounts: BTreeMap<String, BTreeMap<String, Option<CommitEntry>>>,
    ) -> Result<CommitOutcome> {
        let parent = self.head_abs(home)?;
        let record = CommitRecord {
            format: COMMIT_FORMAT.into(),
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            parent: parent.clone(),
            timestamp_unix_ms: now_unix_ms(),
            mounts,
        };
        let id = record.id();
        let mut bytes = serde_json::to_vec(&record).expect("commit serialize");
        let sealed = cas
            .seal_object(
                &bytes,
                COMMIT_MEDIA_TYPE,
                COW_STORE_NAMESPACE,
                COMMIT_CODEC_NAME,
                COMMIT_CODEC_MAJOR,
            )
            .map_err(|error| anyhow!("HOME_COW_INCOMPLETE: seal node {id}: {error}"))?;
        ensure!(
            sealed.object.digest == id,
            "HOME_COW_INCOMPLETE: sealed node id {} does not match record id {id}",
            sealed.object.digest
        );
        let objects: usize = record
            .mounts
            .values()
            .flat_map(|tree| tree.values())
            .flatten()
            .filter(|entry| entry.object.is_some())
            .count();
        let bytes_recorded = record
            .mounts
            .values()
            .flat_map(|tree| tree.values())
            .flatten()
            .map(|entry| entry.bytes)
            .sum();
        bytes.push(b'\n');
        append_durable(&home.join(self.rel_commits_file()), &bytes)
            .with_context(|| "append COW commit journal")?;
        self.publish_head_abs(home, &id, parent.as_ref())?;
        Ok(CommitOutcome {
            record,
            id,
            published: true,
            objects,
            bytes: bytes_recorded,
        })
    }

    /// Prepare a workspace branch on disk (host-side seed/bootstrap path;
    /// runtime admission uses [`BranchStore::open`] instead).
    pub fn ensure_abs(home: &Path, workspace: &str, branch: &str) -> Result<Self> {
        let store = Self {
            workspace: workspace.to_string(),
            branch: branch.to_string(),
            rel_branch_dir: Self::rel_branch_dir(workspace, branch),
        };
        fs::create_dir_all(home.join(store.rel_overlay("_init")))
            .map_err(|e| anyhow!("prepare workspace {workspace}/{branch}: {e}"))?;
        // Remove the placeholder: skeleton dirs only.
        remove_any(&home.join(store.rel_overlay("_init"))).ok();
        fs::create_dir_all(home.join(store.rel_branch_dir.join(OVERLAY_DIR)))
            .and_then(|()| fs::create_dir_all(home.join(store.rel_branch_dir.join(TOMBSTONES_DIR))))
            .map_err(|e| anyhow!("prepare branch skeleton: {e}"))?;
        Ok(store)
    }

    /// True when nothing has diverged from the base for the given mounts.
    pub fn is_clean_abs(&self, home: &Path, bases: &BTreeMap<String, PathBuf>) -> Result<bool> {
        for (key, base) in bases {
            if !self.diff_abs(home, key, base)?.is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Verify the whole published history and every commit's byte closure.
    ///
    /// The chain is the set of records reachable from `head` through `parent`
    /// links; every record in the journal is verified as a closure, and each
    /// journal record outside the chain is reported as a prepared-but-
    /// unpublished attempt instead of being silently treated as history.
    pub fn verify_history_abs(
        &self,
        home: &Path,
        cas: &ContentStore,
    ) -> Result<VerifiedBranchHistory> {
        let head = self.head_abs(home)?;
        let mut verified: BTreeMap<Digest, VerifiedCommit> = BTreeMap::new();
        let mut order: Vec<Digest> = Vec::new();
        for record in self.commits_abs(home)? {
            ensure!(
                record.workspace == self.workspace && record.branch == self.branch,
                "HOME_COW_INCOMPLETE: commit {} belongs to {}/{} instead of {}/{}",
                record.id(),
                record.workspace,
                record.branch,
                self.workspace,
                self.branch
            );
            let summary = record.verify_closure(cas)?;
            order.push(summary.id.clone());
            verified.insert(summary.id.clone(), summary);
        }
        let mut chain: Vec<VerifiedCommit> = Vec::new();
        let mut seen: BTreeSet<Digest> = BTreeSet::new();
        let mut cursor = head.clone();
        while let Some(id) = cursor {
            ensure!(
                seen.insert(id.clone()),
                "HOME_COW_INCOMPLETE: commit chain from head {id} loops"
            );
            let summary = verified.get(&id).ok_or_else(|| {
                anyhow!(
                    "HOME_COW_INCOMPLETE: head chain names commit {id} that is not in {}",
                    home.join(self.rel_commits_file()).display()
                )
            })?;
            cursor = summary.parent.clone();
            chain.push(summary.clone());
        }
        chain.reverse();
        let in_chain: BTreeSet<&Digest> = chain.iter().map(|commit| &commit.id).collect();
        let prepared_unpublished: Vec<Digest> = order
            .iter()
            .filter(|id| !in_chain.contains(id))
            .cloned()
            .collect();
        let objects = chain.iter().map(|commit| commit.objects).sum();
        let bytes = chain.iter().map(|commit| commit.bytes).sum();
        Ok(VerifiedBranchHistory {
            head,
            commits: chain,
            prepared_unpublished,
            objects,
            bytes,
        })
    }

    /// Roots this branch contributes to object reclamation.
    ///
    /// The enumeration holds the writer claim for its own lifetime, so a
    /// publisher cannot start between "no marker" and "roots read". A live
    /// writer or an unfinished publication is reported instead of being
    /// assumed harmless.
    pub fn roots_abs(&self, home: &Path, cas: &ContentStore) -> Result<BranchRoots> {
        let writer = self.acquire_writer(home)?;
        let marker = self.publish_marker_abs(home)?;
        let staged = self.staged_objects_abs(home)?;
        let history = self.verify_history_abs(home, cas)?;
        let mut roots = BranchRoots {
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            head: history.head.clone(),
            commits: BTreeSet::new(),
            objects: BTreeSet::new(),
            prepared_unpublished: history.prepared_unpublished.clone(),
            publishing: marker.is_some(),
            staged,
            lease: Some(writer),
        };
        for commit in &history.commits {
            roots.commits.insert(commit.id.clone());
        }
        // Every journal record is a root as well: an unpublished attempt's
        // objects are still named by the branch's own durable journal and are
        // reconciled by `reconcile_publication_abs`, not by reclamation.
        for record in self.commits_abs(home)? {
            let id = record.id();
            roots.commits.insert(id.clone());
            for tree in record.mounts.values() {
                for entry in tree.values().flatten() {
                    if let Some(object) = &entry.object {
                        roots.objects.insert(object.digest.clone());
                    }
                }
            }
        }
        Ok(roots)
    }
}

/// Roots one branch contributes to object reclamation.
pub struct BranchRoots {
    pub workspace: String,
    pub branch: String,
    pub head: Option<Digest>,
    pub commits: BTreeSet<Digest>,
    pub objects: BTreeSet<Digest>,
    pub prepared_unpublished: Vec<Digest>,
    /// True when a publication marker existed: an in-progress publisher.
    pub publishing: bool,
    pub staged: Vec<StagedObject>,
    /// Held so no writer can start while these roots are used.
    lease: Option<BranchWriter>,
}

impl std::fmt::Debug for BranchRoots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchRoots")
            .field("workspace", &self.workspace)
            .field("branch", &self.branch)
            .field("head", &self.head)
            .field("commits", &self.commits.len())
            .field("objects", &self.objects.len())
            .field("publishing", &self.publishing)
            .field("staged", &self.staged.len())
            .finish_non_exhaustive()
    }
}

impl BranchRoots {
    /// Release the writer claim. Roots stay valid for the caller; only the
    /// exclusion is dropped.
    pub fn release(&mut self) {
        self.lease = None;
    }

    pub fn holds_lease(&self) -> bool {
        self.lease.is_some()
    }

    /// Hand the writer claims to a caller that must keep them for longer than
    /// this value, so no publisher can start while its roots are in use.
    pub fn into_exclusions(mut self) -> Vec<BranchWriter> {
        self.lease.take().into_iter().collect()
    }
}

/// Summary of a verified branch history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedBranchHistory {
    pub head: Option<Digest>,
    /// Commits reachable from `head`, oldest first.
    pub commits: Vec<VerifiedCommit>,
    /// Journal entries not reachable from `head`: complete records whose
    /// publication stopped before `head` moved.
    pub prepared_unpublished: Vec<Digest>,
    pub objects: usize,
    pub bytes: u64,
}

/// What materializing a commit produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedCommit {
    pub id: Digest,
    pub directories: usize,
    pub files: usize,
    pub links: usize,
    pub bytes: u64,
}

/// Recreate one commit's recorded content in `destination`.
///
/// This is the byte-closure proof in practice: a commit that only stored
/// digests could not be written back. The recorded layer is written exactly —
/// file bytes from this installation's CAS, permission bits, and symbolic
/// links with their literal targets — and nothing else is invented. Base
/// files that the branch never copied up are not part of the commit and are
/// therefore not recreated: only the base mount can provide them.
pub fn materialize_commit_abs(
    cas: &ContentStore,
    record: &CommitRecord,
    destination: &Path,
) -> Result<MaterializedCommit> {
    record.verify_closure(cas)?;
    closure_root(destination)?;
    let admitted = OwnedDir::admit_absolute(destination).with_context(|| {
        format!(
            "HOME_COW_INCOMPLETE: admit materialization target {}",
            destination.display()
        )
    })?;
    let root = admitted.dir();
    let mut outcome = MaterializedCommit {
        id: record.id(),
        directories: 0,
        files: 0,
        links: 0,
        bytes: 0,
    };
    // Directories first: verification guarantees every recorded path's
    // ancestors are recorded directories, so one ordered pass is enough.
    for tree in record.mounts.values() {
        for (rel, entry) in tree {
            if entry.as_ref().is_some_and(|entry| entry.kind == CommitEntryKind::Directory) {
                closure_directory(root, rel)?;
                outcome.directories += 1;
            }
        }
    }
    for tree in record.mounts.values() {
        for (rel, entry) in tree {
            let Some(entry) = entry else {
                // A removed base path has no recorded content.
                continue;
            };
            match entry.kind {
                CommitEntryKind::Directory => {}
                CommitEntryKind::File => {
                    let object = entry.object.as_ref().ok_or_else(|| {
                        anyhow!("HOME_COW_INCOMPLETE: commit file {rel:?} has no sealed object")
                    })?;
                    let path = cas.verified_object_path(object).map_err(|error| {
                        anyhow!(
                            "HOME_COW_INCOMPLETE: commit file {rel:?} object {}: {error}",
                            object.digest
                        )
                    })?;
                    closure_file(root, rel, &path, entry.bytes, entry.mode)?;
                    outcome.files += 1;
                    outcome.bytes += entry.bytes;
                }
                CommitEntryKind::Symlink => {
                    let target = entry.target.as_deref().ok_or_else(|| {
                        anyhow!("HOME_COW_INCOMPLETE: commit link {rel:?} has no target")
                    })?;
                    closure_symlink(root, rel, target)?;
                    outcome.links += 1;
                }
            }
        }
    }
    sync_dir(destination)?;
    Ok(outcome)
}

/// Counters for one commit.
#[derive(Debug, Default)]
struct CommitStats {
    objects: usize,
    bytes: u64,
}

/// Seal one overlay entry into the commit form: bytes for a file, exact
/// target bytes for a link, nothing for a directory. Files are opened without
/// following a final symbolic link and their identity is compared before and
/// after sealing, so a file that changes while it is committed is refused
/// instead of being published as a mixed closure.
fn seal_overlay_entry(
    cas: &ContentStore,
    overlay: &cap_std::fs::Dir,
    rel: &str,
    stats: &mut CommitStats,
) -> Result<CommitEntry> {
    let metadata = overlay
        .symlink_metadata(rel)
        .with_context(|| format!("HOME_COW_INCOMPLETE: inspect overlay entry {rel:?}"))?;
    if metadata.is_symlink() {
        let target = overlay
            .read_link(rel)
            .with_context(|| format!("HOME_COW_INCOMPLETE: read overlay link {rel:?}"))?;
        let target = target.to_str().ok_or_else(|| {
            anyhow!("HOME_COW_UNSUPPORTED: non-UTF-8 link target at {rel:?}")
        })?;
        return Ok(CommitEntry {
            kind: CommitEntryKind::Symlink,
            bytes: 0,
            digest: Digest::blake3(target.as_bytes()),
            mode: None,
            target: Some(target.to_string()),
            object: None,
        });
    }
    if metadata.is_dir() {
        return Ok(CommitEntry {
            kind: CommitEntryKind::Directory,
            bytes: 0,
            digest: Digest::blake3(b""),
            mode: entry_mode(&metadata),
            target: None,
            object: None,
        });
    }
    if !metadata.is_file() {
        bail!(
            "HOME_COW_UNSUPPORTED: overlay entry {rel:?} is a special file; a COW commit records regular files, directories and symbolic links only"
        );
    }
    let mode = entry_mode(&metadata);
    let before = file_identity(&metadata);
    let mut file = fsx::open_component_with(
        overlay,
        Path::new(rel),
        &OpenOptions::new().read(true),
    )
    .with_context(|| format!("HOME_COW_INCOMPLETE: open overlay file {rel:?}"))?
    .ok_or_else(|| anyhow!("HOME_COW_INCOMPLETE: overlay file {rel:?} vanished during commit"))?;
    let sealed = cas
        .seal_reader(&mut file, FILE_MEDIA_TYPE, COW_STORE_NAMESPACE, FILE_CODEC_NAME, FILE_CODEC_MAJOR)
        .map_err(|error| anyhow!("HOME_COW_INCOMPLETE: seal {rel:?}: {error}"))?;
    let after = overlay
        .symlink_metadata(rel)
        .with_context(|| format!("HOME_COW_INCOMPLETE: re-inspect overlay file {rel:?}"))?;
    ensure!(
        file_identity(&after) == before,
        "HOME_COW_UNSTABLE: overlay file {rel:?} changed while it was committed; commit a quiescent branch"
    );
    stats.objects += 1;
    stats.bytes += sealed.object.bytes;
    Ok(CommitEntry {
        kind: CommitEntryKind::File,
        bytes: sealed.object.bytes,
        digest: sealed.object.digest.clone(),
        mode,
        target: None,
        object: Some(cow_object_ref(&sealed.object.digest, sealed.object.bytes)),
    })
}

fn entry_mode(metadata: &cap_std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// `(len, mtime_ns, inode)` identity used to detect a file that changed while
/// it was being sealed.
fn file_identity(metadata: &cap_std::fs::Metadata) -> (u64, Option<i128>, u64) {
    use std::time::UNIX_EPOCH;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.into_std().duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128);
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        (metadata.len(), modified, metadata.ino())
    }
    #[cfg(not(unix))]
    {
        (metadata.len(), modified, 0)
    }
}

/// Relative paths of every entry below an admitted overlay directory, files
/// and directories alike, sorted so removals can be attempted leaf-first.
fn walk_overlay_relative(dir: &cap_std::fs::Dir, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let entries = dir
        .entries()
        .with_context(|| format!("list overlay {prefix:?}"))?;
    for entry in entries {
        let entry = entry.context("read overlay entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            bail!("HOME_COW_UNSUPPORTED: non-UTF-8 overlay entry under {prefix:?}");
        };
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let file_type = entry.file_type().context("type of overlay entry")?;
        if file_type.is_dir() {
            let child = dir.open_dir(name).context("open overlay subdirectory")?;
            walk_overlay_relative(&child, &rel, out)?;
            out.push(rel);
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

fn overlay_handle_clone(home: &OwnedDir) -> Result<OwnedDir> {
    OwnedDir::admit_relative(home, Path::new("."))
        .context("clone home handle for COW persistence")
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e2) if e2.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(e),
        },
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| anyhow!("write {}: {e}", tmp.display()))?;
    if let Ok(file) = fs::File::open(&tmp) {
        let _ = file.sync_all();
    }
    fs::rename(&tmp, path).map_err(|e| anyhow!("rename {}: {e}", path.display()))?;
    sync_dir(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

fn append_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| anyhow!("open {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| anyhow!("append {}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| anyhow!("sync {}: {e}", path.display()))?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        bail!("HOME_COW_UNSUPPORTED: durable publication requires directory synchronization");
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Guest-visible per-mount COW state. `overlay` is the admitted writable
/// handle; `tombstones` mirrors the durable JSON set and is persisted
/// (atomic tmp+rename, file fsync) after every mutation.
pub struct CowMountStore {
    home: OwnedDir,
    pub overlay: OwnedDir,
    rel_overlay: PathBuf,
    rel_tombstones: PathBuf,
    pub tombstones: Tombstones,
}

impl std::fmt::Debug for CowMountStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CowMountStore")
            .field("rel_overlay", &self.rel_overlay)
            .field("rel_tombstones", &self.rel_tombstones)
            .field("tombstones", &self.tombstones.removed.len())
            .finish_non_exhaustive()
    }
}

impl CowMountStore {
    pub fn overlay_dir(&self) -> &cap_std::fs::Dir {
        self.overlay.dir()
    }

    /// Persist tombstones after a mutation (best-effort durability: atomic
    /// rename + file fsync; parent-dir fsync is not performed, see
    /// docs/specs/wasm-home.md).
    pub fn persist_tombstones(&mut self) -> Result<()> {
        let bytes = self.tombstones.save();
        let tmp = self.rel_tombstones.with_extension("tmp");
        {
            use std::io::Write;
            let mut file = self
                .home
                .dir()
                .open_with(
                    &tmp,
                    cap_std::fs::OpenOptions::new().write(true).create(true).truncate(true),
                )
                .map_err(|e| anyhow!("open tombstones tmp {}: {e}", tmp.display()))?;
            file.write_all(&bytes)
                .map_err(|e| anyhow!("write tombstones: {e}"))?;
            file.sync_all().map_err(|e| anyhow!("sync tombstones: {e}"))?;
        }
        self.home
            .dir()
            .rename(&tmp, self.home.dir(), &self.rel_tombstones)
            .map_err(|e| anyhow!("persist tombstones: {e}"))?;
        Ok(())
    }
}

/// Where the effective view finds `rel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CowLocation {
    Base,
    Overlay,
    Missing,
}

/// Host-side recursive diff of base vs overlay∖tombstones.
fn diff_trees(base: &Path, overlay: &Path, tombstones: &Tombstones) -> Result<CowDiff> {
    let mut diff = CowDiff::default();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut replaced_directories: BTreeSet<PathBuf> = BTreeSet::new();
    walk_tree(base, Path::new(""), 0, &mut |rel, meta| {
        let key = rel_slash(rel);
        if !visited.insert(key.clone()) {
            return Ok(());
        }
        if tombstones.is_removed(rel)
            || rel.ancestors().skip(1).any(|ancestor| replaced_directories.contains(ancestor))
        {
            diff.removed.insert(key);
        } else {
            let over = overlay.join(rel);
            match fs::symlink_metadata(&over) {
                Ok(om) => {
                    // A file or symlink replaces this directory entry. Its
                    // old base subtree stays hidden even when following the
                    // link makes a later overlay lookup return NotFound.
                    if meta.is_dir() && !om.is_dir() {
                        replaced_directories.insert(rel.to_path_buf());
                    }
                    let same_kind = om.is_dir() == meta.is_dir();
                    let same = same_kind
                        && (meta.is_dir()
                            || (om.len() == meta.len()
                                && file_contents_equal(&over, &base.join(rel))?));
                    if !same {
                        diff.modified.insert(key);
                    }
                }
                // No overlay entry leaves the base visible. A non-directory
                // overlay ancestor instead masks this base descendant.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {
                    diff.removed.insert(key);
                }
                Err(e) => return Err(anyhow!("stat overlay {}: {e}", over.display())),
            }
        }
        Ok(())
    })?;
    walk_tree(overlay, Path::new(""), 0, &mut |rel, _meta| {
        let key = rel_slash(rel);
        if visited.insert(key.clone()) {
            if !tombstones.is_removed(rel) {
                let original = base.join(rel);
                match fs::symlink_metadata(&original) {
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
                        diff.added.insert(key);
                    }
                    Err(e) => return Err(anyhow!("stat base {}: {e}", original.display())),
                }
            }
        }
        Ok(())
    })?;
    // Base paths hidden by tombstones that no longer exist anywhere still
    // count as removed (they were removed from the view).
    for key in tombstones.removed.iter() {
        if diff.removed.contains(key) {
            continue;
        }
        let rel = Path::new(key);
        let original = base.join(rel);
        match fs::symlink_metadata(&original) {
            Ok(_) => { diff.removed.insert(key.clone()); }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {}
            Err(e) => return Err(anyhow!("stat removed base {}: {e}", original.display())),
        }
    }
    Ok(diff)
}

fn file_contents_equal(left: &Path, right: &Path) -> Result<bool> {
    use std::io::Read;

    fn read_chunk(file: &mut fs::File, bytes: &mut [u8]) -> std::io::Result<usize> {
        let mut filled = 0;
        while filled < bytes.len() {
            match file.read(&mut bytes[filled..]) {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }

    let mut left_file = fs::File::open(left)
        .with_context(|| format!("open compared file {}", left.display()))?;
    let mut right_file = fs::File::open(right)
        .with_context(|| format!("open compared file {}", right.display()))?;
    let mut left_bytes = [0u8; 8192];
    let mut right_bytes = [0u8; 8192];
    loop {
        let left_count = read_chunk(&mut left_file, &mut left_bytes)
            .with_context(|| format!("read compared file {}", left.display()))?;
        let right_count = read_chunk(&mut right_file, &mut right_bytes)
            .with_context(|| format!("read compared file {}", right.display()))?;
        if left_count != right_count || left_bytes[..left_count] != right_bytes[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

fn walk_tree(
    root: &Path,
    rel: &Path,
    depth: usize,
    visit: &mut dyn FnMut(&Path, &fs::Metadata) -> Result<()>,
) -> Result<()> {
    if depth > MAX_TREE_DEPTH {
        bail!("COW_TREE_TOO_DEEP: tree exceeds {MAX_TREE_DEPTH} levels");
    }
    let dir = if rel.as_os_str().is_empty() {
        fs::read_dir(root)
    } else {
        fs::read_dir(root.join(rel))
    };
    let entries = match dir {
        Ok(entries) => entries,
        Err(e) => return Err(anyhow!("walk {}: {e}", root.display())),
    };
    let mut count = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| anyhow!("walk {}: {e}", root.display()))?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let child_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel.join(&name)
        };
        count += 1;
        if count > MAX_TREE_ENTRIES {
            bail!("COW_TREE_TOO_LARGE: tree exceeds {MAX_TREE_ENTRIES} entries");
        }
        let meta = entry
            .metadata()
            .map_err(|e| anyhow!("walk {}: {e}", child_rel.display()))?;
        visit(&child_rel, &meta)?;
        if meta.is_dir() {
            walk_tree(root, &child_rel, depth + 1, visit)?;
        }
    }
    Ok(())
}
