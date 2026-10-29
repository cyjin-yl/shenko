//! The real backend: one `WasiReplSession` per session directory, executing
//! each cell as a WASI preview1 command through `shenko-wasm`.
//!
//! Honesty boundary (§19/A15):
//! * R0 only — the journal is durable; interpreter objects are not claimed.
//! * Every cell is a fresh guest process. Interpreter state persists only
//!   through files the kernel declares (`KernelProfile::persistence`); nothing
//!   in memory survives a cell.
//! * stdin/stdout/stderr are explicit file streams per cell; argv/env are the
//!   kernel's exact values. Output written before a trap stays durable.
//! * `cancel` reports no interrupt ability; `install_dependencies` reports
//!   Unsupported; COW mounts are rejected by `shenko-wasm`.

use crate::contract::*;
use crate::journal as jrn;
use crate::kernel::{self, KernelProfile, ModuleSource, PackageSpec, StatePersistence};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_wasm::{
    HOME_GUEST_PATH, InputFile, MountMode, MountSpec, OutputFile, WasiCommand, WasmRuntime,
};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MANIFEST_SCHEMA: u32 = 1;
const DEFAULT_PREVIEW_BYTES: usize = 4096;

/// Persisted session manifest (host side, outside guest preopens).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub session_id: EntityId,
    pub kernel: KernelProfile,
    /// BLAKE3 of the interpreter module bytes at create; re-verified per cell.
    pub module_blake3: String,
    pub bind: TaskBinding,
    pub limits: ResourceLimits,
    pub environment_generation: shenko_types::ids::RuntimeGeneration,
    pub recovery_level: RecoveryLevel,
    pub forked_from: Option<EntityId>,
    pub created_unix_ms: i64,
    pub disposed_unix_ms: Option<i64>,
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Provider of WASI preview1 REPL sessions under one sessions root.
pub struct WasiReplProvider {
    pub sessions_root: PathBuf,
}

impl WasiReplProvider {
    pub fn new(sessions_root: PathBuf) -> Self {
        Self { sessions_root }
    }

    /// The canonical sessions root: mount sources must be canonical, and the
    /// root is created only by being used, never silently elsewhere.
    fn canonical_root(&self) -> Result<PathBuf> {
        fs::create_dir_all(&self.sessions_root)?;
        self.sessions_root
            .canonicalize()
            .with_context(|| format!("canonicalize {}", self.sessions_root.display()))
    }

    /// Recover a session from its directory: reopen it and rebuild interpreter
    /// state by re-executing journaled Pure cells. Declared-effect cells keep
    /// their durable record as the receipt — their external effect is never
    /// re-executed (§19 replay boundary, A15). Returns the session and the
    /// per-cell decision report.
    pub fn recover(
        session_dir: &Path,
        preview_bytes: usize,
    ) -> Result<(WasiReplSession, RecoverReport)> {
        let mut session = open_session(session_dir, preview_bytes)?;
        let report = session.restore()?;
        Ok((session, report))
    }
}

impl ReplProvider for WasiReplProvider {
    type Session = WasiReplSession;

    fn kernels(&self) -> Vec<kernel::KernelTarget> {
        kernel::named_targets()
    }

