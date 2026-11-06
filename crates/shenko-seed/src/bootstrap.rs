use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::{EntityId, RuntimeGeneration};
use shenko_types::object::ObjectRef;
use shenko_node::Node;
use shenko_store::cas::ContentStore;
use shenko_wasm::{
    checkpoint::HomeCheckpointStore,
    HOME_GUEST_PATH, MountMode, MountSourceState, MountSpec, TOOL_DIR_NAME, WasiCommand,
    WasiRunOutcome, WasmRuntime,
};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const HOME_MANIFEST_FILE: &str = "home-manifest.json";
pub const HOME_READY_FILE: &str = ".shenko-home-ready";
pub const HOME_READY_BODY: &str = "shenko-home-v1\n";
pub const MEMORY_GUEST_PATH: &str = "/memory";

/// This real WASI preview1 command is part of the seed closure sealed into CAS.
/// It checks both preopens, creates or rewrites one fixed persistent home
/// file through guest fd_write. The host separately reads the same file.
pub const HOME_PROBE_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_prestat_get"
    (func $fd_prestat_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_prestat_dir_name"
    (func $fd_prestat_dir_name (param i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 200) ".shenko-home-ready")
  (data (i32.const 240) "shenko-home-v1\n")
  (func (export "_start")
    (if (i32.ne (call $fd_prestat_get (i32.const 3) (i32.const 0)) (i32.const 0))
      (then (call $proc_exit (i32.const 41))))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const 5))
      (then (call $proc_exit (i32.const 42))))
    (if (i32.ne (call $fd_prestat_dir_name (i32.const 3) (i32.const 80) (i32.const 5)) (i32.const 0))
      (then (call $proc_exit (i32.const 43))))
    (if (i32.ne (i32.load (i32.const 80)) (i32.const 0x6d6f682f))
      (then (call $proc_exit (i32.const 44))))
    (if (i32.ne (i32.load8_u (i32.const 84)) (i32.const 0x65))
      (then (call $proc_exit (i32.const 45))))
    (if (i32.ne (call $fd_prestat_get (i32.const 4) (i32.const 16)) (i32.const 0))
      (then (call $proc_exit (i32.const 46))))
    (if (i32.ne (i32.load (i32.const 20)) (i32.const 7))
      (then (call $proc_exit (i32.const 47))))
    (if (i32.ne (call $fd_prestat_dir_name (i32.const 4) (i32.const 96) (i32.const 7)) (i32.const 0))
      (then (call $proc_exit (i32.const 48))))
    (if (i32.ne (i32.load (i32.const 96)) (i32.const 0x6d656d2f))
      (then (call $proc_exit (i32.const 49))))
    (if (i32.ne (i32.load8_u (i32.const 100)) (i32.const 0x6f))
      (then (call $proc_exit (i32.const 50))))
    (if (i32.ne (i32.load8_u (i32.const 101)) (i32.const 0x72))
      (then (call $proc_exit (i32.const 51))))
    (if (i32.ne (i32.load8_u (i32.const 102)) (i32.const 0x79))
      (then (call $proc_exit (i32.const 52))))
    (i32.store (i32.const 300) (i32.const 240))
    (i32.store (i32.const 304) (i32.const 15))
    (if (i32.ne
      (call $path_open
        (i32.const 3) (i32.const 0) (i32.const 200) (i32.const 18)
        (i32.const 1) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 312))
      (i32.const 0))
      (then (call $proc_exit (i32.const 53))))
    (if (i32.ne
      (call $fd_write (i32.load (i32.const 312)) (i32.const 300) (i32.const 1) (i32.const 320))
      (i32.const 0))
      (then (call $proc_exit (i32.const 54))))
    (if (i32.ne (i32.load (i32.const 320)) (i32.const 15))
      (then (call $proc_exit (i32.const 55))))
  )
)
"#;

#[derive(Debug, Clone)]
pub struct SeedConfig {
    pub data_dir: PathBuf,
    pub agent_name: Option<String>,
    pub napcat_ws_url: Option<String>,
}

