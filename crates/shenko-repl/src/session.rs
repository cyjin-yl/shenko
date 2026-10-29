//! WASI preview1 cell execution with host-only R0 work records. This is a
//! process per cell, not a claim of interpreter or VM object persistence.

use crate::contract::*;
use crate::journal as jrn;
use crate::kernel::{self, KernelProfile, ModuleSource, PackageSpec, StatePersistence};
pub use crate::state::Baseline;
use crate::state::StateFile;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_wasm::{HOME_GUEST_PATH, InputFile, MountMode, MountSpec, OutputFile, WasiCommand, WasmRuntime};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MANIFEST_SCHEMA: u32 = 2;
const DEFAULT_PREVIEW_BYTES: usize = 4096;

/// Host-only session identity and immutable initial interpreter-state baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub session_id: EntityId,
    pub kernel: KernelProfile,
    pub module_blake3: String,
    pub bind: TaskBinding,
    pub limits: ResourceLimits,
    pub environment_generation: shenko_types::ids::RuntimeGeneration,
    pub recovery_level: RecoveryLevel,
    pub forked_from: Option<EntityId>,
    pub baseline: Option<Baseline>,
    pub created_unix_ms: i64,
    pub disposed_unix_ms: Option<i64>,
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64).unwrap_or_default()
}
fn preview_len(requested: usize) -> usize {
    if requested == 0 { DEFAULT_PREVIEW_BYTES } else { requested }
}

pub struct WasiReplProvider { pub sessions_root: PathBuf }
impl WasiReplProvider {
    pub fn new(sessions_root: PathBuf) -> Self { Self { sessions_root } }

    fn canonical_root(&self) -> Result<PathBuf> {
        fs::create_dir_all(&self.sessions_root)?;
        let root = self.sessions_root.canonicalize().context("canonicalize sessions root")?;
        sync_directory_ancestors(&root)?;
        Ok(root)
    }

    pub fn recover(session_dir: &Path, preview_bytes: usize) -> Result<(WasiReplSession, RecoverReport)> {
        let mut session = open_session(session_dir, preview_bytes)?;
        let report = session.restore()?;
        Ok((session, report))
    }
}

impl ReplProvider for WasiReplProvider {
    type Session = WasiReplSession;
    fn kernels(&self) -> Vec<kernel::KernelTarget> { kernel::named_targets() }

    fn create(&self, request: CreateSession) -> Result<WasiReplSession> {
        kernel::validate_profile(&request.kernel)?;
        let expected_root = prospective_path(&self.sessions_root)?;
        let session_id = EntityId::new();
        let dir = expected_root.join(session_id.to_string());
        // Admit the complete namespace and effective state owner before even
        // creating the sessions root. An external owner is capability-opened
        // now; only the not-yet-existing private roots defer their open.
        let preopens = session_preopens(&dir, &request.kernel)?;
        let prepared_state = prepare_state(&preopens, &request.kernel.persistence)?;
        let ModuleSource::ModulePath(module_path) = &request.kernel.module;
        let module_bytes = fs::read(module_path).with_context(||
            format!("REPL_KERNEL_UNAVAILABLE: cannot read interpreter module {}", module_path.display()))?;
        let module_blake3 = jrn::hash_bytes(&module_bytes);
        let environment_generation = kernel::environment_generation(&request.kernel);
        let root = self.canonical_root()?;
        if root != expected_root {
            bail!("REPL_MOUNT_CONFLICT: sessions root changed after admission");
        }
        fs::create_dir(&dir).context("create session directory")?;
        for sub in [jrn::GUEST_HOME_DIR, jrn::GUEST_WORK_DIR, jrn::CELLS_DIR, jrn::REPLAYS_DIR] {
            fs::create_dir(dir.join(sub))?;
        }
        File::create(dir.join(jrn::JOURNAL_FILE))?.sync_all()?;
        File::open(&dir)?.sync_all()?;
        File::open(&root)?.sync_all()?;
        let dir = dir.canonicalize()?;
        let state = prepared_state.map(|state| state.bind(&preopens)).transpose()?;
        let baseline = match &state { Some(state) => state.file.snapshot(&dir)?, None => None };
        let mut limits = request.limits;
        limits.enforced = false;
        let manifest = SessionManifest {
            schema_version: MANIFEST_SCHEMA, session_id, kernel: request.kernel, module_blake3,
            bind: request.bind, limits, environment_generation,
            recovery_level: RecoveryLevel::R0WorkRecord, forked_from: None, baseline,
            created_unix_ms: unix_ms(), disposed_unix_ms: None,
        };
        let runtime = build_runtime(preopens)?;
        jrn::write_manifest(&dir, &manifest)?;
        let journal = load_journal(&dir, &manifest)?;
        Ok(WasiReplSession { dir, manifest, runtime, state, journal, busy: None,
            disposed: false, preview_bytes: preview_len(request.preview_bytes) })
    }