    fn create(&self, request: CreateSession) -> Result<WasiReplSession> {
        kernel::validate_profile(&request.kernel)?;
        let ModuleSource::ModulePath(module_path) = &request.kernel.module;
        let module_bytes = fs::read(module_path).with_context(|| {
            format!(
                "REPL_KERNEL_UNAVAILABLE: cannot read interpreter module {}",
                module_path.display()
            )
        })?;
        let module_blake3 = jrn::hash_bytes(&module_bytes);

        let root = self.canonical_root()?;
        let session_id = EntityId::new();
        let dir = root.join(session_id.to_string());
        if dir.exists() {
            bail!("REPL_SESSION_EXISTS: {}", dir.display());
        }
        for sub in [
            jrn::GUEST_HOME_DIR,
            jrn::GUEST_WORK_DIR,
            jrn::CELLS_DIR,
            jrn::REPLAYS_DIR,
        ] {
            fs::create_dir_all(dir.join(sub))?;
        }
        // Fresh empty journal so a later reopen/repair sees a well-formed file.
        fs::File::create(dir.join(jrn::JOURNAL_FILE))?.sync_all()?;
        fs::File::open(&dir)?.sync_all()?;
        // Mount sources must be canonical for shenko-wasm admission.
        let dir = dir.canonicalize()?;

        let manifest = SessionManifest {
            schema_version: MANIFEST_SCHEMA,
            session_id,
            kernel: request.kernel,
            module_blake3,
            bind: request.bind,
            limits: request.limits,
            environment_generation: kernel::environment_generation(&request.kernel),
            recovery_level: RecoveryLevel::R0WorkRecord,
            forked_from: None,
            created_unix_ms: unix_ms(),
            disposed_unix_ms: None,
        };
        jrn::write_manifest(&dir, &manifest)?;
        let journal = jrn::load(&dir)?;
        let runtime = build_runtime(&dir, &manifest.kernel)?;
        Ok(WasiReplSession {
            dir,
            manifest,
            runtime,
            journal,
            busy: None,
            disposed: false,
            preview_bytes: preview_len(request.preview_bytes),
        })
    }

    fn reopen(&self, session_dir: &Path) -> Result<WasiReplSession> {
        open_session(session_dir, DEFAULT_PREVIEW_BYTES)
    }
}

fn preview_len(requested: usize) -> usize {
    if requested == 0 { DEFAULT_PREVIEW_BYTES } else { requested }
}

/// Reopen an existing session directory: manifest + journal reloaded, mounts
/// rebuilt. Does not replay anything — recovery is `WasiReplProvider::recover`.
pub fn open_session(dir: &Path, preview_bytes: usize) -> Result<WasiReplSession> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("REPL_SESSION_INVALID: cannot resolve {}", dir.display()))?;
    let manifest: SessionManifest = jrn::read_manifest(&dir)?;
    if manifest.schema_version != MANIFEST_SCHEMA {
        bail!(
            "REPL_SESSION_INVALID: manifest schema {} (supported {MANIFEST_SCHEMA})",
            manifest.schema_version
        );
    }
    let journal = jrn::load(&dir)?;
    let runtime = build_runtime(&dir, &manifest.kernel)?;
    Ok(WasiReplSession {
        dir,
        busy: None,
        disposed: manifest.disposed_unix_ms.is_some(),
        preview_bytes: preview_len(preview_bytes),
        manifest,
        runtime,
        journal,
    })
}

/// The guest-visible host directory a kernel's workdir maps to.
fn host_work_root(dir: &Path, kernel: &KernelProfile) -> Result<PathBuf> {
    if kernel.workdir_guest == HOME_GUEST_PATH {
        return Ok(dir.join(jrn::GUEST_HOME_DIR));
    }
    if let Some(mount) = kernel
        .extra_mounts
        .iter()
        .find(|m| m.guest_path == kernel.workdir_guest)
    {
        return Ok(mount.source.clone());
    }
    Ok(dir.join(jrn::GUEST_WORK_DIR))
}

fn build_runtime(dir: &Path, kernel: &KernelProfile) -> Result<WasmRuntime> {
    let mut mounts = vec![MountSpec::new(
        dir.join(jrn::GUEST_HOME_DIR),
        HOME_GUEST_PATH,
        MountMode::ReadWrite,
        true,
        false,
    )];
    if kernel.workdir_guest != HOME_GUEST_PATH
        && !kernel
            .extra_mounts
            .iter()
            .any(|m| m.guest_path == kernel.workdir_guest)
    {
        mounts.push(MountSpec::new(
            dir.join(jrn::GUEST_WORK_DIR),
            kernel.workdir_guest.clone(),
            MountMode::ReadWrite,
            true,
            false,
        ));
    }
    mounts.extend(kernel.extra_mounts.iter().cloned());
    WasmRuntime::with_mounts(mounts)
}

