//! Stateful REPL sessions over two real engine models.
//!
//! * Driver kernels ([`crate::kernel::KernelDriver`] != `None`) run a
//!   **live process** ([`crate::live`]): one long-lived owned interpreter
//!   process per session; variables, closures and imported modules persist in
//!   interpreter memory while it lives; recovery kills and rebuilds it from
//!   the durable journal.
//! * Raw kernels run **one-shot**: one independent WASI process per cell;
//!   the R0 journal plus replay of caller-declared `Pure` cells is the whole
//!   recovery story, and `inspect` says so.
//!
//! Everything here is honest about what is not implemented: resource fuel
//! and memory limits are declarations without a mechanism; R2 VM-state
//! recovery exists nowhere; interrupt success is never stop-proof.

use crate::contract::*;
use crate::journal as jrn;
use crate::kernel::{self, KernelProfile, ModuleSource, StatePersistence};
use crate::live::{self, CellHeader, LiveProcess};
pub use crate::state::Baseline;
use crate::state::StateFile;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_wasm::{HOME_GUEST_PATH, InputFile, MountMode, MountSpec, OutputFile, WasiCancellation, WasiCommand, WasiRunControl, WasiRunOutcome, WasmRuntime};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MANIFEST_SCHEMA: u32 = 3;
const DEFAULT_PREVIEW_BYTES: usize = 4096;
/// Guest-visible root of driver material, per-cell streams and environments.
const SHENKO_GUEST_ROOT: &str = "/home/.shenko";

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
    /// Advisory cache of the journal's current generation; the authoritative
    /// value is recomputed from the kernel lock plus committed `env` records.
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
        materialize_driver(&dir, &request.kernel)?;
        let cell_ns = cell_namespace(&dir, session_id);
        fs::create_dir_all(&cell_ns.host)?;
        fs::create_dir_all(dir.join("home/.shenko/replays"))?;
        fs::create_dir_all(env_dir_host(&dir, &generation_tag(&environment_generation)))?;
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
            recovery_level: request.kernel.recovery_level(), forked_from: None, baseline,
            created_unix_ms: unix_ms(), disposed_unix_ms: None,
        };
        let engine = build_engine(&dir, &manifest.kernel, preopens)?;
        jrn::write_manifest(&dir, &manifest)?;
        let journal = load_journal(&dir, &manifest)?;
        Ok(WasiReplSession { dir, manifest, engine, state, journal, busy: None,
            oneshot_cancel: None, disposed: false, preview_bytes: preview_len(request.preview_bytes) })
    }

    fn reopen(&self, session_dir: &Path) -> Result<WasiReplSession> {
        open_session(session_dir, DEFAULT_PREVIEW_BYTES)
    }
}

/// Driver material under the session home tree (guest-visible, host-written).
fn driver_host_dir(dir: &Path) -> PathBuf { dir.join("home/.shenko/drivers") }
fn driver_guest_path(profile: &KernelProfile) -> Option<String> {
    profile.driver.asset().map(|asset| format!("{SHENKO_GUEST_ROOT}/drivers/{}", asset.file_name))
}
fn driver_host_path(dir: &Path, profile: &KernelProfile) -> Option<PathBuf> {
    profile.driver.asset().map(|asset| driver_host_dir(dir).join(asset.file_name))
}

fn materialize_driver(dir: &Path, profile: &KernelProfile) -> Result<()> {
    let Some(asset) = profile.driver.asset() else { return Ok(()); };
    let drivers = driver_host_dir(dir);
    fs::create_dir_all(&drivers)?;
    jrn::write_fresh_file(&drivers.join(asset.file_name), asset.bytes)?;
    Ok(())
}

/// Verify the materialized driver still has the embedded bytes' digest.
fn verify_driver(dir: &Path, profile: &KernelProfile) -> Result<()> {
    let Some(asset) = profile.driver.asset() else { return Ok(()); };
    let path = driver_host_dir(dir).join(asset.file_name);
    let bytes = fs::read(&path).with_context(||
        format!("REPL_SESSION_INVALID: driver {} missing", path.display()))?;
    if jrn::hash_bytes(&bytes) != jrn::hash_bytes(asset.bytes) {
        bail!("REPL_SESSION_INVALID: driver {} was modified", path.display());
    }
    Ok(())
}

/// Per-session namespace for live per-cell stream files, so a forked session
/// never overwrites its parent's cell files.
struct CellNamespace { host: PathBuf, guest: String }

fn cell_namespace(dir: &Path, session_id: EntityId) -> CellNamespace {
    let guest = format!("{SHENKO_GUEST_ROOT}/cells/{session_id}");
    CellNamespace { host: dir.join("home/.shenko/cells").join(session_id.to_string()), guest }
}

/// Guest staging dir for live replay streams (under the mounted home tree).
fn live_replay_guest_dir() -> String { format!("{SHENKO_GUEST_ROOT}/replays") }

