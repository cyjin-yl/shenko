//! Filesystem materialization, artifact verification and owned-directory retirement.
//!
//! A partial write stays unmaterialized; retirement keeps paths whose ownership
//! the recorded canonical package directory does not establish.

use anyhow::{Context, Result, ensure};
use crate::Node;
use rusqlite::{params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use std::path::{Path, PathBuf};
use super::codec::{MAX_ARTIFACT_BYTES, UNIT_SCHEMA, UnitDescriptor, external_artifact_of};
use super::installation::{InstalledUnit, now};

/// A path the uninstall could not retire, with the reason it was left.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetainedPath {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub(super) struct Materialization {
    pub(super) root: String,
    pub(super) path: String,
    pub(super) state: String,
    pub(super) detail: String,
}

/// The materialization record of a unit version.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MaterializationFact {
    pub root: String,
    pub path: String,
    pub state: String,
    pub detail: String,
}

fn safe_component(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && name.len() <= 128,
        "invalid name component"
    );
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized == name && !sanitized.starts_with('.') {
        Ok(sanitized)
    } else {
        let hex = Digest::blake3(name.as_bytes()).hex().to_string();
        Ok(format!("{sanitized}.{}", &hex[..8]))
    }
}

/// Retire the canonical package directory of exactly one version.
///
/// Only `<root>/units/<unit>/<revision>/<digest>` may be removed, and only
/// when the recorded materialization describes that directory. An external
/// artifact, a record whose path points somewhere else, a symlink or a
/// replaced directory is reported and left in place: an uninstall never
/// deletes a path it cannot prove it owns.
pub(super) fn retire_package_directory(
    materialization: &MaterializationFact,
    unit_id: &str,
    revision: &str,
    digest: &Digest,
    files_retired: &mut Vec<String>,
    files_retained: &mut Vec<RetainedPath>,
) -> Result<()> {
    if materialization.state == "external" {
        files_retained.push(RetainedPath {
            path: if materialization.detail.is_empty() {
                materialization.root.clone()
            } else {
                materialization.detail.clone()
            },
            reason: "the artifact bytes live outside this installation".into(),
        });
        return Ok(());
    }
    let canonical = Path::new(&materialization.root)
        .join("units")
        .join(safe_component(unit_id)?)
        .join(safe_component(revision)?)
        .join(safe_component(&digest.0)?);
    let canonical_text = canonical.to_string_lossy().into_owned();
    // A completed materialization names its exact artifact file. A failed one
    // may have created the directory before writing, so it records no path;
    // both are removable only under the canonical directory computed above.
    let owned = if materialization.state == "materialized" {
        !materialization.path.is_empty()
            && Path::new(&materialization.path).parent() == Some(canonical.as_path())
    } else {
        materialization.path.is_empty()
    };
    if !owned {
        files_retained.push(RetainedPath {
            path: materialization.path.clone(),
            reason: format!(
                "the recorded package path does not belong to the canonical directory {canonical_text} of this version"
            ),
        });
        return Ok(());
    }
    match std::fs::symlink_metadata(&canonical) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            files_retained.push(RetainedPath {
                path: canonical_text,
                reason: format!("the package directory could not be inspected: {error}"),
            });
            Ok(())
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            match std::fs::remove_dir_all(&canonical) {
                Ok(()) => {
                    files_retired.push(canonical_text);
                    // Prune the now-empty version ancestors, never the
                    // caller-chosen root: a later install must not find a
                    // stale `units/` tree it cannot admit as its own.
                    let root = Path::new(&materialization.root);
                    let mut ancestor = canonical.parent();
                    while let Some(current) = ancestor {
                        if current == root || !current.starts_with(root) {
                            break;
                        }
                        if std::fs::remove_dir(current).is_err() {
                            break;
                        }
                        ancestor = current.parent();
                    }
                    Ok(())
                }
                Err(error) => {
                    files_retained.push(RetainedPath {
                        path: canonical_text,
                        reason: format!("retiring the package directory failed: {error}"),
                    });
                    Ok(())
                }
            }
        }
        Ok(_) => {
            files_retained.push(RetainedPath {
                path: canonical_text,
                reason: "the canonical package path is not a plain directory".into(),
            });
            Ok(())
        }
    }
}

/// The package root is chosen by the caller and stays private to it.
pub(super) fn normalize_root(root: &str) -> Result<String> {
    ensure!(!root.trim().is_empty(), "a package root is required");
    let path = PathBuf::from(root);
    if path.exists() {
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "the package root must be a real directory, not a symlink"
        );
    } else {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)?;
    }
    let path = std::fs::canonicalize(path)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Write the actual package bytes and a readable manifest into the root.
fn write_package(directory: &Path, bytes: &[u8], descriptor: &UnitDescriptor) -> Result<String> {
    std::fs::create_dir_all(directory)?;
    let artifact = directory.join("package.bin");
    let temporary = directory.join("package.bin.partial");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, &artifact)?;
    let manifest = serde_json::to_vec_pretty(&json!({
        "schema": UNIT_SCHEMA,
        "unit_id": descriptor.unit_id,
        "kind": descriptor.kind,
        "revision": descriptor.revision,
        "artifact_digest": descriptor.artifact_digest,
        "provides": descriptor.provides,
        "dependencies": descriptor.dependencies,
        "config": descriptor.config,
        "applicability": descriptor.applicability,
    }))?;
    let manifest_path = directory.join("manifest.json");
    let manifest_temporary = directory.join("manifest.json.partial");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&manifest_temporary)?;
        file.write_all(&manifest)?;
        file.sync_all()?;
    }
    std::fs::rename(&manifest_temporary, &manifest_path)?;
    Ok(artifact.to_string_lossy().into_owned())
}