/// A live REPL session on the real WASI backend.
pub struct WasiReplSession {
    dir: PathBuf,
    manifest: SessionManifest,
    runtime: WasmRuntime,
    journal: jrn::Journal,
    /// Cell currently executing under this handle (bookkeeping for `cancel`;
    /// a synchronous caller cannot observe Busy).
    busy: Option<EntityId>,
    disposed: bool,
    preview_bytes: usize,
}

impl WasiReplSession {
    /// Re-read and re-hash the module for this cell. A swapped file fails
    /// binding instead of executing a different interpreter.
    fn bound_module(&self) -> Result<Vec<u8>> {
        let ModuleSource::ModulePath(path) = &self.manifest.kernel.module;
        let bytes = fs::read(path).with_context(|| {
            format!(
                "REPL_KERNEL_UNAVAILABLE: interpreter module {} unreadable",
                path.display()
            )
        })?;
        let actual = jrn::hash_bytes(&bytes);
        if actual != self.manifest.module_blake3 {
            bail!(
                "REPL_KERNEL_UNAVAILABLE: module {} changed (bound {}, now {actual})",
                path.display(),
                self.manifest.module_blake3
            );
        }
        Ok(bytes)
    }

    /// One cell as one real WASI command. Replay output files land under
    /// `replays/`, normal cells under `cells/`.
    fn run_guest(
        &mut self,
        cell: EntityId,
        source: &[u8],
        file_stem: &str,
        file_dir: &str,
    ) -> Result<(CellOutcome, PathBuf, PathBuf, u64)> {
        let cell_dir = self.dir.join(file_dir);
        let source_path = cell_dir.join(format!("{file_stem}.source"));
        let stdout_path = cell_dir.join(format!("{file_stem}.stdout"));
        let stderr_path = cell_dir.join(format!("{file_stem}.stderr"));

        let module = self.bound_module()?;
        jrn::write_fresh_file(&source_path, source)?;

        let stdin = InputFile::new(
            fs::File::open(&source_path)
                .with_context(|| format!("open {}", source_path.display()))?,
        );
        let stdout = OutputFile::new(
            fs::File::create_new(&stdout_path)
                .with_context(|| format!("create {}", stdout_path.display()))?,
        );
        let stderr = OutputFile::new(
            fs::File::create_new(&stderr_path)
                .with_context(|| format!("create {}", stderr_path.display()))?,
        );
        let mut command = WasiCommand::new(stdin, stdout, stderr);
        command.arguments = self.manifest.kernel.argv.clone();
        command.environment = self.manifest.kernel.env.clone();

        self.busy = Some(cell);
        let started = Instant::now();
        let result = self.runtime.run_wasi_module(&module, command);
        self.busy = None;
        let outcome = match result {
            Ok(0) => CellOutcome::Completed { exit_code: 0 },
            Ok(code) => CellOutcome::Exited { exit_code: code },
            // Real failure text; pre-trap stream bytes remain in the files.
            Err(e) => CellOutcome::Failed {
                error: e.to_string(),
            },
        };
        Ok((
            outcome,
            stdout_path,
            stderr_path,
            started.elapsed().as_millis() as u64,
        ))
    }

    fn stream_preview(&self, path: &Path) -> Result<StreamPreview> {
        let meta = fs::metadata(path)?;
        let file = fs::File::open(path)?;
        let mut limited = file.take(self.preview_bytes as u64 + 1);
        let mut buf = Vec::new();
        limited.read_to_end(&mut buf)?;
        let truncated = buf.len() > self.preview_bytes;
        buf.truncate(self.preview_bytes);
        Ok(StreamPreview {
            text: String::from_utf8_lossy(&buf).into_owned(),
            total_bytes: meta.len(),
            truncated,
            blake3: jrn::hash_file(path)?,
            path: path.strip_prefix(&self.dir).unwrap_or(path).to_path_buf(),
        })
    }