/// Environment directory for one generation tag (`g-<blake3-16>`).
fn generation_tag(generation: &shenko_types::ids::RuntimeGeneration) -> String {
    let hex = generation.0.rsplit(':').next().unwrap_or(&generation.0);
    let short: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).take(16).collect();
    format!("g-{short}")
}
fn env_dir_guest(generation: &shenko_types::ids::RuntimeGeneration) -> String {
    format!("{SHENKO_GUEST_ROOT}/envs/{}", generation_tag(generation))
}
fn env_dir_host(dir: &Path, tag: &str) -> PathBuf {
    dir.join("home/.shenko/envs").join(tag)
}

fn validate_manifest(dir: &Path, manifest: &SessionManifest) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA || manifest.session_id == EntityId::nil()
        || dir.file_name().and_then(|n| n.to_str()) != Some(manifest.session_id.to_string().as_str())
        || !jrn::valid_digest(&manifest.module_blake3)
        || manifest.recovery_level != manifest.kernel.recovery_level()
        || manifest.limits.enforced
        || manifest.forked_from == Some(EntityId::nil())
        || manifest.forked_from == Some(manifest.session_id)
        || manifest.baseline.as_ref().is_some_and(|b| !jrn::valid_digest(&b.blake3))
        || (manifest.baseline.is_some() && matches!(&manifest.kernel.persistence, StatePersistence::None))
    { bail!("REPL_SESSION_INVALID: manifest identity, schema, binding or limits mismatch"); }
    kernel::validate_profile(&manifest.kernel)?;
    Ok(())
}

/// True when the manifest carries the environment this session forked from or
/// installed. `manifest.environment_generation` is advisory; the journal's
/// generation is authoritative.
fn load_journal(dir: &Path, manifest: &SessionManifest) -> Result<jrn::Journal> {
    let journal = jrn::load(dir, manifest.session_id, &manifest.module_blake3, &manifest.environment_generation)?;
    reconcile_manifest_generation(dir, manifest, &journal)?;
    Ok(journal)
}

/// Crash window between committing an `env` journal record and persisting the
/// updated manifest: adopt the journal's generation (recomputed from the
/// kernel lock plus committed env packages) and rewrite the manifest. Both
/// orders are idempotent, so a repeated crash re-heals to the same content.
fn reconcile_manifest_generation(dir: &Path, manifest: &SessionManifest, journal: &jrn::Journal) -> Result<()> {
    if journal.generation() == &kernel::environment_generation(&manifest.kernel) {
        return Ok(());
    }
    let mut healed = manifest.kernel.clone();
    for env in &journal.envs {
        healed.dependency_lock.extend(env.packages.iter().cloned());
    }
    if kernel::environment_generation(&healed) != *journal.generation() {
        bail!("REPL_JOURNAL_CORRUPT: environment generation does not match kernel lock plus env records");
    }
    let mut updated = manifest.clone();
    updated.kernel = healed;
    updated.environment_generation = journal.generation().clone();
    jrn::write_manifest(dir, &updated)?;
    Ok(())
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
        "workdir_guest", "persistence", "extra_mounts", "dependency_lock", "driver"])?;
    keys(&json["bind"], &["task", "worker", "scope"])?;
    keys(&json["limits"], &["max_wall_clock_ms", "max_fuel", "max_memory_bytes", "enforced"])?;
    if !json["baseline"].is_null() { keys(&json["baseline"], &["bytes", "blake3"])?; }
    let mounts = json.pointer("/kernel/extra_mounts").and_then(|v| v.as_array())
        .context("REPL_SESSION_INVALID: missing extra mounts")?;
    // MountSpec is shared with the WASM crate and versioned there; the
    // session manifest requires the stable base keys and delegates value
    // admission to MountSpec's own strict serde + validate().
    let base_keys = ["source", "guest_path", "mode", "persistent", "versioned"];
    for mount in mounts {
        let object = mount.as_object().context("REPL_SESSION_INVALID: expected mount object")?;
        if base_keys.iter().any(|key| !object.contains_key(*key)) {
            bail!("REPL_SESSION_INVALID: missing mount base field");
        }
    }
    serde_json::from_slice(&bytes).context("REPL_SESSION_INVALID: manifest schema")
}