    fn reopen(&self, session_dir: &Path) -> Result<WasiReplSession> {
        open_session(session_dir, DEFAULT_PREVIEW_BYTES)
    }
}

fn validate_manifest(dir: &Path, manifest: &SessionManifest) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA || manifest.session_id == EntityId::nil()
        || dir.file_name().and_then(|n| n.to_str()) != Some(manifest.session_id.to_string().as_str())
        || !jrn::valid_digest(&manifest.module_blake3)
        || manifest.environment_generation != kernel::environment_generation(&manifest.kernel)
        || manifest.recovery_level != RecoveryLevel::R0WorkRecord || manifest.limits.enforced
        || manifest.forked_from == Some(EntityId::nil())
        || manifest.forked_from == Some(manifest.session_id)
        || manifest.baseline.as_ref().is_some_and(|b| !jrn::valid_digest(&b.blake3))
        || (manifest.baseline.is_some() && matches!(&manifest.kernel.persistence, StatePersistence::None))
    { bail!("REPL_SESSION_INVALID: manifest identity, schema, binding or limits mismatch"); }
    kernel::validate_profile(&manifest.kernel)?;
    Ok(())
}

fn load_journal(dir: &Path, manifest: &SessionManifest) -> Result<jrn::Journal> {
    jrn::load(dir, manifest.session_id, &manifest.module_blake3, &manifest.environment_generation)
}

fn read_manifest(dir: &Path) -> Result<SessionManifest> {
    let bytes = fs::read(dir.join(jrn::SESSION_MANIFEST)).context("REPL_SESSION_INVALID: missing manifest")?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).context("REPL_SESSION_INVALID: manifest JSON")?;
    fn keys(value: &serde_json::Value, expected: &[&str]) -> Result<()> {
        let object = value.as_object().context("REPL_SESSION_INVALID: expected object")?;
        if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
            bail!("REPL_SESSION_INVALID: missing or unknown manifest field");
        }
        Ok(())
    }
    keys(&json, &["schema_version", "session_id", "kernel", "module_blake3", "bind", "limits",
        "environment_generation", "recovery_level", "forked_from", "baseline", "created_unix_ms", "disposed_unix_ms"])?;
    keys(&json["kernel"], &["kernel_id", "runtime", "runtime_label", "backend", "module", "argv", "env",
        "workdir_guest", "persistence", "extra_mounts", "dependency_lock"])?;
    keys(&json["bind"], &["task", "worker", "scope"])?;
    keys(&json["limits"], &["max_wall_clock_ms", "max_fuel", "max_memory_bytes", "enforced"])?;
    if !json["baseline"].is_null() { keys(&json["baseline"], &["bytes", "blake3"])?; }
    let mounts = json.pointer("/kernel/extra_mounts").and_then(|v| v.as_array())
        .context("REPL_SESSION_INVALID: missing extra mounts")?;
    // MountSpec is shared with the WASM crate and intentionally permissive for
    // other consumers; the session manifest has a strict, versioned schema.
    for mount in mounts {
        keys(mount, &["source", "guest_path", "mode", "persistent", "versioned"])?;
    }
    serde_json::from_slice(&bytes).context("REPL_SESSION_INVALID: manifest schema")
}