    /// Re-execute one journaled Pure cell into `replays/`, then journal the
    /// replay fact. Only called by `restore`/`recover` after source-hash
    /// verification.
    fn replay_cell(&mut self, record: &CellRecord, replay_seq: u64) -> Result<u64> {
        let source_path = self.dir.join(&record.source_path);
        let source = fs::read(&source_path)?;
        let stem = format!("{:06}", replay_seq);
        let (outcome, stdout_path, stderr_path, _) =
            self.run_guest(record.cell, &source, &stem, jrn::REPLAYS_DIR)?;
        let (exit_code, error) = match &outcome {
            CellOutcome::Completed { exit_code } | CellOutcome::Exited { exit_code } => {
                (Some(*exit_code), None)
            }
            CellOutcome::Failed { error } => (None, Some(error.clone())),
        };
        jrn::append_replay(
            &mut self.journal,
            jrn::ReplayRecord {
                kind: String::new(),
                schema_version: 0,
                session_id: self.manifest.session_id,
                seq: 0,
                cell: record.cell,
                cell_seq: record.seq,
                stdout_path: stdout_path
                    .strip_prefix(&self.dir)
                    .unwrap_or(&stdout_path)
                    .to_path_buf(),
                stderr_path: stderr_path
                    .strip_prefix(&self.dir)
                    .unwrap_or(&stderr_path)
                    .to_path_buf(),
                exit_code,
                error,
                completed_unix_ms: unix_ms(),
            },
        )
    }

    /// Host path of the kernel's declared interpreter-state file, if any.
    fn state_file_host_path(&self) -> Result<Option<PathBuf>> {
        match &self.manifest.kernel.persistence {
            StatePersistence::None => Ok(None),
            StatePersistence::GuestFile { guest_path } => {
                let root = host_work_root(&self.dir, &self.manifest.kernel)?;
                let rel = guest_path
                    .strip_prefix(&self.manifest.kernel.workdir_guest)
                    .unwrap_or("")
                    .trim_start_matches('/');
                if rel.is_empty() || rel.split('/').any(|c| c == "..") {
                    bail!("REPL_KERNEL_INVALID: state path {guest_path} escapes workdir");
                }
                Ok(Some(root.join(rel)))
            }
        }
    }
}

impl ReplSession for WasiReplSession {
    fn execute(&mut self, request: CellRequest) -> Result<CellResult> {
        if self.disposed {
            bail!("REPL_SESSION_DISPOSED: session is closed");
        }
        let cell = EntityId::new();
        let stem = cell.to_string();
        let (outcome, stdout_path, stderr_path, wall_clock_ms) =
            self.run_guest(cell, &request.source, &stem, jrn::CELLS_DIR)?;
        let stdout_meta = fs::metadata(&stdout_path)?;
        let stderr_meta = fs::metadata(&stderr_path)?;
        let record = CellRecord {
            kind: String::new(),
            schema_version: 0,
            session_id: self.manifest.session_id,
            seq: self.journal.next_cell_seq,
            cell,
            cell_kind: request.cell_kind,
            source_blake3: jrn::hash_bytes(&request.source),
            source_bytes: request.source.len() as u64,
            source_path: PathBuf::from(jrn::CELLS_DIR).join(format!("{stem}.source")),
            outcome,
            stdout_blake3: jrn::hash_file(&stdout_path)?,
            stdout_bytes: stdout_meta.len(),
            stderr_blake3: jrn::hash_file(&stderr_path)?,
            stderr_bytes: stderr_meta.len(),
            stdout_path: PathBuf::from(jrn::CELLS_DIR).join(format!("{stem}.stdout")),
            stderr_path: PathBuf::from(jrn::CELLS_DIR).join(format!("{stem}.stderr")),
            module_blake3: self.manifest.module_blake3.clone(),
            environment_generation: self.manifest.environment_generation.clone(),
            wall_clock_ms,
            completed_unix_ms: unix_ms(),
        };
        let stdout = self.stream_preview(&stdout_path)?;
        let stderr = self.stream_preview(&stderr_path)?;
        // The durable record is appended last: a crash between run and append
        // leaves orphan files, not a forged record.
        let record = jrn::append_cell(&mut self.journal, record)?;
        Ok(CellResult {
            record,
            stdout,
            stderr,
        })
    }

