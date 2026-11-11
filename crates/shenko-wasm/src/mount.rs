//! Host-to-guest mount policy and owned-handle admission (docs/specs/wasm-home.md#spec-8-3, A08).
//!
//! Every mount declares its source path, guest path and mode. Admission is
//! the A08 boundary: each mount is opened ONCE into an owned directory
//! capability whose handle is reused for every guest resolution. No guest
//! path operation re-resolves a host pathname after validation, so an
//! ancestor swap between validation and use cannot redirect a preopen.
//!
//! COW mounts (docs/specs/durable-state.md#spec-41-3) are real managed copy-on-write: `source` stays a
//! readonly base on disk, guest writes copy up into a per-branch overlay
//! under the Shenko home workspace store, and removals are recorded as
//! tombstones. COW is never relabelled as direct RW.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fsx;
use crate::cow::{self, BranchStore};

/// Explicit host-to-guest mount semantics (docs/specs/wasm-home.md#spec-8-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
    CopyOnWrite,
}

/// One declared mount. `source` is a host path; for `CopyOnWrite` it is the
/// readonly base. `workspace`/`branch` select the home workspace-store
/// binding for COW mounts and are absent (or ignored) for other modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    pub source: PathBuf,
    pub guest_path: String,
    pub mode: MountMode,
    pub persistent: bool,
    pub versioned: bool,
    /// COW workspace id inside the home workspace store (default "main").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// COW branch id inside the workspace (default "main").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Guest write permission for this mount. `false` withdraws write
    /// capability without changing the declared mode: a managed COW mount
    /// stays a COW mount and still shows the branch's overlay, a direct
    /// mount becomes read-only. Older manifests default to writable.
    #[serde(default = "default_writable")]
    pub writable: bool,
}

/// Default COW binding names.
pub const DEFAULT_WORKSPACE: &str = "main";
pub const DEFAULT_BRANCH: &str = "main";

/// Read-only classification of a declared mount source (docs/specs/wasm-home.md#spec-8-3).
///
/// A missing directory, a directory this process may not enter, a symbolic
/// link and a non-directory entry need different actions and produce
/// different guest outcomes, so they are never collapsed into one "not an
/// accessible directory" message. Classification never follows a final
/// symbolic link: a source that only resolves through a link is reported as
/// such instead of being admitted through the link target.
#[derive(Debug)]
pub enum MountSourceState {
    /// A real directory entry; admission still opens and pins its own handle.
    Ready,
    /// Nothing exists at the path, or at one of its ancestors.
    Missing,
    /// The final component is a symbolic link. `resolves_to` is the canonical
    /// target when the link itself resolves.
    SymbolicLink { resolves_to: Option<PathBuf> },
    /// The path exists but is not a directory (regular file, socket, ...).
    NotADirectory,
    /// The path or one of its ancestors could not be inspected.
    Inaccessible(io::Error),
}

impl MountSourceState {
    /// The reason as an operator-facing phrase; `None` for `Ready`.
    fn reason(&self) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::Missing => Some("does not exist".into()),
            Self::NotADirectory => Some("is not a directory".into()),
            Self::SymbolicLink { resolves_to } => Some(match resolves_to {
                Some(target) => format!("is a symbolic link to {}", target.display()),
                None => "is a dangling symbolic link".into(),
            }),
            Self::Inaccessible(error) => Some(format!("is inaccessible: {error}")),
        }
    }
}

fn default_writable() -> bool {
    true
}

impl MountSpec {
    pub fn new(
        source: PathBuf,
        guest_path: impl Into<String>,
        mode: MountMode,
        persistent: bool,
        versioned: bool,
    ) -> Self {
        Self {
            source,
            guest_path: guest_path.into(),
            mode,
            persistent,
            versioned,
            workspace: None,
            branch: None,
            writable: true,
        }
    }

    /// Declare a managed COW mount over a readonly base.
    pub fn cow(
        source: PathBuf,
        guest_path: impl Into<String>,
        workspace: impl Into<String>,
        branch: impl Into<String>,
        persistent: bool,
        versioned: bool,
    ) -> Self {
        Self {
            source,
            guest_path: guest_path.into(),
            mode: MountMode::CopyOnWrite,
            persistent,
            versioned,
            workspace: Some(workspace.into()),
            branch: Some(branch.into()),
            writable: true,
        }
    }

    /// Effective COW workspace id (defaults to the home default workspace).
    pub fn workspace(&self) -> &str {
        self.workspace.as_deref().unwrap_or(DEFAULT_WORKSPACE)
    }

    /// Effective COW branch id (defaults to the workspace's default branch).
    pub fn branch(&self) -> &str {
        self.branch.as_deref().unwrap_or(DEFAULT_BRANCH)
    }