/// Reopen without replay. An interrupted reset is finished before the handle
/// is returned; an interrupted restore fences execution until restore retries.
pub fn open_session(dir: &Path, preview_bytes: usize) -> Result<WasiReplSession> {
    let dir = dir.canonicalize().with_context(|| format!("REPL_SESSION_INVALID: {}", dir.display()))?;
    let manifest = read_manifest(&dir)?;
    validate_manifest(&dir, &manifest)?;
    let preopens = session_preopens(&dir, &manifest.kernel)?;
    let prepared_state = prepare_state(&preopens, &manifest.kernel.persistence)?;
    let journal = load_journal(&dir, &manifest)?;
    let state = prepared_state.map(|state| state.bind(&preopens)).transpose()?;
    let runtime = build_runtime(preopens)?;
    if journal.last_reset.is_some() && state.is_none() {
        bail!("REPL_JOURNAL_CORRUPT: reset boundary without declared state");
    }
    let mut session = WasiReplSession {
        dir, busy: None, disposed: manifest.disposed_unix_ms.is_some(),
        preview_bytes: preview_len(preview_bytes), manifest, runtime, state, journal,
    };
    if session.journal.pending_reset.is_some() {
        session.finish_reset()?;
    }
    Ok(session)
}

/// Extra preopens must neither contain nor be contained by private storage.
/// Sources must already be canonical, just as in the underlying WASI runtime.
fn validate_extra_mounts(dir: &Path, kernel: &KernelProfile) -> Result<()> {
    let mut guest_paths = HashSet::from([HOME_GUEST_PATH]);
    for mount in &kernel.extra_mounts {
        mount.validate()?;
        if !guest_paths.insert(mount.guest_path.as_str()) {
            bail!("REPL_MOUNT_CONFLICT: duplicate guest preopen {}", mount.guest_path);
        }
        let source = mount.source.canonicalize().context("canonicalize extra preopen")?;
        if source != mount.source || source.starts_with(dir) || dir.starts_with(&source) {
            bail!("REPL_MOUNT_CONFLICT: extra preopen {} overlaps private session storage {}",
                mount.source.display(), dir.display());
        }
    }
    Ok(())
}

/// The actual preopen list, shared by runtime construction and state admission.
/// `private` means the host source belongs to this session and can be forked.
struct SessionPreopen {
    spec: MountSpec,
    private: bool,
}

fn session_preopens(dir: &Path, kernel: &KernelProfile) -> Result<Vec<SessionPreopen>> {
    validate_extra_mounts(dir, kernel)?;
    let mut preopens = vec![SessionPreopen {
        spec: MountSpec::new(dir.join(jrn::GUEST_HOME_DIR), HOME_GUEST_PATH,
            MountMode::ReadWrite, true, false),
        private: true,
    }];
    if kernel.workdir_guest != HOME_GUEST_PATH
        && !kernel.extra_mounts.iter().any(|m| m.guest_path == kernel.workdir_guest) {
        preopens.push(SessionPreopen {
            spec: MountSpec::new(dir.join(jrn::GUEST_WORK_DIR), kernel.workdir_guest.clone(),
                MountMode::ReadWrite, true, false),
            private: true,
        });
    }
    preopens.extend(kernel.extra_mounts.iter().cloned()
        .map(|spec| SessionPreopen { spec, private: false }));
    Ok(preopens)
}

fn build_runtime(preopens: Vec<SessionPreopen>) -> Result<WasmRuntime> {
    let mounts = preopens.into_iter().map(|preopen| preopen.spec).collect();
    WasmRuntime::with_mounts(mounts)
}

struct StateBinding {
    file: StateFile,
    private: bool,
}

/// External state roots already exist and are capability-opened during
/// admission. Private roots are created only after that admission succeeds.
enum PreparedState {
    Private { preopen: usize, relative: PathBuf },
    External(StateFile),
}

impl PreparedState {
    fn bind(self, preopens: &[SessionPreopen]) -> Result<StateBinding> {
        match self {
            Self::Private { preopen, relative } => Ok(StateBinding {
                file: StateFile::new(&preopens[preopen].spec.source, relative)?,
                private: true,
            }),
            Self::External(file) => Ok(StateBinding { file, private: false }),
        }
    }
}

/// Select the most specific component-prefix owner in the SAME list later
/// consumed by build_runtime. A nested extra preopen shadows its covering
/// root; a lexical workdir prefix does not determine actual state ownership.
fn prepare_state(preopens: &[SessionPreopen], persistence: &StatePersistence)
    -> Result<Option<PreparedState>>
{
    let StatePersistence::GuestFile { guest_path } = persistence else { return Ok(None); };
    let (index, owner) = preopens.iter().enumerate()
        .filter(|(_, preopen)| Path::new(guest_path.as_str()).starts_with(&preopen.spec.guest_path))
        .max_by_key(|(_, preopen)| preopen.spec.guest_path.len())
        .context("REPL_KERNEL_INVALID: declared state is not covered by any preopen")?;
    let relative = kernel::state_relative_path(&owner.spec.guest_path, guest_path)?;
    if owner.spec.mode != MountMode::ReadWrite {
        bail!("REPL_KERNEL_INVALID: declared state requires a writable owning preopen");
    }
    if owner.private {
        Ok(Some(PreparedState::Private { preopen: index, relative }))
    } else {
        Ok(Some(PreparedState::External(StateFile::new(&owner.spec.source, relative)?)))
    }
}