    fn inspect(&self) -> Result<SessionInfo> {
        Ok(SessionInfo {
            session_id: self.manifest.session_id,
            kernel_id: self.manifest.kernel.kernel_id.clone(),
            runtime: self.manifest.kernel.runtime.clone(),
            backend: self.manifest.kernel.backend,
            state: if self.disposed {
                SessionState::Disposed
            } else if self.busy.is_some() {
                SessionState::Busy
            } else {
                SessionState::Ready
            },
            recovery_level: self.manifest.recovery_level,
            interpreter_state_persists: false,
            cells_executed: self.journal.cells.len() as u64,
            environment_generation: self.manifest.environment_generation.clone(),
            bind: self.manifest.bind.clone(),
            limits: self.manifest.limits.clone(),
        })
    }

    fn checkpoint(&mut self) -> Result<ReplCheckpoint> {
        let mut state_files = std::collections::BTreeMap::new();
        if let Some(host_path) = self.state_file_host_path()? {
            if host_path.exists() {
                let rel = host_path
                    .strip_prefix(&self.dir)
                    .unwrap_or(&host_path)
                    .to_path_buf();
                state_files.insert(
                    rel.to_string_lossy().into_owned(),
                    jrn::hash_file(&host_path)?,
                );
            }
        }
        Ok(ReplCheckpoint {
            checkpoint: EntityId::new(),
            session_id: self.manifest.session_id,
            level: RecoveryLevel::R0WorkRecord,
            journal_blake3: jrn::hash_file(&self.dir.join(jrn::JOURNAL_FILE))?,
            cells: self.journal.cells.len() as u64,
            state_files,
            created_unix_ms: unix_ms(),
        })
    }

    /// Rebuild interpreter state by replaying journaled Pure cells.
    /// Declared-effect cells are receipts, never re-executed (A15).
    fn restore(&mut self) -> Result<RecoverReport> {
        // Capture before replays: appending a replay repairs the tail.
        let torn_tail_at = self.journal.torn_tail_at;
        let cells = self.journal.cells.clone();
        let mut entries = Vec::new();
        for record in &cells {
            // Verify the durable source bytes before trusting the record.
            let source_path = self.dir.join(&record.source_path);
            let verified = jrn::hash_file(&source_path)
                .map(|h| h == record.source_blake3)
                .unwrap_or(false);
            if !verified {
                entries.push(RecoverEntry {
                    cell: record.cell,
                    seq: record.seq,
                    cell_kind: record.cell_kind,
                    recovery: CellRecovery::Corrupt {
                        detail: "source file missing or blake3 mismatch".into(),
                    },
                });
                continue;
            }
            match record.cell_kind {
                CellKind::Pure => {
                    let seq = self.journal.next_replay_seq;
                    let replayed_as = self.replay_cell(record, seq)?;
                    entries.push(RecoverEntry {
                        cell: record.cell,
                        seq: record.seq,
                        cell_kind: record.cell_kind,
                        recovery: CellRecovery::Replayed { replayed_as },
                    });
                }
                CellKind::DeclaredEffect => entries.push(RecoverEntry {
                    cell: record.cell,
                    seq: record.seq,
                    cell_kind: record.cell_kind,
                    recovery: CellRecovery::ReceiptOnly,
                }),
                CellKind::Unknown => entries.push(RecoverEntry {
                    cell: record.cell,
                    seq: record.seq,
                    cell_kind: record.cell_kind,
                    recovery: CellRecovery::NeedsReconciliation,
                }),
            }
        }
        Ok(RecoverReport {
            session_id: self.manifest.session_id,
            entries,
            torn_tail_at,
        })
    }