impl Default for SeedConfig {
    fn default() -> Self {
        Self {
            data_dir: dirs_next::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("shenko"),
            agent_name: None,
            napcat_ws_url: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeState {
    Preparing,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointLevel {
    FilesAndMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeCheckpoint {
    pub level: CheckpointLevel,
    pub live_wasm_memory: bool,
    pub model_kv: bool,
    pub sockets: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
    Unsupported,
    NotImplemented,
    NotCaptured,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub status: CapabilityStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeManifest {
    pub schema_version: u32,
    pub installation_id: EntityId,
    pub state: HomeState,
    pub runtime_generation: RuntimeGeneration,
    pub runtime: String,
    pub wasi: String,
    pub home_guest_path: String,
    pub mounts: Vec<MountSpec>,
    pub checkpoint: HomeCheckpoint,
    pub capabilities: BTreeMap<String, Capability>,
    pub created_at: DateTime<Utc>,
}

impl HomeManifest {
    pub fn home_dir(&self) -> &Path {
        self.mount_dir(HOME_GUEST_PATH)
    }

    pub fn memory_dir(&self) -> &Path {
        self.mount_dir(MEMORY_GUEST_PATH)
    }

    fn mount_dir(&self, guest_path: &str) -> &Path {
        self.mounts
            .iter()
            .find(|mount| mount.guest_path == guest_path)
            .map(|mount| mount.source.as_path())
            .expect("validated home manifest has both managed mounts")
    }
}

/// Why a home in the incomplete `preparing` state was treated as this seed's
/// own interrupted first start rather than as unexplained damage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryCause {
    /// The seed's guest probe created its readiness file but the process
    /// stopped before the guest wrote the complete `HOME_READY_BODY`. The
    /// partial file is a strict prefix of that exact body, so re-running the
    /// same probe rewrites this seed's own artifact.
    InterruptedGuestProbe,
    /// Files and metadata of the persistent mounts were restored from a
    /// verified, published checkpoint closure. `safety` is the closure captured
    /// from the damaged state immediately before the restore, so those bytes
    /// remain in CAS instead of being destroyed.
    CheckpointRestore {
        checkpoint: shenko_types::object::Digest,
        safety: shenko_types::object::Digest,
        head: shenko_types::object::Digest,
        written: usize,
        removed: usize,
    },
}

/// A recovery this seed actually performed on its own first-start closure.
///
/// Recovery only rematerializes seed-owned artifacts through the real guest:
/// it never recreates a lost persistent directory, never copies host bytes
/// into the home to stand in for guest writes, and never replays or reverses
/// an external effect. Durable facts outside the home (ledger, inbox/outbox,
/// sent messages, remote writes) are untouched, so a file-level recovery is
/// not a rollback of anything that already left the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeRecovery {
    /// The persisted state the home was in when the interruption was found.
    pub from_state: HomeState,
    pub cause: RecoveryCause,
    /// Guest paths the seed's factory closure rewrote to finish first start.
    pub rematerialized: Vec<String>,
}

impl std::fmt::Display for HomeRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            RecoveryCause::InterruptedGuestProbe => write!(
                f,
                "interrupted_guest_probe from {:?}: rewrote {}",
                self.from_state,
                self.rematerialized.join(", ")
            ),
            RecoveryCause::CheckpointRestore {
                checkpoint,
                safety,
                head,
                written,
                removed,
            } => write!(
                f,
                "checkpoint_restore from {:?}: restored {checkpoint} ({} written, {} removed, safety {safety}, head {head}) into {}",
                self.from_state,
                written,
                removed,
                self.rematerialized.join(", ")
            ),
        }
    }
}

/// Which part of a persisted home is damaged in a way a file/metadata closure
/// can repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeDamageKind {
    LostHomeDirectory,
    LostMemoryDirectory,
    LostReadinessMarker,
    ChangedReadinessMarker,
    LostGitRepository,
}

/// A damaged persisted home, carried as a typed error so recovery never depends
/// on matching message text. Damage that is *not* file damage — a foreign
/// installation, an unsupported capability/runtime declaration, a symlinked
/// managed directory — is a different error and is never "repaired" by a
/// restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeDamage {
    /// State the home declared when the damage was found.
    pub state: HomeState,
    pub kind: HomeDamageKind,
    pub path: PathBuf,
}

impl std::fmt::Display for HomeDamage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.path.display();
        match self.kind {
            HomeDamageKind::LostHomeDirectory => write!(
                f,
                "SEED_CLOSURE_INCOMPLETE: ready home lost its persistent home directory at {path}; data preserved"
            ),
            HomeDamageKind::LostMemoryDirectory => write!(
                f,
                "SEED_CLOSURE_INCOMPLETE: ready home lost its persistent memory directory at {path}; data preserved"
            ),
            HomeDamageKind::LostReadinessMarker => write!(
                f,
                "SEED_CLOSURE_INCOMPLETE: ready home lost its guest readiness file at {path}; data preserved"
            ),
            HomeDamageKind::ChangedReadinessMarker => write!(
                f,
                "SEED_CLOSURE_INCOMPLETE: guest readiness content changed at {path}; data preserved"
            ),
            HomeDamageKind::LostGitRepository => write!(
                f,
                "SEED_CLOSURE_INCOMPLETE: managed Git repository is missing at {path}; data preserved"
            ),
        }
    }
}

impl std::error::Error for HomeDamage {}