/// Resolve the existing ancestor before creating a session or fork root.
fn prospective_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    if absolute.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("REPL_FORK_CONFLICT: parent components in fork root");
    }
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut base) => {
                for part in missing.iter().rev() { base.push(part); }
                return Ok(base);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().context("invalid fork root")?.to_os_string());
                ancestor = ancestor.parent().context("invalid fork root")?;
            }
            Err(error) => return Err(error).context("resolve fork root"),
        }
    }
}

/// Include newly created session-root names in the durability barrier.
fn sync_directory_ancestors(path: &Path) -> Result<()> {
    let mut current = path;
    loop {
        File::open(current)?.sync_all()?;
        match current.parent() {
            Some(parent) => current = parent,
            None => return Ok(()),
        }
    }
}

fn validate_fork_destination(source_session: &Path, destination: &Path) -> Result<()> {
    for source in [source_session.join(jrn::GUEST_HOME_DIR), source_session.join(jrn::GUEST_WORK_DIR)] {
        if destination.starts_with(&source) || source.starts_with(destination) {
            bail!("REPL_FORK_CONFLICT: destination overlaps copied state");
        }
    }
    Ok(())
}

pub struct WasiReplSession {
    dir: PathBuf,
    manifest: SessionManifest,
    runtime: WasmRuntime,
    state: Option<StateBinding>,
    journal: jrn::Journal,
    busy: Option<EntityId>,
    /// An immediate execution fence; disposal may still need persistence retry.
    disposed: bool,
    preview_bytes: usize,
}

impl WasiReplSession {
    fn ensure_ready(&self) -> Result<()> {
        if self.disposed { bail!("REPL_SESSION_DISPOSED: session is closed"); }
        if self.journal.pending_reset.is_some() || self.journal.active_restore.is_some() {
            bail!("REPL_RECOVERY_REQUIRED: finish the interrupted reset or restore");
        }
        Ok(())
    }

    fn finish_reset(&mut self) -> Result<()> {
        self.state.as_ref().context("REPL_SESSION_INVALID: reset without state file")?.file.clear()?;
        jrn::complete_reset(&mut self.journal)
    }

    fn bound_module(&self) -> Result<Vec<u8>> {
        let ModuleSource::ModulePath(path) = &self.manifest.kernel.module;
        let bytes = fs::read(path).with_context(|| format!("REPL_KERNEL_UNAVAILABLE: {} unreadable", path.display()))?;
        if jrn::hash_bytes(&bytes) != self.manifest.module_blake3 {
            bail!("REPL_KERNEL_UNAVAILABLE: module {} changed", path.display());
        }
        Ok(bytes)
    }

    fn run_guest(&mut self, cell: EntityId, source: &[u8], file_stem: &str, file_dir: &str)
        -> Result<(CellOutcome, PathBuf, PathBuf, u64)>
    {
        // Recheck before every invocation: the runtime reopens pathnames.
        validate_extra_mounts(&self.dir, &self.manifest.kernel)?;
        let cell_dir = self.dir.join(file_dir);
        let source_path = cell_dir.join(format!("{file_stem}.source"));
        let stdout_path = cell_dir.join(format!("{file_stem}.stdout"));
        let stderr_path = cell_dir.join(format!("{file_stem}.stderr"));
        let module = self.bound_module()?;
        let stdin = InputFile::new(jrn::write_fresh_file(&source_path, source)?);
        let stdout_file = File::create_new(&stdout_path)?;
        let stderr_file = File::create_new(&stderr_path)?;
        let stdout = OutputFile::new(stdout_file.try_clone()?);
        let stderr = OutputFile::new(stderr_file.try_clone()?);
        let mut command = WasiCommand::new(stdin, stdout, stderr);
        command.arguments = self.manifest.kernel.argv.clone();
        command.environment = self.manifest.kernel.env.clone();
        self.busy = Some(cell);
        let started = Instant::now();
        let result = self.runtime.run_wasi_module(&module, command);
        self.busy = None;
        // Output data and names must reach storage before any journal record.
        stdout_file.sync_all()?;
        stderr_file.sync_all()?;
        File::open(&cell_dir)?.sync_all()?;
        let outcome = match result {
            Ok(0) => CellOutcome::Completed { exit_code: 0 },
            Ok(code) => CellOutcome::Exited { exit_code: code },
            Err(e) => CellOutcome::Failed { error: e.to_string() },
        };
        Ok((outcome, stdout_path, stderr_path, started.elapsed().as_millis() as u64))
    }