    /// Withdraw guest write permission from this mount. The declared mode is
    /// preserved, so a managed COW mount keeps its branch view.
    pub fn withdraw_write(&mut self) {
        self.writable = false;
    }

    /// Classify this mount's declared source without creating or mutating it.
    /// This inspects the entry; admission proves access with its own handle.
    /// A missing source is never created or substituted by another directory.
    pub fn source_state(&self) -> MountSourceState {
        let from_error = |error: io::Error| match error.kind() {
            io::ErrorKind::NotFound => MountSourceState::Missing,
            io::ErrorKind::NotADirectory => MountSourceState::NotADirectory,
            _ => MountSourceState::Inaccessible(error),
        };
        match std::fs::symlink_metadata(&self.source) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let resolves_to = std::fs::canonicalize(&self.source).ok();
                MountSourceState::SymbolicLink { resolves_to }
            }
            Ok(metadata) if metadata.file_type().is_dir() => MountSourceState::Ready,
            Ok(_) => MountSourceState::NotADirectory,
            Err(error) => from_error(error),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !self.source.is_absolute() {
            bail!(
                "DATA_PATH_CONFLICT: mount source must be absolute: {}",
                self.source.display()
            );
        }
        validate_guest_path(&self.guest_path)?;
        let state = self.source_state();
        if let Some(reason) = state.reason() {
            bail!(
                "WASM_PROFILE_UNAVAILABLE: declared mount {} source {} {reason}; \
                 a declared mount is never created or silently substituted",
                self.guest_path,
                self.source.display()
            );
        }
        if self.mode == MountMode::CopyOnWrite {
            validate_binding_name(self.workspace(), "workspace")?;
            validate_binding_name(self.branch(), "branch")?;
        }
        Ok(())
    }
}

/// Stable per-mount key under a branch overlay/tombstone set. Hash the exact
/// canonical guest path so separators and literal underscores stay distinct;
/// the fixed-size key also fits a single host directory component.
pub fn mount_key(guest_path: &str) -> String {
    format!("mount-{}", blake3::hash(guest_path.as_bytes()).to_hex())
}

fn validate_binding_name(name: &str, what: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name.contains('\0')
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
    {
        bail!("WASM_PROFILE_UNAVAILABLE: invalid COW {what} name {name:?}");
    }
    Ok(())
}

fn validate_guest_path(guest_path: &str) -> Result<()> {
    if !guest_path.starts_with('/') || guest_path.contains('\0') {
        bail!("WASM_PROFILE_UNAVAILABLE: invalid absolute guest path {guest_path:?}");
    }
    if guest_path.len() > 1 && guest_path.ends_with('/') {
        bail!("WASM_PROFILE_UNAVAILABLE: non-canonical guest path {guest_path:?}");
    }
    if guest_path.len() > 1 {
        for component in guest_path[1..].split('/') {
            if component.is_empty() || component == "." || component == ".." {
                bail!("WASM_PROFILE_UNAVAILABLE: non-canonical guest path {guest_path:?}");
            }
        }
    }
    Ok(())
}

/// Validate the complete mount list before any command starts. `/home` is a
/// required, unique preopen and its source is the runtime's persistent home.
pub fn validate_mounts(mounts: &[MountSpec], home_dir: &Path) -> Result<()> {
    let mut guests = BTreeSet::new();
    let mut home_count = 0usize;
    for mount in mounts {
        mount.validate()?;
        if !guests.insert(mount.guest_path.clone()) {
            bail!(
                "WASM_PROFILE_UNAVAILABLE: duplicate guest mount {}",
                mount.guest_path
            );
        }
        if mount.guest_path == crate::HOME_GUEST_PATH {
            home_count += 1;
            if mount.source != home_dir {
                bail!(
                    "DATA_PATH_CONFLICT: /home source {} differs from configured {}",
                    mount.source.display(),
                    home_dir.display()
                );
            }
        }
        let canonical = mount.source.canonicalize().map_err(|error| {
            anyhow!(
                "WASM_PROFILE_UNAVAILABLE: canonicalize mount {}: {error}",
                mount.source.display()
            )
        })?;
        if canonical != mount.source {
            bail!(
                "WASM_PROFILE_UNAVAILABLE: mount source {} resolves to {}; record the canonical binding explicitly",
                mount.source.display(),
                canonical.display()
            );
        }
    }
    if home_count != 1 {
        bail!("WASM_PROFILE_UNAVAILABLE: exactly one /home preopen is required");
    }
    Ok(())
}

