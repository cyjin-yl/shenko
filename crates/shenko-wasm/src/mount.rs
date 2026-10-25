use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Explicit host-to-guest mount semantics. COW is represented so manifests do
/// not confuse it with direct RW, but this runtime deliberately rejects it
/// until an actual managed implementation exists (SPEC §8.3/A08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
    CopyOnWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    pub source: PathBuf,
    pub guest_path: String,
    pub mode: MountMode,
    pub persistent: bool,
    pub versioned: bool,
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
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.mode == MountMode::CopyOnWrite {
            bail!(
                "MOUNT_MODE_UNSUPPORTED: copy_on_write {} -> {}; no direct-RW fallback",
                self.source.display(),
                self.guest_path
            );
        }
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
        Ok(())
    }
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

/// Validate the complete preopen list before any command starts. `/home` is a
/// required, unique preopen and its source is the runtime's persistent home.
pub fn validate_mounts(mounts: &[MountSpec], home_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut guests = BTreeSet::new();
    let mut home_count = 0usize;
    let mut canonical_sources = Vec::with_capacity(mounts.len());
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
            anyhow::anyhow!(
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
        canonical_sources.push(canonical);
    }
    if home_count != 1 {
        bail!("WASM_PROFILE_UNAVAILABLE: exactly one /home preopen is required");
    }
    Ok(canonical_sources)
}