    fn stream_preview(&self, path: &Path, blake3: String, total_bytes: u64) -> Result<StreamPreview> {
        let mut limited = File::open(path)?.take((self.preview_bytes as u64).saturating_add(1));
        let mut buf = Vec::new();
        limited.read_to_end(&mut buf)?;
        let truncated = buf.len() > self.preview_bytes;
        buf.truncate(self.preview_bytes);
        Ok(StreamPreview { text: String::from_utf8_lossy(&buf).into_owned(), total_bytes,
            truncated, blake3, path: path.strip_prefix(&self.dir)?.to_path_buf() })
    }

    fn replay_cell(&mut self, record: &CellRecord, source: &[u8], attempt: EntityId) -> Result<u64> {
        // UUID is independent of journal seq: an orphan from a crash cannot
        // collide with the next attempt, even when the last line was torn.
        let artifact = EntityId::new();
        let (outcome, stdout_path, stderr_path, _) =
            self.run_guest(record.cell, source, &artifact.to_string(), jrn::REPLAYS_DIR)?;
        let (exit_code, error) = match outcome {
            CellOutcome::Completed { exit_code } | CellOutcome::Exited { exit_code } => (Some(exit_code), None),
            CellOutcome::Failed { error } => (None, Some(error)),
        };
        jrn::append_replay(&mut self.journal, jrn::ReplayRecord {
            kind: String::new(), schema_version: 0, session_id: self.manifest.session_id, seq: 0,
            attempt, artifact, cell: record.cell, cell_seq: record.seq,
            stdout_path: stdout_path.strip_prefix(&self.dir)?.to_path_buf(),
            stderr_path: stderr_path.strip_prefix(&self.dir)?.to_path_buf(),
            exit_code, error, completed_unix_ms: unix_ms(),
        })
    }

    /// Verify source once and return the exact owned bytes for replay. All
    /// stream sizes and hashes are checked before even receipt-only admission.
    fn verified_source(&self, record: &CellRecord) -> Result<Vec<u8>> {
        let source = fs::read(self.dir.join(&record.source_path)).context("source missing")?;
        if source.len() as u64 != record.source_bytes || jrn::hash_bytes(&source) != record.source_blake3 {
            bail!("source bytes or digest mismatch");
        }
        for (path, expected_bytes, expected_digest) in [
            (&record.stdout_path, record.stdout_bytes, &record.stdout_blake3),
            (&record.stderr_path, record.stderr_bytes, &record.stderr_blake3),
        ] {
            let file = File::open(self.dir.join(path)).context("stream missing")?;
            if !file.metadata()?.is_file() { bail!("stream is not a regular file"); }
            let (digest, bytes) = jrn::hash_reader(file)?;
            if bytes != expected_bytes || digest != *expected_digest { bail!("stream bytes or digest mismatch"); }
        }
        Ok(source)
    }
}