/// An owned, validated directory capability (A08).
///
/// The handle is opened exactly once at admission time; every subsequent
/// guest resolution proceeds from this handle via `openat`-style relative
/// operations, so no host pathname is ever re-resolved after validation. On
/// Unix each admission walk opens every path component with `O_NOFOLLOW`
/// (via cap-std `FollowSymlinks::No`) from the previously pinned descriptor,
/// which makes the walk immune to symlink/ancestor swaps; the resulting
/// descriptor pins the directory inode and its `(dev, ino)` identity is
/// recorded so callers can prove confinement.
pub struct OwnedDir {
    dir: cap_std::fs::Dir,
    dev: u64,
    ino: u64,
}

impl std::fmt::Debug for OwnedDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedDir")
            .field("dev", &self.dev)
            .field("ino", &self.ino)
            .finish_non_exhaustive()
    }
}

impl OwnedDir {
    /// Admit an absolute host path as an owned directory capability.
    pub fn admit_absolute(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            bail!("DATA_PATH_CONFLICT: admission requires an absolute path: {}", path.display());
        }
        #[cfg(unix)]
        {
            let mut dir = cap_std::fs::Dir::open_ambient_dir(
                std::path::Path::new("/"),
                cap_std::ambient_authority(),
            )
            .map_err(|e| anyhow!("admit root: {e}"))?;
            Self::walk(dir, path)
        }
        #[cfg(not(unix))]
        {
            let dir = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
                .map_err(|e| anyhow!("admit {}: {e}", path.display()))?;
            Ok(Self::finish(dir))
        }
    }

    /// Admit a normalized relative path below an already-admitted parent
    /// handle. Confinement is inherited: the new handle can never name
    /// anything outside `parent`.
    pub fn admit_relative(parent: &OwnedDir, rel: &Path) -> Result<Self> {
        let dir = parent.dir.try_clone().map_err(|e| anyhow!("clone parent dir: {e}"))?;
        Self::walk(dir, rel)
    }

    /// Per-component no-follow walk from an open handle. Every component is
    /// pinned by descriptor before the next one is opened; `..`/`.` are
    /// rejected (admission paths must be canonical).
    fn walk(start: cap_std::fs::Dir, path: &Path) -> Result<Self> {
        let mut dir = start;
        for component in path.components() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => continue,
                std::path::Component::Normal(name) => {
                    // The component is opened without following a final
                    // symbolic link and is pinned by its own descriptor
                    // before the next component is resolved, so an ancestor
                    // swap between admission and use cannot redirect the
                    // mount.
                    let file = fsx::open_component(&dir, std::path::Path::new(name))
                        .map_err(|error| admission_failure(path, name, error))?
                        .ok_or_else(|| {
                            anyhow!(
                                "WASM_PROFILE_UNAVAILABLE: admission component {:?} does not exist while admitting {}",
                                name,
                                path.display()
                            )
                        })?;
                    dir = cap_std::fs::Dir::from_std_file(file);
                }
                other => bail!("WASM_PROFILE_UNAVAILABLE: non-canonical admission component: {other:?}"),
            }
        }
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            let metadata = dir
                .try_clone()
                .and_then(|d| d.into_std_file().metadata())
                .map_err(|e| anyhow!("stat admitted dir: {e}"))?;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0u64, 0u64);
        Ok(Self { dir, dev, ino })
    }

    #[cfg(not(unix))]
    fn finish(dir: cap_std::fs::Dir) -> Self {
        Self { dir, dev: 0, ino: 0 }
    }

    /// The pinned directory handle used for all guest resolutions.
    pub fn dir(&self) -> &cap_std::fs::Dir {
        &self.dir
    }

    /// Filesystem identity of the pinned inode; receipts bind this so a
    /// swapped directory is detectable after the fact.
    pub fn identity(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

/// One host-side admission failure, classified by cause (docs/specs/wasm-home.md#spec-8-3).
///
/// A component that does not exist, a component this process may not enter,
/// a symbolic link and an entry swapped between inspection and open need
/// different operator actions, so the classification is preserved together
/// with the original `io::Error` instead of being replaced by a generic
/// string. Everything still carries the stable `WASM_PROFILE_UNAVAILABLE`
/// prefix: mount admission is never a silent success.
fn admission_failure(path: &Path, component: &std::ffi::OsStr, error: io::Error) -> anyhow::Error {
    let message = error.to_string();
    let detail = if message == fsx::ELOOP {
        "is a symbolic link and admission never follows a link".to_string()
    } else if message == fsx::EAGAIN {
        "changed between inspection and open".to_string()
    } else if message == "ENOTDIR" || error.kind() == io::ErrorKind::NotADirectory {
        "is not a directory".to_string()
    } else {
        match error.kind() {
            io::ErrorKind::NotFound => "does not exist".to_string(),
            io::ErrorKind::PermissionDenied => {
                format!("is inaccessible to this process: {error}")
            }
            _ => format!("could not be admitted: {error}"),
        }
    };
    anyhow!(
        "WASM_PROFILE_UNAVAILABLE: admission component {:?} of {}: {detail}",
        component,
        path.display()
    )
}

/// Backing capability for one admitted mount.
pub enum MountBacking {
    /// Read-only or read-write direct mount over an admitted handle.
    Direct { root: OwnedDir, writable: bool },
    /// Managed copy-on-write: readonly `base` plus a per-branch writable
    /// overlay under the home workspace store. `store` owns the durable
    /// tombstone set for this branch/mount.
    CopyOnWrite {
        base: OwnedDir,
        store: cow::CowMountStore,
        /// Whether the guest may write through this mount. A read-only COW
        /// mount still shows the branch's overlay; it simply refuses
        /// writes.
        writable: bool,
    },
}

impl MountBacking {
    pub fn writable(&self) -> bool {
        match self {
            Self::Direct { writable, .. } => *writable,
            Self::CopyOnWrite { writable, .. } => *writable,
        }
    }
}

/// One admitted mount: the declared spec plus the opened capability.
pub struct MountBinding {
    pub spec: MountSpec,
    pub backing: MountBacking,
    /// Branch store for COW mounts (`store.open_overlay(...)` materializes
    /// the per-mount overlay); `None` for direct mounts.
    pub branch: Option<Arc<BranchStore>>,
    /// Exclusive writer claim held for the whole run for COW mounts. While it
    /// is alive no other process (and no commit or discard in this one) may
    /// publish or reset this branch. `None` for direct mounts, and for a
    /// second mount that binds the same branch as an earlier one — the claim
    /// is per branch, not per mount.
    pub writer: Option<cow::BranchWriter>,
}

/// Per-run mount admission (A08). Opens one owned handle per mount — for COW
/// mounts the readonly base plus the branch overlay under the admitted home
/// handle — and returns the bindings in declaration order. Guest fds
/// `3..3+len` correspond to these bindings, matching the preopen contract.
pub fn admit_mounts(mounts: &[MountSpec], home_dir: &Path) -> Result<Vec<MountBinding>> {
    validate_mounts(mounts, home_dir)?;
    let home = OwnedDir::admit_absolute(home_dir)
        .with_context(|| format!("admit home {}", home_dir.display()))?;
    let mut out = Vec::with_capacity(mounts.len());
    // One claim per branch, shared by every mount bound to it: a second
    // exclusive lock on the same directory would conflict with the first, and
    // one run owning the branch twice is still one writer.
    let mut claims: BTreeMap<(String, String), cow::BranchWriter> = BTreeMap::new();
    for spec in mounts {
        // The declared source is admitted before any managed workspace state
        // exists: a mount that cannot be entered must not leave a branch
        // overlay, tombstone set or workspace directory behind in the home.
        let root = OwnedDir::admit_absolute(&spec.source).with_context(|| {
            format!(
                "admit declared mount {} ({})",
                spec.guest_path,
                spec.source.display()
            )
        })?;
        let (backing, branch, writer) = match spec.mode {
            MountMode::ReadOnly => (
                MountBacking::Direct {
                    root,
                    writable: false,
                },
                None,
                None,
            ),
            MountMode::ReadWrite => (
                MountBacking::Direct {
                    root,
                    writable: spec.writable,
                },
                None,
                None,
            ),
            MountMode::CopyOnWrite => {
                let branch = Arc::new(
                    cow::BranchStore::open(&home, spec.workspace(), spec.branch())
                        .with_context(|| {
                            format!(
                                "open COW workspace {}/{} for {}",
                                spec.workspace(),
                                spec.branch(),
                                spec.guest_path
                            )
                        })?,
                );
                // The run owns the branch while it executes: a live writer is
                // exactly what a concurrent commit or discard must refuse.
                let key = (spec.workspace().to_string(), spec.branch().to_string());
                let writer = match claims.get(&key) {
                    Some(writer) => writer.clone(),
                    None => {
                        let writer = branch
                            .acquire_writer(home_dir)
                            .with_context(|| {
                                format!(
                                    "own COW branch {}/{} for this run",
                                    spec.workspace(),
                                    spec.branch()
                                )
                            })?;
                        claims.insert(key, writer.clone());
                        writer
                    }
                };
                // The branch overlay is materialized through the admitted
                // home handle: workspace state cannot escape the home.
                let store = branch.mount(&home, &mount_key(&spec.guest_path))?;
                (
                    MountBacking::CopyOnWrite {
                        base: root,
                        store,
                        writable: spec.writable,
                    },
                    Some(branch),
                    Some(writer),
                )
            }
        };
        out.push(MountBinding {
            spec: spec.clone(),
            backing,
            branch,
            writer,
        });
    }
    Ok(out)
}