pub struct PreparedHome {
    pub manifest: HomeManifest,
    pub manifest_path: PathBuf,
    /// Kept only for the bootstrap phase; the caller drops it before the main
    /// loop so merely holding a home does not keep an engine per subject step.
    pub runtime: WasmRuntime,
    /// The named-WASM-tool materialization root (`<data>/tools`). Tool
    /// records themselves live in the node installation-unit inventory; this
    /// directory only holds the bytes the unit authority materialized, so a
    /// foreign copy here is inert.
    pub tools_root: PathBuf,
    /// Recoveries this attempt completed; empty for a clean start or resume.
    pub recovered: Vec<HomeRecovery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapacity {
    Blocked,
    Unprobed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub configured: bool,
    pub capacity: ModelCapacity,
}

impl ModelProfile {
    /// Mirrors the real broker's provider precedence without reading or
    /// persisting credentials. Configured is not a successful probe.
    pub fn from_env() -> Self {
        if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() == Ok("1") {
            return Self {
                provider: Some("local".into()),
                model: Some(
                    std::env::var("SHENKO_LOCAL_MODEL")
                        .unwrap_or_else(|_| "qwen3.8-27b".into()),
                ),
                configured: true,
                capacity: ModelCapacity::Unprobed,
            };
        }
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            return Self {
                provider: Some("anthropic".into()),
                model: Some("claude-opus-5-5".into()),
                configured: true,
                capacity: ModelCapacity::Unprobed,
            };
        }
        Self {
            provider: None,
            model: None,
            configured: false,
            capacity: ModelCapacity::Blocked,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapNeed {
    OwnerClaim,
    ModelBinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapSubjectState {
    PendingGenesis,
    ExistingGenesis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapView {
    pub installation_id: EntityId,
    pub state: String,
    pub needs: Vec<BootstrapNeed>,
    pub subject: BootstrapSubjectState,
    /// Only an explicit persisted grant/provisioning ref clears owner_claim.
    /// Genesis and HTTP visits never imply one.
    pub owner_grant_ref: Option<ObjectRef>,
    pub model: ModelProfile,
}

pub struct Bootstrap {
    pub config: SeedConfig,
}

impl Bootstrap {
    pub fn new(config: SeedConfig) -> Self {
        Self { config }
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_target()
    }

    /// Reject a foreign data target before Store::open adds installation files.
    /// Existing installations are resumed by their persistent ledger identity.
    pub fn validate_target(&self) -> Result<()> {
        let root = &self.config.data_dir;
        if root.exists() && !root.is_dir() {
            bail!("DATA_PATH_CONFLICT: {} is not a directory", root.display());
        }
        if !root.is_dir() {
            return Ok(());
        }
        let manifest_exists = root.join(HOME_MANIFEST_FILE).exists();
        let store_exists = root.join("shenko.db").exists();
        if !manifest_exists && !store_exists {
            for entry in fs::read_dir(root)? {
                let name = entry?.file_name();
                if name != "home" && name != "memory" && name != TOOL_DIR_NAME {
                    bail!(
                        "DATA_PATH_CONFLICT: unrecognized data in {}; choose a new path or import explicitly",
                        root.display()
                    );
                }
            }
        }
        for managed in ["home", "memory", TOOL_DIR_NAME] {
            let path = root.join(managed);
            if path.exists() && !path.is_dir() {
                bail!("DATA_PATH_CONFLICT: {} is not a directory", path.display());
            }
        }
        // Foreign tool ownership is admitted BEFORE any installation state
        // is written: a `tools/` directory is only valid as the
        // materialization root of this installation's own node inventory.
        // A foreign tools tree (or one with no inventory behind it) is a
        // conflict reported here, never absorbed by later bootstrap writes.
        let tools_dir = root.join(TOOL_DIR_NAME);
        if tools_dir.is_dir() {
            let owned = match Node::open_inspect(&root.join("node"))? {
                Some(node) => {
                    let roots = node.materialization_roots()?;
                    let tools = absolute(&tools_dir)
                        .ok()
                        .and_then(|p| p.canonicalize().ok());
                    match tools {
                        Some(tools) => roots.iter().any(|r| {
                            Path::new(r)
                                .canonicalize()
                                .map(|r| r == tools)
                                .unwrap_or(false)
                        }),
                        None => false,
                    }
                }
                None => false,
            };
            ensure!(
                owned || !tools_dir.read_dir()?.next().is_some(),
                "DATA_PATH_CONFLICT: {} holds tool data this installation did not \
                 materialize; data preserved",
                tools_dir.display()
            );
        }
        Ok(())
    }

    pub fn view(
        &self,
        installation_id: EntityId,
        subject: BootstrapSubjectState,
        model: ModelProfile,
        owner_grant_ref: Option<ObjectRef>,
    ) -> BootstrapView {
        let mut needs = Vec::new();
        if owner_grant_ref.is_none() {
            needs.push(BootstrapNeed::OwnerClaim);
        }
        if !model.configured {
            needs.push(BootstrapNeed::ModelBinding);
        }
        let state = if needs.is_empty() {
            "ready_to_start"
        } else {
            "awaiting_input"
        };
        BootstrapView {
            installation_id,
            state: state.into(),
            needs,
            subject,
            owner_grant_ref,
            model,
        }
    }

    /// Prepare or resume the actual seed home. Every external step is safe to
    /// retry under the stable installation identity; no identity is regenerated
    /// merely because a previous attempt stopped after Git or guest I/O.
    pub fn prepare(&self, installation_id: EntityId) -> Result<PreparedHome> {
        self.validate_target()?;
        let root = absolute(&self.config.data_dir)?;
        fs::create_dir_all(&root)
            .with_context(|| format!("create seed data directory {}", root.display()))?;
        let root = root.canonicalize()?;
        let manifest_path = root.join(HOME_MANIFEST_FILE);
        let home_input = root.join("home");
        let memory_input = root.join("memory");

        // The persisted closure is read before anything is created, and the
        // managed directories are inspected without being created: a home
        // that was declared ready must never be rebuilt as an empty one,
        // because an empty `home/` is indistinguishable from a fresh install
        // and would hide the loss of the persistent home.
        let stored = read_stored_manifest(&manifest_path)?;
        let home_present = inspect_managed_dir(&home_input)?;
        let memory_present = inspect_managed_dir(&memory_input)?;

        if stored.is_none() {
            if home_present == ManagedDirState::Present && directory_has_entries(&home_input)? {
                bail!(
                    "DATA_PATH_CONFLICT: nonempty home {} has no installation manifest",
                    home_input.display()
                );
            }
            if memory_present == ManagedDirState::Present && directory_has_entries(&memory_input)? {
                bail!(
                    "DATA_PATH_CONFLICT: nonempty memory {} has no installation manifest; import it explicitly instead of adopting unknown data",
                    memory_input.display()
                );
            }
        }

        // A ready home keeps its persistent directories. Their absence is a
        // typed, recoverable damage: `prepare_with_recovery` restores it from an
        // actually published closure, and a plain `prepare` reports it without
        // creating an empty directory or touching the manifest.
        if let Some(stored) = &stored {
            if stored.state == HomeState::Ready {
                for (present, kind, path) in [
                    (home_present, HomeDamageKind::LostHomeDirectory, &home_input),
                    (memory_present, HomeDamageKind::LostMemoryDirectory, &memory_input),
                ] {
                    if present == ManagedDirState::Absent {
                        return Err(anyhow!(HomeDamage {
                            state: HomeState::Ready,
                            kind,
                            path: path.clone(),
                        }));
                    }
                }
            }
        }

        fs::create_dir_all(&home_input)?;
        fs::create_dir_all(&memory_input)?;
        let home_dir = home_input.canonicalize()?;
        let memory_dir = memory_input.canonicalize()?;
        let expected_mounts = vec![
            MountSpec::new(home_dir, HOME_GUEST_PATH, MountMode::ReadWrite, true, false),
            MountSpec::new(
                memory_dir,
                MEMORY_GUEST_PATH,
                MountMode::ReadWrite,
                true,
                true,
            ),
        ];

        // A stored home whose capability map matches exactly the legacy
        // declaration is upgraded in place; every other stored map mismatch
        // is a migration error with the data preserved.
        let legacy_capabilities = match &stored {
            Some(stored) => admit_stored_manifest(stored, installation_id, &root, &expected_mounts)?,
            None => false,
        };
        let mut manifest = match stored {
            Some(stored) => stored,
            None => {
                let created = HomeManifest {
                    schema_version: 1,
                    installation_id,
                    state: HomeState::Preparing,
                    runtime_generation: RuntimeGeneration::default(),
                    runtime: "wasmtime-45".into(),
                    wasi: "preview1".into(),
                    home_guest_path: HOME_GUEST_PATH.into(),
                    mounts: expected_mounts,
                    checkpoint: home_checkpoint(),
                    capabilities: home_capabilities(),
                    created_at: Utc::now(),
                };
                persist_manifest(&manifest_path, &created)?;
                created
            }
        };

        let memory = manifest.memory_dir().to_path_buf();
        let git_marker = memory.join(".git");
        if !git_marker.exists() {
            if manifest.state == HomeState::Ready || directory_has_entries(&memory)? {
                return Err(anyhow!(HomeDamage {
                    state: manifest.state,
                    kind: HomeDamageKind::LostGitRepository,
                    path: memory,
                }));
            }
            run_git(&memory, &["init", "--quiet", "--initial-branch=main"])?;
        }
        verify_git_repository(&memory)?;

        let mut runtime = WasmRuntime::with_mounts(manifest.mounts.clone())?;
        let ready_file = manifest.home_dir().join(HOME_READY_FILE);
        // Set only for a genuine interrupted first start; the marker is
        // rewritten by the guest below and reported only after the host
        // readback proves this seed's exact body is back on disk.
        let mut interrupted_probe = false;
        match fs::symlink_metadata(&ready_file) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    bail!(
                        "SEED_CLOSURE_INCOMPLETE: guest readiness path is not a regular file at {}; data preserved",
                        ready_file.display()
                    );
                }
                let previous = fs::read_to_string(&ready_file).with_context(|| {
                    format!(
                        "read existing guest readiness file {}",
                        ready_file.display()
                    )
                })?;
                if previous != HOME_READY_BODY {
                    // The seed probe creates this file and writes exactly
                    // HOME_READY_BODY. Inside the incomplete `preparing`
                    // state, a *strict prefix* of that exact body is the
                    // signature of the guest stopping before it finished the
                    // write, which the same probe can complete. A different,
                    // longer or post-ready body is unexplained damage and
                    // stays preserved.
                    let interrupted = previous.len() < HOME_READY_BODY.len()
                        && HOME_READY_BODY.starts_with(previous.as_str());
                    if manifest.state != HomeState::Preparing || !interrupted {
                        return Err(anyhow!(HomeDamage {
                            state: manifest.state,
                            kind: HomeDamageKind::ChangedReadinessMarker,
                            path: ready_file,
                        }));
                    }
                    interrupted_probe = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if manifest.state == HomeState::Ready {
                    return Err(anyhow!(HomeDamage {
                        state: manifest.state,
                        kind: HomeDamageKind::LostReadinessMarker,
                        path: ready_file,
                    }));
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect guest readiness file {}", ready_file.display())
                });
            }
        }
        // This fixed probe uses only its preopens and deliberately has no
        // process input or output. Other commands must choose their streams.
        let command = WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty());
        let outcome = runtime.run_wasi_module(HOME_PROBE_WAT.as_bytes(), command)?;
        if outcome != WasiRunOutcome::Exited(0) {
            bail!("SEED_CLOSURE_INCOMPLETE: home guest probe outcome {outcome:?}");
        }
        let observed = fs::read_to_string(&ready_file)
            .with_context(|| format!("host readback of {}", ready_file.display()))?;
        if observed != HOME_READY_BODY {
            bail!(
                "SEED_CLOSURE_INCOMPLETE: guest/home host readback mismatch at {}",
                ready_file.display()
            );
        }

        let mut recovered = Vec::new();
        if interrupted_probe {
            recovered.push(HomeRecovery {
                from_state: HomeState::Preparing,
                cause: RecoveryCause::InterruptedGuestProbe,
                rematerialized: vec![format!("{HOME_GUEST_PATH}/{HOME_READY_FILE}")],
            });
        }

        if manifest.state != HomeState::Ready || legacy_capabilities {
            manifest.state = HomeState::Ready;
            manifest.capabilities = home_capabilities();
            persist_manifest(&manifest_path, &manifest)?;
        }
        Ok(PreparedHome {
            manifest,
            manifest_path,
            runtime,
            tools_root: root.join(TOOL_DIR_NAME),
            recovered,
        })
    }