/// Reopen without replay. An interrupted reset is finished before the handle
/// is returned; an interrupted restore fences execution until restore retries.
pub fn open_session(dir: &Path, preview_bytes: usize) -> Result<WasiReplSession> {
    let dir = dir.canonicalize().with_context(|| format!("REPL_SESSION_INVALID: {}", dir.display()))?;
    let mut manifest = read_manifest(&dir)?;
    validate_manifest(&dir, &manifest)?;
    verify_driver(&dir, &manifest.kernel)?;
    let preopens = session_preopens(&dir, &manifest.kernel)?;
    let prepared_state = prepare_state(&preopens, &manifest.kernel.persistence)?;
    let journal = load_journal(&dir, &manifest)?;
    reconcile_healed_manifest(&dir, &mut manifest)?;
    let state = prepared_state.map(|state| state.bind(&preopens)).transpose()?;
    let engine = build_engine(&dir, &manifest.kernel, preopens)?;
    if journal.last_reset.is_some() && state.is_none() {
        bail!("REPL_JOURNAL_CORRUPT: reset boundary without declared state");
    }
    let mut session = WasiReplSession {
        dir, busy: None, oneshot_cancel: None, disposed: manifest.disposed_unix_ms.is_some(),
        preview_bytes: preview_len(preview_bytes), manifest, engine, state, journal,
    };
    if session.journal.pending_reset.is_some() {
        session.finish_reset()?;
    }
    Ok(session)
}

