//! File and Git recovery with delivered-content baselines and retirement facts.
//!
//! Import keeps its ledger transaction across filesystem publication. Retirement
//! roots are registered before it begins, so rollback cannot hide preserved bytes.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{OptionalExtension, params};

use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

use crate::cognition_archive::{
    CognitionFileDescriptor, CognitionFileOrigin, CognitionGap, CognitionGitDescriptor,
    CognitionManifest,
};
use crate::store::Store;

use super::{CognitionConflict, CognitionImportOptions, db, storage_failure};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileBaselineState {
    Present,
    LocalDeleted,
    Deleted,
}

impl FileBaselineState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::LocalDeleted => "local_deleted",
            Self::Deleted => "deleted",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "present" => Ok(Self::Present),
            "local_deleted" => Ok(Self::LocalDeleted),
            "deleted" => Ok(Self::Deleted),
            _ => bail!("invalid cognition file baseline state {value}"),
        }
    }
}

struct FileBaseline {
    path: String,
    digest: Digest,
    state: FileBaselineState,
}

struct GitBaseline {
    head: String,
    // Older local baselines did not record the symbolic ref. Their commit is
    // still useful; the next matching import binds the exact source ref.
    reference: Option<String>,
}

impl Store {
    pub(super) fn prepare_file_retirements(
        &self,
        manifest: &CognitionManifest,
        peer_id: EntityId,
        options: &CognitionImportOptions,
        permit: Option<&shenko_types::installation::InstallationPermit>,
    ) -> Result<Vec<(CognitionFileOrigin, PathBuf)>> {
        let mut retirements = Vec::new();
        for origin in &manifest.file_origins {
            let incoming: HashSet<&str> = manifest.files.iter()
                .filter(|file| file.origin == *origin).map(|file| file.path.as_str()).collect();
            let baselines = {
                let conn = self.ledger.conn.lock();
                self.read_file_baselines(&conn, peer_id, file_origin_name(*origin))?
            };
            if !baselines.iter().any(|baseline| baseline.state == FileBaselineState::Present
                && !incoming.contains(baseline.path.as_str())) {
                continue;
            }
            let root = match origin {
                CognitionFileOrigin::Memory => options.memory_root.as_ref(),
                CognitionFileOrigin::Workspace => options.workspace_root.as_ref(),
            }.context("file retirement has no destination root")?;
            let retirement = fs::canonicalize(root)?.join(format!(".shenko-retired-{}", EntityId::new()));
            fs::create_dir(&retirement)?;
            self.ledger.with_maintenance_mutation_under(permit, || {
                let conn = self.ledger.conn.lock();
                conn.execute("INSERT INTO cognition_retirements(path) VALUES(?1)",
                    [retirement.to_string_lossy().as_ref()])?;
                Ok(())
            }, storage_failure)?;
            retirements.push((*origin, retirement));
        }
        Ok(retirements)
    }

