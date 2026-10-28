//! Self-upgrade A/B handoff state machine — SPEC §35.7.
//!
//! A keeps serving while B is prepared on a copy: `prepare` (copy artifact
//! tree + consistent SQLite backup) → `catch_up` → `smoke` → `handoff` →
//! `observe` → `retire`. A failed handoff can roll back to the old closure
//! (`RolledBack`); the old slot is never modified here, so it remains
//! launchable. Facts produced by B are not deleted by a software rollback —
//! wiring code must import/reconcile them when it implements `RolledBack`
//! (SPEC §35.7).
//!
//! Safety boundary: this crate only manages slots and records. It does NOT
//! start a second subject loop, send real QQ messages, or operate real seats.
//! Default smoke checks are local-only (db integrity + artifact presence).
//! External probes, if ever needed, belong to a dedicated operation with its
//! own authorization and §36 effect declaration.
//!
//! TODO(bin/shenko): wire `UpdateCoordinator` into the runtime — perform the
//! round-boundary quiesce, catch-up of inbox/outbox/tasks/seats/operations,
//! epoch switch after a successful handoff, and new-fact preservation on
//! rollback. The record's `created_commit` / `updated_commit` are populated
//! by that integration when each transition is committed to the ledger.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, DatabaseName};
use serde::{Deserialize, Serialize};

use shenko_types::commit::CommitRef;
use shenko_types::ids::{EntityId, RecordRevision, RuntimeGeneration};

/// Filename used for the copied database inside a prepared slot.
pub const SLOT_DB_FILENAME: &str = "shenko.db";

/// Update phase — SPEC §35.7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Prepare,
    CatchUp,
    Smoke,
    Handoff,
    Observe,
    Retire,
    RolledBack,
    Failed { reason: String },
}

impl UpdatePhase {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Retire | Self::RolledBack | Self::Failed { .. })
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::CatchUp => "catch_up",
            Self::Smoke => "smoke",
            Self::Handoff => "handoff",
            Self::Observe => "observe",
            Self::Retire => "retire",
            Self::RolledBack => "rolled_back",
            Self::Failed { .. } => "failed",
        }
    }
}

impl fmt::Display for UpdatePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed { reason } => write!(f, "failed({reason})"),
            other => f.write_str(other.name()),
        }
    }
}

/// Durable update record. Transitions advance `record_revision`; ledger
/// commits are attached by runtime wiring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRecord {
    pub update_id: EntityId,
    pub from_generation: RuntimeGeneration,
    pub to_generation: RuntimeGeneration,
    pub phase: UpdatePhase,
    pub record_revision: RecordRevision,
    pub created_commit: Option<CommitRef>,
    pub updated_commit: Option<CommitRef>,
    pub notes: Vec<String>,
}

/// One A/B slot: a runtime generation with its artifact tree and database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbSlot {
    pub generation: RuntimeGeneration,
    pub artifact_dir: PathBuf,
    pub db_path: PathBuf,
    pub is_active: bool,
}

/// Result of one local smoke check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmokeCheckOutcome {
    pub name: String,
    pub passed: bool,
    pub detail: Option<String>,
}

/// Aggregate smoke report. `Err` is reserved for infrastructure failures of
/// the checker itself; a failed check is a normal `Ok(report)` with
/// `passed == false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmokeReport {
    pub checks: Vec<SmokeCheckOutcome>,
    pub passed: bool,
}

impl SmokeReport {
    pub fn failures(&self) -> impl Iterator<Item = &SmokeCheckOutcome> {
        self.checks.iter().filter(|c| !c.passed)
    }
}

/// A local-only smoke check closure. Checks must never touch the network,
/// real QQ, or real seats (SPEC §35.7).
/// Result of a local smoke check; `Err(message)` marks the check failed.
pub type CheckResult = std::result::Result<(), String>;

pub struct SafeSmokeCheck {
    name: String,
    check: Box<dyn Fn(&AbSlot) -> CheckResult + Send + Sync>,
}