    /// Prepare the home, repairing recoverable damage from a published
    /// file/metadata closure, and keep the closure current.
    ///
    /// Recovery is only attempted for [`HomeDamage`] and only from a closure
    /// that the installation's own CAS verifies. Everything else — a foreign
    /// installation, an unsupported capability or runtime declaration, a
    /// symlinked managed directory — keeps its original error. When there is no
    /// published closure the damage is reported as unrecovered; nothing is
    /// recreated empty and no "attempt" is recorded as a recovery.
    pub fn prepare_with_recovery(
        &self,
        installation_id: EntityId,
        cas: &ContentStore,
    ) -> Result<PreparedHome> {
        let prepared = match self.prepare(installation_id) {
            Ok(prepared) => prepared,
            Err(error) => return self.recover_damaged_home(installation_id, cas, error),
        };
        let root = prepared
            .manifest_path
            .parent()
            .ok_or_else(|| anyhow!("home manifest has no installation directory"))?
            .to_path_buf();
        let store = checkpoint_store(cas, &root, installation_id)?;
        let publish = |initial: bool| -> Result<()> {
            match store.capture("bootstrap") {
                Ok(outcome) if outcome.published => {
                    tracing::info!(
                        checkpoint=%outcome.id,
                        objects=outcome.objects_sealed,
                        bytes=outcome.bytes,
                        "published home checkpoint closure"
                    );
                    Ok(())
                }
                Ok(_) => Ok(()),
                // Once a closure exists this refresh is auxiliary: report the
                // failure instead of failing an otherwise complete bootstrap.
                // The very first closure is not auxiliary — without it this home
                // has no byte-level recovery at all, and claiming otherwise
                // would be false.
                Err(error) if !initial => {
                    tracing::warn!(error=%error, "home checkpoint closure not refreshed");
                    Ok(())
                }
                Err(error) => Err(error).context("publish the initial home checkpoint closure"),
            }
        };
        match store.head() {
            Ok(None) => publish(true)?,
            Ok(Some(_)) => publish(false)?,
            // An unreadable record set must not block ordinary startup, but it
            // is reported and it is never silently rebuilt: recovery refuses to
            // read a corrupt record.
            Err(error) => {
                tracing::warn!(error=%error, "home checkpoint record is unreadable; no closure refreshed");
            }
        }
        Ok(prepared)
    }