    /// Put the memory and workspace files back in place.
    ///
    /// A file that this store still holds exactly as the peer last delivered
    /// it takes the newer content, so a continuing source advances. A file
    /// that this store changed on its own keeps the local content and is
    /// reported as a conflict.
    pub(super) fn restore_files(
        &self,
        conn: &rusqlite::Connection,
        files: &[(CognitionFileDescriptor, Vec<u8>)],
        memory_root: &Option<PathBuf>,
        workspace_root: &Option<PathBuf>,
        origins: &[CognitionFileOrigin],
        peer_id: EntityId,
        retirements: &[(CognitionFileOrigin, PathBuf)],
    ) -> Result<(usize, Vec<CognitionConflict>)> {
        let mut verified = 0usize;
        let mut conflicts: Vec<CognitionConflict> = Vec::new();
        for file_origin in origins {
            let origin = file_origin_name(*file_origin);
            let root = match file_origin {
                CognitionFileOrigin::Memory => memory_root.clone(),
                CognitionFileOrigin::Workspace => workspace_root.clone(),
            }
            .ok_or_else(|| anyhow!("no target root configured for {origin} files"))?;
            let incoming_paths: BTreeSet<&str> = files
                .iter()
                .filter(|(descriptor, _)| descriptor.origin == *file_origin)
                .map(|(descriptor, _)| descriptor.path.as_str())
                .collect();
            let baselines = self.read_file_baselines(conn, peer_id, origin)?;
            for (descriptor, bytes) in files
                .iter()
                .filter(|(descriptor, _)| descriptor.origin == *file_origin)
            {
                let relative = safe_relative_path(&descriptor.path)?;
                let target = root.join(&relative);
                let baseline = self.read_file_baseline(conn, peer_id, origin, &descriptor.path)?;
                if target.exists() {
                    if !fs::symlink_metadata(&target)?.file_type().is_file() {
                        conflicts.push(CognitionConflict {
                            table: "file".into(),
                            key: descriptor.path.clone(),
                            detail: "the local path is no longer a regular file; its independent replacement is kept".into(),
                        });
                        continue;
                    }
                    let existing = fs::read(&target)
                        .with_context(|| format!("read local file {}", target.display()))?;
                    if existing == *bytes {
                        self.write_file_baseline(conn,
                            peer_id,
                            origin,
                            &descriptor.path,
                            &descriptor.digest,
                            FileBaselineState::Present,
                        )?;
                        verified += 1;
                        continue;
                    }
                    let local_digest = Digest::blake3(&existing);
                    if baseline.as_ref().is_some_and(|baseline| {
                        baseline.state == FileBaselineState::Present
                            && baseline.digest == local_digest
                    }) {
                        // Only an untouched peer baseline may advance. A local
                        // recreation after a deletion is independent content.
                        fs::write(&target, bytes)
                            .with_context(|| format!("write file {}", target.display()))?;
                        verify_restored_file(&target, descriptor)?;
                        self.write_file_baseline(conn,
                            peer_id,
                            origin,
                            &descriptor.path,
                            &descriptor.digest,
                            FileBaselineState::Present,
                        )?;
                        verified += 1;
                        continue;
                    }
                    conflicts.push(CognitionConflict {
                        table: "file".into(),
                        key: descriptor.path.clone(),
                        detail: "local file differs; the local content is kept".into(),
                    });
                    continue;
                }
                if let Some(baseline) = baseline {
                    if baseline.state != FileBaselineState::Deleted {
                        self.write_file_baseline(conn,
                            peer_id,
                            origin,
                            &descriptor.path,
                            &baseline.digest,
                            FileBaselineState::LocalDeleted,
                        )?;
                        conflicts.push(CognitionConflict {
                            table: "file".into(),
                            key: descriptor.path.clone(),
                            detail: "the peer file exists but was independently deleted here; the local deletion is kept"
                                .into(),
                        });
                        continue;
                    }
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create directory {}", parent.display()))?;
                }
                fs::write(&target, bytes)
                    .with_context(|| format!("write file {}", target.display()))?;
                verify_restored_file(&target, descriptor)?;
                self.write_file_baseline(conn,
                    peer_id,
                    origin,
                    &descriptor.path,
                    &descriptor.digest,
                    FileBaselineState::Present,
                )?;
                verified += 1;
            }

            // An origin declared by the manifest is a complete current file
            // set. A baseline path absent from that set is a source deletion,
            // not a path that happened to be omitted from the export options.
            for baseline in baselines {
                if incoming_paths.contains(baseline.path.as_str()) {
                    continue;
                }
                let relative = safe_relative_path(&baseline.path)?;
                let target = root.join(&relative);
                if baseline.state == FileBaselineState::Deleted {
                    if target.exists() {
                        conflicts.push(CognitionConflict {
                            table: "file".into(),
                            key: baseline.path,
                            detail: "a local file was recreated where the peer deletion was already applied; the local content is kept"
                                .into(),
                        });
                    }
                    continue;
                }
                if !target.exists() {
                    // Both sides now independently agree on absence. Retain a
                    // tombstone so a later source recreation is distinguishable
                    // from an unchanged source file deleted only by the receiver.
                    self.write_file_baseline(conn,
                        peer_id,
                        origin,
                        &baseline.path,
                        &baseline.digest,
                        FileBaselineState::Deleted,
                    )?;
                    continue;
                }
                if !fs::symlink_metadata(&target)?.file_type().is_file() {
                    conflicts.push(CognitionConflict {
                        table: "file".into(),
                        key: baseline.path,
                        detail: "the peer deleted a file but the local path was independently replaced; the local replacement is kept".into(),
                    });
                    continue;
                }
                let existing = fs::read(&target)
                    .with_context(|| format!("read local file {}", target.display()))?;
                if baseline.state == FileBaselineState::Present
                    && Digest::blake3(&existing) == baseline.digest
                {
                    let retirement = retirements.iter().find(|(origin, _)| origin == file_origin)
                        .context("file retirement root was not durably reserved")?;
                    self.retire_deleted_file(
                        peer_id,
                        origin,
                        &retirement.1,
                        &relative,
                        &target,
                        &baseline.digest,
                    )?;
                    self.write_file_baseline(conn,
                        peer_id,
                        origin,
                        &baseline.path,
                        &baseline.digest,
                        FileBaselineState::Deleted,
                    )?;
                } else {
                    conflicts.push(CognitionConflict {
                        table: "file".into(),
                        key: baseline.path,
                        detail: "the peer deleted this path but the local file changed independently; the local content is kept"
                            .into(),
                    });
                }
            }
        }
        Ok((verified, conflicts))
    }

