//! Host-to-guest mount policy and owned-handle admission (SPEC §8.3, A08).
//!
//! Every mount declares its source path, guest path and mode. Admission is
//! the A08 boundary: each mount is opened ONCE into an owned directory
//! capability whose handle is reused for every guest resolution. No guest
//! path operation re-resolves a host pathname after validation, so an
//! ancestor swap between validation and use cannot redirect a preopen.
//!
//! COW mounts (SPEC §41.3) are real managed copy-on-write: `source` stays a
//! readonly base on disk, guest writes copy up into a per-branch overlay
//! under the Shenko home workspace store, and removals are recorded as
//! tombstones. COW is never relabelled as direct RW.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fsx;
use crate::cow::{self, BranchStore};

/// Explicit host-to-guest mount semantics (SPEC §8.3).
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

    pub fn validate(&self) -> Result<()> {
        if !self.source.is_absolute() {
            bail!(
                "DATA_PATH_CONFLICT: mount source must be absolute: {}",
                self.source.display()
            );
        }
        validate_guest_path(&self.guest_path)?;
        if !self.source.is_dir() {
            bail!(
                "WASM_PROFILE_UNAVAILABLE: mount source is not an accessible directory: {}",
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
                        .map_err(|e| {
                            anyhow!("admit component {:?}: {e}", name)
                        })?
                        .ok_or_else(|| {
                            anyhow!(
                                "WASM_PROFILE_UNAVAILABLE: admission component {:?} does not exist: {path:?}",
                                name
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
    for spec in mounts {
        let branch = match spec.mode {
            MountMode::CopyOnWrite => Some(Arc::new(
                cow::BranchStore::open(&home, spec.workspace(), spec.branch())
                    .with_context(|| {
                        format!(
                            "open COW workspace {}/{} for {}",
                            spec.workspace(),
                            spec.branch(),
                            spec.guest_path
                        )
                    })?,
            )),
            _ => None,
        };
        let backing = match spec.mode {
            MountMode::ReadOnly => MountBacking::Direct {
                root: OwnedDir::admit_absolute(&spec.source)
                    .with_context(|| format!("admit ro mount {}", spec.source.display()))?,
                writable: false,
            },
            MountMode::ReadWrite => MountBacking::Direct {
                root: OwnedDir::admit_absolute(&spec.source)
                    .with_context(|| format!("admit rw mount {}", spec.source.display()))?,
                writable: spec.writable,
            },
            MountMode::CopyOnWrite => {
                // The branch overlay is materialized through the admitted
                // home handle: workspace state cannot escape the home.
                let store = branch
                    .as_ref()
                    .expect("branch store opened above for COW")
                    .mount(&home, &mount_key(&spec.guest_path))?;
                let base = OwnedDir::admit_absolute(&spec.source)
                    .with_context(|| format!("admit COW base {}", spec.source.display()))?;
                MountBacking::CopyOnWrite {
                    base,
                    store,
                    writable: spec.writable,
                }
            }
        };
        out.push(MountBinding {
            spec: spec.clone(),
            backing,
            branch,
        });
    }
    Ok(out)
}