    fn recover_damaged_home(
        &self,
        installation_id: EntityId,
        cas: &ContentStore,
        error: anyhow::Error,
    ) -> Result<PreparedHome> {
        let damage = match error.downcast_ref::<HomeDamage>() {
            Some(damage) => damage.clone(),
            None => return Err(error),
        };
        let root = installation_root(&self.config.data_dir)?;
        let store = checkpoint_store(cas, &root, installation_id)?;
        let (target, resumed) = match store.pending_restore()? {
            // An earlier restore was interrupted. A restore is a full-view
            // application, so re-applying the same closure is the safe resume;
            // the intent that named it is still the record of what was happening.
            Some(intent) => (store.manifest(&intent.checkpoint)?, true),
            None => match store.latest()? {
                Some(manifest) => (manifest, false),
                None => {
                    return Err(error.context(format!(
                        "no published home checkpoint closure exists, so this damage cannot be recovered from an LKG/factory closure: {damage}"
                    )));
                }
            },
        };
        let reason = if resumed {
            format!("resume interrupted restore after {damage}")
        } else {
            format!("recover: {damage}")
        };
        let receipt = store.restore(&target, &reason).with_context(|| {
            format!("recover the persistent home from checkpoint closure: {damage}")
        })?;
        let mut prepared = self.prepare(installation_id).with_context(|| {
            format!(
                "restored closure {} (safety {}) but the home still does not admit",
                receipt.checkpoint, receipt.safety
            )
        })?;
        prepared.recovered.push(HomeRecovery {
            from_state: damage.state,
            cause: RecoveryCause::CheckpointRestore {
                checkpoint: receipt.checkpoint,
                safety: receipt.safety,
                head: receipt.head,
                written: receipt.written,
                removed: receipt.removed,
            },
            rematerialized: store
                .guest_paths()
                .into_iter()
                .map(str::to_string)
                .collect(),
        });
        Ok(prepared)
    }
}

/// The installation directory, canonical when it already exists.
fn installation_root(data_dir: &Path) -> Result<PathBuf> {
    let root = absolute(data_dir)?;
    Ok(root.canonicalize().unwrap_or(root))
}

fn checkpoint_store<'a>(
    cas: &'a ContentStore,
    root: &'a Path,
    installation_id: EntityId,
) -> Result<HomeCheckpointStore<'a>> {
    HomeCheckpointStore::new(
        cas,
        root,
        installation_id,
        vec![
            (HOME_GUEST_PATH.to_string(), root.join("home")),
            (MEMORY_GUEST_PATH.to_string(), root.join("memory")),
        ],
    )
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Whether a managed directory exists as a real directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedDirState {
    /// A real directory; symbolic links are never accepted as one.
    Present,
    /// Nothing exists at the path.
    Absent,
}