    fn retire_deleted_file(
        &self,
        peer_id: EntityId,
        origin: &str,
        retirement_root: &Path,
        relative: &Path,
        target: &Path,
        expected: &Digest,
    ) -> Result<()> {
        let retired = retirement_root
            .join(peer_id.to_string())
            .join(origin)
            .join(EntityId::new().to_string())
            .join(relative);
        if let Some(parent) = retired.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create retirement directory {}", parent.display()))?;
        }
        // The root was registered outside the import transaction. A later
        // rollback cannot turn these preserved bytes into ordinary memory.
        fs::rename(target, &retired).with_context(|| {
            format!(
                "preserve deleted file {} by moving it to {}",
                target.display(),
                retired.display()
            )
        })?;
        let retained = fs::read(&retired)
            .with_context(|| format!("read retired file {}", retired.display()))?;
        ensure!(
            Digest::blake3(&retained) == *expected,
            "retired file {} does not preserve its original bytes",
            retired.display()
        );
        Ok(())
    }

    fn read_file_baselines(&self, conn: &rusqlite::Connection, peer_id: EntityId, origin: &str) -> Result<Vec<FileBaseline>> {
        let mut stmt = conn.prepare(
            "SELECT path,digest,state FROM cognition_file_baselines
             WHERE peer_id=?1 AND origin=?2 ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![peer_id.to_string(), origin], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(path, digest, state)| {
                Ok(FileBaseline {
                    path,
                    digest: Digest(digest),
                    state: FileBaselineState::parse(&state)?,
                })
            })
            .collect()
    }

    /// The content and reconciliation state the peer last delivered for one file.
    fn read_file_baseline(
        &self,
        conn: &rusqlite::Connection,
        peer_id: EntityId,
        origin: &str,
        path: &str,
    ) -> Result<Option<FileBaseline>> {
        let stored: Option<(String, String)> = conn
            .query_row(
                "SELECT digest,state FROM cognition_file_baselines
                 WHERE peer_id=?1 AND origin=?2 AND path=?3",
                params![peer_id.to_string(), origin, path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db)?;
        stored
            .map(|(digest, state)| {
                Ok(FileBaseline {
                    path: path.to_string(),
                    digest: Digest(digest),
                    state: FileBaselineState::parse(&state)?,
                })
            })
            .transpose()
    }

    /// Remember the content and reconciliation state the peer delivered.
    fn write_file_baseline(
        &self,
        conn: &rusqlite::Connection,
        peer_id: EntityId,
        origin: &str,
        path: &str,
        digest: &Digest,
        state: FileBaselineState,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO cognition_file_baselines(peer_id,origin,path,digest,state,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(peer_id,origin,path) DO UPDATE SET
                digest=excluded.digest,
                state=excluded.state,
                updated_at=excluded.updated_at",
            params![
                peer_id.to_string(),
                origin,
                path,
                digest.to_string(),
                state.as_str(),
                chrono::Utc::now().to_rfc3339(),
            ],
        )
        .map_err(db)?;
        Ok(())
    }

    /// Put the memory repository history back in place.
    ///
    /// The history is fetched into a per peer namespace, so a local history is
    /// never overwritten. A store without local commits adopts the replicated
    /// head; a store with its own history keeps both and reports the
    /// divergence.
    pub(super) fn restore_git_history(
        &self,
        conn: &rusqlite::Connection,
        git_history: &Option<(CognitionGitDescriptor, Vec<u8>)>,
        root: &Path,
        peer_id: EntityId,
    ) -> Result<(bool, Option<CognitionConflict>)> {
        let Some((descriptor, bytes)) = git_history.as_ref() else {
            return Ok((false, None));
        };
        let temp =
            std::env::temp_dir().join(format!("shenko-cognition-{}.bundle", EntityId::new()));
        fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
        let outcome = self.restore_git_history_inner(conn, root, peer_id, descriptor, &temp);
        let _ = fs::remove_file(&temp);
        outcome
    }

    fn restore_git_history_inner(
        &self,
        conn: &rusqlite::Connection,
        root: &Path,
        peer_id: EntityId,
        descriptor: &CognitionGitDescriptor,
        bundle: &Path,
    ) -> Result<(bool, Option<CognitionConflict>)> {
        let branch = descriptor
            .head
            .strip_prefix("refs/heads/")
            .filter(|branch| !branch.is_empty())
            .ok_or_else(|| anyhow!("cognition Git HEAD is not a branch: {}", descriptor.head))?;
        let replicated = descriptor
            .refs
            .iter()
            .find(|(name, _)| name == &descriptor.head)
            .map(|(_, sha)| sha.clone())
            .ok_or_else(|| {
                anyhow!(
                    "cognition Git descriptor does not bind HEAD {} to a replicated commit",
                    descriptor.head
                )
            })?;
        if !root.join(".git").exists() {
            fs::create_dir_all(root)?;
            let initial_branch = format!("--initial-branch={branch}");
            let (ok, _, error) = git(root, &["init", "--quiet", initial_branch.as_str()])?;
            ensure!(ok, "git init failed: {error}");
        }
        let label = peer_id.to_string();
        let bundle_path = bundle.to_string_lossy().into_owned();
        let heads = format!("+refs/heads/*:refs/replicated/{label}/*");
        let tags = format!("+refs/tags/*:refs/replicated/{label}/tags/*");
        let (ok, _, error) = git(
            root,
            &[
                "fetch",
                "--quiet",
                bundle_path.as_str(),
                heads.as_str(),
                tags.as_str(),
            ],
        )?;
        ensure!(ok, "git fetch of the memory history failed: {error}");
        let (has_destination, destination_out, _) =
            git(root, &["rev-parse", "--verify", &descriptor.head])?;
        let (_, current_ref, _) = git(root, &["symbolic-ref", "--quiet", "HEAD"])?;
        if has_destination
            && destination_out.trim() != replicated
            && current_ref.trim() != descriptor.head
        {
            return Ok((true, Some(CognitionConflict {
                table: "git".into(),
                key: descriptor.head.clone(),
                detail: "the source HEAD branch already has an independent local tip; both histories are kept".into(),
            })));
        }
        let (has_head, head_out, _) = git(root, &["rev-parse", "--verify", "HEAD"])?;
        if !has_head {
            self.adopt_git_head(conn, root, peer_id, descriptor, &replicated)?;
            return Ok((true, None));
        }
        let local_head = head_out.trim().to_string();
        let (_, local_ref_out, _) = git(root, &["symbolic-ref", "--quiet", "HEAD"])?;
        let local_ref = local_ref_out
            .lines()
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let baseline = self.read_git_baseline(conn, peer_id)?;
        let unchanged = baseline.as_ref().is_some_and(|baseline| {
            baseline.head == local_head
                && baseline
                    .reference
                    .as_deref()
                    .is_none_or(|reference| Some(reference) == local_ref.as_deref())
        });
        if unchanged
            && (replicated != local_head || local_ref.as_deref() != Some(descriptor.head.as_str()))
        {
            // The local history is still the one the peer delivered, so a
            // newer remote head is the peer's continuation, not a conflict.
            let fast_forward = if replicated == local_head {
                true
            } else {
                let (ok, _, _) = git(
                    root,
                    &["merge-base", "--is-ancestor", &local_head, &replicated],
                )?;
                ok
            };
            if fast_forward {
                self.adopt_git_head(conn, root, peer_id, descriptor, &replicated)?;
                return Ok((true, None));
            }
        }
        let conflict = if replicated != local_head
            || local_ref.as_deref() != Some(descriptor.head.as_str())
        {
            Some(CognitionConflict {
                table: "git".into(),
                key: "memory".into(),
                detail: format!(
                    "local HEAD {} ({local_head}) differs from replicated HEAD {} ({replicated}); both histories are kept",
                    local_ref.as_deref().unwrap_or("detached"),
                    descriptor.head
                ),
            })
        } else {
            None
        };
        if conflict.is_none() {
            self.write_git_baseline(conn, peer_id, &replicated, &descriptor.head)?;
        }
        Ok((true, conflict))
    }

    fn adopt_git_head(
        &self,
        conn: &rusqlite::Connection,
        root: &Path,
        peer_id: EntityId,
        descriptor: &CognitionGitDescriptor,
        head: &str,
    ) -> Result<()> {
        let (ok, _, error) = git(root, &["update-ref", &descriptor.head, head])?;
        ensure!(ok, "git update-ref failed: {error}");
        let (ok, _, error) = git(root, &["symbolic-ref", "HEAD", &descriptor.head])?;
        ensure!(ok, "git symbolic-ref failed: {error}");
        let (ok, _, error) = git(root, &["reset", "--mixed"])?;
        ensure!(ok, "git reset failed: {error}");
        self.write_git_baseline(conn, peer_id, head, &descriptor.head)
    }

    /// The remote head this store last took from the peer.
    fn read_git_baseline(&self, conn: &rusqlite::Connection, peer_id: EntityId) -> Result<Option<GitBaseline>> {
        let stored: Option<String> = conn
            .query_row(
                "SELECT row_values FROM cognition_baselines WHERE peer_id=?1 AND table_name='git' AND entity_key='memory'",
                [peer_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        match stored {
            Some(json) => {
                let values: Vec<(String, Option<String>)> = serde_json::from_str(&json)
                    .map_err(|error| anyhow!("invalid cognition git baseline: {error}"))?;
                let head = values
                    .iter()
                    .find(|(name, _)| name == "head")
                    .and_then(|(_, value)| value.clone());
                let reference = values
                    .into_iter()
                    .find(|(name, _)| name == "ref")
                    .and_then(|(_, value)| value);
                Ok(head.map(|head| GitBaseline { head, reference }))
            }
            None => Ok(None),
        }
    }

    /// Remember the remote head this store took from the peer.
    fn write_git_baseline(&self, conn: &rusqlite::Connection, peer_id: EntityId, head: &str, reference: &str) -> Result<()> {
        let values = vec![
            ("head".to_string(), Some(head.to_string())),
            ("ref".to_string(), Some(reference.to_string())),
        ];
        let json = serde_json::to_string(&values)?;
        conn.execute(
            "INSERT INTO cognition_baselines(peer_id,table_name,entity_key,row_values,updated_at)
             VALUES(?1,'git','memory',?2,?3)
             ON CONFLICT(peer_id,table_name,entity_key) DO UPDATE SET
                row_values=excluded.row_values,
                updated_at=excluded.updated_at",
            params![peer_id.to_string(), json, chrono::Utc::now().to_rfc3339(),],
        )
        .map_err(db)?;
        Ok(())
    }
}

pub(super) fn file_origin_name(origin: CognitionFileOrigin) -> &'static str {
    match origin {
        CognitionFileOrigin::Memory => "memory",
        CognitionFileOrigin::Workspace => "workspace",
    }
}

/// Collect the files of one root. The Git directory of the memory root is
/// not copied as files; its history travels as a Git bundle.
pub(super) fn collect_files(
    root: &Path,
    origin: CognitionFileOrigin,
    retired_roots: &BTreeSet<PathBuf>,
    out: &mut Vec<(CognitionFileDescriptor, PathBuf)>,
    gaps: &mut Vec<CognitionGap>,
) -> Result<()> {
    if !root.exists() {
        gaps.push(CognitionGap {
            kind: "missing_root".into(),
            detail: root.display().to_string(),
            blocking: true,
        });
        return Ok(());
    }
    let mut stack: Vec<(PathBuf, String)> = vec![(fs::canonicalize(root)?, String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                gaps.push(CognitionGap {
                    kind: "unreadable_directory".into(),
                    detail: format!("{}: {error}", dir.display()),
                    blocking: true,
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_entry".into(),
                        detail: format!("{}: {error}", dir.display()),
                        blocking: true,
                    });
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if prefix.is_empty() && name == ".git" {
                continue;
            }
            let path = entry.path();
            if retired_roots.contains(&path) {
                continue;
            }
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_entry".into(),
                        detail: format!("{}: {error}", path.display()),
                        blocking: true,
                    });
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                gaps.push(CognitionGap {
                    kind: "symlink".into(),
                    detail: relative,
                    blocking: true,
                });
                continue;
            }
            if metadata.is_dir() {
                stack.push((path, relative));
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    gaps.push(CognitionGap {
                        kind: "unreadable_file".into(),
                        detail: format!("{}: {error}", path.display()),
                        blocking: true,
                    });
                    continue;
                }
            };
            out.push((
                CognitionFileDescriptor {
                    path: relative,
                    bytes: bytes.len() as u64,
                    digest: Digest::blake3(&bytes),
                    origin,
                },
                path,
            ));
        }
    }
    out.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    Ok(())
}