    fn fork(&self, sessions_root: &Path) -> Result<Box<dyn ReplSession>> {
        if self.disposed {
            bail!("REPL_SESSION_DISPOSED: cannot fork a closed session");
        }
        fs::create_dir_all(sessions_root)?;
        let root = sessions_root.canonicalize()?;
        let new_id = EntityId::new();
        let dir = root.join(new_id.to_string());
        for sub in [jrn::CELLS_DIR, jrn::REPLAYS_DIR] {
            fs::create_dir_all(dir.join(sub))?;
        }
        // Copy guest-visible state only: home and the session-local workdir.
        // Extra mounts are referenced, not copied — their ownership stays
        // with the caller. The fork gets a fresh journal: its own R0 record.
        jrn::copy_dir(
            &self.dir.join(jrn::GUEST_HOME_DIR),
            &dir.join(jrn::GUEST_HOME_DIR),
        )?;
        let work = self.dir.join(jrn::GUEST_WORK_DIR);
        if work.exists() {
            jrn::copy_dir(&work, &dir.join(jrn::GUEST_WORK_DIR))?;
        } else {
            fs::create_dir_all(dir.join(jrn::GUEST_WORK_DIR))?;
        }
        fs::File::create(dir.join(jrn::JOURNAL_FILE))?.sync_all()?;
        let dir = dir.canonicalize()?;
        let manifest = SessionManifest {
            forked_from: Some(self.manifest.session_id),
            session_id: new_id,
            created_unix_ms: unix_ms(),
            disposed_unix_ms: None,
            ..self.manifest.clone()
        };
        jrn::write_manifest(&dir, &manifest)?;
        let journal = jrn::load(&dir)?;
        let runtime = build_runtime(&dir, &manifest.kernel)?;
        Ok(Box::new(WasiReplSession {
            dir,
            manifest,
            runtime,
            journal,
            busy: None,
            disposed: false,
            preview_bytes: self.preview_bytes,
        }))
    }

    fn cancel(&self) -> Result<CancelOutcome> {
        if self.disposed {
            return Ok(CancelOutcome::Disposed);
        }
        // WASI preview1 execution has no interrupt mechanism in this runtime;
        // a running cell completes or the runtime errors — report, never claim.
        match self.busy {
            Some(cell) => Ok(CancelOutcome::NotInterruptible { cell }),
            None => Ok(CancelOutcome::NothingRunning),
        }
    }

    fn reset(&mut self) -> Result<()> {
        if self.disposed {
            bail!("REPL_SESSION_DISPOSED: session is closed");
        }
        match self.state_file_host_path()? {
            None => bail!(
                "REPL_KERNEL_UNIMPLEMENTED: kernel {} declares no interpreter state to reset",
                self.manifest.kernel.kernel_id
            ),
            Some(path) => {
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).context("remove interpreter state file"),
                }
                Ok(())
            }
        }
    }

    fn install_dependencies(&mut self, _packages: &[PackageSpec]) -> Result<InstallOutcome> {
        // R083 surface: no installer exists, and the environment generation
        // must not move without a real lock change — report, never fabricate.
        Ok(InstallOutcome::Unsupported {
            reason: format!(
                "no package installer is bound for backend {:?}; environment_generation stays {}",
                self.manifest.kernel.backend, self.manifest.environment_generation.0
            ),
        })
    }

    fn dispose(&mut self) -> Result<()> {
        if !self.disposed {
            self.disposed = true;
            self.manifest.disposed_unix_ms = Some(unix_ms());
            jrn::write_manifest(&self.dir, &self.manifest)?;
        }
        Ok(())
    }

    fn session_dir(&self) -> &Path {
        &self.dir
    }
}