/// Inspect one managed directory without creating it. A symbolic link is a
/// conflict rather than an alias: one installation's home or memory must not
/// silently resolve into another installation's writable state.
fn inspect_managed_dir(path: &Path) -> Result<ManagedDirState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "DATA_PATH_CONFLICT: managed directory {} is a symbolic link; declare the directory itself so installations cannot alias each other",
            path.display()
        ),
        Ok(metadata) if metadata.file_type().is_dir() => Ok(ManagedDirState::Present),
        Ok(_) => bail!(
            "DATA_PATH_CONFLICT: managed path {} is not a directory",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ManagedDirState::Absent),
        Err(error) => Err(error)
            .with_context(|| format!("inspect managed directory {}", path.display())),
    }
}

/// Read and decode the persisted home manifest, if any. Only the shape that
/// every seed understands is checked here; the seed-specific declarations are
/// admitted by [`admit_stored_manifest`] once the managed directories are
/// known to exist.
fn read_stored_manifest(path: &Path) -> Result<Option<HomeManifest>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow!(
                "SEED_CLOSURE_INCOMPLETE: read {}: {error}",
                path.display()
            ));
        }
    };
    let stored: HomeManifest = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow!(
            "SEED_CLOSURE_INCOMPLETE: decode {}: {error}",
            path.display()
        )
    })?;
    if stored.schema_version != 1 {
        bail!(
            "SEED_CLOSURE_INCOMPLETE: unsupported home manifest schema {}",
            stored.schema_version
        );
    }
    Ok(Some(stored))
}

/// Admit a stored manifest against this seed and the managed directories that
/// actually exist. Returns whether the stored capability map is exactly the
/// pre-COW legacy declaration, which a completed preparation upgrades in
/// place; any other declaration difference stays a migration error.
fn admit_stored_manifest(
    stored: &HomeManifest,
    installation_id: EntityId,
    root: &Path,
    expected_mounts: &[MountSpec],
) -> Result<bool> {
    if stored.installation_id != installation_id || stored.mounts != expected_mounts {
        bail!(
            "DATA_PATH_CONFLICT: stored home/installation binding does not match {}; data preserved",
            root.display()
        );
    }
    if stored.home_guest_path != HOME_GUEST_PATH
        || stored.runtime != "wasmtime-45"
        || stored.wasi != "preview1"
        || stored.runtime_generation != RuntimeGeneration::default()
    {
        bail!(
            "WASM_PROFILE_UNAVAILABLE: stored runtime/generation/guest binding requires an explicit compatible activation; data preserved"
        );
    }
    if stored.checkpoint != home_checkpoint() {
        bail!(
            "SEED_CLOSURE_INCOMPLETE: stored checkpoint declaration differs from this seed; migrate explicitly"
        );
    }
    if stored.capabilities == home_capabilities() {
        return Ok(false);
    }
    if stored.capabilities == legacy_home_capabilities() {
        return Ok(true);
    }
    bail!(
        "SEED_CLOSURE_INCOMPLETE: stored capability declaration differs from this seed; migrate explicitly"
    )
}