/// Capture the history of a Git managed memory root.
///
/// The bundle carries the objects and the refs only. The repository's own
/// configuration, and with it any local credential, never leaves the node.
pub(super) fn capture_git_history(root: &Path) -> Result<Option<(CognitionGitDescriptor, Vec<u8>)>> {
    if !root.join(".git").exists() {
        return Ok(None);
    }
    let (has_head, head_id, _) = git(root, &["rev-parse", "--verify", "HEAD"])?;
    if !has_head {
        return Ok(None);
    }
    let (symbolic, head, _) = git(root, &["symbolic-ref", "--quiet", "HEAD"])?;
    ensure!(
        symbolic,
        "memory repository HEAD is detached; replication requires an actual source branch"
    );
    let head = head.trim().to_string();
    ensure!(
        head.starts_with("refs/heads/"),
        "memory repository HEAD is not a branch: {head}"
    );
    let temp = std::env::temp_dir().join(format!("shenko-cognition-{}.bundle", EntityId::new()));
    let temp_path = temp.to_string_lossy().into_owned();
    // Replicated peer refs are receiver-local bookkeeping, not the source's
    // memory history. Including them would make every re-export look changed.
    let (ok, _, error) = git(
        root,
        &[
            "bundle",
            "create",
            temp_path.as_str(),
            "--branches",
            "--tags",
        ],
    )?;
    if !ok {
        let _ = fs::remove_file(&temp);
        bail!("git bundle create failed: {error}");
    }
    let bytes = fs::read(&temp).with_context(|| format!("read {}", temp.display()))?;
    let _ = fs::remove_file(&temp);
    let mut refs: Vec<(String, String)> = Vec::new();
    let (ok, out, error) = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
        ],
    )?;
    ensure!(ok, "git ref enumeration failed: {error}");
    for line in out.lines() {
        if let Some((name, sha)) = line.split_once(' ') {
            refs.push((name.to_string(), sha.to_string()));
        }
    }
    ensure!(
        refs.iter()
            .any(|(name, sha)| { name == &head && sha == head_id.trim() }),
        "memory repository HEAD {head} is not bound to its advertised commit"
    );
    let descriptor = CognitionGitDescriptor {
        bytes: bytes.len() as u64,
        digest: Digest::blake3(&bytes),
        head,
        refs,
    };
    Ok(Some((descriptor, bytes)))
}

fn git(root: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn verify_restored_file(target: &Path, descriptor: &CognitionFileDescriptor) -> Result<()> {
    let retained =
        fs::read(target).with_context(|| format!("read restored file {}", target.display()))?;
    ensure!(
        retained.len() as u64 == descriptor.bytes && Digest::blake3(&retained) == descriptor.digest,
        "restored file {} does not match its verified descriptor",
        target.display()
    );
    Ok(())
}

fn safe_relative_path(path: &str) -> Result<PathBuf> {
    let candidate = Path::new(path);
    ensure!(!candidate.is_absolute(), "file path {path} is not relative");
    let mut out = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => bail!("file path {path} escapes the target root"),
        }
    }
    ensure!(!out.as_os_str().is_empty(), "file path is empty");
    Ok(out)
}