/// `reconcile_manifest_generation` heals on disk during load; pick the healed
/// manifest back up so the in-memory session matches what is durable.
fn reconcile_healed_manifest(dir: &Path, manifest: &mut SessionManifest) -> Result<()> {
    let bytes = fs::read(dir.join(jrn::SESSION_MANIFEST)).context("read healed manifest")?;
    *manifest = serde_json::from_slice(&bytes).context("parse healed manifest")?;
    Ok(())
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

/// One execution engine for this session: an independent runtime for
/// one-shot raw kernels, or the owned live process for driver kernels.
enum Engine {
    OneShot { runtime: WasmRuntime },
    Live(Option<LiveProcess>),
}

impl Engine {
    fn session_model(&self) -> SessionModel {
        match self {
            Engine::OneShot { .. } => SessionModel::OneShot,
            Engine::Live(_) => SessionModel::LiveProcess,
        }
    }
}

fn build_engine(dir: &Path, kernel: &KernelProfile, preopens: Vec<SessionPreopen>) -> Result<Engine> {
    match kernel.session_model() {
        SessionModel::OneShot => Ok(Engine::OneShot { runtime: build_runtime(preopens)? }),
        SessionModel::LiveProcess => {
            // Live processes start lazily on the first cell or restore: fork
            // and reopen must not pay interpreter boot for an idle session.
            let _ = (dir, kernel);
            Ok(Engine::Live(None))
        }
    }
}

fn build_runtime(preopens: Vec<SessionPreopen>) -> Result<WasmRuntime> {
    let mounts = preopens.into_iter().map(|preopen| preopen.spec).collect();
    WasmRuntime::with_mounts(mounts)
}

struct StateBinding {
    file: StateFile,
    private: bool,
}

impl StateBinding {
    fn verify_root(&self) -> Result<()> {
        if !self.private {
            self.file.verify_root()?;
        }
        Ok(())
    }
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
                file: StateFile::new(&preopens[preopen].spec.source, relative, false)?,
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
        // A covering writable host preopen lets the guest rename/recreate the
        // state root while our capability still names the former directory.
        if preopens.iter().any(|coverer| coverer.spec.mode == MountMode::ReadWrite
            && coverer.spec.source != owner.spec.source
            && owner.spec.source.starts_with(&coverer.spec.source)) {
            bail!("REPL_KERNEL_INVALID: writable preopen covers the external state root");
        }
        let file = StateFile::new(&owner.spec.source, relative, true)?;
        file.verify_root()?;
        Ok(Some(PreparedState::External(file)))
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
    engine: Engine,
    state: Option<StateBinding>,
    journal: jrn::Journal,
    busy: Option<EntityId>,
    oneshot_cancel: Option<WasiCancellation>,
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
        if let Engine::Live(Some(process)) = &self.engine {
            if process.is_dead() {
                bail!("REPL_RECOVERY_REQUIRED: the live interpreter process is gone; run restore() before more cells");
            }
        }
        Ok(())
    }

    fn finish_reset(&mut self) -> Result<()> {
        let state = self.state.as_ref().context("REPL_SESSION_INVALID: reset without state file")?;
        state.verify_root()?;
        state.file.clear()?;
        state.verify_root()?;
        if let Engine::Live(process) = &mut self.engine {
            // Reset must also kill the owned process: variables living in
            // interpreter memory would otherwise survive the reset.
            if let Some(process) = process.take() {
                process.shutdown();
            }
        }
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

    /// Current environment directory for the live protocol header.
    fn current_env(&self) -> (String, PathBuf) {
        let guest = env_dir_guest(self.journal.generation());
        let host = env_dir_host(&self.dir, &generation_tag(self.journal.generation()));
        (guest, host)
    }

    /// Materialized cell protocol paths for one live cell.
    fn live_cell_paths(&self, seq: u64) -> LiveCellPaths {
        let (env_guest, env_host) = self.current_env();
        let guest = self.manifest.kernel.backend == BackendKind::WasiPreview1;
        let cell_namespace = cell_namespace(&self.dir, self.manifest.session_id);
        LiveCellPaths {
            header: CellHeader {
                seq,
                cell_dir: if guest { cell_namespace.guest.clone() } else { cell_namespace.host.to_string_lossy().into_owned() },
                state_path: self.state_guest_or_host(guest),
                env_dir: if guest { env_guest } else { env_host.to_string_lossy().into_owned() },
            },
            out_host: cell_namespace.host.join(format!("{seq}.out")),
            err_host: cell_namespace.host.join(format!("{seq}.err")),
            report_host: cell_namespace.host.join(format!("{seq}.report.json")),
        }
    }

    fn state_guest_or_host(&self, guest: bool) -> String {
        match &self.manifest.kernel.persistence {
            StatePersistence::GuestFile { guest_path } if guest => guest_path.clone(),
            StatePersistence::GuestFile { guest_path } => {
                // Host path: the state file's owning preopen is always the
                // session home tree for driver kernels (validated).
                self.dir.join(guest_path.trim_start_matches('/')).to_string_lossy().into_owned()
            }
            StatePersistence::None => String::new(),
        }
    }

    fn start_live_process(&mut self) -> Result<()> {
        if let Engine::Live(Some(process)) = &self.engine {
            if !process.is_dead() {
                return Ok(());
            }
        }
        let module = self.bound_module()?;
        verify_driver(&self.dir, &self.manifest.kernel)?;
        let preopens = session_preopens(&self.dir, &self.manifest.kernel)?;
        let Some(driver_guest) = driver_guest_path(&self.manifest.kernel) else {
            bail!("REPL_KERNEL_INVALID: live session model requires a driver");
        };
        let driver_host = driver_host_path(&self.dir, &self.manifest.kernel)
            .context("REPL_KERNEL_INVALID: live session model requires a driver")?;
        let guest_stdout = live::open_guest_log(&self.dir.join("guest.log"))?;
        let guest_stderr = live::open_guest_log(&self.dir.join("guest.err.log"))?;
        let process = match self.manifest.kernel.backend {
            BackendKind::WasiPreview1 => {
                let mut argv = self.manifest.kernel.argv.clone();
                if argv.is_empty() {
                    argv.push("kernel.wasm".into());
                }
                argv.push(driver_guest);
                let runtime = build_runtime(preopens)?;
                LiveProcess::spawn_wasi(runtime, module, argv, self.manifest.kernel.env.clone(),
                    guest_stdout, guest_stderr)?
            }
            BackendKind::NativeHost => {
                let ModuleSource::ModulePath(executable) = &self.manifest.kernel.module;
                let workdir_host = preopens.iter()
                    .find(|preopen| preopen.spec.guest_path == self.manifest.kernel.workdir_guest)
                    .map(|preopen| preopen.spec.source.clone())
                    .context("REPL_KERNEL_INVALID: native workdir has no host preopen")?;
                LiveProcess::spawn_native(executable.clone(), driver_host,
                    self.manifest.kernel.argv.clone(), self.manifest.kernel.env.clone(),
                    workdir_host, guest_stdout, guest_stderr)?
            }
            other => bail!("REPL_KERNEL_INVALID: backend {other:?} cannot host a live session"),
        };
        self.engine = Engine::Live(Some(process));
        Ok(())
    }

    fn stop_live_process(&mut self) -> Option<live::ProcessOutcome> {
        match &mut self.engine {
            Engine::Live(process) => process.take().and_then(|process| process.shutdown()),
            Engine::OneShot { .. } => None,
        }
    }

    // -----------------------------------------------------------------------
    // One-shot raw-kernel execution (one real WASI process per cell).
    // -----------------------------------------------------------------------

    fn run_oneshot_cell(&mut self, cell: EntityId, source: &[u8], file_stem: &str, file_dir: &str)
        -> Result<(CellOutcome, Option<DriverReport>, PathBuf, PathBuf, u64)>
    {
        let Engine::OneShot { runtime } = &mut self.engine else {
            bail!("REPL_KERNEL_INVALID: one-shot engine requested for a live kernel");
        };
        // Recheck before every invocation: the runtime reopens pathnames.
        validate_extra_mounts(&self.dir, &self.manifest.kernel)?;
        if let Some(state) = &self.state { state.verify_root()?; }
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
        let cancel = WasiCancellation::default();
        command.control = WasiRunControl {
            deadline: self.manifest.limits.max_wall_clock_ms.map(Duration::from_millis),
            cancel: Some(cancel.clone()),
        };
        self.oneshot_cancel = Some(cancel);
        self.busy = Some(cell);
        let started = Instant::now();
        let result = runtime.run_wasi_module(&module, command);
        self.busy = None;
        self.oneshot_cancel = None;
        // Output data and names must reach storage before any journal record.
        stdout_file.sync_all()?;
        stderr_file.sync_all()?;
        File::open(&cell_dir)?.sync_all()?;
        if let Some(state) = &self.state { state.verify_root()?; }
        let outcome = match result {
            Ok(WasiRunOutcome::Exited(0)) => CellOutcome::Completed { exit_code: 0 },
            Ok(WasiRunOutcome::Exited(code)) => CellOutcome::Exited { exit_code: code },
            Ok(WasiRunOutcome::Cancelled) => CellOutcome::Cancelled,
            Ok(WasiRunOutcome::DeadlineExceeded) => CellOutcome::DeadlineExceeded,
            Err(e) => CellOutcome::Failed { error: e.to_string() },
        };
        Ok((outcome, None, stdout_path, stderr_path, started.elapsed().as_millis() as u64))
    }

    // -----------------------------------------------------------------------
    // Live driver-kernel execution (one long-lived owned process).
    // -----------------------------------------------------------------------

    fn run_live_cell(&mut self, cell: EntityId, seq: u64, source: &[u8])
        -> Result<(CellOutcome, Option<DriverReport>, u64)>
    {
        self.start_live_process()?;
        let Engine::Live(Some(process)) = &mut self.engine else {
            bail!("REPL_KERNEL_INVALID: live engine unavailable");
        };
        if let Some(state) = &self.state { state.verify_root()?; }
        let paths = self.live_cell_paths(seq);
        self.busy = Some(cell);
        let started = Instant::now();
        let run = process.run_cell(&paths.header, source, &paths.report_host,
            self.manifest.limits.max_wall_clock_ms.map(Duration::from_millis));
        self.busy = None;
        let wall_clock_ms = started.elapsed().as_millis() as u64;
        let outcome = match run {
            Ok(()) => {
                let report = live::read_driver_report(&paths.report_host)?
                    .context("REPL_KERNEL_UNAVAILABLE: driver report missing after completion")?;
                if report.ok {
                    CellOutcome::Completed { exit_code: 0 }
                } else {
                    CellOutcome::Exited { exit_code: 1 }
                }
            }
            Err(live::CellInterrupt::CancelRequested) => CellOutcome::Cancelled,
            Err(live::CellInterrupt::DeadlineExceeded) => CellOutcome::DeadlineExceeded,
        };
        if let Some(state) = &self.state { state.verify_root()?; }
        Ok((outcome, report_of(&paths, outcome.clone()), wall_clock_ms))
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

    /// Re-execute one journaled Pure cell through the current engine during
    /// restore, with a fresh artifact identity, and append the replay record.
    fn replay_cell(&mut self, record: &CellRecord, source: &[u8], attempt: EntityId) -> Result<u64> {
        // UUID is independent of journal seq: an orphan from a crash cannot
        // collide with the next attempt, even when the last line was torn.
        let artifact = EntityId::new();
        let (exit_code, error) = match self.engine.session_model() {
            SessionModel::OneShot => {
                let (outcome, _, stdout_path, stderr_path, _) =
                    self.run_oneshot_cell(record.cell, source, &artifact.to_string(), jrn::REPLAYS_DIR)?;
                move_replay_streams(&stdout_path, &stderr_path, &self.dir, artifact)?;
                match outcome {
                    CellOutcome::Completed { exit_code } | CellOutcome::Exited { exit_code } => (Some(exit_code), None),
                    CellOutcome::Cancelled => (None, Some("replay guest cancelled".into())),
                    CellOutcome::DeadlineExceeded => (None, Some("replay guest deadline exceeded".into())),
                    CellOutcome::Failed { error } => (None, Some(error)),
                }
            }
            SessionModel::LiveProcess => {
                self.start_live_process()?;
                let Engine::Live(Some(process)) = &mut self.engine else {
                    bail!("REPL_KERNEL_INVALID: live engine unavailable");
                };
                // Replay evidence goes to artifact-named files under the
                // host replays/ dir, never over the original per-seq cell
                // files. The guest only sees /home, so the driver writes
                // per-seq streams to a guest staging dir the host renames.
                let staging = self.dir.join("home/.shenko/replays");
                let mut header = self.live_cell_paths(record.seq).header;
                header.cell_dir = live_replay_guest_dir();
                if self.manifest.kernel.backend == BackendKind::NativeHost {
                    // Native drivers take host paths verbatim.
                    header.cell_dir = staging.to_string_lossy().into_owned();
                }
                // The driver names the report <cell_dir>/<seq>.report.json;
                // poll that exact path, then move it to the artifact-named
                // audit location with the streams.
                let report_host = staging.join(format!("{}.report.json", record.seq));
                let run = process.run_cell(&header, source, &report_host,
                    self.manifest.limits.max_wall_clock_ms.map(Duration::from_millis));
                let stdout_host = staging.join(format!("{}.out", record.seq));
                let stderr_host = staging.join(format!("{}.err", record.seq));
                move_replay_streams(&stdout_host, &stderr_host, &self.dir, artifact)?;
                match run {
                    Ok(()) => match live::read_driver_report(&report_host)? {
                        Some(report) if report.ok => (Some(0), None),
                        Some(report) => (None, report.error.or_else(|| Some("replayed cell raised".into()))),
                        None => (None, Some("replay driver report missing".into())),
                    },
                    Err(live::CellInterrupt::CancelRequested) => (None, Some("replay interrupted".into())),
                    Err(live::CellInterrupt::DeadlineExceeded) => (None, Some("replay deadline exceeded".into())),
                }
            }
        };
        jrn::append_replay(&mut self.journal, jrn::ReplayRecord {
            kind: String::new(), schema_version: 0, session_id: self.manifest.session_id, seq: 0,
            attempt, artifact, cell: record.cell, cell_seq: record.seq,
            stdout_path: jrn::replay_path(artifact, "stdout"),
            stderr_path: jrn::replay_path(artifact, "stderr"),
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

struct LiveCellPaths {
    header: CellHeader,
    out_host: PathBuf,
    err_host: PathBuf,
    report_host: PathBuf,
}

fn report_of(paths: &LiveCellPaths, outcome: CellOutcome) -> Option<DriverReport> {
    if matches!(outcome, CellOutcome::Cancelled | CellOutcome::DeadlineExceeded) {
        return None;
    }
    live::read_driver_report(&paths.report_host).ok().flatten()
}

/// Move guest-written per-seq replay streams to artifact-named replay files
/// and make the rename durable.
fn move_replay_streams(stdout: &Path, stderr: &Path, dir: &Path, artifact: EntityId) -> Result<()> {
    let target_out = dir.join(jrn::replay_path(artifact, "stdout"));
    let target_err = dir.join(jrn::replay_path(artifact, "stderr"));
    if stdout.exists() {
        fs::rename(stdout, &target_out)?;
    } else {
        File::create_new(&target_out)?;
    }
    if stderr.exists() {
        fs::rename(stderr, &target_err)?;
    } else {
        File::create_new(&target_err)?;
    }
    File::open(dir.join(jrn::REPLAYS_DIR))?.sync_all()?;
    Ok(())
}

impl ReplSession for WasiReplSession {
    fn execute(&mut self, request: CellRequest) -> Result<CellResult> {
        self.ensure_ready()?;
        let cell = EntityId::new();
        let (outcome, driver_report, stdout_path, stderr_path, wall_clock_ms);
        match self.engine.session_model() {
            SessionModel::OneShot => {
                let stem = cell.to_string();
                let (o, d, out, err, ms) =
                    self.run_oneshot_cell(cell, &request.source, &stem, jrn::CELLS_DIR)?;
                (o, d, out, err, ms)
            }
            SessionModel::LiveProcess => {
                let seq = self.journal.next_cell_seq;
                let stem = cell.to_string();
                jrn::write_fresh_file(&self.dir.join(jrn::CELLS_DIR).join(format!("{stem}.source")),
                    &request.source)?;
                let (o, d, ms) = self.run_live_cell(cell, seq, &request.source)?;
                let paths = self.live_cell_paths(seq);
                (o, d, paths.out_host, paths.err_host, ms)
            }
        };
        let (stdout_blake3, stdout_bytes) = jrn::hash_reader(File::open(&stdout_path)?)?;
        let (stderr_blake3, stderr_bytes) = jrn::hash_reader(File::open(&stderr_path)?)?;
        let stdout = self.stream_preview(&stdout_path, stdout_blake3.clone(), stdout_bytes)?;
        let stderr = self.stream_preview(&stderr_path, stderr_blake3.clone(), stderr_bytes)?;
        let stem = cell.to_string();
        let record = CellRecord {
            kind: String::new(), schema_version: 0, session_id: self.manifest.session_id,
            seq: self.journal.next_cell_seq, cell, cell_kind: request.cell_kind,
            source_blake3: jrn::hash_bytes(&request.source), source_bytes: request.source.len() as u64,
            source_path: PathBuf::from(jrn::CELLS_DIR).join(format!("{stem}.source")), outcome,
            stdout_blake3, stdout_bytes, stderr_blake3, stderr_bytes,
            stdout_path: stdout_path.strip_prefix(&self.dir)?.to_path_buf(),
            stderr_path: stderr_path.strip_prefix(&self.dir)?.to_path_buf(),
            module_blake3: self.manifest.module_blake3.clone(),
            environment_generation: self.journal.generation().clone(),
            driver_report,
            wall_clock_ms, completed_unix_ms: unix_ms(),
        };
        let record = jrn::append_cell(&mut self.journal, record)?;
        Ok(CellResult { record, stdout, stderr })
    }

    fn inspect(&self) -> Result<SessionInfo> {
        let live_running = match &self.engine {
            Engine::Live(Some(process)) => !process.is_dead(),
            _ => false,
        };
        Ok(SessionInfo {
            session_id: self.manifest.session_id, kernel_id: self.manifest.kernel.kernel_id.clone(),
            runtime: self.manifest.kernel.runtime.clone(), backend: self.manifest.kernel.backend,
            session_model: self.engine.session_model(),
            state: if self.disposed { SessionState::Disposed } else if self.busy.is_some() { SessionState::Busy }
                else if self.journal.active_restore.is_some() || self.journal.pending_reset.is_some()
                    || live_running == false && matches!(self.engine, Engine::Live(Some(_)))
                { SessionState::RecoveryRequired }
                else { SessionState::Ready },
            recovery_level: self.manifest.recovery_level,
            interpreter_state_persists: matches!(self.engine, Engine::Live(Some(_)) if live_running),
            live_process_running: live_running,
            cells_executed: self.journal.cells.len() as u64,
            environment_generation: self.journal.generation().clone(), bind: self.manifest.bind.clone(),
            limits: self.manifest.limits.clone(),
            enforcement: self.manifest.limits.enforcement(),
            driver: self.manifest.kernel.driver.identity(),
        })
    }

    fn checkpoint(&mut self) -> Result<ReplCheckpoint> {
        self.ensure_ready()?;
        let mut state_files = std::collections::BTreeMap::new();
        if let (Some(state), StatePersistence::GuestFile { guest_path }) = (&self.state, &self.manifest.kernel.persistence) {
            state.verify_root()?;
            if let Some(digest) = state.file.digest()? { state_files.insert(guest_path.clone(), digest); }
            state.verify_root()?;
        }
        Ok(ReplCheckpoint { checkpoint: EntityId::new(), session_id: self.manifest.session_id,
            level: self.manifest.recovery_level, journal_blake3: jrn::hash_file(&self.dir.join(jrn::JOURNAL_FILE))?,
            cells: self.journal.cells.len() as u64, state_files, created_unix_ms: unix_ms() })
    }

    fn restore(&mut self) -> Result<RecoverReport> {
        if self.disposed { bail!("REPL_SESSION_DISPOSED: session is closed"); }
        if self.journal.pending_reset.is_some() { self.finish_reset()?; }
        // Recovery rebuilds from the immutable baseline, never from live
        // process memory: the owned process is terminated first.
        self.stop_live_process();
        self.oneshot_cancel = None;
        let torn_tail_at = self.journal.torn_tail_at;
        let cells = self.journal.cells.clone();
        let last_reset = self.journal.last_reset.clone();
        let attempt = jrn::begin_restore(&mut self.journal)?;
        if let Some(state) = &self.state {
            state.verify_root()?;
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
        if let Some(state) = &self.state { state.verify_root()?; }
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
        copy_fork_home(&self.dir.join(jrn::GUEST_HOME_DIR), &dir.join(jrn::GUEST_HOME_DIR))?;
        jrn::copy_dir(&self.dir.join(jrn::GUEST_WORK_DIR), &dir.join(jrn::GUEST_WORK_DIR))?;
        File::create(dir.join(jrn::JOURNAL_FILE))?.sync_all()?;
        File::open(&dir)?.sync_all()?;
        File::open(&root)?.sync_all()?;
        let dir = dir.canonicalize()?;
        let state = prepared_state.map(|state| state.bind(&preopens)).transpose()?;
        let baseline = match &state { Some(state) => state.file.snapshot(&dir)?, None => None };
        let manifest = SessionManifest { forked_from: Some(self.manifest.session_id), session_id: new_id,
            baseline, created_unix_ms: unix_ms(), disposed_unix_ms: None, ..self.manifest.clone() };
        let engine = build_engine(&dir, &manifest.kernel, preopens)?;
        jrn::write_manifest(&dir, &manifest)?;
        let journal = load_journal(&dir, &manifest)?;
        Ok(Box::new(WasiReplSession { dir, manifest, engine, state, journal, busy: None,
            oneshot_cancel: None, disposed: false, preview_bytes: self.preview_bytes }))
    }

    fn cancel(&self) -> Result<CancelOutcome> {
        if self.disposed { return Ok(CancelOutcome::Disposed); }
        Ok(match self.busy {
            Some(cell) => {
                match &self.engine {
                    Engine::OneShot { .. } => {
                        if let Some(cancel) = &self.oneshot_cancel { cancel.cancel(); }
                        CancelOutcome::InterruptRequested { cell }
                    }
                    Engine::Live(Some(process)) => {
                        process.cancel();
                        CancelOutcome::InterruptRequested { cell }
                    }
                    Engine::Live(None) => CancelOutcome::NotInterruptible { cell },
                }
            }
            None => CancelOutcome::NothingRunning,
        })
    }

    fn reset(&mut self) -> Result<()> {
        if self.disposed { bail!("REPL_SESSION_DISPOSED: session is closed"); }
        if self.state.is_none() { bail!("REPL_KERNEL_UNIMPLEMENTED: no interpreter state declared"); }
        if self.journal.pending_reset.is_none() { jrn::begin_reset(&mut self.journal)?; }
        self.finish_reset()
    }

    fn install_dependencies(&mut self, request: InstallRequest) -> Result<InstallOutcome> {
        if self.disposed { return Ok(InstallOutcome::Refused { reason: "REPL_SESSION_DISPOSED".into() }); }
        if self.journal.pending_reset.is_some() || self.journal.active_restore.is_some() {
            return Ok(InstallOutcome::Refused {
                reason: "REPL_RECOVERY_REQUIRED: finish the interrupted reset or restore first".into(),
            });
        }
        if self.manifest.kernel.driver == kernel::KernelDriver::None {
            return Ok(crate::install::unsupported(&self.manifest.kernel));
        }
        if request.wheels.is_empty() && request.packages.is_empty() {
            return Ok(InstallOutcome::Refused { reason: "REPL_INSTALL_INVALID: nothing requested".into() });
        }
        // QuickJS has no package route; JS drivers use npm/bun only.
        if self.manifest.kernel.driver == kernel::KernelDriver::QuickJsCellV1 {
            return Ok(crate::install::unsupported(&self.manifest.kernel));
        }
        let candidate_tag = format!(".candidate-{}", uuid::Uuid::new_v4());
        let candidate = self.dir.join("home/.shenko/envs").join(&candidate_tag);
        let log_dir = self.dir.join("home/.shenko/install-logs").join(candidate_tag.trim_start_matches('.').to_string());
        fs::create_dir_all(&log_dir)?;
        let attempt = match crate::install::install_into_candidate(
            &self.manifest.kernel, &request, &candidate, &log_dir)
        {
            Ok(attempt) => attempt,
            Err(error) => {
                let _ = fs::remove_dir_all(&candidate);
                return Ok(InstallOutcome::Refused { reason: format!("{error:#}") });
            }
        };
        if attempt.installed.is_empty() {
            let _ = fs::remove_dir_all(&candidate);
            let reasons = attempt.skipped.iter()
                .map(|s| format!("{}: {}", s.name, s.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(InstallOutcome::Refused {
                reason: format!("REPL_INSTALL_FAILED: nothing installed; {reasons}"),
            });
        }
        let previous_generation = self.journal.generation().clone();
        let environment_generation =
            crate::install::next_generation(&self.manifest.kernel, &attempt.installed);
        // Durable transition first; the manifest follows. A crash between is
        // healed by reconcile_manifest_generation on reopen.
        jrn::append_env(&mut self.journal, attempt.installed.clone(), previous_generation.clone(),
            environment_generation.clone(), unix_ms())?;
        let tag = generation_tag(&environment_generation);
        let committed = env_dir_host(&self.dir, &tag);
        fs::rename(&candidate, &committed)?;
        sync_directory_ancestors(&self.dir.join("home/.shenko/envs"))?;
        File::open(&committed)?.sync_all()?;
        self.manifest.kernel.dependency_lock.extend(attempt.installed.iter().cloned());
        self.manifest.environment_generation = environment_generation.clone();
        jrn::write_manifest(&self.dir, &self.manifest)?;
        Ok(InstallOutcome::Installed {
            installed: attempt.installed.iter()
                .map(|package| format!("{}={}", package.name, package.pinned)).collect(),
            skipped: attempt.skipped,
            previous_generation,
            environment_generation,
        })
    }

    fn dispose(&mut self) -> Result<()> {
        // Fence immediately, but retry persistence until a write succeeds.
        self.disposed = true;
        self.busy = None;
        self.oneshot_cancel = None;
        if let Some(outcome) = self.stop_live_process() {
            if let Some(error) = outcome.transport_error {
                // Isolation is already achieved (the process is gone); the
                // durable close marker is retried below via the manifest.
                eprintln!("shenko-repl: live process transport error at dispose: {error}");
            }
        }
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

/// Copy the session home for fork, skipping in-flight install candidates
/// (they belong to the parent's unfinished transition, not to the fork).
fn copy_fork_home(source: &Path, target: &Path) -> Result<()> {
    if target.exists() { bail!("REPL_FORK_CONFLICT: {} already exists", target.display()); }
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".candidate-") { continue; }
        let ty = entry.file_type()?;
        let dest = target.join(&name);
        if ty.is_dir() { jrn::copy_dir(&entry.path(), &dest)?; }
        else if ty.is_file() {
            std::io::copy(&mut File::open(entry.path())?, &mut fs::File::create(&dest)?)?;
            File::open(&dest)?.sync_all()?;
        } else { bail!("REPL_FORK_UNSUPPORTED: {} is not a regular file or directory", entry.path().display()); }
    }
    File::open(target)?.sync_all()?;
    Ok(())
}