/// Admission checks for executing a guest under a stored managed home —
/// shared by `Bootstrap::prepare` and any caller that runs a tool with the
/// declared mounts (`shenko tool exec`). Validates the manifest's schema,
/// stored installation and mount bindings (mount sources must be this data
/// dir's own home/memory, so a copied installation cannot alias a sibling's
/// files), runtime/generation markers and the persisted checkpoint and
/// capability declarations. Read-only: it never creates or mutates state.
pub fn validate_home_for_exec(data_dir: &Path) -> Result<HomeManifest> {
    let root = absolute(data_dir)?.canonicalize().map_err(|error| {
        anyhow!(
            "SEED_CLOSURE_INCOMPLETE: resolve data directory {}: {error}",
            data_dir.display()
        )
    })?;
    let manifest_path = root.join(HOME_MANIFEST_FILE);
    let stored = read_stored_manifest(&manifest_path)?.ok_or_else(|| {
        anyhow!(
            "WASM_PROFILE_UNAVAILABLE: no managed home manifest at {}; run the seed once",
            manifest_path.display()
        )
    })?;
    // A home whose preparation never finished has no usable guest home yet.
    // Saying so here keeps a half-prepared installation from being reported
    // as a bare missing-file error.
    if stored.state != HomeState::Ready {
        bail!(
            "SEED_CLOSURE_INCOMPLETE: home preparation did not complete at {} (state {:?}); finish the seed first, data preserved",
            root.display(),
            stored.state
        );
    }
    if stored.mounts.len() != 2 {
        bail!("SEED_CLOSURE_INCOMPLETE: unexpected mount declaration in home manifest");
    }
    // Mount sources must resolve to this data dir's own managed directories.
    // Missing and inaccessible managed directories are reported separately:
    // they need different actions and neither is repaired by re-preparing.
    for (guest_path, name) in [(HOME_GUEST_PATH, "home"), (MEMORY_GUEST_PATH, "memory")] {
        let mount = stored
            .mounts
            .iter()
            .find(|mount| mount.guest_path == guest_path)
            .ok_or_else(|| {
                anyhow!(
                    "SEED_CLOSURE_INCOMPLETE: home manifest declares no {guest_path} mount"
                )
            })?;
        match mount.source_state() {
            MountSourceState::Ready => {}
            MountSourceState::Missing => bail!(
                "SEED_CLOSURE_INCOMPLETE: managed {name} is missing at {}; exec admission never recreates it, data preserved",
                mount.source.display()
            ),
            MountSourceState::NotADirectory => bail!(
                "DATA_PATH_CONFLICT: managed {name} at {} is not a directory",
                mount.source.display()
            ),
            MountSourceState::SymbolicLink { .. } => bail!(
                "DATA_PATH_CONFLICT: managed {name} at {} is a symbolic link; one installation cannot alias another's state",
                mount.source.display()
            ),
            MountSourceState::Inaccessible(error) => bail!(
                "SEED_CLOSURE_INCOMPLETE: managed {name} at {} is inaccessible: {error}; data preserved",
                mount.source.display()
            ),
        }
        let expected = root.join(name).canonicalize().with_context(|| {
            format!("managed {} is missing under {}", name, root.display())
        })?;
        let actual = mount.source.canonicalize().with_context(|| {
            format!("resolve stored {guest_path} mount {}", mount.source.display())
        })?;
        ensure!(
            actual == expected,
            "DATA_PATH_CONFLICT: stored {guest_path} mount does not resolve to {}; data preserved",
            expected.display()
        );
    }
    if stored.home_guest_path != HOME_GUEST_PATH
        || stored.runtime != "wasmtime-45"
        || stored.wasi != "preview1"
        || stored.runtime_generation != RuntimeGeneration::default()
    {
        bail!(
            "WASM_PROFILE_UNAVAILABLE: stored runtime/generation/guest binding requires an explicit compatible activation; data preserved"
        );
    }
    // A home prepared before managed COW mounts existed carries the earlier,
    // conservative capability declaration. Exec admission keeps it usable and
    // leaves the upgrade to the next `prepare`, which rewrites that exact map;
    // any other declaration difference is still a migration error.
    if stored.checkpoint != home_checkpoint()
        || (stored.capabilities != home_capabilities()
            && stored.capabilities != legacy_home_capabilities())
    {
        bail!(
            "SEED_CLOSURE_INCOMPLETE: stored checkpoint/capability declaration differs from this seed; migrate explicitly"
        );
    }
    let ready_file = stored.home_dir().join(HOME_READY_FILE);
    match fs::symlink_metadata(&ready_file) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => bail!(
            "SEED_CLOSURE_INCOMPLETE: guest readiness path is not a regular file at {}; data preserved",
            ready_file.display()
        ),
        Err(error) => bail!(
            "SEED_CLOSURE_INCOMPLETE: ready home has no readable guest readiness file at {}: {error}; data preserved",
            ready_file.display()
        ),
    }
    let previous = fs::read_to_string(&ready_file).with_context(|| {
        format!("read existing guest readiness file {}", ready_file.display())
    })?;
    if previous != HOME_READY_BODY {
        bail!(
            "SEED_CLOSURE_INCOMPLETE: guest readiness content changed at {}; data preserved",
            ready_file.display()
        );
    }
    Ok(stored)
}