impl ReplSession for WasiReplSession {
    fn execute(&mut self, request: CellRequest) -> Result<CellResult> {
        self.ensure_ready()?;
        let cell = EntityId::new();
        let stem = cell.to_string();
        let (outcome, stdout_path, stderr_path, wall_clock_ms) =
            self.run_guest(cell, &request.source, &stem, jrn::CELLS_DIR)?;
        let (stdout_blake3, stdout_bytes) = jrn::hash_reader(File::open(&stdout_path)?)?;
        let (stderr_blake3, stderr_bytes) = jrn::hash_reader(File::open(&stderr_path)?)?;
        let stdout = self.stream_preview(&stdout_path, stdout_blake3.clone(), stdout_bytes)?;
        let stderr = self.stream_preview(&stderr_path, stderr_blake3.clone(), stderr_bytes)?;
        let record = CellRecord {
            kind: String::new(), schema_version: 0, session_id: self.manifest.session_id,
            seq: self.journal.next_cell_seq, cell, cell_kind: request.cell_kind,
            source_blake3: jrn::hash_bytes(&request.source), source_bytes: request.source.len() as u64,
            source_path: PathBuf::from(jrn::CELLS_DIR).join(format!("{stem}.source")), outcome,
            stdout_blake3, stdout_bytes, stderr_blake3, stderr_bytes,
            stdout_path: PathBuf::from(jrn::CELLS_DIR).join(format!("{stem}.stdout")),
            stderr_path: PathBuf::from(jrn::CELLS_DIR).join(format!("{stem}.stderr")),
            module_blake3: self.manifest.module_blake3.clone(),
            environment_generation: self.manifest.environment_generation.clone(), wall_clock_ms, completed_unix_ms: unix_ms(),
        };
        let record = jrn::append_cell(&mut self.journal, record)?;
        Ok(CellResult { record, stdout, stderr })
    }

    fn inspect(&self) -> Result<SessionInfo> {
        Ok(SessionInfo { session_id: self.manifest.session_id, kernel_id: self.manifest.kernel.kernel_id.clone(),
            runtime: self.manifest.kernel.runtime.clone(), backend: self.manifest.kernel.backend,
            state: if self.disposed { SessionState::Disposed } else if self.busy.is_some() { SessionState::Busy }
                else if self.journal.active_restore.is_some() || self.journal.pending_reset.is_some() { SessionState::RecoveryRequired }
                else { SessionState::Ready },
            recovery_level: self.manifest.recovery_level, interpreter_state_persists: false,
            cells_executed: self.journal.cells.len() as u64,
            environment_generation: self.manifest.environment_generation.clone(), bind: self.manifest.bind.clone(),
            limits: self.manifest.limits.clone() })
    }

    fn checkpoint(&mut self) -> Result<ReplCheckpoint> {
        self.ensure_ready()?;
        let mut state_files = std::collections::BTreeMap::new();
        if let (Some(state), StatePersistence::GuestFile { guest_path }) = (&self.state, &self.manifest.kernel.persistence) {
            if let Some(digest) = state.file.digest()? { state_files.insert(guest_path.clone(), digest); }
        }
        Ok(ReplCheckpoint { checkpoint: EntityId::new(), session_id: self.manifest.session_id,
            level: RecoveryLevel::R0WorkRecord, journal_blake3: jrn::hash_file(&self.dir.join(jrn::JOURNAL_FILE))?,
            cells: self.journal.cells.len() as u64, state_files, created_unix_ms: unix_ms() })
    }

    fn restore(&mut self) -> Result<RecoverReport> {
        if self.disposed { bail!("REPL_SESSION_DISPOSED: session is closed"); }
        if self.journal.pending_reset.is_some() { self.finish_reset()?; }
        let torn_tail_at = self.journal.torn_tail_at;
        let cells = self.journal.cells.clone();
        let last_reset = self.journal.last_reset.clone();
        let attempt = jrn::begin_restore(&mut self.journal)?;
        if let Some(state) = &self.state {
            let baseline = if last_reset.is_some() { None } else { self.manifest.baseline.as_ref() };
            state.file.restore(&self.dir, baseline)?;
        }
        let mut entries = Vec::new();
        for record in &cells {
            let source = match self.verified_source(record) {
                Ok(source) => source,
                Err(error) => {
                    entries.push(RecoverEntry { cell: record.cell, seq: record.seq, cell_kind: record.cell_kind,
                        recovery: CellRecovery::Corrupt { detail: format!("{error:#}") } });
                    continue;
                }
            };
            let recovery = match record.cell_kind {
                CellKind::Pure => {
                    if let Some(reset) = last_reset.as_ref().filter(|reset| record.seq <= reset.after_cell_seq) {
                        CellRecovery::BeforeReset { reset: reset.id }
                    } else {
                        CellRecovery::Replayed { replayed_as: self.replay_cell(record, &source, attempt)? }
                    }
                }
                CellKind::DeclaredEffect => CellRecovery::ReceiptOnly,
                CellKind::Unknown => CellRecovery::NeedsReconciliation,
            };
            entries.push(RecoverEntry { cell: record.cell, seq: record.seq, cell_kind: record.cell_kind, recovery });
        }
        jrn::complete_restore(&mut self.journal, attempt)?;
        Ok(RecoverReport { session_id: self.manifest.session_id, entries, torn_tail_at })
    }