impl SafeSmokeCheck {
    pub fn new(
        name: impl Into<String>,
        check: impl Fn(&AbSlot) -> CheckResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            check: Box::new(check),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn run(&self, slot: &AbSlot) -> SmokeCheckOutcome {
        let outcome = (self.check)(slot);
        SmokeCheckOutcome {
            name: self.name.clone(),
            passed: outcome.is_ok(),
            detail: outcome.err(),
        }
    }
}

impl fmt::Debug for SafeSmokeCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SafeSmokeCheck").field("name", &self.name).finish()
    }
}

/// Errors from the update coordinator.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("invalid phase transition: {from} -> {to}")]
    InvalidTransition { from: UpdatePhase, to: UpdatePhase },
    #[error("update record is terminal: {0}")]
    Terminal(UpdatePhase),
    #[error("generation mismatch: coordinator targets {expected}, got {actual}")]
    GenerationMismatch {
        expected: RuntimeGeneration,
        actual: RuntimeGeneration,
    },
    #[error("source slot is invalid: {0}")]
    InvalidSourceSlot(String),
    #[error("could not create staging path {}: {source}; inspect that path, which was not removed", staged_path.display())]
    StageCreation {
        staged_path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not prepare staging directory {}: {source}; it was not removed", staged_path.display())]
    StagePreparation {
        staged_path: PathBuf,
        #[source]
        source: Box<UpdateError>,
    },
    #[error("publication from {} to {} returned an error: {source}; the outcome may be uncertain, inspect both paths before recovery", staged_path.display(), slot_path.display())]
    SlotPublication {
        staged_path: PathBuf,
        slot_path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, UpdateError>;

/// Drives the §35.7 prepare/catch_up/smoke/handoff/observe/retire sequence.
///
/// The coordinator owns the update record and the staging root where B slots
/// are created. It never starts a subject loop and never mutates the active
/// A slot.
pub struct UpdateCoordinator {
    update: UpdateRecord,
    staging_root: PathBuf,
    checks: Vec<SafeSmokeCheck>,
}

impl UpdateCoordinator {
    /// Start a new update from `from_generation` to `to_generation`.
    /// B slots are created under `staging_root`.
    pub fn new(
        from_generation: RuntimeGeneration,
        to_generation: RuntimeGeneration,
        staging_root: impl Into<PathBuf>,
    ) -> Self {
        let mut notes = Vec::new();
        notes.push(format!(
            "update created: {} -> {}; old closure retained for rollback",
            from_generation, to_generation
        ));
        Self {
            update: UpdateRecord {
                update_id: EntityId::new(),
                from_generation,
                to_generation,
                phase: UpdatePhase::Prepare,
                record_revision: RecordRevision(0),
                created_commit: None,
                updated_commit: None,
                notes,
            },
            staging_root: staging_root.into(),
            checks: default_smoke_checks(),
        }
    }

    /// Replace the default local smoke checks (e.g. additional local probes).
    pub fn set_smoke_checks(&mut self, checks: Vec<SafeSmokeCheck>) {
        self.checks = checks;
    }

    pub fn record(&self) -> &UpdateRecord {
        &self.update
    }

    pub fn phase(&self) -> &UpdatePhase {
        &self.update.phase
    }

    /// Prepare the B slot under `staging_root`:
    /// 1. copy the artifact directory tree (excluding the live SQLite files),
    /// 2. copy the database with the SQLite Backup API — a raw file copy of a
    ///    live database is not a consistent transaction snapshot (SPEC §35.7),
    /// 3. publish the completed sibling directory with a native no-replace
    ///    rename. An error identifies the stage for inspection; no failed or
    ///    losing stage is removed by this coordinator.
    ///
    /// A is left untouched and keeps serving.
    pub fn prepare_slot(
        &self,
        from: &AbSlot,
        to_generation: RuntimeGeneration,
    ) -> Result<AbSlot> {
        if to_generation != self.update.to_generation {
            return Err(UpdateError::GenerationMismatch {
                expected: self.update.to_generation.clone(),
                actual: to_generation,
            });
        }
        if !from.artifact_dir.is_dir() {
            return Err(UpdateError::InvalidSourceSlot(format!(
                "artifact dir is not a directory: {}",
                from.artifact_dir.display()
            )));
        }
        if !from.db_path.is_file() {
            return Err(UpdateError::InvalidSourceSlot(format!(
                "database is missing: {}",
                from.db_path.display()
            )));
        }

        let src_dir = from.artifact_dir.canonicalize()?;
        // This name is distinct even on case-insensitive filesystems, and
        // create_dir, not a prior existence check, claims it exclusively.
        let staged_path = self.staging_root.join(format!(".shenko-stage-{}", EntityId::new()));
        let nested = staging_root_inside_source(&self.staging_root, &src_dir).map_err(|source| {
            UpdateError::StageCreation {
                staged_path: staged_path.clone(),
                source,
            }
        })?;
        if nested {
            return Err(UpdateError::InvalidSourceSlot(format!(
                "staging root {} is inside source artifact directory {}",
                self.staging_root.display(),
                src_dir.display()
            )));
        }
        let artifact_dir = self
            .staging_root
            .join(format!("generation-{}", sanitize_generation(&to_generation)));
        let db_path = artifact_dir.join(SLOT_DB_FILENAME);
        fs::create_dir_all(&self.staging_root).map_err(|source| UpdateError::StageCreation {
            staged_path: staged_path.clone(),
            source,
        })?;
        fs::create_dir(&staged_path).map_err(|source| UpdateError::StageCreation {
            staged_path: staged_path.clone(),
            source,
        })?;

        let result = (|| -> Result<()> {
            let db_canon = from.db_path.canonicalize()?;
            if staged_path.canonicalize()?.starts_with(&src_dir) {
                return Err(UpdateError::InvalidSourceSlot(format!(
                    "staging directory {} is inside source artifact directory {}",
                    staged_path.display(),
                    src_dir.display()
                )));
            }
            // Copy the tree first while excluding A's live SQLite files, then
            // take the consistent backup into the exclusively owned stage.
            copy_dir_tree(&src_dir, &staged_path, &db_canon)?;
            backup_database(&from.db_path, &staged_path.join(SLOT_DB_FILENAME))?;
            Ok(())
        })();

        result.map_err(|source| UpdateError::StagePreparation {
            staged_path: staged_path.clone(),
            source: Box::new(source),
        })?;

        // Never retry or remove either path after an error: an NFS server can
        // complete a rename even if its reply is lost and the client sees an
        // error. Other filesystems can also have unproven failure semantics.
        rename_no_replace(&staged_path, &artifact_dir).map_err(|source| {
            UpdateError::SlotPublication {
                staged_path: staged_path.clone(),
                slot_path: artifact_dir.clone(),
                source,
            }
        })?;

        tracing::info!(
            update_id = %self.update.update_id,
            from = %from.generation,
            to = %to_generation,
            slot = %artifact_dir.display(),
            "A/B slot prepared with consistent SQLite backup"
        );

        Ok(AbSlot {
            generation: to_generation,
            artifact_dir,
            db_path,
            // B is not active until handoff completes.
            is_active: false,
        })
    }

    /// Advance the state machine with validity checks.
    ///
    /// Valid sequence: Prepare → CatchUp → Smoke → Handoff → Observe → Retire.
    /// Any non-failed phase may move to Failed; Handoff may instead move to
    /// RolledBack (old closure retained). Prepare → Handoff is rejected.
    pub fn advance(&mut self, next: UpdatePhase) -> Result<UpdateRecord> {
        let from = self.update.phase.clone();
        if !transition_allowed(&from, &next) {
            return Err(UpdateError::InvalidTransition { from, to: next });
        }

        self.update.record_revision = self.update.record_revision.next();
        self.update.phase = next.clone();
        self.update
            .notes
            .push(format!("phase {} at {}", next.name(), chrono::Utc::now()));

        tracing::info!(
            update_id = %self.update.update_id,
            revision = self.update.record_revision.0,
            phase = %next,
            "update phase advanced"
        );

        Ok(self.update.clone())
    }

    /// Run all configured local smoke checks against the B slot.
    ///
    /// Default checks: the copied database opens and
    /// `PRAGMA integrity_check` returns `ok`; the artifact directory is
    /// non-empty. No network, QQ, or seat access is performed.
    pub fn smoke_checks(&self, slot: &AbSlot) -> Result<SmokeReport> {
        let checks: Vec<SmokeCheckOutcome> = self.checks.iter().map(|c| c.run(slot)).collect();
        let passed = checks.iter().all(|c| c.passed);
        let report = SmokeReport { checks, passed };
        tracing::info!(
            update_id = %self.update.update_id,
            generation = %slot.generation,
            passed,
            "update smoke checks completed"
        );
        Ok(report)
    }
}

fn transition_allowed(from: &UpdatePhase, to: &UpdatePhase) -> bool {
    use UpdatePhase::*;
    match (from, to) {
        // A failed update may be recorded from any live phase.
        (Prepare | CatchUp | Smoke | Handoff | Observe | Retire | RolledBack, Failed { .. }) => true,
        // Handoff failure keeps the old closure launchable.
        (Handoff, RolledBack) => true,
        (Prepare, CatchUp) => true,
        (CatchUp, Smoke) => true,
        (Smoke, Handoff) => true,
        (Handoff, Observe) => true,
        (Observe, Retire) => true,
        _ => false,
    }
}

fn default_smoke_checks() -> Vec<SafeSmokeCheck> {
    vec![
        SafeSmokeCheck::new("db_integrity", |slot| {
            if !slot.db_path.is_file() {
                return Err(format!("database missing: {}", slot.db_path.display()));
            }
            let conn = Connection::open(&slot.db_path)
                .map_err(|e| format!("database opens: {e}"))?;
            let integrity: String = conn
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(|e| format!("integrity_check: {e}"))?;
            if integrity != "ok" {
                return Err(format!("integrity_check returned {integrity:?}"));
            }
            Ok(())
        }),
        SafeSmokeCheck::new("artifact_dir_nonempty", |slot| {
            if !slot.artifact_dir.is_dir() {
                return Err(format!(
                    "artifact dir missing: {}",
                    slot.artifact_dir.display()
                ));
            }
            let mut entries = fs::read_dir(&slot.artifact_dir)
                .map_err(|e| format!("read artifact dir: {e}"))?;
            if entries.next().is_none() {
                return Err(format!(
                    "artifact dir is empty: {}",
                    slot.artifact_dir.display()
                ));
            }
            Ok(())
        }),
    ]
}

/// Copy the source DB with the SQLite Backup API into an exclusively new file.
fn backup_database(src_db: &Path, dst_db: &Path) -> Result<()> {
    let src = Connection::open(src_db)?;
    // A source artifact with this name must not be replaced by the backup.
    let new_db = fs::OpenOptions::new().write(true).create_new(true).open(dst_db)?;
    drop(new_db);
    src.backup(DatabaseName::Main, dst_db, None)?;
    Ok(())
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(Into::into)
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;

    let mut from: Vec<u16> = from.as_os_str().encode_wide().collect();
    let mut to: Vec<u16> = to.as_os_str().encode_wide().collect();
    if from.contains(&0) || to.contains(&0) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"));
    }
    from.push(0);
    to.push(0);
    // SAFETY: both pointers refer to NUL-terminated strings alive for this
    // call; MoveFileW does not replace an existing destination.
    if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
)))]
fn rename_no_replace(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive directory publication is not supported on this platform",
    ))
}