/// Whether the materialized file still is the installed artifact.
pub(super) fn verify_artifact(path: &str, expected: &Digest) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() as usize <= MAX_ARTIFACT_BYTES => {}
        _ => return false,
    }
    match std::fs::read(path) {
        Ok(bytes) => Digest::blake3(&bytes) == *expected,
        Err(_) => false,
    }
}

pub(super) fn materialization_matches(unit: &InstalledUnit) -> bool {
    unit.materialization
        .as_ref()
        .is_some_and(|materialization| {
            materialization.state == "materialized"
                && !materialization.path.is_empty()
                && verify_artifact(&materialization.path, &unit.artifact_digest)
        })
}

impl Node {
    pub(super) fn unit_materialize(&mut self, unit_id: &str, package_root: &str) -> Result<Value> {
        let root = normalize_root(package_root)?;
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let status = self.materialize_unit(unit_id, &root)?;
        Ok(json!({
            "unit_id": unit.unit_id,
            "revision": unit.revision,
            "materialized": status.get("materialized").cloned().unwrap_or(json!(false)),
            "package_path": status.get("package_path").cloned().unwrap_or(Value::Null),
            "detail": status.get("detail").cloned().unwrap_or(json!("")),
        }))
    }

    /// Write the actual package bytes into the caller-scoped package root.
    fn materialize_unit(&mut self, unit_id: &str, root: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        self.materialize_version(&unit.unit_id, &unit.revision, &unit.artifact_digest, root)
    }

    /// Retained versions may share an id and revision; never overwrite their
    /// bytes while another version remains selected in a dependency closure.
    pub(super) fn materialize_version(&mut self, unit_id: &str, revision: &str, artifact_digest: &Digest, root: &str) -> Result<Value> {
        let unit = self
            .installed_unit_version(unit_id, revision, artifact_digest)?
            .context("unit version is not retained on this node")?;
        if let Some(external) = external_artifact_of(&unit.descriptor) {
            // The bytes live outside this installation's package store. The
            // node records where they are; it does not claim a local copy
            // and it does not call the unit ready because of it.
            let detail = format!("{}:{}", external.store, external.locator);
            let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
            tx.execute(
                "INSERT INTO unit_materializations(unit_id,revision,artifact_digest,root,path,state,detail,updated_at) VALUES(?1,?2,?3,?4,'',?5,?6,?7) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET root=excluded.root,path='',state=excluded.state,detail=excluded.detail,updated_at=excluded.updated_at",
                params![unit.unit_id, unit.revision, unit.artifact_digest.0, root, "external", detail, now()],
            )?;
            tx.commit()?;
            return Ok(json!({
                "unit_id": unit.unit_id,
                "revision": unit.revision,
                "artifact_digest": unit.artifact_digest,
                "materialized": false,
                "state": "external",
                "package_path": Value::Null,
                "detail": format!("the artifact bytes live outside this installation: {detail}"),
            }));
        }
        let bytes = self.object_bytes(&unit.artifact_digest)?;
        ensure!(
            Digest::blake3(&bytes) == unit.artifact_digest,
            "stored artifact corruption"
        );
        let directory = Path::new(root)
            .join("units")
            .join(safe_component(&unit.unit_id)?)
            .join(safe_component(&unit.revision)?)
            .join(safe_component(&unit.artifact_digest.0)?);
        let outcome = write_package(&directory, &bytes, &unit.descriptor);
        let (state, path, detail) = match &outcome {
            Ok(path) => ("materialized", path.clone(), String::new()),
            Err(e) => ("failed", String::new(), format!("{e:#}")),
        };
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        tx.execute(
            "INSERT INTO unit_materializations(unit_id,revision,artifact_digest,root,path,state,detail,updated_at) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8) \
             ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET root=excluded.root,path=excluded.path,\
             state=excluded.state,detail=excluded.detail,updated_at=excluded.updated_at",
            params![
                unit.unit_id,
                unit.revision,
                unit.artifact_digest.0,
                root,
                path,
                state,
                detail,
                now()
            ],
        )?;
        tx.commit()?;
        Ok(json!({
            "unit_id": unit.unit_id,
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "materialized": state == "materialized",
            "state": state,
            "package_path": if path.is_empty() { Value::Null } else { json!(path) },
            "detail": detail,
        }))
    }

    /// Every distinct materialization root this installation ever wrote units
    /// into. Bootstrap uses this to admit (or refuse to adopt) an existing
    /// tool/package directory before any new installation state is written.
    pub fn materialization_roots(&self) -> Result<Vec<String>> {
        let mut roots: Vec<String> = self
            .conn
            .prepare(
                "SELECT DISTINCT root FROM unit_materializations WHERE root<>'' ORDER BY root",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        roots.dedup();
        Ok(roots)
    }
}