    fn fork(&self, sessions_root: &Path) -> Result<Box<dyn ReplSession>> {
        self.ensure_ready()?;
        if self.state.as_ref().is_some_and(|state| !state.private) {
            bail!("REPL_FORK_UNSUPPORTED: external state preopen cannot be isolated");
        }
        let root = prospective_path(sessions_root)?;
        let new_id = EntityId::new();
        let dir = root.join(new_id.to_string());
        validate_fork_destination(&self.dir, &dir)?;
        let preopens = session_preopens(&dir, &self.manifest.kernel)?;
        let prepared_state = prepare_state(&preopens, &self.manifest.kernel.persistence)?;
        fs::create_dir_all(&root)?;
        if root.canonicalize()? != root {
            bail!("REPL_MOUNT_CONFLICT: fork root changed after admission");
        }
        sync_directory_ancestors(&root)?;
        let dir = root.join(new_id.to_string());
        validate_fork_destination(&self.dir, &dir)?;
        validate_extra_mounts(&dir, &self.manifest.kernel)?;
        fs::create_dir(&dir)?;
        fs::create_dir(dir.join(jrn::CELLS_DIR))?;
        fs::create_dir(dir.join(jrn::REPLAYS_DIR))?;
        jrn::copy_dir(&self.dir.join(jrn::GUEST_HOME_DIR), &dir.join(jrn::GUEST_HOME_DIR))?;
        jrn::copy_dir(&self.dir.join(jrn::GUEST_WORK_DIR), &dir.join(jrn::GUEST_WORK_DIR))?;
        File::create(dir.join(jrn::JOURNAL_FILE))?.sync_all()?;
        File::open(&dir)?.sync_all()?;
        File::open(&root)?.sync_all()?;
        let dir = dir.canonicalize()?;
        let state = prepared_state.map(|state| state.bind(&preopens)).transpose()?;
        let baseline = match &state { Some(state) => state.file.snapshot(&dir)?, None => None };
        let manifest = SessionManifest { forked_from: Some(self.manifest.session_id), session_id: new_id,
            baseline, created_unix_ms: unix_ms(), disposed_unix_ms: None, ..self.manifest.clone() };
        let runtime = build_runtime(preopens)?;
        jrn::write_manifest(&dir, &manifest)?;
        let journal = load_journal(&dir, &manifest)?;
        Ok(Box::new(WasiReplSession { dir, manifest, runtime, state, journal, busy: None,
            disposed: false, preview_bytes: self.preview_bytes }))
    }

    fn cancel(&self) -> Result<CancelOutcome> {
        if self.disposed { return Ok(CancelOutcome::Disposed); }
        Ok(match self.busy { Some(cell) => CancelOutcome::NotInterruptible { cell }, None => CancelOutcome::NothingRunning })
    }

    fn reset(&mut self) -> Result<()> {
        if self.disposed { bail!("REPL_SESSION_DISPOSED: session is closed"); }
        if self.state.is_none() { bail!("REPL_KERNEL_UNIMPLEMENTED: no interpreter state declared"); }
        if self.journal.pending_reset.is_none() { jrn::begin_reset(&mut self.journal)?; }
        self.finish_reset()
    }

    fn install_dependencies(&mut self, _packages: &[PackageSpec]) -> Result<InstallOutcome> {
        Ok(InstallOutcome::Unsupported { reason: format!("no package installer is bound for backend {:?}; environment_generation stays {}",
            self.manifest.kernel.backend, self.manifest.environment_generation.0) })
    }

    fn dispose(&mut self) -> Result<()> {
        // Fence immediately, but retry persistence until a write succeeds.
        self.disposed = true;
        if self.manifest.disposed_unix_ms.is_none() {
            let mut updated = self.manifest.clone();
            updated.disposed_unix_ms = Some(unix_ms());
            jrn::write_manifest(&self.dir, &updated)?;
            self.manifest = updated;
        }
        Ok(())
    }

    fn session_dir(&self) -> &Path { &self.dir }
}