// Reject a staging root inside A, and missing prefixes inside A that mkdir
// would have to create even if later `..` components leave A. Existing
// prefixes inside A may be traversed to an already existing sibling.
fn staging_root_inside_source(root: &Path, source: &Path) -> std::io::Result<bool> {
    let absolute = std::path::absolute(root)?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => { resolved.pop(); }
            other => resolved.push(other.as_os_str()),
        }
        if resolved.is_absolute() {
            match resolved.canonicalize() {
                Ok(canonical) => resolved = canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if resolved.starts_with(source) {
                        return Ok(true);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(resolved.starts_with(source))
}

fn copy_dir_tree(src: &Path, dst: &Path, skip_db: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();

        if is_sqlite_file(&src_path, skip_db) {
            continue;
        }

        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_tree(&src_path, &dst_path, skip_db)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&src_path)?;
                std::os::unix::fs::symlink(target, &dst_path)?;
            }
            #[cfg(not(unix))]
            {
                // Without a portable symlink API, copy the target content.
                fs::copy(&src_path, &dst_path)?;
            }
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn is_sqlite_file(path: &Path, db_path: &Path) -> bool {
    if path == db_path {
        return true;
    }
    ["-wal", "-shm", "-journal"]
        .iter()
        .any(|suffix| path == sidecar_path(db_path, suffix))
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn sanitize_generation(generation: &RuntimeGeneration) -> String {
    let sanitized: String = generation
        .0
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "shenko-update-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn slot_a(root: &Path) -> (AbSlot, Connection) {
        let artifact_dir = root.join("slot-a");
        fs::create_dir_all(artifact_dir.join("plugins")).unwrap();
        fs::write(artifact_dir.join("plugins/example.wasm"), b"\0asm").unwrap();
        fs::write(artifact_dir.join("manifest.json"), b"{\"generation\":\"a\"}").unwrap();

        let db_path = artifact_dir.join(SLOT_DB_FILENAME);
        let conn = Connection::open(&db_path).unwrap();
        // Keep A live in WAL mode while the backup is taken.
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            "CREATE TABLE facts (id INTEGER PRIMARY KEY, body TEXT);
             INSERT INTO facts (body) VALUES ('committed-1'), ('committed-2');",
        )
        .unwrap();

        (
            AbSlot {
                generation: RuntimeGeneration::new("a"),
                artifact_dir,
                db_path,
                is_active: true,
            },
            conn,
        )
    }

    #[cfg(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos"
    ))]
    #[test]
    fn happy_path_and_consistent_copy() {
        let root = temp_root("happy");
        let (a, keep_a_open) = slot_a(&root);

        let mut coordinator = UpdateCoordinator::new(
            a.generation.clone(),
            RuntimeGeneration::new("b"),
            root.join("staging"),
        );

        let b = coordinator
            .prepare_slot(&a, RuntimeGeneration::new("b"))
            .unwrap();
        assert!(!b.is_active);
        assert_eq!(b.db_path.file_name().unwrap(), SLOT_DB_FILENAME);
        assert!(b.artifact_dir.join("plugins/example.wasm").is_file());
        assert!(b.artifact_dir.join("manifest.json").is_file());
        // The live db and its WAL sidecars are not copied as raw files.
        assert!(!b.artifact_dir.join("shenko.db-wal").exists());

        // The copied DB is a real consistent snapshot, independent of A.
        let copied = Connection::open(&b.db_path).unwrap();
        let integrity: String = copied
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let count: i64 = copied
            .query_row("SELECT count(*) FROM facts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        drop(copied);

        // A is still usable and still owns its own data.
        let a_count: i64 = keep_a_open
            .query_row("SELECT count(*) FROM facts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(a_count, 2);
        assert!(a.generation == RuntimeGeneration::new("a"));
        drop(keep_a_open);

        coordinator.advance(UpdatePhase::CatchUp).unwrap();
        coordinator.advance(UpdatePhase::Smoke).unwrap();

        let report = coordinator.smoke_checks(&b).unwrap();
        assert!(report.passed, "smoke failures: {:?}", report.checks);

        coordinator.advance(UpdatePhase::Handoff).unwrap();
        coordinator.advance(UpdatePhase::Observe).unwrap();
        let final_record = coordinator.advance(UpdatePhase::Retire).unwrap();

        assert_eq!(final_record.phase, UpdatePhase::Retire);
        assert_eq!(final_record.record_revision, RecordRevision(5));
        assert_eq!(final_record.update_id, coordinator.record().update_id);
        assert_eq!(final_record.notes.len(), 6);
        assert!(final_record.phase.is_terminal());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stage_creation_error_reports_attempted_path_and_preserves_root() {
        let root = temp_root("stage-creation");
        let (a, conn) = slot_a(&root);
        drop(conn);
        let staging = root.join("staging-file");
        fs::write(&staging, b"do not replace").unwrap();
        let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

        let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
        let staged_path = match error {
            UpdateError::StageCreation { staged_path, .. } => staged_path,
            other => panic!("unexpected error: {other:?}"),
        };
        assert_eq!(staged_path.parent(), Some(staging.as_path()));
        assert_eq!(fs::read(&staging).unwrap(), b"do not replace");
    }

    #[test]
    fn failed_backup_preserves_partial_stage_and_prior_slot() {
        let root = temp_root("failed-backup");
        let (a, conn) = slot_a(&root);
        drop(conn);
        fs::write(&a.db_path, b"not a sqlite database").unwrap();
        let staging = root.join("staging");
        let final_path = staging.join("generation-b");
        fs::create_dir_all(&final_path).unwrap();
        fs::write(final_path.join("sentinel"), b"old slot").unwrap();
        let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

        let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
        let staged_path = match error {
            UpdateError::StagePreparation { staged_path, source } => {
                assert!(matches!(*source, UpdateError::Sqlite(_)));
                staged_path
            }
            other => panic!("unexpected error: {other:?}"),
        };
        assert_eq!(fs::read(staged_path.join("manifest.json")).unwrap(), b"{\"generation\":\"a\"}");
        assert_eq!(fs::read(staged_path.join("plugins/example.wasm")).unwrap(), b"\0asm");
        assert_eq!(fs::read(final_path.join("sentinel")).unwrap(), b"old slot");
        assert_eq!(fs::read(&a.db_path).unwrap(), b"not a sqlite database");
    }

    #[test]
    fn backup_filename_collision_preserves_staged_artifact() {
        let root = temp_root("backup-collision");
        let (mut a, conn) = slot_a(&root);
        drop(conn);
        let external_db = root.join("external.db");
        fs::copy(&a.db_path, &external_db).unwrap();
        fs::write(&a.db_path, b"unrelated artifact").unwrap();
        a.db_path = external_db;
        let staging = root.join("staging");
        let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

        let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
        let staged_path = match error {
            UpdateError::StagePreparation { staged_path, source } => {
                assert!(matches!(&*source, UpdateError::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists));
                staged_path
            }
            other => panic!("unexpected error: {other:?}"),
        };
        assert_eq!(fs::read(staged_path.join(SLOT_DB_FILENAME)).unwrap(), b"unrelated artifact");
        assert!(!staging.join("generation-b").exists());
    }

    #[cfg(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos"
    ))]
    mod publication {
        use super::*;
        use std::sync::{Arc, Barrier};

        fn assert_complete(path: &Path, manifest: &[u8]) {
            assert_eq!(fs::read(path.join("manifest.json")).unwrap(), manifest);
            assert_eq!(fs::read(path.join("plugins/example.wasm")).unwrap(), b"\0asm");
            let db = path.join(SLOT_DB_FILENAME);
            assert!(db.is_file());
            let conn = Connection::open(db).unwrap();
            let count: i64 = conn.query_row("SELECT count(*) FROM facts", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 2);
        }

        fn losing_stage(error: UpdateError, final_path: &Path) -> PathBuf {
            let staged_path = match error {
                UpdateError::SlotPublication { staged_path, slot_path, .. } => {
                    assert_eq!(slot_path, final_path);
                    staged_path
                }
                other => panic!("unexpected error: {other:?}"),
            };
            assert_ne!(staged_path, final_path);
            staged_path
        }

        #[test]
        fn concurrent_publishers_keep_one_complete_winner_and_one_complete_stage() {
            let root = temp_root("concurrent-publication");
            let (first, first_conn) = slot_a(&root.join("first"));
            let (second, second_conn) = slot_a(&root.join("second"));
            drop(first_conn);
            drop(second_conn);
            fs::write(first.artifact_dir.join("manifest.json"), b"first").unwrap();
            fs::write(second.artifact_dir.join("manifest.json"), b"second").unwrap();
            let staging = root.join("staging");
            let final_path = staging.join("generation-b");
            let barrier = Arc::new(Barrier::new(2));
            let (first_result, second_result) = std::thread::scope(|scope| {
                let first_barrier = Arc::clone(&barrier);
                let first_staging = staging.clone();
                let first_worker = scope.spawn(move || {
                    let coordinator = UpdateCoordinator::new(first.generation.clone(), RuntimeGeneration::new("b"), first_staging);
                    first_barrier.wait();
                    coordinator.prepare_slot(&first, RuntimeGeneration::new("b"))
                });
                let second_worker = scope.spawn(move || {
                    let coordinator = UpdateCoordinator::new(second.generation.clone(), RuntimeGeneration::new("b"), &staging);
                    barrier.wait();
                    coordinator.prepare_slot(&second, RuntimeGeneration::new("b"))
                });
                (first_worker.join().unwrap(), second_worker.join().unwrap())
            });
            let (winner, loser, winner_manifest, loser_manifest): (_, _, &[u8], &[u8]) =
                match (first_result, second_result) {
                    (Ok(winner), Err(loser)) => (winner, loser, b"first", b"second"),
                    (Err(loser), Ok(winner)) => (winner, loser, b"second", b"first"),
                    other => panic!("expected exactly one winner: {other:?}"),
                };
            assert_eq!(winner.artifact_dir, final_path);
            assert_eq!(winner.db_path, final_path.join(SLOT_DB_FILENAME));
            let stage = losing_stage(loser, &final_path);
            assert_eq!(stage.parent(), final_path.parent());
            assert_complete(&final_path, winner_manifest);
            assert_complete(&stage, loser_manifest);
        }

        #[test]
        fn existing_empty_directory_is_not_replaced() {
            let root = temp_root("existing-empty");
            let (a, conn) = slot_a(&root);
            drop(conn);
            let staging = root.join("staging");
            let final_path = staging.join("generation-b");
            fs::create_dir_all(&final_path).unwrap();
            let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

            let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
            let stage = losing_stage(error, &final_path);
            assert_eq!(fs::read_dir(&final_path).unwrap().count(), 0);
            assert_complete(&stage, b"{\"generation\":\"a\"}");
        }

        #[test]
        fn existing_populated_directory_is_not_replaced() {
            let root = temp_root("existing-populated");
            let (a, conn) = slot_a(&root);
            drop(conn);
            let staging = root.join("staging");
            let final_path = staging.join("generation-b");
            fs::create_dir_all(&final_path).unwrap();
            fs::write(final_path.join("sentinel"), b"prior slot").unwrap();
            let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

            let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
            let stage = losing_stage(error, &final_path);
            assert_eq!(fs::read(final_path.join("sentinel")).unwrap(), b"prior slot");
            assert_eq!(fs::read_dir(&final_path).unwrap().count(), 1);
            assert_complete(&stage, b"{\"generation\":\"a\"}");
        }

        #[test]
        fn staging_root_inside_source_is_rejected_before_creation() {
            let root = temp_root("nested-staging");
            let (a, conn) = slot_a(&root);
            drop(conn);
            let staging = a.artifact_dir.join("new-staging");
            let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

            let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
            assert!(matches!(error, UpdateError::InvalidSourceSlot(_)));
            assert!(!staging.exists());
            assert_complete(&a.artifact_dir, b"{\"generation\":\"a\"}");

            // POSIX preserves `..` while resolving this path: mkdir would have
            // to create the missing intermediate inside A before leaving it.
            #[cfg(unix)]
            {
                let would_create = a.artifact_dir.join("missing");
                let escaped = would_create.join("../../escaped");
                let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &escaped);
                assert!(matches!(coordinator.prepare_slot(&a, RuntimeGeneration::new("b")), Err(UpdateError::InvalidSourceSlot(_))));
                assert!(!would_create.exists());
            }

            // A pre-existing path may traverse through A to an outside sibling.
            let sibling = root.join("staging");
            fs::create_dir(&sibling).unwrap();
            let through_a = a.artifact_dir.join("../staging");
            let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &through_a);
            let prepared = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap();
            assert_eq!(prepared.artifact_dir.canonicalize().unwrap(), sibling.join("generation-b").canonicalize().unwrap());
            assert_complete(&prepared.artifact_dir, b"{\"generation\":\"a\"}");
            assert_complete(&a.artifact_dir, b"{\"generation\":\"a\"}");
        }

        #[cfg(unix)]
        #[test]
        fn dangling_destination_symlink_is_not_replaced() {
            use std::os::unix::fs::symlink;

            let root = temp_root("existing-symlink");
            let (a, conn) = slot_a(&root);
            drop(conn);
            let staging = root.join("staging");
            fs::create_dir_all(&staging).unwrap();
            let final_path = staging.join("generation-b");
            symlink("missing-target", &final_path).unwrap();
            let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

            let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
            let stage = losing_stage(error, &final_path);
            assert!(fs::symlink_metadata(&final_path).unwrap().file_type().is_symlink());
            assert_eq!(fs::read_link(&final_path).unwrap(), PathBuf::from("missing-target"));
            assert_complete(&stage, b"{\"generation\":\"a\"}");
        }
    }

    #[cfg(not(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos"
    )))]
    #[test]
    fn unsupported_platform_preserves_completed_stage() {
        let root = temp_root("unsupported-publication");
        let (a, conn) = slot_a(&root);
        drop(conn);
        let staging = root.join("staging");
        let coordinator = UpdateCoordinator::new(a.generation.clone(), RuntimeGeneration::new("b"), &staging);

        let error = coordinator.prepare_slot(&a, RuntimeGeneration::new("b")).unwrap_err();
        match error {
            UpdateError::SlotPublication { staged_path, slot_path, source } => {
                assert_eq!(source.kind(), std::io::ErrorKind::Unsupported);
                assert_eq!(slot_path, staging.join("generation-b"));
                assert!(!slot_path.exists());
                assert!(staged_path.join(SLOT_DB_FILENAME).is_file());
                assert!(staged_path.join("plugins/example.wasm").is_file());
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let root = temp_root("invalid");
        let mut coordinator = UpdateCoordinator::new(
            RuntimeGeneration::new("a"),
            RuntimeGeneration::new("b"),
            root.join("staging"),
        );

        // Prepare -> Handoff skips copy/catch_up/smoke.
        let err = coordinator.advance(UpdatePhase::Handoff).unwrap_err();
        assert!(matches!(
            err,
            UpdateError::InvalidTransition {
                from: UpdatePhase::Prepare,
                to: UpdatePhase::Handoff
            }
        ));
        assert_eq!(coordinator.record().record_revision, RecordRevision(0));
        assert_eq!(coordinator.record().phase, UpdatePhase::Prepare);

        // A failed handoff rolls back to the old closure.
        coordinator.advance(UpdatePhase::CatchUp).unwrap();
        coordinator.advance(UpdatePhase::Smoke).unwrap();
        coordinator.advance(UpdatePhase::Handoff).unwrap();
        coordinator
            .advance(UpdatePhase::RolledBack)
            .unwrap();
        assert_eq!(coordinator.record().phase, UpdatePhase::RolledBack);

        // Terminal states do not transition further.
        assert!(
            coordinator
                .advance(UpdatePhase::Observe)
                .is_err()
        );

        fs::remove_dir_all(&root).unwrap();
    }
}
