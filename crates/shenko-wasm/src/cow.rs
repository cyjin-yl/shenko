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
//! ```
//!
//! The effective guest view is `overlay ∪ base − tombstones` with the
//! overlay winning on conflicts. Commits are content-addressed manifests
//! over that effective view, so `wasi.exec --branch` can prove exactly what
//! the guest changed without ever writing to the base.
//!
//! Two admission surfaces exist on purpose: guest-facing operations go
//! through [`crate::mount::OwnedDir`] handles (A08 confinement — no pathname
//! is ever re-resolved for the guest), while host-side lifecycle operations
//! (`commit`, `diff`, `discard`, seed preparation) run on absolute paths in
//! trusted host code after the guest has finished.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use shenko_types::object::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::mount::OwnedDir;

pub const WORKSPACES_DIR: &str = "workspaces";
pub const BRANCHES_DIR: &str = "branches";
pub const OVERLAY_DIR: &str = "overlay";
pub const TOMBSTONES_DIR: &str = "tombstones";
pub const COMMITS_FILE: &str = "commits.jsonl";
pub const HEAD_FILE: &str = "head";
pub const COMMIT_FORMAT: &str = "shenko-cow-commit/1";

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
}

/// One entry of a commit manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitEntry {
    pub kind: CommitEntryKind,
    pub bytes: u64,
    pub digest: Digest,
}

/// Entry kinds retain the existing manifest strings without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitEntryKind {
    #[serde(rename = "dir")]
    Directory,
    #[serde(rename = "file")]
    File,
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
    /// The manifest is content-addressed; committing an unchanged view
    /// yields the same id as the parent (idempotent no-op change set).
    pub fn commit_abs(
        &self,
        home: &Path,
        bases: &BTreeMap<String, PathBuf>,
    ) -> Result<CommitRecord> {
        let parent = self.head_abs(home)?;
        let mut mounts: BTreeMap<String, BTreeMap<String, Option<CommitEntry>>> = BTreeMap::new();
        for (key, base) in bases {
            let diff = self.diff_abs(home, key, base)?;
            let overlay = home.join(self.rel_overlay(key));
            let mut entries: BTreeMap<String, Option<CommitEntry>> = BTreeMap::new();
            for rel in diff.added.iter().chain(diff.modified.iter()) {
                let path = overlay.join(rel);
                let meta = fs::metadata(&path)
                    .map_err(|e| anyhow!("stat committed {}: {e}", path.display()))?;
                if meta.is_dir() {
                    entries.insert(rel.clone(), Some(CommitEntry {
                        kind: CommitEntryKind::Directory,
                        bytes: 0,
                        digest: Digest::blake3(&[]),
                    }));
                } else {
                    let bytes = fs::read(&path)
                        .map_err(|e| anyhow!("read committed {}: {e}", path.display()))?;
                    entries.insert(rel.clone(), Some(CommitEntry {
                        kind: CommitEntryKind::File,
                        bytes: bytes.len() as u64,
                        digest: Digest::blake3(&bytes),
                    }));
                }
            }
            for rel in &diff.removed {
                entries.insert(rel.clone(), None);
            }
            mounts.insert(key.clone(), entries);
        }
        let record = CommitRecord {
            format: COMMIT_FORMAT.into(),
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            parent,
            timestamp_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            mounts,
        };
        let id = record.id();
        let mut line = serde_json::to_vec(&record).expect("commit serialize");
        line.push(b'\n');
        let commits_path = home.join(self.rel_commits_file());
        append_durable(&commits_path, &line)?;
        write_atomic(&home.join(self.rel_head_file()), id.to_string().as_bytes())?;
        Ok(record)
    }

    /// Reset the branch working state: overlay files, tombstones and commit
    /// history all drop; the next view is exactly the base. The branch
    /// skeleton (and the workspace itself) stays prepared.
    pub fn discard_abs(&self, home: &Path) -> Result<()> {
        let branch_dir = home.join(&self.rel_branch_dir);
        for rel in [
            PathBuf::from(OVERLAY_DIR),
            PathBuf::from(TOMBSTONES_DIR),
            PathBuf::from(COMMITS_FILE),
            PathBuf::from(HEAD_FILE),
        ] {
            let path = branch_dir.join(rel);
            remove_any(&path)
                .with_context(|| format!("discard {}", path.display()))?;
        }
        fs::create_dir_all(branch_dir.join(OVERLAY_DIR))
            .and_then(|()| fs::create_dir_all(branch_dir.join(TOMBSTONES_DIR)))
            .map_err(|e| anyhow!("recreate branch skeleton: {e}"))?;
        Ok(())
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
    Ok(())
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