fn directory_has_entries(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if !path.is_dir() {
        bail!("DATA_PATH_CONFLICT: {} is not a directory", path.display());
    }
    Ok(fs::read_dir(path)?.next().transpose()?.is_some())
}

fn run_git(repository: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .with_context(|| format!("execute host git for {}", repository.display()))?;
    if !output.status.success() {
        bail!(
            "SEED_CLOSURE_INCOMPLETE: host git {:?} failed ({}): {}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn verify_git_repository(repository: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| "execute host git rev-parse".to_string())?;
    if !output.status.success() {
        bail!(
            "SEED_CLOSURE_INCOMPLETE: invalid host Git memory {}: {}",
            repository.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let top = String::from_utf8(output.stdout)?;
    let top = PathBuf::from(top.trim()).canonicalize()?;
    if top != repository.canonicalize()? {
        bail!(
            "DATA_PATH_CONFLICT: memory {} belongs to Git worktree {}",
            repository.display(),
            top.display()
        );
    }
    Ok(())
}

fn persist_manifest(path: &Path, manifest: &HomeManifest) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("home manifest has no parent"))?;
    let temporary = parent.join(format!(".home-manifest.{}.tmp", EntityId::new()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path).with_context(|| {
        format!(
            "publish home manifest {} -> {}",
            temporary.display(),
            path.display()
        )
    })?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn home_checkpoint() -> HomeCheckpoint {
    HomeCheckpoint {
        level: CheckpointLevel::FilesAndMetadata,
        live_wasm_memory: false,
        model_kv: false,
        sockets: false,
    }
}

fn home_capabilities() -> BTreeMap<String, Capability> {
    let mut capabilities = BTreeMap::new();
    let mut add = |name: &str, status: CapabilityStatus, detail: &str| {
        capabilities.insert(
            name.into(),
            Capability {
                status,
                detail: detail.into(),
            },
        );
    };
    add(
        "wasi_preview1",
        CapabilityStatus::Available,
        "wasmtime 45 executes the seed guest with explicit preopens",
    );
    add(
        "host_git_memory",
        CapabilityStatus::Available,
        "host git initializes memory; /memory is direct RW; this is not WASM Git",
    );
    add(
        "wasm_git",
        CapabilityStatus::NotImplemented,
        "no Git protocol or credential helper executes inside WASM",
    );
    add(
        "wasm_ssh",
        CapabilityStatus::NotImplemented,
        "no SSH client protocol executes inside WASM",
    );
    add(
        "copy_on_write_mounts",
        CapabilityStatus::Available,
        "managed COW: the base stays readonly, guest writes copy up into a \
         per-branch overlay under the home workspace store, removals become \
         durable tombstones, and commits are content-addressed manifests over \
         the effective view; special files are not covered",
    );
    add(
        "live_wasm_memory_checkpoint",
        CapabilityStatus::NotCaptured,
        "checkpoint covers persistent files and metadata only",
    );
    add(
        "model_kv_checkpoint",
        CapabilityStatus::NotCaptured,
        "model KV is outside the home checkpoint",
    );
    add(
        "socket_checkpoint",
        CapabilityStatus::NotCaptured,
        "live sockets are outside the home checkpoint",
    );
    add(
        "special_file_semantics",
        CapabilityStatus::Unknown,
        "Linux proc/device/socket compatibility has not been qualified",
    );
    capabilities
}

/// The capability declaration written by homes created before managed COW
/// mounts were wired into the runtime. It differs from the current one in
/// exactly the `copy_on_write_mounts` entry; `prepare` upgrades only this
/// exact map in place, and any other difference stays a migration error with
/// the stored data preserved.
fn legacy_home_capabilities() -> BTreeMap<String, Capability> {
    let mut legacy = home_capabilities();
    legacy.insert(
        "copy_on_write_mounts".into(),
        Capability {
            status: CapabilityStatus::Unsupported,
            detail: "COW requests fail explicitly; never downgraded to direct RW".into(),
        },
    );
    legacy
}
